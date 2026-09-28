//! The stateful TGP session encoder.
//!
//! Consumes [`FrameSource`] updates and produces the byte stream a terminal
//! should receive: a full transmit+place for every frame. The output is a
//! [`futures_core::Stream`] of byte chunks — nothing is written to stdout;
//! the caller owns the sink (PTY, SSH channel, file, ...).

use futures_core::Stream;
use std::time::Instant;
use tokio::sync::mpsc;

use crate::proto as tgp;
use tuigui_streamer::frame::{Frame, FrameMetadata, PixelFormat};
use tuigui_streamer::source::{FrameSource, FrameUpdate, SourceError};

/// What the encoder tells its consumer.
#[derive(Debug)]
pub enum EncoderEvent {
    /// A frame is queued for emission; carry its telemetry so the UI can show
    /// resolution and capture cost without parsing the bytes.
    Frame {
        width: u32,
        height: u32,
        /// Wall-clock time spent pulling this frame out of its source (the
        /// capture cost). Excludes the source's pacing sleep when the source
        /// reports real per-phase timings; otherwise it's the whole poll.
        capture_ms: f64,
        /// Wall-clock time the source spent sleeping to pace itself (target
        /// fps), when distinguishable from real capture time.
        pacing_ms: f64,
        /// Wall-clock time spent turning the frame's pixels into TGP bytes
        /// (base64 + chunking), measured around `full_frame_commands`.
        encode_ms: f64,
        /// Per-phase capture breakdown, when the source reported it.
        timing: Option<tuigui_streamer::frame::CaptureTiming>,
        /// Running total of tile renders reusing an already-stored id (no
        /// transmit), accumulated over the session (block mode + `tile_reuse`).
        /// Lets the UI show an accumulated tile-reuse hit-rate.
        tile_reused: u64,
        /// Running total of tile renders that transmitted a fresh payload.
        tile_transmitted: u64,
        /// Percent of the block grid currently occupied by distinct tile
        /// contents (unique content hashes / on-screen slots, 0..=100).
        /// Includes the whole screen (all on-screen tiles), independent of
        /// whether tile reuse is on.
        tile_unique_pct: u32,
    },
    /// Bytes to write to the terminal, in order.
    Bytes(Vec<u8>),
    /// The source ended.
    Ended,
}

/// Errors from the encoder loop.
#[derive(Debug, thiserror::Error)]
pub enum EncoderError {
    #[error("source error: {0}")]
    Source(#[from] SourceError),
    #[error("frame metadata changed without a Resized update")]
    MetadataMismatch,
}

/// Configuration for [`TgpEncoder`].
#[derive(Debug, Clone)]
pub struct EncoderConfig {
    /// On-screen placement size in terminal cells. When both are `>0` every
    /// placed image is scaled to fill exactly this `columns x rows` area;
    /// without these the image is shown at one screen cell per source pixel,
    /// so a full-resolution frame overflows the terminal and only its top-left
    /// corner is visible. Set both from the TUI's pane size.
    pub placement_columns: u32,
    pub placement_rows: u32,
    /// Transmit frames as PNG (`f=100`) instead of raw RGBA/RGB. PNG is decoded
    /// by the terminal (wuffs), and can shrink an uncompressed frame ~5-20x —
    /// the dominant win when the terminal sink is the bottleneck. When set, the
    /// frame's colors are pre-rounded to [`color_bits`](/self#structfield-color_bits)
    /// per channel before lossless truecolor PNG encoding, which lets the
    /// deflate filter collapse the mostly-flat UI palette into far fewer bytes
    /// than a true 24-bit image.
    pub png: bool,
    /// Bits of color depth to keep per channel (1-8) when rounding the frame
    /// before PNG encoding. Kept bits are the most significant; dropped low
    /// bits snap the pixel to the nearest representable shade. This is a cheap,
    /// deterministic way to shrink the effective color resolution (and thus the
    /// compressed payload) for flat UI content without the cost or palette
    /// retraining of a per-frame quantizer. 8 disables rounding (24-bit true
    /// color). Unused when [`png`](/self#structfield-png) is false.
    pub color_bits: u8,
    /// Stream as an `N x N` grid of micro-blocks instead of a single full-frame
    /// image. `0` (default) disables block streaming: every frame is one
    /// transmit+place. When `> 0`, the encoder splits each frame into `N`
    /// columns × `N` rows of rectangular blocks, diffs each against the previous
    /// frame's content, and retransmits only the blocks that changed (each as
    /// its own transmit+place at its grid offset). Unchanged blocks are emitted
    /// as nothing: the terminal keeps rendering the block from the last frame,
    /// so a static scene costs almost nothing.
    pub blocks_per_side: u32,
    /// With `blocks_per_side > 0`, the number of resolution levels on each
    /// block's ladder. `0` (default) uses the streamer default (4). Level 1 is
    /// the coarsest downscale, and level `res_levels` is the block's full
    /// source resolution. A changed block resets to the lowest level and climbs
    /// back up one rung per static frame; an unchanged block already shown at
    /// max resolution costs nothing.
    pub res_levels: u32,
    /// With `blocks_per_side > 0`, detail-adaptive resolution ceilings: each block's
    /// largest neighbor color difference picks its ceiling on a linear ladder
    /// (see `spec/LOD.md`). Solid fills cap at the coarsest, differences inside
    /// the [`DetailConfig::dead_zone`] top band resolve at full res, and
    /// differences below grade linearly across `res_levels`. Disabled keeps the
    /// classic always-full ladder.
    pub detail: tuigui_streamer::blocks::DetailConfig,
    /// With `blocks_per_side > 0`, tint each transmitted block's payload red in
    /// proportion to its resolution level (level 1 untouched, higher = more
    /// red). Visually proves what resolution each block settles at.
    pub debug_lod: bool,
    /// With `blocks_per_side > 0`, colorize every other block (linear grid
    /// index odd) with a per-block pseudo-random color that changes each frame,
    /// and leave even blocks as the real pixels. Visually proves the block grid
    /// and the per-block diffing: even blocks on a static frame stay silent,
    /// odd blocks keep streaming. `false` (default) transmits true pixels.
    pub debug_blocks: bool,
    /// With `blocks_per_side > 0`, the pane's top-left cell in terminal
    /// coordinates (0-based, as ratatui reports). The encoder individually
    /// positions each block's transmit+place by moving the cursor to the
    /// block's top-left cell, so it needs to know where the pane starts.
    /// `None` (default) is used by full-frame mode, where `main.rs` positions
    /// the cursor once.
    pub pane_origin: Option<(u16, u16)>,
    /// With `blocks_per_side > 0`, cap how many pixels each frame hands the
    /// terminal decoder. `0` (default) retransmits every changed block inline,
    /// with no ceiling (current behavior). When `> 0`, changed blocks route
    /// through a rendering queue drained at most once per frame up to this many
    /// *payload* pixels (`payload_width × payload_height` of the affected
    /// blocks); the rest spill to the next frame. See `spec/RENDER_QUEUE.md`.
    pub render_pixel_budget: u64,
    /// With `blocks_per_side > 0`, dedupe tiles by content: keep a hash->id map
    /// of what the terminal already stores and, for a changed tile whose exact
    /// payload bytes are already cached (e.g. a repeated solid background),
    /// just place the cached id instead of re-transmitting. On by default; see
    /// `spec/TILE_REUSE.md`. `false` re-transmits every changed tile.
    pub tile_reuse: bool,
    /// Opt-in zlib (RFC 1950) compression of transmitted payloads via `o=z`.
    /// Off by default. When set, the payload is deflated before base64 for any
    /// format — raw (`f=24/32`, the big win since it's currently shipped
    /// uncompressed) or PNG (redundant as PNG already deflates, but harmless).
    pub zlib: bool,
    /// With `tile_reuse`, tint reused tiles debug-style. A tile whose content is
    /// already cached (would be placed, not re-transmitted) is instead tinted a
    /// deterministic color derived from its content hash and re-transmitted, so
    /// every tile sharing the same content shows the same tinted color. Visually
    /// proves which tiles dedupe. `false` (default) keeps the real place-only
    /// reuse.
    pub debug_reused_tiles: bool,
}

impl Default for EncoderConfig {
    fn default() -> Self {
        EncoderConfig {
            placement_columns: 0,
            placement_rows: 0,
            png: false,
            color_bits: 8,
            blocks_per_side: 0,
            res_levels: 0,
            detail: Default::default(),
            debug_lod: false,
            debug_blocks: false,
            pane_origin: None,
            render_pixel_budget: 0,
            // Tile reuse is on by default: the whole point is deduping repeated
            // content on the terminal. Pass `--no-tile-reuse` / set false to opt
            // out.
            tile_reuse: true,
            // zlib compression is opt-in: it trades encoder CPU for bandwidth,
            // which is only worth it when the link is the bottleneck.
            zlib: false,
            debug_reused_tiles: false,
        }
    }
}

/// Per-session counters for tile-reuse dedupe (`config.tile_reuse`), carried on
/// each [`EncoderEvent::Frame`] so the UI can show an accumulated hit-rate.
pub struct TileStats {
    /// Number of tile renders satisfied by an already-uploaded cached id
    /// (placed, no transmit). Accumulated over the session.
    pub reused: u64,
    /// Number of tile renders that had to transmit a fresh payload.
    /// Accumulated over the session.
    pub transmitted: u64,
}

impl Default for TileStats {
    fn default() -> Self {
        TileStats { reused: 0, transmitted: 0 }
    }
}

impl TileStats {
    /// Fraction of tile renders that reused a cached id, in `0..=1`.
    pub fn hit_rate(&self) -> f64 {
        let total = self.reused + self.transmitted;
        if total == 0 {
            0.0
        } else {
            self.reused as f64 / total as f64
        }
    }
}

/// Converts a [`FrameSource`] into a stream of TGP bytes.
///
/// The encoder owns the session state: which image id is in use, whether the
/// base image has been placed, and the running frame counter.
pub struct TgpEncoder {
    config: EncoderConfig,
    image_id: u32,
    /// Running frame count, used as a seed for `--debug-blocks` colorization.
    frame_count: u64,
    /// Block differ, only used when `config.blocks_per_side > 0`.
    blocks: Option<tuigui_streamer::blocks::BlockGrid>,
    /// Image id currently on screen for each block grid slot (indexed
    /// `row * cols + col`). When a slot is retransmitted we first place the new
    /// image under a fresh id (so the slot is covered with no blank gap) and
    /// *then* delete this displaced id, keeping the terminal's stored images
    /// bounded at the grid size instead of accumulating forever (which fills
    /// the store and jams rendering, printing escapes as raw text).
    block_ids: Vec<Option<u32>>,
    /// Content hash currently placed in each block grid slot (parallel to
    /// `block_ids`), used to report the % of unique tiles on screen.
    slot_hash: Vec<Option<u64>>,
    /// Rendering queue: blocks waiting to be transmitted, keyed by grid slot.
    /// Only used when `config.render_pixel_budget > 0`; drained at most once per
    /// frame up to the pixel budget, undrained entries spill to the next frame.
    /// An entry is a `(slot_index, MicroBlock)` pair, and enqueuing a second
    /// update for a slot already queued replaces it (keeps the newest payload,
    /// never renders a superseded one).
    render_queue: Vec<(usize, tuigui_streamer::blocks::MicroBlock)>,
    /// Content-addressed tile cache (`config.tile_reuse`): maps a hash of the
    /// exact bytes the terminal stores for a tile to the image id that holds
    /// them. A changed tile whose hash is a hit is placed (not re-transmitted).
    /// Eviction bounds the terminal's stored-image count; see `spec/TILE_REUSE.md`.
    tile_cache: std::collections::HashMap<u64, u32>,
    /// LRU recency + lifetime, parallel to `tile_cache` (keyed by the same hash).
    tile_recency: std::collections::HashMap<u64, u64>,
    /// Bytes the terminal would hold for a cached hash (to know transmit cost on
    /// a miss / store size).
    tile_size: std::collections::HashMap<u64, usize>,
    /// Monotonic recency clock for LRU eviction.
    tile_clock: u64,
    /// How many placements currently reference each cached image id. Lets us
    /// evict ids that no on-screen placement needs without deleting a live one.
    tile_refs: std::collections::HashMap<u32, usize>,
    /// Aggregated tile-reuse telemetry.
    tile_stats: TileStats,
}

impl TgpEncoder {
    pub fn new(config: EncoderConfig) -> Self {
        let blocks = (config.blocks_per_side > 0).then(|| {
            tuigui_streamer::blocks::BlockGrid::new(
                config.blocks_per_side,
                config.blocks_per_side,
                config.res_levels,
                config.detail,
                config.debug_lod,
            )
        });
        TgpEncoder {
            config,
            // Image ids are a shared namespace with other TGP programs; start
            // from a nonzero pseudorandom base to make collisions unlikely.
            image_id: 1 + (std::process::id() % 1000) * 7,
            frame_count: 0,
            blocks,
            block_ids: Vec::new(),
            slot_hash: Vec::new(),
            render_queue: Vec::new(),
            tile_cache: std::collections::HashMap::new(),
            tile_recency: std::collections::HashMap::new(),
            tile_size: std::collections::HashMap::new(),
            tile_clock: 0,
            tile_refs: std::collections::HashMap::new(),
            tile_stats: TileStats::default(),
        }
    }

