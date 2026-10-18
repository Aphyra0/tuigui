//! `gui` — the client of the remote-streaming experiment.
//!
//! Usage (inside a pane spawned by `tui`):
//!   gui play path/to/video.mp4          # decode + send frames to `tui`
//!   gui placeholders                    # print the U+10EEEE grid for a pane
//!
//! The placeholder grid is *text*: it goes through the pty, and through any
//! multiplexer running inside `tui`, as ordinary glyphs. The frames do not; they
//! travel out of band and `tui` paints them onto the real terminal. That split
//! is the whole point of the experiment: a multiplexer never has to understand
//! graphics for the image to move correctly with its text.
//!
//! The image id is not known in advance. This side owns the frame size (it is
//! the one decoding), so it tells the server that size in `Hello`, the server
//! creates the image, and the id comes back in `Image`. Only then can the grid
//! be drawn, because the id lives in each cell's foreground color.

use std::io::{self, Write as _};
use std::os::fd::AsFd as _;
use std::path::Path;
use std::time::Duration;

use tuigui_wire::{p2p, session};

mod widget;

/// ffmpeg pulls and decodes frames; we ask it for packed RGB24 on stdout.
type Frames = Box<dyn Iterator<Item = ffmpeg_sidecar::event::OutputVideoFrame> + Send>;

/// What the client was asked to do.
#[derive(Debug, Clone)]
pub enum Command {
    /// Draw the placeholder grid and hold it, without sending any frames.
    Placeholders,
    /// Decode `video` and stream it to the server.
    Play(String),
}

/// Parse `placeholders` / `play <video>`.
pub fn command_from_args(
    args: impl Iterator<Item = String>,
) -> Result<Command, Box<dyn std::error::Error>> {
    let mut args = args;
    match args.next().unwrap_or_default().as_str() {
        "placeholders" | "ph" => Ok(Command::Placeholders),
        "play" => {
            let path = args.next().ok_or("usage: gui play <video>")?;
            Ok(Command::Play(path))
        }
        _ => Err("usage: gui play <video> | gui placeholders".into()),
    }
}

/// Run `command` against the session the server published.
pub fn run(command: Command) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        Command::Placeholders => placeholders(),
        Command::Play(path) => play(&path),
    }
}

/// Report a step to stderr and flush, so it is visible even though a pty
/// normally line-buffers stderr. Every command narrates through this.
fn say(msg: impl std::fmt::Display) {
    eprintln!("gui: {msg}");
    let _ = io::stderr().flush();
}

/// Whether the cleanse mode is on. See [`wait_for_clean_screen`].
fn cleanse() -> bool {
    std::env::var("TUI_CLEANSE").is_ok_and(|v| v != "0")
}

/// Wait for the server's escape writer to drain before we touch the screen.
///
/// A TGP transmit outlives the code that started it: `sync` can return before
/// the terminal's ack for one command while the next is still queued. When the
/// client immediately paints over that in-flight sequence, the terminal is left
/// with a half-applied image, and its graphics state is wrong until something
/// clears it. Draining first is cheap and removes the whole class of glitch.
///
/// Waits on the writer the server owns rather than a fixed sleep, so it returns
/// as soon as the wire is quiet: a sleep long enough to be safe would be longer
/// than this on every run.
fn wait_for_clean_screen() {
    if !cleanse() {
        return;
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let mut waited = Duration::ZERO;
    while easytgp::writer_in_flight() > 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(2));
        waited += Duration::from_millis(2);
    }
    say(format!(
        "cleanse: writer drained after {}ms ({} escape(s) outstanding)",
        waited.as_millis(),
        easytgp::writer_in_flight()
    ));

    // The writer being idle can still leave the *terminal* mid-parse if the
    // last ack has not come back, so never return with nothing at all spent.
    std::thread::sleep(Duration::from_millis(50));
}

