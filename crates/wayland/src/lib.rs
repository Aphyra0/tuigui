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
mod surface;
mod utils;
mod window;

pub use client::{RootClient, SubClient};
pub use error::{CompError, Result};
pub use inputs::{Button, Key, KeyState, PointerState, ScrollDelta};
pub use window::{Input, Window};

use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::{Duration, Instant};

use smithay::output::GlobalId;
use smithay::reexports::calloop::EventLoop;
use smithay::reexports::wayland_server::{Display, ListeningSocket};

use crate::client::StderrTail;
use crate::state::{fire_frame_callbacks, State};

/// A single captured frame.
#[derive(Clone)]
pub struct Frame {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// RGBA8, row-major, top-left origin (`width * height * 4` bytes).
    pub rgba: Vec<u8>,
    /// Damaged regions since the previous capture, in pixel coordinates.
    /// Empty means "unknown / treat everything as changed".
    pub damage: Vec<crate::Rect>,
}

/// Axis-aligned pixel-space rectangle (mirrors the renderer's `Rect` shape
/// without forcing a dependency the other direction).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

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
pub struct WaylandServer<C = (), W = ()> {
    /// The session state: the root client, its windows, the seat, the X11 side.
    state: State<C, W>,
    event_loop: EventLoop<'static, State<C, W>>,
    display: Display<State<C, W>>,
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

impl<C, W> WaylandServer<C, W> {
    /// Start the headless session: display, private socket, the single output,
    /// the seat, and (when asked) the XWayland server.
    #[tracing::instrument(name = "server.new", skip_all)]
    pub fn new(
        options: Options,
    ) -> Result<Self, CompError>
    where
        C: Default,
    {
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
        let output_global = output.create_global::<State<C, W>>(&dh);

        // The globals, the seat with its keyboard and pointer, and the output.
        let state = State::new(&dh, output, options.resolution, options.dpi, options.show_cursor)?;

        // The loop that sources ride on; XWayland's source joins when asked.
        let mut event_loop =
            EventLoop::try_new().map_err(|e| CompError::Server(e.to_string()))?;
        if options.use_x11 {
            state.spawn_xwayland(&dh, event_loop.handle())?;
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
    /// child gets this as `XDG_RUNTIME_DIR` so the only Wayland socket it can
    /// auto-discover is ours; the host compositor simply isn't there.
    fn new_runtime_dir() -> Result<PathBuf, CompError> {
        let runtime_dir = std::env::temp_dir().join(format!(
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

    /// Advance the session: accept new clients, drain their input mailboxes,
    /// dispatch their protocol, fire pending frame callbacks, and flush
    /// everything back down the sockets.
    ///
    /// The server only moves forward when this is called, so the caller's loop
    /// is the session's heartbeat.
    #[tracing::instrument(name = "server.step", skip(self))]
    pub fn step(&mut self, timeout: Duration) -> Result<(), CompError> {
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
            state.accept_client(stream)?;
        }

        // Queued inputs dispatch here, against the window each queue came
        // from, so they land at a well-defined point in the protocol stream.
        state.drain_inputs();

        event_loop
            .dispatch(Some(timeout), state)
            .map_err(|e| CompError::Server(e.to_string()))?;
        display
            .dispatch_clients(state)
            .map_err(|e| CompError::Server(e.to_string()))?;
        display
            .flush_clients()
            .map_err(|e| CompError::Server(e.to_string()))?;

        // Frame callbacks fire one `done()` per turn, so the client animates
        // as long as the caller steps. The extra dispatch sends those `done()`
        // events out before the turn ends.
        let surfaces = state.window_surfaces();
        for surface in surfaces {
            fire_frame_callbacks(&surface);
            display
                .dispatch_clients(state)
                .map_err(|e| CompError::Server(e.to_string()))?;
            display
                .flush_clients()
                .map_err(|e| CompError::Server(e.to_string()))?;
        }
        Ok(())
    }

    /// The root client, and through it every window it has.
    ///
    /// Weak, upgraded on use: the owning `Arc` is the connection's data slot,
    /// so the record lives exactly as long as the client's wayland
    /// connection.
    pub fn root_client(&self) -> Option<Arc<RootClient<C, W>>> {
        self.state.root_client.as_ref().and_then(Weak::upgrade)
    }

    /// Spawn `path` into the session and wait for its first toplevel commit.
    ///
    /// Native Wayland clients reach our private socket via `WAYLAND_DISPLAY`;
    /// X11-only clients are routed to the XWayland server we own via `DISPLAY`.
    /// While waiting, this server is stepped, so the client's protocol and our
    /// admission both progress on the same thread.
    #[tracing::instrument(name = "server.spawn", skip_all)]
    pub fn spawn(&mut self, path: impl AsRef<Path>) -> Result<Arc<RootClient<C, W>>, CompError>
    where
        C: Default,
    {
        tracing::info!(path = %path.as_ref().display(), "spawning client");

        // The command that can reach both display stacks, scrubbed of
        // everything that could reach a host compositor.
        let mut command = Self::new_command(path.as_ref(), &self.socket_name, &self.runtime_dir);

        // X11-only clients are routed to our XWayland; step until it is up.
        if self.use_x11 {
            self.route_display(&mut command)?;
        } else {
            command.env_remove("DISPLAY");
        }

        // The child starts piped: stdout discarded, stderr tailed.
        let (child, stderr, stderr_reader) = Self::start(command, path.as_ref())?;

        // The child becomes the session's root client; its connection
        // attaches at the next step.
        self.state.attach_root_client(child, stderr.clone())?;

        // Wait for the window's first commit, so the first capture after
        // this is a real one rather than the empty output.
        self.wait_for_first_commit(&stderr, stderr_reader, path.as_ref())?;

        let root = self
            .state
            .root_client
            .as_ref()
            .and_then(Weak::upgrade)
            .expect("the root client was just attached");
        Ok(root)
    }

    /// The command a client is launched with: a native Wayland client
    /// connects to us via `WAYLAND_DISPLAY`; an X11-only one falls back to
    /// our XWayland via `DISPLAY` (routed separately, in `route_display`).
    /// The env is scrubbed so the only display it can reach is ours.
    fn new_command(
        path: &Path,
        socket_name: &Path,
        runtime_dir: &Path,
    ) -> std::process::Command {
        let wl_name = socket_name
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        let mut command = std::process::Command::new(path);
        command
            .env("WAYLAND_DISPLAY", wl_name)
            .env_remove("WAYLAND_SOCKET")
            .env("XDG_RUNTIME_DIR", runtime_dir)
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
                    self.step(Duration::from_millis(10))?;
                }
            }
        }
    }

    /// Start the child. Its stdout is discarded on a reader thread — a GUI
    /// app's own terminal writes (glxgears prints FPS) would otherwise bleed
    /// into the TUI — and its stderr is drained into a bounded tail, so the
    /// reason a headless client refused us survives in the log.
    ///
    /// Returns the child, the tail, and the reader's handle. The handle is
    /// kept, because a child that dies immediately is the case the tail
    /// exists for and the case it races: the wait loop below can see the
    /// process gone before the reader has read a byte. The early-exit path
    /// joins the reader first — instant once the writer is dead, the pipe at
    /// EOF.
    fn start(
        mut command: std::process::Command,
        path: &Path,
    ) -> Result<(Child, StderrTail, Option<std::thread::JoinHandle<()>>), CompError> {
        command
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|e| CompError::Spawn(format!("{}: {e}", path.display())))?;

        if let Some(mut out) = child.stdout.take() {
            std::thread::Builder::new()
                .name("compositor-out".into())
                .spawn(move || {
                    let _zone = tracing::info_span!("compositor.stdout").entered();
                    let mut tmp = [0u8; 2048];
                    while out.read(&mut tmp).is_ok() {}
                })
                .ok();
        }

        // Drained on a reader thread so we never block on the child's pipe;
        // the tail is only read under a mutex. A thread that would not start
        // is not a reason to fail the spawn: the child is running, and its
        // output is only diagnostics. The `flatten` covers both the pipe
        // being absent and the thread failing; `None` means "nothing to
        // join, and nothing said".
        let stderr = StderrTail::new();
        let stderr_buf = stderr.clone();
        let stderr_reader = child
            .stderr
            .take()
            .map(|mut er| {
                std::thread::Builder::new()
                    .name("compositor-err".into())
                    .spawn(move || {
                        // Also persist the tail to a file, so a headless
                        // client's error output survives after shutdown.
                        let trace_path = std::path::PathBuf::from(format!(
                            "/tmp/comp_child_stderr-{}.log",
                            std::process::id()
                        ));
                        let mut tmp = [0u8; 2048];
                        let mut file = std::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(&trace_path)
                            .ok();
                        loop {
                            match er.read(&mut tmp) {
                                Ok(0) => break,
                                Ok(n) => {
                                    stderr_buf.push(&tmp[..n]);
                                    if let Some(f) = file.as_mut() {
                                        let _ = std::io::Write::write_all(f, &tmp[..n]);
                                        let _ = f.flush();
                                    }
                                }
                                Err(_) => break,
                            }
                        }
                    })
                    .ok()
            })
            .flatten();
        Ok((child, stderr, stderr_reader))
    }

