//! wayland: the headless Wayland session an app runs in.
//!
//! The session is a Smithay server this process owns and drives inline: the
//! caller advances it with [`WaylandServer::step`], reads its windows with
//! [`Window::capture`], and pushes inputs through the windows' mailboxes.
//! There is no session thread and no control channel — the server only moves
//! forward when stepped, so whoever owns it owns the loop.
//!
//! The output's resolution is the terminal's resolution, always; the session
//! is never resized on the Wayland level. `Options::dpi` is the one scale
//! knob: it is the scale the output advertises, and it decides how large a
//! client's elements look in the pane. Saving bandwidth means downscaling the
//! captured frame somewhere else, not shrinking the session.
//!
//! Linux-only: smithay is this crate's backend, and smithay does not run on
//! macOS. TODO: implement the macOS windowing backend (Quartz/WindowServer;
//! plan in spec/PLATFORMS.md) as a sibling of this crate.
#![cfg(target_os = "linux")]

mod client;
mod error;
mod handlers;
mod inputs;
mod state;
mod utils;
mod window;

pub use client::{RootClient, SubClient};
pub use error::{CompError, Result};
pub use inputs::{Button, Input, Key, KeyState, ScrollDelta};
pub use window::Window;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use smithay::reexports::wayland_server::backend::GlobalId;
use smithay::reexports::calloop::EventLoop;
use smithay::reexports::wayland_server::{Display, ListeningSocket};

use crate::state::State;

/// Everything the session needs, passed once, at `WaylandServer::new`.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// The output's resolution in pixels. The terminal's, always.
    pub resolution: (u32, u32),
    /// The scale the output advertises to its clients: how large a client's
    /// elements look in the pane. A client that honours this draws its content
    /// at this many times the logical size, so the same interface is legible
    /// on a high-density panel rather than half-sized. The resolution does not
    /// change: the output's mode stays what it was and the window still fills
    /// it. What does change is the buffer a client commits, which is its
    /// logical size times this, so the capture scales that back to the output
    /// instead of assuming the two are the same.
    pub dpi: f64,
    /// Draw a software cursor overlay on captured frames.
    pub show_cursor: bool,
    /// Spawn an XWayland server so X11-only clients can render through the
    /// session. When false, only native Wayland clients are supported
    /// (`DISPLAY` is left unset).
    pub use_x11: bool,
}

/// How long one [`WaylandServer::step`] turn dispatches for at most.
pub const STEP_TIMEOUT: Duration = Duration::from_millis(4);

/// A monotonic timestamp for event delivery, since process start.
pub(crate) fn server_time() -> u32 {
    static BOOT: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    BOOT.get_or_init(Instant::now).elapsed().as_millis() as u32
}

/// The headless Wayland session, owned by the caller and driven inline.
pub struct WaylandServer {
    /// The session state: the root client, its windows, the seat, the X11 side.
    state: State,
    event_loop: EventLoop<'static, State>,
    display: Display<State>,
    socket: ListeningSocket,
    /// Absolute path of the listening socket, for `WAYLAND_DISPLAY`.
    socket_name: PathBuf,
    /// Private per-session runtime dir holding our socket. The child's
    /// `XDG_RUNTIME_DIR` points here so it can only discover our socket, never
    /// the host compositor's. Removed on drop.
    runtime_dir: PathBuf,
    /// The wl_output global, kept alive for the session's lifetime.
    _output_global: GlobalId,
    /// Whether to spawn XWayland and route X11-only clients to it (`DISPLAY`).
    use_x11: bool,
}