/// The preamble every command runs before touching the screen.
///
/// It resolves the session, checks the terminal is usable, and pings the server.
/// Each step reports and the whole thing exits on failure: a misconfigured pane
/// should say why, not leave a half-drawn screen behind.
fn preflight() -> Result<(session::Session, Sink), Box<dyn std::error::Error>> {
    say("reading session");
    let mut s = session()?;
    say(format!(
        "grid {}x{} socket {}",
        s.cols,
        s.rows,
        if s.socket.is_empty() { "-" } else { &s.socket },
    ));
    if std::env::var("TERM").is_err() {
        say("warning: TERM is unset");
    }

    // Cleansing exists precisely for the case where stdout is *not* the
    // terminal: it lets the client be useful before it owns the screen, which
    // is when a redirect is in effect. Without it, stdout being a pipe means
    // there is nothing to paint on and no reason to continue.
    if !rustix::termios::isatty(std::io::stdout()) {
        if !cleanse() {
            return Err(
                "stdout is not a terminal (set TUI_CLEANSE=1 to drain and continue anyway)"
                    .into(),
            );
        }
        say("warning: stdout is not a terminal; cleaning with TUI_CLEANSE");
    }

    // The grid is the *client's* pane, not whatever size the server saw at
    // startup. Pane splits, resizes, and a server started under a different
    // geometry all make the session's numbers stale, so measure here.
    //
    // Measured on the pty, not stdout: with a redirect, stdout has no size at
    // all, and the pane's geometry is what the session file is describing.
    // Bound to locals first: `as_fd()` borrows a temporary `Stdout`/`Stdin`
    // that would otherwise be dropped before the call.
    let (stdout, stdin) = (std::io::stdout(), std::io::stdin());
    let size_from = if rustix::termios::isatty(&stdout) {
        stdout.as_fd()
    } else {
        stdin.as_fd()
    };
    match rustix::termios::tcgetwinsize(size_from) {
        Ok(ws) if ws.ws_col > 0 && ws.ws_row > 0 => {
            if (ws.ws_col as u32, ws.ws_row as u32) != (s.cols, s.rows) {
                say(format!(
                    "using this pane's size {}x{} (session said {}x{})",
                    ws.ws_col, ws.ws_row, s.cols, s.rows
                ));
            }
            s.cols = ws.ws_col as u32;
            s.rows = ws.ws_row as u32;
        }
        _ if s.cols == 0 || s.rows == 0 => {
            return Err(format!(
                "cannot determine grid size: this pane reports 0x0 and the session says {}x{}",
                s.cols, s.rows
            )
            .into());
        }
        _ => say(format!("using the session's size {}x{}", s.cols, s.rows)),
    }

    say("connecting");
    let mut link = connect(&s)?;
    say("connected, pinging");
    let nonce: u32 = 0x7057_7567; // "pWug"
    p2p::Msg::Ping { nonce }.write(&mut link)?;
    match link.read_pong(nonce) {
        Ok(()) => say("server replied to ping"),
        Err(e) => return Err(format!("ping failed: {e}").into()),
    }
    wait_for_clean_screen();
    say("all checks ok");
    Ok((s, link))
}

/// Let the reported lines be read before the caller takes over the screen.
fn settle() {
    std::thread::sleep(Duration::from_secs(1));
}

/// Ask the server to create the image, and return the id it assigned.
///
/// This must happen before anything is drawn: the id is what the placeholder
/// cells encode, and the server cannot choose it until it knows the frame size.
fn hello(
    s: &session::Session,
    sink: &mut Sink,
    width: u32,
    height: u32,
) -> Result<u32, Box<dyn std::error::Error>> {
    say(format!("asking for an image at {width}x{height}"));
    p2p::Msg::Hello {
        cols: s.cols,
        rows: s.rows,
        width,
        height,
    }
    .write(sink)?;
    match sink.read_image() {
        Ok(id) => {
            say(format!("server assigned image {id}"));
            Ok(id)
        }
        Err(e) => Err(format!("server did not assign an image: {e}").into()),
    }
}

