//! A minimal wayland client used by the compositor crate's tests: creates an
//! xdg_toplevel, draws a solid color via wl_shm, and repaints on input
//! events (key, click, scroll).

use std::os::unix::io::AsFd;

use wayland_client::globals::GlobalListContents;
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_keyboard, wl_pointer, wl_registry, wl_seat, wl_shm,
    wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

const WIDTH: u32 = 320;
const HEIGHT: u32 = 240;
const STRIDE: i32 = (WIDTH * 4) as i32;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Scene {
    #[default]
    Start,
    Key,
    Click,
    Scroll,
}

fn scene_color(scene: Scene) -> [u8; 3] {
    match scene {
        Scene::Start => [0x10, 0x20, 0x40],
        Scene::Key => [0x40, 0x20, 0x10],
        Scene::Click => [0x10, 0x60, 0x10],
        Scene::Scroll => [0x60, 0x60, 0x10],
    }
}

#[derive(Default)]
struct Smoke {
    scene: Scene,
    needs_repaint: bool,
    configured: bool,
    last_serial: u32,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Smoke {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = event {
            eprintln!("SMOKE GLOBAL {name}: {interface} v{version}");
        }
    }
}

impl Dispatch<wl_compositor::WlCompositor, ()> for Smoke {
    fn event(
        _: &mut Self,
        _: &wl_compositor::WlCompositor,
        _: wl_compositor::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for Smoke {
    fn event(
        _: &mut Self,
        _: &wl_surface::WlSurface,
        _: wl_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_shm::WlShm, ()> for Smoke {
    fn event(
        _: &mut Self,
        _: &wl_shm::WlShm,
        _: wl_shm::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_shm_pool::WlShmPool, ()> for Smoke {
    fn event(
        _: &mut Self,
        _: &wl_shm_pool::WlShmPool,
        _: wl_shm_pool::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_buffer::WlBuffer, ()> for Smoke {
    fn event(
        _: &mut Self,
        _: &wl_buffer::WlBuffer,
        _: wl_buffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for Smoke {
    fn event(
        _: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities { capabilities: WEnum::Value(_caps) } = event {
        }
        let _ = seat;
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for Smoke {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Key {
            state: WEnum::Value(s),
            ..
        } = event
        {
            if s == wl_keyboard::KeyState::Pressed {
                state.scene = Scene::Key;
                state.needs_repaint = true;
            }
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for Smoke {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Button {
                state: WEnum::Value(s),
                ..
            } => {
                if s == wl_pointer::ButtonState::Pressed {
                    state.scene = Scene::Click;
                    state.needs_repaint = true;
                }
            }
            wl_pointer::Event::Axis { .. } => {
                state.scene = Scene::Scroll;
                state.needs_repaint = true;
            }
            _ => {}
        }
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for Smoke {
    fn event(
        _: &mut Self,
        wm_base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for Smoke {
    fn event(
        state: &mut Self,
        _: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            state.last_serial = serial;
            state.configured = true;
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for Smoke {
    fn event(
        _: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        _: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

fn repaint(
    scene: Scene,
    shm: &wl_shm::WlShm,
    surface: &wl_surface::WlSurface,
    qh: &QueueHandle<Smoke>,
) {
    let memfd =
        rustix::fs::memfd_create("comp-smoke", rustix::fs::MemfdFlags::CLOEXEC)
            .expect("memfd");
    rustix::fs::ftruncate(&memfd, (STRIDE * HEIGHT as i32) as u64).expect("ftruncate");
    let mut mmap = unsafe {
        memmap2::MmapOptions::new()
            .len((STRIDE * HEIGHT as i32) as usize)
            .map_mut(&memfd)
            .expect("mmap")
    };
    let color = scene_color(scene);
    for row in 0..HEIGHT as usize {
        for col in 0..WIDTH as usize {
            let o = row * STRIDE as usize + col * 4;
            mmap[o] = color[0];
            mmap[o + 1] = color[1];
            mmap[o + 2] = color[2];
            mmap[o + 3] = 0xff;
        }
    }
    mmap.flush().expect("flush mmap");
    let pool = shm.create_pool(memfd.as_fd(), STRIDE * HEIGHT as i32, qh, ());
    let buffer = pool.create_buffer(
        0,
        WIDTH as i32,
        HEIGHT as i32,
        STRIDE,
        wl_shm::Format::Argb8888,
        qh,
        (),
    );
    surface.attach(Some(&buffer), 0, 0);
    surface.damage(0, 0, WIDTH as i32, HEIGHT as i32);
    surface.commit();
}

fn main() {
    let conn = Connection::connect_to_env().expect("connect");
    let (globals, mut event_queue) =
        wayland_client::globals::registry_queue_init::<Smoke>(&conn).expect("registry init");
    let qh = event_queue.handle();
    {
        let list = globals.contents().clone_list();
        for g in list {
            eprintln!("SMOKE GLOBAL {}: {} v{}", g.name, g.interface, g.version);
        }
    }

    let compositor = globals
        .bind::<wl_compositor::WlCompositor, Smoke, _>(&qh, 1..=6, ())
        .expect("bind compositor");
    let shm = globals
        .bind::<wl_shm::WlShm, Smoke, _>(&qh, 1..=1, ())
        .expect("bind shm");
    let seat = globals
        .bind::<wl_seat::WlSeat, Smoke, _>(&qh, 1..=9, ())
        .expect("bind seat");
    let wm_base = globals
        .bind::<xdg_wm_base::XdgWmBase, Smoke, _>(&qh, 1..=6, ())
        .expect("bind xdg_wm_base");

    let _keyboard = seat.get_keyboard(&qh, ());
    let _pointer = seat.get_pointer(&qh, ());

    let surface = compositor.create_surface(&qh, ());
    let xdg = wm_base.get_xdg_surface(&surface, &qh, ());
    let _toplevel = xdg.get_toplevel(&qh, ());

    // Initial commit (unconfigured); the configure comes back as an event.
    surface.commit();

    let mut state = Smoke::default();
    let mut acked = false;
    loop {
        event_queue.blocking_dispatch(&mut state).expect("dispatch");
        if state.configured && !acked {
            // Ack the configure, then attach the first buffer.
            xdg.ack_configure(state.last_serial);
            acked = true;
            repaint(Scene::Start, &shm, &surface, &qh);
            state.configured = false;
            continue;
        }
        if state.needs_repaint {
            repaint(state.scene, &shm, &surface, &qh);
            state.needs_repaint = false;
        }
    }
}
