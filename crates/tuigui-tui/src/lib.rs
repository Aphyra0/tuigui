//! `tui` — the wrapping server: run a command in a pty we own, and paint
//! remotely-sent frames onto the real terminal out of band.
//!
//! ```text
//!   terminal  <── the command's output + our own TGP escapes ──┐
//!                                                              │
//!                    ┌──────────── tui ────────────┐          │
//!   `gui play` ──▶   │  pty master   →  real tty   │ ─────────┘
//!      (unix)        │  shm frame    →  TGP escape │
//!                    └─────────────────────────────┘
//! ```
//!
//! The wrapped command only ever sees text. Frames arrive on a side channel, are
//! staged in a POSIX shared memory object, and are shown by writing TGP straight
//! to the real terminal, deliberately bypassing the command (and any muxer it
//! runs) entirely.
//!
//! # Why a reserved image
//!
//! Unicode placeholders encode the image id in the cell's foreground color, so
//! the id must be known *before* the client draws the grid. easytgp allocates
//! the id during `transmit`, so the server transmits a 1x1 blank image at
//! startup to reserve one, publishes it in the session file, and creates the
//! virtual placement (`U=1`). Every later frame edits that same image in place:
//! the id never changes, so the client's placeholders never need repainting.
//!
//! Usage:
//!   tui <command> [args...]        # unix socket at $TUI_SOCK (default /tmp/tui.sock)
//!   tui --path P <command>         # choose the socket path

use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read as _, Write as _};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::net::UnixListener as StdUnixListener;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use easytgp::{Controller, DynImage, PlacementRequest};
use rustix::pty::{grantpt, openpt, ptsname, unlockpt, OpenptFlags};
use tuigui_wire::{p2p, session};

/// The placement id both images are given, so a swap moves one placement rather
/// than inventing a second. Placement ids are scoped to their image, so a value
/// here is not a collision with anything else on the terminal.
const PLACEMENT_ID: u32 = 1;

/// Serializes writes to the terminal, so the command's output pump and our own
/// escape batches never interleave mid-line.
///
/// Also the one place that sees everything the wrapped command emits, which is
/// why the placeholder check lives here: the client's grid travels up this
/// stream, and a cell naming the wrong image is invisible on screen but
/// diagnosable here.
#[derive(Clone)]
struct Out(Arc<Mutex<OutState>>);

struct OutState {
    stdout: io::Stdout,
    /// Parses the wrapped command's output, for placeholder cells and erases.
    scanner: p2p::PlaceholderScanner,
    /// A second parser for our own escaping, which travels `Out::write` rather
    /// than the pump.
    ///
    /// Separate from `scanner` because a parser holds a partial sequence across
    /// calls: sharing one between two independent byte streams would splice
    /// half an escape from one onto the other.
    own_apcs: p2p::PlaceholderScanner,
    /// The id the client was told to encode, once a client has asked.
    expected_id: Option<u32>,
    /// Set when a cell was seen carrying a color that is not `expected_id`.
    ///
    /// The pump runs on its own thread with only stdout in hand, so it cannot
    /// report directly. It records here and the accept loop drains it.
    mismatch: Arc<Mutex<Option<String>>>,
    /// High-water mark of erases already acted on, so [`Out::take_erases`] can
    /// hand back only the new ones.
    acted_on_erases: u64,
    /// Rewrites the grid's marker cells into kitty placeholder cells.
    ///
    /// Here rather than in the client because this is the only place the pane's
    /// bytes are visible *after* a multiplexer has re-rendered them, and the only
    /// place the live image id is known. A placeholder the client drew itself
    /// would be blanked by a muxer that parses its panes (see `p2p::MARKER`).
    rewriter: p2p::MarkerRewriter,
}

/// Feed bytes to `scanner` purely to collect the APC sequences among them.
fn record_apcs(scanner: &mut p2p::PlaceholderScanner, bytes: &[u8]) {
    scanner.push(bytes);
    scanner.found.clear();
}

impl io::Write for Out {
    /// Write straight through the same lock the output pump takes.
    ///
    /// This is how easytgp's escapes reach the terminal: installed as its sink,
    /// it lands here instead of writing fd 1 on its own. The two writers then
    /// share one lock and one stdout, so an escape can never land inside the
    /// wrapped command's bytes.
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut out = self.0.lock().unwrap();
        // Our own graphics travel this way, so this is the only place their
        // bytes can be observed. Recording them answers whether the transmit
        // reached the terminal at all, which nothing else can.
        record_apcs(&mut out.own_apcs, buf);
        let n = out.stdout.write(buf)?;
        out.stdout.flush()?;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.lock().unwrap().stdout.flush()
    }
}

