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
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::DebugOptions;
use widgets::decorated_window::DecoratedWindow;

pub mod widgets;
pub mod windowing;

pub use windowing::{StatefulWindow, WindowState, WindowingServer};

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
    pub fn input(&mut self, event: &termina::Event) -> std::io::Result<()> {
        match event {
            termina::Event::Key(key) => {
                if let Some(window) = self.focused_window().or_else(|| self.root_window()) {
                    window.key_input(windowing::event_map::key(key))?;
                }
            }
            termina::Event::Mouse(mouse) => {
                let pane = self.pane;
                // Back to front: the frontmost window is the last of the vector,
                // so the walk finds the one nearest the user first. The root sits
                // at index 0 and fills the pane, so it is found when no window
                // above it wanted the event.
                let Some(focus) = self.windows.iter_mut().rposition(|window| {
                    window.handle_mouse(*mouse, pane).is_none()
                        && matches!(mouse.kind, termina::event::MouseEventKind::Down(_))
                }) else {
                    return Ok(());
                };
                // Index 0 is the root, which is the pane itself: raising it would
                // cover its own windows, so it stays where it is. Anything else
                // comes forward, and the root only answers for the pane.
                if focus == 0 {
                    return Ok(());
                }

                let window = self.windows.remove(focus);
                self.focused = Some(Arc::downgrade(&window.window));
                self.windows.push(window);
            }
            _ => {}
        }
        Ok(())
    }

    /// One session turn, advanced: accept what arrived, dispatch what the
    /// program did, then bring the stack up to date with what it offers now. A
    /// dummy session moves forward by being drawn instead.
    pub fn advance(&mut self, timeout: std::time::Duration) {
        self.windowing.advance(timeout);

        let Some(root) = self.windowing.root_window().and_then(|root| root.upgrade()) else {
            return;
        };
        self.maybe_resize_root(&root);
        self.sync(root);
    }

    /// The root is the pane, so a change of pane is a change of the root window's
    /// size. The root takes the pane as its own rectangle, top left: it is the one
    /// window that fills the screen, and everything else is somewhere on it.
    fn maybe_resize_root(&mut self, root: &Arc<dyn StatefulWindow>) {
        if root.state().area != self.pane {
            root.set_state(WindowState { area: self.pane });
            root.set_size((self.pane.width as u32, self.pane.height as u32));
            self.windowing
                .set_size((self.pane.width as u32, self.pane.height as u32));
        }
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
        for window in offered {
            let root = Arc::ptr_eq(&window, &root);
            self.windows.push(DecoratedWindow::new(window, root));
        }
    }
}

/// The compositor as the pane draws it: every window, bottom of the stack first.
impl<T: WindowingServer> Widget for &mut Compositor<T> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        self.pane = area;
        for window in self.windows.iter_mut() {
            let window_area = window.window.state().area;
            window.render(window_area, buf);
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
