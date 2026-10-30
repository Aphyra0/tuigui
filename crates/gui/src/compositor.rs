//! The programs running in a compositor of ours, drawn into the pane as windows.
//!
//! The video path decodes a file; this one runs programs. Each program gets a
//! Wayland session of its own — a compositor in this process, one output — and
//! the pixels it commits are read straight out of that compositor's buffer.
//!
//! # What is whose
//!
//! The compositor holds the **session** ([`WindowingServer`]): the program and
//! its windows. It also holds the windows it has seen, in stacking order — the
//! session's root first and therefore at the bottom, the frontmost last — and the
//! one with focus.
//!
//! What a window *is* — where it sits, what its chrome looks like, what a mouse
//! event does to it — is [`widgets::decorated_window`]'s business. This module is
//! only the stack: which window is in front, which one has the keyboard, and the
//! pass that offers an event to each of them in turn.

use std::sync::{Arc, Weak};

use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::Color;
use ratatui::widgets::Widget;

use crate::DebugOptions;
use widgets::decorated_window::DecoratedWindow;

pub mod widgets;
pub mod windowing;

pub use windowing::{StatefulWindow, WindowState, WindowingServer};

/// The pointer, as the pane draws it.
///
/// An arrow, because a pointer is looked for rather than read: it sits on top of whatever
/// window is under it, and the glyph only has to be findable at a glance.
const CURSOR: char = '➤';

/// A program, or several, running in a compositor of ours.
///
/// `T` is the window session: a real one (wayland, later macOS) or the dummy
/// one (`--dbg-dummy-windowing`), which draws windows nobody is running.
pub struct Compositor<T: WindowingServer> {
    /// The session the windows live in. Dropping it ends them.
    windowing: T,
    /// Every window, in stacking order: the session's root first, so it is at the
    /// bottom of the pane, and the windows above it in the order they were raised,
    /// so the frontmost is last. Each one carries its own chrome and the drag it
    /// is in the middle of, so an entry outlives the frame that drew it.
    ///
    /// Bottom first is what makes an input event one back-to-front walk: the
    /// frontmost window is the last of the vector, and the root — which fills the
    /// pane — is the first.
    windows: Vec<DecoratedWindow>,
    /// The window a key belongs to.
    focused: Option<Weak<dyn StatefulWindow>>,
    /// The switches this pane runs with (`--dbg-*`).
    debug: Arc<DebugOptions>,
    /// The pane's rectangle as of the last render, in cells.
    pane: Rect,
    /// Where the pointer last was, in cells of the pane.
    ///
    /// Kept because it is asked for at a different time from the event that says it: a
    /// mouse event arrives between frames and the pane is drawn once per frame, so the
    /// cell has to outlive the event to be drawn. `None` until the pointer has moved,
    /// which is a pane with no pointer on it yet rather than one at the origin.
    pointer: Option<Position>,
}

impl<T: WindowingServer> Compositor<T> {
    /// Take a session and the switches, and start compositing.
    pub fn new(windowing: T, debug: Arc<DebugOptions>) -> Self {
        Self {
            windowing,
            windows: Vec::new(),
            focused: None,
            debug,
            pane: Rect::ZERO,
            pointer: None,
        }
    }

    /// Whether the session's program has ended.
    ///
    /// A program that has not connected yet has not ended: the session is
    /// still waiting, which is what `is_running` answers.
    pub fn finished(&self) -> bool {
        !self.windowing.is_running()
    }

    /// Hand one event to the pane.
    ///
    /// Keys go to the focused window, or to the root until something is focused:
    /// the pane is the program, and a key reaches the program first. A mouse event
    /// is offered to the windows from the front, and the first one that takes it
    /// ends the walk — see [`DecoratedWindow::handle_mouse`].
    pub fn input(
        &mut self,
        event: &termina::Event,
        winsize: rustix::termios::Winsize,
    ) -> std::io::Result<()> {
        match event {
            termina::Event::Key(key) => {
                // The window being typed into, or the root when nothing has been
                // focused yet: the root is the program, so a key pressed before any
                // window was chosen is a key for the program itself.
                if let Some(window) = self.focused_window().or_else(|| self.root_window()) {
                    window.key_input(windowing::event_map::key(key))?;
                }
            }
            termina::Event::Mouse(mouse) => {
                let pane = self.pane;
                // Where the pointer is, in cells of the pane, kept for the draw that
                // comes after this event: the cursor is painted last, over everything.
                self.pointer = Some(Position::new(mouse.column, mouse.row));
                // Back to front: the frontmost window is the last of the vector,
                // so the walk finds the one nearest the user first. The root sits
                // at index 0 and fills the pane, so it is found when no window
                // above it wanted the event.
                let Some(focus) = self
                    .windows
                    .iter_mut()
                    .rposition(|window| window.handle_mouse(*mouse, pane, winsize).is_none())
                else {
                    return Ok(());
                };
                // What claimed the event is what the keyboard talks to, the root
                // included: the root is the pane, so over the pane is over the
                // program. This is set before the raise below because the two are
                // different decisions, and the root takes focus without ever coming up.
                self.focused = Some(Arc::downgrade(&self.windows[focus].window));
                // Index 0 is the root, which is the pane itself: raising it would
                // cover its own windows, so it stays where it is. Anything else
                // comes forward.
                if focus == 0 {
                    return Ok(());
                }

                let window = self.windows.remove(focus);
                self.windows.push(window);
            }
            _ => {}
        }
        Ok(())
    }

