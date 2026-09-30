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
        /// Number of blocks edited (transmitted) in the last frame.
        /// `0` when not in block mode.
        tiles_edited: u32,
        /// Total block grid slots (blocks per side squared). `0` when not in
        /// block mode.
        tiles_total: u32,
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
    /// bits snap the pixel to the nearest representable shade. Unused when
    /// [`png`](/self#structfield-png) is false.
    pub color_bits: u8,
    /// The animation backend's block grid: the canvas is split into an
    /// `N x N` grid and each changed block is diffed and edited in place with
    /// `a=f` animation-frame commands. `10` (default) = a 10x10 grid (100
    /// blocks). The encoder never falls back to whole-frame transmit+place.
    pub blocks_per_side: u32,
    /// The downscale factor applied to the *loading* layer — a coarse copy of the
    /// canvas stacked on top (front, z=1) of the full-res sharp layer (z=0).
    /// While a block changes it is shown coarse on the loading layer; once it
    /// settles the block's rect is cleared there to transparent so the sharp
    /// layer shows through. `0` (default) disables the loading layer entirely
    /// (single-layer animation backend).
    pub loading_scale: u32,
    /// Detail-adaptive resolution ceilings: each block's
    /// largest neighbor color difference picks its ceiling on a linear ladder
    /// (see `spec/LOD.md`). Disabled keeps the classic always-full ladder.
    pub detail: tuigui_streamer::blocks::DetailConfig,
    /// Tint each transmitted block's payload red in
    /// proportion to its resolution level (level 1 untouched, higher = more
    /// red). Visually proves what resolution each block settles at.
    pub debug_lod: bool,
    /// Colorize every other block (linear grid
    /// index odd) with a per-block pseudo-random color that changes each frame,
    /// and leave even blocks as the real pixels. Visually proves the block grid
    /// and the per-block diffing. `false` (default) transmits true pixels.
    pub debug_blocks: bool,
    /// The pane's top-left cell in terminal
    /// coordinates (0-based, as ratatui reports). Unused here (the pane is
    /// positioned once by the host TUI); kept for API stability.
    pub pane_origin: Option<(u16, u16)>,
    /// Opt-in zlib (RFC 1950) compression of transmitted payloads via `o=z`.
    /// Off by default. When set, the payload is deflated before base64.
    pub zlib: bool,
}

impl Default for EncoderConfig {
    fn default() -> Self {
        EncoderConfig {
            placement_columns: 0,
            placement_rows: 0,
            png: false,
            color_bits: 8,
            blocks_per_side: 10,
            loading_scale: 0,
            detail: Default::default(),
            debug_lod: false,
            debug_blocks: false,
            pane_origin: None,
            zlib: false,
        }
    }
}


/// Converts a [`FrameSource`] into a stream of TGP bytes.
///
/// In block mode the encoder keeps two full-canvas `W x H` animation images
/// (each an animation root whose frame 1 is the canvas, edited in place via
/// `a=f`): a full-res **sharp** layer (z=0) and a **loading** layer (z=1).
/// The loading layer is the same pixel size as sharp so every block rect is
/// 1:1 aligned between them (no scale-coordinate drift), but its content is
/// coarse: each changed block is downscaled by `loading_scale`. Settling a
/// block replaces its loading rect with a transparent PNG so sharp shows through.
pub struct TgpEncoder {
    config: EncoderConfig,
    image_id: u32,
    /// Running frame count, used as a seed for `--debug-blocks` colorization.
    frame_count: u64,
    /// Block differ: the animation backend's change detector. Built with
    /// `res_levels=1` (pure change detection) — the ladder is not used; the
    /// loading layer provides all LOD via `loading_scale`.
    blocks: tuigui_streamer::blocks::BlockGrid,
    /// Full-res sharp canvas image id (z=0). The source of truth.
    sharp_id: Option<u32>,
    /// Whether the sharp canvas has been transmitted+placed yet.
    sharp_placed: bool,
    /// Downscaled loading canvas image id (z=1), only when `loading_scale > 0`.
    loading_id: Option<u32>,
    /// Whether the loading canvas has been transmitted+placed yet.
    loading_placed: bool,
    /// Per-slot: is this block currently shown coarse on the loading layer
    /// (i.e. its rect is NOT yet cleared to transparent)? A changed block is
    /// written coarse here and marked; the frame it stops changing it is
    /// cleared to reveal the sharp layer behind.
    loading_covered: Vec<bool>,
}

