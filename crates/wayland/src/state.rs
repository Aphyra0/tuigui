//! The server state: the session's fields, connection admission, XWayland
//! startup, and the frame-callback replay.

use std::process::Child;
use std::sync::Arc;

use smithay::input::keyboard::XkbConfig;
use smithay::input::{Seat, SeatState};
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::utils::Serial;
use smithay::wayland::compositor::CompositorState;
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::selection::data_device::DataDeviceState;
use smithay::wayland::shm::ShmState;
use smithay::wayland::xwayland_shell::XWaylandShellState;
use smithay::xwayland::xwm::X11Wm;

use crate::client::RootClient;
use crate::CompError;

/// The Smithay-registered protocol state: what every handler impl returns.
/// Registration, not policy.
pub struct Globals {
    pub compositor_state: CompositorState,
    pub xdg_shell_state: smithay::wayland::shell::xdg::XdgShellState,
    pub shm_state: ShmState,
    /// Clipboard / drag-and-drop. Required by GTK: it refuses to open a
    /// Wayland display entirely if `wl_data_device_manager` is absent.
    pub data_device_state: DataDeviceState,
    /// X11 surface association (the `xwayland_shell` global).
    pub xwayland_shell_state: XWaylandShellState,
    pub seat_state: SeatState<crate::State>,
    pub output_state: OutputManagerState,
    /// The running X11 window manager, once XWayland is ready.
    pub x11_wm: Option<X11Wm>,
}

/// The session: the root client's window tree, the helpers, the scales, the
/// X11 side.
#[derive(derive_getters::Getters)]
pub struct State {
    /// The Smithay-registered protocol objects (see [`Globals`]).
    pub globals: Globals,
    /// The single present output. Used to send `wl_surface.enter` so committed
    /// surfaces know an output is attached (Firefox waits for this before the
    /// WebRender presents content).
    pub output: smithay::output::Output,
    pub seat: Seat<Self>,
    pub display_handle: DisplayHandle,
    pub server_size: (u32, u32),
    /// The last serial this session handed out; bumped by [`State::next_serial`].
    serial: u64,
    /// The root client, once the caller spawned it: its process, its root
    /// window tree, and the helpers it started.
    ///
    /// A strong `Arc`, not a `Weak`, and that is not an oversight. Smithay's connection
    /// slot does own the record — but only from the moment `insert_client` runs, and the
    /// record is made before the program has even connected. A `Weak` here would be dead
    /// in the gap between the spawn and the connection, so the session would lose its
    /// root client before the program that owns it had a chance to say anything.
    ///
    /// Holding it strongly costs nothing: the slot and this field share one count, and
    /// when the connection ends the slot drops its share, leaving this one.
    pub root_client: Option<Arc<RootClient>>,
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
    /// Commits that arrived before the surface had a window to draw into.
    ///
    /// A client may commit between being configured and being admitted: `xdg-shell`
    /// sends the configure while the window is still being built, and Wayland delivers
    /// that first commit before the window exists. The commit used to be dropped, and
    /// the window it belonged to then never had pixels — nothing else asks it to draw,
    /// because frame callbacks are fired by `capture`, which only runs once a window
    /// exists.
    ///
    /// Held by surface, drained by the admission that creates the window. Bounded by the
    /// number of surfaces mid-admission, which is one per client at a time.
    pub pending_commits: std::collections::HashMap<smithay::reexports::wayland_server::protocol::wl_surface::WlSurface, (Vec<u8>, (u32, u32))>,
}

impl State {
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
            serial: 0,
            root_client: None,
            output_scale,
            show_cursor,
            x11_display: None,
            pending_commits: std::collections::HashMap::new(),
        };
        seat.add_keyboard(XkbConfig::default(), 200, 25)
            .map_err(|e| CompError::Server(format!("keyboard init: {e}")))?;
        seat.add_pointer();
        Ok(state)
    }

    /// Record the spawned child: this becomes the session's root client.
    pub(crate) fn attach_root_client(&mut self, child: Child) -> Result<(), CompError> {
        if self.root_client.is_some() {
            return Err(CompError::Spawn("the session already has a root client".into()));
        }
        self.root_client = Some(Arc::new(RootClient::new(child)));
        Ok(())
    }

    pub(crate) fn next_serial(&mut self) -> Serial {
        self.serial = self.serial.wrapping_add(1);
        Serial::from(self.serial as u32)
    }
}
