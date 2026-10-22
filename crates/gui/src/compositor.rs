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

use std::path::Path;

use compositor::{Client, Compositor};
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
    #[tracing::instrument(name = "app.start", skip_all)]
    pub fn start(
        path: &Path,
        size: (u32, u32),
    ) -> std::io::Result<BoxFuture<'static, std::io::Result<Self>>> {
        let compositor = Compositor::init_with_options(
            size,
            1.0,
            compositor::CompositorOptions {
                // Drawn into the captured pixels, so the pointer is visible in
                // the pane without the terminal having to place one. The
                // compositor has no cursor of its own to show, so without this
                // there is nothing on screen saying where the pointer is.
                show_cursor: true,
                // X11 apps are a later concern. Waiting for XWayland's window manager to
                // come up blocks the first frame on machines where it never does, which
                // looks exactly like a program that drew nothing.
                use_x11: false,
                // Hardcoded for now: a dense panel shows a 1x interface as a
                // half-sized one, and 1.75 is a comfortable middle on one. The
                // output's resolution is unchanged, so this makes the program
                // draw its interface larger in the same pixels rather than
                // making the window bigger.
                scale: 1.75,
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

    /// Deliver one input event to the program.
    ///
    /// The `cell` is the terminal's cell size in output pixels, and it is what
    /// converts a mouse position: an event arrives in the terminal's cells and the
    /// compositor's pointer is in output pixels, so the two do not agree on what a
    /// coordinate means, and the caller is the side that knows the cell.
    #[tracing::instrument(name = "input", skip(self))]
    pub fn input(
        &self,
        event: &termputs::InputEvent,
        cell: (u32, u32),
    ) -> std::io::Result<()> {
        match event {
            termputs::InputEvent::Key { key, state } => self.key(key, *state),
            termputs::InputEvent::Mouse(mouse) => self.mouse(mouse, cell),
            // A paste is text, not the keys that produce it, and a seat has no
            // text channel; a resize is the pane's shape rather than the
            // program's; focus follows the pane and not the window. None of them
            // is input this side can hand to a compositor.
            termputs::InputEvent::Paste(_)
            | termputs::InputEvent::Resize { .. }
            | termputs::InputEvent::FocusGained
            | termputs::InputEvent::FocusLost => Ok(()),
        }
    }

    /// Deliver one mouse event to the program.
    fn mouse(&self, mouse: &termputs::MouseEvent, cell: (u32, u32)) -> std::io::Result<()> {
        // The pointer moves first, always: every other kind of event happens *at*
        // a position, and a click that did not move first would land wherever the
        // pointer already was.
        //
        // The cell's centre is where it lands: a cell has area and a pointer has
        // none, so the middle is the only choice that is not a bias toward one
        // edge of the cell the user aimed at.
        let (cell_w, cell_h) = cell;
        let x = (mouse.column as f64 + 0.5) * cell_w as f64;
        let y = (mouse.row as f64 + 0.5) * cell_h as f64;
        // The pointer moves first, always: every other kind of event happens *at*
        // a position, and a press that did not move first would land wherever the
        // pointer already was.
        self.mouse_move(x, y)?;

        // A press and a release are sent as they are reported, not collapsed into
        // one click, because they are two different gestures. A click is a press
        // and a release with nothing in between; a **drag** is a press, motion
        // while the button is still held, and only then a release. Collapsing the
        // pair would make the button up before any drag motion arrived, so
        // selecting text with the mouse could never work.
        match mouse.kind {
            termputs::MouseKind::Down(button) => self.mouse_button(button, true),
            termputs::MouseKind::Up(button) => self.mouse_button(button, false),
            // Motion beyond the move above: a drag carries no button of its own
            // because the press already said which one is held.
            termputs::MouseKind::Scroll(direction) => self.mouse_scroll(direction),
            termputs::MouseKind::Drag(_) | termputs::MouseKind::Moved => Ok(()),
        }
    }

    /// Deliver one key event to the program.
    ///
    /// The key goes in as a **position**, not a character: the seat derives the
    /// character and the modifier state from the keymap it holds, so sending a
    /// position is what lets a modifier and a shifted key mean anything. That is
    /// also why the modifiers are pressed as their own keys first: a seat learns
    /// `ctrl` by being sent the control key, and there is no other way to ask.
    ///
    /// A key is sent as a whole press-and-release, and that is not a detail of
    /// style: an ordinary terminal reports presses only, with no byte for a
    /// release, so a press that waits for its own release is a key the program
    /// holds down forever. One event is therefore one complete keystroke -- the
    /// modifiers, the key down, the key up, the modifiers up.
    ///
    /// Repeat and release events are dropped for the same reason: on a terminal
    /// that does not send them they cannot arrive, and on one that does the
    /// keystroke above has already delivered the equivalent.
    pub fn key(&self, key: &termputs::Key, state: termputs::KeyState) -> std::io::Result<()> {
        if state == termputs::KeyState::Released {
            return Ok(());
        }

        // A repeat is a press of the same key, which is what a keystroke already
        // does.
        let Some(keycode) = key.keycode() else {
            return Ok(());
        };

        for modifier in key.modifiers.keycodes() {
            self.client
                .keycode(modifier.to_evdev(), compositor::KeyState::Pressed)
                .map_err(std::io::Error::other)?;
        }
        // The key itself, down and up. The release is what stops the program from
        // believing the key is still held, and it is why this cannot be a plain
        // press.
        self.client
            .type_keycode(keycode.to_evdev())
            .map_err(std::io::Error::other)?;
        for modifier in key.modifiers.keycodes().into_iter().rev() {
            self.client
                .keycode(modifier.to_evdev(), compositor::KeyState::Released)
                .map_err(std::io::Error::other)?;
        }
        Ok(())
    }

    /// Move the pointer, in **output pixels**: the compositor's pointer has no
    /// idea what a terminal cell is.
    pub fn mouse_move(&self, x: f64, y: f64) -> std::io::Result<()> {
        self.client.move_mouse(x, y).map_err(std::io::Error::other)
    }

    /// Press or release a pointer button at the pointer's current position.
    pub fn mouse_button(
        &self,
        button: termputs::Button,
        down: bool,
    ) -> std::io::Result<()> {
        let button = match button {
            termputs::Button::Left => compositor::Button::Left,
            termputs::Button::Right => compositor::Button::Right,
            termputs::Button::Middle => compositor::Button::Middle,
        };
        // The state goes through as reported, so a held button stays held: that
        // is what makes a drag a drag rather than a click and a stray move.
        self.client
            .button(button, down)
            .map_err(std::io::Error::other)
    }

    /// Scroll by one wheel step in `direction`.
    pub fn mouse_scroll(&self, direction: termputs::Scroll) -> std::io::Result<()> {
        // Positive vertical is down, which is what a terminal's scroll-down
        // means, so the direction maps straight across.
        let delta = match direction {
            termputs::Scroll::Up => compositor::ScrollDelta::vertical(-1),
            termputs::Scroll::Down => compositor::ScrollDelta::vertical(1),
            termputs::Scroll::Left => compositor::ScrollDelta {
                vertical: 0,
                horizontal: -1,
            },
            termputs::Scroll::Right => compositor::ScrollDelta {
                vertical: 0,
                horizontal: 1,
            },
        };
        self.client.scroll(delta).map_err(std::io::Error::other)
    }

    /// The next frame, as JPEG, or `None` when there is nothing new to send.
    ///
    /// `None` is not an end and not an error: a window that has not repainted has nothing
    /// to send, and re-sending the frame it already shows would spend the whole budget on
    /// identical bytes. A capture that fails is also `None`, and the caller asks again:
    /// one missed round-trip is not the end of the stream.
    ///
    /// Instrumented because this is the one function that knows what a frame cost:
    /// the capture is a round trip to the compositor's thread, the encode is the
    /// single largest cost in the budget, and neither is visible from outside.
    #[tracing::instrument(name = "app.frame", skip(self))]
    pub fn frame(&mut self) -> Option<Vec<u8>> {
        let frame = match self.client.capture() {
            Ok(frame) => frame,
            Err(e) => {
                log::debug!("capture did not complete: {e}");
                return None;
            }
        };

        let _compare = tracing::info_span!("compare").entered();
        if self.last.as_deref() == Some(frame.rgba.as_slice()) {
            return None;
        }
        drop(_compare);

        // The captured buffer is a Wayland `Argb8888` buffer copied byte for byte, whose
        // bytes are B, G, R, A on a little-endian machine. Encoded as `BGRX` so the
        // terminal shows the colours the program used, not a red/blue swap.
        let _encode = tracing::info_span!("encode").entered();
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
}
