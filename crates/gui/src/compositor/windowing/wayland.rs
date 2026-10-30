//! The wayland session, wearing the windowing vocabulary.
//!
//! The session runs in `wayland`: surfaces, commits, the seat. This is the
//! layer over it: the session's windows as this crate's [`StatefulWindow`]
//! values, each with its state on the pane and the windows it opened after it.
//!
//! The shape is the demo session's, deliberately: a server holding its root, a
//! window holding its own state and its children. A window is minted once and
//! kept — the compositor hands the same window back every frame, and with it the
//! place the compositor last put it.

use std::sync::{Arc, Mutex, RwLock, Weak};

use wayland::WaylandServer as Native;

use super::{StatefulWindow, WindowState, WindowingServer, send};

/// The wayland session as a windowing server.
pub struct WaylandServer {
    /// The session.
    inner: Native,
    /// The codec every window of this session streams with: the session's own
    /// choice, made once on the command line and carried to each window as it is
    /// minted.
    codec: codecs::CodecId,
    /// The session's root window, minted at the first read: the one window whose
    /// size is the session's own.
    root: RwLock<Option<Arc<WaylandWindow>>>,
}

/// One window of the session: the session's window, its state on the pane, the
/// windows its program opened after it, and the stream its own frames go out on.
pub struct WaylandWindow {
    /// The window in the session. An `Arc`: the window outlives any read of it, and
    /// the compositor holds weak handles.
    inner: Arc<wayland::Window>,
    /// The window's state on the pane, in cells.
    state: RwLock<WindowState>,
    /// The windows this one's program opened after it.
    children: RwLock<Vec<Arc<WaylandWindow>>>,
    /// The encoder this window's frames go through: its own, because an encoder's
    /// state is the pictures it has already sent. Built on the first capture, since
    /// an encoder is built for a size and the size is only known once it has drawn.
    encoder: Mutex<Option<Box<dyn codecs::Encoder>>>,
    /// The codec that encoder is for: the session's, handed over when the window
    /// was minted.
    codec: codecs::CodecId,
    /// Whether this window is the session's root: the one that opens the tree on
    /// the far side, rather than a window under it.
    root: bool,
    /// Whether this window has been announced on the link. Once only: the far
    /// side addresses a window by its surface, so the announcement has to arrive
    /// before the first frame that names it, and must not be repeated.
    announced: std::sync::atomic::AtomicBool,
}

