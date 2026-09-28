//! Real `wlr-screencopy` capture (M1): connects as a Wayland client to the
//! headless session, binds `zwlr_screencopy_manager_v1`, copies the single
//! output into a wl_shm buffer, converts XRGB8888 to RGBA, and emits frames.

use std::fs::File;
use std::os::unix::io::{AsFd, FromRawFd};
use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use memmap2::MmapMut;
use tuigui_streamer::frame::CaptureTiming;
use tuigui_streamer::{Cadence, Frame, FrameMetadata, FrameSource, FrameUpdate, PixelFormat, Rect};
use wayland_client::delegate_noop;
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::wl_buffer;
use wayland_client::protocol::wl_output;
use wayland_client::protocol::wl_registry;
use wayland_client::protocol::wl_shm::{self, Format};
use wayland_client::protocol::wl_shm_pool;
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, WEnum};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1, zwlr_screencopy_manager_v1,
};

use crate::CageError;

/// One captured frame: RGBA8, row-major, top-left origin.
#[derive(Debug, Clone)]
pub struct Screenshot {
    pub width: u32,
    pub height: u32,
    /// `width * height * 4` bytes.
    pub rgba: Vec<u8>,
    /// Per-phase capture timing breakdown.
    pub timing: Option<CaptureTiming>,
    /// Damage regions reported by the compositor (pixel coords). May be empty.
    pub damage: Vec<Rect>,
}

impl Screenshot {
    /// Encode the captured RGBA pixels into a PNG file at `path` (overwrites).
    pub fn write_png(&self, path: impl AsRef<std::path::Path>) -> Result<(), CageError> {
        let file = File::create(path.as_ref())?;
        let mut enc = png::Encoder::new(file, self.width, self.height);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header()?;
        let data =
            &self.rgba[..(self.width as usize * self.height as usize * 4).min(self.rgba.len())];
        writer.write_image_data(data)?;
        Ok(())
    }

    pub fn to_frame(&self) -> Frame {
        Frame {
            metadata: FrameMetadata {
                width: self.width,
                height: self.height,
                format: PixelFormat::Rgba32,
            },
            data: bytes::Bytes::copy_from_slice(&self.rgba),
            presentation_timestamp: Some(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or(Duration::ZERO),
            ),
            timing: self.timing,
            damage: self.damage.clone(),
        }
    }
}

/// Connection parameters for the capture client.
#[derive(Debug, Clone)]
pub struct CaptureConfig {
    /// Absolute path to the session's Wayland socket.
    pub socket: PathBuf,
    /// Poll interval for the live source (target frame period). `max_fps` maps
    /// to this: `1000 / max_fps` ms.
    pub poll_interval: Duration,
    /// Downscale factor for captured frames (1.0 = full size, 2.0 = half).
    /// Applied after capture so the encoder sees fewer pixels regardless of the
    /// compositor's output resolution.
    pub scale: f32,
}

impl CaptureConfig {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        CaptureConfig {
            socket: socket.into(),
            poll_interval: Duration::from_millis(Self::default_max_fps_ms()),
            scale: 1.0,
        }
    }

    /// Default frame rate (30 fps) expressed as a millisecond period.
    fn default_max_fps_ms() -> u64 {
        33
    }

    /// Set the capture's maximum frame rate, overriding the default 30 fps.
    pub fn max_fps(mut self, fps: u32) -> Self {
        self.poll_interval = Duration::from_millis(1000 / fps.max(1) as u64);
        self
    }

    /// Set the frame downscale factor (>=1.0). Higher values shrink each
    /// captured frame, reducing encoder and sink load.
    pub fn scale(mut self, s: f32) -> Self {
        self.scale = s.max(1.0);
        self
    }
}

/// Live client-side state for a pending screencopy frame.
#[derive(Default)]
struct FrameRequest {
    width: u32,
    height: u32,
    stride: u32,
    format: Option<Format>,
    y_invert: bool,
    damage: Vec<Rect>,
    ready: bool,
    failed: bool,
    /// Source file the pool was created from; kept alive so the mmap stays valid.
    _file: Option<File>,
}