/// Draw the `U+10EEEE` grid for `image_id`, then hold the full-screen grid.
fn placeholders() -> Result<(), Box<dyn std::error::Error>> {
    let (s, mut link) = preflight()?;
    // No frames are sent by this command, so the image is created at the
    // smallest size. `play` is the one that needs a real pixel size.
    let id = hello(&s, &mut link, 1, 1)?;
    settle();

    // Raw mode without the alternate screen: an alt screen has its own graphics
    // store, which can be torn down underneath the image.
    let mut terminal = ratatui::try_init_with_options(ratatui::TerminalOptions {
        viewport: ratatui::Viewport::Fullscreen,
    })?;
    // Nothing feeds this grid, so its counters stay at zero; the overlay still
    // reports the server's side, which is what shows frames arriving from other
    // clients.
    let mut state = widget::GridState {
        image_id: id,
        slot: 0,
        repaint_grid: false,
        stats: widget::Stats::default(),
        server: None,
    };
    let res = draw(&mut terminal, &mut state);
    ratatui::restore();
    res?;
    say("grid held; frames appear as the server receives them");
    // Hold the screen so the cells stay put; the image lives as long as they do.
    hold_until(&mut || link.server_gone());
    Ok(())
}

/// Draw one frame of the grid and overlay, timing the redraw.
fn draw(
    terminal: &mut ratatui::DefaultTerminal,
    state: &mut widget::GridState,
) -> Result<(), Box<dyn std::error::Error>> {
    let t = std::time::Instant::now();
    terminal.draw(|f| f.render_stateful_widget(widget::GridWidget, f.area(), state))?;
    state.stats.draw_total += t.elapsed();
    Ok(())
}

/// Decode `video` frame by frame and stream each to the server.
fn play(video: &str) -> Result<(), Box<dyn std::error::Error>> {
    let (s, mut sink) = preflight()?;

    // Peek the first frame so the image is created at the right pixel size.
    // A terminal will not accept a frame larger than the base image, so this
    // has to happen before the server transmits.
    say(format!("decoding {video}"));
    let mut frames = spawn_ffmpeg(Path::new(video));
    let first = frames.next().ok_or("no frames in that file")?;
    let (fw, fh) = (first.width, first.height);
    let id = hello(&s, &mut sink, fw, fh)?;

    settle();

    // Draw the grid: a frame needs cells to resolve against, and the streamed
    // image has nowhere to appear without them.
    let mut terminal = ratatui::try_init_with_options(ratatui::TerminalOptions {
        viewport: ratatui::Viewport::Fullscreen,
    })?;
    let mut state = widget::GridState {
        image_id: id,
        slot: 0,
        repaint_grid: false,
        stats: widget::Stats::default(),
        server: None,
    };
    draw(&mut terminal, &mut state)?;
    say(format!("{}x{} placeholder cells written", s.cols, s.rows));

    // JPEG per frame: intra-only, so no decoder state crosses frames and the
    // server can drop any frame without corrupting the next.
    let codec = match std::env::var("TUI_CODEC").as_deref() {
        Ok("rgb") => p2p::Codec::Rgb24,
        _ => p2p::Codec::Jpeg,
    };
    say(format!("streaming, codec {codec:?}"));

    let mut frame = Some(first);
    loop {
        let f = match frame.take() {
            Some(f) => f,
            None => match frames.next() {
                Some(f) => f,
                None => break,
            },
        };
        if f.width != fw || f.height != fh {
            // The base image has a fixed size, so a differently-sized frame
            // cannot be shown. Skipping it beats corrupting the stream.
            say(format!(
                "skipping a {}x{} frame (image is {fw}x{fh})",
                f.width, f.height
            ));
            continue;
        }

        let t = std::time::Instant::now();
        let payload = match codec {
            p2p::Codec::Rgb24 => f.data,
            // Encoded straight from ffmpeg's buffer: no image::RgbImage
            // round-trip, which used to copy the whole frame before the
            // encoder even started.
            p2p::Codec::Jpeg => encode_jpeg(f.width, f.height, &f.data)?,
        };
        let encode = t.elapsed();

        let bytes = payload.len() as u64;
        let t = std::time::Instant::now();
        p2p::Msg::Frame {
            width: f.width,
            height: f.height,
            codec,
            payload,
        }
        .write(&mut sink)?;
        let send = t.elapsed();

        // Read whatever the server has said since the last frame: its stats
        // reports, and any rejection. Both arrive on the reply stream, so this
        // single read is what keeps the overlay live.
        let t = std::time::Instant::now();
        drain_replies(&mut sink, &mut state)?;
        state.stats.echo_total += t.elapsed();

        state.stats.record_send(bytes, encode, send);

        // Ask for the other half of the pair, and repaint so the pane sees the
        // change: the server has this frame, and the next one belongs in the
        // image the cells do not name. The client never learns which image that
        // is; it only says that the frame moved on.
        state.slot = 1 - state.slot;
        state.repaint_grid = true;

        // Repaint *before* the next frame is sent, not after. A swap means the
        // server has finished one image and will write the next frame into the
        // other, so the cells have to be pointing at that other image by then.
        // Drawing after the send would let a frame be written into an image the
        // screen is still showing.
        if state.repaint_grid {
            state.repaint_grid = false;
            draw(&mut terminal, &mut state)?;
        } else if state.stats.sent_frames % p2p::STATS_EVERY == 0 {
            // The overlay is otherwise the only thing that changes between frames,
            // so it is redrawn on a counter boundary.
            draw(&mut terminal, &mut state)?;
        }
    }
    say(format!("done, {} frames", state.stats.sent_frames));
    // One last draw so the final numbers are on screen, not the ones from the
    // frame before the stream ended.
    draw(&mut terminal, &mut state)?;
    // Keep the grid: the image lives only as long as the cells that reference
    // it, so restoring the terminal here would blank it the moment playback
    // ends. Hold instead, like `placeholders` does.
    say("holding the grid; q or Ctrl-C to exit");
    hold_until(&mut || sink.server_gone());
    Ok(())
}

