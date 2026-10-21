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

    /// Deliver one key event to the program.
    ///
    /// The key goes in as a **position**, not a character: the seat derives the
    /// character and the modifier state from the keymap it holds, so sending a
    /// position is what lets a modifier and a shifted key mean anything. That is
    /// also why the modifiers are pressed as their own keys first: a seat learns
    /// `ctrl` by being sent the control key, and there is no other way to ask.
    pub fn key(&self, key: &termputs::Key, state: termputs::KeyState) -> std::io::Result<()> {
        let compositor_state = match state {
            termputs::KeyState::Pressed | termputs::KeyState::Repeated => {
                compositor::KeyState::Pressed
            }
            termputs::KeyState::Released => compositor::KeyState::Released,
        };

        // Modifiers only go down on a press. Releasing them on the way out would
        // fight the sequence: the caller's release of `c` is not a release of
        // the control key that is still held.
        if state == termputs::KeyState::Pressed {
            for modifier in key.modifiers.keycodes() {
                self.client
                    .keycode(modifier.to_evdev(), compositor::KeyState::Pressed)
                    .map_err(std::io::Error::other)?;
            }
        }

        let Some(keycode) = key.keycode() else {
            // A key with no position: the modifier keys above have already gone
            // down, so they are put back up rather than left held.
            return self.release_modifiers(key);
        };
        self.client
            .keycode(keycode.to_evdev(), compositor_state)
            .map_err(std::io::Error::other)?;

        if state == termputs::KeyState::Released {
            return self.release_modifiers(key);
        }
        Ok(())
    }

    /// Raise the modifiers a key was holding.
    fn release_modifiers(&self, key: &termputs::Key) -> std::io::Result<()> {
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
        // The compositor's `click` is a press and release in one call, so a
        // release has nothing to do: the button is already up.
        if !down {
            return Ok(());
        }
        let button = match button {
            termputs::Button::Left => compositor::Button::Left,
            termputs::Button::Right => compositor::Button::Right,
            termputs::Button::Middle => compositor::Button::Middle,
        };
        self.client.click(button).map_err(std::io::Error::other)
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
}