impl TgpEncoder {
    pub fn new(config: EncoderConfig) -> Self {
        // Pure change detection (res_levels=1): the two-layer loading
        // scheme does LOD by downscaling whole blocks onto the coarse layer,
        // not via a per-block sharpening ladder. The animation backend is the
        // only encode path, so a block grid is mandatory.
        let n = config.blocks_per_side.max(1);
        let blocks = tuigui_streamer::blocks::BlockGrid::new(n, n, 1, config.detail, config.debug_lod);
        TgpEncoder {
            config,
            // Image ids are a shared namespace with other TGP programs; start
            // from a nonzero pseudorandom base to make collisions unlikely.
            image_id: 1 + (std::process::id() % 1000) * 7,
            frame_count: 0,
            blocks,
            sharp_id: None,
            sharp_placed: false,
            loading_id: None,
            loading_placed: false,
            loading_covered: Vec::new(),
        }
    }

    pub fn image_id(&self) -> u32 {
        self.image_id
    }

    /// Compose a changed block into the loading layer at the SAME full-size
    /// rect it occupies in sharp, but with blocky/coarse content: the block's
    /// pixels are downscaled by `loading_scale` then nearest-upscaled back to
    /// the block rect, so it renders low-res yet stays 1:1 aligned with sharp
    /// (no scale-coordinate drift, so nothing looks zoomed or misplaced).
    #[allow(clippy::too_many_arguments)]
    fn emit_loading_block(
        b: &tuigui_streamer::blocks::MicroBlock,
        loading_id: u32,
        scale: u32,
        color_bits: u8,
        zlib: bool,
    ) -> Vec<Vec<u8>> {
        let (down, _pw, _ph) = downscale_rgba(&b.data, b.payload_width, b.payload_height, scale);
        let blocky = upscale_nearest(&down, scale, b.payload_width, b.payload_height);
        frame_edit(loading_id, b.x, b.y, b.payload_width, b.payload_height, &blocky, color_bits, zlib, 32)
    }

    /// Emit an `a=f` edit that replaces a now-settled block's rect on the loading
    /// layer with a fully transparent (0,0,0,0) PNG, revealing the sharp layer
    /// behind via alpha compositing of the two placements. Uses the same full-size
    /// rect as the block so it aligns perfectly with sharp (and it's a PNG,
    /// per the agreed design).
    #[allow(clippy::too_many_arguments)]
    fn emit_clear_loading(
        block_w: u32,
        block_h: u32,
        x: u32,
        y: u32,
        loading_id: u32,
        color_bits: u8,
        zlib: bool,
    ) -> Vec<Vec<u8>> {
        let transparent = vec![0u8; (block_w * block_h) as usize * 4];
        frame_edit(loading_id, x, y, block_w, block_h, &transparent, color_bits, zlib, 100)
    }

