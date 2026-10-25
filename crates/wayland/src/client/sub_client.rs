//! A helper process the root spawned, which connected as its own wayland
//! client.

use std::sync::{RwLock, Weak};

use derive_getters::Getters;
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::wayland::compositor::CompositorClientState;

use crate::state::State;
use crate::window::Window;

/// A helper process the root spawned, which connected as its own wayland
/// client. Observe-only: the session does not own its process, so it can only
/// be waited on and disconnected at the protocol level.
#[derive(Getters)]
pub struct SubClient<C = (), W = ()> {
    /// The wayland peer's process id. The root client's pid lives in its
    /// child handle instead.
    pub(crate) pid: u32,
    /// The sub-client's root window, and the toplevels its program spawned.
    pub(crate) root_window: RwLock<Option<Window<W>>>,
    /// Helper processes this sub-client itself spawned, as their own clients.
    pub(crate) sub_clients: RwLock<Vec<Weak<SubClient<C, W>>>>,
    /// The per-connection compositor state smithay reads back.
    #[getter(skip)]
    pub(crate) compositor_state: CompositorClientState,
    /// The holder's own data.
    pub(crate) attached: RwLock<C>,
}

impl<C, W> SubClient<C, W>
where
    C: proto::Shared + Default,
    W: proto::Shared + Default,
{
    pub(crate) fn new(pid: u32) -> Self {
        SubClient {
            pid,
            root_window: RwLock::new(None),
            sub_clients: RwLock::new(Vec::new()),
            compositor_state: CompositorClientState::default(),
            attached: RwLock::new(C::default()),
        }
    }

    /// Dispatch this helper's queued inputs: its own window tree, then the
    /// helpers it started.
    pub(crate) fn dispatch_inputs(&self, state: &mut State<C, W>) {
        if let Some(tree) = self.root_window.write().unwrap().as_mut() {
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

impl<C, W> ClientData for SubClient<C, W>
where
    C: proto::Shared,
    W: proto::Shared,
{
    fn initialized(&self, _id: ClientId) {}
    fn disconnected(&self, _id: ClientId, _reason: DisconnectReason) {}
}

impl<C, W> proto::windowing::Client<C, W> for SubClient<C, W>
where
    C: proto::Shared,
    W: proto::Shared,
{
    fn pid(&self) -> u32 {
        self.pid
    }

    fn is_root(&self) -> bool {
        false
    }

    /// Whether the process is still running.
    ///
    /// A helper's process is not this session's to wait on — it is a peer that connects
    /// and disconnects — so this reports it as running while the connection is up. The
    /// connection closing is what says it is gone, and that is the compositor's to
    /// observe rather than a pid's to answer.
    fn is_running(&self) -> bool {
        true
    }

    fn with_attached(&self, f: &mut dyn FnMut(&C)) {
        f(&self.attached.read().unwrap());
    }

    fn with_window(&self, f: &mut dyn FnMut(&dyn proto::windowing::Window<W>)) {
        if let Some(window) = self.root_window.read().unwrap().as_ref() {
            f(window);
        }
    }

    fn with_children(&self, f: &mut dyn FnMut(&dyn proto::windowing::Client<C, W>)) {
        let subs: Vec<_> = self
            .sub_clients
            .read()
            .unwrap()
            .iter()
            .filter_map(Weak::upgrade)
            .collect();
        for sub in subs {
            f(&*sub);
        }
    }
}