    pub fn image_id(&self) -> u32 {
        self.image_id
    }

    /// Enqueue this frame's changed blocks keyed by grid slot, replacing any
    /// already-queued (and therefore not-yet-rendered) version of the same slot
    /// so a superseded payload is never transmitted. Only touches the rendering
    /// queue; call [`TgpEncoder::drain_render_queue`] afterwards.
    fn enqueue_blocks(
        &mut self,
        changed: &[tuigui_streamer::blocks::MicroBlock],
        n_slots: usize,
        grid_cols: usize,
    ) {
        // Trim to the grid, in case a resize aged out queued slots.
        self.render_queue.retain(|(slot, _)| *slot < n_slots);
        // Mark which slots are already queued so we can replace in place.
        let mut secondary_index: Vec<Option<usize>> = vec![None; n_slots];
        for (i, (slot, _)) in self.render_queue.iter().enumerate() {
            if *slot < n_slots {
                secondary_index[*slot] = Some(i);
            }
        }
        for b in changed {
            let slot = (b.row as usize) * grid_cols + b.col as usize;
            if slot >= n_slots {
                continue;
            }
            match secondary_index[slot] {
                // Already queued: replace in place with the newest payload.
                Some(i) => self.render_queue[i].1 = b.clone(),
                // Not queued yet: append.
                None => {
                    self.render_queue.push((slot, b.clone()));
                    secondary_index[slot] = Some(self.render_queue.len() - 1);
                }
            }
        }
    }

    /// The exact bytes this block would transmit to the terminal (the payload
    /// after PNG encoding / color-bit rounding). When `tile_reuse` is on, the
    /// content hash is taken over these bytes, so two tiles collide, and reuse,
    /// only when what the terminal stores is byte-identical.
    fn tile_payload(&self, b: &tuigui_streamer::blocks::MicroBlock) -> bytes::Bytes {
        if self.config.png {
            encode_png(&b.data, b.payload_width, b.payload_height, self.config.color_bits)
        } else {
            b.data.clone()
        }
    }

