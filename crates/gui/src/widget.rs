//! The pane the client draws, and the session it draws from.
//!
//! ```text
//!   widget.rs          this: the widget, the Session, and the fixed sockets
//!   widget/state.rs    where the session is, and the steps that move it along
//!   widget/popup.rs    what is open over it
//! ```

mod popup;
mod state;

pub use popup::PopupState;
pub use state::State;

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, StatefulWidget, Widget as _};

/// What the session holds while a step is running.
///
/// A step consumes the state it is given, so there has to be something in the session's
/// place for as long as it takes. This is that something, and it is a failure rather than
/// a state: if it is ever drawn, a step did not return the state it moved to, which is a
/// bug and reads as one.
const INTERNAL_ERROR: &str = "internal error: a step did not return its state";

/// The ways a session can be reached.
///
/// One per transport: what the client can dial, in the order the list offers them. Only
/// the first is built, and the rest are named here anyway so the choice a user is making is
/// visible as a choice rather than as a single button.
pub const CONNECTIONS: [&str; 4] = [
    "Unix Socket",
    "SSH Socket",
    "Peer to Peer",
    "Reverse SSH",
];

/// The socket a Unix Socket session dials.
///
/// One fixed name, so a user and a server can both write it down without a scan, and it is
/// the server's to bind.
///
/// Under `XDG_RUNTIME_DIR` and not `/tmp`: a socket in `/tmp` is reachable by every user on
/// the machine, and this is the platform's directory for exactly this.
pub fn socket_path() -> std::path::PathBuf {
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    std::path::Path::new(&dir).join("tuigui.sock")
}

/// The picture's anchor, filled across the display area.
///
/// The server replaces every one of these with an image placeholder, so the fill is not
/// decoration: it is the address of the picture, and this is the only thing the terminal
/// has to be told for the image to have somewhere to be.
pub const FILL: char = '❋';

/// This terminal's input, boxed so a caller may hand in its own.
///
/// The session holds one of these for its whole life, and `Gui::run` is generic
/// over what it was given, so the two handles are boxed rather than named.
///
/// The input is `termputs`'s trait rather than plain `Read`: the reader has to
/// wait on the same descriptor it reads, and a box of `Read` alone has no
/// descriptor to wait on.
pub type Input = Box<dyn termputs::Input>;
/// This terminal's output, boxed for the same reason.
pub type Output = Box<dyn std::io::Write + Send>;

/// One session: where it is, and what is open over it.
pub struct Session {
    /// Where the session is, or what went wrong getting there.
    ///
    /// A `Result` and not a state, because a step consumes the state it is given: there
    /// has to be something in its place while the step runs, and a failure is the only
    /// honest thing to put there. A step that never returns its next state leaves the
    /// failure behind, and the pane shows it.
    pub state: Result<State, String>,
    /// What is open over it.
    pub popup_state: PopupState,
    /// The buttons along the bottom.
    pub footer: Footer,
    /// The program to run, once a socket has been dialed.
    pub program: std::path::PathBuf,
    /// The compositor's output size, which is also the program's window size.
    ///
    /// Fixed when the session starts: the compositor's output being the window is what
    /// the MVP fixes, so this is read once and reused for every later step.
    pub size: (u32, u32),
    /// This terminal's input, with the mouse and keyboard protocols requested.
    ///
    /// Kept for the whole session and not per frame: it owns the requests that
    /// turn reporting on, so dropping it between frames would turn a terminal's
    /// mouse reporting off and on again. The `Capture`s it hands out are the
    /// disposable part.
    pub input_listener: termputs::Listener<Input, Output>,
    /// What one terminal cell is in output pixels.
    ///
    /// A mouse event arrives in cells and the compositor's pointer is in pixels,
    /// so this is the conversion, and it is the same cell size `output_size`
    /// derived the output from.
    pub cell: (u32, u32),
}