/// Hold the grid until the server ends the session.
///
/// The client never reads the keyboard, and must not: its stdin is the pty when it
/// runs inside the server's pty (`tuigui`), so a read there steals every keystroke
/// and mouse report before the server's input loop can forward them on. An earlier
/// version read stdin here to notice a `q`, and that is exactly what broke keys
/// and mouse.
///
/// The connection is the signal instead, and it is the right one: the server owns
/// the terminal and the input, so when it stops there is nothing left to draw for.
fn hold_until(server_gone: &mut impl FnMut() -> bool) {
    loop {
        if server_gone() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Put stdin in non-blocking mode, so reading it never parks the hold loop.
/// Apply every message the server has waiting, without blocking for more.
///
/// Non-blocking by contract: the caller is in a send loop, and a server with
/// nothing to say must not hold a frame back.
fn drain_replies(
    sink: &mut Sink,
    state: &mut widget::GridState,
) -> Result<(), Box<dyn std::error::Error>> {
    while let Some(msg) = sink.try_read_msg()? {
        match msg {
            p2p::Msg::Stats(server) => state.server = Some(server),
            // The server alternates between two images so a frame never
            // overwrites pixels the cells are already resolving against, which
            // means the id the grid must encode changes on every frame. The
            // caller repaints on the flag.
            p2p::Msg::Image { id } => {
                if id != state.image_id {
                    state.image_id = id;
                }
            }
            // Surfaced immediately: pushing more frames at a server that has
            // already refused one only deepens the failure.
            p2p::Msg::Error { message } => {
                return Err(format!("server rejected the stream: {message}").into())
            }
            other => say(format!("ignoring {other:?}")),
        }
    }
    Ok(())
}

/// Encode `pixels` (packed RGB24) as one baseline JPEG.
///
/// Uses libjpeg-turbo's SIMD kernels rather than a scalar encoder: at full-pane
/// resolution the encode was the largest single item in the frame budget, so
/// this is the difference between streaming smoothly and not.
///
/// 4:2:0 subsampling (`Subsamp::Sub2x2`) because chroma is subsampled by eye
/// anyway over a video signal, it roughly halves the payload, and the server
/// re-expands it in one pass. A quality just below lossless keeps the block
/// artifacts off text in the surrounding terminal, which is visible when this is
/// wrong.
fn encode_jpeg(
    width: u32,
    height: u32,
    pixels: &[u8],
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    use turbojpeg::{Image, PixelFormat, Subsamp};

    let want = width as usize * height as usize * 3;
    if pixels.len() != want {
        return Err(format!("rgb24 payload {} != {want}", pixels.len()).into());
    }

    let mut compressor = turbojpeg::Compressor::new()?;
    compressor.set_quality(JPEG_QUALITY)?;
    compressor.set_subsamp(Subsamp::Sub2x2)?;

    let image = Image {
        pixels,
        width: width as usize,
        height: height as usize,
        pitch: width as usize * 3,
        format: PixelFormat::RGB,
    };
    Ok(compressor.compress_to_vec(image)?)
}

/// Encoder quality. High enough that terminal text under the image stays
/// legible, low enough to keep the payload well under the raw frame.
const JPEG_QUALITY: i32 = 80;

fn spawn_ffmpeg(path: &Path) -> Frames {
    use ffmpeg_sidecar::command::FfmpegCommand;

    // `-nostdin` is required, not tidiness. The sidecar pipes ffmpeg's stdin
    // (`command.rs`: `inner.stdin(Stdio::piped())`) and never writes to it, so
    // ffmpeg sees an immediate EOF; without `-nostdin` it treats that as "the
    // input is over" and sits polling a closed stdin forever instead of decoding
    // the file it was given. That is invisible when the client has no stdin of
    // its own, and fatal when it runs inside a pty, where there is one.
    let mut cmd = FfmpegCommand::new();
    cmd.args(["-nostdin"]).input(path.as_os_str());
    let frames = cmd
        .rawvideo()
        .spawn()
        .expect("spawn ffmpeg")
        .iter()
        .expect("attach ffmpeg iterator")
        .filter_frames();
    Box::new(frames)
}

/// Connection details, from the session file `tui` writes.
///
/// Environment is deliberately not consulted: a pane owned by a multiplexer's
/// own server does not inherit the client's env, so a file at a well-known path
/// is the only channel that survives that boundary.
fn session() -> Result<session::Session, Box<dyn std::error::Error>> {
    let path = session::path();
    session::Session::read(&path).map_err(|e| {
        format!(
            "no session at {}: {e}\nstart one with `tui <command>` on the machine that owns the terminal",
            path.display()
        )
        .into()
    })
}

/// The byte sink for frames: the server's unix socket.
fn connect(s: &session::Session) -> Result<Sink, Box<dyn std::error::Error>> {
    let sock = std::os::unix::net::UnixStream::connect(&s.socket).map_err(|e| {
        format!("connecting to {}: {e}", s.socket)
    })?;
    Ok(Sink::Unix(sock))
}

/// Blocking sink: writes plus the reply reads the startup handshake needs.
enum Sink {
    Unix(std::os::unix::net::UnixStream),
}

impl io::Write for Sink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Sink::Unix(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Sink::Unix(s) => s.flush(),
        }
    }
}