/// Small dispatch state that owns only frame results. It is the `State` of the
/// event queue, so its `Dispatch` impls get `&mut CaptureState` and never
/// alias the proxy handles (which live in `ScreenDriver`).
pub struct CaptureState {
    pending: Option<FrameRequest>,
}

impl CaptureState {
    fn new() -> Self {
        CaptureState { pending: None }
    }

    /// Take a completed frame out of the state.
    fn take_done(&mut self) -> Option<FrameRequest> {
        if self.pending.as_ref().map(|p| p.ready).unwrap_or(false) {
            self.pending.take()
        } else {
            None
        }
    }

    fn is_failed(&self) -> bool {
        self.pending.as_ref().map(|p| p.failed).unwrap_or(false)
    }

    fn start(&mut self, file: Option<File>) {
        self.pending = Some(FrameRequest {
            _file: file,
            ..FrameRequest::default()
        });
    }

    /// Record the pool file (set after we learn dims).
    fn set_file(&mut self, file: Option<File>) {
        if let Some(p) = self.pending.as_mut() {
            p._file = file;
        }
    }

    /// Return the announced dimensions once the `buffer` event has arrived.
    fn get_dims(&self) -> Option<(u32, u32)> {
        self.pending
            .as_ref()
            .map(|p| (p.width, p.height))
            .filter(|&(w, h)| w > 0 && h > 0)
    }

    fn get_format(&self) -> Option<Format> {
        self.pending.as_ref().and_then(|p| p.format)
    }

    fn get_stride(&self) -> Option<u32> {
        self.pending.as_ref().map(|p| p.stride).filter(|&s| s > 0)
    }

    /// Marker hook used to require a dispatch before checking dims.
    #[allow(dead_code)]
    fn ready_for_copy_meta(&mut self) {}
}

/// Internal driver that owns the Wayland connection, the proxies, and the
/// dispatch state — but keeps the queue's `State` separate so dispatch never
/// aliases the proxies.
pub struct ScreenDriver {
    _connection: Connection,
    queue: EventQueue<CaptureState>,
    manager: zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
    output: wl_output::WlOutput,
    shm: wl_shm::WlShm,
    state: CaptureState,
}

impl ScreenDriver {
    /// Connect to `socket`, bind the globals, and yield a ready driver.
    pub fn connect(cfg: &CaptureConfig) -> Result<Self, CageError> {
        if !cfg.socket.exists() {
            return Err(CageError::Capture(format!(
                "socket {} does not exist",
                cfg.socket.display()
            )));
        }
        let dir = cfg.socket.parent().unwrap().to_path_buf();
        let name = cfg
            .socket
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        std::env::set_var("XDG_RUNTIME_DIR", &dir);
        std::env::set_var("WAYLAND_DISPLAY", &name);

        let conn = Connection::connect_to_env().map_err(|e| CageError::Capture(e.to_string()))?;
        let (globals, mut queue) = registry_queue_init::<CaptureState>(&conn)
            .map_err(|e| CageError::Capture(format!("registry init: {e}")))?;

        let qh = queue.handle();
        let manager = globals
            .bind::<zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1, _, _>(&qh, 1..=3, ())
            .map_err(|e| CageError::Capture(format!("bind manager: {e}")))?;
        let output = globals
            .bind::<wl_output::WlOutput, _, _>(&qh, 1..=1, ())
            .map_err(|e| CageError::Capture(format!("bind output: {e}")))?;
        let shm = globals
            .bind::<wl_shm::WlShm, _, _>(&qh, 1..=1, ())
            .map_err(|e| CageError::Capture(format!("bind shm: {e}")))?;

        let mut state = CaptureState::new();
        queue
            .roundtrip(&mut state)
            .map_err(|e| CageError::Capture(e.to_string()))?;

        Ok(ScreenDriver {
            _connection: conn.clone(),
            queue,
            manager,
            output,
            shm,
            state,
        })
    }