    /// Encode the full lifetime of a source into events. Drives `source` to
    /// completion; use [`TgpEncoder::into_stream`] for the Stream API.
    pub async fn run<S: FrameSource>(
        &mut self,
        source: &mut S,
        mut on_event: impl FnMut(EncoderEvent) -> Result<(), EncoderError>,
    ) -> Result<(), EncoderError> {
        let mut last_meta: Option<FrameMetadata> = None;
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
                FrameUpdate::Resized(_m) => on_event(EncoderEvent::Bytes(Vec::new()))?,
                FrameUpdate::Idle => continue,
                FrameUpdate::Ended => {
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
                    let t_encode = Instant::now();
                    let grid_total = self.blocks.cols() * self.blocks.rows();
                    let edited_this_frame: u32;
                    {
                        let seed = self.frame_count.wrapping_add(1);
                        let n_slots = (grid_total as usize).max(1);
                        let grid = &mut self.blocks;
                        let changed = grid.diff(&frame, self.config.debug_blocks, seed);
                        edited_this_frame = (changed.len() as u32).min(grid_total);
                        if self.loading_covered.len() != n_slots {
                            self.loading_covered = vec![false; n_slots];
                        }
                        // Which slots changed this frame? (for reveal decisions)
                        let mut cur_changed = vec![false; n_slots];
                        for b in &changed {
                            let slot = (b.row * grid.rows() + b.col) as usize;
                            if slot < n_slots {
                                cur_changed[slot] = true;
                            }
                        }

                        // Establish the sharp canvas (z=0, full res) on frame 1.
                        if !self.sharp_placed {
                            let sid = self.image_id;
                            self.image_id = self.image_id.wrapping_add(1);
                            for bytes in full_frame_commands(
                                &frame,
                                &base_control(&frame.metadata, sid, placement_columns, placement_rows, 0),
                                self.config.png,
                                self.config.color_bits,
                                self.config.zlib,
                            ) {
                                on_event(EncoderEvent::Bytes(bytes))?;
                            }
                            self.sharp_id = Some(sid);
                            self.sharp_placed = true;
                        }
                        let sharp_id = self.sharp_id.unwrap_or(0);

                        // Establish the loading canvas (z=1, full-size) on frame 1.
                        let scale = self.config.loading_scale.max(1);
                        let loading_id = if self.config.loading_scale > 0 {
                            if !self.loading_placed {
                                // Transmit a full-size W x H loading root frame whose
                                // content is a coarse (blocky) downscale of the whole
                                // canvas, so it stacks exactly over sharp (1:1 rects).
                                let lid = self.image_id;
                                self.image_id = self.image_id.wrapping_add(1);
                                let (down, _dw, _dh) = downscale_rgba(
                                    &frame.data,
                                    frame.metadata.width,
                                    frame.metadata.height,
                                    scale,
                                );
                                let blocky = upscale_nearest(
                                    &down,
                                    scale,
                                    frame.metadata.width,
                                    frame.metadata.height,
                                );
                                let fmt = 32; // always RGBA
                                let mut ctrl: Vec<(char, String)> = vec![('a', "T".into()), ('C', "1".into())];
                                ctrl.extend(base_control(
                                    &frame.metadata,
                                    lid,
                                    placement_columns,
                                    placement_rows,
                                    1,
                                ));
                                for c in tgp::chunked_transmit(fmt, ctrl, &blocky, self.config.zlib) {
                                    on_event(EncoderEvent::Bytes(c))?;
                                }
                                self.loading_id = Some(lid);
                                self.loading_placed = true;
                                self.loading_covered = vec![true; n_slots];
                            }
                            self.loading_id.unwrap_or(0)
                        } else {
                            0
                        };
                        for b in &changed {
                            // sharp: full-res, 1:1.
                            for c in frame_edit(
                                sharp_id,
                                b.x,
                                b.y,
                                b.payload_width,
                                b.payload_height,
                                &b.data,
                                self.config.color_bits,
                                self.config.zlib,
                                if self.config.png { 100 } else { 32 },
                            ) {
                                on_event(EncoderEvent::Bytes(c))?;
                            }
                            if loading_id != 0 {
                                let slot = (b.row * grid.rows() + b.col) as usize;
                                for c in Self::emit_loading_block(
                                    b,
                                    loading_id,
                                    scale,
                                    self.config.color_bits,
                                    self.config.zlib,
                                ) {
                                    on_event(EncoderEvent::Bytes(c))?;
                                }
                                // Mark covered AFTER emitting so we don't hold a
                                // mutable borrow on self.loading_covered while
                                // emit_loading_block borrows self mutably.
                                self.loading_covered[slot] = true;
                            }
                        }

                        // Reveal settled blocks: a slot that was shown coarse last
                        // frame but did NOT change this frame is now stable —
                        // clear its rect to transparent on the loading layer so
                        // the sharp layer shows through.
                        if loading_id != 0 {
                            let bw = frame.metadata.width.div_ceil(grid.cols());
                            let bh = frame.metadata.height.div_ceil(grid.rows());
                            for slot in 0..n_slots {
                                if !self.loading_covered[slot] || cur_changed[slot] {
                                    continue;
                                }
                                self.loading_covered[slot] = false;
                                let col = (slot % grid.cols() as usize) as u32;
                                let row = (slot / grid.cols() as usize) as u32;
                                let bx = col * bw;
                                let by = row * bh;
                                for c in Self::emit_clear_loading(
                                    bw,
                                    bh,
                                    bx,
                                    by,
                                    loading_id,
                                    self.config.color_bits,
                                    self.config.zlib,
                                ) {
                                    on_event(EncoderEvent::Bytes(c))?;
                                }
                            }
                        }
                        self.frame_count += 1;
                    }
                    let encode_ms = t_encode.elapsed().as_secs_f64() * 1000.0;
                    let capture_ms = frame
                        .timing
                        .map(|t| t.total_ms())
                        .unwrap_or(poll_ms);
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
                        tiles_edited: edited_this_frame,
                        tiles_total: grid_total,
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
    let payload = if png {
        encode_png(&frame.data, frame.metadata.width, frame.metadata.height, color_bits)
    } else {
        frame.data.clone()
    };
    let mut out = Vec::new();
    for c in tgp::chunked_transmit(fmt, ctrl, &payload, zlib) {
        out.push(c);
    }
    out
}

/// Palette resume helper for a full-canvas image (sharp layer, z=0).
fn base_control(
    m: &FrameMetadata,
    image_id: u32,
    placement_columns: u32,
    placement_rows: u32,
    z: u32,
) -> Vec<(char, String)> {
    let mut c = vec![
        ('i', image_id.to_string()),
        ('s', m.width.to_string()),
        ('v', m.height.to_string()),
        ('z', z.to_string()),
    ];
    if placement_columns > 0 && placement_rows > 0 {
        c.push(('c', placement_columns.to_string()));
        c.push(('r', placement_rows.to_string()));
    }
    c.push(('p', 1u32.to_string()));
    c
}

/// Control block for a downscaled loading-canvas transmit (its intrinsic pixel
/// size is `dw x dh`, placed over the same pane but on z=1).
/// pixel payload into `image_id`'s root frame at `(x, y)` sized `w x h`.
/// 1:1 (no terminal upscale), `X=1` overwrite.
fn frame_edit(
    image_id: u32,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    payload: &[u8],
    color_bits: u8,
    zlib: bool,
    fmt: u32,
) -> Vec<Vec<u8>> {
    debug_assert!(fmt == 32 || fmt == 100, "frame_edit only supports raw RGBA or PNG");
    let ctrl: Vec<(char, String)> = vec![
        ('a', "f".into()),
        ('i', image_id.to_string()),
        ('r', "1".into()),
        ('x', x.to_string()),
        ('y', y.to_string()),
        ('s', w.to_string()),
        ('v', h.to_string()),
        ('X', "1".into()),
    ];
    let payload = if fmt == 100 {
        encode_png(payload, w, h, color_bits)
    } else if color_bits < 8 {
        // raw RGBA: round channels if requested
        bytes::Bytes::from(round_channels(payload, color_bits))
    } else {
        bytes::Bytes::copy_from_slice(payload)
    };
    tgp::chunked_transmit(fmt, ctrl, &payload, zlib)
}

/// Compress raw RGB/RGBA pixel data into a PNG byte string (in-memory).
///
/// RGB24 input is upgraded to RGBA (alpha=255) before encoding. The animation
/// backend applies `a=f` edits by alpha-compositing the transmitted block onto
/// the existing frame, so an edit lacking an alpha channel (an `ColorType::Rgb`
/// PNG) is dropped and the frame never repaints — only fades. Video sources
/// decode to RGB24, so they must be widened here or their in-place updates are
/// silently ignored (first frame shows, nothing after). `a=T` placements are
/// unaffected either way.
fn encode_png(data: &[u8], width: u32, height: u32, color_bits: u8) -> bytes::Bytes {
    let _ = color_bits; // rounding is applied before this call
    let pixels: Vec<u8> = if data.len() == (width * height * 4) as usize {
        data.to_vec()
    } else {
        // RGB24 -> RGBA: inject a full-alpha channel so in-place `a=f` edits
        // carry alpha and aren't dropped by the terminal's compositor.
        let mut out = Vec::with_capacity((width * height) as usize * 4);
        for px in data.chunks_exact(3) {
            out.extend_from_slice(&[px[0], px[1], px[2], 255]);
        }
        out
    };
    let mut buf = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut buf, width, height);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Default);
        enc.set_color(png::ColorType::Rgba);
        let mut w = enc.write_header().expect("png header");
        w.write_image_data(&pixels).expect("png data");
    }
    bytes::Bytes::from(buf)
}

