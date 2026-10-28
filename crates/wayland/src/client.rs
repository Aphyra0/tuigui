//! The clients: the root program's process, and the helpers it spawns.
//!
//! One root per session: the process the caller spawned, holding the root
//! window and every helper that showed up as its own wayland connection. The
//! runtime dir is private, so a connection can only come from a process we
//! started — every accepted connection is a sub-client by construction.
//!
//! Clients implement Smithay's `ClientData` directly: the connection's data
//! *is* the client record, and the type says which one it is. Smithay holds
//! the `Arc`; the session and the holder keep `Weak`s.
//!
//! No `unsafe` is involved in the sharing smithay requires: every field is
//! natively `Send + Sync` (locks, not channels), so `ClientData` is satisfied
//! by the compiler's own analysis rather than by a promise.

mod root_client;
mod sub_client;

pub use root_client::RootClient;
pub use sub_client::SubClient;

use std::os::unix::net::UnixStream;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use smithay::wayland::compositor::CompositorClientState;

use crate::error::CompError;
use crate::state::State;
use crate::utils::{peer_pid, proc_parent};

/// The per-connection compositor state, resolved whichever kind of client the
/// connection belongs to. XWayland's client carries `XWaylandClientData`
/// instead, which smithay owns.
pub(crate) fn compositor_client_state(
    client: &smithay::reexports::wayland_server::Client,
) -> Option<&CompositorClientState> {
    if let Some(root) = client.get_data::<RootClient>() {
        return Some(&root.compositor_state);
    }
    if let Some(sub) = client.get_data::<SubClient>() {
        return Some(&sub.compositor_state);
    }
    None
}

impl State {
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
            .clone()
            .ok_or_else(|| {
                CompError::Server("a connection arrived before the root was spawned".into())
            })?;
        if !root.connected.load(Ordering::SeqCst) {
            // The same `Arc` goes into the slot, so Smithay owns the record from here and
            // the session's field is only a second share of one count.
            self.display_handle
                .insert_client(stream, root.clone())
                .map_err(|e| CompError::Server(format!("insert_client: {e}")))?;
            // Set only once the slot took it: a flag that says "connected" while the
            // connection was never admitted would make the next attempt look like a
            // helper's, and its parent lookup fail for a reason that is not true.
            root.connected.store(true, Ordering::SeqCst);
            log::info!("admitted the root client, pid {}", root.child.id());
            return Ok(());
        }

        let pid = peer_pid(&stream)
            .ok_or_else(|| CompError::Server("peer pid of a connection unmeasurable".into()))?;

        // The connection belongs to the nearest **ancestor that is already a client**, not
        // to its immediate parent. A session is a process tree, and most of it never
        // connects: a terminal emulator starts a shell, the shell starts a browser, and
        // only the emulator and the browser speak Wayland. Looking one level up would
        // refuse the browser because the shell has no connection to find.
        //
        // Walking up also keeps the trust chain that makes a private socket safe: a
        // process can only attach under an ancestor, and an ancestor is a process that
        // started it. The walk stops at us, which is the whole session's parent.
        let me = std::process::id();
        let mut parent = proc_parent(pid);
        let mut depth = 0;
        let root_pid = root.child.id();

        // Every connected client, read once: the walk compares against these, so the
        // set does not have to be re-read per step.
        let handle = self.display_handle.backend_handle();
        let mut ids = Vec::new();
        handle.with_all_clients(|id| ids.push(id));
        let mut connected: Vec<u32> = Vec::new();
        for id in ids {
            let data = handle
                .get_client_data(id)
                .map_err(|e| CompError::Server(e.to_string()))?;
            if let Some(sub) = data.downcast_ref::<SubClient>() {
                connected.push(sub.pid);
            } else if data.downcast_ref::<RootClient>().is_some() {
                connected.push(root_pid);
            }
        }
        connected.push(me);

        while let Some(candidate) = parent {
            depth += 1;
            // A floor on the walk: a process tree is deep but not endless, and a loop
            // that never found a client would otherwise climb to pid 1 and stop there.
            if depth > 64 {
                break;
            }
            if connected.contains(&candidate) {
                break;
            }
            parent = proc_parent(candidate);
        }

        let Some(owner) = parent else {
            log::warn!("{pid} has no ancestor in the session; ignoring the connection");
            return Ok(());
        };

        // Every process we started is under the root, whether its parent is us or
        // something we started: a session is one program and what that program spawned.
        let sub = Arc::new(SubClient::new(pid));
        root.sub_clients
            .write()
            .unwrap()
            .push(Arc::downgrade(&sub));
        self.display_handle
            .insert_client(stream, sub)
            .map_err(|e| CompError::Server(format!("insert_client: {e}")))?;
        log::info!("admitted {pid}, nearest ancestor {owner}");
        Ok(())
    }
}
