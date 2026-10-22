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

/// Convert a committed wl_shm buffer into pixels in `out`, sized `out_size`.
///
/// The capture surface is ARGB8888 little-endian, i.e. bytes B,G,R,A. A
/// straight row copy of an Argb8888/Xrgb8888 shm buffer already yields that
/// layout, so no channel order is changed here.
///
/// The buffer is **scaled** to `out_size`, not cropped to it. A committed buffer
/// covers the whole surface and the surface covers the whole output, so the two
/// differ only in resolution: the client picks how many pixels one logical pixel
/// is worth (`wl_surface.set_buffer_scale`) and the output's density is another
/// number. Cropping assumes the two agree, which loses a client's right and
/// bottom edges when it renders at a higher density and leaves it in a corner
/// when it does not.
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
        // Every dimension is the client's, so none of them is trusted to be
        // positive or to fit. A buffer that cannot be read whole keeps the
        // previous frame rather than producing a torn one.
        if data.width <= 0 || data.height <= 0 || data.stride <= 0 || data.offset < 0 {
            return;
        }
        let (bw, bh) = (data.width as usize, data.height as usize);
        let stride = data.stride as usize;
        let base = data.offset as usize;
        let (ow, oh) = (out_size.0 as usize, out_size.1 as usize);
        if ow == 0 || oh == 0 {
            return;
        }
        // One row has to fit in its stride, and the last row in the pool.
        let fits = (bw * 4 <= stride)
            .then(|| bh.checked_mul(stride).and_then(|s| base.checked_add(s)))
            .flatten()
            .is_some_and(|end| end <= len);
        if !fits {
            return;
        }
        out.clear();
        out.resize(ow * oh * 4, 0);
        // The pool is handed to this closure for exactly this, and the bounds
        // are checked above, so the slice covers every byte that is read.
        let pool = unsafe { std::slice::from_raw_parts(ptr, len) };
        scale_bgra(&pool[base..], stride, (bw, bh), out, (ow, oh));
    })
    .map_err(|e| CompError::Capture(e.to_string()))
}

