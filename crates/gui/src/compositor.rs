//! A program run inside our own compositor, and its window turned into frames.
//!
//! The video path decodes a file; this one runs a program. The program gets a Wayland
//! session of its own — a compositor in this process, with a single output and no
//! window manager beyond the one-window policy — and the pixels it commits there are
//! read straight out of that compositor's buffer. There is no screencopy protocol and
//! no second process in between: the window being presented is already in this address
//! space, so a frame is a copy.
//!
//! ```text
//!   comp.spawn(app) ──▶ the app commits a window ──▶ capture ──▶ JPEG ──▶ the link
//!   keys ──▶ client.type_key / click / scroll ──┘
//! ```
//!
//! Both halves are blocking work — a compositor loop and a program that has to start —
//! so both live on threads of their own and hand frames over a channel, exactly as the
//! ffmpeg decode does. The channel holds one frame, which is the pacing: a link that
//! falls behind drops frames nobody would have looked at rather than delaying the
//! window behind the app that is drawing it.

use std::io::Read;
use std::path::Path;
use std::sync::mpsc as std_mpsc;

use compositor::{Client, Compositor, Key};
use tokio::sync::mpsc;

/// JPEG quality for a captured window.
///
/// The same value the video path uses, and for the same reason: these pixels are on
/// their way to a grid of terminal cells, where legibility is bought long before
/// fidelity is.
const QUALITY: i32 = 80;

/// How long to wait between captures when the window has not changed.
///
/// A capture is a request the compositor answers from its own state, so polling faster
/// than this only spends a round-trip; a window that repaints itself is the case this
/// has to keep up with, and that is tens of milliseconds by nature. Not zero, because
/// a loop with no wait at all would spin against the compositor thread for as long as
/// the app is idle, which is most of the time.
const IDLE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(16);

/// Run `path` in a compositor of `size` pixels and produce JPEG frames of its window.
///
/// `stdin` is this client's keyboard, and what arrives on it is delivered to the app as
/// key events. It comes from the caller because the caller is what owns the pane's
/// input: nothing here reads the process's own stdin.
///
/// The compositor is created on this thread and not on the capture thread, so a session
/// that cannot be started at all is an error the caller sees. Everything after that —
/// starting the program, waiting for its first frame — is on the thread, because it
/// blocks for as long as the program takes to draw.
pub fn capture(
    path: &Path,
    size: (u32, u32),
    stdin: impl Read + Send + 'static,
) -> std::io::Result<mpsc::Receiver<Vec<u8>>> {
    let compositor = Compositor::init_with_options(
        size,
        1.0,
        compositor::CompositorOptions {
            show_cursor: true,
            use_x11: false,
        },
    )
    .map_err(std::io::Error::other)?;

    let (tx, frames) = mpsc::channel(1);

    // The program's own handle travels back to the input thread, which cannot make one
    // for itself: `spawn` both starts the program and waits for its first frame, so the
    // handle only exists once the capture thread has run. The input thread waits here
    // for exactly that, and reads no input before it has somewhere to put it.
    let (handoff_tx, handoff_rx) = std_mpsc::channel::<Client>();

    let path = path.to_path_buf();
    std::thread::Builder::new()
        .name("gui-comp".into())
        .spawn(move || {
            if let Err(e) = pump(compositor, &path, size, tx, handoff_tx) {
                eprln!("gui: {}: {e}", path.display());
            }
        })?;

    std::thread::Builder::new()
        .name("gui-input".into())
        .spawn(move || {
            // A handoff that never arrives means `spawn` failed, and the capture thread
            // has already reported that. Nothing to type into.
            let Ok(client) = handoff_rx.recv() else {
                return;
            };
            if let Err(e) = route_input(stdin, client) {
                eprln!("gui: the keyboard ended: {e}");
            }
        })?;

    Ok(frames)
}

