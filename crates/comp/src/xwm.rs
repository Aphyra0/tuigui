//! X11 window manager (`Xwayland`) glue.
//!
//! XWayland lets X11-only apps (glxgears, old GTK/Qt, xterm, ...) render
//! through our headless compositor. It does so by exposing, to those clients,
//! an X11 server whose drawing is redirected onto `wl_surface`s that reach us
//! like any other Wayland surface. Smithay turns almost the entire classic
//! reparenting XWM job over to us: we must grant window mapping, keep the
//! window configured to fill our single output, and — because this crate has a
//! hard one-window policy — admit the first mapped window into the same slot
//! that an xdg toplevel would otherwise take.

use std::os::unix::io::OwnedFd;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Rectangle};
use smithay::wayland::selection::SelectionTarget;
use smithay::wayland::xwayland_shell::{XWaylandShellHandler, XWaylandShellState};
use smithay::xwayland::xwm::{ResizeEdge, Reorder, X11Surface, X11Wm, XwmHandler, XwmId};

use crate::server::WindowShared;
use crate::surface::Window;

impl XwmHandler for crate::state::CompState {
    fn xwm_state(&mut self, _xwm: XwmId) -> &mut X11Wm {
        self.x11_wm
            .as_mut()
            .expect("XWM handler invoked before the X11 WM was started")
    }

    fn new_window(&mut self, _xwm: XwmId, window: X11Surface) {
        tracing::debug!(window = window.window_id(), "x11: new window");
        if window.is_override_redirect() {
            return;
        }
        // Remember as the candidate for the one-window slot. The slot is only
        // filled when the window maps and its wl_surface is known.
        self.pending_x11 = Some(window);
    }

    fn new_override_redirect_window(&mut self, _xwm: XwmId, window: X11Surface) {
        tracing::debug!(window = window.window_id(), "x11: new override-redirect window (ignored)");
    }

    fn map_window_request(&mut self, _xwm: XwmId, window: X11Surface) {
        tracing::debug!(window = window.window_id(), "x11: map request");
        // Keep the window filling the full output and grant the mapping. The
        // window's own requested geometry may be smaller than our output; the
        // X11 client renders at whatever size we configure, so pin it to the
        // output so capture sees the whole thing.
        let (w, h) = self.server_size;
        let full = Rectangle::<i32, Logical>::from_size((w as i32, h as i32).into());
        let _ = window.configure(full);
        let _ = window.set_mapped(true);
        self.admit_x11(&window);
    }

    fn mapped_override_redirect_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn unmapped_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn destroyed_window(&mut self, _xwm: XwmId, _window: X11Surface) {
        // The window is gone; drop the slot so a later app could be admitted.
        self.window = None;
        self.pending_x11 = None;
    }

    fn configure_request(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        _x: Option<i32>,
        _y: Option<i32>,
        _w: Option<u32>,
        _h: Option<u32>,
        _reorder: Option<Reorder>,
    ) {
        // The client asks to resize/move itself; we keep it full-output.
        let (w, h) = self.server_size;
        let full = Rectangle::<i32, Logical>::from_size((w as i32, h as i32).into());
        let _ = window.configure(full);
    }

    fn configure_notify(
        &mut self,
        _xwm: XwmId,
        _window: X11Surface,
        _geometry: Rectangle<i32, Logical>,
        _above: Option<u32>,
    ) {
    }

    fn resize_request(&mut self, _xwm: XwmId, _window: X11Surface, _button: u32, _resize_edge: ResizeEdge) {}

    fn move_request(&mut self, _xwm: XwmId, _window: X11Surface, _button: u32) {}

    fn send_selection(&mut self, _xwm: XwmId, _selection: SelectionTarget, _mime_type: String, _fd: OwnedFd) {
        // We never allow_selection_access, so this must not be called.
    }

    fn allow_selection_access(&mut self, _xwm: XwmId, _selection: SelectionTarget) -> bool {
        false
    }
}

impl XWaylandShellHandler for crate::state::CompState {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland_shell_state
    }

    fn surface_associated(&mut self, _xwm: XwmId, _wl_surface: WlSurface, window: X11Surface) {
        // Association gives us the wl_surface for an X11 window. If we haven't
        // admitted it at map time (ordering can vary), do it now.
        self.admit_x11(&window);
    }
}

// Tighter access to the window slot, shared with xdg-toplevel admission.
impl crate::state::CompState {
    /// Admit an X11 client's surface into the one-window slot, if the slot is
    /// free and the surface has an associated wl_surface to capture.
    pub fn admit_x11(&mut self, window: &X11Surface) {
        if self.window.is_some() {
            tracing::debug!("x11: window already admitted; ignoring extra");
            return;
        }
        let Some(surface) = window.wl_surface() else {
            // Not yet associated; `surface_associated` will call back.
            return;
        };
        let size = self.server_size;
        let shared = Arc::new(WindowShared {
            surface: surface.clone(),
            size,
            dead: AtomicBool::new(false),
            committed: AtomicBool::new(false),
        });
        let win = Arc::new(Window::new(shared));
        self.window = Some(win);
        self.output.enter(&surface);
        tracing::info!("x11: admitted window into the one-window slot");
    }
}