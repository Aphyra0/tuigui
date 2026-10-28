//! `xdg_shell` handler: the window-tree admission and its configure.

use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::protocol::wl_seat::WlSeat;
use smithay::utils::Serial;
use smithay::wayland::shell::xdg::{PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler};
use wayland_protocols::xdg::shell::server::xdg_toplevel;

use crate::client::{RootClient, SubClient};
use crate::state::State;
use crate::window::Window;

impl<C, W> XdgShellHandler for State<C, W>
where
    C: proto::Shared + Default,
    W: proto::Shared + Default,
{
    fn xdg_shell_state(&mut self) -> &mut smithay::wayland::shell::xdg::XdgShellState {
        &mut self.globals.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        tracing::debug!("compositor: new toplevel requested");
        let client = surface
            .wl_surface()
            .client()
            .expect("toplevel always has a client");

        // Whose connection produced this toplevel: the root's or a sub's.
        // The first toplevel of a client is its root window; the toplevels
        // its program spawns afterwards are that window's children.
        if client.get_data::<RootClient<C, W>>().is_some() {
            admit_into_root_client(self, surface);
            return;
        }
        if client.get_data::<SubClient<C, W>>().is_some() {
            admit_into_sub(self, surface);
            return;
        }

        // XWayland's connection: its admitted surface is the root client's
        // window; an unadmitted X11 toplevel does not reach here (it comes
        // through the XWM).
        tracing::warn!("compositor: rejecting a toplevel of an unknown connection");
        surface.send_close();
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        let Some(client) = surface.wl_surface().client() else { return };
        if let Some(root) = client.get_data::<RootClient<C, W>>() {
            let was_root = {
                let mut tree = root.root_window.write().unwrap();
                let was_root = tree.as_ref().is_some_and(|t| t.surface.wl_surface == *surface.wl_surface());
                if was_root {
                    *tree = None;
                }
                was_root
            };
            if was_root {
                // The root window is gone: the program is over. The Weak
                // dies with the slot; the record drops (and kills the
                // child) when the connection does.
                self.root_client = None;
                tracing::info!("compositor: the root window is gone; the root client is out");
            } else if let Some(tree) = root.root_window.write().unwrap().as_mut() {
                tree.remove_surface(surface.wl_surface());
            }
        } else if let Some(sub) = client.get_data::<SubClient<C, W>>() {
            let was_root = {
                let mut tree = sub.root_window.write().unwrap();
                let was_root = tree.as_ref().is_some_and(|t| t.surface.wl_surface == *surface.wl_surface());
                if was_root {
                    *tree = None;
                }
                was_root
            };
            if !was_root {
                if let Some(tree) = sub.root_window.write().unwrap().as_mut() {
                    tree.remove_surface(surface.wl_surface());
                }
            }
        }
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        tracing::warn!("compositor: rejecting popup (no popups in the MVP)");
        // Send popup_done (the "dismiss" event); the client tears the popup
        // down itself. Destroying the resource server-side would be a
        // protocol error for a live popup, so we don't.
        surface.send_popup_done();
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: WlSeat, _serial: Serial) {}

    fn reposition_request(
        &mut self,
        _surface: PopupSurface,
        _positioner: PositionerState,
        _token: u32,
    ) {
    }
}

