//! XWayland glue: the X11 window manager handler and X11 window admission.
//!
//! XWayland lets X11-only apps (glxgears, old GTK/Qt, xterm, ...) render
//! through our headless session. It exposes, to those clients, an X11 server
//! whose drawing is redirected onto `wl_surface`s that reach us like any
//! other Wayland surface. Smithay turns almost the entire classic reparenting
//! XWM job over to us: we must grant window mapping, keep the window
//! configured to fill our single output, and admit a mapped window into the
//! root client's window tree like an xdg toplevel would be.

use std::os::unix::io::OwnedFd;

use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Rectangle};
use smithay::wayland::selection::SelectionTarget;
use smithay::wayland::xwayland_shell::{XWaylandShellHandler, XWaylandShellState};
use smithay::xwayland::xwm::{ResizeEdge, Reorder, X11Surface, X11Wm, XwmHandler, XwmId};

use crate::state::State;
use crate::window::Window;

impl<C, W> XwmHandler for State<C, W>
where
    C: proto::Shared + Default,
    W: proto::Shared + Default,
{
    fn xwm_state(&mut self, _xwm: XwmId) -> &mut X11Wm {
        self.globals
            .x11_wm
            .as_mut()
            .expect("XWM handler invoked before the X11 WM was started")
    }

    fn new_window(&mut self, _xwm: XwmId, window: X11Surface) {
        tracing::debug!(window = window.window_id(), "x11: new window");
        if window.is_override_redirect() {
            return;
        }
    }

    fn new_override_redirect_window(&mut self, _xwm: XwmId, _window: X11Surface) {
        tracing::debug!("x11: new override-redirect window (ignored)");
    }

    fn map_window_request(&mut self, _xwm: XwmId, window: X11Surface) {
        tracing::debug!(window = window.window_id(), "x11: map request");
        // Grant the mapping first (the client draws at the size it requested),
        // then admit. We do NOT force a full-screen configure at map time:
        // X11 clients like glxgears draw at the size they asked for and can
        // withdraw+recreate the window if a WM slams a different size on them
        // before they've presented. The output-size override happens only on
        // an explicit `configure_request`.
        let _ = window.set_mapped(true);
        self.admit_x11(&window);
    }

    fn mapped_override_redirect_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn unmapped_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn destroyed_window(&mut self, _xwm: XwmId, window: X11Surface) {
        // Only drop the window we admitted. XWayland regularly creates and
        // destroys helper windows (tooltips, override-redirect, transient
        // bookkeeping) that share the same client; clearing the tree for those
        // would wipe the live capture surface mid-session.
        let Some(surface) = window.wl_surface() else {
            return;
        };
        let Some(root) = self.root_client.as_ref().and_then(std::sync::Weak::upgrade) else {
            return;
        };
        let was_root = {
            let mut tree = root.root_window.write().unwrap();
            let was_root = tree
                .as_ref()
                .is_some_and(|t| t.surface.wl_surface == surface);
            if was_root {
                *tree = None;
            }
            was_root
        };
        if was_root {
            self.root_client = None;
            tracing::debug!("x11: admitted window destroyed; dropping the root slot");
        } else if let Some(tree) = root.root_window.write().unwrap().as_mut() {
            tree.remove_surface(&surface);
        }
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

    fn resize_request(
        &mut self,
        _xwm: XwmId,
        _window: X11Surface,
        _button: u32,
        _resize_edge: ResizeEdge,
    ) {
    }

    fn move_request(&mut self, _xwm: XwmId, _window: X11Surface, _button: u32) {}

    fn send_selection(
        &mut self,
        _xwm: XwmId,
        _selection: SelectionTarget,
        _mime_type: String,
        _fd: OwnedFd,
    ) {
        // We never allow_selection_access, so this must not be called.
    }

    fn allow_selection_access(&mut self, _xwm: XwmId, _selection: SelectionTarget) -> bool {
        false
    }
}

impl<C, W> XWaylandShellHandler for State<C, W>
where
    C: proto::Shared + Default,
    W: proto::Shared + Default,
{
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.globals.xwayland_shell_state
    }

    fn surface_associated(&mut self, _xwm: XwmId, _wl_surface: WlSurface, window: X11Surface) {
        // Association gives us the wl_surface for an X11 window. If we haven't
        // admitted it at map time (ordering can vary), do it now.
        self.admit_x11(&window);
    }
}

impl<C, W> State<C, W>
where
    C: proto::Shared + Default,
    W: proto::Shared + Default,
{
    /// Admit an X11 client's surface into the root client's window tree, as a
    /// child when the tree already has a root window.
    pub(crate) fn admit_x11(&mut self, window: &X11Surface) {
        let Some(surface) = window.wl_surface() else {
            // Not yet associated; `surface_associated` will call back.
            return;
        };
        if self
            .root_client
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
            .and_then(|root| {
                root.root_window
                    .read()
                    .unwrap()
                    .as_ref()
                    .map(|t| t.surface.wl_surface == surface)
            })
            .unwrap_or(false)
        {
            tracing::debug!("x11: window already admitted; ignoring extra");
            return;
        }
        let win: Window<W> = Window::new(surface.clone(), Some(self.server_size), W::default());
        let Some(root) = self.root_client.as_ref().and_then(std::sync::Weak::upgrade) else {
            tracing::debug!("x11: no root client to admit into");
            return;
        };
        let mut tree = root.root_window.write().unwrap();
        match tree.as_mut() {
            Some(tree) => tree.children.push(win),
            None => *tree = Some(win),
        }
        drop(tree);
        self.output.enter(&surface);
        tracing::info!("x11: admitted window into the root client's tree");
    }
}
