//! tuigui-comp: an in-process headless Wayland compositor.
//!
//! Instead of shelling out to the wlroots-based `cage` binary and attaching
//! to it from the outside (separate screencopy client + virtual input
//! protocols), this crate *is* the compositor: a headless single-window
//! Wayland session running on a dedicated thread inside our own process.
//!
//! - capture is a read of the committed client buffer (already in our address
//!   space) instead of a `wlr-screencopy` round-trip through a shm pool,
//! - input injection is a direct seat dispatch instead of
//!   `wlr-virtual-pointer` / `zwp-virtual-keyboard` protocol calls.
//!
//! Hard window policy: exactly one toplevel is ever admitted. A client that
//! tries to spawn a second window gets that surface closed immediately. In
//! the future the compositor itself will spawn additional *clients* and
//! manage them as separate handles; the one-surface-per-handle contract
//! stands.
//!
//! See `spec/COMP.md` for the design notes.

// Reserved for upcoming milestones: OUTPUT_FORMAT feeds the GL capture path,
// geometry/mark_dead/window_contains the multi-window policy work.
#![allow(dead_code)]

mod server;
mod state;
mod surface;
pub mod tui;
mod xwm;

use std::io::{Read as _, Write as _};
use std::path::PathBuf;
use std::process::Child;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use smithay::reexports::calloop::channel as calloop_channel;

/// Bound on a [`Client`]/[`Compositor`] control round-trip through the wl
/// thread. Real-time discipline: no call waits forever, so a stalled compositor
/// thread can never wedge the caller (or, at startup, starve every tokio worker
/// and freeze the UI). A caller that hits this should retry; the request is
/// simply not serviced within budget.
const RPC_TIMEOUT: Duration = Duration::from_millis(500);

/// How many bytes of the child's stderr to retain for diagnostics. Bounded so
/// a chatty client can't grow memory without limit.
const STDERR_TAIL_CAP: usize = 32 * 1024;

/// Synchronized tail of a spawned child's stderr (most recent bytes). Populated
/// by a dedicated reader thread so we never block on the child's pipe; surfaced
/// in errors and via [`Client::stderr_tail`] for troubleshooting headless
/// clients that refuse our minimal compositor.
#[derive(Clone)]
struct StderrTail(Arc<Mutex<Vec<u8>>>);

impl StderrTail {
    fn new() -> Self {
        StderrTail(Arc::new(Mutex::new(Vec::new())))
    }
    fn push(&self, bytes: &[u8]) {
        let mut buf = self.0.lock().unwrap();
        buf.extend_from_slice(bytes);
        if buf.len() > STDERR_TAIL_CAP {
            let surplus = buf.len() - STDERR_TAIL_CAP;
            buf.drain(..surplus);
        }
    }
    fn as_string(&self) -> String {
        let buf = self.0.lock().unwrap();
        String::from_utf8_lossy(&buf).into_owned()
    }
    /// The last non-empty stderr line, or empty if nothing captured.
    fn last_line(&self) -> String {
        let s = self.as_string();
        s.lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .to_string()
    }
}

/// A single captured frame.
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

/// Axis-aligned pixel-space rectangle (mirrors `tuigui_streamer::Rect` shape
/// without forcing a dependency the other direction).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// A keyboard key for `Client::key`. Named keys cover the common navigation
/// and editing set; [`Key::Char`] covers printable text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Return,
    Tab,
    Backspace,
    Delete,
    Escape,
    Space,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
}

impl Key {
    /// The XKB keycode: evdev code + 8 (X's broken keycode convention).
    fn keycode(self) -> smithay::backend::input::Keycode {
        smithay::backend::input::Keycode::new(self.evdev_code() + 8)
    }

    fn evdev_code(self) -> u32 {
        // linux/input-event-codes.h values.
        const KEY_BACKSPACE: u32 = 14;
        const KEY_TAB: u32 = 15;
        const KEY_ENTER: u32 = 28;
        const KEY_ESC: u32 = 1;
        const KEY_SPACE: u32 = 57;
        const KEY_DELETE: u32 = 111;
        const KEY_HOME: u32 = 102;
        const KEY_END: u32 = 106;
        const KEY_PAGEUP: u32 = 104;
        const KEY_PAGEDOWN: u32 = 109;
        const KEY_LEFT: u32 = 105;
        const KEY_RIGHT: u32 = 106;
        const KEY_UP: u32 = 103;
        const KEY_DOWN: u32 = 108;
        match self {
            Key::Char(c) => char_evdev(c),
            Key::Return => KEY_ENTER,
            Key::Tab => KEY_TAB,
            Key::Backspace => KEY_BACKSPACE,
            Key::Delete => KEY_DELETE,
            Key::Escape => KEY_ESC,
            Key::Space => KEY_SPACE,
            Key::Left => KEY_LEFT,
            Key::Right => KEY_RIGHT,
            Key::Up => KEY_UP,
            Key::Down => KEY_DOWN,
            Key::Home => KEY_HOME,
            Key::End => KEY_END,
            Key::PageUp => KEY_PAGEUP,
            Key::PageDown => KEY_PAGEDOWN,
        }
    }
}