impl WaylandWindow {
    /// One window of the session, with nothing known about where it sits yet.
    fn new(inner: Arc<wayland::Window>, codec: codecs::CodecId, root: bool) -> Self {
        Self {
            state: RwLock::new(WindowState::default()),
            inner,
            children: RwLock::new(Vec::new()),
            encoder: Mutex::new(None),
            codec,
            root,
            announced: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

impl WaylandServer {
    /// Start the session, streaming with `codec`.
    pub fn new(options: wayland::Options, codec: codecs::CodecId) -> std::io::Result<Self> {
        let inner = Native::new(options).map_err(|e| std::io::Error::other(e.to_string()))?;
        Ok(Self {
            inner,
            codec,
            root: RwLock::new(None),
        })
    }
}

impl WindowingServer for WaylandServer {
    /// The root window: the session's, minted at the first read and kept.
    fn root_window(&self) -> Option<Weak<dyn StatefulWindow>> {
        let mut root = self.root.write().unwrap();
        if root.is_none() {
            let tree = self.inner.root_tree()?;
            *root = Some(Arc::new(WaylandWindow::new(tree, self.codec, true)));
        }
        let root = root.as_ref()?;
        Some(Arc::downgrade(root) as Weak<dyn StatefulWindow>)
    }

    fn spawn(&mut self, path: &std::path::Path) -> std::io::Result<()> {
        self.inner.spawn(path)
    }

    fn advance(&mut self, timeout: std::time::Duration) {
        self.inner.step(timeout);
    }

    fn is_ready(&self) -> bool {
        self.inner.root_tree().is_some()
    }

    /// The session is resized: the output changes mode, and the root window is
    /// measured against the new size on its next read.
    ///
    /// The caller states the size the pane is every frame, so this is where the
    /// comparing is done: a session already that size changes nothing, which is
    /// what keeps an untouched terminal from a configure per frame.
    fn set_size(&mut self, size: (u32, u32)) {
        if self.inner.size() != size {
            self.inner.set_size(size);
        }
    }
}

impl StatefulWindow for WaylandWindow {
    fn key_input(&self, event: ui_events::keyboard::KeyboardEvent) -> Result<(), super::Error> {
        self.inner.queue_key(event);
        Ok(())
    }

    fn pointer_input(&self, event: ui_events::pointer::PointerEvent) -> Result<(), super::Error> {
        self.inner.queue_pointer(event);
        Ok(())
    }

    /// What the window last drew, encoded and sent as this window's frame.
    ///
    /// The first call announces the window and then sends its picture; every
    /// later one sends the picture alone. Announcement precedes the frame because
    /// the far side places what it has been told about: a frame for a window it
    /// has never heard of is a frame it drops.
    fn capture(&self, link: &wire::transport::Connection) -> Result<(), super::Error> {
        let surface = *self.inner.id();
        if !self
            .announced
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            // The root opens the session's tree on the far side; everything else
            // hangs under it. Every window carries the same pid: one program, and
            // these are the windows it opened.
            let said = if self.root {
                wire::message::Message::InitClient {
                    pid: 1,
                    surface,
                    z: 0,
                }
            } else {
                wire::message::Message::NewWindow {
                    pid: 1,
                    surface,
                    z: 0,
                }
            };
            send(link, &said)?;
        }

        let (pixels, size) = self.inner.capture().ok_or(super::Error::NotReady)?;

        // Built once, on the first frame: an encoder is made for a size, and the
        // size is only known once the window has drawn. The lock is released
        // before the send, so a link that stalls holds nothing but the frame.
        let bytes = {
            let mut held = self.encoder.lock().unwrap();
            let encoder = match held.as_mut() {
                Some(encoder) => encoder,
                None => held.insert(
                    self.codec
                        .to_encoder(size)
                        .map_err(|e| super::Error::Build(e.to_string()))?,
                ),
            };
            encoder
                .encode(&pixels, size.0, size.1, false)
                .map_err(|e| super::Error::Encode(e.to_string()))?
        };
        // Nothing to send is not a failure: a codec holding a frame back has
        // nothing for the wire yet.
        if bytes.is_empty() {
            return Ok(());
        }

        let frame = wire::message::Message::Frame {
            pid: 1,
            surface,
            bytes,
        };
        send(link, &frame)
    }

    /// Ask the session's window to draw at a size. The next commit answers.
    ///
    /// Only when it differs from what the window was last asked for: this is
    /// called every frame, and a configure per frame is a client re-laying itself
    /// out per frame.
    fn set_size(&self, size: (u32, u32)) {
        if self.inner.size() != size {
            self.inner.set_size(size);
        }
    }

    fn state(&self) -> WindowState {
        *self.state.read().unwrap()
    }

    fn set_state(&self, state: WindowState) {
        *self.state.write().unwrap() = state;
    }

    /// The windows this one's program opened after it, and the root windows of the
    /// helper clients it started. Each is minted once and kept, so the compositor
    /// sees the same window as last frame.
    fn get_children(&self) -> Vec<Weak<dyn StatefulWindow>> {
        let descendants = self.inner.descendants();
        let mut children = self.children.write().unwrap();
        // A window already seen keeps its entry, and with it the place the
        // compositor put it; the ones that went away are dropped.
        children.retain(|child| {
            descendants
                .iter()
                .any(|tree| Arc::ptr_eq(&child.inner, tree))
        });
        for tree in descendants {
            if children
                .iter()
                .any(|child| Arc::ptr_eq(&child.inner, &tree))
            {
                continue;
            }
            children.push(Arc::new(WaylandWindow::new(tree, self.codec, false)));
        }
        children
            .iter()
            .map(|child| Arc::downgrade(child) as Weak<dyn StatefulWindow>)
            .collect()
    }

    /// What the program calls its window: the title it set, or its app id when it
    /// set no title, or nothing when it set neither.
    fn title(&self) -> String {
        self.inner.title().unwrap_or_default()
    }

    /// Ask the program to close the window. It is a request: the client is told
    /// and decides, which is why the window may still be there afterwards.
    fn terminate(&self) {
        self.inner.terminate();
    }
}

/// A window, the way the pane draws it: every cell filled with the window's
/// own surface id, which is the anchor the other side scans for.
impl ratatui::widgets::WidgetRef for WaylandWindow {
    fn render_ref(&self, area: ratatui::layout::Rect, buf: &mut ratatui::buffer::Buffer) {
        let id = *self.inner.id();
        let style = ratatui::style::Style::new()
            .fg(ratatui::style::Color::Rgb(id.fg.0, id.fg.1, id.fg.2))
            .bg(ratatui::style::Color::Rgb(id.bg.0, id.bg.1, id.bg.2));
        buf.set_style(area, style);
        for row in area.y..area.bottom() {
            for column in area.x..area.right() {
                if let Some(cell) = buf.cell_mut((column, row)) {
                    cell.set_char(id.symbol);
                }
            }
        }
    }
}
