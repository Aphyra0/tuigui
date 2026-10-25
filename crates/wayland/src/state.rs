//! Smithay handler trait implementations + the wl-thread event loop.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use smithay::input::keyboard::{FilterResult, XkbConfig};
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::reexports::calloop::{channel, generic::Generic, EventLoop, Interest, Mode};
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::wl_seat::WlSeat;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, DisplayHandle, Resource};
use smithay::utils::{Point, Serial};
use smithay::wayland::compositor::{
    BufferAssignment, CompositorClientState, CompositorHandler, CompositorState,
    SurfaceAttributes,
};
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
};
use smithay::wayland::selection::data_device::{
    ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
};
use smithay::wayland::selection::SelectionHandler;
use smithay::wayland::shm::ShmHandler;
use smithay::wayland::shm::ShmState;
use smithay::wayland::xwayland_shell::XWaylandShellState;
use smithay::xwayland::xwm::X11Wm;

use crate::server::{Request, Server, WindowShared};
use crate::surface::{commit_damage, map_rgba, Window};
use crate::{Button, CompError, Key, KeyState, ScrollDelta};

use wayland_protocols::xdg::shell::server::xdg_toplevel;

/// Delegated per-client state: tracks whether this client owns the window slot.
pub struct ClientState {
    pub compositor_state: CompositorClientState,
    /// Set once this client's toplevel was admitted (the one window).
    pub admitted: AtomicBool,
    /// Set by spawn: the size to configure the first toplevel with.
    pub configure_size: Mutex<Option<(u32, u32)>>,
}

impl ClientState {
    pub fn new() -> Arc<Self> {
        Arc::new(ClientState {
            compositor_state: CompositorClientState::default(),
            admitted: AtomicBool::new(false),
            configure_size: Mutex::new(None),
        })
    }
}

impl ClientData for ClientState {
    fn initialized(&self, _id: ClientId) {}
    fn disconnected(&self, _id: ClientId, _reason: DisconnectReason) {}
}

/// Boot monotonic instant, shared per-process, for event timestamps.
static BOOT: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

pub fn server_time() -> u32 {
    BOOT.get_or_init(Instant::now).elapsed().as_millis() as u32
}

/// Overlay a red disc of radius 3 (the software cursor) onto captured pixels at
/// `loc`, so the pointer position is visible even though the headless output has
/// no real cursor plane. `rgba` is `w * h * 4` bytes of Argb8888 little-endian.
#[tracing::instrument(name = "draw_cursor", skip(rgba))]
fn draw_cursor(rgba: &mut [u8], w: u32, h: u32, loc: Point<f64, smithay::utils::Logical>) {
    const R: f64 = 3.0;
    let (cx, cy) = (loc.x, loc.y);
    // Clamp the disc center to the visible area (minus margin so the outline
    // can't wrap).
    let (cx, cy) = (cx.max(0.0).min(w.max(1) as f64 - 1.0), cy.max(0.0).min(h.max(1) as f64 - 1.0));
    // Bounding box of the disc as integer rows/cols.
    let x0 = (cx - R).floor().max(0.0) as u32;
    let x1 = (cx + R).ceil().min(w as f64) as u32;
    let y0 = (cy - R).floor().max(0.0) as u32;
    let y1 = (cy + R).ceil().min(h as f64) as u32;
    let r2 = R * R;
    for y in y0..y1 {
        let row = (y as usize) * (w as usize) * 4;
        for x in x0..x1 {
            let dx = x as f64 - cx;
            let dy = y as f64 - cy;
            if dx * dx + dy * dy <= r2 {
                let o = row + (x as usize) * 4;
                // Argb8888 little-endian, so the bytes are B, G, R, A. Pure red.
                rgba[o] = 0;
                rgba[o + 1] = 0;
                rgba[o + 2] = 255;
                rgba[o + 3] = 255;
            }
        }
    }
}