impl Sink {
    /// Whether the server has closed its end of the connection.
    ///
    /// A client holding the grid has nothing else to watch: the server owns the
    /// terminal and its own stdin, so this is the only signal that the session is
    /// over. Reading zero bytes, or a closed socket, both mean it ended.
    ///
    /// Never blocks: a zero-length peek returns immediately whether or not any
    /// data is waiting.
    fn server_gone(&mut self) -> bool {
        match self {
            Sink::Unix(s) => {
                // `recv(MSG_PEEK)` rather than `UnixStream::peek`, which is still
                // unstable. A peek leaves any waiting message in place, so the
                // reply this client still has to read is not consumed.
                let mut byte = [0u8; 1];
                match rustix::net::recv(s, &mut byte[..], rustix::net::RecvFlags::PEEK) {
                    // A closed connection.
                    Ok((_, 0)) => true,
                    // Data waiting, or nothing to report: either way live.
                    Ok(_) => false,
                    Err(e) => e != rustix::io::Errno::WOULDBLOCK,
                }
            }
        }
    }

    /// Read the next message, blocking.
    fn read_msg(&mut self) -> io::Result<p2p::Msg> {

        match self {
            Sink::Unix(s) => p2p::Msg::read(s),
        }
    }

    /// Block until a `Pong` carrying `nonce` arrives, discarding anything else.
    ///
    /// Bounded so a dead or mute server cannot hang a startup forever. The
    /// bound is a real socket timeout, not a deadline checked between reads:
    /// reading a message blocks until it arrives, so a deadline tested in the
    /// loop would never be reached and the client would wait indefinitely.
    fn read_pong(&mut self, nonce: u32) -> Result<(), Box<dyn std::error::Error>> {
        match self.with_timeout(Some(Duration::from_secs(5)), |s| {
            loop {
                match s.read_msg()? {
                    p2p::Msg::Pong { nonce: n } if n == nonce => return Ok(()),
                    p2p::Msg::Error { message } => return Err(message.into()),
                    other => say(format!("ignoring {other:?} while waiting for pong")),
                }
            }
        }) {
            Ok(()) => Ok(()),
            Err(e) if timed_out(e.as_ref()) => {
                Err(format!("no pong within 5s (wanted nonce {nonce:#x})").into())
            }
            Err(e) => Err(e),
        }
    }

