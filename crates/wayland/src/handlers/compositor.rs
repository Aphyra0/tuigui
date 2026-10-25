//! `wl_compositor` handler: buffer commits become owned pixels + damage.

use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::compositor::{
    BufferAssignment, CompositorHandler, CompositorState, SurfaceAttributes,
};

use crate::state::State;
use crate::surface::{commit_damage, map_rgba};

impl<C, W> CompositorHandler for State<C, W>
where
    C: Send + Sync + 'static + Default,
    W: Send + Sync + 'static + Default,
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
        let out_scale = self.output_scale;

        // Whose connection produced this commit: the root's, a sub's, or
        // XWayland's.
        let client = match surface.client() {
            Some(client) => client,
            None => return,
        };
        if let Some(root) = client.get_data::<crate::client::RootClient<C, W>>() {
            let mut tree = root.root_window.write().unwrap();
            if let Some(win) = tree.as_mut().and_then(|t| t.commit_target(surface)) {
                commit_into(win, out_size, out_scale, surface);
            }
        } else if let Some(sub) = client.get_data::<crate::client::SubClient<C, W>>() {
            let mut tree = sub.root_window.write().unwrap();
            if let Some(win) = tree.as_mut().and_then(|t| t.commit_target(surface)) {
                commit_into(win, out_size, out_scale, surface);
            }
        } else {
            // XWayland's connection: its admitted surface is the root
            // client's window.
            let admitted = self
                .root_client
                .as_ref()
                .and_then(Weak::upgrade)
                .and_then(|root| {
                    let tree = root.root_window.read().unwrap();
                    tree.as_ref().map(|w| w.surface.clone())
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
                if let Some(root) = self.root_client.as_ref().and_then(Weak::upgrade) {
                    if let Some(win) = root.root_window.write().unwrap().as_mut() {
                        commit_into(win, out_size, out_scale, surface);
                    }
                }
            }
        }
    }
}

/// Read the committed buffer into `win`'s pixel store and accumulate damage.
fn commit_into<C, W>(
    win: &mut crate::window::Window<W>,
    out_size: (u32, u32),
    out_scale: f64,
    surface: &WlSurface,
) {
    let mut any_pixels = false;
    let mut damage: Vec<crate::Rect> = Vec::new();
    // Buffer consumed this commit; we `memcpy` its contents in `map_rgba`,
    // so it can be released back to the client as soon as we're done.
    let mut release: Option<smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer> =
        None;
    smithay::wayland::compositor::with_states(surface, |states| {
        let mut attrs = states.cached_state.get::<SurfaceAttributes>();
        if let Some(BufferAssignment::NewBuffer(buffer)) = &attrs.current().buffer {
            let mut px = win.pixels.lock().unwrap();
            let copied = map_rgba(buffer, &mut px, out_size).is_ok();
            if copied {
                any_pixels = true;
                release = Some(buffer.clone());
            }
        }
        commit_damage(&attrs.current(), out_size, out_scale, &mut damage);
    });
    // Return the consumed buffer to the client. Firefox's buffer pools wait
    // for `wl_buffer.release` before reusing/allocating; never sending it
    // starves the pool and the surface paints white.
    if let Some(buf) = release {
        buf.release();
    }

    {
        let mut dmg = win.damage.lock().unwrap();
        if damage.is_empty() {
            let (w, h) = out_size;
            dmg.push(crate::Rect { x: 0, y: 0, width: w, height: h });
        } else {
            dmg.extend(damage);
        }
    }

    if any_pixels {
        win.committed = true;
    }
}