pub struct CompState {
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub shm_state: ShmState,
    /// Clipboard / drag-and-drop. Required by GTK: it refuses to open a
    /// Wayland display entirely if `wl_data_device_manager` is absent.
    pub data_device_state: DataDeviceState,
    /// X11 surface association (the `xwayland_shell` global).
    pub xwayland_shell_state: XWaylandShellState,
    /// The running X11 window manager, once XWayland is ready.
    pub x11_wm: Option<X11Wm>,
    /// The pending X11 window candidate for the one-window slot.
    pub pending_x11: Option<smithay::xwayland::X11Surface>,
    pub seat_state: SeatState<Self>,
    pub output_state: OutputManagerState,
    /// The single present output. Used to send `wl_surface.enter` so committed
    /// surfaces know an output is attached (Firefox waits for this before the
    /// WebRender presents content).
    pub output: smithay::output::Output,
    pub seat: Seat<Self>,
    pub display_handle: DisplayHandle,
    pub server_size: (u32, u32),
    /// The admitted window (None until the first client's toplevel arrives).
    pub window: Option<Arc<Window>>,
    /// Last pointer position (output logical space).
    pub pointer_loc: Point<f64, smithay::utils::Logical>,
    /// Number of currently held pointer buttons (from click press/release).
    pub buttons: u32,
    serial: u64,
    /// Client content scale applied via `ClientHandle::set_client_scale` on
    /// admission (`--ui-scale / --scale`). Buffer sizes unaffected, so the
    /// window stays full-output.
    client_scale: f64,
    /// The scale the output advertises. What a client multiplies the size it is
    /// configured with by to get its buffer, so the configured size has to be
    /// divided by it for the buffer to be the output's resolution.
    output_scale: f64,
    /// Set false when the control channel closes to wind the loop down.
    serve: bool,
    /// Whether capture draws the software cursor overlay (from `--no-cursor`).
    show_cursor: bool,
}

impl CompState {
    /// The size a client is configured with: the output's **logical** size.
    ///
    /// A client that honours the output's scale multiplies what it is
    /// configured with to get its buffer, so configuring it with the physical
    /// resolution would have it lay its interface out at that many logical
    /// pixels and render each at the scale — the same interface, denser, rather
    /// than the larger one this is asking for. Dividing by the scale is what
    /// makes one logical pixel cover `scale` of the output's pixels.
    fn logical_size(&self) -> (u32, u32) {
        let scale = if self.output_scale.is_finite() && self.output_scale > 0.0 {
            self.output_scale
        } else {
            1.0
        };
        (
            (self.server_size.0 as f64 / scale).round().max(1.0) as u32,
            (self.server_size.1 as f64 / scale).round().max(1.0) as u32,
        )
    }

    fn next_serial(&mut self) -> Serial {
        self.serial = self.serial.wrapping_add(1);
        Serial::from(self.serial as u32)
    }

    fn focus_surface(&self) -> Option<WlSurface> {
        self.window.as_ref().map(|w| w.shared.surface.clone())
    }
}

impl smithay::wayland::buffer::BufferHandler for CompState {
    fn buffer_destroyed(&mut self, _buffer: &smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer) {}
}

impl CompositorHandler for CompState {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        // XWayland's client carries `XWaylandClientData` (not our `ClientState`)
        // with its own compositor state; our admitted clients carry `ClientState`.
        // Both must be resolved so commit processing works for either.
        if let Some(data) = client.get_data::<crate::state::ClientState>() {
            return &data.compositor_state;
        }
        if let Some(data) = client.get_data::<smithay::xwayland::XWaylandClientData>() {
            return &data.compositor_state;
        }
        // Fall back to a temporary default won't work (cannot return ref to
        // temp); treat as fatal. This is a programming error.
        panic!("client has neither ClientState nor XWaylandClientData");
    }

    #[tracing::instrument(name = "commit", skip_all)]
    fn commit(&mut self, surface: &WlSurface) {
        // Software compositing: read the committed buffer straight into our
        // owned pixel store and accumulate damage. (No renderer-side
        // on_commit_buffer_handler — we don't run a GPU renderer.)
        let out_size = self.server_size;
        let out_scale = self.output_scale;
        let Some(win) = self.window.as_ref() else { return };
        let is_win = win.shared.surface == *surface;
        let is_kid = !is_win
            && smithay::wayland::compositor::get_parent(surface)
                .map(|p| p == win.shared.surface)
                .unwrap_or(false);
        if !(is_win || is_kid) {
            return;
        }

        let mut any_pixels = false;
        let mut damage: Vec<crate::Rect> = Vec::new();
        // Buffer consumed this commit; we `memcpy` its contents in `map_rgba`,
        // so it can be released back to the client as soon as we're done.
        let mut release: Option<smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer> = None;
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
            win.shared.committed.store(true, Ordering::SeqCst);
        }
    }
}

