//! In-process headless Wayland compositor: spawns a client into a private
//! Wayland session and exposes capture + input injection through gobo-core.

/// Placeholder for the compositor implementation.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}