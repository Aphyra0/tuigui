//! The root client: the process the session was built around.

use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock, Weak};

use derive_getters::Getters;
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::wayland::compositor::CompositorClientState;

use super::{StderrTail, SubClient};
use crate::state::State;
use crate::window::Window;

/// The root client: the process the session was built around.
#[derive(Getters)]
pub struct RootClient<C = (), W = ()> {
    /// The process. Killed when the root client goes away.
    #[getter(skip)]
    pub(crate) child: Child,
    /// The process's stderr tail, for diagnostics.
    pub(crate) stderr: StderrTail,
    /// The root window, and the toplevels its program spawned afterwards.
    pub(crate) root_window: RwLock<Option<Window<W>>>,
    /// Helper processes that connected as their own wayland clients.
    pub(crate) sub_clients: RwLock<Vec<Weak<SubClient<C, W>>>>,
    /// Set when this client's connection was attached to it.
    pub(crate) connected: AtomicBool,
    /// The per-connection compositor state smithay reads back.
    #[getter(skip)]
    pub(crate) compositor_state: CompositorClientState,
    /// The holder's own data.
    pub(crate) attached: RwLock<C>,
}

impl<C, W> RootClient<C, W>
where
    C: proto::Shared + Default,
    W: proto::Shared + Default,
{
    pub(crate) fn new(child: Child, stderr: StderrTail) -> Self {
        RootClient {
            child,
            stderr,
            root_window: RwLock::new(None),
            sub_clients: RwLock::new(Vec::new()),
            connected: AtomicBool::new(false),
            compositor_state: CompositorClientState::default(),
            attached: RwLock::new(C::default()),
        }
    }

    /// Whether the client process is still running.
    ///
    /// The child handle cannot be taken through a shared reference, so the
    /// process is checked by PID: `WNOHANG` does not block, `0` means still
    /// running, anything else means exited (or already reaped). A child
    /// reaped here makes the final `wait` on drop return `ECHILD`, which is
    /// ignored there.
    pub fn is_running(&self) -> bool {
        match unsafe {
            libc::waitpid(self.child.id() as libc::pid_t, std::ptr::null_mut(), libc::WNOHANG)
        } {
            0 => true,
            _ => false,
        }
    }

    /// Dispatch the root's queued inputs: its own window tree, then every
    /// helper it started.
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

impl<C, W> Drop for RootClient<C, W> {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl<C, W> ClientData for RootClient<C, W>
where
    C: proto::Shared,
    W: proto::Shared,
{
    fn initialized(&self, _id: ClientId) {}
    fn disconnected(&self, _id: ClientId, _reason: DisconnectReason) {}
}

impl<C, W> proto::windowing::Client<C, W> for RootClient<C, W>
where
    C: proto::Shared,
    W: proto::Shared,
{
    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn is_root(&self) -> bool {
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
