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
use smithay::wayland::shm::ShmHandler;
use smithay::wayland::shm::ShmState;

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

/// Overlay a red disc of radius 3 (the software cursor) onto captured RGBA
/// pixels at `loc`, so the pointer position is visible even though the
/// headless output has no real cursor plane. `rgba` is `w * h * 4` RGBA8.
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
                // RGBA8: R, G, B, A. Pure red cursor.
                rgba[o] = 255;
                rgba[o + 1] = 0;
                rgba[o + 2] = 0;
                rgba[o + 3] = 255;
            }
        }
    }
}

pub struct CompState {
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub shm_state: ShmState,
    pub seat_state: SeatState<Self>,
    pub output_state: OutputManagerState,
    pub seat: Seat<Self>,
    pub display_handle: DisplayHandle,
    pub server_size: (u32, u32),
    /// The admitted window (None until the first client's toplevel arrives).
    pub window: Option<Arc<Window>>,
    /// Last pointer position (output logical space).
    pub pointer_loc: Point<f64, smithay::utils::Logical>,
    serial: u64,
    /// Set false when the control channel closes to wind the loop down.
    serve: bool,
}

impl CompState {
    fn next_serial(&mut self) -> Serial {
        self.serial = self.serial.wrapping_add(1);
        Serial::from(self.serial as u32)
    }

