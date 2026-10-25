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
//! - the `crates/compositor` root wires the public `Compositor`/`Client` API.

use crate::state::CompState;
use crate::{Button, CompError, Frame, Key, KeyState, Rect, ScrollDelta};

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::wayland_server::{BindError, Display, ListeningSocket};
use smithay::utils::Rectangle;

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
    /// Deliver a synthetic key press/release by evdev code, for a caller that
    /// knows the key position rather than a character.
    Keycode {
        evdev: u32,
        state: KeyState,
        reply: std::sync::mpsc::Sender<Result<(), CompError>>,
    },
    /// Move the pointer to an absolute output position.
    MouseMove {
        x: f64,
        y: f64,
        reply: std::sync::mpsc::Sender<Result<(), CompError>>,
    },
    /// Press or release a pointer button.
    ///
    /// One state at a time rather than a click, so a drag can be expressed: a
    /// press, motion with the button still held, then a release.
    Button {
        button: Button,
        down: bool,
        reply: std::sync::mpsc::Sender<Result<(), CompError>>,
    },
    /// Scroll by discrete wheel steps.
    Scroll {
        delta: ScrollDelta,
        reply: std::sync::mpsc::Sender<Result<(), CompError>>,
    },
    /// Query the current pointer position and button state.
    PointerState {
        reply: std::sync::mpsc::Sender<Result<crate::PointerState, CompError>>,
    },
}

/// State shared between the wl thread and the `Client` handle.
pub struct WindowShared {
    /// The admitted surface (tl xdg toplevel or an X11 window's wl_surface).
    pub surface: smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
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
    /// Global render dimensions (physical pixels); what the output presents.
    pub size: (u32, u32),
    /// The scale the output advertises. A client multiplies the size it is
    /// configured with by this to get its buffer, so the configured size is the
    /// output's resolution divided by it.
    pub scale: f64,
    /// Client content scale (`--ui-scale / --scale`) applied via
    /// `ClientHandle::set_client_scale` on admission.
    pub client_scale: f64,
    /// X11 display number for the XWayland server, once it starts.
    /// `Some(n)` means `DISPLAY=:n` should be set for spawned clients.
    /// Shared with the `Compositor`/`Client` API so `spawn` reads it.
    pub x11_display: std::sync::Arc<std::sync::RwLock<Option<String>>>,
    /// Set true once XWayland is known to be unavailable (spawn failed or the
    /// WM failed to start). Lets `spawn()`'s display wait give up deterministically
    /// instead of racing an arbitrary timeout.
    pub x11_failed: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Whether captures should draw the software cursor overlay. Disabled with
    /// `--no-cursor` (e.g. for clean pixel-exact screenshots).
    pub show_cursor: bool,
    /// Whether to spawn an XWayland server so X11-only clients can render
    /// through the compositor. Disabled with `--no-x`.
    pub use_x11: bool,
}

