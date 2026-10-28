//! A helper process the root spawned, which connected as its own wayland
//! client.

use std::sync::{Arc, RwLock, Weak};

use derive_getters::Getters;
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::wayland::compositor::CompositorClientState;

use crate::state::State;
use crate::window::Window;

/// A helper process the root spawned, which connected as its own wayland
/// client. Observe-only: the session does not own its process, so it can only
/// be waited on and disconnected at the protocol level.
#[derive(Getters)]
pub struct SubClient {
    /// The wayland peer's process id. The root client's pid lives in its
    /// child handle instead.
    pub(crate) pid: u32,
    /// The sub-client's root window, and the toplevels its program spawned.
    pub(crate) root_window: RwLock<Option<Arc<Window>>>,
    /// Helper processes this sub-client itself spawned, as their own clients.
    pub(crate) sub_clients: RwLock<Vec<Weak<SubClient>>>,
    /// The per-connection compositor state smithay reads back.
    #[getter(skip)]
    pub(crate) compositor_state: CompositorClientState,
}

impl SubClient {
    pub(crate) fn new(pid: u32) -> Self {
        SubClient {
            pid,
            root_window: RwLock::new(None),
            sub_clients: RwLock::new(Vec::new()),
            compositor_state: CompositorClientState::default(),
        }
    }

    /// Dispatch this helper's queued inputs: its own window tree, then the
    /// helpers it started.
    pub(crate) fn dispatch_inputs(&self, state: &mut State) {
        if let Some(tree) = self.root_window.read().unwrap().as_ref() {
            tree.dispatch_inputs(state);
        }
        let subs: Vec<_> = self
            .sub_clients
            .read()
            .unwrap()
            .iter()
            .filter_map(Weak::upgrade)
            .collect();
        for sub in subs {
            sub.dispatch_inputs(state);
        }
    }
}

impl ClientData for SubClient {
    fn initialized(&self, _id: ClientId) {}
    fn disconnected(&self, _id: ClientId, _reason: DisconnectReason) {}
}