impl Out {
    fn new(mismatch: Arc<Mutex<Option<String>>>) -> Self {
        Self(Arc::new(Mutex::new(OutState {
            stdout: io::stdout(),
            scanner: p2p::PlaceholderScanner::new(),
            own_apcs: p2p::PlaceholderScanner::new(),
            expected_id: None,
            mismatch,
            acted_on_erases: 0,
            rewriter: p2p::MarkerRewriter::new(),
        })))
    }

    /// The id the client's grid must encode from now on.
    fn expect_id(&self, id: u32) {
        self.0.lock().unwrap().expected_id = Some(id);
    }

    fn write_all(&self, bytes: &[u8]) {
        let mut out = self.0.lock().unwrap();
        let _ = out.stdout.write_all(bytes);
        let _ = out.stdout.flush();
    }

    fn pump(&self, from: &mut impl io::Read) {
        let mut buf = [0u8; 8192];
        loop {
            match from.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    self.check_placeholders(&buf[..n]);
                    self.rewrite_and_write(&buf[..n]);
                }
            }
        }
    }

    /// Rewrite any marker cells for the expected image, then write.
    ///
    /// Separate from [`Out::write_all`] because only this stream carries the
    /// client's grid: the escapes easytgp sends travel the other path and must
    /// not be scanned for cells.
    fn rewrite_and_write(&self, bytes: &[u8]) {
        let mut out = self.0.lock().unwrap();
        let id = out.expected_id.unwrap_or(0);
        // With no client yet the grid cannot be addressed, so a marker is left as
        // the block it looks like rather than becoming a placeholder naming image
        // 0, which would draw nothing at all.
        if id == 0 {
            drop(out);
            self.write_all(bytes);
            return;
        }
        let rewritten = out.rewriter.push(bytes, id);
        let _ = out.stdout.write_all(&rewritten);
        let _ = out.stdout.flush();
    }

    /// Scan a chunk for placeholder cells and record the first wrong one.
    ///
    /// The command's bytes are forwarded regardless: this observes, it does not
    /// filter. A cell referencing the wrong image still reaches the terminal,
    /// where it draws nothing, and the point is to explain that rather than to
    /// hide it.
    fn check_placeholders(&self, chunk: &[u8]) {
        let mut out = self.0.lock().unwrap();

        // Scan first, unconditionally. An erase is about the terminal's state,
        // not about whether a client has asked for an image yet: a clear issued
        // before the first frame still destroys whatever is drawn after it.
        // Gating this on `expected_id` silently dropped every one.
        out.scanner.push(chunk);

        // Judge cells only once we know which id they are meant to name.
        let Some(expected) = out.expected_id else {
            return;
        };
        let want = p2p::id_placeholder_rgb(expected);

        let found = std::mem::take(&mut out.scanner.found);
        let mismatched = found
            .iter()
            .find(|cell| cell.rgb != Some((want[0], want[1], want[2])));
        if let Some(cell) = mismatched {
            let described = match cell.rgb {
                Some((r, g, b)) => {
                    format!("rgb({r},{g},{b}) = image id {}", p2p::color_to_id(r, g, b))
                }
                None => "no foreground color at all".to_string(),
            };
            let message = format!(
                "placeholder cell does not name this image: cell has {described}, \
                 but the image is id {expected} (rgb({},{},{}))",
                want[0], want[1], want[2]
            );
            let mut slot = out.mismatch.lock().unwrap();
            slot.get_or_insert(message);
        }
    }

    /// Whole-screen erases seen since the previous call.
    ///
    /// A delta, not the running total. The caller acts on each one, and a total
    /// it has already acted on would be re-counted on every later frame, so the
    /// image would be re-sent forever.
    fn take_erases(&self) -> u64 {
        let mut out = self.0.lock().unwrap();
        let seen = out.scanner.full_erases;
        let fresh = seen.saturating_sub(out.acted_on_erases);
        out.acted_on_erases = seen;
        fresh
    }

    /// What the command's output stream actually carried, and forget it.
    ///
    /// Every APC the terminal was handed, so a transmit of ours can be looked
    /// for in it. Nothing else distinguishes "the graphics command never reached
    /// the terminal" from "it reached it and had no effect", and those need
    /// opposite fixes.
    fn take_apcs(&self) -> Vec<Vec<u8>> {
        let mut out = self.0.lock().unwrap();
        std::mem::take(&mut out.scanner.apcs)
    }

    /// Count our own graphics escapes, and how many are kitty commands.
    ///
    /// A kitty graphics APC begins with `G`. Separating "we emitted graphics"
    /// from "the terminal got nothing graphics-like" is the difference between a
    /// rendering problem and a plumbing one.
    fn own_apc_summary(&self) -> (usize, usize) {
        let mut out = self.0.lock().unwrap();
        let apcs = std::mem::take(&mut out.own_apcs.apcs);
        let graphics = apcs
            .iter()
            .filter(|a| a.first().is_some_and(|c| *c == b'G'))
            .count();
        (apcs.len(), graphics)
    }
}

