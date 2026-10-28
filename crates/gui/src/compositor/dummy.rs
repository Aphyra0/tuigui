//! The dummy window session: windows nobody is running, for looking at.
//!
//! `--dbg-dummy-windowing` runs this instead of a wayland server. It implements
//! the same [`WindowingServer`] tree as the real one — one client, its window,
//! the windows that window opened after it, and the clients it started — so
//! the compositor cannot tell it apart, and every window draws itself as a
//! pane that says how to grow the tree: `C` and `W` spawn a window under it.
//!
//! Every window is its contents behind a lock: the draw reads through it and
//! a mutation takes it, and what is inside stays a plain value.

use std::sync::RwLock;

use proto::windowing;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::WidgetRef;

use super::{ClientData, WindowState};

/// The dummy window session.
#[derive(Default)]
pub struct DummyServer {
    /// The session's root client.
    root: DummyClient,
}

/// One client of the dummy session: a program that owns one window, behind a
/// lock like its window is.
///
/// The lock is the client the session hands out: a walk reads through it, a
/// mutation takes it, and what is inside stays a plain value.
#[derive(Default)]
pub struct DummyClient(RwLock<DummyClientInner>);

/// What one dummy client is, without its lock.
#[derive(Default)]
struct DummyClientInner {
    /// The client's own window.
    window: DummyWindow,
    /// The helpers this client started.
    children: Vec<DummyClient>,
}

/// One window of the dummy session: what its program drew, behind a lock.
///
/// The lock is the window the session hands out: a draw reads through it, a
/// mutation takes it, and what is inside stays a plain value.
#[derive(Default)]
pub struct DummyWindow(RwLock<DummyWindowInner>);

/// What one dummy window is, without its lock.
#[derive(Default)]
struct DummyWindowInner {
    /// The window's place on the pane.
    state: WindowState,
    /// The windows this one's program opened after it.
    children: Vec<DummyWindow>,
    /// The capybara this window shows, as ANSI cells, from the demo set.
    capy: std::sync::OnceLock<Option<halftone::pipeline::Grid>>,
}

impl windowing::WindowingServer<ClientData, WindowState> for DummyServer {
    fn root_client(&self) -> Option<&dyn windowing::Client<ClientData, WindowState>> {
        Some(&self.root)
    }

    fn is_ready(&self) -> bool {
        true
    }

    /// The session is resized: the root window is placed at the size, top
    /// left. It is the one window that fills the screen.
    fn set_size(&mut self, size: (u32, u32)) {
        let root = self.root.0.write().unwrap();
        let mut window = root.window.0.write().unwrap();
        window.state = WindowState {
            size: (size.0 as u16, size.1 as u16),
            position: (0, 0),
        };
    }
}

impl windowing::Client<ClientData, WindowState> for DummyClient {
    fn pid(&self) -> u32 {
        0
    }

    fn is_root(&self) -> bool {
        true
    }

    fn with_window(&self, f: &mut dyn FnMut(&dyn windowing::Window<WindowState>)) {
        f(&self.0.read().unwrap().window);
    }

    fn with_children(&self, f: &mut dyn FnMut(&dyn windowing::Client<ClientData, WindowState>)) {
        for child in &self.0.read().unwrap().children {
            f(child);
        }
    }
}

/// The locked window is what the session hands out: a draw reads through it,
/// a drag takes it.
impl windowing::Window<WindowState> for DummyWindow {
    /// A key is what grows the tree: `C` or `W` spawns a window this one
    /// opened after it, stepped over by how many already sit here.
    fn input(&self, input: windowing::Input) -> Result<(), windowing::Error> {
        let windowing::Input::Key {
            key: windowing::Key::Char(c),
            state: windowing::KeyState::Pressed,
        } = input
        else {
            return Ok(());
        };
        match c {
            'c' | 'C' | 'w' | 'W' => {
                let mut inner = self.0.write().unwrap();
                let cascade = inner.children.len() as u16 * 2;
                let (half_w, half_h) = (
                    (inner.state.size.0 / 2).max(1),
                    (inner.state.size.1 / 2).max(1),
                );
                let mut window = DummyWindow::default();
                window.0.get_mut().unwrap().state = WindowState {
                    size: (half_w, half_h),
                    position: (2 + cascade, 2 + cascade),
                };
                inner.children.push(window);
            }
            _ => {}
        }
        Ok(())
    }

    /// A dummy window never draws a picture.
    fn capture(&self) -> Result<Vec<u8>, windowing::Error> {
        Err(windowing::Error::NotReady)
    }

    fn with_attached(&self, f: &mut dyn FnMut(&WindowState)) {
        f(&self.0.read().unwrap().state);
    }

    fn with_children(&self, f: &mut dyn FnMut(&dyn windowing::Window<WindowState>)) {
        for child in &self.0.read().unwrap().children {
            f(child);
        }
    }

    fn set_size(&self, _size: (u32, u32)) {}
}

/// A dummy window, the way the pane draws it: gray, with the three buttons.
///
/// The buttons are the content area's last row split into thirds, under
/// [`BUTTONS`]. The area is the one the compositor worked out; the window's
/// own place is already in it.
impl WidgetRef for DummyWindow {
    /// The window: one capybara of the demo set, halftoned to ANSI cells, as
    /// the background — and the two keys that grow the tree.
    fn render_ref(&self, area: Rect, buf: &mut Buffer) {
        let inner = self.0.read().unwrap();
        let capy = inner.capy.get_or_init(|| {
            let dir = std::env::var_os("CAPYBARA_IMG_DIR")?;
            let files: Vec<_> = std::fs::read_dir(&dir)
                .ok()?
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.is_file())
                .collect();
            if files.is_empty() {
                return None;
            }
            use std::hash::{BuildHasher, Hash, Hasher};
            let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
            files.hash(&mut hasher);
            let start = (hasher.finish() % files.len() as u64) as usize;
            // One image after another, from a random start, until one reads
            // and converts: a bad file in the set is skipped, and only a set
            // where none works leaves the window its black background. Each
            // failure is reported, since a skipped image is otherwise invisible.
            for offset in 0..files.len() {
                let file = &files[(start + offset) % files.len()];
                let Ok(bytes) = std::fs::read(file) else {
                    log::warn!("could not read {}", file.display());
                    continue;
                };
                match halftone::pipeline::convert_bytes(
                    &bytes,
                    &halftone::pipeline::ConvertSettings {
                        width: area.width.max(1) as u32,
                        cell_bg: true,
                        bg: None,
                        tone: halftone::pipeline::Tone::Natural,
                    },
                ) {
                    Ok(conversion) => return Some(conversion.grid),
                    Err(e) => log::warn!("could not halftone {}: {e}", file.display()),
                }
            }
            None
        });
        buf.set_style(area, Style::new().bg(Color::Rgb(0x00, 0x00, 0x00)));
        if let Some(grid) = capy {
            for (index, cell) in grid.cells.iter().enumerate() {
                let row = (index as u32 / grid.width) as u16;
                if row >= area.height {
                    break;
                }
                let place = (area.x + (index as u32 % grid.width) as u16, area.y + row);
                if let Some(slot) = buf.cell_mut(place) {
                    slot.set_char(cell.glyph);
                    let [r, g, b] = cell.color;
                    slot.set_fg(Color::Rgb(r, g, b));
                    let [r, g, b] = cell.shade;
                    slot.set_bg(Color::Rgb(r, g, b));
                }
            }
        }
    }
}