    /// One frame of the session: step it forward, catch the stack up with what
    /// it offers now, and send every window's picture down `link`.
    ///
    /// Stepping first is what makes the captures worth taking: the pixels sent
    /// here are the ones the programs committed during this step.
    ///
    /// `winsize` is the terminal's own report of itself, read once per frame by the
    /// caller: it is the only place the pane's size in **pixels** comes from, and
    /// a terminal may change it without changing its grid — a font that grew is a
    /// bigger picture of the same number of cells.
    pub fn frame(
        &mut self,
        link: &wire::transport::Connection,
        timeout: std::time::Duration,
        winsize: rustix::termios::Winsize,
    ) {
        self.windowing.advance(timeout);

        let Some(root) = self.windowing.root_window().and_then(|root| root.upgrade()) else {
            return;
        };
        self.maybe_resize_root(&root, winsize);
        self.sync(root);

        // Each window encodes its own picture: the pixels are that window's, and
        // so is the encoder, which is the pictures it has already sent. A window
        // that has not drawn has nothing to send, which is not a failure — the
        // next frame that commits carries a picture.
        for window in &self.windows {
            match window.window.capture(link) {
                Ok(()) | Err(windowing::Error::NotReady) => {}
                Err(e) => log::warn!("a frame was not sent: {e}"),
            }
            // After the capture, never before it: a configure changes the size the
            // window's next picture is expected at, so sending it first would leave the
            // frame just encoded labelled with a size it does not have.
            window.maybe_set_size(winsize);
        }
    }

    /// The root is the pane, so a change of pane is a change of the root window's
    /// rectangle on the pane, and a change of the terminal's shape is a change of
    /// the session's output. The root takes the pane as its own, top left: it is
    /// the one window that fills the screen, and everything else is somewhere on
    /// it.
    ///
    /// The program is configured in **pixels**, which is what the terminal reports
    /// and not what the grid says: `ws_xpixel`/`ws_ypixel` are the whole pane in
    /// pixels, and the cells merely say where that picture goes. Configuring the
    /// program at the cell count instead gives it a window of `80x24` pixels for
    /// the pane to stretch over `80x24` cells.
    fn maybe_resize_root(
        &mut self,
        root: &Arc<dyn StatefulWindow>,
        winsize: rustix::termios::Winsize,
    ) {
        let pixels = (u32::from(winsize.ws_xpixel), u32::from(winsize.ws_ypixel));
        // A terminal that reports no pixels is one that cannot say, and it is not
        // an error: the grid is still known, so the program is configured at one
        // pixel a cell. That is the smallest picture that keeps the layout
        // honest, and the pane stretches it to whatever the terminal really is.
        let pixels = match pixels {
            (0, _) | (_, 0) => (
                u32::from(winsize.ws_col).max(1),
                u32::from(winsize.ws_row).max(1),
            ),
            _ => pixels,
        };

        let was = root.state().area;
        if was != self.pane {
            // Worth a line every time it happens: the root is what the session
            // hears about its own size, and a root that disagrees with the pane is
            // a window drawn in the wrong place with nothing to show for it.
            log::info!(
                "compositor: the pane is {:?} cells ({pixels:?} pixels), so the root goes from {:?} to it",
                self.pane,
                was
            );
            root.set_state(WindowState {
                area: self.pane,
                ..root.state()
            });
        }

        // The output is told the size the pane is, every frame: a backend that is
        // already that size does nothing, so an untouched terminal costs no
        // configure, and a terminal that changed without changing its grid is
        // still caught.
        root.set_size(pixels);
        self.windowing.set_size(pixels);
    }