    /// Capture the whole output exactly once and return the RGBA pixels,
    /// along with a per-phase timing breakdown.
    pub fn grab_once(&mut self) -> Result<Screenshot, CageError> {
        let qh = self.queue.handle();
        tracing::debug!("capture: begin");

        let mut t_phase = std::time::Instant::now();

        // overlay_cursor=1 so the headless cursor (if any) is baked in. This
        // only creates the frame object; the compositor will announce the
        // real output dimensions via the `buffer` event next.
        let frame = self.manager.capture_output(1, &self.output, &qh, ());
        self.state.start(None);

        // Phase 1: dispatch until the `buffer` (dimensions + format) event
        // arrives; the elapsed time is the announce cost. Bounded so a
        // compositor that stalls (weston momentarily drops output on transient
        // damage) can't hang the grab forever and freeze the whole session.
        let announce_ms = {
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while self.state.get_dims().is_none() || self.state.get_format().is_none() {
                if std::time::Instant::now() >= deadline {
                    self.state.pending = None;
                    return Err(CageError::Capture("timed out waiting for screencopy buffer announce".into()));
                }
                self.queue
                    .blocking_dispatch(&mut self.state)
                    .map_err(|e| CageError::Capture(e.to_string()))?;
            }
            t_phase.elapsed().as_secs_f64() * 1000.0
        };
        t_phase = std::time::Instant::now();

        // The compositor dictates the pixel format and layout; honor it. These
        // can be absent if a frame races to Ready/Failed before the `buffer`
        // event announced dims; treat that as a capture error rather than
        // panicking, since a panicking encoder task silently ends the whole
        // stream.
        let (w, h) = self
            .state
            .get_dims()
            .ok_or_else(|| CageError::Capture("screencopy frame ready before buffer dims".into()))?;
        let fmt = self
            .state
            .get_format()
            .ok_or_else(|| CageError::Capture("screencopy frame ready before buffer format".into()))?;
        // The compositor may hand us a packed 24-bit row (rgb888/bgr888) as
        // well as the usual 32-bit formats; honor its bytes-per-pixel so a
        // legitimate 3-byte row is not mistaken for a broken one.
        let bpp = match format_bpp(fmt) {
            b @ (3 | 4) => b,
            other => {
                self.state.pending = None;
                return Err(CageError::Capture(format!(
                    "unsupported screencopy bytes-per-pixel {other} (format {fmt:?})"
                )));
            }
        };
        let stride = self.state.get_stride().unwrap_or(w * bpp as u32);
        // `stride` and the pool size must be sane. wl_shm's create_pool takes a
        // signed 32-bit `size`; if stride*height overflows i32 the value wraps
        // negative and the server rejects it with "invalid arguments" (which,
        // on a large or mis-reported output, recurs every frame). Guard the
        // arithmetic and sanity-check the dims before handing them to the
        // compositor. A row is tightly at least `w * bpp` bytes; padding is
        // allowed but a stride below that means broken dims.
        if w == 0 || h == 0 || stride < w * bpp as u32 {
            self.state.pending = None;
            return Err(CageError::Capture(format!(
                "implausible screencopy dims: {w}x{h}, stride {stride}"
            )));
        }
        let size64 = stride as u64 * h as u64;
        if size64 > i32::MAX as u64 {
            self.state.pending = None;
            return Err(CageError::Capture(format!(
                "screencopy pool too large for wl_shm: {size64} bytes ({w}x{h}, stride {stride})"
            )));
        }
        let size = size64 as usize;
        tracing::debug!(w, h, stride, size, "screencopy shm pool");
        // Back the wl_shm pool with an anonymous memfd (RAM only) instead of a
        // disk tempfile, so the compositor's frame is never read off disk.
        let file = memfd_file()?;
        // Size the pool backing before exposing it to the compositor.
        file.set_len(size as u64).map_err(CageError::Io)?;
        let pool = self.shm.create_pool(file.as_fd(), size as i32, &qh, ());
        let buffer = pool.create_buffer(0, w as i32, h as i32, stride as i32, fmt, &qh, ());

        self.state.set_file(Some(file));
        frame.copy(&buffer);
        let setup_ms = t_phase.elapsed().as_secs_f64() * 1000.0;
        t_phase = std::time::Instant::now();

        // Phase 2: dispatch until ready / failed; the elapsed time is the
        // compositor's copy cost. The readout cost is measured inside
        // `build_screenshot`. Bounded for the same reason as phase 1: a stalled
        // compositor must not pin the encoder task forever.
        let (monotonic_copy_ms, mut shot) = 'phase2: loop {
            if std::time::Instant::now() >= t_phase + Duration::from_secs(2) {
                self.state.pending = None;
                return Err(CageError::Capture("timed out waiting for screencopy frame ready".into()));
            }
            self.queue
                .blocking_dispatch(&mut self.state)
                .map_err(|e| CageError::Capture(e.to_string()))?;
            if self.state.is_failed() {
                self.state.pending = None;
                return Err(CageError::Capture("screencopy frame failed".into()));
            }
            if let Some(req) = self.state.take_done() {
                let copy_ms = t_phase.elapsed().as_secs_f64() * 1000.0;
                let shot = build_screenshot(&req)
                    .ok_or_else(|| CageError::Capture("screencopy produced no bytes".into()))?;
                break 'phase2 (copy_ms, shot);
            }
        };
        // Build the phase breakdown: readout was measured inside
        // build_screenshot, the other phases are captured here.
        let mut timing = CaptureTiming {
            announce_ms,
            setup_ms,
            copy_ms: monotonic_copy_ms,
            ..CaptureTiming::default()
        };
        if let Some(rt) = shot.timing.take() {
            timing.readout_ms = rt.readout_ms;
        }
        shot.timing = Some(timing);
        tracing::debug!(
            ?timing,
            "capture phase timings"
        );
        Ok(shot)
    }
}