impl WaylandServer {
    /// Start the headless session: display, private socket, the single output,
    /// the seat, and (when asked) the XWayland server.
    #[tracing::instrument(name = "server.new", skip_all)]
    pub fn new(options: Options) -> Result<Self, CompError> {
        // A private runtime dir, isolated from the caller's (Hyprland's): the
        // child's `XDG_RUNTIME_DIR` points here, so the only Wayland socket it
        // can auto-discover is ours.
        let runtime_dir = Self::new_runtime_dir()?;

        let display = Display::new().map_err(|e| CompError::Server(e.to_string()))?;
        let dh = display.handle();

        // The socket is bound inside the runtime dir, at a free name.
        let (socket, socket_name) = Self::new_socket(&runtime_dir)?;

        // The one output, at the resolution the session is asked for; the
        // window fills it.
        let output = Self::new_output(options.resolution, options.dpi)?;
        let output_global = output.create_global::<State>(&dh);

        // The globals, the seat with its keyboard and pointer, and the output.
        let state = State::new(&dh, output, options.resolution, options.dpi, options.show_cursor)?;

        // The loop that sources ride on; XWayland's source joins when asked.
        let event_loop =
            EventLoop::try_new().map_err(|e| CompError::Server(e.to_string()))?;
        if options.use_x11 {
            tracing::warn!("x11: XWayland is not wired up in this build; ignoring --x11");
        }

        Ok(WaylandServer {
            state,
            event_loop,
            display,
            socket,
            socket_name,
            runtime_dir,
            _output_global: output_global,
            use_x11: options.use_x11,
        })
    }