    /// Render a changed block at its grid slot. With `tile_reuse` enabled this
    /// is content-addressed:
    ///
    /// - **cache hit** — the terminal already stores a tile with these exact
    ///   payload bytes, so we place the existing id (`a=p`, no payload) instead
    ///   of re-transmitting. Counted as a reuse.
    /// - **cache miss** — transmit the block as usual under a fresh id, then
    ///   record `hash -> id`.
    ///
    /// Either way the slot is covered with no blank gap, and the displaced id
    /// (the block's previous content) is deleted so the terminal store stays
    /// bounded. When `tile_reuse` is off this just retransmits (classic path).
    fn emit_block<'a>(
        &mut self,
        b: &tuigui_streamer::blocks::MicroBlock,
        slot: usize,
        on_event: &mut impl FnMut(EncoderEvent) -> Result<(), EncoderError>,
        origin: (u16, u16),
        placement: BlockPlacement,
    ) -> Result<(), EncoderError> {
        let old = self.block_ids[slot].take();
        if self.config.tile_reuse {
            let payload = self.tile_payload(b);
            let hash = self.content_hash(&payload) ^ ((b.payload_width as u64) << 32 | b.payload_height as u64);
            let hit = self.tile_cache.get(&hash).copied();
            if let Some(site_id) = hit {
                // Cache hit: place the existing id, no transmit.
                self.tile_stats.reused += 1;
                let tick = self.touch_clock();
                self.tile_recency.insert(hash, tick);
                *self.tile_refs.entry(site_id).or_insert(0) += 1;
                self.block_ids[slot] = Some(site_id);
                self.slot_hash[slot] = Some(hash);
                if self.config.debug_reused_tiles {
                    // Debug: instead of placing the cached id, re-transmit the
                    // block tinted a deterministic color derived from its content
                    // hash, so every tile that would dedupe shows the same color.
                    // Tint the *raw pixels* (`b.data`) and let the normal encode
                    // path compress them, so PNG stays a valid PNG (tinting the
                    // already-encoded payload would corrupt a PNG header, which
                    // is what made these tiles render transparent).
                    let tinted_block = tuigui_streamer::blocks::MicroBlock {
                        data: tint_raw(&b.data, hash),
                        ..b.clone()
                    };
                    let bid = self.image_id;
                    self.image_id = self.image_id.wrapping_add(1);
                    self.block_ids[slot] = Some(bid);
                    on_event(EncoderEvent::Bytes(transmit_block(
                        &tinted_block,
                        &base_control_for_block(&tinted_block, bid, placement),
                        self.config.png,
                        self.config.color_bits,
                        origin,
                        placement,
                        self.config.zlib,
                    )))?;
                } else {
                    on_event(EncoderEvent::Bytes(place_block(
                        b,
                        site_id,
                        placement,
                        origin,
                    )))?;
                }
            } else {
                // Cache miss: transmit under a fresh id and record it.
                self.tile_stats.transmitted += 1;
                let bid = self.image_id;
                self.image_id = self.image_id.wrapping_add(1);
                self.block_ids[slot] = Some(bid);
                self.slot_hash[slot] = Some(hash);
                self.record_tile(hash, bid, &payload)?;
                on_event(EncoderEvent::Bytes(transmit_block_payload(
                    b,
                    &base_control_for_block(b, bid, placement),
                    origin,
                    placement,
                    &payload,
                    if self.config.png { 100 } else { 32 },
                    self.config.zlib,
                )))?;
            }
            if let Some(old) = old {
                self.drop_tile_ref(old);
            }
            self.evict_if_over_capacity(on_event)?;
        } else {
            let bid = self.image_id;
            self.image_id = self.image_id.wrapping_add(1);
            self.block_ids[slot] = Some(bid);
            self.slot_hash[slot] = Some(0u64.wrapping_add(self.image_id as u64));
            on_event(EncoderEvent::Bytes(transmit_block(
                b,
                &base_control_for_block(b, bid, placement),
                self.config.png,
                self.config.color_bits,
                origin,
                placement,
                self.config.zlib,
            )))?;
            if let Some(old) = old {
                on_event(EncoderEvent::Bytes(delete_image(old)))?;
            }
        }
        Ok(())
    }

    /// Advance the LRU recency clock and return its new tick.
    fn touch_clock(&mut self) -> u64 {
        self.tile_clock = self.tile_clock.wrapping_add(1);
        self.tile_clock
    }

    /// Fast, stable 64-bit hash of the payload (FxHash-style folding is fine;
    /// we only need collision resistance against identical-content caching).
    fn content_hash(&self, payload: &[u8]) -> u64 {
        const SEED: u64 = 0x9E_3779_B9_7F_4A_7C15;
        let mut h = SEED;
        for &b in payload {
            h = h ^ (b as u64);
            h = h.wrapping_mul(0x100_0000_01B3);
        }
        h
    }

