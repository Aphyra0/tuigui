//! The windowing vocabulary: what a session is, what a window is, and the
//! input a window takes.
//!
//! A **session** runs one program and gives back what it draws. Its windows
//! form a tree: the root window, and the windows the program opened after it,
//! each opening more. The backends live beside this module — [`wayland`] and
//! [`dummy`] — and this module is the traits they implement, plus the state
//! the compositor keeps about a window.
//!
//! Deliberately absent: keycodes and button codes. Those are per-platform
//! (evdev on Linux, a virtual keycode on macOS), so the vocabulary here is
//! logical — a key, a button, a wheel step — and the backend owns the mapping.

pub mod dummy;
pub mod event_map;
pub mod wayland;

use std::sync::Weak;

use ratatui::layout::Rect;
use ratatui::widgets::WidgetRef;

/// Where a window is and how big it is, in cells, and the cells it draws into.
///
/// The rectangle is on the pane, whose origin is `(0, 0)`, so it is already the
/// screen rectangle. The surface is the window's identity: what a frame is
/// addressed by, and what the other side scans the pane for. Nothing here is the
/// pointer's business: which gesture is in flight belongs to the compositor, in
/// [`InputMode`](super::window::InputMode).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WindowState {
    /// The window's rectangle on the pane, in cells.
    pub area: Rect,
    /// The cells this window's picture goes into.
    pub surface: wire::message::SurfaceId,
}

/// The window session as a whole: what a program runs in.
///
/// A server owns one session: it starts the program, steps it forward, and
/// offers the windows it produced. The holder works through this trait, so a
/// backend with no real program behind it — a debug one that only draws — is
/// as good a session as a wayland one.
pub trait WindowingServer {
    /// The root window: the program's main one, as a weak handle. `None`
    /// while it has not been built; the session dropping the window is what
    /// kills the handle.
    fn root_window(&self) -> Option<Weak<dyn StatefulWindow>>;

    /// Start the session's program. A session whose program is not its own to
    /// start — the dummy's, or a session attached to one already running —
    /// starts nothing.
    fn spawn(&mut self, _path: &std::path::Path) -> std::io::Result<()> {
        Ok(())
    }

    /// One turn of the session: accept what arrived, dispatch what the
    /// program did, and flush back down the sockets. A dummy session moves
    /// forward by being drawn instead, so its turn is nothing.
    fn advance(&mut self, _timeout: std::time::Duration) {}

    /// Whether the session's program has connected. False while nobody has
    /// said otherwise.
    fn is_ready(&self) -> bool {
        false
    }

    /// Whether the session's program is still running: the fact that ends a
    /// session.
    fn is_running(&self) -> bool {
        true
    }

    /// Resize the session: the size every window is measured against.
    fn set_size(&mut self, size: (u32, u32));
}

impl WindowingServer for Box<dyn WindowingServer> {
    fn root_window(&self) -> Option<Weak<dyn StatefulWindow>> {
        (**self).root_window()
    }

    fn spawn(&mut self, path: &std::path::Path) -> std::io::Result<()> {
        (**self).spawn(path)
    }

    fn advance(&mut self, timeout: std::time::Duration) {
        (**self).advance(timeout)
    }

    fn is_ready(&self) -> bool {
        (**self).is_ready()
    }

    fn is_running(&self) -> bool {
        (**self).is_running()
    }

    fn set_size(&mut self, size: (u32, u32)) {
        (**self).set_size(size);
    }
}

/// One admitted window: what it draws, where it sits, and what may be pushed
/// into it.
///
/// A window keeps its own state, so a later read reads where the compositor
/// put it. Children are handed out as weak handles, so the compositor can
/// keep a registry of what it has seen and prune what died.
pub trait StatefulWindow: WidgetRef + Send + Sync {
    /// Queue one key event for delivery to this window.
    ///
    /// Keys and pointer events arrive through separate methods because UI Events
    /// keeps them apart, and a seat depends on it: a modified key is a sequence
    /// of key events that only means something in order, while a pointer event
    /// carries a whole state and must not be interleaved into that sequence.
    fn key_input(&self, event: ui_events::keyboard::KeyboardEvent) -> Result<(), Error>;