/// The live display: two images sharing one virtual placement.
///
/// A frame cannot be re-transmitted over the image the cells currently name: a
/// transmit replaces that image's data, so the cells would resolve to an image
/// whose new pixels are still being decoded. Nor can it be edited in place,
/// because the terminal may have discarded the image without saying so.
///
/// So the frames alternate between two images. The new one is transmitted while
/// hidden, and the virtual placement is then moved to it. The cells encode only
/// the image id, so each swap needs the grid repainted with the new id; the
/// placement's rectangle, and therefore every cell's position, is unchanged.
struct Display {
    /// The image the cells currently name, if any.
    image: Option<DynImage<easytgp::Shm>>,
    /// The image the next frame goes into.
    ///
    /// Held across the swap: creating it in the transmit would put the terminal
    /// in the state the swap exists to avoid, with one image carrying both the
    /// old and the new pixels partway through.
    hidden: Option<DynImage<easytgp::Shm>>,
    /// The placement both images share, moved from one to the other on a swap.
    placement: PlacementRequest,
    /// The id the client's cells must currently encode.
    id: u32,
    /// Dimensions of the last frame, for sizing a re-sent object.
    width: u32,
    height: u32,
}

/// What the server needs to know before it starts.
///
/// Separated from argument parsing so a caller that is not a command-line
/// program (`tuigui`, which starts a server and a client together) can drive
/// the same code without manufacturing an `argv`.
#[derive(Debug, Clone)]
pub struct Config {
    /// Unix socket to listen on.
    pub socket: String,
    /// Where to publish the session file a client reads to find the socket and
    /// grid size. `None` means the environment's `TUI_SESSION`, else the shared
    /// default.
    pub session_path: Option<std::path::PathBuf>,
    /// The command to run inside the pty.
    pub command: Vec<String>,
}

impl Config {
    /// Parse `--path P`, `TUI_SOCK` and the command to wrap.
    ///
    /// Returns an error when no command was given, so the caller decides what
    /// a usage message looks like rather than the library exiting the process.
    pub fn from_args(
        args: impl Iterator<Item = String>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let mut args = args.peekable();
        let mut socket = std::env::var("TUI_SOCK").unwrap_or_else(|_| "/tmp/tui.sock".into());
        if args.peek().map(String::as_str) == Some("--path") {
            args.next();
            socket = args.next().ok_or("--path needs a value")?;
        }
        let command: Vec<String> = args.collect();
        if command.is_empty() {
            return Err("usage: tui [--path SOCK] <command> [args...]".into());
        }
        Ok(Self {
            socket,
            session_path: None,
            command,
        })
    }
}

/// Stops a running server without waiting for the wrapped command to exit.
///
/// The wrapped command is usually a shell, which outlives whatever client was
/// drawing; without this the process would sit at a prompt forever after the
/// client is done, holding the terminal.
#[derive(Clone)]
pub struct Handle {
    /// Written to by anything that wants the server to stop. The server watches
    /// the read end alongside its clients.
    stop_tx: tokio::sync::mpsc::UnboundedSender<()>,
    /// The wrapped command, so a stopping caller can also end it rather than
    /// leaving an orphan at a prompt inside a pty nobody is reading.
    pid: Option<rustix::process::Pid>,
}

impl Handle {
    /// Ask the server to stop. Idempotent, and safe to call from any thread.
    pub fn stop(&self) {
        let _ = self.stop_tx.send(());
        if let Some(pid) = self.pid {
            // Best effort: the server's own wait thread will reap it.
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
        }
    }
}

/// Run `config`'s command in a pty and paint frames sent by clients onto the
/// real terminal. Returns when the wrapped command exits.
///
/// Builds its own runtime and blocks. The server needs a multi-thread runtime
/// for its accept tasks, and a caller that already has one (`tuigui` runs the
/// client on the main thread) would only have to rebuild it.
pub fn run_blocking(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(config))
}

/// Start `config`'s command, painting frames sent by clients onto the real
/// terminal. Returns when the wrapped command exits.
pub async fn run(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    let (_handle, server) = prepare(config)?;
    serve(server).await
}

/// Start the server on its own thread and return immediately with a [`Handle`].
///
/// For callers that need the server running *while* they do something else,
/// which is the whole point of `tuigui`: the client needs the terminal, so the
/// server cannot be driven on the calling thread.
pub fn spawn(config: Config) -> Result<Handle, Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let (handle, server) = prepare(config)?;
    std::thread::Builder::new()
        .name("tui-serve".into())
        .spawn(move || {
            if let Err(e) = rt.block_on(serve(server)) {
                eprintln!("tui: {e}");
            }
        })?;
    Ok(handle)
}