/// Start the program, then capture its window until it is gone.
///
/// The compositor is moved in and dropped here, which is what shuts it down: its thread
/// only stops once every control sender is gone.
fn pump(
    compositor: Compositor,
    path: &Path,
    size: (u32, u32),
    frames: mpsc::Sender<Vec<u8>>,
    handoff: std_mpsc::Sender<Client>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Blocks until the program has committed a window, so the first frame after this is
    // a real one rather than the empty output.
    let client = compositor.spawn(path, size)?;

    // The input thread's handle. A failure here means it has gone, which does not stop
    // the window being captured: the app is still running and still has something to
    // show.
    let _ = handoff.send(client.clone());

    let mut encoder = turbojpeg::Compressor::new()?;
    encoder.set_quality(QUALITY)?;

    let mut last: Option<Vec<u8>> = None;
    while client.is_running() {
        let frame = match client.capture() {
            Ok(frame) => frame,
            // Not ready yet (the window has not committed) or the control round-trip
            // timed out. Neither is the end of the stream, so wait a beat and ask again.
            Err(e) if !matches!(e, compositor::CompError::WindowGone) => {
                eprln!("gui: capture did not complete: {e}");
                std::thread::sleep(IDLE_INTERVAL);
                continue;
            }
            // The window is gone, or the app exited between the check above and here.
            Err(_) => return Ok(()),
        };

        // Nothing changed, so there is nothing to send. The comparison is what keeps an
        // idle window from spending its repaint rate on identical frames.
        if last.as_deref() == Some(frame.rgba.as_slice()) {
            std::thread::sleep(IDLE_INTERVAL);
            continue;
        }

        // The captured buffer is BGRA, not RGBA: it is a Wayland `Argb8888` buffer copied
        // byte for byte, which little-endian means the channels are B, G, R, A. Encoded
        // as `BGRX` so the terminal shows the colours the app used, not a red/blue swap.
        let jpeg = encoder.compress_to_vec(turbojpeg::Image {
            pixels: frame.rgba.as_slice(),
            width: frame.width as usize,
            height: frame.height as usize,
            pitch: frame.width as usize * 4,
            format: turbojpeg::PixelFormat::BGRX,
        })?;
        last = Some(frame.rgba);

        // What is about to go on the wire. A picture that does not appear can be a frame
        // that never left this process or one that left and was refused, and the two look
        // the same from a pane: nothing but the background.
        eprln!(
            "gui: captured {}x{} -> {} bytes of JPEG",
            frame.width,
            frame.height,
            jpeg.len()
        );

        // Blocks while the link is behind, which is the backpressure: a window repainting
        // faster than it can be sent must not become unbounded memory.
        if frames.blocking_send(jpeg).is_err() {
            // The client stopped taking frames, so the session is over.
            return Ok(());
        }
    }
    Ok(())
}

