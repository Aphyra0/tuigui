//! gobo: run a GUI app in an in-process headless Wayland compositor and stream
//! its window into this terminal over TGP.
//!
//! Usage: `gobo /path/to/app`
//!
//! # One reader for fd 0
//!
//! The terminal's input stream carries two unrelated things: TGP replies (APC
//! sequences, `ESC _ G ... ESC \`) that easytgp's router needs to settle its
//! transmits, and user input (SGR mouse reports, keys) that the render loop
//! needs. Both arrive on fd 0, so exactly one place may read it.
//!
//! [`input_thread`] is that place. It reads bytes, routes whole APC sequences
//! down a pipe to the renderer's router, and turns everything else into
//! [`Input`] events. Letting crossterm read fd 0 as well would be a race with a
//! silent failure mode: a reply consumed as an unknown escape never reaches the
//! router, and the first transmit then waits for an ack that will not come.

use std::io::{Read as _, Write as _};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use gobo_comp::{Button, Client, Compositor, ScrollDelta};
use gobo_renderer::{State, TgpRendererWidget};
use ratatui::crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use ratatui::crossterm::execute;

/// Pointer events queued for the input thread.
#[derive(Debug, Clone, Copy)]
enum Input {
    Motion { x: f64, y: f64 },
    Button(Button),
    Scroll(ScrollDelta),
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let app = match args.next() {
        Some(app) => app,
        None => {
            eprintln!("usage: gobo <app>");
            std::process::exit(2);
        }
    };

    // Query the cell size before raw mode: the terminal's `CSI 16 t` reply would
    // otherwise be consumed by the input reader below.
    let (cell_w, cell_h) = query_cell_size().unwrap_or((8, 16));
    let (cols, rows) = ratatui::crossterm::terminal::size().unwrap_or((80, 24));
    let size = (cols as u32 * cell_w, rows as u32 * cell_h);

    let compositor = Compositor::init(size, 1.0)?;
    let client = compositor.spawn(&app, size)?;

    // Raw mode without the alternate screen: the alt screen keeps its own
    // graphics storage, which can be torn down on resize and strand the image.
    let mut terminal = ratatui::try_init_with_options(ratatui::TerminalOptions {
        viewport: ratatui::Viewport::Fullscreen,
    })?;
    execute!(std::io::stdout(), EnableMouseCapture)?;

    // Split fd 0 before the renderer exists: the reader owns the terminal, the
    // renderer gets the APC stream, and the loop gets pointer events. The loop
    // keeps its own client handle, since the renderer takes ownership of one.
    let (reply_rx, reply_tx) = std::io::pipe()?;
    let (tx, rx) = mpsc::channel::<Input>();
    let (quit_tx, quit_rx) = mpsc::channel::<()>();
    let reader = std::thread::Builder::new()
        .name("gobo-input".into())
        .spawn(move || input_thread(std::io::stdin(), reply_tx, tx, quit_tx))?;

    let input_client = client.clone();
    let mut state = State::with_replies(client, reply_rx);

    let res = (|| -> Result<(), Box<dyn std::error::Error>> {
        loop {
            terminal.draw(|f| {
                let area = f.area();
                f.render_stateful_widget(TgpRendererWidget, area, &mut state);
            })?;

            // Quit is checked non-blocking so a quiet terminal cannot hold a
            // frame back; pointer events go to the compositor thread.
            if quit_rx.try_recv().is_ok() {
                return Ok(());
            }
            while let Ok(ev) = rx.try_recv() {
                let _ = client_tx(&input_client, ev);
            }
        }
    })();

    execute!(std::io::stdout(), DisableMouseCapture)?;
    ratatui::restore();

    // Teardown order matters: the compositor thread only exits once every
    // control-channel sender is dropped, and both the render state and the
    // reader thread hold client handles that own a sender. Release them first,
    // otherwise `shutdown` would block forever on the thread join. Closing
    // stdin also unblocks the reader if it is sitting in a read.
    drop(state);
    close_stdin();
    let _ = reader.join();
    compositor.shutdown();
    res
}

/// Send one pointer event to the compositor, blocking. Runs on the render
/// thread, so a stalled compositor costs a frame rather than correctness.
fn client_tx(client: &Client, ev: Input) -> Result<(), gobo_comp::CompError> {
    match ev {
        Input::Motion { x, y } => client.move_mouse(x, y),
        Input::Button(button) => client.click(button),
        Input::Scroll(delta) => client.scroll(delta),
    }
}

