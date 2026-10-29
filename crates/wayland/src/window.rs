//! Per-window bookkeeping: the window tree and the toplevels it holds.
//!
//! The session keeps a **tree**: the root client's window, and the toplevels
//! its program spawned afterwards branching under it. Detaching a window
//! detaches its subtree with it.
//!
//! Windows are shared, not owned by their parent: a child lives in an `Arc`,
//! and weak handles to it are what reach past the session. What the session
//! mutates on a window sits behind locks (the children, the committed
//! pixels), so a borrow of a window is as good as a mutable one.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use derive_getters::Getters;
use derive_setters::Setters;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::shell::xdg::{ToplevelSurface, XdgToplevelSurfaceData};
use ui_events::keyboard::KeyboardEvent;
use ui_events::pointer::PointerEvent;

use crate::state::State;

/// One admitted window: its surface, its input mailbox, and the toplevels its
/// program spawned.
#[derive(Getters, Setters)]
#[setters(generate = false)]
pub struct Window {
    /// The admitted surface: buffer bookkeeping and the committed contents.
    pub(crate) surface: Surface,
    /// The surface id this window draws itself as: its identity on the pane
    /// and on the wire.
    pub(crate) id: wire::message::SurfaceId,
    /// The xdg toplevel behind this window: what a configure (a size
    /// proposal) is sent through. Kept because admission is the only place
    /// that holds one, and `set_size` has to reach it later.
    pub(crate) toplevel: ToplevelSurface,
    /// Serial of the last configure sent (spawn waits for the first commit).
    pub(crate) last_configure_serial: Mutex<Option<u64>>,
    /// Toplevels this window's program spawned after being admitted, shared
    /// so weak handles can reach them from the session's holder. Detaching
    /// this window detaches its subtree with it.
    pub(crate) children: RwLock<Vec<Arc<Window>>>,
    /// Key events queued by the holder, drained at the session's next step.
    ///
    /// A plain queue behind a mutex rather than a channel: nothing waits on a
    /// receiver, so a channel's single-consumer machinery only buys the type
    /// `!Sync`.
    ///
    /// Keys and pointers are queued apart because they are not the same kind of
    /// event: a key is a position and a state, while a pointer event carries a
    /// whole state. One queue would only mean sorting them out again.
    pub(crate) keys: Mutex<VecDeque<KeyboardEvent>>,
    /// Pointer events queued by the holder.
    pub(crate) pointers: Mutex<VecDeque<PointerEvent>>,
}

/// The admitted surface of a window: its `wl_surface`, the client-facing size,
/// the commit state, and the contents its client last committed.
#[derive(Getters)]
pub(crate) struct Surface {
    /// The admitted surface (an xdg toplevel's, or an X11 window's wl_surface).
    pub(crate) wl_surface: WlSurface,
    /// The client-facing window size: what the window was last configured
    /// at. Set at admission and rewritten by every later configure.
    ///
    /// An `RwLock`, not a plain field: a re-configure arrives through `&self`
    /// (the same `Arc` the holder reads capture from), so the size a client
    /// was last configured at has to change behind a lock.
    pub(crate) size: RwLock<(u32, u32)>,
    /// Set once this window committed a readable buffer at least once. A
    /// write arrives through `&self`, so it is a flag behind an atomic.
    pub(crate) committed: AtomicBool,
    /// Latest committed contents, owned by us, written on commit.
    pub(crate) pixels: Mutex<Vec<u8>>,
}

impl Window {
    pub fn new(surface: WlSurface, toplevel: ToplevelSurface, size: (u32, u32)) -> Self {
        Window {
            surface: Surface {
                wl_surface: surface,
                size: RwLock::new(size),
                committed: AtomicBool::new(false),
                pixels: Mutex::new(Vec::new()),
            },
            id: wire::message::SurfaceId::random(),
            toplevel,
            last_configure_serial: Mutex::new(None),
            children: RwLock::new(Vec::new()),
            keys: Mutex::new(VecDeque::new()),
            pointers: Mutex::new(VecDeque::new()),
        }
    }

    /// Queue one key event; drained by the session's next step.
    pub fn queue_key(&self, event: KeyboardEvent) {
        self.keys.lock().unwrap().push_back(event);
    }

    /// Queue one pointer event; drained by the session's next step.
    pub fn queue_pointer(&self, event: PointerEvent) {
        self.pointers.lock().unwrap().push_back(event);
    }

    /// What the window last drew: RGBA8, row-major, `width * height * 4` bytes.
    /// `None` until it has drawn at least once.
    ///
    /// Reading fires the window's frame callbacks: taking the pixels is the
    /// point where this window's frame is consumed, so it is the point where
    /// the client may draw the next one. `done` is back-pressure, not a
    /// presentation promise, and the serial caller loop is what throttles it.
    ///
    /// NOTE: if the caller ever overlaps encoding with reading, this fires a
    /// frame too early and the client runs one encode period ahead. Move it
    /// to the presentation point (or gate it on the encode queue being empty)
    /// at that point.
    pub fn capture(&self) -> Option<Vec<u8>> {
        if !self.surface.committed.load(Ordering::Acquire) {
            return None;
        }
        let pixels = self.surface.pixels.lock().unwrap().clone();
        if pixels.is_empty() {
            return None;
        }
        crate::utils::fire_frame_callbacks(&self.surface.wl_surface);
        Some(pixels)
    }

