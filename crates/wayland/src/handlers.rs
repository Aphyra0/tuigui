//! Smithay handler impls on [`State`].
//!
//! The delegate impls are written with the forwarding macros from
//! `crate::utils` (`forward!`/`forward_global!`) — the same impls smithay's
//! `delegate_*!` macros would emit, carrying our generics, which the macros
//! cannot. Traits whose impl carries real work get their own file in
//! `handlers/`.

mod compositor;
mod inputs;
mod seat;
mod xdg_shell;
mod xwayland;

use smithay::input::{SeatHandler, SeatState};
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_compositor::WlCompositor;
use smithay::reexports::wayland_server::protocol::wl_data_device::WlDataDevice;
use smithay::reexports::wayland_server::protocol::wl_data_device_manager::WlDataDeviceManager;
use smithay::reexports::wayland_server::protocol::wl_data_source::WlDataSource;
use smithay::reexports::wayland_server::protocol::wl_keyboard::WlKeyboard;
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::protocol::wl_pointer::WlPointer;
use smithay::reexports::wayland_server::protocol::wl_region::WlRegion;
use smithay::reexports::wayland_server::protocol::wl_seat::WlSeat;
use smithay::reexports::wayland_server::protocol::wl_shm::WlShm;
use smithay::reexports::wayland_server::protocol::wl_shm_pool::WlShmPool;
use smithay::reexports::wayland_server::protocol::wl_subcompositor::WlSubcompositor;
use smithay::reexports::wayland_server::protocol::wl_subsurface::WlSubsurface;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::protocol::wl_touch::WlTouch;
use smithay::reexports::wayland_protocols::xwayland::shell::v1::server::xwayland_shell_v1::XwaylandShellV1;
use smithay::reexports::wayland_protocols::xwayland::shell::v1::server::xwayland_surface_v1::XwaylandSurfaceV1;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_popup::XdgPopup;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_positioner::XdgPositioner;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_surface::XdgSurface;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::XdgToplevel;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_wm_base::XdgWmBase;
use smithay::reexports::wayland_protocols::xdg::xdg_output::zv1::server::zxdg_output_manager_v1::ZxdgOutputManagerV1;
use smithay::reexports::wayland_protocols::xdg::xdg_output::zv1::server::zxdg_output_v1::ZxdgOutputV1;
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{CompositorState, RegionUserData, SubsurfaceUserData, SurfaceUserData};
use smithay::wayland::output::{
    OutputHandler, OutputManagerState, OutputUserData, WlOutputData, XdgOutputUserData,
};
use smithay::wayland::selection::data_device::{
    ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, DataDeviceUserData,
    DataSourceUserData, ServerDndGrabHandler,
};
use smithay::wayland::selection::SelectionHandler;
use smithay::wayland::seat::{
    KeyboardUserData, PointerUserData, SeatGlobalData, SeatUserData, TouchUserData,
};
use smithay::wayland::shell::xdg::{
    XdgPositionerUserData, XdgShellState, XdgShellSurfaceUserData, XdgSurfaceUserData,
    XdgWmBaseUserData,
};
use smithay::wayland::shm::{ShmBufferUserData, ShmHandler, ShmPoolUserData, ShmState};
use smithay::wayland::xwayland_shell::{XWaylandShellState, XWaylandSurfaceUserData};

use crate::state::State;
use crate::utils::{forward, forward_global};
use proto::Shared;

// wl_compositor and its surfaces.
forward_global!(WlCompositor: () => CompositorState);
forward!(WlCompositor: () => CompositorState);
forward!(WlSurface: SurfaceUserData => CompositorState);
forward!(WlRegion: RegionUserData => CompositorState);
forward_global!(WlSubcompositor: () => CompositorState);
forward!(WlSubcompositor: () => CompositorState);
forward!(WlSubsurface: SubsurfaceUserData => CompositorState);
forward!(smithay::reexports::wayland_server::protocol::wl_callback::WlCallback: () => CompositorState);