/// Owns fd 0 for the whole session.
///
/// Bytes are classified by walking the stream once: an `ESC _ G` opens a TGP
/// command, which is copied verbatim (through its `ESC \` terminator) into the
/// reply pipe for easytgp's router; everything else is fed to the mouse parser.
/// Routing whole sequences rather than "the bytes that look like replies" is
/// what keeps the router's own framing intact.
fn input_thread(
    stdin: impl std::io::Read,
    mut replies: impl std::io::Write,
    events: mpsc::Sender<Input>,
    quit: mpsc::Sender<()>,
) {
    let mut bytes = std::io::BufReader::new(stdin).bytes();
    // Pending mouse bytes, since a report can be split across reads.
    let mut pending: Vec<u8> = Vec::new();

    while let Some(Ok(b)) = bytes.next() {
        if b != 0x1b {
            pending.push(b);
            continue;
        }

        // An escape arrives: look ahead far enough to tell a TGP command from a
        // mouse report. `ESC [ M` is the legacy report, `ESC [ <` SGR, both
        // shorter than the APC prefix, so a bounded lookahead cannot swallow a
        // report that happens to start with `ESC`.
        match bytes.next() {
            Some(Ok(b'_')) => match bytes.next() {
                Some(Ok(b'G')) => {
                    let mut apc = vec![0x1b, b'_', b'G'];
                    // Copy through the ST terminator. A malformed sequence ends
                    // at the next escape rather than eating the stream.
                    for next in bytes.by_ref() {
                        match next {
                            Ok(0x1b) => {
                                apc.push(0x1b);
                                if let Some(Ok(b'\\')) = bytes.next() {
                                    apc.push(b'\\');
                                }
                                break;
                            }
                            Ok(c) => apc.push(c),
                            Err(_) => break,
                        }
                    }
                    if replies.write_all(&apc).is_err() {
                        // The renderer is gone; nothing left to route to.
                        return;
                    }
                    let _ = replies.flush();
                }
                Some(Ok(c)) => pending.extend_from_slice(&[0x1b, b'_', c]),
                Some(Err(_)) => return,
                None => pending.extend_from_slice(&[0x1b, b'_']),
            },
            Some(Ok(b'[')) => {
                pending.extend_from_slice(&[0x1b, b'[']);
            }
            Some(Ok(c)) => {
                // `ESC` `c` is a full reset, which a terminal sends on resize.
                if c == b'c' {
                    pending.clear();
                } else {
                    pending.extend_from_slice(&[0x1b, c]);
                }
            }
            Some(Err(_)) => return,
            None => pending.push(0x1b),
        }

        if parse_input(&pending, &events).is_err() {
            let _ = quit.send(());
            return;
        }
        pending.clear();
    }
}

/// Parse accumulated terminal input bytes into pointer events.
///
/// Returns `Err(())` when the reader should stop (the user quit), so the caller
/// can tell "keep reading" from "shut down".
fn parse_input(bytes: &[u8], events: &mpsc::Sender<Input>) -> Result<(), ()> {
    let text = String::from_utf8_lossy(bytes);

    for seq in text.split('\x1b').filter(|s| !s.is_empty()) {
        let Some(body) = seq.strip_prefix('[') else {
            // A bare modifier or control byte: `q` and ctrl-c quit.
            if seq == "q" || seq == "\u{3}" {
                return Err(());
            }
            continue;
        };

        // SGR mouse: `ESC [ < b ; x ; y M/m`.
        if let Some(rest) = body.strip_prefix('<') {
            let (params, press) = match rest.strip_suffix('M') {
                Some(p) => (p, true),
                None => match rest.strip_suffix('m') {
                    Some(p) => (p, false),
                    None => continue,
                },
            };
            let nums: Vec<i32> = params
                .split(';')
                .filter_map(|v| v.parse().ok())
                .collect();
            if let [code, col, row] = nums[..] {
                // Coordinates are 1-based cells; the widget fills the terminal.
                let (x, y) = cell_to_pixel(col, row);
                if let Some(button) = scroll_button(code) {
                    let _ = events.send(Input::Scroll(button));
                } else if code & 3 == 3 {
                    let _ = events.send(Input::Motion { x, y });
                } else if press {
                    let _ = events.send(Input::Motion { x, y });
                    if let Some(button) = button_for(code) {
                        let _ = events.send(Input::Button(button));
                    }
                }
            }
            continue;
        }

        // LEGACY mouse: `ESC [ M b x y` with 32-offset bytes. Some terminals
        // still emit this for drags.
        if let Some(rest) = body.strip_prefix('M') {
            let raw = rest.as_bytes();
            if raw.len() >= 3 {
                let code = raw[0] as i32 - 32;
                let col = raw[1] as i32 - 32;
                let row = raw[2] as i32 - 32;
                let (x, y) = cell_to_pixel(col, row);
                if code & 64 != 0 {
                    let steps = if code & 1 == 0 { 1 } else { -1 };
                    let _ = events.send(Input::Scroll(ScrollDelta::vertical(steps)));
                } else if code & 3 == 3 {
                    let _ = events.send(Input::Motion { x, y });
                } else if let Some(button) = button_for(code) {
                    let _ = events.send(Input::Motion { x, y });
                    let _ = events.send(Input::Button(button));
                }
            }
        }
    }
    Ok(())
}

