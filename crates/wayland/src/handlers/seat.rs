//! The seat handler: smithay's registration hooks, not the input dispatch.
//!
//! The real input work lives in `handlers/inputs.rs` (drain + dispatch); this
//! trait impl is what the seat stack needs our state to provide.

use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;

use crate::state::State;

impl<C: Send + Sync + 'static + Default, W: Send + Sync + 'static + Default> SeatHandler
    for State<C, W>
{
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.globals.seat_state
    }

    fn focus_changed(&mut self, _seat: &Seat<Self>, _focused: Option<&WlSurface>) {}
    fn cursor_image(
        &mut self,
        _seat: &Seat<Self>,
        _image: smithay::input::pointer::CursorImageStatus,
    ) {
    }
}