/// Whether the wrapped command is a terminal multiplexer.
///
/// The distinction decides how the client draws its grid, and it cannot be
/// detected from the terminal: a muxer looks like any other program until it
/// re-renders its pane, at which point a placeholder cell's color is already
/// gone. Recognizing the command is what makes the choice before that happens.
///
/// A false negative costs a blank image, so the list is generous; a false
/// positive costs a rewrite that changes nothing, since a terminal that is not a
/// muxer passes the marker through and the pump replaces it either way.
fn muxer_in_path(command: &[String]) -> bool {
    // The program, and any interpreter's script argument: `tui sh -c 'herdr'`
    // is as much a muxer as `tui herdr`.
    const MUXERS: [&str; 6] = ["herdr", "tmux", "zellij", "screen", "byobu", "tmate"];
    command.iter().any(|arg| {
        let name = Path::new(arg)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| arg.clone());
        MUXERS.contains(&name.as_str())
            // Also matched inside a shell argument, e.g. `sh -c "exec herdr"`.
            || MUXERS.iter().any(|m| arg.split_whitespace().any(|w| w == *m))
    })
}

/// A prepared server: everything started except the accept loop.
///
/// Split from [`serve`] so a caller can take a [`Handle`] first and only then
/// hand the loop to a thread. Starting a thread that might fail before it can
/// report the failure is the alternative, and it loses the error.
pub struct Server {
    listener: StdUnixListener,
    pty: Pty,
    controller: Arc<Controller>,
    /// Wrapped command's pid, for the handle that stops it.
    child_pid: i32,
    /// Kept alive so raw mode is restored when the server ends.
    _raw: RawGuard,
    stop_rx: tokio::sync::mpsc::UnboundedReceiver<()>,
    /// The output pump, so a new client's image id can be made known to the
    /// placeholder check.
    out: Out,
    /// Set by the pump when a cell named the wrong image.
    mismatch: Arc<Mutex<Option<String>>>,
}

/// Bind the socket, start the wrapped command, and publish the session file.
///
/// Nothing here can leave a half-started server behind: every failure returns
/// before the first client could connect.
pub fn prepare(config: Config) -> Result<(Handle, Server), Box<dyn std::error::Error>> {
    let Config {
        socket: sock_path,
        session_path,
        command: cmd,
    } = config;

    if let Err(e) = rustix::termios::tcgetattr(std::io::stdin()) {
        return Err(format!("tui: stdin is not a terminal: {e}").into());
    }

    // Our terminal must be raw for the same reason the child's pty is: with the
    // line discipline on, typed keys are echoed here *and* by the child (double
    // echo) and are not delivered until a newline. The guard restores on exit.
    let raw = RawGuard::install();

    let ws = rustix::termios::tcgetwinsize(std::io::stdin()).unwrap_or(rustix::termios::Winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    });
    let (cols, rows) = (ws.ws_col as u32, ws.ws_row as u32);

    match std::fs::remove_file(&sock_path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("removing stale {sock_path}: {e}").into()),
    }
    let listener = StdUnixListener::bind(&sock_path)?;
    listener.set_nonblocking(true)?;

    let child_env = [
        ("TUI_SOCK", sock_path.clone()),
        ("TUI_COLS", cols.to_string()),
        ("TUI_ROWS", rows.to_string()),
    ];
    let mut pty = Pty::open(ws)?;

    // One reader for fd 0, always: whole APC sequences (TGP replies) go to
    // easytgp's router, everything else is a keystroke for the command. Two
    // readers of the same stream would split bytes nondeterministically.
    let (reply_r, reply_w) = std::io::pipe()?;
    {
        let mut master = pty.master.try_clone()?;
        std::thread::Builder::new()
            .name("tui-stdin".into())
            .spawn(move || input_loop(&mut master, reply_w))?;
    }

    // The command's output goes to the real terminal through the same lock our
    // escape batches take. The same pump watches for placeholder cells that name
    // the wrong image; the accept loop reads that out and tells the client.
    let mismatch: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let out = Out::new(mismatch.clone());
    let out_for_pump = out.clone();
    {
        let mut master = pty.master.try_clone()?;
        std::thread::Builder::new()
            .name("tui-out".into())
            .spawn(move || out_for_pump.pump(&mut master))?;
    }

    // easytgp owns the wire: it writes escapes and its router reads the replies
    // we funneled into `reply_r`.
    //
    // Its escapes must go through `Out`, not its own stdout, or the wrapped
    // command and the escape writer race on the same fd: an escape can land
    // inside a line the command is printing. Done before the first transmit so
    // the sink is installed for every escape.
    if !easytgp::set_output_sink(Arc::new(Mutex::new(out.clone()))) {
        eprintln!("tui: an output sink was already installed; escapes may interleave");
    }
    let controller = Controller::init(reply_r);

    eprintln!("tui: socket {sock_path}  grid {cols}x{rows}");

    // Publish connection details to a well-known file. Environment cannot carry
    // them into panes owned by a multiplexer's own server, so the client reads
    // this instead (see `session.rs`).
    let session = session::Session {
        socket: sock_path.clone(),
        image_id: 0,
        cols,
        rows,
        // A muxer re-renders the pane's bytes, so a placeholder cell's color
        // never reaches the terminal and the cell must be drawn as a marker for
        // the pump to rewrite. Detected from the command, which is what decides
        // whether there is anything in the path at all.
        grid: if muxer_in_path(&cmd) {
            session::GridMode::Marker
        } else {
            session::GridMode::Placeholder
        },
    };
    let session_path = session_path.unwrap_or_else(session::path);
    if let Err(e) = session.write(&session_path) {
        eprintln!("tui: could not write {}: {e}", session_path.display());
    } else {
        eprintln!("tui: session {}", session_path.display());
    }

    pty.exec(&cmd, &child_env)?;
    let child_pid = pty.child.as_ref().expect("exec just set it").id() as i32;

    let (stop_tx, stop_rx) = tokio::sync::mpsc::unbounded_channel();
    let handle = Handle {
        stop_tx,
        pid: rustix::process::Pid::from_raw(child_pid),
    };
    let server = Server {
        listener,
        pty,
        controller,
        child_pid,
        _raw: raw,
        stop_rx,
        out,
        mismatch,
    };
    Ok((handle, server))
}