    /// A private runtime dir, isolated from the caller's (Hyprland's). The
    /// child gets this as `XDG_RUNTIME_DIR` so the only Wayland socket it
    /// can auto-discover is ours; the host compositor simply isn't there.
    fn new_runtime_dir() -> Result<PathBuf, CompError> {
        // `/tmp` and not `temp_dir()`: `TMPDIR` is whatever launched us, and under a
        // shell that is a per-shell directory that a grandchild does not inherit. The
        // socket's path travels to the child as an absolute path, so it has to be a
        // path every process can reach — and `/tmp` is the one that is.
        let runtime_dir = Path::new("/tmp").join(format!(
            "wayland-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&runtime_dir)
            .map_err(|e| CompError::Server(format!("create runtime dir: {e}")))?;
        Ok(runtime_dir)
    }

    /// Bind the session's private socket, picking a free name under the
    /// runtime dir. Returns the socket and the path it was bound at.
    fn new_socket(
        runtime_dir: &Path,
    ) -> Result<(ListeningSocket, PathBuf), CompError> {
        let mut name = format!("wayland-{}", std::process::id());
        let mut socket_path = runtime_dir.join(&name);
        let mut attempts = 0u32;
        let socket = loop {
            match ListeningSocket::bind_absolute(socket_path.clone()) {
                Ok(s) => break s,
                Err(smithay::reexports::wayland_server::BindError::AlreadyInUse) => {
                    attempts += 1;
                    if attempts > 64 {
                        return Err(CompError::Server(format!(
                            "no free socket name under {}",
                            runtime_dir.display()
                        )));
                    }
                    // Some collision in our private dir; try the next suffix.
                    name = format!("wayland-{}-{attempts}", std::process::id());
                    socket_path = runtime_dir.join(&name);
                }
                Err(e) => {
                    return Err(CompError::Server(format!("bind socket: {e}")));
                }
            }
        };
        // `bind_absolute` does not populate the auto-name (that's a `bind`/
        // `bind_auto` convenience); the absolute path we bound is the socket.
        Ok((socket, socket_path))
    }

    /// The session's single output: the one window fills it.
    fn new_output(
        resolution: (u32, u32),
        dpi: f64,
    ) -> Result<smithay::output::Output, CompError> {
        use smithay::output::{Mode, Output, PhysicalProperties, Scale, Subpixel};

        let (w, h) = (resolution.0 as i32, resolution.1 as i32);
        let mode = Mode {
            size: (w, h).into(),
            refresh: 60_000,
        };
        let output = Output::new(
            "wayland-headless".into(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                model: "wayland".into(),
                make: "wayland".into(),
            },
        );
        // The third argument is the output's `Scale`, which is what a client
        // reads to decide how many buffer pixels one logical pixel is worth.
        // It is advertised here and the mode is left alone, so a client that
        // honours it draws the same interface larger rather than the window
        // changing size.
        //
        // A whole value is advertised as an integer and not a fraction: some
        // clients only understand integer scaling, and a fractional value
        // would be rounded up for them, making 2.0 arrive as 2 and 1.5 arrive
        // as 2 as well -- two different requests answered the same way.
        let advertised = if dpi.fract() == 0.0 {
            Scale::Integer(dpi as i32)
        } else {
            Scale::Fractional(dpi)
        };
        output.change_current_state(Some(mode), None, Some(advertised), Some((0, 0).into()));
        output.set_preferred(mode);
        Ok(output)
    }

    /// The output's resolution in pixels.
    pub fn size(&self) -> (u32, u32) {
        self.state.server_size
    }

    /// The root window's tree, shared by the `Arc` the tree keeps it under:
    /// the strong share from which weak handles are minted.
    pub fn root_tree(&self) -> Option<Arc<Window>> {
        self.state.root_client.as_ref()?.root_window.read().unwrap().clone()
    }

    /// Whether the session's program is still running.
    pub fn is_running(&self) -> bool {
        self.state
            .root_client
            .as_ref()
            .is_some_and(|root| root.is_running())
    }

    /// The command a client is launched with: a native Wayland client
    /// connects to us via `WAYLAND_DISPLAY`; an X11-only one falls back to
    /// our XWayland via `DISPLAY` (routed separately, in `route_display`).
    ///
    /// `WAYLAND_DISPLAY` is the socket's **absolute path**, not a name. A bare name is
    /// resolved against `XDG_RUNTIME_DIR`, and the only way to make that point at us
    /// would be to override `XDG_RUNTIME_DIR` — which is the session directory, shared
    /// with dbus, audio, and everything else under `$XDG_RUNTIME_DIR`. Overriding it
    /// breaks all of that, and it does not survive a process in between: a program that
    /// spawns its own child hands it a runtime dir and a socket name that no longer
    /// agree, and the child finds no display at all. An absolute path needs neither,
    /// and travels through any number of grandchildren unchanged.
    fn new_command(path: &Path, socket_name: &Path) -> std::process::Command {
        let mut command = std::process::Command::new(path);
        command
            .env("WAYLAND_DISPLAY", socket_name)
            .env_remove("WAYLAND_SOCKET")
            .env_remove("GDK_BACKEND")
            .env_remove("QT_QPA_PLATFORM")
            .env_remove("HYPRLAND_INSTANCE_SIGNATURE");
        command
    }

    /// Point `DISPLAY` at our XWayland, but not before its window manager
    /// holds `WM_S0`: an X11 client that connects earlier dies with "Can't
    /// open display". Stepping drives the XWayland source until the display
    /// is advertised or XWayland reports itself unavailable.
    fn route_display(&mut self, command: &mut std::process::Command) -> Result<(), CompError> {
        loop {
            match &self.state.x11_display {
                Some(Ok(d)) => {
                    command.env("DISPLAY", d.clone());
                    return Ok(());
                }
                Some(Err(_)) => {
                    command.env_remove("DISPLAY");
                    return Ok(());
                }
                None => {
                    self.step(Duration::from_millis(10));
                }
            }
        }
    }

    /// Spawn `path` into the session and return at once.
    ///
    /// Native Wayland clients reach our private socket via `WAYLAND_DISPLAY`;
    /// X11-only clients are routed to the XWayland server we own via `DISPLAY`.
    ///
    /// Nothing is waited for. The child has not connected yet, so it has no
    /// window: the caller learns about both by advancing the server and asking
    /// `root_tree` again, which is what makes the loop the caller's to own.
    #[tracing::instrument(name = "server.spawn", skip_all)]
    pub fn spawn(&mut self, path: &std::path::Path) -> std::io::Result<()> {
        tracing::info!(path = %path.display(), "spawning client");

        // The command that can reach both display stacks, scrubbed of
        // everything that could reach a host compositor.
        let mut command = Self::new_command(path, &self.socket_name);

        // X11-only clients are routed to our XWayland; step until it is up.
        if self.use_x11 {
            self.route_display(&mut command)
                .map_err(|e| std::io::Error::other(e.to_string()))?;
        } else {
            command.env_remove("DISPLAY");
        }

        // What the child is told, said out loud: a program that reports "no display"
        // cannot be asked which value it read, so the value goes in the log next to
        // whether the socket is really there. The diff between this line and what the
        // program says is the whole diagnosis.
        //
        // Through `log`, not `tracing`: the binary installs a `log` capturer, and a
        // `tracing` event only reaches it when no tracing subscriber is installed. The
        // profiler build installs one that keeps every event to itself.
        log::info!(
            "launching {} with WAYLAND_DISPLAY={} (socket exists: {})",
            path.display(),
            self.socket_name.display(),
            self.socket_name.exists()
        );

        // A program's own diagnostics go to a file, not the pane: this process owns the
        // terminal, and a toolkit's warnings written to stdout would land in the middle
        // of the screen the pane is drawing on.
        //
        // Descriptors rather than pipes: a pipe can fill, and a program blocked writing
        // to one is a program that never draws. A file never blocks.
        let chatter = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open("/tmp/tuigui-client.log")
            .map_err(|e| std::io::Error::other(format!("opening the client log: {e}")))?;
        let chatter_err = chatter
            .try_clone()
            .map_err(|e| std::io::Error::other(format!("cloning the client log: {e}")))?;
        command.stdout(std::process::Stdio::from(chatter));
        command.stderr(std::process::Stdio::from(chatter_err));
        let child = command
            .spawn()
            .map_err(|e| std::io::Error::other(format!("{}: {e}", path.display())))?;

        // The child becomes the session's root client; its connection
        // attaches at the next step.
        self.state
            .attach_root_client(child)
            .map_err(|e| std::io::Error::other(e.to_string()))
    }

    /// Advance the session: accept new clients, drain their input mailboxes,
    /// dispatch their protocol, fire pending frame callbacks, and flush
    /// everything back down the sockets.
    ///
    /// The server only moves forward when this is called, so the caller's loop
    /// is the session's heartbeat.
    #[tracing::instrument(name = "server.step", skip(self))]
    pub fn step(&mut self, timeout: Duration) {
        let WaylandServer {
            state,
            event_loop,
            display,
            socket,
            ..
        } = self;

        // New clients on our private socket. The socket is polled here rather
        // than as a loop source, so `step` owns every part of the turn.
        while let Ok(Some(stream)) = socket.accept() {
            if let Err(e) = state.accept_client(stream) {
                log::warn!("a connection was not admitted: {e}");
            }
        }

        // Queued inputs dispatch here, against the window each queue came
        // from, so they land at a well-defined point in the protocol stream.
        state.dispatch_inputs();

        if let Err(e) = event_loop.dispatch(Some(timeout), state) {
            log::warn!("the event loop did not dispatch: {e}");
        }
        if let Err(e) = display.dispatch_clients(state) {
            log::warn!("the display did not dispatch: {e}");
        }
        if let Err(e) = display.flush_clients() {
            log::warn!("the display did not flush: {e}");
        }
    }

    /// Resize the session: the output's mode, and the resolution future
    /// windows are configured and captured at. Windows are not re-configured
    /// here; whoever wants one resized asks the window itself, through
    /// `wayland::Window::set_size`.
    pub fn set_size(&mut self, size: (u32, u32)) {
        let mode = smithay::output::Mode {
            size: (size.0 as i32, size.1 as i32).into(),
            refresh: 60_000,
        };
        self.state.output.change_current_state(Some(mode), None, None, None);
        self.state.output.set_preferred(mode);
        self.state.server_size = size;
        log::info!("resized the session to {}x{}", size.0, size.1);
    }
}

impl Drop for WaylandServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.runtime_dir);
    }
}