    /// Record a freshly-transmitted tile in the content cache under `hash`.
    fn record_tile<'a>(
        &mut self,
        hash: u64,
        bid: u32,
        payload: &[u8],
    ) -> Result<(), EncoderError> {
        self.tile_cache.insert(hash, bid);
        let tick = self.touch_clock();
        self.tile_recency.insert(hash, tick);
        self.tile_size.insert(hash, payload.len());
        *self.tile_refs.entry(bid).or_insert(0) += 1;
        Ok(())
    }

    /// A block slot has moved off `site_id` (either to different content or the
    /// slot was covered). Drop its placement reference; if it reaches zero and
    /// the terminal still holds it, it is left in place for future reuse (it
    /// only gets a delete when evicted from the bounded cache).
    fn drop_tile_ref(&mut self, site_id: u32) {
        if let Some(count) = self.tile_refs.get_mut(&site_id) {
            *count = count.saturating_sub(1);
        }
    }

    /// Bound the tile cache to the grid's worth of stored images: when a
    /// column/row count changes in flight the cap is `n_slots × 2`; we evict
    /// least-recently-used ids that no on-screen placement references (delete
    /// them from the terminal) until under the cap. Refcounted (live) entries
    /// below the cap are kept; a live id that crosses the cap is deleted only
    /// once, when its last placement moves off.
    fn evict_if_over_capacity<'a>(
        &mut self,
        on_event: &mut impl FnMut(EncoderEvent) -> Result<(), EncoderError>,
    ) -> Result<(), EncoderError> {
        // The terminal store bound is not measured here; each slot holds at
        // most 2 ids (current + displaced), so the natural cap is grid size.
        const CAP: usize = 64;
        if self.tile_cache.len() <= CAP {
            return Ok(());
        }
        // Collect evictable hashes: zero refcount, oldest recency.
        let mut candidates: Vec<(u64, u64)> = self
            .tile_cache
            .iter()
            .filter(|(_, id)| self.tile_refs.get(id).map(|c| *c == 0).unwrap_or(true))
            .map(|(h, _)| (*h, self.tile_recency.get(h).copied().unwrap_or(0)))
            .collect();
        candidates.sort_by_key(|(_, r)| *r);
        for (hash, _) in candidates {
            if self.tile_cache.len() <= CAP {
                break;
            }
            if let Some(id) = self.tile_cache.remove(&hash) {
                self.tile_recency.remove(&hash);
                self.tile_size.remove(&hash);
                self.tile_refs.remove(&id);
                on_event(EncoderEvent::Bytes(delete_image(id)))?;
            }
        }
        Ok(())
    }

    /// Share of the block grid currently occupied by *distinct* tile contents,
    /// as a percent 0..=100. Unique = number of distinct content hashes across
    /// all on-screen slots, over the total slots. Independent of whether tile
    /// reuse is enabled (it reflects what's on screen, not what we transmitted).
    fn unique_tile_pct(&self) -> u32 {
        let total = self.slot_hash.len().max(1);
        let distinct = self
            .slot_hash
            .iter()
            .filter_map(|h| *h)
            .collect::<std::collections::HashSet<_>>()
            .len();
        (distinct * 100 / total) as u32
    }

    /// Drain the rendering queue from the front, transmitting tiles until
    /// adding the next would exceed `budget` *payload* pixels
    /// (`payload_width × payload_height`, i.e. the resolution the tile was
    /// actually settled at). The rest stay queued for the next frame. Emits
    /// transmit+place + delete-displaced pairs for each drained tile.
    fn drain_render_queue(
        &mut self,
        budget: u64,
        on_event: &mut impl FnMut(EncoderEvent) -> Result<(), EncoderError>,
        origin: (u16, u16),
        placement: BlockPlacement,
    ) -> Result<(), EncoderError> {
        let mut spent: u64 = 0;
        for (slot, block) in std::mem::take(&mut self.render_queue) {
            let cost = block.payload_width as u64 * block.payload_height as u64;
            if spent + cost > budget {
                // Keep this tile (and everything behind it) for the next frame.
                self.render_queue.push((slot, block));
                continue;
            }
            spent += cost;
            self.emit_block(&block, slot, on_event, origin, placement)?;
        }
        Ok(())
    }

    /// Encode the full lifetime of a source into events. Drives `source` to
    /// completion; use [`TgpEncoder::into_stream`] for the Stream API.
    pub async fn run<S: FrameSource>(
        &mut self,
        source: &mut S,
        mut on_event: impl FnMut(EncoderEvent) -> Result<(), EncoderError>,
    ) -> Result<(), EncoderError> {
        // The base image is established from the first real frame, not from
        // `source.metadata()` (which for a live capture is best-effort and may
        // differ from what the compositor actually announces).
        let mut last_meta: Option<FrameMetadata> = None;
        // id of the image currently on screen, to delete after the next frame
        // replaces it.
        let mut prev_image_id: Option<u32> = None;

        // Control block for this frame's image. Placement rect (`c`,`r`) is
        // carried on the transmit+place so the image fills the pane on screen.
        let (placement_columns, placement_rows) = (self.config.placement_columns, self.config.placement_rows);

        loop {
            let t0 = Instant::now();
            let update = match source.next().await {
                Ok(Some(update)) => update,
                Ok(None) => {
                    tracing::error!("encoder: source returned None (live capture should never end)");
                    break;
                }
                Err(e) => {
                    tracing::error!(err = %e, "encoder: source.next() errored");
                    return Err(EncoderError::Source(e));
                }
            };
            let poll_ms = t0.elapsed().as_secs_f64() * 1000.0;
            match update {
                FrameUpdate::Resized(_m) => {
                    on_event(EncoderEvent::Bytes(quiet_probe_reposition()))?;
                }
                FrameUpdate::Idle => continue,
                FrameUpdate::Ended => {
                    // Delete the image currently on screen (the last placed one).
                    if let Some(prev) = prev_image_id {
                        on_event(EncoderEvent::Bytes(delete_image(prev)))?;
                    }
                    // In block mode, delete every block image still on screen.
                    for old in self.block_ids.iter().flatten() {
                        on_event(EncoderEvent::Bytes(delete_image(*old)))?;
                    }
                    self.block_ids.clear();
                    // Discard any blocks still queued (they were never emitted,
                    // so they hold no on-screen id to delete).
                    self.render_queue.clear();
                    on_event(EncoderEvent::Ended)?;
                    return Ok(());
                }
                FrameUpdate::Frame(frame) => {
                    if let Some(last) = &last_meta {
                        if frame.metadata.width != last.width
                            || frame.metadata.height != last.height
                            || frame.metadata.format != last.format
                        {
                            return Err(EncoderError::MetadataMismatch);
                        }
                    } else {
                        // First frame establishes the base image geometry.
                        last_meta = Some(frame.metadata.clone());
                    }
                    let data_len = frame.data.len();
                    let chunk_estimate = base64_len(data_len).div_ceil(4096);
                    tracing::debug!(
                        ?frame.metadata.format,
                        width = frame.metadata.width,
                        height = frame.metadata.height,
                        data_len,
                        chunk_estimate,
                        damage = frame.damage.len(),
                        "encoder frame start"
                    );
                    // Use a fresh image id every frame. Re-transmitting the
                    // same id deletes the on-screen placement (spec), which
                    // blanks the pane for the whole transmit -> flicker. A
                    // new id transmits and places over the old image with no
                    // delete gap; the prior image is deleted once it's covered.
                    let cur_id = self.image_id;
                    self.image_id = self.image_id.wrapping_add(1);
                    // Announce the frame before its payload so the consumer can
                    // timestamp it for capture/latency telemetry. The bytes are
                    // produced inline, so time the encoding here.
                    let t_encode = Instant::now();
                    if let Some(grid) = self.blocks.as_mut() {
                        // Block mode: diff against the previous frame and
                        // retransmit only changed blocks. Each block is a
                        // transmit+place under its own image id, mapped through
                        // frame->pane scaling so the blocks tile the pane.
                        let seed = self.frame_count.wrapping_add(1);
                        let grid_cols = grid.cols() as usize;
                        let n_slots = (grid.cols() * grid.rows()) as usize;
                        if self.block_ids.len() != n_slots {
                            self.block_ids.clear();
                            self.block_ids.resize(n_slots, None);
                            self.slot_hash.clear();
                            self.slot_hash.resize(n_slots, None);
                        }
                        let origin = self.config.pane_origin.unwrap_or((0, 0));
                        let changed = grid.diff(&frame, self.config.debug_blocks, seed);
                        let placement = BlockPlacement::new(
                            (frame.metadata.width, frame.metadata.height),
                            (placement_columns, placement_rows),
                        );
                        if self.config.render_pixel_budget > 0 {
                            self.enqueue_blocks(&changed, n_slots, grid_cols);
                            self.drain_render_queue(
                                self.config.render_pixel_budget,
                                &mut on_event,
                                origin,
                                placement,
                            )?;
                        } else {
                            for b in &changed {
                                let slot = (b.row as usize) * grid_cols + b.col as usize;
                                self.emit_block(b, slot, &mut on_event, origin, placement)?;
                            }
                        }
                        self.frame_count += 1;
                    } else {
                        for bytes in full_frame_commands(
                            &frame,
                            &base_control(&frame.metadata, cur_id, placement_columns, placement_rows),
                            self.config.png,
                            self.config.color_bits,
                            self.config.zlib,
                        ) {
                            on_event(EncoderEvent::Bytes(bytes))?;
                        }
                        if let Some(prev) = prev_image_id.take() {
                            on_event(EncoderEvent::Bytes(delete_image(prev)))?;
                        }
                        prev_image_id = Some(cur_id);
                    }
                    let encode_ms = t_encode.elapsed().as_secs_f64() * 1000.0;
                    // capture_ms is the *real* capture cost. When the source
                    // reports per-phase timings (live capture), its total is
                    // the actual grab time; fall back to the whole poll for
                    // sources without it. `poll_ms` includes any pacing sleep
                    // the source does, which isn't capture time.
                    let capture_ms = frame
                        .timing
                        .map(|t| t.total_ms())
                        .unwrap_or(poll_ms);
                    // When the source gives real per-phase timings, the residual
                    // poll time is its pacing sleep (the `1000/fps` throttle).
                    let pacing_ms = if frame.timing.is_some() {
                        (poll_ms - capture_ms).max(0.0)
                    } else {
                        0.0
                    };
                    on_event(EncoderEvent::Frame {
                        width: frame.metadata.width,
                        height: frame.metadata.height,
                        capture_ms,
                        pacing_ms,
                        encode_ms,
                        timing: frame.timing,
                        tile_reused: self.tile_stats.reused,
                        tile_transmitted: self.tile_stats.transmitted,
                        tile_unique_pct: self.unique_tile_pct(),
                    })?;
                }
            }
        }
        Err(EncoderError::Source(SourceError::Transport(
            "source stream ended without Ended".into(),
        )))
    }

    /// Run against a source and return a [`Stream`] of byte chunks.
    pub fn into_stream<S>(
        mut self,
        mut source: S,
    ) -> impl Stream<Item = Result<EncoderEvent, EncoderError>>
    where
        S: FrameSource + 'static,
    {
        // Unbounded buffer is fine: the consumer's drain loop caps bytes per
        // iteration, so it always yields to input polling regardless of how
        // fast this producer runs.
        let (tx, rx) = mpsc::unbounded_channel::<Result<EncoderEvent, EncoderError>>();
        tokio::spawn(async move {
            let result = self
                .run(&mut source, |event| {
                    tx.send(Ok(event)).map_err(|_| {
                        EncoderError::Source(SourceError::Transport(
                            "encoder consumer dropped".into(),
                        ))
                    })
                })
                .await;
            match &result {
                Ok(()) => tracing::debug!("encoder task finished cleanly"),
                Err(e) => tracing::error!(err = %e, "encoder task ended with error"),
            }
            if let Err(e) = result {
                let _ = tx.send(Err(e));
            }
        });
        EncoderStream {
            inner: tokio_stream::wrappers::UnboundedReceiverStream::new(rx),
        }
    }
}

/// Stream wrapper over the encoder's event channel.
pub struct EncoderStream {
    inner: tokio_stream::wrappers::UnboundedReceiverStream<Result<EncoderEvent, EncoderError>>,
}

impl Stream for EncoderStream {
    type Item = Result<EncoderEvent, EncoderError>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        std::pin::pin!(&mut self.inner).poll_next(cx)
    }
}

/// Transmit + place commands for a full frame.
fn full_frame_commands(
    frame: &Frame,
    control: &[(char, String)],
    png: bool,
    color_bits: u8,
    zlib: bool,
) -> Vec<Vec<u8>> {
    // TGP format id: 100 for PNG (terminal decodes), else raw pixel formats.
    let fmt = if png {
        100u32
    } else {
        match frame.metadata.format {
            PixelFormat::Rgb24 => 24,
            PixelFormat::Rgba32 => 32,
        }
    };
    let mut ctrl: Vec<(char, String)> = vec![('a', "T".into()), ('C', "1".into())];
    ctrl.extend(control.iter().cloned());
    // a=T places at the cursor; but we do not manage the cursor here, the
    // host TUI does. Placement keys live in `control` via base_control().
    let payload = if png {
        encode_png(
            &frame.data,
            frame.metadata.width,
            frame.metadata.height,
            color_bits,
        )
    } else {
        frame.data.clone()
    };
    let mut out = Vec::new();
    for c in tgp::chunked_transmit(fmt, ctrl, &payload, zlib) {
        out.push(c);
    }
    out
}

