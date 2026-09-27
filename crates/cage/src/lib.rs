//! tuigui-cage: runs a GUI application inside a single-window headless
//! Wayland session and exposes snapshot / frame-stream / input-injection.
//!
//! Composition model:
//! - `cage` binary: wlroots-based single-window kiosk compositor. We run it
//!   with the `WLR_BACKENDS=headless` env so it never opens a real output,
//!   and hand the app's Wayland socket to the launched app via `WAYLAND_DISPLAY`.
//! - Frame capture: a separate Wayland client (this crate) connects to that
//!   socket and uses `wlr-screencopy` to copy the output into a shm buffer.
//! - Input: virtual pointer/keyboard Wayland protocols against the same socket.
//!
//! If the `cage` binary is not found, all operations return [`CageError::CageMissing`].

pub mod capture;
pub mod input;
pub mod session;

pub use capture::{CageFrameSource, CaptureConfig, PageSource, ScreenDriver, Screenshot};
pub use input::{InputSink, InputSock, KeyEvent, KeyState, PointerEvent, RecordingSink};
pub use session::{CageSession, CageSpec, HeadlessConfig};

use thiserror::Error;

/// Crate-level error.
#[derive(Debug, Error)]
pub enum CageError {
    #[error("`cage` binary not found in PATH; install it or point TUIGUI_CAGE_BIN at it")]
    CageMissing,
    #[error("cage exited unexpectedly: {0}")]
    CageDied(String),
    #[error("capture failed: {0}")]
    Capture(String),
    #[error("input injection failed: {0}")]
    Input(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("png encode failed: {0}")]
    Png(#[from] png::EncodingError),
}

/// Result alias used across the crate.
pub type Result<T, E = CageError> = std::result::Result<T, E>;
