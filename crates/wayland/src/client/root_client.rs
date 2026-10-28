//! The root client: the process the session was built around.

use std::process::Child;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, RwLock, Weak};

use derive_getters::Getters;
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::wayland::compositor::CompositorClientState;

use super::SubClient;
use crate::state::State;
use crate::window::Window;

/// The root client: the process the session was built around.
#[derive(Getters)]
pub struct RootClient {
    /// The process. Killed when the root client goes away.
    #[getter(skip)]
    pub(crate) child: Child,
    /// The root window: the tree the compositor reads. An `Arc`, because the
    /// handles the holder mints from it are weak.
    pub(crate) root_window: RwLock<Option<Arc<Window>>>,
    /// Helper processes that connected as their own wayland clients.
    pub(crate) sub_clients: RwLock<Vec<Weak<SubClient>>>,
    /// Set when this client's connection was attached to it.
    pub(crate) connected: AtomicBool,
    /// The per-connection compositor state smithay reads back.
    #[getter(skip)]
    pub(crate) compositor_state: CompositorClientState,
}

impl RootClient {
    pub(crate) fn new(child: Child) -> Self {
        RootClient {
            child,
            root_window: RwLock::new(None),
            sub_clients: RwLock::new(Vec::new()),
            connected: AtomicBool::new(false),
            compositor_state: CompositorClientState::default(),
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
        running(&self.child)
    }

    /// Dispatch the root's queued inputs: its own window tree, then every
    /// helper it started.
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

impl Drop for RootClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl ClientData for RootClient {
    fn initialized(&self, _id: ClientId) {}
    fn disconnected(&self, _id: ClientId, _reason: DisconnectReason) {}
}

/// Whether `child` is still running, without blocking or reaping it.
///
/// `WNOHANG` asks the kernel rather than the handle, because the handle cannot be taken
/// through a shared reference. `0` means still running; anything else means it exited
/// (or was already reaped, which the final `wait` on drop ignores).
fn running(child: &Child) -> bool {
    let code = unsafe { libc::waitpid(child.id() as libc::pid_t, std::ptr::null_mut(), libc::WNOHANG) };
    code == 0
}