/// evdev code for a printable ASCII char on the standard PC 105-key layout
/// (unshifted symbol row).
fn char_evdev(c: char) -> u32 {
    const ROW_NUM: [u32; 10] = [11, 2, 3, 4, 5, 6, 7, 8, 9, 10]; // 1..0
    match c {
        '1'..='9' => ROW_NUM[(c as u8 - b'1') as usize],
        '0' => ROW_NUM[9],
        'a'..='z' | 'A'..='Z' => {
            let lower = c.to_ascii_lowercase();
            30 + (lower as u8 - b'a') as u32
        }
        ',' => 51,
        '.' => 52,
        '/' => 53,
        ';' => 39,
        '\'' => 40,
        '[' => 26,
        ']' => 27,
        '\\' => 43,
        '-' => 12,
        '=' => 13,
        '`' => 41,
        _ => 0,
    }
}

/// A mouse button for `Client::click`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
    Middle,
}

impl Button {
    fn evdev_code(self) -> u32 {
        match self {
            Button::Left => 0x110,   // BTN_LEFT
            Button::Right => 0x111,  // BTN_RIGHT
            Button::Middle => 0x112, // BTN_MIDDLE
        }
    }
}

/// Keyboard key state for `Client::key`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyState {
    Pressed,
    Released,
}

/// Scroll delta in discrete wheel steps (positive = down / right).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ScrollDelta {
    pub vertical: i32,
    pub horizontal: i32,
}

impl ScrollDelta {
    pub fn vertical(steps: i32) -> Self {
        ScrollDelta {
            vertical: steps,
            horizontal: 0,
        }
    }
}

/// The compositor's current pointer position and button state, queried via
/// [`Client::pointer_state`]. Used by the terminal shell to show where the
/// pointer is while debugging input delivery to a headless client.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct PointerState {
    /// Pointer position in output pixels.
    pub x: f64,
    pub y: f64,
    /// True if any button is currently held.
    pub button_down: bool,
}

/// Crate-level error.
#[derive(Debug, thiserror::Error)]
pub enum CompError {
    #[error("compositor server failed: {0}")]
    Server(String),
    #[error("compositor thread died: {0}")]
    Thread(String),
    #[error("client not ready yet (no committed frame)")]
    NotReady,
    #[error("window is gone (client exited or closed the window)")]
    WindowGone,
    #[error("spawn failed: {0}")]
    Spawn(String),
    #[error("capture failed: {0}")]
    Capture(String),
    #[error("control round-trip timed out (compositor thread stalled)")]
    Timeout,
}

/// Result alias used across the crate.
pub type Result<T, E = CompError> = std::result::Result<T, E>;

/// Tunables for [`Compositor::init_with_options`].
#[derive(Debug, Clone, Copy)]
pub struct CompositorOptions {
    /// Draw a software cursor overlay on captured frames. Defaults to true.
    pub show_cursor: bool,
    /// Spawn an XWayland server so X11-only clients can render through the
    /// compositor. When false, only native Wayland clients are supported
    /// (`DISPLAY` is left unset). Defaults to true.
    pub use_x11: bool,
}

impl Default for CompositorOptions {
    fn default() -> Self {
        CompositorOptions {
            show_cursor: true,
            use_x11: true,
        }
    }
}