    fn focus_surface(&self) -> Option<WlSurface> {
        self.window
            .as_ref()
            .map(|w| w.shared.toplevel.wl_surface().clone())
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
        &client.get_data::<ClientState>().unwrap().compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        // Software compositing: read the committed buffer straight into our
        // owned pixel store and accumulate damage. (No renderer-side
        // on_commit_buffer_handler — we don't run a GPU renderer.)
        let out_size = self.server_size;
        let Some(win) = self.window.as_ref() else { return };
        let is_window_surface = win.shared.toplevel.wl_surface() == surface;
        let is_child = !is_window_surface
            && smithay::wayland::compositor::get_parent(surface)
                .map(|p| p == *win.shared.toplevel.wl_surface())
                .unwrap_or(false);
        if !(is_window_surface || is_child) {
            return;
        }

        let mut any_pixels = false;
        let mut damage: Vec<crate::Rect> = Vec::new();
        smithay::wayland::compositor::with_states(surface, |states| {
            let mut attrs = states.cached_state.get::<SurfaceAttributes>();
            if let Some(BufferAssignment::NewBuffer(buffer)) = &attrs.current().buffer {
                let mut px = win.pixels.lock().unwrap();
                if map_rgba(buffer, &mut px, out_size).is_ok() {
                    any_pixels = true;
                }
            }
            commit_damage(&attrs.current(), out_size, &mut damage);
        });

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
        tracing::debug!("comp: new toplevel requested");
        let client = surface
            .wl_surface()
            .client()
            .expect("toplevel always has a client");
        let data = client.get_data::<ClientState>().unwrap();

        if self.window.is_some() || data.admitted.load(Ordering::SeqCst) {
            // The hard rule: one window. Anything else is rejected outright.
            tracing::warn!("comp: rejecting extra toplevel (one-window policy)");
            surface.send_close();
            return;
        }

        let size = *data
            .configure_size
            .lock()
            .unwrap()
            .get_or_insert(self.server_size);

        let shared = Arc::new(WindowShared {
            toplevel: surface.clone(),
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
        self.window = Some(win);
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        tracing::warn!("comp: rejecting popup (one-window policy)");
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

/// Request handlers (run on the wl thread, between event-loop turns).
impl CompState {
    pub fn handle_capture(
        &mut self,
        reply: std::sync::mpsc::Sender<Result<crate::Frame, CompError>>,
    ) {
        let _ = reply.send(self.capture_now());
    }

    fn capture_now(&mut self) -> Result<crate::Frame, CompError> {
        let Some(win) = self.window.as_ref() else {
            return Err(CompError::NotReady);
        };
        if !win.shared.committed.load(Ordering::SeqCst) {
            return Err(CompError::NotReady);
        }
        if win.shared.dead.load(Ordering::SeqCst) {
            return Err(CompError::WindowGone);
        }
        let pixels = win.pixels.lock().unwrap().clone();
        if pixels.is_empty() {
            return Err(CompError::NotReady);
        }
        let (w, h) = self.server_size;
        let mut rgba = pixels;
        // Overlay a software cursor so the position is visible in captures:
        // a red disc of radius 3 at the pointer's location.
        draw_cursor(&mut rgba, w, h, self.pointer_loc);
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

    pub fn handle_mouse_move(&mut self, x: f64, y: f64) -> Result<(), CompError> {
        let Some(surface) = self.focus_surface() else {
            return Err(CompError::NotReady);
        };
        let (w, h) = self.server_size;
        let x = x.clamp(0.0, w.max(2) as f64 - 1.0);
        let y = y.clamp(0.0, h.max(2) as f64 - 1.0);
        let loc = Point::new(x, y);
        self.pointer_loc = loc;
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

    pub fn handle_click(&mut self, button: Button) -> Result<(), CompError> {
        // Ensure the pointer is somewhere sane first (defaults to center).
        if self.pointer_loc.x == 0.0 && self.pointer_loc.y == 0.0 {
            let (w, h) = self.server_size;
            self.handle_mouse_move(w as f64 / 2.0, h as f64 / 2.0)?;
        }
        let pointer = self.seat.get_pointer().ok_or(CompError::NotReady)?;
        let time = server_time();
        let code = button.evdev_code();
        let press_serial = self.next_serial();
        pointer.button(
            self,
            &smithay::input::pointer::ButtonEvent {
                serial: press_serial,
                time,
                button: code,
                state: smithay::backend::input::ButtonState::Pressed,
            },
        );
        pointer.frame(self);
        let release_serial = self.next_serial();
        pointer.button(
            self,
            &smithay::input::pointer::ButtonEvent {
                serial: release_serial,
                time,
                button: code,
                state: smithay::backend::input::ButtonState::Released,
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
    let mut event_loop: EventLoop<'_, CompState> =
        EventLoop::try_new().map_err(|e| CompError::Server(e.to_string()))?;
    let handle = event_loop.handle();

    let mut state = init_state(&mut server)?;

    // Advertise the output global.
    let _output_global = server
        .output
        .create_global::<CompState>(&server.display.handle());

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
                tracing::debug!("comp: new client connection accepted");
                let data = ClientState::new();
                *data.configure_size.lock().unwrap() = Some(state.server_size);
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
            Request::MouseMove { x, y, reply } => {
                let _ = reply.send(state.handle_mouse_move(x, y));
            }
            Request::Click { button, reply } => {
                let _ = reply.send(state.handle_click(button));
            }
            Request::Scroll { delta, reply } => {
                let _ = reply.send(state.handle_scroll(delta));
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
            *data.configure_size.lock().unwrap() = Some(state.server_size);
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
                fire_frame_callbacks(win.shared.toplevel.wl_surface());
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
    let mut seat = seat_state.new_wl_seat(&dh, "tuigui-comp");
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
        seat_state,
        output_state: OutputManagerState::new_with_xdg_output::<CompState>(&dh),
        seat: seat.clone(),
        display_handle: dh.clone(),
        server_size: server.size,
        window: None,
        pointer_loc: Point::new(0.0, 0.0),
        serial: 0,
        serve: true,
    };
    seat.add_keyboard(XkbConfig::default(), 200, 25)
        .map_err(|e| CompError::Server(format!("keyboard init: {e}")))?;
    seat.add_pointer();
    Ok(result)
}

// Delegate macros: one per protocol global we run.
smithay::delegate_compositor!(CompState);
smithay::delegate_shm!(CompState);
smithay::delegate_xdg_shell!(CompState);
smithay::delegate_seat!(CompState);
smithay::delegate_output!(CompState);
