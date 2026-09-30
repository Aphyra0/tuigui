//! Per-window bookkeeping: the one admitted toplevel, its committed buffers,
//! pending frame callbacks, and damage accumulation.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use smithay::backend::allocator::format::get_bpp;
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_shm;
use smithay::utils::{Logical, Point, Rectangle, Size};
use smithay::wayland::shm;

use crate::server::{clamp_rect, WindowShared};
use crate::CompError;

/// The one admitted window.
pub struct Window {
    /// Shared with the `Client` API handle.
    pub shared: Arc<WindowShared>,
    /// The surface's own coordinate space == output space (scale 1, offset 0).
    pub geometry: Rectangle<i32, Logical>,
    /// Latest committed contents, owned by us, written on commit.
    pub pixels: Mutex<Vec<u8>>,
    /// Damage since the last capture (output pixel space).
    pub damage: Mutex<Vec<crate::Rect>>,
    /// Serial of the last configure sent (spawn waits for the first commit).
    pub last_configure_serial: Mutex<Option<u64>>,
}

impl Window {
    pub fn new(shared: Arc<WindowShared>) -> Self {
        let (w, h) = shared.size;
        Window {
            shared,
            geometry: Rectangle::from_size(Size::from((w as i32, h as i32))),
            pixels: Mutex::new(Vec::new()),
            damage: Mutex::new(Vec::new()),
            last_configure_serial: Mutex::new(None),
        }
    }

    pub fn mark_dead(&self) {
        self.shared.dead.store(true, Ordering::SeqCst);
    }
}

/// Convert a committed wl_shm buffer into RGBA8 pixels in `out`.
///
/// The output capture surface is ARGB8888 little-endian, i.e. bytes
/// B,G,R,A — which is exactly wire RGBA8. So a straight row copy of an
/// Argb8888/Xrgb8888 shm buffer already yields the target layout.
pub fn map_rgba(
    buffer: &WlBuffer,
    out: &mut Vec<u8>,
    out_size: (u32, u32),
) -> Result<(), CompError> {
    shm::with_buffer_contents(buffer, |ptr, len, data| {
        let bpp = wl_bpp(data.format);
        if bpp != 4 {
            // Unsupported layout for the MVP: leave the previous frame.
            return;
        }
        let stride = data.stride as usize;
        let (ow, oh) = (out_size.0 as usize, out_size.1 as usize);
        out.clear();
        out.resize(ow * oh * 4, 0);
        let copy_w = (data.width as usize).min(ow);
        let copy_h = (data.height as usize).min(oh);
        if copy_w == 0 || copy_h == 0 {
            return;
        }
        // Per-row memcpy: shm pools may be sparse, so never assume the row
        // stride makes rows contiguous.
        let base = data.offset as usize;
        unsafe {
            for row in 0..copy_h {
                if base + (row + 1) * stride > len {
                    break;
                }
                let src = ptr.add(base + row * stride);
                let dst = out.as_mut_ptr().add(row * ow * 4);
                std::ptr::copy_nonoverlapping(src, dst, copy_w * 4);
            }
        }
    })
    .map_err(|e| CompError::Capture(e.to_string()))
}

fn wl_bpp(f: wl_shm::Format) -> usize {
    shm::shm_format_to_fourcc(f)
        .and_then(|fourcc| get_bpp(fourcc))
        .map(|bpp| bpp / 8)
        .unwrap_or(0)
}

/// Extract commit damage as output-space pixel rects.
pub fn commit_damage(
    attrs: &smithay::wayland::compositor::SurfaceAttributes,
    out_size: (u32, u32),
    out: &mut Vec<crate::Rect>,
) {
    for d in &attrs.damage {
        if let smithay::wayland::compositor::Damage::Surface(rect) = d {
            if let Some(r) = clamp_rect(*rect, out_size) {
                out.push(r);
            }
        }
        // Buffer-space damage is skipped: the MVP only admits scale-1
        // untransformed buffers (buffer == surface space), and surface-space
        // rects already cover the common case.
    }
}

/// Whether the pointer is inside `window.geometry` at `point`.
pub fn window_contains(window: &Window, point: Point<f64, Logical>) -> bool {
    let g = &window.geometry;
    point.x >= g.loc.x as f64
        && point.x < (g.loc.x + g.size.w) as f64
        && point.y >= g.loc.y as f64
        && point.y < (g.loc.y + g.size.h) as f64
}