impl ShmHandler for CompState {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl XdgShellHandler for CompState {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        tracing::debug!("compositor: new toplevel requested");
        let client = surface
            .wl_surface()
            .client()
            .expect("toplevel always has a client");
        let data = client.get_data::<ClientState>().unwrap();

        if self.window.is_some() || data.admitted.load(Ordering::SeqCst) {
            // The hard rule: one window. Anything else is rejected outright.
            tracing::warn!("compositor: rejecting extra toplevel (one-window policy)");
            surface.send_close();
            return;
        }

        let size = *data
            .configure_size
            .lock()
            .unwrap()
            .get_or_insert_with(|| self.logical_size());

        // Scale the client's content (`--ui-scale / --scale`) independently of
        // the window size: smithay maps this into the client's logical space
        // without changing its buffer size, so content stays a constant
        // on-screen size while the window keeps filling the full output.
        data.compositor_state.set_client_scale(self.client_scale);

        let shared = Arc::new(WindowShared {
            surface: surface.wl_surface().clone(),
            size,
            dead: AtomicBool::new(false),
            committed: AtomicBool::new(false),
        });
        let win = Arc::new(Window::new(shared));
        let serial = self.next_serial();
        surface.with_pending_state(|state| {
            state.size = Some((size.0 as i32, size.1 as i32).into());
            state.states.set(xdg_toplevel::State::Activated);
        });
        surface.send_configure();
        *win.last_configure_serial.lock().unwrap() = Some(u32::from(serial) as u64);
        data.admitted.store(true, Ordering::SeqCst);

        // The window now "fills" the single output. Send `wl_surface.enter` so
        // the client knows an output is attached and can start presenting.
        // Clients like Firefox's WebRender withhold content until they see this.
        self.output.enter(surface.wl_surface());

        self.window = Some(win);
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        tracing::warn!("compositor: rejecting popup (one-window policy)");
        // Send popup_done (the "dismiss" event); the client tears the popup
        // down itself. Destroying the resource server-side would be a
        // protocol error for a live popup, so we don't.
        surface.send_popup_done();
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: WlSeat, _serial: Serial) {}

    fn reposition_request(
        &mut self,
        _surface: PopupSurface,
        _positioner: PositionerState,
        _token: u32,
    ) {
    }
}

impl SeatHandler for CompState {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, _seat: &Seat<Self>, _focused: Option<&WlSurface>) {}
    fn cursor_image(
        &mut self,
        _seat: &Seat<Self>,
        _image: smithay::input::pointer::CursorImageStatus,
    ) {
    }
}

impl smithay::wayland::output::OutputHandler for CompState {}

// `wl_data_device_manager` (clipboard + drag-and-drop). The handler is what
// makes GTK apps (nautilus, etc.) actually open our Wayland display: GTK's
// `_gdk_wayland_display_open` bails with "not using Wayland display" unless a
// data device manager global (plus an xdg/zxdg shell) is present.
impl SelectionHandler for CompState {
    type SelectionUserData = ();
}
impl ClientDndGrabHandler for CompState {}
impl ServerDndGrabHandler for CompState {}
impl DataDeviceHandler for CompState {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

/// Request handlers (run on the wl thread, between event-loop turns).
impl CompState {
    pub fn handle_capture(
        &mut self,
        reply: std::sync::mpsc::Sender<Result<crate::Frame, CompError>>,
    ) {
        let _ = reply.send(self.capture_now());
    }