/// Deliver everything that arrives on `stdin` to the running program.
///
/// Every byte here is input: the terminal's answers to this side's graphics escapes are
/// consumed by `tui`'s router before they reach this client, so nothing that arrives has
/// to be told apart from a keystroke. That is why this is a loop over bytes and not the
/// two-way classification the same job needs when the same descriptor is carrying both.
fn route_input(stdin: impl Read, client: Client) -> std::io::Result<()> {
    let mut input = stdin;
    let mut buf = [0u8; 1024];
    loop {
        match input.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => {
                for key in keys(&buf[..n]) {
                    if client.type_key(key).is_err() {
                        // The compositor has gone, so there is nothing left to type into.
                        return Ok(());
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

/// The key events in one chunk of terminal input.
///
/// A terminal sends printable keys as themselves and everything else as an escape
/// sequence, so this walks the bytes and looks only for the sequences that name a key.
/// An escape sequence it does not recognize is skipped rather than typed: sending the
/// bytes of an unknown sequence would type its `ESC`, its `[`, and its parameter digits
/// into the app, which is worse than doing nothing.
fn keys(bytes: &[u8]) -> Vec<Key> {
    let mut keys = Vec::new();
    let mut rest = bytes;
    while let Some(&first) = rest.first() {
        rest = &rest[1..];
        match first {
            0x1b => {
                let Some(body) = rest.strip_prefix(b"[") else {
                    // A bare `ESC`: the one escape that is its own key. A sequence split
                    // across two reads is the limit of this, and it costs a stray Esc.
                    keys.push(Key::Escape);
                    continue;
                };
                // `ESC [ <params> <final>`. The parameters are skipped: which key this is
                // is decided by the final byte alone, and the modifiers some terminals
                // append (`1;5A`) are not distinguishable in the compositor's key set.
                let end = body
                    .iter()
                    .position(|b| (0x40..=0x7e).contains(b))
                    .unwrap_or(body.len());
                match body.get(end) {
                    Some(b'A') => keys.push(Key::Up),
                    Some(b'B') => keys.push(Key::Down),
                    Some(b'C') => keys.push(Key::Right),
                    Some(b'D') => keys.push(Key::Left),
                    Some(b'H') => keys.push(Key::Home),
                    Some(b'F') => keys.push(Key::End),
                    _ => {}
                }
                // Past the final byte. A truncated sequence ends the chunk.
                rest = body.get(end + 1..).unwrap_or_default();
            }
            b'\r' | b'\n' => keys.push(Key::Return),
            b'\t' => keys.push(Key::Tab),
            // Both are what terminals send for the backspace key, depending on whether
            // the erase character is `^H` or `DEL`.
            0x08 | 0x7f => keys.push(Key::Backspace),
            // Printable ASCII, which is what a key press produces. A control byte is not
            // a key in the compositor's set, and dropping one is better than typing
            // something the user did not.
            0x20..=0x7e => keys.push(Key::Char(first as char)),
            _ => {}
        }
    }
    keys
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One chunk, the way `route_input` would hand it over.
    fn decoded(input: &str) -> Vec<Key> {
        keys(input.as_bytes())
    }

    #[test]
    fn a_run_of_letters_is_a_run_of_keys() {
        assert_eq!(
            decoded("hi"),
            [Key::Char('h'), Key::Char('i')],
            "each printable byte is its own key press"
        );
    }

    /// The sequences a terminal actually sends for the keys the compositor names.
    ///
    /// This is the assertion that matters: the cursor keys are multi-byte escapes, and
    /// getting the parse wrong types `ESC`, `[`, and `A` into the app instead of moving
    /// the cursor.
    #[test]
    fn cursor_and_editing_sequences_are_named_keys() {
        assert_eq!(decoded("\x1b[A"), [Key::Up]);
        assert_eq!(decoded("\x1b[B"), [Key::Down]);
        assert_eq!(decoded("\x1b[C"), [Key::Right]);
        assert_eq!(decoded("\x1b[D"), [Key::Left]);
        assert_eq!(decoded("\x1b[H"), [Key::Home]);
        assert_eq!(decoded("\x1b[F"), [Key::End]);
    }

    /// A sequence either side of a printable key must not swallow that key, and the
    /// printable key must not interrupt the sequence's own parse.
    #[test]
    fn a_sequence_next_to_a_letter_is_two_keys() {
        assert_eq!(decoded("\x1b[Aa"), [Key::Up, Key::Char('a')]);
        assert_eq!(decoded("a\x1b[A"), [Key::Char('a'), Key::Up]);
        assert_eq!(decoded("a\x1b[Ab"), [Key::Char('a'), Key::Up, Key::Char('b')]);
    }

    /// A sequence with parameters, which is what a terminal sends when a modifier is
    /// held. The key is the final byte regardless of the parameters in front of it.
    #[test]
    fn parameters_before_the_final_byte_are_skipped() {
        assert_eq!(decoded("\x1b[1;5A"), [Key::Up]);
        assert_eq!(decoded("\x1b[3~"), [], "an unnamed final byte is not a key");
    }

    #[test]
    fn control_bytes_are_the_keys_that_have_names() {
        assert_eq!(decoded("\r"), [Key::Return]);
        assert_eq!(decoded("\n"), [Key::Return]);
        assert_eq!(decoded("\t"), [Key::Tab]);
        assert_eq!(decoded("\x7f"), [Key::Backspace]);
        assert_eq!(decoded("\x08"), [Key::Backspace]);
        assert_eq!(decoded("\x1b"), [Key::Escape]);
    }

    /// A control byte with no name is dropped rather than typed as something else.
    #[test]
    fn an_unnamed_control_byte_is_dropped() {
        assert_eq!(decoded("\x00\x01\x02"), []);
        // Surrounded, so the drop is a gap and not the end of the run.
        assert_eq!(
            decoded("a\x00b"),
            [Key::Char('a'), Key::Char('b')],
            "an unnamed byte must not stop the ones after it"
        );
    }

    /// A truncated escape is the split-read case: the rest of the sequence has not
    /// arrived. Only the `ESC` is lost, and nothing is typed from the fragment.
    #[test]
    fn a_truncated_sequence_types_nothing_from_its_fragment() {
        assert_eq!(decoded("\x1b[1;5"), []);
    }
}
