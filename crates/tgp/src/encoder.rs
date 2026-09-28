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
#[derive(Debug, Clone, Default)]
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
}

impl TgpEncoder {
    pub fn new(config: EncoderConfig) -> Self {
        let blocks = (config.blocks_per_side > 0).then(|| {
            tuigui_streamer::blocks::BlockGrid::new(
                config.blocks_per_side,
                config.blocks_per_side,
                config.res_levels,
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
        }
    }

    pub fn image_id(&self) -> u32 {
        self.image_id
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
                        let changed = grid.diff(&frame, self.config.debug_blocks, seed);
                        let origin = self.config.pane_origin.unwrap_or((0, 0));
                        let placement = BlockPlacement::new(
                            (frame.metadata.width, frame.metadata.height),
                            (placement_columns, placement_rows),
                        );
                        // Keep the on-screen id per grid slot in step with the
                        // grid dimensions (reset on layout change).
                        let n_slots = (grid.cols() * grid.rows()) as usize;
                        if self.block_ids.len() != n_slots {
                            self.block_ids.clear();
                            self.block_ids.resize(n_slots, None);
                        }
                        for b in &changed {
                            let slot = (b.row * grid.cols() + b.col) as usize;
                            let old = self.block_ids[slot].take();
                            // Place the new image FIRST (fresh id) so the slot
                            // is covered with no blank gap, then delete the
                            // displaced id so the store never grows unbounded.
                            let bid = self.image_id;
                            self.image_id = self.image_id.wrapping_add(1);
                            self.block_ids[slot] = Some(bid);
                            on_event(EncoderEvent::Bytes(transmit_block(
                                b,
                                &base_control_for_block(b, bid, placement),
                                self.config.png,
                                self.config.color_bits,
                                origin,
                                placement,
                            )))?;
                            if let Some(old) = old {
                                on_event(EncoderEvent::Bytes(delete_image(old)))?;
                            }
                        }
                        self.frame_count += 1;
                    } else {
                        for bytes in full_frame_commands(
                            &frame,
                            &base_control(&frame.metadata, cur_id, placement_columns, placement_rows),
                            self.config.png,
                            self.config.color_bits,
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
    for c in tgp::chunked_transmit(fmt, ctrl, &payload) {
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
) -> Vec<u8> {
    let (cs, rs) = placement.cell_origin(b.x, b.y);
    let cur_col = pane_origin.0 as u32 + cs;
    let cur_row = pane_origin.1 as u32 + rs;

    let mut out = Vec::new();
    // Move the cursor to the block's top-left cell (CSI r;c H).
    out.extend_from_slice(format!("\x1b[{};{}H", cur_row + 1, cur_col + 1).as_bytes());
    let mut ctrl: Vec<(char, String)> = vec![('a', "T".into()), ('C', "1".into())];
    ctrl.extend(control.iter().cloned());
    let fmt = if png {
        100u32
    } else {
        32
    };
    // Payload is the block's content at its (possibly downscaled) resolution.
    let payload = if png {
        encode_png(&b.data, b.payload_width, b.payload_height, color_bits)
    } else {
        b.data.clone()
    };
    for c in tgp::chunked_transmit(fmt, ctrl, &payload) {
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

fn quiet_probe_reposition() -> Vec<u8> {
    // Placeholder for re-placement after resize: the TUI layer owns cursor
    // management, so this is a no-op marker for now.
    Vec::new()
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
        assert!(strings[0].starts_with("\x1b_Ga=T"));
        assert!(strings[0].contains("a=T"));
        assert!(strings[3].starts_with("\x1b_Ga=d"));
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
        assert!(strings[0].starts_with("\x1b_Ga=T"));
        assert!(strings[0].contains("a=T"));
        assert!(strings[1].starts_with("\x1b_Ga=T"), "frame 2 must retransmit");
        assert!(strings[1].contains("a=T"));
        // Each frame must use a distinct image id (no delete-gap flicker).
        let id0 = id_of(&strings[0]);
        let id1 = id_of(&strings[1]);
        assert_ne!(id0, id1, "each frame must transmit under a fresh image id");
        // frame 2's delete targets the previous first-frame id.
        assert!(strings[2].starts_with("\x1b_Ga=d"));
        assert!(strings[2].contains(&format!("i={id0}")));
        // final delete removes the last placed image on Ended.
        assert!(strings[3].starts_with("\x1b_Ga=d"));
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