    /// The window a key belongs to when nothing is focused: the root, which is
    /// the program itself and sits at the bottom of the stack.
    pub(crate) fn root_window(&self) -> Option<Arc<dyn StatefulWindow>> {
        let root = self.windows.first()?;
        Some(root.window.clone())
    }

    /// The window a key belongs to, if it is still alive.
    pub(crate) fn focused_window(&self) -> Option<Arc<dyn StatefulWindow>> {
        self.focused.as_ref().and_then(|focused| focused.upgrade())
    }

    /// Bring the stack up to date with the session: drop what died, add what is
    /// new, keep everything else as it is, chrome and drag included.
    fn sync(&mut self, root: Arc<dyn StatefulWindow>) {
        // Bottom first, which is the order the stack is kept in: the root, then
        // everything under it, each window after the one it was opened from.
        let mut offered = vec![root.clone()];
        offered.extend(collect(&root));

        // A weak handle's only identity is the pointer, so that is how two reads
        // of one window are told apart. What the retain keeps is what the stack
        // already had; what it takes out of `offered` is what the stack has yet to
        // see, which is all that is left to add.
        self.windows.retain(|window| {
            let Some(seen) = offered
                .iter()
                .position(|new| Arc::ptr_eq(new, &window.window))
            else {
                return false;
            };

            offered.remove(seen);
            true
        });

        // Whatever is left is new, and goes on top, in the order it was offered:
        // the root first if it is the one missing, which leaves it at index 0.
        //
        for window in offered {
            let root = Arc::ptr_eq(&window, &root);
            // A window appearing is the event worth a line: what it is, where it
            // was put, and whether it wears chrome.
            log::info!(
                "compositor: a window arrived, {} at {:?}",
                if root { "the root" } else { "opened" },
                window.state().area
            );
            // New windows get focus: a window that just appeared is the one the user
            // asked for by opening it, and so the one they are about to type into.
            self.focused = Some(Arc::downgrade(&window));
            self.windows.push(DecoratedWindow::new(window, root));
        }

        if self.windows.is_empty() {
            log::warn!("compositor: the stack is empty, nothing to draw");
        }
    }
}

/// The compositor as the pane draws it: every window, bottom of the stack first.
impl<T: WindowingServer> Widget for &mut Compositor<T> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // The pane is what every window is placed against, so a change of pane is
        // worth a line: a frame drawn at a size nobody expected looks like a window
        // in the wrong place, and this says whether the pane itself moved.
        if self.pane != area {
            log::info!("compositor: the pane is {area:?}, was {:?}", self.pane);
        }
        self.pane = area;
        for window in self.windows.iter_mut() {
            let window_area = window.window.state().area;
            window.render(window_area, buf);
        }
        // The pointer last, over every window: it belongs to the pane rather than to any
        // one of them, and a window drawn after it would cover it.
        //
        // The glyph carries the pane's own anchor colours nowhere: the other side finds
        // a window's picture by scanning for its surface's exact glyph and colours, so a
        // pointer that matched one would be read as a stray cell of that window.
        if let Some(pointer) = self.pointer {
            if area.contains(pointer) {
                if let Some(cell) = buf.cell_mut(pointer) {
                    cell.set_char(CURSOR)
                        .set_fg(Color::Rgb(0xff, 0xff, 0xff))
                        .set_bg(Color::Rgb(0, 0, 0));
                }
            }
        }
    }
}

/// Walk a window's children into `out`, each before its own: a window is
/// drawn under the windows it opened.
fn collect(window: &Arc<dyn StatefulWindow>) -> Vec<Arc<dyn StatefulWindow>> {
    // The smallest a window may be, in cells: below this the chrome alone would
    // take more room than the window has.
    const MIN: (u16, u16) = (10, 10);

    let mut out = Vec::new();
    // A dummy session opens windows at a fraction of its own, so a window opened
    // late is smaller than the chrome can be drawn in: it is grown to the floor,
    // and its next read says so.
    let area = window.state().area;
    if area.width < MIN.0 || area.height < MIN.1 {
        window.set_state(WindowState {
            area: Rect::new(
                area.x,
                area.y,
                area.width.max(MIN.0),
                area.height.max(MIN.1),
            ),
            ..window.state()
        });
    }
    for child in window.get_children() {
        if let Some(child) = child.upgrade() {
            if out.iter().any(|known| Arc::ptr_eq(known, &child)) {
                continue;
            }
            out.extend(collect(&child));
            out.push(child);
        }
    }
    out
}
