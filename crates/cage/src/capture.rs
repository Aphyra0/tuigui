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
    /// Poll interval for the live source when there is no damage event.
    pub poll_interval: Duration,
}

impl CaptureConfig {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        CaptureConfig {
            socket: socket.into(),
            poll_interval: Duration::from_millis(33),
        }
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
        // arrives; the elapsed time is the announce cost.
        let announce_ms = {
            while self.state.get_dims().is_none() || self.state.get_format().is_none() {
                self.queue
                    .blocking_dispatch(&mut self.state)
                    .map_err(|e| CageError::Capture(e.to_string()))?;
            }
            t_phase.elapsed().as_secs_f64() * 1000.0
        };
        t_phase = std::time::Instant::now();

        // The compositor dictates the pixel format and layout; honor it.
        let (w, h) = self.state.get_dims().unwrap();
        let fmt = self.state.get_format().unwrap();
        let stride = self.state.get_stride().unwrap_or(w * 4);
        let size = (stride * h) as usize;
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
        // `build_screenshot`.
        let (monotonic_copy_ms, mut shot) = 'phase2: loop {
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

/// Build an RGBA Screenshot by reading the mapped shm pool.
fn build_screenshot(req: &FrameRequest) -> Option<Screenshot> {
    let w = req.width.max(1);
    let h = req.height.max(1);
    let stride = req.stride.max(w * 4);
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

    // wl_shm 32-bit formats use little-endian memory layout, so the colour byte
    // order matches the fourcc letters left-to-right:
    //   XBGR8888 / ABGR8888 : word 0x00BBGGRR -> [R, G, B, X|A]  (== RGBA)
    //   XRGB8888 / ARGB8888 : word 0x00RRGGBB -> [B, G, R, X|A]  (R/B swapped)
    // The X (opaque) formats have no alpha; force it to opaque.
    let pixel_fmt = req.format.unwrap_or(Format::Argb8888);
    let native_rgba_order = matches!(pixel_fmt, Format::Xbgr8888 | Format::Abgr8888);
    let has_alpha = matches!(pixel_fmt, Format::Argb8888 | Format::Abgr8888);
    let mut rgba = vec![0u8; (w as usize) * (h as usize) * 4];
    let t_read = std::time::Instant::now();

    // Fast path: pixel byte order already matches RGBA and rows are tightly
    // packed (stride == w*4) with no y-flip. Then the frame is literally the
    // first `size` bytes of the mmap — a single bulk copy, no per-pixel work.
    // For opaque formats the only fix-up is writing alpha=0xff (the 4th byte
    // of every pixel), done with a word-wise fill that skips the 4th byte.
    if !req.y_invert && stride == w * 4 && native_rgba_order {
        if has_alpha {
            // ABGR8888: stored R,G,B,A already — plain memcpy.
            rgba.copy_from_slice(&mmap[..size]);
        } else {
            // XBGR8888: stored R,G,B,X — copy then force the X byte to 0xff.
            rgba.copy_from_slice(&mmap[..size]);
            let words = unsafe {
                std::slice::from_raw_parts_mut(rgba.as_mut_ptr() as *mut u32, rgba.len() / 4)
            };
            let fill = u32::from_le_bytes([0, 0, 0, 0xff]);
            for w_ in words {
                *w_ = (*w_ & 0x00ff_ffff) | fill;
            }
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
    // processing each pixel, one 4-byte pixel per step.
    for row in 0..h as usize {
        // The server may report y-inverted contents; read bottom-up if so.
        let src_row = if req.y_invert {
            h as usize - 1 - row
        } else {
            row
        };
        let src_off = src_row * stride as usize;
        let dst_off = row * w as usize * 4;
        // Open-code the hot row loop instead of iterating over every pixel
        // individually: copy or transform one full 4-byte pixel per step.
        let n = w as usize;
        for col in 0..n {
            let p = src_off + col * 4;
            let d = dst_off + col * 4;
            if native_rgba_order {
                // No byte reorder, only an alpha fill to apply.
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
}

impl CageFrameSource {
    pub fn connect(cfg: &CaptureConfig) -> Result<Self, CageError> {
        Ok(CageFrameSource {
            driver: ScreenDriver::connect(cfg)?,
        })
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
        FrameMetadata {
            width: 1280,
            height: 800,
            format: PixelFormat::Rgba32,
        }
    }

    fn cadence(&self) -> Cadence {
        Cadence::Live
    }

    async fn next(&mut self) -> Result<Option<FrameUpdate>, tuigui_streamer::SourceError> {
        match self.driver.grab_once() {
            Ok(shot) => Ok(Some(FrameUpdate::Frame(shot.to_frame()))),
            Err(e) => Err(tuigui_streamer::SourceError::Transport(e.to_string())),
        }
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
}
