//! Crate-level error.

/// Crate-level error.
#[derive(Debug, thiserror::Error)]
pub enum CompError {
    #[error("wayland server failed: {0}")]
    Server(String),
    #[error("client not ready yet (no committed frame)")]
    NotReady,
    #[error("window is gone (client exited or closed the window)")]
    WindowGone,
    #[error("spawn failed: {0}")]
    Spawn(String),
    #[error("capture failed: {0}")]
    Capture(String),
}

/// Result alias used across the crate.
pub type Result<T, E = CompError> = std::result::Result<T, E>;
