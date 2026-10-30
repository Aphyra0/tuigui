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

use super::{StatefulWindow, WindowState, WindowingServer};

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
    /// The capybara this window shows, as ANSI cells, and the width it was
    /// halftoned for: a grid made for a narrower window fills only its left, so
    /// the width is kept with it and the grid rebuilt when the window is drawn
    /// wider. The image's name is kept beside it, which is what the bar shows.
    capy: RwLock<(u16, Option<(String, halftone::pipeline::Grid)>)>,
    /// Set by `terminate`. Nothing runs behind a dummy window, so there is no
    /// program to ask to close: being asked is the window being over, and its
    /// parent stops reporting it at the next read.
    gone: std::sync::atomic::AtomicBool,
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
            area: Rect::new(0, 0, size.0 as u16, size.1 as u16),
            surface: wire::message::SurfaceId::random(),
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
    fn key_input(&self, event: ui_events::keyboard::KeyboardEvent) -> Result<(), super::Error> {
        use ui_events::keyboard::Key;
        if !event.state.is_down() {
            return Ok(());
        }
        let Key::Character(typed) = &event.key else {
            return Ok(());
        };
        if !matches!(typed.as_str(), "c" | "C" | "w" | "W") {
            return Ok(());
        }
        let window = DummyWindow::default();
        window.set_state(WindowState {
            area: Rect::new(10, 10, 30, 10),
            surface: wire::message::SurfaceId::random(),
        });
        self.children.write().unwrap().push(Arc::new(window));
        Ok(())
    }

    /// A dummy window has no program to send a pointer event to: the keys are
    /// the whole of what it understands.
    fn pointer_input(&self, _event: ui_events::pointer::PointerEvent) -> Result<(), super::Error> {
        Ok(())
    }

    /// A dummy window never draws a picture: nothing is running behind it to
    /// capture, so there is no frame to send.
    fn capture(&self, _link: &wire::transport::Connection) -> Result<(), super::Error> {
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
        // A child that was asked to close is dropped here rather than merely
        // filtered: it is this list that keeps it alive, and a window nobody can
        // see must not stay alive, or the weak handle asking for the keyboard is
        // still good.
        let mut children = self.children.write().unwrap();
        children.retain(|child| !child.gone.load(std::sync::atomic::Ordering::Acquire));
        children
            .iter()
            .map(|child| Arc::downgrade(child) as Weak<dyn StatefulWindow>)
            .collect()
    }

    /// A dummy window is named after the image it draws, which is the only thing
    /// there is to call it.
    fn title(&self) -> String {
        let name = self.capy.read().unwrap().1.as_ref().map(|(name, _)| name.clone());
        name.unwrap_or_else(|| "capybara".to_string())
    }

    /// Nothing runs behind a dummy window, so there is no program to ask: being
    /// asked to close is the window being over, and its parent stops reporting it.
    fn terminate(&self) {
        self.gone.store(true, std::sync::atomic::Ordering::Release);
    }
}

/// One capybara of the demo set, halftoned to ANSI cells, as the window's
/// background.
impl WidgetRef for DummyWindow {
    fn render_ref(&self, area: Rect, buf: &mut Buffer) {
        buf.set_style(area, Style::new().bg(Color::Rgb(0x00, 0x00, 0x00)));

        let mut capy = self.capy.write().unwrap();
        if capy.0 != area.width {
            *capy = (area.width, halftone(area.width));
        }
        let Some((_, grid)) = &capy.1 else {
            return;
        };
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

/// One image from the demo set, halftoned to `width` cells, with the name of the
/// file it came from.
///
/// The set is walked from a random start, and one image after another is tried
/// until one reads and converts: a bad file is skipped, and only a set where none
/// works leaves the window its black background. Each failure is reported, since
/// a skipped image is otherwise invisible.
fn halftone(width: u16) -> Option<(String, halftone::pipeline::Grid)> {
    let dir = std::env::var_os("CAPYBARA_IMG_DIR")?;
    let files: Vec<_> = std::fs::read_dir(dir)
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

    (0..files.len()).find_map(|offset| {
        let file = &files[(start + offset) % files.len()];
        let Ok(bytes) = std::fs::read(file) else {
            log::warn!("could not read {}", file.display());
            return None;
        };
        match halftone::pipeline::convert_bytes(
            &bytes,
            &halftone::pipeline::ConvertSettings {
                width: width.max(1) as u32,
                cell_bg: true,
                bg: None,
                tone: halftone::pipeline::Tone::Natural,
            },
        ) {
            Ok(conversion) => {
                let name = file
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| file.display().to_string());
                Some((name, conversion.grid))
            }
            Err(e) => {
                log::warn!("could not halftone {}: {e}", file.display());
                None
            }
        }
    })
}