    /// Block until the server assigns an image id, bounded the same way.
    fn read_image(&mut self) -> Result<u32, Box<dyn std::error::Error>> {
        match self.with_timeout(Some(Duration::from_secs(5)), |s| loop {
            match s.read_msg()? {
                p2p::Msg::Image { id } => return Ok(id),
                p2p::Msg::Error { message } => return Err(message.into()),
                other => say(format!("ignoring {other:?} while waiting for the image id")),
            }
        }) {
            Ok(id) => Ok(id),
            Err(e) if timed_out(e.as_ref()) => Err("no image id within 5s".into()),
            Err(e) => Err(e),
        }
    }

    /// Run `f` with a read timeout installed, restoring the previous one after.
    fn with_timeout<T>(
        &mut self,
        timeout: Option<Duration>,
        f: impl FnOnce(&mut Self) -> Result<T, Box<dyn std::error::Error>>,
    ) -> Result<T, Box<dyn std::error::Error>> {
        let previous = match self {
            Sink::Unix(s) => {
                let previous = s.read_timeout()?;
                s.set_read_timeout(timeout)?;
                Some(previous)
            }
        };
        let res = f(self);
        if let (Sink::Unix(s), Some(previous)) = (&self, previous) {
            let _ = s.set_read_timeout(previous);
        }
        res
    }

    /// Non-blocking read, for checking whether the server reported anything
    /// without stalling the send loop.
    fn try_read_msg(&mut self) -> Result<Option<p2p::Msg>, Box<dyn std::error::Error>> {
        match self {
            Sink::Unix(s) => {
                // A message is only worth reading if a whole one is waiting.
                // `read_timeout` bounds the wait so the send loop is never
                // blocked by a server that has nothing to say.
                s.set_read_timeout(Some(Duration::from_millis(1)))?;
                let res = p2p::Msg::read(s);
                s.set_read_timeout(None)?;
                match res {
                    Ok(msg) => Ok(Some(msg)),
                    Err(e) if is_would_block(&e) => Ok(None),
                    Err(e) => Err(e.into()),
                }
            }
        }
    }
}

/// Whether an error is the timeout a bounded read raises.
fn timed_out(e: &(dyn std::error::Error + 'static)) -> bool {
    e.downcast_ref::<io::Error>().is_some_and(is_would_block)
}

/// A timeout and "no data yet" are the same condition to a caller here.
fn is_would_block(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