    #[tracing::instrument(name = "capture_now", skip_all)]
    fn capture_now(&mut self) -> Result<crate::Frame, CompError> {
        let Some(win) = self.window.as_ref() else {
            return Err(CompError::NotReady);
        };
        if win.shared.dead.load(Ordering::SeqCst) {
            return Err(CompError::WindowGone);
        }
        let pixels = win.pixels.lock().unwrap().clone();
        if pixels.is_empty() {
            return Err(CompError::NotReady);
        }
        // We accept a frame once we have pixels, even if `committed` briefly
        // reads false (e.g. the XWayland window was just replaced/remapped and
        // its first new commit hasn't landed yet). Returning the last good
        // pixels keeps the stream alive instead of idling on a fresh-slot flap.
        if !win.shared.committed.load(Ordering::SeqCst) && pixels.iter().all(|&b| b == 0) {
            return Err(CompError::NotReady);
        }
        let (w, h) = self.server_size;
        let mut rgba = pixels;
        // Overlay a software cursor so the position is visible in captures:
        // a red disc of radius 3 at the pointer's location. Skipped with
        // `--no-cursor`.
        if self.show_cursor {
            let _cursor = tracing::info_span!("draw_cursor").entered();
            draw_cursor(&mut rgba, w, h, self.pointer_loc);
        }
        let damage = std::mem::take(&mut *win.damage.lock().unwrap());
        Ok(crate::Frame {
            width: w,
            height: h,
            rgba,
            damage,
        })
    }

    pub fn handle_key(&mut self, key: Key, state: KeyState) -> Result<(), CompError> {
        let Some(surface) = self.focus_surface() else {
            return Err(CompError::NotReady);
        };
        let keyboard = self.seat.get_keyboard().ok_or(CompError::NotReady)?;
        if !keyboard.is_focused() {
            let serial = self.next_serial();
            keyboard.set_focus(self, Some(surface), serial);
        }
        let keycode = key.keycode();
        let pressed = matches!(state, KeyState::Pressed);
        let serial = self.next_serial();
        let time = server_time();
        keyboard.input(
            self,
            keycode,
            if pressed {
                smithay::backend::input::KeyState::Pressed
            } else {
                smithay::backend::input::KeyState::Released
            },
            serial,
            time,
            |_, _, _| FilterResult::Forward::<()>,
        );
        Ok(())
    }

    /// Deliver a key by its evdev code, with no character resolution here.
    ///
    /// The seat takes a keycode and derives its modifier state from the keys it
    /// sees, so anything that knows a key position can drive it directly. That
    /// is what makes a modifier and a shifted character expressible at all: the
    /// `Key` enum is a character plus a small set of named keys, and a character
    /// cannot carry a modifier.
    pub fn handle_keycode(
        &mut self,
        evdev: u32,
        state: KeyState,
    ) -> Result<(), CompError> {
        let Some(surface) = self.focus_surface() else {
            return Err(CompError::NotReady);
        };
        let keyboard = self.seat.get_keyboard().ok_or(CompError::NotReady)?;
        if !keyboard.is_focused() {
            let serial = self.next_serial();
            keyboard.set_focus(self, Some(surface), serial);
        }
        // Wayland keycodes are evdev codes plus 8 (X11's offset), which is the
        // conversion this compositor's own `Key` also performs.
        let keycode = smithay::backend::input::Keycode::new(evdev + 8);
        let serial = self.next_serial();
        let time = server_time();
        keyboard.input(
            self,
            keycode,
            match state {
                KeyState::Pressed => smithay::backend::input::KeyState::Pressed,
                KeyState::Released => smithay::backend::input::KeyState::Released,
            },
            serial,
            time,
            |_, _, _| FilterResult::Forward::<()>,
        );
        Ok(())
    }