/// The accept loop: hand each client to `drive` until the wrapped command exits
/// or a [`Handle`] stops us.
async fn serve(server: Server) -> Result<(), Box<dyn std::error::Error>> {
    let Server {
        listener,
        mut pty,
        controller,
        child_pid,
        _raw,
        mut stop_rx,
        out,
        mismatch,
    } = server;

    let mut display: Option<Display> = None;

    // The command owns the session: when it exits, so do we. Its exit has to be
    // watched from another thread, because a connected client parks the loop
    // inside `drive` indefinitely and polling between connections would never
    // observe it.
    let (exit_tx, mut exit_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    std::thread::Builder::new()
        .name("tui-wait".into())
        .spawn(move || {
            wait_for_child(child_pid);
            let _ = exit_tx.send(());
        })?;

    loop {
        if exit_rx.try_recv().is_ok() {
            eprintln!("tui: command exited; shutting down");
            break;
        }
        if stop_rx.try_recv().is_ok() {
            eprintln!("tui: stop requested; shutting down");
            break;
        }
        if let Ok((s, _)) = listener.accept() {
            s.set_nonblocking(true)?;
            let (r, w) = tokio::io::split(tokio::net::UnixStream::from_std(s)?);
            let mut r = tokio::io::BufReader::new(r);
            let mut w = Some(w);
            tokio::select! {
                _ = drive(&controller, &mut display, &out, &mismatch, &mut r, &mut w) => {}
                _ = exit_rx.recv() => {
                    eprintln!("tui: command exited; shutting down");
                    break;
                }
                _ = stop_rx.recv() => {
                    eprintln!("tui: stop requested; shutting down");
                    break;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    if let Some(child) = pty.child.as_mut() {
        let _ = child.kill();
    }
    Ok(())
}

/// Create the two images the client will draw into, and the placement they
/// share.
///
/// `width` x `height` are the *frame's* pixel size, not the grid's: the base
/// image a terminal holds has a fixed size, and a later frame may not exceed it.
/// So the first frame's dimensions become both images', and the returned id is
/// what the client encodes into its placeholder cells.
async fn reserve(
    c: &Controller,
    cols: u32,
    rows: u32,
    width: u32,
    height: u32,
) -> Result<Display, Box<dyn std::error::Error>> {
    let (w, h) = (width.max(1), height.max(1));
    let placement = PlacementRequest {
        cols: Some(cols),
        rows: Some(rows),
        // The prototype the placeholder cells resolve against. It has no screen
        // position of its own, so nothing is drawn until cells reference it.
        virtual_placement: true,
        // Named rather than allocated: a swap re-declares the placement on the
        // other image, and that is only the *same* placement if the id matches.
        placement_id: Some(PLACEMENT_ID),
        ..Default::default()
    };

    let mut image = transmit_blank(c, w, h).await?;
    image.place(placement.clone());
    // Wait for the transmit and the placement to settle before returning the id.
    image.sync().await?;
    let id = image.id();

    // The other half of the pair, sent before the client draws anything, so the
    // first real frame has somewhere to go that the cells do not already name.
    let hidden = transmit_blank(c, w, h).await?;

    Ok(Display {
        image: Some(image),
        hidden: Some(hidden),
        placement,
        id,
        width: w,
        height: h,
    })
}

/// A blank image of `width` x `height`, transmitted under a fresh id.
///
/// Sized rather than 1x1: a terminal will not accept a frame larger than the
/// image it edits into, so the buffer has to be created at the frame size.
async fn transmit_blank(
    c: &Controller,
    width: u32,
    height: u32,
) -> Result<DynImage<easytgp::Shm>, Box<dyn std::error::Error>> {
    let blank = image::RgbImage::from_pixel(width, height, image::Rgb([0, 0, 0]));
    Ok(c.new_shm_rgb(blank)?.transmit().await?)
}

/// Turn a wire frame into packed RGB24 for the shared memory object.
fn decode_rgb(
    codec: p2p::Codec,
    width: u32,
    height: u32,
    payload: Vec<u8>,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    match codec {
        p2p::Codec::Rgb24 => {
            let want = width as usize * height as usize * 3;
            if payload.len() != want {
                return Err(format!("rgb24 payload {} != {want}", payload.len()).into());
            }
            Ok(payload)
        }
        p2p::Codec::Jpeg => {
            let img = image::load_from_memory_with_format(&payload, image::ImageFormat::Jpeg)?;
            Ok(img.to_rgb8().into_raw())
        }
    }
}

/// One client's frames, until its stream ends.
async fn drive(
    c: &Controller,
    display: &mut Option<Display>,
    out: &Out,
    mismatch: &Arc<Mutex<Option<String>>>,
    r: &mut (impl tokio::io::AsyncRead + Unpin),
    w: &mut Option<impl tokio::io::AsyncWrite + Unpin>,
) -> Result<(), Box<dyn std::error::Error>> {
    let res = drive_inner(c, display, out, mismatch, r, w).await;
    if let Err(e) = &res {
        // Tell the client before anything else: it is the side that asked for
        // the work that failed, and it is the side that should give up. The
        // server stays up for the next client.
        let message = e.to_string();
        eprintln!("tui: session error: {message}");
        if let Some(w) = w.as_mut() {
            let _ = p2p::Msg::Error { message }.write_async(w).await;
        }
    }
    res
}

async fn drive_inner(
    c: &Controller,
    display: &mut Option<Display>,
    out: &Out,
    mismatch: &Arc<Mutex<Option<String>>>,
    r: &mut (impl tokio::io::AsyncRead + Unpin),
    w: &mut Option<impl tokio::io::AsyncWrite + Unpin>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut stats = p2p::ServerStats::default();
    loop {
        let msg = match p2p::Msg::read_async(r).await? {
            Some(m) => m,
            None => return Ok(()),
        };
        match msg {
            p2p::Msg::Hello {
                cols,
                rows,
                width,
                height,
            } => {
                // The client owns the pane and knows the frame size, so it
                // decides both. Create the image now and tell it the id it must
                // encode into the placeholder cells it is about to draw.
                let d = reserve(c, cols, rows, width, height).await?;
                eprintln!(
                    "tui: image {} reserved for {width}x{height}, grid {cols}x{rows}",
                    d.id
                );
                if let Some(w) = w.as_mut() {
                    p2p::Msg::Image { id: d.id }.write_async(w).await?;
                }
                // From here the client's cells must name this image. The pump
                // will check the bytes it forwards against it.
                out.expect_id(d.id);
                *mismatch.lock().unwrap() = None;
                *display = Some(d);
            }
            p2p::Msg::Image { .. } | p2p::Msg::Error { .. } => {}
            p2p::Msg::Stats(_) => {}
            p2p::Msg::Ping { nonce } => {
                if let Some(w) = w.as_mut() {
                    p2p::Msg::Pong { nonce }.write_async(w).await?;
                }
            }
            p2p::Msg::Pong { .. } => {}
            p2p::Msg::Frame {
                width,
                height,
                codec,
                payload,
            } => {
                // A grid whose cells name the wrong image draws nothing, and
                // nothing about that is visible from either side's counters. The
                // pump saw the actual bytes, so if it found a mismatch, say so:
                // this is the difference between "the canvas is blank" and a
                // specific wrong byte.
                if let Some(reason) = mismatch.lock().unwrap().take() {
                    let message = format!(
                        "{reason}. The image exists and frames are arriving, so the \
                         cells are what is wrong; the color must encode the id the \
                         server assigned."
                    );
                    eprintln!("tui: {message}");
                    if let Some(w) = w.as_mut() {
                        p2p::Msg::Error { message }.write_async(w).await?;
                    }
                }

                let Some(d) = display.as_mut() else {
                    // A frame cannot be shown without the image its `Hello`
                    // should have created; dropping it beats guessing an id the
                    // client's placeholders do not reference.
                    continue;
                };
                // Time the two halves of showing a frame: turning the wire bytes
                // back into pixels, and getting the terminal to accept them.
                // They fail and slow down for different reasons, and a single
                // number for "the server is slow" would not say which.
                let payload_len = payload.len() as u64;
                let t = std::time::Instant::now();
                // A frame that cannot be shown is counted and skipped, not
                // fatal. Frames are independent, so one bad or oversized frame
                // says nothing about the next, and killing the session would
                // punish every later frame for it.
                let img = match decode_rgb(codec, width, height, payload)
                    .and_then(|rgb| {
                        image::RgbImage::from_raw(width, height, rgb)
                            .ok_or_else(|| "frame dims do not match pixel count".into())
                    }) {
                    Ok(img) => img,
                    Err(e) => {
                        // Report the miss immediately: `should_report` is
                        // frame-counted, and a dropped frame never reaches the
                        // count, so waiting for the next boundary would leave
                        // the client's `behind` growing with nothing to explain
                        // it.
                        stats.errors += 1;
                        eprintln!("tui: dropping a frame: {e}");
                        if let Some(w) = w.as_mut() {
                            p2p::Msg::Stats(stats).write_async(w).await?;
                        }
                        continue;
                    }
                };
                let decode = t.elapsed();

                let t = std::time::Instant::now();
                let before = d.id;
                let presented = match present(c, d, img).await {
                    Ok(id) => id,
                    Err(e) => {
                        stats.errors += 1;
                        eprintln!("tui: dropping a frame: {e}");
                        if let Some(w) = w.as_mut() {
                            p2p::Msg::Stats(stats).write_async(w).await?;
                        }
                        continue;
                    }
                };
                let sync = t.elapsed();

                // A swap changes which image the cells name, so the client has
                // to repaint the grid with the new id: the id is part of each
                // cell's color, and a cell naming the previous image draws
                // nothing. Sent rather than inferred, because there is no signal
                // on the client's side to infer it from.
                if presented != before {
                    if let Some(w) = w.as_mut() {
                        p2p::Msg::Image { id: presented }.write_async(w).await?;
                    }
                    out.expect_id(presented);
                }

                stats.record_frame(width, height, payload_len, micros(decode), micros(sync));
                // The image is re-sent unconditionally by `present`, so an erase
                // needs no separate handling. Counting them is still worthwhile:
                // a rising figure is the only sign the terminal is discarding
                // the image constantly, which is what the re-sends cost.
                let erases = out.take_erases();
                if erases > 0 {
                    for _ in 0..erases {
                        stats.record_reassert();
                    }
                }
                // Report whether our own graphics escapes were emitted at all.
                // That is the one fact nothing else in this program can observe,
                // and without it "the image does not appear" cannot be told
                // apart from "the image was never sent".
                let (total, graphics) = out.own_apc_summary();
                if total > 0 && stats.frames % p2p::STATS_EVERY == 0 {
                    eprintln!(
                        "tui: {total} escape(s) written, {graphics} of them kitty graphics"
                    );
                }
                if stats.should_report() {
                    if let Some(w) = w.as_mut() {
                        p2p::Msg::Stats(stats).write_async(w).await?;
                    }
                }
            }
        }
    }
}
/// Show `img` by filling the hidden image and moving the placement onto it.
///
/// The frame cannot go into the image the cells currently name. Its pixels are
/// replaced the instant the terminal parses the transmit, while decoding and the
/// command's arrival are still in progress, so the cells would resolve to
/// half-written data. Neither can it be edited in place, because the terminal
/// may have discarded the image on a whole-screen erase without telling anyone.
///
/// So the frames alternate. The hidden image is filled and synced first, so it
/// is complete before anything points at it, and only then is the placement
/// moved: one command, and the switch is atomic. The id the cells encode changes
/// with it, which is why the caller has to tell the client the new one.
///
/// Returns the id the client's cells must now name.
async fn present(
    c: &Controller,
    display: &mut Display,
    img: image::RgbImage,
) -> Result<u32, Box<dyn std::error::Error>> {
    let (w, h) = (img.width(), img.height());
    let request = c.new_shm_rgb(img)?;

    // Fill the buffer nothing points at yet. `transmit_as` names an image that
    // already exists, so the reply cannot introduce a third id; asserted rather
    // than assumed, because the swap depends on knowing which image the cells
    // will name next.
    let hidden_id = display.hidden.as_ref().map(DynImage::id).ok_or("no hidden image")?;
    let mut next = request.transmit_as(hidden_id).await?;
    if next.id() != hidden_id {
        return Err(format!(
            "terminal reassigned the image id: sent {hidden_id}, got {}",
            next.id()
        )
        .into());
    }

    // Move the placement onto the freshly filled image. `a=p` on an image that
    // has no placement creates it, and the id is the one both images were given,
    // so this is the same placement moving rather than a second one appearing.
    next.place(display.placement.clone());
    next.sync().await?;

    // The terminal re-scans placeholder lines when a virtual placement changes,
    // so the cells resolve against the new image from here on. The client has to
    // repaint, because the id is part of the cell's color.
    let id = next.id();
    display.hidden = display.image.take();
    display.image = Some(next);
    display.id = id;
    display.width = w;
    display.height = h;
    Ok(id)
}

/// Block until the wrapped command exits.
///
/// Runs on its own thread. `waitpid` without `WNOHANG` parks until the child is
/// reaped, which is exactly the event we want; polling would miss the exit while
/// the main loop is parked inside a client read. The status itself is not sent:
/// all the caller needs is that it happened, and `WaitStatus` cannot be built by
/// a caller if `waitpid` reports something unexpected.
fn wait_for_child(pid: i32) {
    if let Some(pid) = rustix::process::Pid::from_raw(pid) {
        let _ = rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::empty());
    }
}

/// A duration as whole microseconds, saturating rather than wrapping.
///
/// Saturating because a frame that took longer than u32 microseconds (about 71
/// minutes) is a stall worth reporting as "very long", not as a small number
/// from a wrapped counter.
fn micros(d: Duration) -> u32 {
    d.as_micros().min(u32::MAX as u128) as u32
}

/// Split fd 0: whole APC sequences to easytgp's reply router, the rest to the
/// command. APC = `ESC _ ... ESC \`; a partial sequence stays buffered.
fn input_loop(master: &mut File, mut replies: impl io::Write) {
    let mut inp = std::io::stdin();
    let mut buf = [0u8; 8192];
    let mut pending: Vec<u8> = Vec::new();
    loop {
        let n = match inp.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        pending.extend_from_slice(&buf[..n]);
        let (apcs, keys) = split_apcs(&mut pending);
        if !apcs.is_empty() {
            let _ = replies.write_all(&apcs);
            let _ = replies.flush();
        }
        if !keys.is_empty() {
            let _ = master.write_all(&keys);
            let _ = master.flush();
        }
    }
}

fn split_apcs(pending: &mut Vec<u8>) -> (Vec<u8>, Vec<u8>) {
    let mut apcs = Vec::new();
    let mut rest = Vec::new();
    let mut i = 0;
    while i < pending.len() {
        if pending[i] == 0x1b && i + 1 < pending.len() && pending[i + 1] == b'_' {
            match find_st(pending, i + 2) {
                Some(end) => {
                    apcs.extend_from_slice(&pending[i..end + 2]);
                    i = end + 2;
                }
                None => break,
            }
        } else if pending[i] == 0x1b && i + 1 == pending.len() {
            // Possible APC split across reads; wait for more.
            break;
        } else {
            rest.push(pending[i]);
            i += 1;
        }
    }
    pending.drain(..i);
    (apcs, rest)
}

/// Index of the `ESC` of the terminating `ESC \`, if present.
fn find_st(buf: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 1 < buf.len() {
        if buf[i] == 0x1b && buf[i + 1] == b'\\' {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// A pty we own, with the wrapped command attached to its slave.
struct Pty {
    master: File,
    /// The pty slave, opened but not yet handed to anyone. Kept so the command
    /// can be started later, after the image id is known.
    slave: File,
    child: Option<Child>,
}

impl Pty {
    /// Open a pty of the given size, before any command exists.
    fn open(ws: rustix::termios::Winsize) -> io::Result<Self> {
        let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).map_err(io::Error::from)?;
        grantpt(&master).map_err(io::Error::from)?;
        unlockpt(&master).map_err(io::Error::from)?;
        let name = ptsname(&master, Vec::new()).map_err(io::Error::from)?;
        let slave = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(Path::new(OsStr::from_bytes(name.as_bytes())))?;
        rustix::termios::tcsetwinsize(&master, ws).map_err(io::Error::from)?;
        Ok(Self {
            master: File::from(master),
            slave,
            child: None,
        })
    }

    /// Start `cmd` on this pty's slave.
    fn exec(
        &mut self,
        cmd: &[String],
        env: &[(&str, String)],
    ) -> io::Result<()> {
        let mut c = Command::new(&cmd[0]);
        c.args(&cmd[1..]);
        for (k, v) in env {
            c.env(k, v);
        }
        c.stdin(Stdio::from(self.slave.try_clone()?));
        c.stdout(Stdio::from(self.slave.try_clone()?));
        c.stderr(Stdio::from(self.slave.try_clone()?));
        unsafe {
            c.pre_exec(|| {
                // New session, then adopt the pts as the controlling terminal so
                // job control and job-control signals behave.
                rustix::process::setsid().map_err(io::Error::from)?;
                rustix::process::ioctl_tiocsctty(std::io::stdin()).map_err(io::Error::from)?;
                Ok(())
            });
        }
        self.child = Some(c.spawn()?);
        Ok(())
    }
}

/// Puts the real terminal in raw mode for the process lifetime and restores the
/// original settings on drop.
struct RawGuard {
    original: rustix::termios::Termios,
}

impl RawGuard {
    fn install() -> Self {
        let fd = std::io::stdin();
        let original = rustix::termios::tcgetattr(&fd).expect("checked above");
        let mut raw = original.clone();
        raw.make_raw();
        let _ = rustix::termios::tcsetattr(&fd, rustix::termios::OptionalActions::Now, &raw);
        Self { original }
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        let _ = rustix::termios::tcsetattr(
            std::io::stdin(),
            rustix::termios::OptionalActions::Now,
            &self.original,
        );
    }
}