impl Server {
    /// Create the display, private socket, and the single output.
    ///
    /// `scale` is what the output advertises to its clients: a client that
    /// honours it draws at that many times the logical size, so its interface is
    /// legible on a dense panel instead of half-sized. The **mode is unchanged**,
    /// so the window still fills the same pixels.
    ///
    /// `runtime_dir` is a private, per-session directory owned by this
    /// compositor. The listening socket is bound *inside* it. Giving the child
    /// this same `XDG_RUNTIME_DIR` means the only `wayland-*` socket it can
    /// discover is ours — never Hyprland's — which is the whole point of having
    /// a headless session.
    pub fn new(
        size: (u32, u32),
        client_scale: f64,
        runtime_dir: &std::path::Path,
        show_cursor: bool,
        use_x11: bool,
        scale: f64,
    ) -> Result<Self, CompError> {
        let display = Display::new().map_err(|e| CompError::Server(e.to_string()))?;
        // Pick a free name and bind the socket under the private runtime dir.
        let mut name = format!("wayland-{}", std::process::id());
        let mut socket_path = runtime_dir.join(&name);
        let mut attempts = 0u32;
        let socket = loop {
            match ListeningSocket::bind_absolute(socket_path.clone()) {
                Ok(s) => break s,
                Err(BindError::AlreadyInUse) => {
                    attempts += 1;
                    if attempts > 64 {
                        return Err(CompError::Server(format!(
                            "no free socket name under {}",
                            runtime_dir.display()
                        )));
                    }
                    // Some collision in our private dir; try the next suffix.
                    name = format!("wayland-{}-{attempts}", std::process::id());
                    socket_path = runtime_dir.join(&name);
                }
                Err(e) => {
                    return Err(CompError::Server(format!("bind socket: {e}")));
                }
            }
        };
        // `bind_absolute` does not populate the auto-name (that's a `bind`/
        // `bind_auto` convenience); the absolute path we bound is the socket.
        let socket_name: PathBuf = socket_path;

        let (w, h) = (size.0 as i32, size.1 as i32);
        let mode = Mode {
            size: (w, h).into(),
            refresh: 60_000,
        };
        let output = Output::new(
            "compositor-headless".into(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                model: "compositor".into(),
                make: "compositor".into(),
            },
        );
        // The third argument is the output's `Scale`, which is what a client
        // reads to decide how many buffer pixels one logical pixel is worth. It
        // is advertised here and the mode is left alone, so a client that honours
        // it draws the same interface larger rather than the window changing
        // size.
        //
        // A whole value is advertised as an integer and not a fraction: some
        // clients only understand integer scaling, and a fractional value would
        // be rounded up for them, making 2.0 arrive as 2 and 1.5 arrive as 2 as
        // well -- two different requests answered the same way.
        let advertised = if scale.fract() == 0.0 {
            smithay::output::Scale::Integer(scale as i32)
        } else {
            smithay::output::Scale::Fractional(scale)
        };
        output.change_current_state(Some(mode), None, Some(advertised), Some((0, 0).into()));
        output.set_preferred(mode);

        Ok(Server {
            display,
            socket,
            socket_name,
            output,
            size,
            scale,
            client_scale,
            x11_display: std::sync::Arc::new(std::sync::RwLock::new(None)),
            x11_failed: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            show_cursor,
            use_x11,
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

/// Turn a surface-space damage rect into output pixels, clamped to the output.
///
/// Damage arrives in surface coordinates, which are logical: one unit is one
/// interface pixel, not one output pixel, and the output's scale is what
/// converts. Clamping first would bound a logical box by a pixel count and
/// report every rect past the top-left as the wrong size.
pub fn clamp_rect(
    rect: Rectangle<i32, smithay::utils::Logical>,
    size: (u32, u32),
    scale: f64,
) -> Option<Rect> {
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    let (ow, oh) = (size.0 as f64, size.1 as f64);
    let x0 = ((rect.loc.x as f64 * scale).floor().max(0.0)) as i32;
    let y0 = ((rect.loc.y as f64 * scale).floor().max(0.0)) as i32;
    let x1 = (((rect.loc.x + rect.size.w) as f64 * scale).ceil().min(ow)) as i32;
    let y1 = (((rect.loc.y + rect.size.h) as f64 * scale).ceil().min(oh)) as i32;
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

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::utils::{Rectangle, Size};

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rectangle<i32, smithay::utils::Logical> {
        Rectangle::new((x, y).into(), Size::from((w, h)))
    }

    /// Damage is reported in surface coordinates, which are logical. One
    /// logical pixel is `scale` of the output's pixels, so a rect at the far
    /// side of the interface reaches the far side of the capture. Bounding it
    /// by the pixel count first is what reported the whole picture as damaged.
    #[test]
    fn damage_is_converted_from_surface_to_output_space() {
        let (ow, oh) = (1200, 800);
        let scale = 1.75;
        let far = clamp_rect(rect(685, 456, 1, 1), (ow, oh), scale).expect("a visible rect");
        assert_eq!(far, crate::Rect { x: 1198, y: 798, width: 2, height: 2 });

        // A whole-window damage is the whole capture, not the logical size.
        let all = clamp_rect(rect(0, 0, 686, 457), (ow, oh), scale).expect("a visible rect");
        assert_eq!(all, crate::Rect { x: 0, y: 0, width: 1200, height: 800 });
    }

    /// A rect that leaves the output is gone, not clamped to a sliver of it.
    #[test]
    fn damage_outside_the_output_is_dropped() {
        assert_eq!(clamp_rect(rect(700, 500, 10, 10), (1200, 800), 1.75), None);
        assert_eq!(clamp_rect(rect(-50, -50, 10, 10), (1200, 800), 1.75), None);
    }

    /// A rect that only partly leaves is clamped to the part that is visible.
    #[test]
    fn damage_over_the_edge_is_clamped() {
        let part = clamp_rect(rect(680, 450, 20, 20), (1200, 800), 1.75).expect("a visible rect");
        assert_eq!(part, crate::Rect { x: 1190, y: 787, width: 10, height: 13 });
    }
}