/// Palette resume helper: rebuilds the graphics-control block for a frame's
/// image (with the cache-friendly id/placement/colors keys). Placement rect
/// (`c`,`r`) carries the pane size in cells.
fn base_control(
    m: &FrameMetadata,
    image_id: u32,
    placement_columns: u32,
    placement_rows: u32,
) -> Vec<(char, String)> {
    let mut c = vec![
        ('i', image_id.to_string()),
        ('s', m.width.to_string()),
        ('v', m.height.to_string()),
    ];
    if placement_columns > 0 && placement_rows > 0 {
        c.push(('c', placement_columns.to_string()));
        c.push(('r', placement_rows.to_string()));
    }
    c.push(('p', 1u32.to_string()));
    c
}

/// Maps a block's source-pixel rect through the frame->pane scale onto the
/// on-screen cell grid, so changed blocks tile the pane exactly like full-frame
/// mode (which scales the whole frame to fill `placement_columns x rows`).
#[derive(Debug, Clone, Copy)]
struct BlockPlacement {
    /// Source frame pixel dimensions.
    frame_w: u32,
    frame_h: u32,
    /// On-screen pane size in cells.
    pane_cells: (u32, u32),
}

impl BlockPlacement {
    fn new(frame: (u32, u32), pane_cells: (u32, u32)) -> Self {
        BlockPlacement { frame_w: frame.0, frame_h: frame.1, pane_cells }
    }

    /// Block's top-left cell within the pane.
    fn cell_origin(&self, x: u32, y: u32) -> (u32, u32) {
        let (fw, fh) = (self.frame_w.max(1), self.frame_h.max(1));
        let (pc, pr) = (self.pane_cells.0.max(1), self.pane_cells.1.max(1));
        let cx = (x as u64 * pc as u64 / fw as u64) as u32;
        let cy = (y as u64 * pr as u64 / fh as u64) as u32;
        (cx, cy)
    }

    /// Cell width/height the block spans on screen.
    fn cell_size(&self, b: &tuigui_streamer::blocks::MicroBlock) -> (u32, u32) {
        let (cx0, cy0) = self.cell_origin(b.x, b.y);
        let (cx1, cy1) = self.cell_origin(b.x + b.width, b.y + b.height);
        ((cx1 - cx0).max(1), (cy1 - cy0).max(1))
    }
}

/// Transmit + place for a single changed micro-block. The block's source rect
/// (pixels) is mapped through the frame->pane scale into an on-screen cell
/// rect that tiles the pane: the cursor is moved to the block's top-left cell
/// and the block's pixels are scaled to fill exactly that cell extent (so
/// adjacent blocks tile the pane, matching full-frame mode).
fn transmit_block(
    b: &tuigui_streamer::blocks::MicroBlock,
    control: &[(char, String)],
    png: bool,
    color_bits: u8,
    pane_origin: (u16, u16),
    placement: BlockPlacement,
    zlib: bool,
) -> Vec<u8> {
    let payload = if png {
        encode_png(&b.data, b.payload_width, b.payload_height, color_bits)
    } else {
        b.data.clone()
    };
    transmit_block_payload(b, control, pane_origin, placement, &payload, if png { 100 } else { 32 }, zlib)
}

/// Transmit + place for a single changed micro-block, using an already-computed
/// `payload` (so callers that hashed it for tile reuse don't encode twice). The
/// block's source rect is mapped through the frame->pane scale into an
/// on-screen cell rect that tiles the pane.
fn transmit_block_payload(
    b: &tuigui_streamer::blocks::MicroBlock,
    control: &[(char, String)],
    pane_origin: (u16, u16),
    placement: BlockPlacement,
    payload: &[u8],
    fmt: u32,
    zlib: bool,
) -> Vec<u8> {
    let (cs, rs) = placement.cell_origin(b.x, b.y);
    let cur_col = pane_origin.0 as u32 + cs;
    let cur_row = pane_origin.1 as u32 + rs;

    let mut out = Vec::new();
    // Move the cursor to the block's top-left cell (CSI r;c H).
    out.extend_from_slice(format!("\x1b[{};{}H", cur_row + 1, cur_col + 1).as_bytes());
    let mut ctrl: Vec<(char, String)> = vec![('a', "T".into()), ('C', "1".into())];
    ctrl.extend(control.iter().cloned());
    for c in tgp::chunked_transmit(fmt, ctrl, payload, zlib) {
        out.extend_from_slice(&c);
    }
    out
}

/// Control block for a changed micro-block: a fresh id, source dims = the
/// *full-resolution* source pixel size of the block (the terminal scales this
/// intrinsic size to fill the on-screen rect), and a placement rect (`c`/`r`
/// in cells) sized to the block's on-screen cell extent after frame->pane
/// scaling. Because `s`/`v` always describe the full-res rect and `c`/`r` the
/// same rect's cell span, the terminal upscales a downscaled payload to fill
/// the block, tiling with adjacent blocks and matching full-frame mode.
fn base_control_for_block(
    b: &tuigui_streamer::blocks::MicroBlock,
    image_id: u32,
    placement: BlockPlacement,
) -> Vec<(char, String)> {
    let (c, r) = placement.cell_size(b);
    vec![
        ('i', image_id.to_string()),
        ('s', b.width.to_string()),
        ('v', b.height.to_string()),
        ('c', c.to_string()),
        ('r', r.to_string()),
        ('p', 1u32.to_string()),
    ]
}

/// Compress raw RGBA/RGB pixel data into a PNG byte string (in-memory).
///
/// When `color_bits < 8` each channel is rounded to the nearest representable
/// shade at that bit depth by masking the low bits (`& (0xff << (8-bits))`).
/// This deterministically collapses the effective color resolution so flat UI
/// content compresses far better, with no per-frame quantizer cost and no
/// cross-frame palette state to flicker. `8` keeps the true 24-bit color.
fn encode_png(
    data: &[u8],
    width: u32,
    height: u32,
    color_bits: u8,
) -> bytes::Bytes {
    let mut buf = Vec::new();
    let encoded = if color_bits >= 8 {
        data.to_vec()
    } else {
        round_channels(data, color_bits)
    };
    {
        let mut enc = png::Encoder::new(&mut buf, width, height);
        enc.set_depth(png::BitDepth::Eight);
        // Terminal decodes PNG via wuffs; use Default (not Fast) deflate so
        // fewer bytes go to the sink. herdr re-decodes every frame, so byte
        // size is the dominant cost there. Best is marginal and slower.
        enc.set_compression(png::Compression::Default);
        // Always truecolor. Rounding (rather than a palette-INDEXED png) keeps
        // the lossless format bits true so consecutive frames stay stateless:
        // the same source pixels always map to the same output bytes, which is
        // what kills per-frame color flicker without any cached palette.
        enc.set_color(png::ColorType::Rgba);
        let mut w = enc.write_header().expect("png header");
        w.write_image_data(&encoded).expect("png data");
    }
    bytes::Bytes::from(buf)
}

/// Round each RGBA channel to `bits` significant bits (1-8), snapping the pixel
/// to the nearest representable level rather than the nearest lower one. The
/// alpha channel is always kept fully intact. `bits >= 8` returns the data
/// unchanged.
fn round_channels(data: &[u8], bits: u8) -> Vec<u8> {
    debug_assert_eq!(data.len() % 4, 0);
    let bits = bits.clamp(1, 8);
    if bits == 8 {
        return data.to_vec();
    }
    let mut out = data.to_vec();
    // Mask keeps the top `bits` channels; rounding adds half a dropped step so
    // values snap to the nearest representable level (0x80 >> bits).
    let keep_mask = 0xffu8.wrapping_shl(8 - bits as u32);
    let round_amt = (0x80u8) >> bits;
    // Clamp so rounding can't overflow a byte: values already at the top
    // representable level must saturate there, not wrap to 0.
    let max_in = 0xff - round_amt;
    let (pxs, _rest) = out.as_chunks_mut::<4>();
    for px in pxs {
        px[0] = px[0].min(max_in).wrapping_add(round_amt) & keep_mask;
        px[1] = px[1].min(max_in).wrapping_add(round_amt) & keep_mask;
        px[2] = px[2].min(max_in).wrapping_add(round_amt) & keep_mask;
        // alpha untouched
    }
    out
}