    pub fn handle_mouse_move(&mut self, x: f64, y: f64) -> Result<(), CompError> {
        let Some(surface) = self.focus_surface() else {
            return Err(CompError::NotReady);
        };
        let (w, h) = self.server_size;
        let x = x.clamp(0.0, w.max(2) as f64 - 1.0);
        let y = y.clamp(0.0, h.max(2) as f64 - 1.0);
        let loc = Point::new(x, y);
        self.pointer_loc = loc;
        // smithay reports surface-local pointer coords as `location * scale`
        // (PointerTarget::motion -> to_client), where the scale it composes with
        // includes the output's. The location injected here is in **output**
        // pixels -- that is what the caller's cell arithmetic produces -- so the
        // two divisions below turn it into the surface-local position the client
        // expects.
        //
        // The client's own content scale is the first: it is set on admission
        // (`client_scale`) and smithay multiplies by it. The output's advertised
        // scale is the second, and it is not the same number: a client that
        // honours it lays its surface out at `output / scale` logical pixels, so
        // it reads the position in that smaller space. Dividing by only the
        // client scale leaves every coordinate off by the output scale, which is
        // why the pointer drifts away from the cursor as it moves.
        let client_scale = surface
            .client()
            .map(|c| self.client_compositor_state(&c).client_scale())
            .unwrap_or(1.0);
        let client_scale = if client_scale.is_finite() && client_scale > 0.0 {
            client_scale
        } else {
            1.0
        };
        let output_scale = if self.output_scale.is_finite() && self.output_scale > 0.0 {
            self.output_scale
        } else {
            1.0
        };
        let loc = Point::new(x / (client_scale * output_scale), y / (client_scale * output_scale));
        let pointer = self.seat.get_pointer().ok_or(CompError::NotReady)?;
        let serial = self.next_serial();
        let time = server_time();
        pointer.motion(
            self,
            Some((surface, Point::new(0.0, 0.0))),
            &smithay::input::pointer::MotionEvent {
                location: loc,
                serial,
                time,
            },
        );
        pointer.frame(self);
        Ok(())
    }

    /// Press a pointer button and leave it down.
    ///
    /// Separate from the click so a **drag** is expressible: dragging is a press,
    /// then motion with the button still held, then a release, and a single
    /// press-and-release request can never describe one -- the button is up again
    /// before the motion arrives, so the client sees a click and then a plain
    /// pointer move.
    pub fn handle_button(
        &mut self,
        button: Button,
        down: bool,
    ) -> Result<(), CompError> {
        if self.pointer_loc.x == 0.0 && self.pointer_loc.y == 0.0 {
            let (w, h) = self.server_size;
            self.handle_mouse_move(w as f64 / 2.0, h as f64 / 2.0)?;
        }
        let pointer = self.seat.get_pointer().ok_or(CompError::NotReady)?;
        let time = server_time();
        let serial = self.next_serial();
        if down {
            self.buttons = self.buttons.saturating_add(1);
            // Establish the keyboard focus on press, like anvil's
            // `on_pointer_button`: a real client routes clicks/hover through the
            // active window, and without a prior key event it never became active.
            if let (Some(surface), Some(kbd)) = (self.focus_surface(), self.seat.get_keyboard())
            {
                if !kbd.is_focused() {
                    kbd.set_focus(self, Some(surface), serial);
                }
            }
        } else {
            self.buttons = self.buttons.saturating_sub(1);
        }
        pointer.button(
            self,
            &smithay::input::pointer::ButtonEvent {
                serial,
                time,
                button: button.evdev_code(),
                state: if down {
                    smithay::backend::input::ButtonState::Pressed
                } else {
                    smithay::backend::input::ButtonState::Released
                },
            },
        );
        pointer.frame(self);
        Ok(())
    }


    pub fn handle_scroll(&mut self, delta: ScrollDelta) -> Result<(), CompError> {
        if self.pointer_loc.x == 0.0 && self.pointer_loc.y == 0.0 {
            let (w, h) = self.server_size;
            self.handle_mouse_move(w as f64 / 2.0, h as f64 / 2.0)?;
        }
        let pointer = self.seat.get_pointer().ok_or(CompError::NotReady)?;
        let (steps_v, steps_h) = (delta.vertical, delta.horizontal);
        let time = server_time();
        let mut frame = smithay::input::pointer::AxisFrame::new(time)
            .source(smithay::backend::input::AxisSource::Wheel);
        if steps_v != 0 {
            frame = frame
                .value(smithay::backend::input::Axis::Vertical, steps_v as f64 * 10.0)
                .v120(smithay::backend::input::Axis::Vertical, steps_v);
        }
        if steps_h != 0 {
            frame = frame
                .value(
                    smithay::backend::input::Axis::Horizontal,
                    steps_h as f64 * 10.0,
                )
                .v120(smithay::backend::input::Axis::Horizontal, steps_h);
        }
        pointer.axis(self, frame);
        pointer.frame(self);
        Ok(())
    }

