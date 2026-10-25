//! The clients: the root program's process, and the helpers it spawns.
//!
//! One root per session: the process the caller spawned, holding the root
//! window and every helper that showed up as its own wayland connection. The
//! runtime dir is private, so a connection can only come from a process we
//! started — every accepted connection is a sub-client by construction.
//!
//! Clients implement Smithay's `ClientData` directly: the connection's data
//! *is* the client record, and the type says which one it is. Smithay holds
//! the `Arc`; the session and the holder keep `Weak`s. `attached` is where
//! the holder parks its own data.
//!
//! The unsafe `Sync` impls are honest for this crate: the session drives
//! everything from one thread, so the `Arc`'s other holders never touch the
//! record concurrently.

use std::os::unix::net::UnixStream;
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock, Weak};

use derive_getters::Getters;
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::wayland::compositor::CompositorClientState;

use crate::error::CompError;
use crate::state::State;
use crate::utils::peer_pid;
use crate::window::Window;

/// Synchronized tail of a client's stderr (most recent bytes). Populated by a
/// dedicated reader thread so we never block on the pipe; read through
/// [`RootClient::stderr`] for troubleshooting headless clients that refuse
/// our minimal session (GTK/Qt print which Wayland interfaces they need).
pub struct StderrTail(Arc<std::sync::Mutex<Vec<u8>>>);

