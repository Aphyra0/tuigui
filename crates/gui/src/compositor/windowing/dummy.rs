//! The dummy window session: windows nobody is running, for looking at.
//!
//! `--dbg-dummy-windowing` runs this instead of a wayland server. It implements
//! the same vocabulary as the real one, so the compositor cannot tell it
//! apart, and every window draws itself as a pane showing a capybara. `C` and
//! `W` spawn a window under the one the key reached.

use std::sync::{Arc, RwLock, Weak};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::WidgetRef;

use super::{Input, Key, KeyState, StatefulWindow, WindowState, WindowingServer};

/// The dummy window session.
#[derive(Default)]
pub struct DummyServer {
    /// The session's root window, minted at the first read.
    root: RwLock<Option<Arc<DummyWindow>>>,
}

/// One window of the dummy session: its state, the windows it opened after
/// it, and the picture it draws.
#[derive(Default)]
pub struct DummyWindow {
    /// The window's state on the pane.
    state: RwLock<WindowState>,
    /// The windows this one's program opened after it.
    children: RwLock<Vec<Arc<DummyWindow>>>,
    /// The capybara this window shows, as ANSI cells, from the demo set.
    capy: std::sync::OnceLock<Option<halftone::pipeline::Grid>>,
}

impl WindowingServer for DummyServer {
    /// The root window: minted at the first read, so there is something to
    /// resize and draw from the very first frame.
    fn root_window(&self) -> Option<Weak<dyn StatefulWindow>> {
        let mut root = self.root.write().unwrap();
        if root.is_none() {
            *root = Some(Arc::new(DummyWindow::default()));
        }
        let root = root.as_ref()?;
        Some(Arc::downgrade(root) as Weak<dyn StatefulWindow>)
    }

    fn is_ready(&self) -> bool {
        true
    }

    /// The session is resized: the root window is placed at the size, top
    /// left. It is the one window that fills the screen.
    fn set_size(&mut self, size: (u32, u32)) {
        let state = WindowState {
            size: (size.0 as u16, size.1 as u16),
            position: (0, 0),
        };
        let mut root = self.root.write().unwrap();
        match root.as_ref() {
            Some(root) => root.set_state(state),
            None => {
                let minted = DummyWindow::default();
                minted.set_state(state);
                *root = Some(Arc::new(minted));
            }
        }
    }
}

impl StatefulWindow for DummyWindow {
    /// A key is what grows the tree: `C` or `W` spawns a window this one
    /// opened after it, stepped over by how many already sit here.
    fn input(&self, input: Input) -> Result<(), super::Error> {
        let Input::Key {
            key: Key::Char(c),
            state: KeyState::Pressed,
        } = input
        else {
            return Ok(());
        };
        match c {
            'c' | 'C' | 'w' | 'W' => {
                let size = self.state.read().unwrap().size;
                let (half_w, half_h) = ((size.0 / 2).max(1), (size.1 / 2).max(1));
                let mut children = self.children.write().unwrap();
                let cascade = children.len() as u16 * 2;
                let window = DummyWindow::default();
                window.set_state(WindowState {
                    size: (half_w, half_h),
                    position: (2 + cascade, 2 + cascade),
                });
                children.push(Arc::new(window));
            }
            _ => {}
        }
        Ok(())
    }

    /// A dummy window never draws a picture.
    fn capture(&self) -> Result<Vec<u8>, super::Error> {
        Err(super::Error::NotReady)
    }

    /// A dummy window has no client to configure.
    fn set_size(&self, _size: (u32, u32)) {}

    fn state(&self) -> WindowState {
        *self.state.read().unwrap()
    }

    fn set_state(&self, state: WindowState) {
        *self.state.write().unwrap() = state;
    }

    fn get_children(&self) -> Vec<Weak<dyn StatefulWindow>> {
        self.children
            .read()
            .unwrap()
            .iter()
            .map(|child| Arc::downgrade(child) as Weak<dyn StatefulWindow>)
            .collect()
    }
}

/// One capybara of the demo set, halftoned to ANSI cells, as the window's
/// background.
impl WidgetRef for DummyWindow {
    fn render_ref(&self, area: Rect, buf: &mut Buffer) {
        let capy = self.capy.get_or_init(|| {
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
