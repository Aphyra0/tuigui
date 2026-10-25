//! `wl_compositor` handler: buffer commits become owned pixels.

use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::compositor::{
    BufferAssignment, CompositorHandler, CompositorState, SurfaceAttributes,
};

use crate::state::State;
use crate::utils::map_rgba;

impl<C, W> CompositorHandler for State<C, W>
where
    C: proto::Shared + Default,
    W: proto::Shared + Default,
{
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.globals.compositor_state
    }

    fn client_compositor_state<'a>(
        &self,
        client: &'a smithay::reexports::wayland_server::Client,
    ) -> &'a smithay::wayland::compositor::CompositorClientState {
        crate::client::compositor_client_state::<C, W>(client)
            .expect("client has neither RootClient, SubClient nor XWaylandClientData")
    }

    #[tracing::instrument(name = "commit", skip_all)]
    fn commit(&mut self, surface: &WlSurface) {
        // Software compositing: read the committed buffer straight into the
        // owning client's window and accumulate damage. (No renderer-side
        // on_commit_buffer_handler — we don't run a GPU renderer.)
        let out_size = self.server_size;

        // Whose connection produced this commit: the root's, a sub's, or
        // XWayland's.
        let client = match surface.client() {
            Some(client) => client,
            None => return,
        };
        if let Some(root) = client.get_data::<crate::client::RootClient<C, W>>() {
            let mut tree = root.root_window.write().unwrap();
            if let Some(win) = tree.as_mut().and_then(|t| t.commit_target(surface)) {
                commit_into(win, out_size, surface);
            }
        } else if let Some(sub) = client.get_data::<crate::client::SubClient<C, W>>() {
            let mut tree = sub.root_window.write().unwrap();
            if let Some(win) = tree.as_mut().and_then(|t| t.commit_target(surface)) {
                commit_into(win, out_size, surface);
            }
        } else {
            // XWayland's connection: its admitted surface is the root
            // client's window.
            let admitted = self
                .root_client
                .as_ref()
                .and_then(|root| {
                    let tree = root.root_window.read().unwrap();
                    tree.as_ref().map(|w| w.surface.wl_surface.clone())
                });
            if let Some(admitted) = admitted {
                let is_win = admitted == *surface;
                let is_kid = !is_win
                    && smithay::wayland::compositor::get_parent(surface)
                        .map(|p| p == admitted)
                        .unwrap_or(false);
                if !(is_win || is_kid) {
                    return;
                }
                if let Some(root) = self.root_client.as_ref() {
                    if let Some(win) = root.root_window.write().unwrap().as_mut() {
                        commit_into(win, out_size, surface);
                    }
                }
            }
        }
    }
}

/// Read the committed buffer into `win`'s pixel store.
fn commit_into<W>(
    win: &mut crate::window::Window<W>,
    out_size: (u32, u32),
    surface: &WlSurface,
) {
    let mut any_pixels = false;
    // Buffer consumed this commit; we `memcpy` its contents in `map_rgba`,
    // so it can be released back to the client as soon as we're done.
    let mut release: Option<smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer> =
        None;
    smithay::wayland::compositor::with_states(surface, |states| {
        let mut attrs = states.cached_state.get::<SurfaceAttributes>();
        if let Some(BufferAssignment::NewBuffer(buffer)) = &attrs.current().buffer {
            let mut px = win.surface.pixels.lock().unwrap();
            let copied = map_rgba(buffer, &mut px, out_size).is_ok();
            if copied {
                any_pixels = true;
                release = Some(buffer.clone());
            }
        };
    });
    // Return the consumed buffer to the client. Firefox's buffer pools wait
    // for `wl_buffer.release` before reusing/allocating; never sending it
    // starves the pool and the surface paints white.
    if let Some(buf) = release {
        buf.release();
    }

    if any_pixels {
        win.surface.committed = true;
    }
}