/// The compositor: owns the headless session on a dedicated thread.
pub struct Compositor {
    tx: calloop_channel::Sender<server::Request>,
    thread: std::thread::JoinHandle<()>,
    socket: PathBuf,
    /// Private per-session runtime dir holding our socket. The child's
    /// `XDG_RUNTIME_DIR` points here so it can only discover our socket, never
    /// the host compositor's. Removed on drop.
    runtime_dir: std::path::PathBuf,
    size: (u32, u32),
    /// Client content scale (`--ui-scale / --scale`). Applied to each admission
    /// via `ClientHandle::set_client_scale`, so content stays a constant on-screen
    /// size when the `--scale` resolution divisor changes. Buffer sizes are
    /// unaffected (the window still fills the full output).
    client_scale: f64,
    /// X11 display number for the XWayland server, once it starts. Read at
    /// `spawn` time to set `DISPLAY` for X11-routed clients.
    x11_display: std::sync::Arc<std::sync::RwLock<Option<String>>>,
    /// Whether XWayland is known to be unavailable; `spawn`'s display wait
    /// gives up deterministically when this is set instead of racing a timeout.
    x11_failed: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Whether to spawn XWayland and route X11-only clients to it (`DISPLAY`).
    /// `Compositor` needs this in `spawn` to decide whether to wait on
    /// `x11_display` at all.
    use_x11: bool,
}

impl Compositor {
    /// Start the headless session. The output resolution is the single
    /// window's size (MVP: every spawn uses the same fixed resolution).
    ///
    /// `client_scale` (`--ui-scale / --scale`) is applied to clients via
    /// `ClientHandle::set_client_scale`, so content stays a constant on-screen
    /// size when the `--scale` resolution divisor changes — without shrinking
    /// the window (buffer sizes are unaffected). The wl_output scale itself
    /// stays 1.0 so the window always fills the full output.
    pub fn init(size: (u32, u32), client_scale: f64) -> Result<Self, CompError> {
        Self::init_with_options(size, client_scale, CompositorOptions::default())
    }