    pub fn handle_pointer_state(&self) -> crate::PointerState {
        let (x, y) = (self.pointer_loc.x, self.pointer_loc.y);
        crate::PointerState {
            x,
            y,
            button_down: self.buttons > 0,
        }
    }
}

/// Spawn the XWayland server and register it as an event source on `handle`.
///
/// The `XWayland` handle is owned by the event loop (as the inserted source),
/// keeping the X server alive for the thread's lifetime. On `Ready` we start
/// the X11 WM and advertise `DISPLAY` so spawned clients can reach it.
///
/// No-op if `Xwayland` cannot be spawned; X11 support is best-effort and must
/// never take the headless session down.
fn spawn_xwayland(
    server: &Server,
    handle: smithay::reexports::calloop::LoopHandle<'static, CompState>,
) -> Result<(), CompError> {
    let xwayland = match smithay::xwayland::XWayland::spawn(
        &server.display.handle(),
        None,
        // Sleep between discrete display slots? Let smithay pick a free one.
        [] as [(&str, &str); 0],
        true,
        std::process::Stdio::null(),
        // Discard Xwayland's own stderr (xkbcomp warnings, glamor notices):
        // it would otherwise flood the TUI's alternate screen.
        std::process::Stdio::null(),
        |_| {},
    ) {
        Ok((xwayland, client)) => (xwayland, client),
        Err(e) => {
            tracing::warn!("xwayland: could not spawn Xwayland (X11 apps disabled): {e}");
            server.x11_failed.store(true, std::sync::atomic::Ordering::Release);
            return Ok(());
        }
    };
    let (xwayland, client) = xwayland;

    let x11_display = server.x11_display.clone();
    let x11_failed = server.x11_failed.clone();
    let wm_handle = handle.clone();
    handle
        .insert_source(xwayland, move |event, _, state: &mut CompState| {
            match event {
                smithay::xwayland::XWaylandEvent::Ready {
                    x11_socket,
                    display_number,
                } => {
                    tracing::info!(display_number, "xwayland: X server ready");
                    // Start the X11 window manager. It drives window mapping
                    // and funnels every window's drawing to a wl_surface that
                    // our normal commit/capture path sees.
                    match smithay::xwayland::X11Wm::start_wm(wm_handle.clone(), x11_socket, client.clone()) {
                        Ok(wm) => {
                            state.x11_wm = Some(wm);
                            // Only once the WM holds `WM_S0` is `DISPLAY=:n`
                            // advertised and `Compositor::spawn` unblocks. This
                            // guarantees the X11 client is never launched before
                            // XWayland can serve it.
                            if let Ok(mut guard) = x11_display.write() {
                                *guard = Some(format!(":{display_number}"));
                            }
                            tracing::info!("xwayland: X11 window manager started; display :{display_number} available");
                        }
                        Err(e) => {
                            tracing::error!("xwayland: failed to start X11 WM: {e}");
                            x11_failed.store(true, std::sync::atomic::Ordering::Release);
                        }
                    }
                }
                smithay::xwayland::XWaylandEvent::Error => {
                    tracing::warn!("xwayland: X server exited unexpectedly during startup");
                    x11_failed.store(true, std::sync::atomic::Ordering::Release);
                }
            }
        })
        .map_err(|e| CompError::Server(format!("xwayland event source: {e}")))?;

    Ok(())
}

/// Fire all pending frame callbacks on the window's surface tree.
fn fire_frame_callbacks(surface: &WlSurface) {
    smithay::wayland::compositor::with_surface_tree_downward(
        surface,
        (),
        |_, _, _| smithay::wayland::compositor::TraversalAction::DoChildren(()),
        |_, states, _| {
            let mut attrs = states.cached_state.get::<SurfaceAttributes>();
            let now = server_time();
            for cb in attrs.current().frame_callbacks.drain(..) {
                cb.done(now);
            }
        },
        |_, _, _| true,
    );
}

/// The wl-thread loop: owns `CompState` + `Server`, services the control
/// The wl-thread loop: owns `CompState` + `Server`, services the control
/// channel and the wayland socket.
pub fn run_thread(
    mut server: Server,
    rx: channel::Channel<Request>,
    pending_windows: std::sync::mpsc::Receiver<std::os::unix::net::UnixStream>,
) -> Result<(), CompError> {
    let mut event_loop: EventLoop<'static, CompState> =
        EventLoop::try_new().map_err(|e| CompError::Server(e.to_string()))?;
    let handle = event_loop.handle();

    let mut state = init_state(&mut server)?;
    state.show_cursor = server.show_cursor;

    // Advertise the output global.
    let _output_global = server
        .output
        .create_global::<CompState>(&server.display.handle());

    // Advertise the xwayland_shell global (created in init_state) so XWayland
    // can associate X11 windows with wl_surfaces. XWaylandShellState::new
    // already created the global on the display handle.

    // Spawn the XWayland server, if available and enabled. X11-only apps
    // (glxgears, xterm, old toolkits) reach it through `DISPLAY`; its rendering
    // is redirected onto wl_surfaces that flow through our normal capture path.
    // With `--no-x` (`server.use_x11` false) we skip it so only native Wayland
    // clients are supported and `x11_failed` is never set.
    if server.use_x11 {
        let xwayland = spawn_xwayland(&server, handle.clone())?;
        let _ = xwayland;
    }

    // Accept new wayland clients from the listening socket. calloop needs an
    // owned source, so the whole Server lives in an Arc<Mutex> shared between
    // the accept callback and the dispatch loop below.
    let shared_server = std::sync::Arc::new(std::sync::Mutex::new(server));

    struct SocketSource(std::sync::Arc<std::sync::Mutex<Server>>);
    impl std::os::unix::io::AsFd for SocketSource {
        fn as_fd(&self) -> std::os::unix::io::BorrowedFd<'_> {
            // The Mutex guard can't be returned, so hand calloop a borrowed
            // view of the raw fd: it is only polled for readability, never
            // read/written through. The socket outlives the poll call.
            use std::os::fd::AsRawFd;
            let fd = self.0.lock().unwrap().socket.as_raw_fd();
            unsafe { std::os::unix::io::BorrowedFd::borrow_raw(fd) }
        }
    }