impl Session {
    /// Start a session over a terminal's input.
    ///
    /// The listener is made here, not later, because making it is what asks the
    /// terminal for its mouse and keyboard events: a session that waited until
    /// it was streaming would miss everything typed while it connected.
    pub fn new(
        program: std::path::PathBuf,
        size: (u32, u32),
        cell: (u32, u32),
        input: termputs::Listener<Input, Output>,
    ) -> Self {
        Self {
            state: Ok(State::ConnSelect),
            popup_state: PopupState::None,
            footer: Footer,
            program,
            size,
            cell,
            input_listener: input,
        }
    }

    /// Whether the session has ended.
    pub fn done(&self) -> bool {
        match &self.state {
            // Failed, or finished: either way there is nothing left to do.
            Err(_) => true,
            Ok(state) => state.done(),
        }
    }
}

/// The client itself: a widget over [`Session`].
pub struct GuiWidget;

impl StatefulWidget for GuiWidget {
    type State = Session;

    fn render(self, area: Rect, buf: &mut Buffer, session: &mut Session) {
        // The bottom line holds the buttons, and the rest belongs to the session. Split
        // once, so the state and the buttons agree about where the content ends.
        let [content, footer] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area);

        // Where the session is may change because of drawing this frame: the async step is
        // driven here, and it draws itself where it stands.
        //
        // The state is taken out and the result put back, which is why the session holds a
        // `Result`: what goes in its place while the step runs is the failure a state that
        // never comes back deserves, and a step that does not return leaves exactly that
        // on screen for the next frame to draw.
        let taken = std::mem::replace(&mut session.state, Err(INTERNAL_ERROR.to_string()));
        session.state = taken.and_then(|state| {
            // Driven by the **tokio** runtime, not by a bare block-on, and that is the
            // point: the step awaits tokio IO and tokio timers, and a runtime must be
            // polled for either to make progress. `pollster::block_on` drives the future
            // itself and nothing else, so a timer registered with the runtime never
            // expires and a socket does not wake this thread when it becomes writable --
            // the future returns `Pending`, nobody unparks, and the thread spins forever
            // on a step that will never finish.
            //
            // `block_in_place` first, because this *is* a runtime worker thread and the
            // draw is synchronous: it tells the runtime to move other tasks elsewhere
            // while this one blocks, which is what makes blocking here safe.
            let result = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(state.render(content, buf, session))
            });
            result
        });

        // Whether the step failed just now or failed on an earlier frame, the pane says
        // why. This is the one place a failure is drawn: a step hands its failure back
        // rather than drawing it, because the pane is this widget's and not the step's.
        //
        // Cleared first, because centred text only writes the cells it sits on: whatever
        // the frame before this one drew — the advert, "connecting to ..." — would still be
        // there around it, and two states' words on one screen read as neither.
        if let Err(message) = &session.state {
            log::error!("{message}");
            ratatui::widgets::Clear.render(content, buf);
            buf.set_style(content, Style::new().bg(Color::Rgb(0x40, 0, 0)));
            let lines: Vec<Line> = std::iter::once(Line::from("the session failed"))
                .chain(message.lines().map(Line::from))
                .collect();
            Paragraph::new(lines).centered().render(content, buf);
        }

        // The footer, then the panel over the session, never instead of it.
        session.footer.render(footer, buf, session.popup_state);
        session.popup_state.render(content, buf);
    }
}

/// The line of buttons along the bottom.
///
/// Its own struct with a `render` rather than a loop inside the widget, because this line
/// is the one part of the pane that is the same in every state: what pressing one into
/// opens is a panel, which is not the session's business.
pub struct Footer;

impl Footer {
    /// Draw the buttons on `area`, marking whichever panel is open.
    pub fn render(&mut self, area: Rect, buf: &mut Buffer, open: PopupState) {
        buf.set_style(area, Style::new().bg(Color::Rgb(0x28, 0x28, 0x28)));
        let mut x = 1u16;
        for (index, label) in popup::BUTTONS.iter().enumerate() {
            let style = if open.is_open_for(index) {
                Style::new().fg(Color::Black).bg(Color::Rgb(0xff, 0x69, 0xb4))
            } else {
                Style::new().fg(Color::Gray)
            };
            buf.set_string(area.x + x, area.y, format!(" {label} "), style);
            x += label.len() as u16 + 3;
        }
    }
}