    /// Start the headless session with explicit options. See [`Compositor::init`].
    pub fn init_with_options(
        size: (u32, u32),
        client_scale: f64,
        options: CompositorOptions,
    ) -> Result<Self, CompError> {
        // A private runtime dir, isolated from the caller's (Hyprland's).
        // The child gets this as XDG_RUNTIME_DIR so the only Wayland socket it
        // can auto-discover is ours; the host compositor simply isn't there.
        let runtime_dir = std::env::temp_dir().join(format!(
            "tuigui-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&runtime_dir)
            .map_err(|e| CompError::Server(format!("create runtime dir: {e}")))?;
        let srv = server::Server::new(
            size,
            client_scale,
            &runtime_dir,
            options.show_cursor,
            options.use_x11,
        )?;
        // Query the socket name before handing the server (and its socket) to
        // the compositor thread.
        let socket = srv.socket_name();
        let x11_display = srv.x11_display.clone();
        let x11_failed = srv.x11_failed.clone();
        let (tx, rx) = calloop_channel::channel::<server::Request>();
        let (win_tx, win_rx) = std::sync::mpsc::channel::<std::os::unix::net::UnixStream>();
        let _ = &win_tx; // accepted on the wl thread via pending_windows
        let thread = std::thread::Builder::new()
            .name("tuigui-comp".into())
            .spawn(move || {
                if let Err(e) = state::run_thread(srv, rx, win_rx) {
                    tracing::error!("comp thread failed: {e}");
                }
            })
            .map_err(|e| CompError::Thread(e.to_string()))?;
        Ok(Compositor {
            tx,
            thread,
            socket,
            runtime_dir,
            size,
            client_scale,
            x11_display,
            x11_failed,
            use_x11: options.use_x11,
        })
    }

    /// Path to this session's private Wayland socket (for `WAYLAND_DISPLAY`).
    pub fn wayland_socket(&self) -> &PathBuf {
        &self.socket
    }

    /// Spawn `path` with the given resolution, wait for its first toplevel
    /// commit, and return a handle. Native Wayland clients reach our private
    /// socket via `WAYLAND_DISPLAY`; X11-only clients are routed to the
    /// XWayland server we own via `DISPLAY`.
    pub fn spawn(
        &self,
        path: impl AsRef<std::path::Path>,
        resolution: (u32, u32),
    ) -> Result<Client, CompError> {
        if resolution != self.size {
            // MVP: one fixed output size; the configure honors the requested
            // size only when it matches the output.
            return Err(CompError::Spawn(format!(
                "resolution {resolution:?} != session size {:?}; the MVP session is fixed-size",
                self.size
            )));
        }
        tracing::info!(path = %path.as_ref().display(), "spawning client");
        // Launch the child able to reach both display stacks: a *native* Wayland
        // client connects straight to our compositor via `WAYLAND_DISPLAY`, while
        // an X11-only client (glxgears, xterm) falls back to our XWayland via
        // `DISPLAY`. Scrub the env so it can only reach what we provide:
        //   - WAYLAND_DISPLAY -> our private socket name (relative to
        //     XDG_RUNTIME_DIR), so native clients hit our compositor,
        //   - WAYLAND_SOCKET -> removed (a client-libvar that would pin another
        //     compositor's socket),
        //   - DISPLAY -> our own XWayland instance (set once its WM is up), never
        //     the host's ambient DISPLAY,
        //   - XDG_RUNTIME_DIR -> our private dir, so the `wayland-*` socket a
        //     client auto-discovers (or resolves WAYLAND_DISPLAY against) is
        //     always ours, never a host compositor's,
        //   - GDK_BACKEND / QT_QPA_PLATFORM -> removed so toolkits auto-detect;
        //     with WAYLAND_DISPLAY set they pick our compositor natively, and
        //     X11-only apps still use X11,
        //   - HYPRLAND_INSTANCE_SIGNATURE -> removed, so hyprland-specific
        //     autodiscovery (e.g. the drop-in `wayland-*` socket path) fails.
        let wl_name = self
            .socket
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        let mut child = std::process::Command::new(path.as_ref());
        child
            .env("WAYLAND_DISPLAY", wl_name)
            .env_remove("WAYLAND_SOCKET")
            .env("XDG_RUNTIME_DIR", &self.runtime_dir)
            .env_remove("GDK_BACKEND")
            .env_remove("QT_QPA_PLATFORM")
            .env_remove("HYPRLAND_INSTANCE_SIGNATURE");
        // If XWayland is running, point `DISPLAY` at it. Wait a short, bounded
        // beat for it to become ready so a pure-X11 app (glxgears) launched
        // right after init doesn't miss its display. When X11 is disabled
        // (`use_x11` false) leave `DISPLAY` unset and skip the wait entirely.
        if self.use_x11 {
            set_display_env(&self.x11_display, &self.x11_failed, &mut child);
        } else {
            child.env_remove("DISPLAY");
        }
        let mut child = child
            // Pipe (not inherit) the child's stdout: a GUI app's own terminal
            // writes (glxgears prints FPS to stdout) would otherwise bleed into
            // the TUI and corrupt the live display. stderr is drained into a
            // tail for diagnostics.
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| CompError::Spawn(format!("{}: {e}", path.as_ref().display())))?;

        // Discard the child's stdout: we don't need it and it must not reach
        // the terminal (which is showing the TUI).
        if let Some(mut out) = child.stdout.take() {
            std::thread::Builder::new()
                .name("tuigui-comp-stdout".into())
                .spawn(move || {
                    let mut tmp = [0u8; 2048];
                    while out.read(&mut tmp).is_ok() {}
                })
                .ok();
        }

        // Drain the child's stderr on a reader thread into a bounded tail so we
        // can surface *why* a headless client refused us (e.g. GTK's "The
        // Wayland compositor does not provide one or more of the required
        // interfaces") instead of a bare "spawn failed". Non-blocking for us:
        // we only read from the shared tail under a mutex.
        let stderr = StderrTail::new();
        let stderr_buf = stderr.clone();
        if let Some(mut er) = child.stderr.take() {
            std::thread::Builder::new()
                .name("tuigui-comp-stderr".into())
                .spawn(move || {
                    // Diagnostic: also persist the child's stderr to a file so
                    // a headless client's error output survives after shutdown.
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
                .ok();
        }

        // Window admission happens on the wl thread when the client's first
        // toplevel commits; poll capture readiness for the ack.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match self.try_capture() {
                Ok(_) => break,
                // Transient: not committed yet, or the wl thread was too busy
                // to answer within the round-trip budget. Keep polling.
                Err(CompError::NotReady) | Err(CompError::Timeout) => {
                    // The child seems alive; give it a beat to finish starting.
                    if !Self::child_ok(&mut child) {
                        // Include the client's own stderr tail so GTK/Qt
                        // diagnostics (e.g. "not providing required interfaces")
                        // are visible instead of a bare "exited early".
                        let why = stderr.last_line();
                        return Err(CompError::Spawn(if why.is_empty() {
                            "client process exited early (captured no diagnostic output)".into()
                        } else {
                            format!("client process exited early: last stderr: {why}")
                        }));
                    }
                    if Instant::now() > deadline {
                        let _ = child.kill();
                        let why = stderr.last_line();
                        return Err(CompError::Spawn(if why.is_empty() {
                            format!(
                                "client {} never committed a frame within 10s (no diagnostic output)",
                                path.as_ref().display()
                            )
                        } else {
                            format!(
                                "client {} never committed a frame within 10s; last stderr: {why}",
                                path.as_ref().display()
                            )
                        }));
                    }
                    std::thread::sleep(Duration::from_millis(15));
                }
                Err(e) => {
                    let _ = child.kill();
                    return Err(e);
                }
            }
        }
        Ok(Client {
            tx: self.tx.clone(),
            child: Arc::new(Mutex::new(Some(child))),
            stderr: stderr.clone(),
            size: self.size,
        })
    }

    /// True while the spawned child is still alive.
    fn child_ok(child: &mut Child) -> bool {
        child.try_wait().map(|st| st.is_none()).unwrap_or(true)
    }

    /// One-shot capture without a client handle (used by `spawn` internally).
    fn try_capture(&self) -> Result<Frame, CompError> {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        self.tx
            .send(server::Request::Capture { reply: reply_tx })
            .map_err(|_| CompError::Thread("compositor thread stopped".into()))?;
        recv_reply(reply_rx)?
    }

    /// Shut the session down (joins the thread).
    pub fn shutdown(self) {
        drop(self.tx);
        let _ = self.thread.join();
        let _ = std::fs::remove_dir_all(&self.runtime_dir);
    }
}

/// Receive a control-channel reply with a hard [`RPC_TIMEOUT`] deadline so no
/// API call ever blocks forever on a stalled wl thread.
fn recv_reply<T>(reply_rx: std::sync::mpsc::Receiver<T>) -> Result<T, CompError> {
    reply_rx
        .recv_timeout(RPC_TIMEOUT)
        .map_err(|_| CompError::Timeout)
}

/// Set (or clear) `DISPLAY` on a child command from the XWayland display slot.
///
/// The compositor thread fills `x11_display` only once the X11 window manager
/// is actually running (not merely when the X server listens), because a
/// client that connects before the WM holds `WM_S0` dies with "Can't open
/// display". WM startup is normally milliseconds, but can take longer on a
/// cold NixOS first-init (glamor -> swrast fallback), so we wait — bounded
/// only by either the display appearing (win) or `x11_failed` being set
/// (XWayland deterministically unavailable). An arbitrary deadline that races
/// the WM would set `DISPLAY` too early and break X11 apps.
///
/// If XWayland is unavailable we leave `DISPLAY` unset (the host's ambient
/// display is intentionally never inherited).
fn set_display_env(
    x11_display: &std::sync::Arc<std::sync::RwLock<Option<String>>>,
    x11_failed: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    child: &mut std::process::Command,
) {
    use std::sync::atomic::Ordering;
    // Wait until the XWayland WM is fully up and has published DISPLAY before
    // returning, so the caller only spawns the X11 client once the display is
    // genuinely available. Give up only when XWayland is deterministically
    // unavailable.
    loop {
        match x11_display.read() {
            Ok(guard) => match guard.as_ref() {
                Some(d) => {
                    child.env("DISPLAY", d.clone());
                    return;
                }
                None => {
                    if x11_failed.load(Ordering::Acquire) {
                        child.env_remove("DISPLAY");
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            },
            Err(_) => {
                child.env_remove("DISPLAY");
                return;
            }
        }
    }
}

/// A running client window: capture + input injection.
pub struct Client {
    tx: calloop_channel::Sender<server::Request>,
    child: Arc<Mutex<Option<Child>>>,
    stderr: StderrTail,
    size: (u32, u32),
}

impl Clone for Client {
    fn clone(&self) -> Self {
        Client {
            tx: self.tx.clone(),
            child: self.child.clone(),
            stderr: self.stderr.clone(),
            size: self.size,
        }
    }
}

impl Client {
    /// The tail of the client process's stderr (most recent bytes captured).
    /// Useful for diagnosing why a headless client refused our compositor
    /// (GTK/Qt print which Wayland interfaces they need here).
    pub fn stderr_tail(&self) -> String {
        self.stderr.as_string()
    }
    /// Capture the window contents as RGBA8 + damage.
    ///
    /// This is the efficient path the crate exists for: the pixels are read
    /// straight out of the compositor's own buffer, no protocol round-trip.
    pub fn capture(&self) -> Result<Frame, CompError> {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        self.tx
            .send(server::Request::Capture { reply: reply_tx })
            .map_err(|_| CompError::Thread("compositor thread stopped".into()))?;
        recv_reply(reply_rx)?
    }

    /// Send a key press + release pair.
    pub fn type_key(&self, key: Key) -> Result<(), CompError> {
        self.key(key, KeyState::Pressed)?;
        self.key(key, KeyState::Released)
    }

    /// Send a single key event with explicit state.
    pub fn key(&self, key: Key, state: KeyState) -> Result<(), CompError> {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        self.tx
            .send(server::Request::Key {
                key,
                state,
                reply: reply_tx,
            })
            .map_err(|_| CompError::Thread("compositor thread stopped".into()))?;
        recv_reply(reply_rx)?
    }

    /// Move the pointer to an absolute position inside the window.
    pub fn move_mouse(&self, x: f64, y: f64) -> Result<(), CompError> {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        self.tx
            .send(server::Request::MouseMove {
                x,
                y,
                reply: reply_tx,
            })
            .map_err(|_| CompError::Thread("compositor thread stopped".into()))?;
        recv_reply(reply_rx)?
    }

    /// Click a button at the current pointer position.
    pub fn click(&self, button: Button) -> Result<(), CompError> {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        self.tx
            .send(server::Request::Click {
                button,
                reply: reply_tx,
            })
            .map_err(|_| CompError::Thread("compositor thread stopped".into()))?;
        recv_reply(reply_rx)?
    }

    /// Scroll by discrete wheel steps (positive = down / right).
    pub fn scroll(&self, delta: ScrollDelta) -> Result<(), CompError> {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        self.tx
            .send(server::Request::Scroll {
                delta,
                reply: reply_tx,
            })
            .map_err(|_| CompError::Thread("compositor thread stopped".into()))?;
        recv_reply(reply_rx)?
    }

    /// The compositor's current pointer position and button state.
    pub fn pointer_state(&self) -> Result<PointerState, CompError> {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        self.tx
            .send(server::Request::PointerState { reply: reply_tx })
            .map_err(|_| CompError::Thread("compositor thread stopped".into()))?;
        recv_reply(reply_rx)?
    }

    /// The client-facing window size.
    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// Whether the client process is still running.
    pub fn is_running(&self) -> bool {
        let mut guard = self.child.lock().unwrap();
        match guard.as_mut() {
            Some(child) => child.try_wait().map(|st| st.is_none()).unwrap_or(true),
            None => false,
        }
    }

    /// Wait for the child to exit and return its status.
    pub fn wait(self) -> Option<std::process::ExitStatus> {
        let mut guard = self.child.lock().unwrap();
        if let Some(mut child) = guard.take() {
            return child.wait().ok();
        }
        None
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        // Only kill the child when the last Client clone drops. A transient
        // clone (e.g. the per-frame one in the capture source) must not tear
        // the app down while other clones still own it.
        if Arc::strong_count(&self.child) != 1 {
            return;
        }
        let mut guard = self.child.lock().unwrap();
        if let Some(child) = guard.take() {
            let mut child = child;
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Adapter: any crate-level [`CompError`] collapses into the core error enum.
fn into_core_err(_: CompError) -> tuigui_core::Err {
    tuigui_core::Err::Unknown
}

impl tuigui_core::TGCompositor for Compositor {
    // The trait's `init` is infallible and takes dpi; our real init is fallible
    // and takes a client scale. We interpret dpi 96 == scale 1.0 and panic on
    // genuine startup failure (thread/socket creation) rather than paper over
    // it — starting a compositor is one-shot at the top of the session.
    fn init(resolution: (u32, u32), dpi: u32) -> Self {
        let scale = dpi as f64 / 96.0;
        Compositor::init(resolution, scale)
            .expect("tuigui-core: compositor init failed")
    }

    fn spawn(&mut self, command: std::process::Command) -> Result<impl tuigui_core::TGClient, tuigui_core::Err> {
        Compositor::spawn(self, command.get_program(), self.size)
            .map_err(into_core_err)
    }
}

impl tuigui_core::TGClient for Client {
    fn snapshot(&self) -> Result<tuigui_core::Frame, tuigui_core::Err> {
        self.capture().map(|f| tuigui_core::Frame {
            size: (f.width, f.height),
            rgba: f.rgba,
        })
        .map_err(into_core_err)
    }

    fn forward_input() {}

    // The MVP admits exactly one toplevel, so there are never children to
    // hand out.
    fn children(&self) -> Vec<&impl tuigui_core::TGClient> {
        Vec::<&Client>::new()
    }
}