/// The admission common to both client kinds: configure, activate, and build
/// the window that will hold the surface.
///
/// `size` is the size to configure with — the root window's, the server's own
/// resolution. `None` proposes nothing: the client keeps whatever size it
/// asked for, clamped to `server_size`, which is the most this session can
/// ever show. Either way the window's recorded size is the size it was
/// configured at.
fn admit<W: Default>(
    surface: &ToplevelSurface,
    size: Option<(u32, u32)>,
    server_size: (u32, u32),
    serial: Serial,
) -> Window<W> {
    let wanted = size.unwrap_or_else(|| {
        // The client's own request, bounded by what we can display.
        let req = surface.current_state().size;
        match req {
            Some(s) => (
                (s.w.max(1) as u32).min(server_size.0),
                (s.h.max(1) as u32).min(server_size.1),
            ),
            None => server_size,
        }
    });
    let win = Window::new(
        surface.wl_surface().clone(),
        surface.clone(),
        wanted,
        W::default(),
    );
    surface.with_pending_state(|pending| {
        if size.is_some() {
            pending.size = Some((wanted.0 as i32, wanted.1 as i32).into());
        }
        pending.states.set(xdg_toplevel::State::Activated);
    });
    surface.send_configure();
    *win.last_configure_serial.lock().unwrap() = Some(u32::from(serial) as u64);
    win
}

/// Admit a toplevel into the root client's window tree.
///
/// The first toplevel is the client's root window and fills the output; later
/// ones are children and keep their own size.
fn admit_into_root_client<C, W>(state: &mut State<C, W>, surface: ToplevelSurface)
where
    C: proto::Shared + Default,
    W: proto::Shared + Default,
{
    let server_size = state.server_size;
    let serial = state.next_serial();
    let root = state.root_client.as_ref().expect("root data seen");

    // Whether the root slot is occupied decides the configured size, so the
    // tree is consulted before the window is built.
    let has_root = root.root_window.read().unwrap().is_some();
    let size = if has_root { None } else { Some(server_size) };
    let win = admit(&surface, size, server_size, serial);

    let mut tree = root.root_window.write().unwrap();
    match tree.as_mut() {
        Some(tree) => tree.children.push(win),
        None => *tree = Some(win),
    }
    // A commit that arrived before this window existed was held for it: that picture is
    // the only one the client will draw until something asks it again, so it goes in
    // now. See `hold` in `handlers/compositor.rs`.
    let held = state.pending_commits.remove(&surface.wl_surface().clone());
    if let Some(tree) = tree.as_mut() {
        apply_pending(held, tree);
    }
    drop(tree);

    // The window "fills" the output. Send `wl_surface.enter` so the client
    // knows an output is attached and can start presenting. Clients like
    // Firefox's WebRender withhold content until they see this.
    state.output.enter(surface.wl_surface());
    log::info!("admitted a window into the root client's tree");
}

/// Apply a commit held for a window that has just been admitted.
fn apply_pending<W>(
    held: Option<(Vec<u8>, (u32, u32))>,
    tree: &mut crate::window::Window<W>,
) {
    let Some((pixels, _size)) = held else {
        return;
    };
    *tree.surface.pixels.lock().unwrap() = pixels;
    tree.surface.committed = true;
    log::info!("applied a held commit to the window it belongs to");
}

/// Admit a toplevel into a sub-client's window tree.
fn admit_into_sub<C, W>(state: &mut State<C, W>, surface: ToplevelSurface)
where
    C: proto::Shared + Default,
    W: proto::Shared + Default,
{
    // A sub-client's windows never fill the output: one window does, and it
    // is the root client's. The configure proposes nothing, so the window
    // keeps the size its own program asked for.
    let serial = state.next_serial();
    let client = surface.wl_surface().client().expect("seen above");
    let sub = client.get_data::<SubClient<C, W>>().expect("seen above");
    let win = admit(&surface, None, state.server_size, serial);

    let mut tree = sub.root_window.write().unwrap();
    match tree.as_mut() {
        Some(tree) => tree.children.push(win),
        None => *tree = Some(win),
    }
    // The commit that raced this admission goes in now: it is the picture the client
    // drew when it was configured, and nothing else will ask it to draw.
    let held = state.pending_commits.remove(&surface.wl_surface().clone());
    if let Some(tree) = tree.as_mut() {
        apply_pending(held, tree);
    }
    drop(tree);

    state.output.enter(surface.wl_surface());
    log::info!("admitted a window into a sub-client's tree");
}
