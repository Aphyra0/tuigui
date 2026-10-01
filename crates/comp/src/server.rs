//! The compositor server: a headless single-window Wayland session on a
//! dedicated thread, plus the capture and input-injection surface.
//!
//! Layout:
//! - [`state::CompState`] implements all smithay handler traits; the wl thread
//!   owns it and services both the wayland display and the control channel.
//! - [`surface::Window`] tracks the one admitted toplevel (geometry, buffer,
//!   frame callbacks, damage bookkeeping).
//! - [`surface::map_rgba`] turns the client's committed buffer into an
//!   owned RGBA8 image (only wl_shm buffers in the MVP).
//! - the `crates/comp` root wires the public `Compositor`/`Client` API.

use crate::state::CompState;
use crate::{Button, CompError, Frame, Key, KeyState, Rect, ScrollDelta};

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::wayland_server::{Display, ListeningSocket};
use smithay::utils::Rectangle;
use smithay::wayland::shell::xdg::ToplevelSurface;

/// Requests sent from the `Compositor`/`Client` API to the wl thread.
pub enum Request {
    /// Grab the current output contents as RGBA8 (+ damage since last grab).
    Capture {
        reply: std::sync::mpsc::Sender<Result<Frame, CompError>>,
    },
    /// Deliver a synthetic key press/release to the focused surface.
    Key {
        key: Key,
        state: KeyState,
        reply: std::sync::mpsc::Sender<Result<(), CompError>>,
    },
    /// Move the pointer to an absolute output position.
    MouseMove {
        x: f64,
        y: f64,
        reply: std::sync::mpsc::Sender<Result<(), CompError>>,
    },
    /// Click a pointer button (press + release in one request).
    Click {
        button: Button,
        reply: std::sync::mpsc::Sender<Result<(), CompError>>,
    },
    /// Scroll by discrete wheel steps.
    Scroll {
        delta: ScrollDelta,
        reply: std::sync::mpsc::Sender<Result<(), CompError>>,
    },
}

/// State shared between the wl thread and the `Client` handle.
pub struct WindowShared {
    /// The admitted toplevel surface.
    pub toplevel: ToplevelSurface,
    /// The client-facing window size (from spawn's resolution).
    pub size: (u32, u32),
    /// Set when the window is gone (client closed it or died).
    pub dead: AtomicBool,
    /// Set once this window committed a readable buffer at least once.
    pub committed: AtomicBool,
}

/// The output's pixel format for capture reads (and the only shm format the
/// MVP admits windows with).
pub const OUTPUT_FORMAT: smithay::backend::allocator::Fourcc =
    smithay::backend::allocator::Fourcc::Argb8888;

pub struct Server {
    pub display: Display<CompState>,
    pub socket: ListeningSocket,
    /// Absolute path of the listening socket, captured at bind time so the
    /// API surface can read it after the server moved into the wl thread.
    pub socket_name: PathBuf,
    /// The single output presenting the one window.
    pub output: Output,
    /// Global render dimensions (physical pixels); the output scale is fixed
    /// at 1.0 so the window always covers the full output.
    pub size: (u32, u32),
    /// Client content scale (`--ui-scale / --scale`) applied via
    /// `ClientHandle::set_client_scale` on admission.
    pub client_scale: f64,
}

impl Server {
    /// Create the display, private socket, and the single output. The wl_output
    /// scale stays 1.0 (neutral) so the window always fills the output; content
    /// scaling is done per-client via `client_scale`.
    pub fn new(size: (u32, u32), client_scale: f64) -> Result<Self, CompError> {
        let display = Display::new().map_err(|e| CompError::Server(e.to_string()))?;
        let socket = ListeningSocket::bind("tuigui-comp")
            .map_err(|e| CompError::Server(format!("bind socket: {e}")))?;
        let socket_name: PathBuf = socket
            .socket_name()
            .ok_or_else(|| CompError::Server("listening socket has no name".into()))?
            .into();

        let (w, h) = (size.0 as i32, size.1 as i32);
        let mode = Mode {
            size: (w, h).into(),
            refresh: 60_000,
        };
        let output = Output::new(
            "tuigui-comp-headless".into(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                model: "tuigui-comp".into(),
                make: "tuigui".into(),
            },
        );
        output.change_current_state(Some(mode), None, None, Some((0, 0).into()));
        output.set_preferred(mode);

        Ok(Server {
            display,
            socket,
            socket_name,
            output,
            size,
            client_scale,
        })
    }

    pub fn dispatch(&mut self, state: &mut CompState) -> Result<(), CompError> {
        self.display
            .dispatch_clients(state)
            .map_err(|e| CompError::Server(e.to_string()))?;
        self.display
            .flush_clients()
            .map_err(|e| CompError::Server(e.to_string()))?;
        Ok(())
    }

    pub fn socket_name(&self) -> PathBuf {
        self.socket_name.clone()
    }
}

/// Clamp a logical-space rect to the output and normalize to pixel-space u32.
pub fn clamp_rect(rect: Rectangle<i32, smithay::utils::Logical>, size: (u32, u32)) -> Option<Rect> {
    let (ow, oh) = (size.0 as i32, size.1 as i32);
    let x0 = rect.loc.x.max(0);
    let y0 = rect.loc.y.max(0);
    let x1 = (rect.loc.x + rect.size.w as i32).min(ow);
    let y1 = (rect.loc.y + rect.size.h as i32).min(oh);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some(crate::Rect {
        x: x0 as u32,
        y: y0 as u32,
        width: (x1 - x0) as u32,
        height: (y1 - y0) as u32,
    })
}