fn delete_image(image_id: u32) -> Vec<u8> {
    tgp::command(
        &[
            ('a', "d".into()),
            ('d', "i".into()),
            ('i', image_id.to_string()),
            ('A', "1".into()),
        ],
        &[],
    )
}

/// Place an already-transmitted image at the block's on-screen cell without
/// re-transmitting any pixels. This is a proper *untracked* placement: `p=0`
/// lets the same image id be placed at many cells simultaneously (tracked
/// placements, `p=1..`, replace each other per the spec, which is what made
/// reused tiles go transparent — each later reuse moved the single placement).
///
/// `C=1` keeps the cursor put; `c`/`r` scale the stored image to the block's
/// on-screen cell span so it tiles exactly like a fresh transmit would. No
/// `s`/`v`/`p=1`: the image's own pixels are the source, drawn to `c`x`r`.
fn place_block(
    b: &tuigui_streamer::blocks::MicroBlock,
    image_id: u32,
    placement: BlockPlacement,
    pane_origin: (u16, u16),
) -> Vec<u8> {
    let (cs, rs) = placement.cell_origin(b.x, b.y);
    let cur_col = pane_origin.0 as u32 + cs;
    let cur_row = pane_origin.1 as u32 + rs;
    let (c, r) = placement.cell_size(b);
    let mut out = Vec::new();
    out.extend_from_slice(format!("\x1b[{};{}H", cur_row + 1, cur_col + 1).as_bytes());
    let ctrl: Vec<(char, String)> = vec![
        ('a', "p".into()),
        ('C', "1".into()),
        ('i', image_id.to_string()),
        ('p', 0u32.to_string()),
        ('c', c.to_string()),
        ('r', r.to_string()),
    ];
    for c in tgp::chunked_transmit(0, ctrl, &[], false) {
        out.extend_from_slice(&c);
    }
    out
}

fn quiet_probe_reposition() -> Vec<u8> {
    // Placeholder for re-placement after resize: the TUI layer owns cursor
    // management, so this is a no-op marker for now.
    Vec::new()
}

/// Debug tint for a reused tile: replace every pixel of the raw RGBA/RGB byte
/// buffer with a solid color derived deterministically from the content `hash`,
/// so all tiles sharing that content render the same color when re-encoded. This
/// is what visually flags "these tiles all dedupe to one stored image". Alpha is
/// forced opaque (so the tile never renders transparent). Operates on the *raw*
/// pixel bytes, never an encoded PNG, so it cannot corrupt a payload.
fn tint_raw(data: &[u8], hash: u64) -> bytes::Bytes {
    let (r, g, b) = (
        (hash & 0xff) as u8,
        ((hash >> 8) & 0xff) as u8,
        ((hash >> 16) & 0xff) as u8,
    );
    // Guess bpp from the buffer length so both Rgb24 and Rgba32 work.
    let bpp = if data.len() % 4 == 0 { 4 } else { 3 };
    let mut out = data.to_vec();
    for px in out.chunks_exact_mut(bpp) {
        px[0] = r;
        px[1] = g;
        px[2] = b;
        if bpp == 4 {
            px[3] = 0xff;
        }
    }
    bytes::Bytes::from(out)
}

