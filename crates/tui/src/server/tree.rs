//! The window tree, as this side mirrors it.
//!
//! The compositor owns the tree; the server rebuilds it from what arrives on the
//! link. Only two facts travel — a client's pid and its root window's
//! [`SurfaceId`] — because those are the two the server needs to address either:
//!
//! - a helper is matched to its parent **by pid**, which is the same rule the
//!   compositor admits connections by, so the two trees agree without a third
//!   identifier being invented;
//! - a window is found **by its cells**, so its surface is its identity.
//!
//! A client owns exactly one window. A second toplevel from the same program is
//! that window's child, not a second window beside it, which is why `Window`
//! nests and `Client` does not flatten.
//!
//! Nothing here carries a size. A window's size is the gui's decision, made when it
//! lays the pane out, and a number on the wire would be that decision made twice.

use std::sync::Arc;

use easytgp::Format;
use wire::message::SurfaceId;

use crate::session::State;

/// One window: the cells it draws into, what the session knows about it, and the
/// toplevels its program opened afterwards.
pub struct Window<F: Format> {
    /// The cells this window draws into, and the name it is placed under.
    ///
    /// Shared because the proxy holds it weakly: the registry of surfaces is a list
    /// of these, and a window going away is what makes its entry stop resolving.
    pub surface: std::sync::Arc<SurfaceId>,
    /// Where its picture sits in the stacking order, higher in front.
    ///
    /// From the gui, which is the side with a window manager: it knows what a window
    /// is in front of. The server places at this number and never invents one.
    pub z: i32,
    /// What the session knows about this window: its image, its decoder, and what
    /// it is currently showing. Per window, because two windows are two pictures
    /// and a decoder that followed one stream cannot follow the other.
    pub state: State<F>,
    /// Toplevels this program opened after its root window.
    pub children: Vec<Window<F>>,
}

impl<F: Format> Window<F> {
    /// A window that has been admitted and has not drawn yet.
    pub fn new(surface: Arc<SurfaceId>, z: i32) -> Self {
        Self {
            surface,
            z,
            state: State::new(),
            children: Vec::new(),
        }
    }

    /// The window in this subtree whose cells are `surface`.
    ///
    /// The whole subtree, not just this window: a program's later toplevel is a
    /// child, and a frame may belong to either.
    pub fn find_mut(&mut self, surface: SurfaceId) -> Option<&mut Window<F>> {
        if *self.surface == surface {
            return Some(self);
        }
        self.children
            .iter_mut()
            .find_map(|child| child.find_mut(surface))
    }

    /// The box this window occupies on screen, in cells: `(left, top, width, height)`.
    ///
    /// The first cell carrying the surface is the top left and the last is the bottom
    /// right, which is what makes the screen itself the authority on where a window
    /// is: no position is sent, and a window a multiplexer moved is found where it
    /// moved to. `None` when no cell carries it, which means the window is scrolled
    /// away, occluded to nothing, or has not drawn yet.
    pub fn rectangle(&self, screen: &vt100::Screen) -> Option<(u16, u16, u16, u16)> {
        let (rows, cols) = screen.size();
        let (mut left, mut top) = (u16::MAX, u16::MAX);
        let (mut right, mut bottom) = (0u16, 0u16);
        let mut found = false;

        for row in 0..rows.get() {
            for col in 0..cols.get() {
                let Some(cell) = screen.cell(row, col) else {
                    continue;
                };
                if !is_surface(cell, &self.surface) {
                    continue;
                }
                found = true;
                left = left.min(col);
                top = top.min(row);
                right = right.max(col);
                bottom = bottom.max(row);
            }
        }

        found.then(|| (left, top, right - left + 1, bottom - top + 1))
    }

    /// Remove a toplevel from this window's subtree.
    fn remove_child(&mut self, surface: SurfaceId) -> bool {
        let before = self.children.len();
        self.children.retain(|child| *child.surface != surface);
        if self.children.len() != before {
            return true;
        }
        self.children
            .iter_mut()
            .any(|child| child.remove_child(surface))
    }
}

/// Whether `cell` carries `surface`'s signature.
///
/// True colour only: a palette index names whatever the terminal has in that slot,
/// which is not a fact a surface carries.
fn is_surface(cell: &vt100::Cell, surface: &SurfaceId) -> bool {
    let rgb = |colour: vt100::Color| match colour {
        vt100::Color::Rgb(r, g, b) => Some((r, g, b)),
        vt100::Color::Default | vt100::Color::Idx(_) => None,
    };
    cell.contents().chars().next() == Some(surface.symbol)
        && rgb(cell.fgcolor()) == Some(surface.fg)
        && rgb(cell.bgcolor()) == Some(surface.bg)
}

/// One client of the compositor: the program itself, or a helper it started.
///
/// A helper connects on its own, so it is a client of its own with a root window of
/// its own, which is why the clients nest. A client is never without a window: the
/// two are born together, so a client the server cannot address is not a state it
/// has to represent.
pub struct Client<F: Format> {
    /// The process id, which is what a helper is matched to its parent by.
    pub pid: u32,
    /// This client's root window.
    pub window: Window<F>,
    /// Processes this client started which connected as clients of their own.
    pub sub_clients: Vec<Client<F>>,
}

impl<F: Format> Client<F> {
    /// A client whose first window is ready to be drawn.
    pub fn new(pid: u32, surface: Arc<SurfaceId>, z: i32) -> Self {
        Self {
            pid,
            window: Window::new(surface, z),
            sub_clients: Vec::new(),
        }
    }

    /// The window whose cells are `surface`, preferring the tree of the client at `pid`.
    ///
    /// The surface is the identity: it is what the cells say, and it is unique. The pid is
    /// a preference, not a requirement, because a window is not always filed under the pid
    /// its frames carry: a program that opens more than one connection gets its windows
    /// attached to the root, while the frames still name the connection's pid. Looking up
    /// by pid alone would then never find the window again, and every frame for it would
    /// be dropped — a pane with the window's signature and no picture.
    pub fn get_window_mut(&mut self, pid: u32, surface: SurfaceId) -> Option<&mut Window<F>> {
        if self.pid == pid {
            if self.window.find_mut(surface).is_some() {
                return self.window.find_mut(surface);
            }
        }
        if let Some(found) = self
            .sub_clients
            .iter_mut()
            .find_map(|client| client.get_window_mut(pid, surface))
        {
            return Some(found);
        }
        self.window.find_mut(surface)
    }

    /// The client at `pid`, or a helper's own, however deep it sits.
    pub fn find_mut(&mut self, pid: u32) -> Option<&mut Client<F>> {
        if self.pid == pid {
            return Some(self);
        }
        self.sub_clients.iter_mut().find_map(|c| c.find_mut(pid))
    }

    /// Remove the window whose cells are `surface`, wherever it sits.
    ///
    /// Returns whether one was removed. A client's root window cannot be removed this
    /// way: it is the client, and a client with no root window is a client that is
    /// gone, which `ClientClosed` says instead.
    pub fn remove_window(&mut self, surface: SurfaceId) -> bool {
        for sub in &mut self.sub_clients {
            if sub.remove_window(surface) {
                return true;
            }
        }
        self.window.remove_child(surface)
    }

    /// Restack the window whose cells are `surface`.
    pub fn restack(&mut self, surface: SurfaceId, z: i32) -> bool {
        if let Some(window) = self.window.find_mut(surface) {
            window.z = z;
            return true;
        }
        self.sub_clients.iter_mut().any(|c| c.restack(surface, z))
    }
}