    /// Ask this window to draw at `size` pixels instead of its current one.
    /// The client answers by committing a buffer at whatever size it chose.
    pub fn set_size(&self, size: (u32, u32)) {
        // The size proposal travels on the toplevel, not the surface.
        self.toplevel.with_pending_state(|pending| {
            pending.size = Some((size.0 as i32, size.1 as i32).into());
        });
        self.toplevel.send_configure();
        *self.surface.size.write().unwrap() = size;
    }

    /// What this window calls itself: the title its program set, or the app id it
    /// set instead. `None` when it set neither, which is a window with no name
    /// rather than one with an invented name.
    pub fn title(&self) -> Option<String> {
        smithay::wayland::compositor::with_states(&self.surface.wl_surface, |states| {
            let attributes = states.data_map.get::<XdgToplevelSurfaceData>()?;
            let attributes = attributes.lock().unwrap();
            attributes
                .title
                .clone()
                .or_else(|| attributes.app_id.clone())
        })
    }

    /// Ask the program to close this window: an xdg close request, which the
    /// client may ignore. The window outlives this call until it does not.
    pub fn terminate(&self) {
        self.toplevel.send_close();
    }

    /// Everything under this window: the windows its program opened after it,
    /// then the root windows of the helper clients that program started.
    ///
    /// A helper's window lives beside this one rather than inside it, but it
    /// is drawn as a window of the session all the same, so it is read here.
    pub fn descendants(&self) -> Vec<Arc<Window>> {
        let mut all = self.children.read().unwrap().clone();
        for sub in self.sub_clients() {
            if let Some(tree) = sub.root_window.read().unwrap().clone() {
                all.push(tree);
            }
        }
        all
    }

    /// The helper clients this window's program started, found through the
    /// connection that owns this window.
    fn sub_clients(&self) -> Vec<Arc<crate::client::SubClient>> {
        use smithay::reexports::wayland_server::Resource as _;
        let Some(client) = self.surface.wl_surface.client() else {
            return Vec::new();
        };
        // Copied out: the guard `get_data` hands back borrows a handle that
        // lives only until the end of this statement.
        let Some(root) = client.get_data::<crate::client::RootClient>() else {
            return Vec::new();
        };
        let subs: Vec<_> = root
            .sub_clients
            .read()
            .unwrap()
            .iter()
            .filter_map(std::sync::Weak::upgrade)
            .collect();
        subs
    }

    /// Dispatch this window's queued inputs, then its children's.
    ///
    /// The tree is walked shallow-to-deep; each window dispatches its own
    /// mailbox against itself, so an input reaches the window it was queued for.
    ///
    /// Keys go first, and as a group: a modified key is several key events that
    /// only mean something in order, so slipping a pointer event between the
    /// control press and the `c` press would leave the modifier dangling.
    pub(crate) fn dispatch_inputs(&self, state: &mut State) {
        let keys: Vec<KeyboardEvent> = self.keys.lock().unwrap().drain(..).collect();
        for event in keys {
            if let Err(e) = state.dispatch_key_input(self.surface.wl_surface(), &event) {
                tracing::debug!("a queued key did not dispatch: {e}");
            }
        }
        let pointers: Vec<PointerEvent> = self.pointers.lock().unwrap().drain(..).collect();
        for event in pointers {
            if let Err(e) = state.dispatch_mouse_input(self.surface.wl_surface(), &event) {
                tracing::debug!("a queued pointer event did not dispatch: {e}");
            }
        }
        for child in &*self.children.read().unwrap() {
            child.dispatch_inputs(state);
        }
    }

    /// The window whose surface is `surface`, or whose surface is the parent
    /// of it (a subsurface commit lands on the toplevel that owns the tree).
    pub(crate) fn commit_target(this: &Arc<Self>, surface: &WlSurface) -> Option<Arc<Self>> {
        let is_win = this.surface.wl_surface == *surface;
        let is_kid = !is_win
            && smithay::wayland::compositor::get_parent(surface)
                .map(|p| p == this.surface.wl_surface)
                .unwrap_or(false);
        if is_win || is_kid {
            return Some(this.clone());
        }
        for child in &*this.children.read().unwrap() {
            if let Some(found) = Self::commit_target(child, surface) {
                return Some(found);
            }
        }
        None
    }

    /// Remove the window whose surface is `surface` (a child, or a child's
    /// child). Returns whether something was removed.
    pub(crate) fn remove_surface(&self, surface: &WlSurface) -> bool {
        let mut children = self.children.write().unwrap();
        let before = children.len();
        children.retain(|child| child.surface.wl_surface != *surface);
        let mut removed = children.len() != before;
        for child in &*children {
            removed |= child.remove_surface(surface);
        }
        drop(children);
        removed
    }
}
