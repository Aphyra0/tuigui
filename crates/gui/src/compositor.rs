//! The program, running in a compositor of our own, and its window as frames.
//!
//! The video path decodes a file; this one runs a program. The program gets a Wayland
//! session of its own — a compositor in this process, one output, one window admitted —
//! and the pixels it commits there are read straight out of that compositor's buffer.
//! There is no screencopy protocol and no second process in between: the window being
//! presented is already in this address space, so a frame is a copy and an encode.
//!
//! ```text
//!   start(program) ──▶ the program commits a window ──▶ frame() ──▶ JPEG
//! ```
//!
//! Frames are pulled, not pushed: the session's own loop calls [`App::frame`], so there
//! is no queue and nothing that can hold a frame this side has not asked for.

use std::io::Read;
use std::path::Path;

use compositor::{Client, Compositor, Key};
use wire::transport::BoxFuture;

/// JPEG quality for a captured window.
///
/// The same value the video path uses, and for the same reason: these pixels are on their
/// way to a grid of terminal cells, where legibility is bought long before fidelity is.
const QUALITY: i32 = 80;

/// A program running in a compositor, with its window ready to be read.
pub struct App {
    /// The program's window, and the only handle that reads it.
    client: Client,
    /// The compositor the program is running in.
    ///
    /// Held rather than dropped: the compositor's thread stops when every control sender
    /// is gone, so this is what keeps the session alive. Dropping the `App` ends it.
    _compositor: Compositor,
    /// The JPEG encoder, kept across frames: building one per frame is the expensive part
    /// of encoding, not the encoding.
    encoder: turbojpeg::Compressor,
    /// The last frame sent, so an idle window is not re-encoded and re-transmitted.
    last: Option<Vec<u8>>,
}

impl App {
    /// Begin starting `path` in a compositor of `size` pixels.
    ///
    /// Returns the compositor and a future that spawns the program and waits for its first
    /// window. The wait is *not* done here, because it is a wait: a caller that is drawing
    /// has to finish its frame before an address it drew can reach the terminal, so a step
    /// that blocked here would stop the very frame it is part of. The caller polls the
    /// future instead, once per frame, and gets the [`App`] when the program has drawn.
    ///
    /// The future owns the compositor, so nothing else has to keep it alive while the wait
    /// runs, and dropping the future shuts both the compositor and the program down.
    pub fn start(
        path: &Path,
        size: (u32, u32),
    ) -> std::io::Result<BoxFuture<'static, std::io::Result<Self>>> {
        let compositor = Compositor::init_with_options(
            size,
            1.0,
            compositor::CompositorOptions {
                show_cursor: true,
                // X11 apps are a later concern. Waiting for XWayland's window manager to
                // come up blocks the first frame on machines where it never does, which
                // looks exactly like a program that drew nothing.
                use_x11: false,
            },
        )
        .map_err(std::io::Error::other)?;

        let path = path.to_path_buf();
        Ok(Box::pin(async move {
            // Blocks until the program has committed a window, so the first frame after
            // this is a real one rather than the empty output.
            let client = compositor
                .spawn(&path, size)
                .map_err(std::io::Error::other)?;

            // Built after the program is up, because a compressor is only worth making for
            // a session that has something to encode.
            let mut encoder = turbojpeg::Compressor::new().map_err(std::io::Error::other)?;
            encoder
                .set_quality(QUALITY)
                .map_err(std::io::Error::other)?;

            Ok(App {
                client,
                _compositor: compositor,
                encoder,
                last: None,
            })
        }))
    }

    /// Whether the program is still running.
    pub fn running(&self) -> bool {
        self.client.is_running()
    }

    /// The next frame, as JPEG, or `None` when there is nothing new to send.
    ///
    /// `None` is not an end and not an error: a window that has not repainted has nothing
    /// to send, and re-sending the frame it already shows would spend the whole budget on
    /// identical bytes. A capture that fails is also `None`, and the caller asks again:
    /// one missed round-trip is not the end of the stream.
    pub fn frame(&mut self) -> Option<Vec<u8>> {
        let frame = match self.client.capture() {
            Ok(frame) => frame,
            Err(e) => {
                log::debug!("capture did not complete: {e}");
                return None;
            }
        };

        if self.last.as_deref() == Some(frame.rgba.as_slice()) {
            return None;
        }

        // The captured buffer is a Wayland `Argb8888` buffer copied byte for byte, whose
        // bytes are B, G, R, A on a little-endian machine. Encoded as `BGRX` so the
        // terminal shows the colours the program used, not a red/blue swap.
        let jpeg = match self.encoder.compress_to_vec(turbojpeg::Image {
            pixels: frame.rgba.as_slice(),
            width: frame.width as usize,
            height: frame.height as usize,
            pitch: frame.width as usize * 4,
            format: turbojpeg::PixelFormat::BGRX,
        }) {
            Ok(jpeg) => jpeg,
            Err(e) => {
                log::error!("could not encode a {}x{} frame: {e}", frame.width, frame.height);
                return None;
            }
        };
        self.last = Some(frame.rgba);
        Some(jpeg)
    }

    /// Deliver everything that arrives on `stdin` to the running program.
    ///
    /// Blocking, and on a thread of its own: it reads a descriptor for as long as the
    /// session lasts, and nothing else in this client may wait like that. Every byte here
    /// is input — the terminal's answers to this side's graphics escapes are consumed by
    /// `tui`'s router before they reach this client — so nothing has to be told apart from
    /// a keystroke.
    ///
    /// Borrows the app rather than taking it: the caller keeps reading frames from the same
    /// program while this thread feeds it keys, and the handle that does either is
    /// cloneable.
    pub fn route_input(&self, stdin: impl Read + Send + 'static) -> std::io::Result<()> {
        let client = self.client.clone();
        std::thread::Builder::new()
            .name("gui-input".into())
            .spawn(move || {
                let mut input = stdin;
                let mut buf = [0u8; 1024];
                loop {
                    match input.read(&mut buf) {
                        Ok(0) => return,
                        Ok(n) => {
                            for key in keys(&buf[..n]) {
                                // A failure means the compositor has gone, so there is
                                // nothing left to type into.
                                if client.type_key(key).is_err() {
                                    return;
                                }
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(e) => {
                            log::info!("the keyboard ended: {e}");
                            return;
                        }
                    }
                }
            })
            .map(|_| ())
    }
}

/// The key events in one chunk of terminal input.
///
/// A terminal sends printable keys as themselves and everything else as an escape
/// sequence, so this walks the bytes and looks only for the sequences that name a key.
/// An escape sequence it does not recognize is skipped rather than typed: sending the
/// bytes of an unknown sequence would type its `ESC`, its `[`, and its parameter digits
/// into the program, which is worse than doing nothing.
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

    /// One chunk, the way `route_input` hands it over.
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
    /// getting the parse wrong types `ESC`, `[`, and `A` into the program instead of
    /// moving the cursor.
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