fn round_channels(data: &[u8], bits: u8) -> Vec<u8> {
    debug_assert_eq!(data.len() % 4, 0);
    let bits = bits.clamp(1, 8);
    if bits == 8 {
        return data.to_vec();
    }
    let mut out = data.to_vec();
    let keep_mask = 0xffu8.wrapping_shl(8 - bits as u32);
    let round_amt = (0x80u8) >> bits;
    let max_in = 0xff - round_amt;
    let (pxs, _rest) = out.as_chunks_mut::<4>();
    for px in pxs {
        px[0] = px[0].min(max_in).wrapping_add(0 + round_amt) & keep_mask;
        px[1] = px[1].min(max_in).wrapping_add(round_amt) & keep_mask;
        px[2] = px[2].min(max_in).wrapping_add(round_amt) & keep_mask;
    }
    out
}

/// Downscale `data` (an `w x h` image of `bpp`-byte pixels) by `factor`,
/// averaging each `factor x factor` region into one output RGBA pixel. Always
/// returns 4 bpp (RGBA). `factor <= 1` returns `(data, w, h)` unchanged.
fn downscale_rgba(data: &[u8], w: u32, h: u32, factor: u32) -> (bytes::Bytes, u32, u32) {
    if factor <= 1 {
        // ensure 4bpp output; if input is already RGBA use it
        if data.len() == (w * h * 4) as usize {
            return (bytes::Bytes::copy_from_slice(data), w, h);
        }
        // RGB -> RGBA
        let mut out = Vec::with_capacity((w * h) as usize * 4);
        for px in data.chunks_exact(3) {
            out.extend_from_slice(&[px[0], px[1], px[2], 255]);
        }
        return (bytes::Bytes::from(out), w, h);
    }
    let pw = w.div_ceil(factor);
    let ph = h.div_ceil(factor);
    let mut out = vec![0u8; (pw * ph) as usize * 4];
    let bpp = if data.len() == (w * h * 4) as usize { 4usize } else { 3usize };
    for oy in 0..ph {
        let y0 = oy * factor;
        let y1 = (y0 + factor).min(h);
        for ox in 0..pw {
            let x0 = ox * factor;
            let x1 = (x0 + factor).min(w);
            let mut sums = [0u32; 4];
            let mut count = 0u32;
            for sy in y0..y1 {
                for sx in x0..x1 {
                    let o = ((sy * w + sx) as usize) * bpp;
                    sums[0] += data[o] as u32;
                    sums[1] += data[o + 1] as u32;
                    sums[2] += data[o + 2] as u32;
                    sums[3] += if bpp == 4 { data[o + 3] as u32 } else { 255 };
                    count += 1;
                }
            }
            let dst = ((oy * pw + ox) as usize) * 4;
            out[dst] = (sums[0] / count) as u8;
            out[dst + 1] = (sums[1] / count) as u8;
            out[dst + 2] = (sums[2] / count) as u8;
            out[dst + 3] = (sums[3] / count) as u8;
        }
    }
    (bytes::Bytes::from(out), pw, ph)
}

