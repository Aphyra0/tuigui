//! Per-window bookkeeping: the window tree and the toplevels it holds.
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

use std::collections::VecDeque;
use std::sync::Mutex;

use derive_getters::Getters;
use derive_setters::Setters;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;

use crate::inputs::Input;
use crate::state::State;
use crate::CompError;

/// One admitted window: its surface, its input mailbox, and the toplevels its
/// program spawned.
#[derive(Getters, Setters)]
#[setters(generate = false)]
pub struct Window<W = ()> {
    /// The admitted surface: buffer bookkeeping and the committed contents.
    pub(crate) surface: Surface,
    /// Serial of the last configure sent (spawn waits for the first commit).
    pub(crate) last_configure_serial: Mutex<Option<u64>>,
    /// Toplevels this window's program spawned after being admitted. Detaching
    /// this window detaches its subtree with it.
    pub(crate) children: Vec<Window<W>>,
    /// Input queued by the holder, drained by the session at the next step.
    /// A plain queue behind a mutex rather than a channel: nothing waits on a
    /// receiver, so a channel's single-consumer machinery only buys the type
    /// `!Sync`.
    pub(crate) inputs: Mutex<VecDeque<Input>>,
    /// Whatever the holder attaches: compositing metadata and the like.
    pub attached: W,
}

/// The admitted surface of a window: its `wl_surface`, the client-facing size,
/// the commit state, and the contents its client last committed.
#[derive(Getters)]
pub(crate) struct Surface {
    /// The admitted surface (an xdg toplevel's, or an X11 window's wl_surface).
    pub(crate) wl_surface: WlSurface,
    /// The client-facing window size (the session's resolution). `None` for a
    /// root surface, which is configured rather than sized here.
    pub(crate) size: Option<(u32, u32)>,
    /// Set once this window committed a readable buffer at least once.
    pub(crate) committed: bool,
    /// Latest committed contents, owned by us, written on commit.
    pub(crate) pixels: Mutex<Vec<u8>>,
}

impl<W> Window<W> {
    pub fn new(surface: WlSurface, size: Option<(u32, u32)>, attached: W) -> Self {
        Window {
            surface: Surface {
                wl_surface: surface,
                size,
                committed: false,
                pixels: Mutex::new(Vec::new()),
            },
            last_configure_serial: Mutex::new(None),
            children: Vec::new(),
            inputs: Mutex::new(VecDeque::new()),
            attached,
        }
    }

    /// Dispatch this window's queued inputs, then its children's.
    ///
    /// The tree is walked shallow-to-deep; each window dispatches its own
    /// mailbox against itself, so an input reaches the window it was queued for.
    pub(crate) fn dispatch_inputs<C>(&self, state: &mut State<C, W>)
    where
        C: proto::Shared + Default,
        W: proto::Shared + Default,
    {
        let inputs: Vec<Input> = self.inputs.lock().unwrap().drain(..).collect();
        for input in inputs {
            if let Err(e) = state.dispatch_input(self.surface.wl_surface(), input) {
                tracing::debug!("a queued input did not dispatch: {e}");
            }
        }
        for child in &self.children {
            child.dispatch_inputs(state);
        }
    }

    /// The window whose surface is `surface`, or whose surface is the parent
    /// of it (a subsurface commit lands on the toplevel that owns the tree).
    pub(crate) fn commit_target(&mut self, surface: &WlSurface) -> Option<&mut Window<W>> {
        let is_win = self.surface.wl_surface == *surface;
        let is_kid = !is_win
            && smithay::wayland::compositor::get_parent(surface)
                .map(|p| p == self.surface.wl_surface)
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
        self.children
            .retain(|c| c.surface.wl_surface != *surface);
        if self.children.len() != before {
            return true;
        }
        self.children.iter_mut().any(|c| c.remove_surface(surface))
    }
}

impl<W: proto::Shared> proto::windowing::Window<W> for Window<W> {
    fn input(&self, input: Input) -> Result<(), proto::windowing::Error> {
        // Cannot fail: appending never errors, unlike a channel with no
        // receiver. The queue is drained by the session's next step.
        self.inputs.lock().unwrap().push_back(input);
        Ok(())
    }

    fn capture(&self) -> Result<Vec<u8>, proto::windowing::Error> {
        if !self.surface.committed {
            return Err(proto::windowing::Error::NotReady);
        }
        let pixels = self.surface.pixels.lock().unwrap().clone();
        if pixels.is_empty() {
            return Err(proto::windowing::Error::NotReady);
        }
        // The frame callbacks fire here, with the read: taking the pixels is
        // the point where this window's frame is consumed, so it is the point
        // where the client may draw the next one. `done` is back-pressure, not
        // a presentation promise, and the serial caller loop is what throttles
        // it (nothing else can read until this call returns).
        //
        // NOTE: if the caller ever overlaps encoding with reading, this fires a
        // frame too early and the client runs one encode period ahead. Move it
        // to the presentation point (or gate it on the encode queue being
        // empty) at that point.
        crate::utils::fire_frame_callbacks(&self.surface.wl_surface);
        Ok(pixels)
    }

    fn with_attached(&self, f: &mut dyn FnMut(&W)) {
        f(&self.attached);
    }

    fn with_children(&self, f: &mut dyn FnMut(&dyn proto::windowing::Window<W>)) {
        for child in &self.children {
            f(child);
        }
    }
}