impl StderrTail {
    pub(crate) fn new() -> Self {
        StderrTail(Arc::new(std::sync::Mutex::new(Vec::new())))
    }
    pub(crate) fn push(&self, bytes: &[u8]) {
        let mut buf = self.0.lock().unwrap();
        buf.extend_from_slice(bytes);
        if buf.len() > STDERR_TAIL_CAP {
            let surplus = buf.len() - STDERR_TAIL_CAP;
            buf.drain(..surplus);
        }
    }
    pub fn as_string(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

/// How many bytes of a client's stderr to retain for diagnostics. Bounded so
/// a chatty client can't grow memory without limit.
const STDERR_TAIL_CAP: usize = 32 * 1024;

/// The root client: the process the session was built around.
#[derive(Getters)]
pub struct RootClient<C = (), W = ()> {
    /// The process. Killed when the root client goes away.
    #[getter(skip)]
    child: Child,
    /// The process's stderr tail, for diagnostics.
    stderr: StderrTail,
    /// The root window, and the toplevels its program spawned afterwards.
    root_window: RwLock<Option<Window<W>>>,
    /// Helper processes that connected as their own wayland clients.
    sub_clients: RwLock<Vec<Weak<SubClient<C, W>>>>,
    /// Set when this client's connection was attached to it.
    connected: AtomicBool,
    /// The per-connection compositor state smithay reads back.
    #[getter(skip)]
    compositor_state: CompositorClientState,
    /// The holder's own data.
    attached: RwLock<C>,
}

impl<C: Default, W> RootClient<C, W> {
    pub(crate) fn new(child: Child, stderr: StderrTail) -> Self {        RootClient {
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
}

impl<C, W> Drop for RootClient<C, W> {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// SAFETY: the session drives everything from one thread; the Arc's other
// holders (smithay's connection slot) never touch the record concurrently,
// and the holder's access goes through the session.
unsafe impl<C, W> Sync for RootClient<C, W> {}

impl<C, W> ClientData for RootClient<C, W> {
    fn initialized(&self, _id: ClientId) {}
    fn disconnected(&self, _id: ClientId, _reason: DisconnectReason) {}
}

/// A helper process the root spawned, which connected as its own wayland
/// client. Observe-only: the session does not own its process, so it can only
/// be waited on and disconnected at the protocol level.
#[derive(Getters)]
pub struct SubClient<C = (), W = ()> {
    /// The wayland peer's process id. The root client's pid lives in its
    /// child handle instead.
    pid: u32,
    /// The sub-client's root window, and the toplevels its program spawned.
    root_window: RwLock<Option<Window<W>>>,
    /// Helper processes this sub-client itself spawned, as their own clients.
    sub_clients: RwLock<Vec<Weak<SubClient<C, W>>>>,
    /// The per-connection compositor state smithay reads back.
    #[getter(skip)]
    compositor_state: CompositorClientState,
    /// The holder's own data.
    attached: RwLock<C>,
}

impl<C: Default, W> SubClient<C, W> {
    pub(crate) fn new(pid: u32) -> Self {
        SubClient {
            pid,
            root_window: RwLock::new(None),
            sub_clients: RwLock::new(Vec::new()),
            compositor_state: CompositorClientState::default(),
            attached: RwLock::new(C::default()),
        }
    }
}

// Same justification as `RootClient`'s.
unsafe impl<C, W> Sync for SubClient<C, W> {}

impl<C, W> ClientData for SubClient<C, W> {
    fn initialized(&self, _id: ClientId) {}
    fn disconnected(&self, _id: ClientId, _reason: DisconnectReason) {}
}

/// The per-connection compositor state, resolved whichever kind of client the
/// connection belongs to. XWayland's client carries `XWaylandClientData`
/// instead, which smithay owns.
pub(crate) fn compositor_client_state<C, W>(
    client: &smithay::reexports::wayland_server::Client,
) -> Option<&CompositorClientState> {
    if let Some(root) = client.get_data::<RootClient<C, W>>() {
        return Some(&root.compositor_state);
    }
    if let Some(sub) = client.get_data::<SubClient<C, W>>() {
        return Some(&sub.compositor_state);
    }
    None
}

impl<C: Default, W> State<C, W> {
    /// Admit a newly connected wayland client.
    ///
    /// The runtime dir is private, so a connection can only come from a process
    /// under our root. The first connection is the root's own. Every later one
    /// must name an already-connected client as its parent — the process tree is
    /// the trust chain — and is attached to that parent's sub-clients. A
    /// connection whose parent is unknown is warned about and dropped: there is
    /// nothing to attach it to.
    pub(crate) fn accept_client(&mut self, stream: UnixStream) -> Result<(), CompError> {
        let root = self
            .root_client
            .as_ref()
            .and_then(Weak::upgrade)
            .ok_or_else(|| {
                CompError::Server("a connection arrived before the root was spawned".into())
            })?;
        if !root.connected.load(Ordering::SeqCst) {
            root.connected.store(true, Ordering::SeqCst);
            self.display_handle
                .insert_client(stream, root)
                .map_err(|e| CompError::Server(format!("insert_client: {e}")))?;
            return Ok(());
        }

        let pid = peer_pid(&stream)
            .ok_or_else(|| CompError::Server("peer pid of a connection unmeasurable".into()))?;
        let parent = proc_parent(pid)
            .ok_or_else(|| CompError::Server(format!("no parent pid for {pid}")))?;

        // Every connected client, flat, straight from the display: the new
        // connection's parent must be one of them, and the new client attaches
        // to that parent's sub-clients.
        let handle = self.display_handle.backend_handle();
        let mut ids = Vec::new();
        handle.with_all_clients(|id| ids.push(id));
        for id in ids {
            let data = match handle.get_client_data(id) {
                Ok(data) => data,
                Err(_) => continue,
            };
            if let Some(root_c) = data.downcast_ref::<RootClient<C, W>>() {
                if root_c.child.id() != parent {
                    continue;
                }
                let sub = Arc::new(SubClient::new(pid));
                root_c.sub_clients
                    .write()
                    .unwrap()
                    .push(Arc::downgrade(&sub));
                self.attach_sub_client(stream, sub)?;
                return Ok(());
            }
            if let Some(parent_sub) = data.downcast_ref::<SubClient<C, W>>() {
                if parent_sub.pid != parent {
                    continue;
                }
                let sub = Arc::new(SubClient::new(pid));
                parent_sub
                    .sub_clients
                    .write()
                    .unwrap()
                    .push(Arc::downgrade(&sub));
                self.attach_sub_client(stream, sub)?;
                return Ok(());
            }
        }

        eprintln!(
            "wayland: parent {parent} of {pid} is not a connected client; ignoring the connection"
        );
        Ok(())
    }

    fn attach_sub_client(
        &self,
        stream: UnixStream,
        sub: Arc<SubClient<C, W>>,
    ) -> Result<(), CompError> {
        self.display_handle
            .insert_client(stream, sub)
            .map_err(|e| CompError::Server(format!("insert_client: {e}")))
    }
}

/// The parent of a pid, read from `/proc`. Linux-only by the crate gate.
fn proc_parent(pid: u32) -> Option<u32> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let line = status.lines().find(|l| l.starts_with("PPid:"))?;
    let raw = match line["PPid:".len()..].trim().parse::<u32>() {
        Ok(raw) => raw,
        Err(_) => return None,
    };
    Some(raw)
}