    let listener = Generic::new(
        SocketSource(shared_server.clone()),
        Interest::READ,
        Mode::Level,
    );
    let accept_socket = shared_server.clone();
    handle
        .insert_source(listener, move |_, _, state: &mut CompState| {
            let Ok(server) = accept_socket.lock() else {
                return Ok(smithay::reexports::calloop::PostAction::Continue);
            };
            while let Ok(Some(stream)) = server.socket.accept() {
                tracing::debug!("compositor: new client connection accepted");
                let data = ClientState::new();
                *data.configure_size.lock().unwrap() = Some(state.logical_size());
                if let Err(e) = state.display_handle.insert_client(stream, data) {
                    tracing::warn!("insert_client failed: {e}");
                }
            }
            Ok(smithay::reexports::calloop::PostAction::Continue)
        })
        .map_err(|e| CompError::Server(e.to_string()))?;

    // Control channel from `Compositor`/`Client` API objects.
    handle
        .insert_source(rx, |event, _, state: &mut CompState| {
            let smithay::reexports::calloop::channel::Event::Msg(req) = event else {
                // Channel closed: the API side dropped its last sender (or
                // `Compositor::shutdown`), so there is nothing left to service.
                // Signal the loop below to exit.
                state.serve = false;
                return;
            };
            match req {
            Request::Capture { reply } => state.handle_capture(reply),
            Request::Key {
                key,
                state: ks,
                reply,
            } => {
                let _ = reply.send(state.handle_key(key, ks));
            }
            Request::Keycode {
                evdev,
                state: ks,
                reply,
            } => {
                let _ = reply.send(state.handle_keycode(evdev, ks));
            }
            Request::MouseMove { x, y, reply } => {
                let _ = reply.send(state.handle_mouse_move(x, y));
            }
            Request::Button { button, down, reply } => {
                let _ = reply.send(state.handle_button(button, down));
            }
            Request::Scroll { delta, reply } => {
                let _ = reply.send(state.handle_scroll(delta));
            }
            Request::PointerState { reply } => {
                let _ = reply.send(Ok(state.handle_pointer_state()));
            }
            }
        })
        .map_err(|e| CompError::Server(e.to_string()))?;

