//! Small helpers with no home of their own: the cursor overlay, and the
//! dispatch-forwarding macros our generic state needs in place of smithay's
//! `delegate_*!` macros (which take lifetimes, but not type generics).

use smithay::utils::{Logical, Point};

/// The connecting process's pid, straight from the socket (`SO_PEERCRED`).
/// Linux-only by the crate gate.
pub(crate) fn peer_pid(stream: &std::os::unix::net::UnixStream) -> Option<u32> {
    use std::os::unix::io::AsRawFd;

    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let ok = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    if ok == 0 {
        Some(cred.pid as u32)
    } else {
        None
    }
}

/// The cursor overlay: a red disc of radius 3 drawn onto a captured frame at
/// the pointer's position, so the pointer is visible even though the headless
/// output has no real cursor plane. `rgba` is `w * h * 4` bytes of Argb8888
/// little-endian.
impl crate::Frame {
    pub fn draw_cursor(&mut self, loc: Point<f64, Logical>) {
        let (w, h) = (self.width, self.height);
        const R: f64 = 3.0;
        let (cx, cy) = (loc.x, loc.y);
        // Clamp the disc center to the visible area (minus margin so the outline
        // can't wrap).
        let (cx, cy) = (cx.max(0.0).min(w.max(1) as f64 - 1.0), cy.max(0.0).min(h.max(1) as f64 - 1.0));
        // Bounding box of the disc as integer rows/cols.
        let x0 = (cx - R).floor().max(0.0) as u32;
        let x1 = (cx + R).ceil().min(w as f64) as u32;
        let y0 = (cy - R).floor().max(0.0) as u32;
        let y1 = (cy + R).ceil().min(h as f64) as u32;
        let r2 = R * R;
        for y in y0..y1 {
            let row = (y as usize) * (w as usize) * 4;
            for x in x0..x1 {
                let dx = x as f64 - cx;
                let dy = y as f64 - cy;
                if dx * dx + dy * dy <= r2 {
                    let o = row + (x as usize) * 4;
                    // Argb8888 little-endian, so the bytes are B, G, R, A. Pure red.
                    self.rgba[o] = 0;
                    self.rgba[o + 1] = 0;
                    self.rgba[o + 2] = 255;
                    self.rgba[o + 3] = 255;
                }
            }
        }
    }
}

/// Forward a `GlobalDispatch` from our state to the helper that owns the
/// protocol's bookkeeping.
///
/// The names in the body resolve at the invocation site, so the caller must
/// have the smithay types in scope.
macro_rules! forward_global {
    ($interface:ty: $udata:ty => $helper:ty) => {
        impl<C: Send + Sync + 'static + Default, W: Send + Sync + 'static + Default>
            GlobalDispatch<$interface, $udata> for State<C, W>
        {
            fn bind(
                state: &mut Self,
                dh: &DisplayHandle,
                client: &Client,
                resource: New<$interface>,
                data: &$udata,
                data_init: &mut DataInit<'_, Self>,
            ) {
                <$helper as GlobalDispatch<$interface, $udata, Self>>::bind(
                    state, dh, client, resource, data, data_init,
                )
            }
        }
    };
}

/// Forward a `Dispatch` from our state to the helper that owns the protocol's
/// bookkeeping. Same rule: names resolve at the invocation site.
macro_rules! forward {
    ($interface:ty: $udata:ty => $helper:ty) => {
        impl<C: Send + Sync + 'static + Default, W: Send + Sync + 'static + Default>
            Dispatch<$interface, $udata> for State<C, W>
        {
            fn request(
                state: &mut Self,
                client: &Client,
                resource: &$interface,
                request: <$interface as Resource>::Request,
                data: &$udata,
                dhandle: &DisplayHandle,
                data_init: &mut DataInit<'_, Self>,
            ) {
                <$helper as Dispatch<$interface, $udata, Self>>::request(
                    state, client, resource, request, data, dhandle, data_init,
                )
            }
            fn destroyed(
                state: &mut Self,
                client: BackendClientId,
                resource: &$interface,
                data: &$udata,
            ) {
                <$helper as Dispatch<$interface, $udata, Self>>::destroyed(
                    state, client, resource, data,
                )
            }
        }
    };
}

pub(crate) use forward;
pub(crate) use forward_global;
