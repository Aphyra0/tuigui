//! In-process headless Wayland compositor: spawns a client into a private
//! Wayland session and exposes capture + input injection through gobo-core.

/// Placeholder for the compositor implementation.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

pub struct Compositor {}

impl gobo_renderer::Streamer for Compositor {
    fn next_frame(&mut self) -> Option<gobo_renderer::RgbImage> {
        todo!()
    }
}