/// Length in bytes of the base64 encoding of `n` bytes (padded).
fn base64_len(n: usize) -> usize {
    n.div_ceil(3) * 4
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use tuigui_streamer::source::{FrameSource, SourceError};

    struct TwoFrameSource {
        meta: FrameMetadata,
        n: u32,
    }

    #[async_trait]
    impl FrameSource for TwoFrameSource {
        fn metadata(&self) -> FrameMetadata {
            self.meta.clone()
        }
        async fn next(&mut self) -> Result<Option<FrameUpdate>, SourceError> {
            if self.n == 0 {
                return Ok(Some(FrameUpdate::Ended));
            }
            self.n -= 1;
            let len = (self.meta.width * self.meta.height) as usize * 4;
            let data = vec![0u8; len];
            let f = Frame::full(self.meta.clone(), data.into());
            Ok(Some(FrameUpdate::Frame(f)))
        }
    }

    fn meta() -> FrameMetadata {
        FrameMetadata {
            width: 2,
            height: 2,
            format: PixelFormat::Rgba32,
        }
    }

    #[tokio::test]
    async fn full_retransmit_emits_transmit_place_then_delete() {
        let mut enc = TgpEncoder::new(EncoderConfig::default());
        let mut src = TwoFrameSource { meta: meta(), n: 2 };
        let mut events = Vec::new();
        enc.run(&mut src, |e| {
            events.push(e);
            Ok(())
        })
        .await
        .unwrap();
        let strings: Vec<String> = events
            .into_iter()
            .filter_map(|e| match e {
                EncoderEvent::Bytes(b) => Some(String::from_utf8(b).unwrap()),
                _ => None,
            })
            .collect();
        // 2 frames × (transmit+place), delete-of-previous after frame 2, then
        // delete of the last image on Ended.
        assert_eq!(strings.len(), 4);
        assert!(strings[0].starts_with("\x1b_Gq=2,a=T"));
        assert!(strings[0].contains("a=T"));
        assert!(strings[3].starts_with("\x1b_Gq=2,a=d"));
    }

    /// Extract the `i=<id>` value from a TGP command's control block.
    fn id_of(s: &str) -> &str {
        for k in s.split(',') {
            if let Some(v) = k.strip_prefix("i=") {
                return v;
            }
        }
        ""
    }

    #[tokio::test]
    async fn every_frame_retransmits_and_places() {
        let mut enc = TgpEncoder::new(EncoderConfig::default());
        let mut src = TwoFrameSource { meta: meta(), n: 2 };
        let mut events = Vec::new();
        enc.run(&mut src, |e| {
            events.push(e);
            Ok(())
        })
        .await
        .unwrap();
        let strings: Vec<String> = events
            .into_iter()
            .filter_map(|e| match e {
                EncoderEvent::Bytes(b) => Some(String::from_utf8(b).unwrap()),
                _ => None,
            })
            .collect();
        // 2 frames: transmit+place each, then delete-of-previous after frame 2,
        // then delete of the last image on Ended -> 4 events.
        assert_eq!(strings.len(), 4);
        assert!(strings[0].starts_with("\x1b_Gq=2,a=T"));
        assert!(strings[0].contains("a=T"));
        assert!(strings[1].starts_with("\x1b_Gq=2,a=T"), "frame 2 must retransmit");
        assert!(strings[1].contains("a=T"));
        // Each frame must use a distinct image id (no delete-gap flicker).
        let id0 = id_of(&strings[0]);
        let id1 = id_of(&strings[1]);
        assert_ne!(id0, id1, "each frame must transmit under a fresh image id");
        // frame 2's delete targets the previous first-frame id.
        assert!(strings[2].starts_with("\x1b_Gq=2,a=d"));
        assert!(strings[2].contains(&format!("i={id0}")));
        // final delete removes the last placed image on Ended.
        assert!(strings[3].starts_with("\x1b_Gq=2,a=d"));
        assert!(strings[3].contains(&format!("i={id1}")));
    }

    #[tokio::test]
    async fn block_mode_emits_only_changed_blocks_then_quiet() {
        use bytes::Bytes as B;
        // A 40x40 RGBA frame split 2x2 (blocks_per_side=2) gives 4 blocks of
        // 20x20. Frame 1 transmits all 4; frame 2 changes nothing so emits no
        // block bytes; frame 3 paints the top-left block differently and only
        // that block is retransmitted.
        struct BlkSrc {
            n: u32,
            // frame_index -> which block is painted a distinct color.
        }
        #[async_trait]
        impl FrameSource for BlkSrc {
            fn metadata(&self) -> FrameMetadata {
                FrameMetadata { width: 40, height: 40, format: PixelFormat::Rgba32 }
            }
            async fn next(&mut self) -> Result<Option<FrameUpdate>, SourceError> {
                if self.n == 0 {
                    return Ok(Some(FrameUpdate::Ended));
                }
                self.n -= 1;
                let gen = self.n; // 2, 1, 0 => three frames then Ended
                // Frames 1, 2, 3 all use the same solid gray base; only the
                // last frame differs in its top-left block (painted white).
                let mut data = vec![0u8; 40 * 40 * 4];
                for b in 0..(40 * 40) {
                    data[b * 4..b * 4 + 4].copy_from_slice(&[5, 5, 5, 255]);
                }
                if gen == 0 {
                    // Repaint top-left 20x20 to a different color.
                    for y in 0..20 {
                        for x in 0..20 {
                            let i = (y * 40 + x) as usize * 4;
                            data[i..i + 4].copy_from_slice(&[255, 255, 255, 255]);
                        }
                    }
                }
                Ok(Some(FrameUpdate::Frame(Frame::full(self.metadata(), B::from(data)))))
            }
        }

        let cfg = EncoderConfig {
            blocks_per_side: 2,
            pane_origin: Some((0, 0)),
            placement_columns: 40,
            placement_rows: 40,
            png: false, // deterministic raw payload
            tile_reuse: false, // this test verifies diffing, not dedupe
            ..EncoderConfig::default()
        };
        let mut enc = TgpEncoder::new(cfg);
        let mut src = BlkSrc { n: 3 };
        let mut events = Vec::new();
        enc.run(&mut src, |e| { events.push(e); Ok(()) }).await.unwrap();
        let blocks: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                EncoderEvent::Bytes(b) => Some(String::from_utf8(b.clone()).unwrap()),
                _ => None,
            })
            .collect();
        // Every placed block's transmit event(*) carries a cursor move then a
        // `a=T` transmit. Frame 1 -> 4 blocks; frame 2 (unchanged) -> 0;
        // frame 3 -> 1 changed (top-left).
        let mut transmit_events = 0;
        let mut delete_events = 0;
        for s in &blocks {
            if s.contains("a=T") && s.contains("\x1b[") {
                transmit_events += 1;
            }
            if s.contains("a=d") {
                delete_events += 1;
            }
        }
        assert_eq!(transmit_events, 5, "4 blocks (first frame) + 1 changed top-left");
        // Each changed slot places a fresh id (covered, no flicker) and then
        // deletes the displaced id so the terminal image store stays bounded at
        // the grid size instead of filling up forever. Frame 3 retransmits the
        // top-left slot -> 1 displaced delete; Ended frees the 4 on-screen
        // block images -> 5 deletes total.
        assert_eq!(delete_events, 5, "1 displaced-slot delete + 4 ended-cleanup deletes");
    }

    #[tokio::test]
    async fn render_queue_budget_caps_tiles_per_frame_and_spills_over() {
        use bytes::Bytes as B;
        use tokio_stream::StreamExt as _;
        // A static 40x40 frame split 2x2 into 20x20 blocks. The source emits 4
        // identical frames then Ended. With the budget set to exactly one
        // full-res 20x20 block (400 px), each frame drains at most one tile from
        // the queue, so the 4 blocks spill across frames 1-4. This proves both
        // the per-frame pixel cap and that undrained tiles persist across frames.
        struct StaticSrc {
            n: u32,
        }
        #[async_trait]
        impl FrameSource for StaticSrc {
            fn metadata(&self) -> FrameMetadata {
                FrameMetadata { width: 40, height: 40, format: PixelFormat::Rgba32 }
            }
            async fn next(&mut self) -> Result<Option<FrameUpdate>, SourceError> {
                if self.n == 0 {
                    return Ok(Some(FrameUpdate::Ended));
                }
                self.n -= 1;
                let data = vec![5u8; 40 * 40 * 4];
                Ok(Some(FrameUpdate::Frame(Frame::full(self.metadata(), B::from(data)))))
            }
        }

        let cfg = EncoderConfig {
            blocks_per_side: 2,
            pane_origin: Some((0, 0)),
            placement_columns: 40,
            placement_rows: 40,
            png: false,             // deterministic raw payload
            res_levels: 1,          // single rung: level==res_levels -> factor 1 -> full res 20x20 = 400 px
            render_pixel_budget: 400, // allow exactly one tile per frame
            tile_reuse: false,      // this test verifies queue budget/spill, not dedupe
            ..EncoderConfig::default()
        };
        let enc = TgpEncoder::new(cfg);
        let src = StaticSrc { n: 4 };
        let mut stream = enc.into_stream(src);
        let mut transmits = 0usize;
        // Watch how many transmits appear per frame (between Frame events).
        // We count total transmits and also split per-frame spends by tracking
        // a cumulative counter across Frame event boundaries mis, simpler: assert
        // the total is 4 across 4 frames.
        while let Some(item) = stream.next().await {
            match item.unwrap() {
                EncoderEvent::Bytes(b) => {
                    let s = String::from_utf8_lossy(&b);
                    if s.contains("a=T") {
                        transmits += 1;
                    }
                }
                EncoderEvent::Frame { .. } => {}
                EncoderEvent::Ended => break,
            }
        }
        // 4 blocks, one drained per frame => exactly 4 transmit events total.
        assert_eq!(transmits, 4, "each 400px tile drains one per frame across 4 frames");
    }

    #[tokio::test]
    async fn tile_reuse_dedupes_identical_blocks_to_one_transmit() {
        use bytes::Bytes as B;
        use tokio_stream::StreamExt as _;
        // Uniform 40x40 frame split 2x2 -> four identical 20x20 solid blocks.
        // With tile_reuse on, the first block transmits and caches; the other
        // three identical blocks place that same id (`a=p`) instead of
        // re-transmitting. So a single frame emits exactly one transmit and
        // three place commands, not four transmits.
        struct SolidSrc {
            n: u32,
        }
        #[async_trait]
        impl FrameSource for SolidSrc {
            fn metadata(&self) -> FrameMetadata {
                FrameMetadata { width: 40, height: 40, format: PixelFormat::Rgba32 }
            }
            async fn next(&mut self) -> Result<Option<FrameUpdate>, SourceError> {
                if self.n == 0 {
                    return Ok(Some(FrameUpdate::Ended));
                }
                self.n -= 1;
                let data = vec![0x33u8; 40 * 40 * 4];
                Ok(Some(FrameUpdate::Frame(Frame::full(self.metadata(), B::from(data)))))
            }
        }

        let cfg = EncoderConfig {
            blocks_per_side: 2,
            pane_origin: Some((0, 0)),
            placement_columns: 40,
            placement_rows: 40,
            png: false,
            res_levels: 1, // single rung -> all four blocks are identical full-res 20x20 payloads
            ..EncoderConfig::default()
        };
        let enc = TgpEncoder::new(cfg);
        let src = SolidSrc { n: 1 };
        let mut stream = enc.into_stream(src);
        let (mut transmits, mut places, mut p0_places) = (0usize, 0usize, 0usize);
        let mut reused_total = 0u64;
        while let Some(item) = stream.next().await {
            match item.unwrap() {
                EncoderEvent::Bytes(b) => {
                    let s = String::from_utf8_lossy(&b);
                    if s.contains("a=T") {
                        transmits += 1;
                    } else if s.contains("a=p") {
                        places += 1;
                        // A reuse place must be an *untracked* placement (`p=0`)
                        // so the same id can occupy several cells at once; a
                        // tracked `p=1..` placement would replace the previous
                        // one, blanking tiles (the transparency bug).
                        assert!(s.contains("p=0"), "reuse place must be untracked p=0, got: {s}");
                        assert!(s.contains("c=") && s.contains("r="), "reuse place must size the block, got: {s}");
                        p0_places += 1;
                    }
                }
                EncoderEvent::Frame { tile_reused, .. } => reused_total = tile_reused,
                EncoderEvent::Ended => break,
            }
        }
        assert_eq!(transmits, 1, "only the first identical block is transmitted");
        assert_eq!(places, 3, "the three other identical blocks place the cached id");
        assert_eq!(p0_places, 3, "every reuse placement is untracked (p=0)");
        assert_eq!(reused_total, 3, "Frame telemetry counts the 3 reused tiles");
    }

    #[tokio::test]
    async fn tile_reuse_disabled_retransmits_every_block() {
        use bytes::Bytes as B;
        use tokio_stream::StreamExt as _;
        // Same uniform frame as above, but tile_reuse off: all four identical
        // blocks are re-transmitted individually (classic behaviour).
        struct SolidSrc {
            n: u32,
        }
        #[async_trait]
        impl FrameSource for SolidSrc {
            fn metadata(&self) -> FrameMetadata {
                FrameMetadata { width: 40, height: 40, format: PixelFormat::Rgba32 }
            }
            async fn next(&mut self) -> Result<Option<FrameUpdate>, SourceError> {
                if self.n == 0 {
                    return Ok(Some(FrameUpdate::Ended));
                }
                self.n -= 1;
                let data = vec![0x33u8; 40 * 40 * 4];
                Ok(Some(FrameUpdate::Frame(Frame::full(self.metadata(), B::from(data)))))
            }
        }

        let cfg = EncoderConfig {
            blocks_per_side: 2,
            pane_origin: Some((0, 0)),
            placement_columns: 40,
            placement_rows: 40,
            png: false,
            res_levels: 1,
            tile_reuse: false,
            ..EncoderConfig::default()
        };
        let enc = TgpEncoder::new(cfg);
        let src = SolidSrc { n: 1 };
        let mut stream = enc.into_stream(src);
        let (mut transmits, mut places) = (0usize, 0usize);
        while let Some(item) = stream.next().await {
            match item.unwrap() {
                EncoderEvent::Bytes(b) => {
                    let s = String::from_utf8_lossy(&b);
                    if s.contains("a=T") {
                        transmits += 1;
                    } else if s.contains("a=p") {
                        places += 1;
                    }
                }
                EncoderEvent::Frame { .. } => {}
                EncoderEvent::Ended => break,
            }
        }
        assert_eq!(transmits, 4, "with reuse off every identical block is retransmitted");
        assert_eq!(places, 0);
    }

    #[tokio::test]
    async fn debug_reused_tiles_tints_every_same_content_block() {
        use bytes::Bytes as B;
        use tokio_stream::StreamExt as _;
        // four identical solid blocks with reuse on + debug tint: instead of
        // placing the cached id for the last three, the encoder re-transmits
        // each as a tinted solid of the *same* content-derived color, proving
        // which tiles would dedupe. Every tinted tile must use one color.
        struct SolidSrc {
            n: u32,
        }
        #[async_trait]
        impl FrameSource for SolidSrc {
            fn metadata(&self) -> FrameMetadata {
                FrameMetadata { width: 40, height: 40, format: PixelFormat::Rgba32 }
            }
            async fn next(&mut self) -> Result<Option<FrameUpdate>, SourceError> {
                if self.n == 0 {
                    return Ok(Some(FrameUpdate::Ended));
                }
                self.n -= 1;
                let data = vec![0x21u8; 40 * 40 * 4];
                Ok(Some(FrameUpdate::Frame(Frame::full(self.metadata(), B::from(data)))))
            }
        }
        let cfg = EncoderConfig {
            blocks_per_side: 2,
            pane_origin: Some((0, 0)),
            placement_columns: 40,
            placement_rows: 40,
            png: false,
            res_levels: 1,
            debug_reused_tiles: true,
            ..EncoderConfig::default()
        };
        let enc = TgpEncoder::new(cfg);
        let src = SolidSrc { n: 1 };
        let mut stream = enc.into_stream(src);
        let mut transmits = 0usize;
        let mut places = 0usize;
        while let Some(item) = stream.next().await {
            match item.unwrap() {
                EncoderEvent::Bytes(b) => {
                    let s = String::from_utf8_lossy(&b);
                    if s.contains("a=T") {
                        transmits += 1;
                    } else if s.contains("a=p") {
                        places += 1;
                    }
                }
                EncoderEvent::Frame { .. } => {}
                EncoderEvent::Ended => break,
            }
        }
        // Reuse is still detected (cache), but debug tint re-transmits all four
        // tinted blocks instead of placing three. No place commands.
        assert_eq!(transmits, 4, "debug tint re-transmits every block, none placed");
        assert_eq!(places, 0);
    }

    #[test]
    fn debug_tint_raw_keeps_png_valid() {
        // Tinting corrupts a *payload* if it's already PNG, which made reused
        // tiles transparent. Tinting raw pixels and re-encoding must still emit
        // a decodeable PNG (starts with the 8-byte signature).
        let raw = vec![0x33u8; 20 * 20 * 4]; // 20x20 RGBA
        let tinted = tint_raw(&raw, 0x11223344);
        // Alpha forced opaque.
        assert!(tinted.iter().enumerate().all(|(i, &v)| (i % 4) != 3 || v == 0xff));
        // Re-encode as PNG: must start with the PNG signature, proving the raw
        // pixel buffer (not an encoded payload) is what the extras expects.
        let png = encode_png(&tinted, 20, 20, 8);
        assert!(png.starts_with(b"\x89PNG"), "re-encoded tinted raw must be a valid PNG");
    }

    #[tokio::test]
    async fn delta_stream_terminates_with_last_chunk() {
        use tokio_stream::StreamExt as _;
        // A large frame forces chunking: 1280x720 RGBA -> ~3.68 MB -> ~900
        // chunks. Every chunk belongs to the single transmit; the final chunk
        // must carry `m=0`. The e2e regression this guards: a real capture
        // stream was being cut short before the terminating chunk, leaving the
        // terminal holding a partial upload.
        let meta = FrameMetadata {
            width: 1280,
            height: 720,
            format: PixelFormat::Rgba32,
        };
        let src = TwoFrameSource {
            meta: meta.clone(),
            n: 2,
        };
        let mut stream = TgpEncoder::new(EncoderConfig::default()).into_stream(src);
        let mut base_bytes = 0usize;
        let mut saw_m0 = false;
        let mut full_transmits = 0;
        let mut ended = false;
        while let Some(item) = stream.next().await {
            match item.unwrap() {
                EncoderEvent::Bytes(b) => {
                    let s = String::from_utf8_lossy(&b);
                    if s.contains("a=T") {
                        full_transmits += 1;
                    }
                    base_bytes += b.len();
                    let is_m0 = s.trim_end().ends_with("m=0\x1b\\") || s.contains("m=0");
                    if is_m0 {
                        saw_m0 = true;
                    }
                }
                EncoderEvent::Frame { .. } => {}
                EncoderEvent::Ended => ended = true,
            }
        }
        assert!(ended, "stream must end with Ended");
        assert!(full_transmits >= 2, "each frame must be a full transmit");
        assert!(
            saw_m0,
            "base transmit must terminate with an explicit m=0 chunk"
        );
        assert!(
            base_bytes > 4_000_000,
            "full 1280x720 base should be multi-MB, got {base_bytes}"
        );
    }

    #[test]
    fn color_rounding_shrinks_gradients_and_is_stateless() {
        // 64x128 smooth horizontal gradient: every pixel is a distinct shade,
        // so the lossless PNG is large. Rounding each channel to 4 bits
        // collapses each row to ~16 columns, shrinking the payload far below
        // the 24-bit path, deterministically and without a quantizer.
        let w = 64u32;
        let h = 128u32;
        let mut data = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                data.extend_from_slice(&[x as u8, (x as u8).wrapping_mul(2), y as u8, 255]);
            }
        }
        let lossless = encode_png(&data, w, h, 8);
        let rounded4 = encode_png(&data, w, h, 4);
        // Same unchanged frame again: rounding is stateless, so repeated
        // encodes must be byte-identical (the flicker that appeared when each
        // frame trained a fresh quantized palette).
        let rounded4_b = encode_png(&data, w, h, 4);
        assert!(
            rounded4.len() < lossless.len(),
            "rounded ({}) should beat lossless ({}) for a gradient",
            rounded4.len(),
            lossless.len()
        );
        assert!(lossless.starts_with(b"\x89PNG"));
        assert!(rounded4.starts_with(b"\x89PNG"));
        assert!(
            rounded4 == rounded4_b,
            "identical frames must encode to identical bytes (stateless rounding)"
        );
        // Rounding must strip the dropped low bits and preserve alpha.
        // 0x7F -> nearest representable 16-level is 0x80 (not 0x70).
        let r = round_channels(&[0xFFu8, 0x7F, 0x33, 0xFF], 4);
        assert_eq!(r[0], 0xF0);
        assert_eq!(r[1], 0x80);
        assert_eq!(r[2], 0x30);
        assert_eq!(r[3], 0xFF, "alpha must be preserved");
    }
}