    loop {
        // The API side dropped the last control sender (shutdown): wind down.
        if !state.serve {
            break Ok(());
        }
        // 1. Adopt any client sockets spawned outside the thread (spawn() path).
        while let Ok(stream) = pending_windows.try_recv() {
            let data = ClientState::new();
            *data.configure_size.lock().unwrap() = Some(state.logical_size());
            if let Err(e) = state
                .display_handle
                .insert_client(stream, data)
            {
                tracing::warn!("insert_client failed: {e}");
            }
        }

        // 2. Protocol dispatch + a short event-loop turn (keeps control
        //    latency low while idling efficiently).
        {
                let server = shared_server.clone();
            let mut server = server.lock().unwrap();
            let _n = server.display.dispatch_clients(&mut state).map_err(|e| CompError::Server(e.to_string()))?;
            server.display.flush_clients().map_err(|e| CompError::Server(e.to_string()))?;
        }
        event_loop
            .dispatch(Duration::from_millis(4), &mut state)
            .map_err(|e| CompError::Server(e.to_string()))?;

        // 3. Frame-callback emulator: fire pending done() events each turn.
        if let Some(win) = &state.window {
            if !win.shared.dead.load(Ordering::SeqCst) {
                fire_frame_callbacks(&win.shared.surface);
                let shared = shared_server.clone();
                let mut server = shared.lock().unwrap();
                display_dispatch(&mut server, &mut state)?;
            }
        }
    }
}


/// Dispatch + flush pending wayland protocol work.
fn display_dispatch(server: &mut Server, state: &mut CompState) -> Result<(), CompError> {
    server.display
        .dispatch_clients(state)
        .map_err(|e| CompError::Server(e.to_string()))?;
    server.display
        .flush_clients()
        .map_err(|e| CompError::Server(e.to_string()))?;
    Ok(())
}

fn init_state(server: &mut Server) -> Result<CompState, CompError> {
    let dh = server.display.handle();
    let mut seat_state = SeatState::new();
    let mut seat = seat_state.new_wl_seat(&dh, "compositor");
    let result = CompState {
        compositor_state: CompositorState::new::<CompState>(&dh),
        xdg_shell_state: XdgShellState::new::<CompState>(&dh),
        shm_state: ShmState::new::<CompState>(
            &dh,
            vec![
                smithay::reexports::wayland_server::protocol::wl_shm::Format::Argb8888,
                smithay::reexports::wayland_server::protocol::wl_shm::Format::Xrgb8888,
            ],
        ),
        data_device_state: DataDeviceState::new::<CompState>(&dh),
        // The xwayland_shell global is what lets X11 apps (routed through
        // XWayland) hand us their surfaces. Its global is created here; the
        // XWayland server and the X11 WM are spawned from `run_thread`.
        xwayland_shell_state: XWaylandShellState::new::<CompState>(&dh),
        x11_wm: None,
        pending_x11: None,
        seat_state,
        output_state: OutputManagerState::new_with_xdg_output::<CompState>(&dh),
        output: server.output.clone(),
        seat: seat.clone(),
        display_handle: dh.clone(),
        server_size: server.size,
        window: None,
        pointer_loc: Point::new(0.0, 0.0),
        buttons: 0,
        serial: 0,
        client_scale: server.client_scale,
        output_scale: server.scale,
        serve: true,
        show_cursor: false,
    };
    seat.add_keyboard(XkbConfig::default(), 200, 25)
        .map_err(|e| CompError::Server(format!("keyboard init: {e}")))?;
    seat.add_pointer();
    Ok(result)
}

// Delegate macros: one per protocol global we run.
smithay::delegate_compositor!(CompState);
smithay::delegate_shm!(CompState);
smithay::delegate_seat!(CompState);
smithay::delegate_data_device!(CompState);
smithay::delegate_xdg_shell!(CompState);
smithay::delegate_output!(CompState);
smithay::delegate_xwayland_shell!(CompState);