/// Map a 1-based terminal cell to the center of that cell in output pixels.
fn cell_to_pixel(col: i32, row: i32) -> (f64, f64) {
    let (cols, rows) = ratatui::crossterm::terminal::size().unwrap_or((80, 24));
    let (cols, rows) = (cols.max(1) as f64, rows.max(1) as f64);
    let (w, h) = terminal_pixels();
    (
        ((col.max(1) as f64 - 0.5) / cols) * w,
        ((row.max(1) as f64 - 0.5) / rows) * h,
    )
}

/// The output size the compositor was created at, in pixels. Cached so the
/// parser does not re-query the cell size per event.
fn terminal_pixels() -> (f64, f64) {
    use std::sync::OnceLock;
    static SIZE: OnceLock<(u32, u32)> = OnceLock::new();
    let (w, h) = *SIZE.get_or_init(|| {
        let (cell_w, cell_h) = query_cell_size().unwrap_or((8, 16));
        let (cols, rows) = ratatui::crossterm::terminal::size().unwrap_or((80, 24));
        (cols as u32 * cell_w, rows as u32 * cell_h)
    });
    (w as f64, h as f64)
}

/// Wheel direction from an SGR button code (64 = up, 65 = down, 66/67 = left/right).
fn scroll_button(code: i32) -> Option<ScrollDelta> {
    match code & 0b1100_0000 {
        64 => Some(ScrollDelta::vertical(1)),
        65 => Some(ScrollDelta::vertical(-1)),
        66 => Some(ScrollDelta {
            vertical: 0,
            horizontal: -1,
        }),
        67 => Some(ScrollDelta {
            vertical: 0,
            horizontal: 1,
        }),
        _ => None,
    }
}

/// Button from an SGR or legacy code's low bits: 0 left, 1 middle, 2 right.
fn button_for(code: i32) -> Option<Button> {
    match code & 3 {
        0 => Some(Button::Left),
        1 => Some(Button::Middle),
        2 => Some(Button::Right),
        _ => None,
    }
}

/// Close fd 0 so a reader blocked in `read` on it sees EOF and exits.
fn close_stdin() {
    if let Ok(null) = std::fs::File::open("/dev/null") {
        let _ = rustix::stdio::dup2_stdin(null);
    }
}

/// Ask the terminal for its cell size in pixels via `CSI 16 t`, which replies
/// `CSI 4 ; height ; width t`. Falls back to `TIOCGWINSZ` when the terminal
/// doesn't answer.
fn query_cell_size() -> Option<(u32, u32)> {
    let mut out = std::io::stdout();
    out.write_all(b"\x1b[16t").ok();
    out.flush().ok();

    let bytes = read_tty(Duration::from_millis(300));
    if let Ok(s) = std::str::from_utf8(&bytes) {
        if let Some(idx) = s.find("\x1b[4;") {
            let rest = &s[idx + 4..];
            let end = rest.find('t').unwrap_or(rest.len());
            let mut it = rest[..end].split(';');
            let h: u32 = it.next().and_then(|v| v.trim().parse().ok()).unwrap_or(0);
            let w: u32 = it.next().and_then(|v| v.trim().parse().ok()).unwrap_or(0);
            if w > 0 && h > 0 {
                return Some((w, h));
            }
        }
    }

    let ws = ratatui::crossterm::terminal::window_size().ok()?;
    if ws.width > 0 && ws.height > 0 && ws.columns > 0 && ws.rows > 0 {
        return Some((
            (ws.width as u32 / ws.columns as u32).max(1),
            (ws.height as u32 / ws.rows as u32).max(1),
        ));
    }
    None
}

/// Read whatever `/dev/tty` has ready within `budget`, non-blocking.
fn read_tty(budget: Duration) -> Vec<u8> {
    use rustix::fs::{open, OFlags};

    let Ok(fd) = open("/dev/tty", OFlags::RDONLY | OFlags::NONBLOCK, rustix::fs::Mode::empty())
    else {
        return Vec::new();
    };
    let mut tty = std::fs::File::from(fd);
    let deadline = Instant::now() + budget;
    let mut buf = Vec::new();
    let mut tmp = [0u8; 64];
    while Instant::now() < deadline {
        match tty.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(_) => std::thread::sleep(Duration::from_millis(5)),
        }
    }
    buf
}