    /// Queue one pointer event for delivery to this window.
    fn pointer_input(&self, event: ui_events::pointer::PointerEvent) -> Result<(), Error>;

    /// What the window last drew, encoded and sent: one frame, addressed by the
    /// cells it draws into. `NotReady` until it has drawn at least once.
    ///
    /// Encoding is the window's own business because the stream is: an encoder's
    /// state is the pictures it has already sent, so it belongs beside the pixels
    /// it encodes.
    fn capture(&self, link: &wire::transport::Connection) -> Result<(), Error>;

    /// Ask this window to draw at `size` pixels instead of its current one.
    ///
    /// The client decides whether to follow: its next commit is the answer.
    fn set_size(&self, size: (u32, u32));

    // TODO: use get_state -> &mut State instead
    /// The window's state: its rectangle on the pane.
    fn state(&self) -> WindowState;

    /// Put the window somewhere else, or make it another size.
    fn set_state(&self, state: WindowState);

    /// Every window this one opened after it, as weak handles. Always all of
    /// them: the caller keeps what it has not seen before — told apart by the
    /// pointer, not by bookkeeping — and prunes the ones that died.
    fn get_children(&self) -> Vec<Weak<dyn StatefulWindow>>;

    /// What the window calls itself: the title its program set, or the name of
    /// the program when it set none. Shown on the bar, and nowhere else.
    fn title(&self) -> String;

    /// Ask the window to close, the way a program's own quit is asked for: the
    /// client is told, and it decides. A window whose program ignores it stays.
    fn terminate(&self);
}

/// Why a window could not do what was asked of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The client that owned the window is gone, so nothing is listening.
    Gone,
    /// The window exists but has not drawn yet: there is nothing to capture.
    NotReady,
    /// The codec could not be built for this window's size.
    Build(String),
    /// The codec refused the frame.
    Encode(String),
    /// The frame was made and the link would not take it.
    Send(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Gone => f.write_str("the window is gone"),
            Error::NotReady => f.write_str("the window has not drawn yet"),
            Error::Build(e) => write!(f, "the codec could not be built: {e}"),
            Error::Encode(e) => write!(f, "the codec refused the frame: {e}"),
            Error::Send(e) => write!(f, "the frame could not be sent: {e}"),
        }
    }
}

impl std::error::Error for Error {}

/// How long one message is given to leave this side.
///
/// A message is a write to another process, which may be busy with the one
/// before it. Bounded so a link that stopped draining is reported rather than
/// waited on: without it the frame stalls with the message in hand and says
/// nothing, which reads as a frozen pane.
pub const SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Write one message to the link from a draw, which is synchronous.
///
/// The blocking is what `block_in_place` makes safe: the draw runs on a runtime
/// worker, and that call is the one that says so. Sending here rather than
/// pushing the write onto the runtime keeps messages in the order the frames
/// that produced them were made.
///
/// FIXME: this should go. A draw that blocks on a socket is a draw waiting for
/// another process, and the frame loop should hand its messages over instead of
/// writing them itself.
pub fn send(
    link: &wire::transport::Connection,
    message: &wire::message::Message,
) -> Result<(), Error> {
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            tokio::time::timeout(SEND_TIMEOUT, link.send(message))
                .await
                .map_err(|_| Error::Send(format!("the link took nothing within {SEND_TIMEOUT:?}")))?
                .map_err(|e| Error::Send(e.to_string()))
        })
    })
}

/// Delivering input is the only failure this module hands back as an I/O error:
/// the caller writes it to the pane's log beside the reads and writes it sits
/// between, so a window that took an event and could not pass it on becomes one
/// error rather than a stdout-shaped one at every call site.
impl From<Error> for std::io::Error {
    fn from(error: Error) -> Self {
        std::io::Error::other(error)
    }
}