/// Resample one B,G,R,A buffer into another, of any size.
///
/// Each destination pixel is the source pixel it lands on. Nearest rather than
/// interpolated because what is being captured is an interface on its way to a
/// grid of terminal cells, where a pixel of blur costs legibility and buys
/// nothing.
///
/// `src` is `src_size.0 * 4` bytes of picture per row with `src_stride` bytes
/// between rows, and `dst` is written densely at `dst_size.0 * 4` per row.
fn scale_bgra(
    src: &[u8],
    src_stride: usize,
    src_size: (usize, usize),
    dst: &mut [u8],
    dst_size: (usize, usize),
) {
    let (bw, bh) = src_size;
    let (ow, oh) = dst_size;
    if bw == 0 || bh == 0 || ow == 0 || oh == 0 {
        return;
    }
    // Per-row memcpy: shm pools may be sparse, so never assume the row stride
    // makes rows contiguous.
    if bw == ow && bh == oh {
        for row in 0..oh {
            let from = row * src_stride;
            let to = row * ow * 4;
            dst[to..to + ow * 4].copy_from_slice(&src[from..from + ow * 4]);
        }
        return;
    }
    // One index per row and per column rather than a divide per pixel: at frame
    // sizes the divides are the whole capture.
    let cols: Vec<usize> = (0..ow).map(|x| x * bw / ow).collect();
    let rows: Vec<usize> = (0..oh).map(|y| y * bh / oh).collect();
    for (y, &sy) in rows.iter().enumerate() {
        let from = sy * src_stride;
        let to = y * ow * 4;
        for (x, &sx) in cols.iter().enumerate() {
            let s = from + sx * 4;
            let d = to + x * 4;
            dst[d..d + 4].copy_from_slice(&src[s..s + 4]);
        }
    }
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
    scale: f64,
    out: &mut Vec<crate::Rect>,
) {
    for d in &attrs.damage {
        if let smithay::wayland::compositor::Damage::Surface(rect) = d {
            if let Some(r) = clamp_rect(*rect, out_size, scale) {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A buffer whose every pixel carries its own coordinates, so a wrong
    /// mapping shows up as the wrong pixel rather than the wrong colour.
    fn tagged(w: usize, h: usize, stride: usize) -> Vec<u8> {
        let mut buf = vec![0u8; stride * h];
        for y in 0..h {
            for x in 0..w {
                let o = y * stride + x * 4;
                buf[o] = x as u8;
                buf[o + 1] = y as u8;
                buf[o + 2] = 0xa5;
                buf[o + 3] = 0xff;
            }
        }
        buf
    }

    fn pixel(buf: &[u8], stride: usize, x: usize, y: usize) -> [u8; 4] {
        let o = y * stride + x * 4;
        buf[o..o + 4].try_into().expect("four bytes")
    }

    #[test]
    fn the_same_size_is_copied_untouched() {
        let src = tagged(6, 4, 6 * 4);
        let mut dst = vec![0u8; 6 * 4 * 4];
        scale_bgra(&src, 6 * 4, (6, 4), &mut dst, (6, 4));
        assert_eq!(dst, src);
    }

    /// The size the output's scale produces: a client renders at twice the
    /// logical size it was configured with, so the buffer is larger than the
    /// output. The picture must be resampled across its whole width and height
    /// — which is what cropping got wrong.
    #[test]
    fn a_larger_buffer_is_scaled_down_across_its_whole_picture() {
        let (bw, bh) = (24, 16);
        let (ow, oh) = (20, 13);
        let src = tagged(bw, bh, bw * 4);
        let mut dst = vec![0u8; ow * oh * 4];
        scale_bgra(&src, bw * 4, (bw, bh), &mut dst, (ow, oh));

        assert_eq!(pixel(&dst, ow * 4, 0, 0), [0, 0, 0xa5, 0xff]);
        // The last output pixel reaches the far side of the buffer: column
        // 19 * 24 / 20 = 22, row 12 * 16 / 13 = 14. A crop left it at column 19
        // and row 12 of the buffer, which is the edge of the interface that went
        // missing.
        assert_eq!(pixel(&dst, ow * 4, ow - 1, oh - 1), [22, 14, 0xa5, 0xff]);
    }

    /// The other direction: a client that ignores the scale commits at the
    /// logical size, which is smaller than the output. The picture has to fill
    /// the frame rather than sit in the top-left corner of it.
    #[test]
    fn a_smaller_buffer_is_scaled_up_and_fills_the_frame() {
        let (bw, bh) = (10, 7);
        let (ow, oh) = (20, 13);
        let src = tagged(bw, bh, bw * 4);
        let mut dst = vec![0u8; ow * oh * 4];
        scale_bgra(&src, bw * 4, (bw, bh), &mut dst, (ow, oh));

        assert_eq!(pixel(&dst, ow * 4, 0, 0), [0, 0, 0xa5, 0xff]);
        assert_eq!(pixel(&dst, ow * 4, ow - 1, oh - 1), [9, 6, 0xa5, 0xff]);
        // The middle is the picture too, not the zero the old copy left behind.
        assert_eq!(pixel(&dst, ow * 4, 10, 6), [5, 3, 0xa5, 0xff]);
    }

    /// Rows are read where the stride says they are, so padding between them is
    /// never mistaken for picture.
    #[test]
    fn a_padded_stride_is_read_by_row() {
        let (bw, bh, stride) = (5, 3, 5 * 4 + 12);
        let src = tagged(bw, bh, stride);
        let mut dst = vec![0u8; bw * bh * 4];
        scale_bgra(&src, stride, (bw, bh), &mut dst, (bw, bh));

        for y in 0..bh {
            for x in 0..bw {
                assert_eq!(pixel(&dst, bw * 4, x, y), pixel(&src, stride, x, y));
            }
        }
    }

    /// A zero-sized side is not a picture, and must not reach the index maths.
    #[test]
    fn an_empty_side_writes_nothing() {
        let src = tagged(4, 4, 4 * 4);
        let mut dst = vec![0xffu8; 4 * 4 * 4];
        scale_bgra(&src, 4 * 4, (4, 4), &mut dst, (0, 4));
        assert!(dst.iter().all(|&b| b == 0xff), "an empty target was written");
    }
}