/// Nearest-neighbor upscale a coarse `pw x ph` RGBA payload (produced by
/// [`downscale_rgba`] with `factor=scale`) back to the full `w x h` block rect.
/// Each coarse pixel becomes a `scale x scale` block of identical RGBA pixels
/// (edge clipped), giving blocky low-res content that occupies exactly the same
/// pixel rect as the block's sharp version. Output is `w * h * 4` bytes.
fn upscale_nearest(data: &[u8], scale: u32, w: u32, h: u32) -> bytes::Bytes {
    let scale = scale.max(1);
    let mut out = vec![0u8; (w * h) as usize * 4];
    // data is the downscaled buffer; its per-pixel RGBA is indexed at
    // (y/scale)*pw + (x/scale).
    let pw = w.div_ceil(scale);
    let ph = h.div_ceil(scale);
    for y in 0..h {
        let sy = (y / scale).min(ph - 1);
        let srow = (sy * pw) as usize;
        for x in 0..w {
            let sx = (x / scale).min(pw - 1);
            let src = (srow + sx as usize) * 4;
            let dst = ((y * w + x) as usize) * 4;
            out[dst] = data[src];
            out[dst + 1] = data[src + 1];
            out[dst + 2] = data[src + 2];
            out[dst + 3] = data[src + 3];
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

    #[tokio::test]
    async fn two_layer_settles_changed_then_clears_loading_to_reveal_sharp() {
        use bytes::Bytes as B;
        // Frame 1: everything changed -> sharp canvas transmitted (z=0) + loading
        // canvas (z=1) + coarse edits for all blocks on the loading layer.
        // Frame 2: identical -> nothing changes, so every loading block rect is
        // cleared to transparent (a=f with an all-zero RGBA payload), revealing
        // the sharp layer. This is the "remove chunk from the closer image".
        struct Src { n: u32 }
        #[async_trait]
        impl FrameSource for Src {
            fn metadata(&self) -> FrameMetadata {
                FrameMetadata { width: 40, height: 40, format: PixelFormat::Rgba32 }
            }
            async fn next(&mut self) -> Result<Option<FrameUpdate>, SourceError> {
                if self.n == 0 { return Ok(Some(FrameUpdate::Ended)); }
                self.n -= 1;
                let data = vec![5u8; 40 * 40 * 4];
                Ok(Some(FrameUpdate::Frame(Frame::full(self.metadata(), B::from(data)))))
            }
        }
        let cfg = EncoderConfig {
            blocks_per_side: 2,
            loading_scale: 2,
            pane_origin: Some((0, 0)),
            placement_columns: 40,
            placement_rows: 40,
            png: false,
            ..EncoderConfig::default()
        };
        let enc = TgpEncoder::new(cfg);
        use tokio_stream::StreamExt as _;
        let src = Src { n: 3 };
        let mut stream = enc.into_stream(src);
        let mut roots = 0usize;
        let mut a_f = 0usize;
        let mut transmit_controls = Vec::new();
        while let Some(item) = stream.next().await {
            match item.unwrap() {
                EncoderEvent::Bytes(b) => {
                    let s = String::from_utf8_lossy(&b);
                    if s.contains("a=T") {
                        roots += 1;
                        let head = s.split(';').next().unwrap_or("").to_string();
                        transmit_controls.push(head);
                    }
                    if s.contains("a=f") {
                        a_f += 1;
                    }
                }
                EncoderEvent::Frame { .. } => {}
                EncoderEvent::Ended => break,
            }
        }
        // Sharp + loading canvases: 2 transmits total (once each).
        assert_eq!(roots, 2, "sharp + loading canvases each transmitted once");
        for ctrl in &transmit_controls {
            assert!(ctrl.contains("z=0") || ctrl.contains("z=1"), "each canvas places on a plane, got {ctrl}");
            assert!(ctrl.contains("s=40") && ctrl.contains("v=40"), "canvas source must be full 40x40, got {ctrl}");
            assert!(ctrl.contains("c=40") && ctrl.contains("r=40"), "canvas placement must fill the 40x40 pane, got {ctrl}");
        }
        // Frame 1: 4 sharp edits + 4 coarse loading edits = 8 a=f. Frame 2
        // (static): every block settled -> 4 clears (reveal) = 4. Total 12.
        assert_eq!(a_f, 12, "4 sharp + 4 coarse on frame 1, 4 clears frame 2, got {a_f}");
    }

    #[tokio::test]
    async fn single_layer_without_loading_scale_is_original_animation_backend() {
        use bytes::Bytes as B;
        // loading_scale=0 (default): only the sharp canvas exists; every changed
        // block is an a=f into it, and there are NO loading transmits or clears.
        struct Src { n: u32 }
        #[async_trait]
        impl FrameSource for Src {
            fn metadata(&self) -> FrameMetadata {
                FrameMetadata { width: 40, height: 40, format: PixelFormat::Rgba32 }
            }
            async fn next(&mut self) -> Result<Option<FrameUpdate>, SourceError> {
                if self.n == 0 { return Ok(Some(FrameUpdate::Ended)); }
                self.n -= 1;
                let data = vec![5u8; 40 * 40 * 4];
                Ok(Some(FrameUpdate::Frame(Frame::full(self.metadata(), B::from(data)))))
            }
        }
        let cfg = EncoderConfig {
            blocks_per_side: 2,
            loading_scale: 0,
            pane_origin: Some((0, 0)),
            placement_columns: 40,
            placement_rows: 40,
            png: false,
            ..EncoderConfig::default()
        };
        let enc = TgpEncoder::new(cfg);
        use tokio_stream::StreamExt as _;
        let src = Src { n: 3 };
        let mut stream = enc.into_stream(src);
        let mut roots = 0usize;
        let mut edits = 0usize;
        let mut ended = false;
        while let Some(item) = stream.next().await {
            match item.unwrap() {
                EncoderEvent::Bytes(b) => {
                    let s = String::from_utf8_lossy(&b);
                    if s.contains("a=T") { roots += 1; }
                    if s.contains("a=f") { edits += 1; }
                }
                EncoderEvent::Frame { .. } => {}
                EncoderEvent::Ended => ended = true,
            }
        }
        assert!(ended);
        // Exactly one full-canvas root (the single sharp layer).
        assert_eq!(roots, 1, "no loading layer when scale=0");
        // Frame 1: 4 edits; frame 2+ are static -> nothing.
        assert_eq!(edits, 4, "first frame edits every block into the single layer");
    }

    #[test]
    fn downscale_rgba_averages_regions_and_outputs_rgba() {
        // 2x2 image, factor 2 -> single RGBA pixel = average of all 4.
        let data = [
            0u8, 0, 0, 255,   100, 0, 0, 255,
            200, 0, 0, 255,   255, 0, 0, 255,
        ];
        let (out, pw, ph) = downscale_rgba(&data, 2, 2, 2);
        assert_eq!((pw, ph), (1, 1));
        // Average = (0+100+200+255)/4 = 138.75 -> 138.
        assert_eq!(out.len(), 4);
        assert_eq!(out[0], 138);
        assert_eq!(out[1], 0);
        assert_eq!(out[2], 0);
        assert_eq!(out[3], 255);
    }

    #[test]
    fn frame_edit_overwrites_root_frame_1x1() {
        let chunks = frame_edit(777, 10, 5, 4, 2, &[9u8; 4 * 2 * 4], 8, false, 32);
        let s = String::from_utf8(chunks.into_iter().next().unwrap()).unwrap();
        assert!(s.contains("a=f"));
        assert!(s.contains("i=777"));
        assert!(s.contains(",r=1,"));
        assert!(s.contains("x=10") && s.contains("y=5"));
        assert!(s.contains("s=4") && s.contains("v=2"));
        assert!(s.contains("X=1"));
    }

    
    /// Measure pure encoder production rate (no UI loop, no terminal sink): how
    /// fast can the two-layer backend emit edits for a 300x300 changing scene?
    /// This isolates the encoder from the consumer-loop cadence that throttles
    /// the UI (input poll + ratatui draw + flush per iteration).
    #[tokio::test]
    async fn encoder_throughput_churning_scene() {
        use bytes::Bytes as B;
        use tokio_stream::StreamExt as _;
        struct Churn { frames: u32 }
        #[async_trait]
        impl FrameSource for Churn {
            fn metadata(&self) -> FrameMetadata {
                FrameMetadata { width: 300, height: 300, format: PixelFormat::Rgba32 }
            }
            async fn next(&mut self) -> Result<Option<FrameUpdate>, SourceError> {
                if self.frames == 0 { return Ok(Some(FrameUpdate::Ended)); }
                self.frames -= 1;
                let w = 300u32;
                let mut data = vec![7u8; (w * w * 4) as usize];
                let v = (self.frames % 250) as u8;
                for r in 0..w { for c in 0..(w/2) {
                    let i = ((r*w + c) as usize) * 4;
                    data[i] = v; data[i+3] = 255;
                }}
                Ok(Some(FrameUpdate::Frame(Frame::full(self.metadata(), B::from(data)))))
            }
        }
        let cfg = EncoderConfig {
            blocks_per_side: 8,
            loading_scale: 2,
            pane_origin: Some((0, 0)),
            placement_columns: 80,
            placement_rows: 40,
            png: false,
            ..EncoderConfig::default()
        };
        let enc = TgpEncoder::new(cfg);
        let src = Churn { frames: 500 };
        let mut stream = enc.into_stream(src);
        let t0 = Instant::now();
        let mut frames = 0u64;
        let mut bytes = 0u64;
        let mut edits = 0u64;
        while let Some(item) = stream.next().await {
            match item.unwrap() {
                EncoderEvent::Bytes(b) => { if !b.is_empty() { bytes += b.len() as u64; edits += 1; } }
                EncoderEvent::Frame { .. } => frames += 1,
                EncoderEvent::Ended => break,
            }
        }
        let secs = t0.elapsed().as_secs_f64();
        eprintln!(
            "encoder-only: {} frames in {:.3}s = {:.0} fps, {:.0} edits/frame, {:.0} B/frame",
            frames, secs, frames as f64 / secs, edits as f64 / frames as f64, bytes as f64 / frames as f64
        );
        assert!(frames as f64 / secs > 65.0, "encoder should far exceed UI fps, got {:.0} fps", frames as f64 / secs);
    }

    /// Pure encoder rate for a single-tiny-change scene (the "even a single
    /// sub-block is 30ms sink / 50fps" case): proves the cost is loop overhead,
    /// not payload size, when the encoder alone can produce far faster.
    #[tokio::test]
    async fn encoder_throughput_single_block_change() {
        use bytes::Bytes as B;
        use tokio_stream::StreamExt as _;
        struct Flip { frames: u32, state: u8 }
        #[async_trait]
        impl FrameSource for Flip {
            fn metadata(&self) -> FrameMetadata {
                FrameMetadata { width: 300, height: 300, format: PixelFormat::Rgba32 }
            }
            async fn next(&mut self) -> Result<Option<FrameUpdate>, SourceError> {
                if self.frames == 0 { return Ok(Some(FrameUpdate::Ended)); }
                self.frames -= 1;
                let w = 300u32;
                let mut data = vec![7u8; (w * w * 4) as usize];
                self.state = self.state.wrapping_add(1);
                for r in 0..37 { for c in 0..37 {
                    let i = ((r*w + c) as usize) * 4;
                    data[i] = self.state; data[i+3] = 255;
                }}
                Ok(Some(FrameUpdate::Frame(Frame::full(self.metadata(), B::from(data)))))
            }
        }
        let cfg = EncoderConfig {
            blocks_per_side: 8,
            pane_origin: Some((0, 0)),
            placement_columns: 80,
            placement_rows: 40,
            png: false,
            ..EncoderConfig::default()
        };
        let enc = TgpEncoder::new(cfg);
        let src = Flip { frames: 1000, state: 0 };
        let mut stream = enc.into_stream(src);
        let t0 = Instant::now();
        let mut frames = 0u64;
        while let Some(item) = stream.next().await {
            match item.unwrap() {
                EncoderEvent::Bytes(_) => {}
                EncoderEvent::Frame { .. } => frames += 1,
                EncoderEvent::Ended => break,
            }
        }
        let secs = t0.elapsed().as_secs_f64();
        eprintln!(
            "encoder-only single-block: {} frames in {:.3}s = {:.0} fps",
            frames, secs, frames as f64 / secs
        );
        assert!(frames as f64 / secs > 200.0, "single-block scene must not be encoder-bound");
    }

}