// xdg_shell.
forward_global!(XdgWmBase: () => XdgShellState);
forward!(XdgWmBase: XdgWmBaseUserData => XdgShellState);
forward!(XdgSurface: XdgSurfaceUserData => XdgShellState);
forward!(XdgToplevel: XdgShellSurfaceUserData => XdgShellState);
forward!(XdgPopup: XdgShellSurfaceUserData => XdgShellState);
forward!(XdgPositioner: XdgPositionerUserData => XdgShellState);

// wl_shm: the only buffer path the MVP admits.
forward_global!(WlShm: () => ShmState);
forward!(WlShm: () => ShmState);
forward!(WlShmPool: ShmPoolUserData => ShmState);
forward!(WlBuffer: ShmBufferUserData => ShmState);

// The seat and its input resources. `SeatState` is smithay's input-side
// holder, which is what implements these dispatches (the `wayland::seat` one
// is private).
forward!(WlSeat: SeatUserData<Self> => smithay::input::SeatState<State<C, W>>);
forward_global!(WlSeat: SeatGlobalData<Self> => smithay::input::SeatState<State<C, W>>);
forward!(WlKeyboard: KeyboardUserData<Self> => smithay::input::SeatState<State<C, W>>);
forward!(WlPointer: PointerUserData<Self> => smithay::input::SeatState<State<C, W>>);
forward!(WlTouch: TouchUserData<Self> => smithay::input::SeatState<State<C, W>>);

// Clipboard / drag-and-drop.
forward_global!(WlDataDeviceManager: () => DataDeviceState);
forward!(WlDataDeviceManager: () => DataDeviceState);
forward!(WlDataDevice: DataDeviceUserData => DataDeviceState);
forward!(WlDataSource: DataSourceUserData => DataDeviceState);

// The output and its xdg_output extension.
forward_global!(WlOutput: WlOutputData => OutputManagerState);
forward!(WlOutput: OutputUserData => OutputManagerState);
forward_global!(ZxdgOutputManagerV1: () => OutputManagerState);
forward!(ZxdgOutputManagerV1: () => OutputManagerState);
forward!(ZxdgOutputV1: XdgOutputUserData => OutputManagerState);

// XWayland's surface association.
forward_global!(XwaylandShellV1: () => XWaylandShellState);
forward!(XwaylandShellV1: () => XWaylandShellState);
forward!(XwaylandSurfaceV1: XWaylandSurfaceUserData => XWaylandShellState);

// The trivial handler hooks: registration, not policy.
impl<C: Shared + Default, W: Shared + Default> BufferHandler
    for State<C, W>
{
    fn buffer_destroyed(&mut self, _buffer: &WlBuffer) {}
}

impl<C: Shared + Default, W: Shared + Default> ShmHandler
    for State<C, W>
{
    fn shm_state(&self) -> &ShmState {
        &self.globals.shm_state
    }
}

impl<C: Shared + Default, W: Shared + Default> OutputHandler
    for State<C, W>
{
}

// `wl_data_device_manager` (clipboard + drag-and-drop). The handler is what
// makes GTK apps (nautilus, etc.) actually open our Wayland display: GTK's
// `_gdk_wayland_display_open` bails with "not using Wayland display" unless a
// data device manager global (plus an xdg/zxdg shell) is present.
impl<C: Shared + Default, W: Shared + Default> SelectionHandler
    for State<C, W>
{
    type SelectionUserData = ();
}
impl<C: Shared + Default, W: Shared + Default> ClientDndGrabHandler
    for State<C, W>
{
}
impl<C: Shared + Default, W: Shared + Default> ServerDndGrabHandler
    for State<C, W>
{
}
impl<C: Shared + Default, W: Shared + Default> DataDeviceHandler
    for State<C, W>
{
    fn data_device_state(&self) -> &DataDeviceState {
        &self.globals.data_device_state
    }
}
