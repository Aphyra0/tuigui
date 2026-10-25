//! The server state: the session's fields, connection admission, XWayland
//! startup, and the frame-callback replay.

use std::process::Child;
use std::sync::{Arc, Weak};

use smithay::input::keyboard::XkbConfig;
use smithay::input::{Seat, SeatState};
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::Serial;
use smithay::wayland::compositor::CompositorState;
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::selection::data_device::DataDeviceState;
use smithay::wayland::shm::ShmState;
use smithay::wayland::xwayland_shell::XWaylandShellState;
use smithay::xwayland::xwm::X11Wm;

use crate::client::{RootClient, StderrTail};
use crate::CompError;

/// The Smithay-registered protocol state: what every handler impl returns.
/// Registration, not policy.
pub struct Globals<C, W> {
    pub compositor_state: CompositorState,
    pub xdg_shell_state: smithay::wayland::shell::xdg::XdgShellState,
    pub shm_state: ShmState,
    /// Clipboard / drag-and-drop. Required by GTK: it refuses to open a
    /// Wayland display entirely if `wl_data_device_manager` is absent.
    pub data_device_state: DataDeviceState,
    /// X11 surface association (the `xwayland_shell` global).
    pub xwayland_shell_state: XWaylandShellState,
    pub seat_state: SeatState<crate::State<C, W>>,
    pub output_state: OutputManagerState,
    /// The running X11 window manager, once XWayland is ready.
    pub x11_wm: Option<X11Wm>,
}

/// The session: the window trees of the root client and its helpers, the
/// scales, the X11 side.
#[derive(derive_getters::Getters)]
pub struct State<C = (), W = ()> {
    /// The Smithay-registered protocol objects (see [`Globals`]).
    pub globals: Globals<C, W>,
    /// The single present output. Used to send `wl_surface.enter` so committed
    /// surfaces know an output is attached (Firefox waits for this before the
    /// WebRender presents content).
    pub output: smithay::output::Output,
    pub seat: Seat<Self>,
    pub display_handle: DisplayHandle,
    pub server_size: (u32, u32),
    /// The root client, once the caller spawned it: its process, its root
    /// window tree, and the helpers it started. Weak because Smithay's
    /// connection slot holds the owning `Arc`.
    pub root_client: Option<Weak<RootClient<C, W>>>,
    /// The scale the output advertises (the session's `dpi`): what a client
    /// multiplies the size it is configured with by to get its buffer.
    pub output_scale: f64,
    /// Whether capture draws the software cursor overlay (from `--no-cursor`).
    pub show_cursor: bool,
    /// X11 display number for the XWayland server, once its WM is up —
    /// `Some(Ok(":n"))` when advertised, `Some(Err(why))` when XWayland is
    /// known to be unavailable. Read at `spawn` time to set `DISPLAY` for
    /// X11-routed clients.
    pub x11_display: Option<std::result::Result<String, String>>,
}

impl<C: Default, W> State<C, W> {
    /// Build the session state: the globals, the seat with its keyboard and
    /// pointer, and the single output.
    pub(crate) fn new(
        dh: &DisplayHandle,
        output: smithay::output::Output,
        server_size: (u32, u32),
        output_scale: f64,
        show_cursor: bool,
    ) -> Result<Self, CompError> {
        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(dh, "compositor");
        let state = State {
            globals: Globals {
                compositor_state: CompositorState::new::<Self>(dh),
                xdg_shell_state: smithay::wayland::shell::xdg::XdgShellState::new::<Self>(dh),
                shm_state: ShmState::new::<Self>(
                    dh,
                    vec![
                        smithay::reexports::wayland_server::protocol::wl_shm::Format::Argb8888,
                        smithay::reexports::wayland_server::protocol::wl_shm::Format::Xrgb8888,
                    ],
                ),
                data_device_state: DataDeviceState::new::<Self>(dh),
                // The xwayland_shell global is what lets X11 apps (routed
                // through XWayland) hand us their surfaces. Its global is
                // created here; the XWayland server and the X11 WM are spawned
                // by the caller.
                xwayland_shell_state: XWaylandShellState::new::<Self>(dh),
                seat_state,
                output_state: OutputManagerState::new_with_xdg_output::<Self>(dh),
                x11_wm: None,
            },
            output,
            seat: seat.clone(),
            display_handle: dh.clone(),
            server_size,
            root_client: None,
            output_scale,
            show_cursor,
            x11_display: None,
        };
        seat.add_keyboard(XkbConfig::default(), 200, 25)
            .map_err(|e| CompError::Server(format!("keyboard init: {e}")))?;
        seat.add_pointer();
        Ok(state)
    }

    /// Record the spawned child: this becomes the session's root client.
    pub(crate) fn attach_root_client(
        &mut self,
        child: Child,
        stderr: StderrTail,
    ) -> Result<(), CompError> {
        if self.root_client.is_some() {
            return Err(CompError::Spawn("the session already has a root client".into()));
        }
        let root = Arc::new(RootClient::new(child, stderr));
        self.root_client = Some(Arc::downgrade(&root));
        Ok(())
    }

    pub(crate) fn next_serial(&mut self) -> Serial {
        self.serial = self.serial.wrapping_add(1);
        Serial::from(self.serial as u32)
    }

    /// Take the root client out of the session, if it is still there.
    pub(crate) fn take_root_client(&mut self) -> Option<Arc<RootClient<C, W>>> {
        self.root_client.take().and_then(Weak::upgrade)
    }

    /// The surface of every admitted window: each client's root window and
    /// its children, the root client's first, then its helpers'.
    pub(crate) fn window_surfaces(&self) -> Vec<WlSurface> {
        let mut out = Vec::new();
        let mut push_tree = |lock: &std::sync::RwLock<Option<Window<W>>>| {
            let tree = lock.read().unwrap();
            if let Some(t) = tree.as_ref() {
                out.push(t.surface.clone());
                for child in &t.children {
                    out.push(child.surface.clone());
                }
            }
        };
        if let Some(root) = self.root_client.as_ref().and_then(Weak::upgrade) {
            push_tree(&root.root_window);
            for sub in root.sub_clients().iter().filter_map(Weak::upgrade) {
                push_tree(&sub.root_window);
            }
        }
        out
    }
}

/// Fire all pending frame callbacks on a window's surface tree.
pub(crate) fn fire_frame_callbacks(surface: &WlSurface) {
    smithay::wayland::compositor::with_surface_tree_downward(
        surface,
        (),
        |_, _, _| smithay::wayland::compositor::TraversalAction::DoChildren(()),
        |_, states, _| {
            let mut attrs = states
                .cached_state
                .get::<smithay::wayland::compositor::SurfaceAttributes>();
            let now = crate::server_time();
            for cb in attrs.current().frame_callbacks.drain(..) {
                cb.done(now);
            }
        },
        |_, _, _| true,
    );
}
