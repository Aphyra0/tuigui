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
//! No `unsafe` is involved in the sharing smithay requires: every field behind
//! the generics is natively `Send + Sync` (locks, not channels), so `ClientData`
//! is satisfied by the compiler's own analysis rather than by a promise.

mod root_client;
mod sub_client;

pub use root_client::RootClient;
pub use sub_client::SubClient;

use std::os::unix::net::UnixStream;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};

use proto::windowing::Client as _;
use smithay::wayland::compositor::CompositorClientState;

use crate::error::CompError;
use crate::state::State;
use crate::utils::{peer_pid, proc_parent};

/// The per-connection compositor state, resolved whichever kind of client the
/// connection belongs to. XWayland's client carries `XWaylandClientData`
/// instead, which smithay owns.
pub(crate) fn compositor_client_state<C, W>(
    client: &smithay::reexports::wayland_server::Client,
) -> Option<&CompositorClientState>
where
    C: proto::Shared,
    W: proto::Shared,
{
    if let Some(root) = client.get_data::<RootClient<C, W>>() {
        return Some(&root.compositor_state);
    }
    if let Some(sub) = client.get_data::<SubClient<C, W>>() {
        return Some(&sub.compositor_state);
    }
    None
}

impl<C, W> State<C, W>
where
    C: proto::Shared + Default,
    W: proto::Shared + Default,
{
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
            tracing::info!("wayland: admitted the root client, pid {}", root.pid());
            return Ok(());
        }

        let pid = peer_pid(&stream)
            .ok_or_else(|| CompError::Server("peer pid of a connection unmeasurable".into()))?;
        let parent = proc_parent(pid)
            .ok_or_else(|| CompError::Server(format!("no parent pid for {pid}")))?;

        // A process we started ourselves is a child of *us*, not of the client that
        // happens to be our root. A program that opens a second connection — a toolkit
        // opening its display after probing, or a helper it forks directly — names this
        // process as its parent, and there is no client record with that pid because the
        // root record is the *program*, not us.
        //
        // Such a connection belongs to the session's program, so it hangs under the root.
        if parent == std::process::id() {
            let sub = Arc::new(SubClient::new(pid));
            root.sub_clients.write().unwrap().push(Arc::downgrade(&sub));
            self.display_handle
                .insert_client(stream, sub)
                .map_err(|e| CompError::Server(format!("insert_client: {e}")))?;
            tracing::info!("wayland: admitted {pid}, started by this process");
            return Ok(());
        }

        // Every connected client, flat, straight from the display: the new
        // connection's parent must be one of them, and the new client attaches
        // to that parent's sub-clients.
        let handle = self.display_handle.backend_handle();
        let mut ids = Vec::new();
        handle.with_all_clients(|id| ids.push(id));
        for id in ids {
            let data = handle
                .get_client_data(id)
                .map_err(|e| CompError::Server(e.to_string()))?;
            // Helpers connect most often, so test them first. Each arm yields
            // the record's pid and its children list; the pid check happens
            // once, right here.
            let (found, children) = if let Some(sub_c) = data.downcast_ref::<SubClient<C, W>>() {
                (sub_c.pid, &sub_c.sub_clients)
            } else if let Some(root_c) = data.downcast_ref::<RootClient<C, W>>() {
                (root_c.pid(), &root_c.sub_clients)
            } else {
                continue;
            };
            if found != parent {
                continue;
            }
            let sub = Arc::new(SubClient::new(pid));
            children.write().unwrap().push(Arc::downgrade(&sub));
            self.display_handle
                .insert_client(stream, sub)
                .map_err(|e| CompError::Server(format!("insert_client: {e}")))?;
            return Ok(());
        }

        eprintln!(
            "wayland: parent {parent} of {pid} is not a connected client; ignoring the connection"
        );
        Ok(())
    }
}