impl Dispatch<zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1, ()> for CaptureState {
    fn event(
        state: &mut Self,
        _: &zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(pending) = state.pending.as_mut() else {
            return;
        };
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer {
                format,
                width,
                height,
                stride,
            } => {
                tracing::debug!(
                    ?format,
                    width,
                    height,
                    stride,
                    "screencopy buffer announced"
                );
                if let WEnum::Value(format) = format {
                    pending.format = Some(format);
                }
                pending.width = width;
                pending.height = height;
                pending.stride = stride;
            }
            zwlr_screencopy_frame_v1::Event::Flags { flags } => {
                pending.y_invert = matches!(
                    flags,
                    WEnum::Value(f) if f == zwlr_screencopy_frame_v1::Flags::YInvert
                );
            }
            zwlr_screencopy_frame_v1::Event::Damage {
                x,
                y,
                width,
                height,
            } => {
                pending.damage.push(Rect {
                    x,
                    y,
                    width,
                    height,
                });
            }
            zwlr_screencopy_frame_v1::Event::Ready { .. } => pending.ready = true,
            zwlr_screencopy_frame_v1::Event::Failed => pending.failed = true,
            _ => {}
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for CaptureState {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
// Pool / buffer / output / shm / manager objects are dispatched without events.
delegate_noop!(CaptureState: ignore zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1);
delegate_noop!(CaptureState: ignore wl_shm_pool::WlShmPool);
delegate_noop!(CaptureState: ignore wl_shm::WlShm);
delegate_noop!(CaptureState: ignore wl_output::WlOutput);
delegate_noop!(CaptureState: ignore wl_buffer::WlBuffer);

/// An anonymous, RAM-backed file for use as the `wl_shm` pool backing. Uses
/// `memfd_create`, so the buffer lives in page cache / anonymous memory and is
/// never read from or written to disk. Falls back to a plain anonymous tmpfile
/// if the memfd syscall is unavailable (older kernels / unusual sandboxes).
fn memfd_file() -> Result<File, CageError> {
    // SAFETY: memfd_create is a libc syscall; MFD_CLOEXEC keeps the fd from
    // leaking into children. The returned fd is owned by the File we wrap.
    let fd = unsafe {
        libc::memfd_create(
            c"tuigui-shm".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    if fd >= 0 {
        return Ok(unsafe { File::from_raw_fd(fd) });
    }
    // Fall back to an anonymous tempfile if memfd_create isn't supported.
    tempfile::tempfile().map_err(CageError::Io)
}

/// Bytes per pixel for the wl_shm formats this capture path can decode. Only
/// the 24-bit (3) and 32-bit (4) RGBA-compatible family (with either byte order
/// and with or without alpha) are supported.
fn format_bpp(fmt: Format) -> u8 {
    use Format::*;
    match fmt {
        // 24-bit packed RGB, no alpha slot.
        Rgb888 | Bgr888 => 3,
        // 32-bit RGB(A).
        Xrgb8888 | Xbgr8888 | Argb8888 | Abgr8888 => 4,
        // Anything else (e.g. 16-bit formats, XR2101010 10-bit, planar/_a8) is
        // not handled by the hardcoded byte layouts below; report 0 and let the
        // caller reject gracefully rather than misdecoding pixels.
        _ => 0,
    }
}

/// Build an RGBA Screenshot by reading the mapped shm pool.
fn build_screenshot(req: &FrameRequest) -> Option<Screenshot> {
    let w = req.width.max(1);
    let h = req.height.max(1);
    let pixel_fmt = req.format.unwrap_or(Format::Argb8888);
    let bpp = format_bpp(pixel_fmt).max(1);
    let stride = req.stride.max(w * bpp as u32);
    let size = (stride as usize) * (h as usize);

    // The pool is backed by the temp file; map it and read the pixels the
    // compositor wrote. `_file` stays alive for the duration of the map.
    let map = req._file.as_ref()?;
    let mmap = unsafe { MmapMut::map_mut(map) };
    let mmap = match mmap {
        Ok(m) => m,
        Err(e) => {
            tracing::error!("mmap failed: {e}");
            return None;
        }
    };
    tracing::debug!(
        len = mmap.len(),
        need = size,
        w,
        h,
        stride,
        "screencopy buffer mapped"
    );
    if mmap.len() < size {
        return None;
    }

    // wl_shm formats use little-endian memory layout, so the colour byte order
    // matches the fourcc letters left-to-right:
    //   XBGR8888 / ABGR8888 : word 0x00BBGGRR -> [R, G, B, X|A]  (== RGBA)
    //   XRGB8888 / ARGB8888 : word 0x00RRGGBB -> [B, G, R, X|A]  (R/B swapped)
    //   BGR888 (24-bit)     : [R, G, B] tightly packed            (== RGBA)
    //   RGB888 (24-bit)     : [B, G, R] tightly packed            (R/B swapped)
    // The X (opaque) formats have no alpha; force it to opaque.
    let native_rgba_order =
        matches!(pixel_fmt, Format::Xbgr8888 | Format::Abgr8888 | Format::Bgr888);
    let has_alpha = matches!(pixel_fmt, Format::Argb8888 | Format::Abgr8888);
    let mut rgba = vec![0u8; (w as usize) * (h as usize) * 4];
    let t_read = std::time::Instant::now();

    // Fast path: pixel byte order already matches RGBA and rows are tightly
    // packed (stride == w * bpp) with no y-flip. Then each row is a bulk
    // copy and the only fix-up is setting alpha per pixel.
    if !req.y_invert && stride == w * bpp as u32 && native_rgba_order {
        if bpp == 4 && has_alpha {
            // ABGR8888: stored R,G,B,A already — plain memcpy.
            rgba.copy_from_slice(&mmap[..size]);
        } else if bpp == 4 {
            // XBGR8888: stored R,G,B,X — copy then force the X byte to 0xff.
            rgba.copy_from_slice(&mmap[..size]);
            let words = unsafe {
                std::slice::from_raw_parts_mut(rgba.as_mut_ptr() as *mut u32, rgba.len() / 4)
            };
            let fill = u32::from_le_bytes([0, 0, 0, 0xff]);
            for w_ in words {
                *w_ = (*w_ & 0x00ff_ffff) | fill;
            }
        } else {
            // 24-bit packed (bgr888): tightly [R,G,B] per pixel, no alpha.
            // Splice alpha=0xff after every 3 bytes to expand to RGBA.
            let mut rgba2 = Vec::with_capacity((w as usize) * (h as usize) * 4);
            let need = (w as usize) * (h as usize) * 3;
            for px in mmap[..need].chunks_exact(3) {
                rgba2.extend_from_slice(&[px[0], px[1], px[2], 0xff]);
            }
            let readout_ms = t_read.elapsed().as_secs_f64() * 1000.0;
            tracing::debug!(readout_ms, "screencopy pixels read out (bulk 24-bit)");
            return Some(Screenshot {
                width: w,
                height: h,
                rgba: rgba2,
                timing: Some(CaptureTiming {
                    readout_ms,
                    ..CaptureTiming::default()
                }),
                damage: req.damage.clone(),
            });
        }
        let readout_ms = t_read.elapsed().as_secs_f64() * 1000.0;
        tracing::debug!(readout_ms, "screencopy pixels read out (bulk)");
        return Some(Screenshot {
            width: w,
            height: h,
            rgba,
            timing: Some(CaptureTiming {
                readout_ms,
                ..CaptureTiming::default()
            }),
            damage: req.damage.clone(),
        });
    }

    // Slow path: handle y-flip, non-tight stride, and R/B-swapped formats by
    // processing each pixel, one `bpp`-byte source pixel into 4 RGBA bytes.
    for row in 0..h as usize {
        // The server may report y-inverted contents; read bottom-up if so.
        let src_row = if req.y_invert {
            h as usize - 1 - row
        } else {
            row
        };
        let src_off = src_row * stride as usize;
        let dst_off = row * w as usize * 4;
        for col in 0..w as usize {
            let p = src_off + col * bpp as usize;
            let d = dst_off + col * 4;
            match bpp {
                3 => {
                    if native_rgba_order {
                        // bgr888: [R,G,B].
                        rgba[d] = mmap[p];
                        rgba[d + 1] = mmap[p + 1];
                        rgba[d + 2] = mmap[p + 2];
                    } else {
                        // rgb888: [B,G,R] -> write R,G,B.
                        rgba[d] = mmap[p + 2];
                        rgba[d + 1] = mmap[p + 1];
                        rgba[d + 2] = mmap[p];
                    }
                    rgba[d + 3] = 0xff;
                }
                _ => {
                    if native_rgba_order {
                        rgba[d] = mmap[p];
                        rgba[d + 1] = mmap[p + 1];
                        rgba[d + 2] = mmap[p + 2];
                        rgba[d + 3] = if has_alpha { mmap[p + 3] } else { 0xff };
                    } else {
                        // R/B swapped: XRGB stored as B,G,R,A.
                        rgba[d] = mmap[p + 2];
                        rgba[d + 1] = mmap[p + 1];
                        rgba[d + 2] = mmap[p];
                        rgba[d + 3] = if has_alpha { mmap[p + 3] } else { 0xff };
                    }
                }
            }
        }
    }
    let readout_ms = t_read.elapsed().as_secs_f64() * 1000.0;
    tracing::debug!(readout_ms, "screencopy pixels read out");
    Some(Screenshot {
        width: w,
        height: h,
        rgba,
        timing: Some(CaptureTiming {
            readout_ms,
            ..CaptureTiming::default()
        }),
        damage: req.damage.clone(),
    })
}

/// Wrap the driver as a [`FrameSource`] for the TGP encoder.
pub struct CageFrameSource {
    driver: ScreenDriver,
    /// The config used to (re)connect the driver. Kept so a poisoned Wayland
    /// connection — a server-raised protocol error on `create_pool`, which
    /// makes the connection unusable and would otherwise fail every subsequent
    /// grab identically — can be torn down and rebuilt instead of retried.
    cfg: CaptureConfig,
    /// Minimum wall-clock gap between emitted frames (target frame period).
    frame_interval: Duration,
    /// When the previous frame was emitted, for pacing.
    prev_emit: Option<std::time::Instant>,
    /// True once a non-blank frame has been emitted. Until then the source
    /// discards blank frames: at boot the app hasn't painted yet, and letting a
    /// black frame through as the encoder's first frame poisons the quantized
    /// palette (everything stays black). Falls back to emitting whatever is on
    /// screen after a short budget so genuinely unlit apps still render.
    seen_content: bool,
    /// Birth time, for the blank-skip budget window.
    boot_start: std::time::Instant,
    /// Most recent captured pixel dims, so `metadata()` reflects the real
    /// output instead of a hardcoded guess. `None` until the first grab.
    last_dims: Option<(u32, u32)>,
    /// Frame downscale factor (>=1.0). Applied to each captured frame before
    /// emission so the encoder sees fewer pixels than the compositor output.
    scale: f32,
}

impl CageFrameSource {
    pub fn connect(cfg: &CaptureConfig) -> Result<Self, CageError> {
        Ok(CageFrameSource {
            driver: ScreenDriver::connect(cfg)?,
            cfg: cfg.clone(),
            frame_interval: cfg.poll_interval,
            prev_emit: None,
            seen_content: false,
            boot_start: std::time::Instant::now(),
            last_dims: None,
            scale: cfg.scale,
        })
    }
}

/// True when a frame is entirely (or almost entirely) black — the signature of
/// a pre-paint capture frame.
fn frame_blank(frame: &Frame) -> bool {
    const SAMPLE: usize = 64;
    let data = &frame.data;
    let px = data.len() / 4;
    if px == 0 {
        return true;
    }
    let step = (px / SAMPLE).max(1);
    for (i, px0) in data.as_chunks::<4>().0.iter().enumerate().step_by(step) {
        if i >= SAMPLE {
            break;
        }
        // Treat near-black as blank too: a fully black boot frame often has
        // off-black gamma/alpha rounding in a few bytes.
        if px0[0] > 8 || px0[1] > 8 || px0[2] > 8 {
            return false;
        }
    }
    true
}

/// Nearest-neighbor downscale of a captured screenshot by integer `scale`
/// (>=1). Produces a new Screenshot of `w/scale x h/scale` with box-sampled
/// pixels, so the encoder's resolution reflects the requested scale even when
/// the compositor fixed its output size.
fn downscale_frame(shot: &Screenshot, scale: f32) -> Frame {
    let ow = shot.width;
    let oh = shot.height;
    let nw = ((ow as f32) / scale).floor().max(1.0) as u32;
    let nh = ((oh as f32) / scale).floor().max(1.0) as u32;
    let mut out = vec![0u8; (nw as usize) * (nh as usize) * 4];
    let sx = ow as f32 / nw as f32;
    let sy = oh as f32 / nh as f32;
    for y in 0..nh {
        for x in 0..nw {
            let sy0 = ((y as f32) * sy) as usize;
            let sx0 = ((x as f32) * sx) as usize;
            let src = &shot.rgba[(sy0 * ow as usize + sx0) * 4..][..4];
            let dst = &mut out[((y as usize) * nw as usize + x as usize) * 4..][..4];
            dst.copy_from_slice(src);
        }
    }
    Frame {
        metadata: FrameMetadata {
            width: nw,
            height: nh,
            format: PixelFormat::Rgba32,
        },
        data: bytes::Bytes::from(out),
        presentation_timestamp: shot
            .to_frame()
            .presentation_timestamp,
        timing: shot.timing,
        damage: shot.damage.clone(),
    }
}

/// A synchronous capture handle: connect once, then grab one screenshot at a
/// time. Used by `--debug-capture` to write PNGs without the TGP stream layer.
pub struct PageSource {
    driver: ScreenDriver,
}

impl PageSource {
    pub fn connect(cfg: &CaptureConfig) -> Result<Self, CageError> {
        Ok(PageSource {
            driver: ScreenDriver::connect(cfg)?,
        })
    }

    /// Synchronously capture the current output once and return the pixels.
    pub fn capture_once(&mut self) -> Result<Screenshot, CageError> {
        self.driver.grab_once()
    }
}

#[async_trait]
impl FrameSource for CageFrameSource {
    fn metadata(&self) -> FrameMetadata {
        let (width, height) = self.last_dims.unwrap_or((0, 0));
        FrameMetadata {
            width,
            height,
            format: PixelFormat::Rgba32,
        }
    }

    fn cadence(&self) -> Cadence {
        Cadence::Live
    }

    async fn next(&mut self) -> Result<Option<FrameUpdate>, tuigui_streamer::SourceError> {
        // Rate-limit the producer to the configured frame interval so it can't
        // outrun the sink. Without this, a fast capture floods the encoder and
        // the (unbounded) channel piles up faster than the terminal can drain,
        // pushing FPS down and latency up as the backlog grows.
        if let Some(prev) = self.prev_emit {
            let elapsed = prev.elapsed();
            if elapsed < self.frame_interval {
                tokio::time::sleep(self.frame_interval - elapsed).await;
            }
        }
        let shot = match self.driver.grab_once() {
            Ok(shot) => shot,
            Err(e) => {
                // A failed grab is usually a transient screencopy hiccup, but a
                // wayland protocol error (e.g. invalid arguments on create_pool)
                // poisons the whole connection: every subsequent request on it
                // fails identically forever, freezing the stream. To recover we
                // must drop the dead connection and reconnect a fresh driver,
                // then retry the grab once, rather than keep poking a poisoned
                // socket.
                tracing::warn!(err = %e, "capture: grab failed, reconnecting");
                match ScreenDriver::connect(&self.cfg) {
                    Ok(driver) => self.driver = driver,
                    Err(re) => {
                        tracing::error!(err = %re, "capture: reconnect failed");
                        self.prev_emit = Some(std::time::Instant::now());
                        return Ok(Some(FrameUpdate::Idle));
                    }
                }
                match self.driver.grab_once() {
                    Ok(shot) => shot,
                    Err(e2) => {
                        tracing::warn!(err = %e2, "capture: grab failed after reconnect");
                        self.prev_emit = Some(std::time::Instant::now());
                        return Ok(Some(FrameUpdate::Idle));
                    }
                }
            }
        };
        self.prev_emit = Some(std::time::Instant::now());
        let frame = if self.scale > 1.0 {
            downscale_frame(&shot, self.scale)
        } else {
            shot.to_frame()
        };
        self.last_dims = Some((frame.metadata.width, frame.metadata.height));

        // Root cause of the boot race: a capture grab often catches the app before
        // its first paint, a fully black frame. Emitting that as the encoder's
        // first frame would show a blank screen. Discard blank frames until real
        // content appears (or a timeout budget elapses, so genuinely unlit apps
        // still show).
        let blank = frame_blank(&frame);
        if !self.seen_content {
            const BLANK_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);
            if blank {
                if self.boot_start.elapsed() < BLANK_BUDGET {
                    tracing::debug!("capture: boot frame blank, skipping until content");
                    return Ok(Some(FrameUpdate::Idle));
                }
            } else {
                tracing::debug!("capture: first content frame");
            }
        }
        self.seen_content = true;
        Ok(Some(FrameUpdate::Frame(frame)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screenshot_to_frame_preserves_damage() {
        let s = Screenshot {
            width: 2,
            height: 2,
            rgba: vec![0; 16],
            timing: None,
            damage: vec![Rect {
                x: 0,
                y: 0,
                width: 2,
                height: 1,
            }],
        };
        let f = s.to_frame();
        assert_eq!(f.metadata.format, PixelFormat::Rgba32);
        assert_eq!(f.effective_damage().len(), 1);
    }

    #[test]
    fn blank_detection_distinguishes_pre_paint_black_from_content() {
        let meta = FrameMetadata {
            width: 8,
            height: 8,
            format: PixelFormat::Rgba32,
        };
        // All black -> blank (the pre-paint signature).
        let black = Frame::full(meta.clone(), vec![0u8; 8 * 8 * 4].into());
        assert!(frame_blank(&black), "all-black frame must be blank");
        // One bright pixel anywhere -> content.
        let mut px = vec![0u8; 8 * 8 * 4];
        px[0] = 255; // R channel of first pixel bright.
        let content = Frame::full(meta.clone(), px.into());
        assert!(!frame_blank(&content), "frame with a bright pixel is content");
    }

    #[test]
    fn format_bpp_classifies_supported_shm_formats() {
        use Format::*;
        // 24-bit packed, no alpha slot -> 3 bytes/pixel.
        assert_eq!(format_bpp(Rgb888), 3);
        assert_eq!(format_bpp(Bgr888), 3);
        // 32-bit RGBA family, either byte order or with/without alpha -> 4.
        assert_eq!(format_bpp(Xrgb8888), 4);
        assert_eq!(format_bpp(Xbgr8888), 4);
        assert_eq!(format_bpp(Argb8888), 4);
        assert_eq!(format_bpp(Abgr8888), 4);
        // Unsupported formats must report 0 so grab_once rejects them cleanly
        // instead of misdecoding pixels (a packed 24-bit row was once miscounted
        // as 4 bpp and rejected as "implausible screencopy dims").
        assert_eq!(format_bpp(Xrgb4444), 0);
        assert_eq!(format_bpp(Xrgb2101010), 0);
    }
}
