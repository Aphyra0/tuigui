//! Per-window bookkeeping: the window tree, committed buffers, damage, and
//! the input mailbox.
//!
//! The session keeps a **tree**: each client has a root window, and the
//! toplevels its program spawns afterwards are that window's children.
//! Detaching a window detaches its subtree with it. The holder (the
//! compositor side of the gui) attaches its own per-window data — compositing
//! metadata, layout, whatever it needs — through the generic `W`;
//! the session itself uses `Window<()>`.
//!
//! One struct, one owner: the session's state. Fields are `pub(crate)`; the
//! derived getters and setters mark what is exposed beyond the crate.

use std::sync::mpsc;
use std::sync::Mutex;

use derive_getters::Getters;
use derive_setters::Setters;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Rectangle};

use crate::{Button, CompError, Frame, KeyState, Rect, ScrollDelta};

/// One queued input event, in compositor shapes: keycodes are evdev codes,
/// positions are output pixels. What happens to a drained queue is decided
/// later; today the window is only the mailbox.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Input {
    Key { evdev: u32, state: KeyState },
    MouseMove { x: f64, y: f64 },
    Button { button: Button, down: bool },
    Scroll { delta: ScrollDelta },
}

/// One admitted window: its surface, its latest committed contents, its
/// damage, its input mailbox, and the toplevels its program spawned.
#[derive(Getters, Setters)]
#[setters(generate = false)]
pub struct Window<W = ()> {
    /// The admitted surface (an xdg toplevel's, or an X11 window's wl_surface).
    pub(crate) surface: WlSurface,
    /// The client-facing window size (the session's resolution).
    pub(crate) size: (u32, u32),
    /// Set once this window committed a readable buffer at least once.
    pub(crate) committed: bool,
    /// The surface's own coordinate space == output space (scale 1, offset 0).
    /// Set by the holder when compositing.
    #[setters(generate, borrow_self)]
    pub(crate) geometry: Rectangle<i32, Logical>,
    /// Latest committed contents, owned by us, written on commit.
    pub(crate) pixels: Mutex<Vec<u8>>,
    /// Damage since the last capture (output pixel space).
    pub(crate) damage: Mutex<Vec<Rect>>,
    /// Serial of the last configure sent (spawn waits for the first commit).
    pub(crate) last_configure_serial: Mutex<Option<u64>>,
    /// Toplevels this window's program spawned after being admitted. Detaching
    /// this window detaches its subtree with it.
    pub(crate) children: Vec<Window<W>>,
    /// The input mailbox's push side, and the only deliberately cloneable
    /// field: a handle pushes here without touching the window.
    pub(crate) inputs: mpsc::Sender<Input>,
    /// The mailbox's receiving side, drained by the session at the next step.
    pub(crate) pending_inputs: mpsc::Receiver<Input>,
    /// Whatever the holder attaches: compositing metadata and the like.
    pub attached: W,
}

impl<W> Window<W> {
    pub fn new(surface: WlSurface, size: (u32, u32), attached: W) -> Self {
        let (inputs, pending_inputs) = mpsc::channel();
        Window {
            surface,
            size,
            committed: false,
            geometry: Rectangle::from_size(smithay::utils::Size::from((
                size.0 as i32,
                size.1 as i32,
            ))),
            pixels: Mutex::new(Vec::new()),
            damage: Mutex::new(Vec::new()),
            last_configure_serial: Mutex::new(None),
            children: Vec::new(),
            inputs,
            pending_inputs,
            attached,
        }
    }

    /// Push one input event onto this window's mailbox. Fails only when the
    /// window is gone.
    pub fn input(&self, input: Input) -> Result<(), CompError> {
        self.inputs.send(input).map_err(|_| CompError::WindowGone)
    }

    /// The window's contents as RGBA8 + the damage since the last capture.
    /// The session overlays the cursor before handing the frame out; this
    /// method is the raw read.
    #[tracing::instrument(name = "window.capture", skip(self))]
    pub fn capture(&self) -> Result<Frame, CompError> {
        let pixels = self.pixels.lock().unwrap().clone();
        if pixels.is_empty() || !self.committed {
            return Err(CompError::NotReady);
        }
        let damage = std::mem::take(&mut *self.damage.lock().unwrap());
        Ok(Frame {
            width: self.size.0,
            height: self.size.1,
            rgba: pixels,
            damage,
        })
    }

    /// Drain this window's input mailbox.
    pub(crate) fn take_inputs(&self) -> Vec<Input> {
        self.pending_inputs.try_iter().collect()
    }

    /// The window whose surface is `surface`, or whose surface is the parent
    /// of it (a subsurface commit lands on the toplevel that owns the tree).
    pub(crate) fn commit_target(&mut self, surface: &WlSurface) -> Option<&mut Window<W>> {
        let is_win = self.surface == *surface;
        let is_kid = !is_win
            && smithay::wayland::compositor::get_parent(surface)
                .map(|p| p == self.surface)
                .unwrap_or(false);
        if is_win || is_kid {
            return Some(self);
        }
        for child in &mut self.children {
            if let Some(found) = child.commit_target(surface) {
                return Some(found);
            }
        }
        None
    }

    /// Remove the window whose surface is `surface` (a child, or a child's
    /// child). Returns whether something was removed.
    pub(crate) fn remove_surface(&mut self, surface: &WlSurface) -> bool {
        let before = self.children.len();
        self.children.retain(|c| c.surface != *surface);
        if self.children.len() != before {
            return true;
        }
        self.children.iter_mut().any(|c| c.remove_surface(surface))
    }

    /// Whether the pointer is inside `self.geometry` at `point`.
    pub fn contains(&self, point: Point<f64, Logical>) -> bool {
        let g = &self.geometry;
        point.x >= g.loc.x as f64
            && point.x < (g.loc.x + g.size.w) as f64
            && point.y >= g.loc.y as f64
            && point.y < (g.loc.y + g.size.h) as f64
    }
}