    /// Step the session until the client's window has committed a buffer, so
    /// the first capture after this is a real one rather than the empty
    /// output.
    fn wait_for_first_commit(
        &mut self,
        stderr: &StderrTail,
        stderr_reader: Option<std::thread::JoinHandle<()>>,
        path: &Path,
    ) -> Result<(), CompError> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            self.step(STEP_TIMEOUT)?;
            let captured = self
                .state
                .root_client
                .as_ref()
                .and_then(Weak::upgrade)
                .and_then(|root| root.root_window().map(|w| w.capture()));
            match captured {
                Some(Ok(_)) => return Ok(()),
                // Transient: not committed yet. Keep stepping and polling.
                Some(Err(CompError::NotReady)) | None => {
                    // The child seems alive; give it a beat to finish starting.
                    if !Self::child_ok(path) {
                        // The child is gone, so its stderr pipe is at EOF and
                        // the reader has everything it will ever have. Join it
                        // before reading: a reader that has not run yet would
                        // report an empty tail, which is the one thing this
                        // path exists to avoid.
                        let why = drain(stderr_reader, stderr);
                        return Err(CompError::Spawn(if why.is_empty() {
                            "client process exited early (it produced no output at all)".into()
                        } else {
                            format!("client process exited early: {why}")
                        }));
                    }
                    if Instant::now() > deadline {
                        // Kill via the root client: dropping the record kills
                        // its process, and the Weak dies with the slot.
                        let child = self.state.take_root_client();
                        drop(child);
                        // Killed, so the pipe is closed and the reader is at
                        // its end.
                        let why = drain(stderr_reader, stderr);
                        let named = path.display();
                        return Err(CompError::Spawn(if why.is_empty() {
                            format!("client {named} never committed a frame within 10s (no output)")
                        } else {
                            format!("client {named} never committed a frame within 10s: {why}")
                        }));
                    }
                }
                Some(Err(e)) => {
                    let child = self.state.take_root_client();
                    drop(child);
                    return Err(e);
                }
            }
        }
    }

    /// True while the spawned child is still alive.
    fn child_ok(path: &Path) -> bool {
        unimplemented!()
    }
}

impl<C, W> Drop for WaylandServer<C, W> {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.runtime_dir);
    }
}

/// Finish reading a dead child's stderr, and hand back everything it said.
///
/// Called once the child is gone, so its end of the pipe is closed: the reader reaches EOF
/// and the join returns rather than waiting. That join is the whole point — the reader is a
/// thread, and a child that dies immediately is exactly the case where it has not run yet,
/// so a tail read without it says "no output" about a process that had plenty.
///
/// The whole tail, not its last line: a program explaining why it refused to start usually
/// says it over several lines, and the line a naive read keeps is often the least of them.
fn drain(reader: Option<std::thread::JoinHandle<()>>, tail: &StderrTail) -> String {
    if let Some(handle) = reader {
        let _ = handle.join();
    }
    tail.as_string()
}
