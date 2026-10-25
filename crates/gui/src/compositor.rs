//! The programs running in a compositor of ours, drawn into the pane as windows.
//!
//! The video path decodes a file; this one runs programs. Each program gets a Wayland
//! session of its own — a compositor in this process, one output — and the pixels it
//! commits are read straight out of that compositor's buffer. There is no screencopy
//! protocol and no second process between: the window being presented is already in
//! this address space, so a frame is a copy and an encode.
//!
//! ```text
//!   Compositor::new ──▶ spawn(program) ──▶ windows on screen ──▶ frames
//! ```
//!
//! # What is whose
//!
//! The compositor owns the **registry**: which windows exist and what they drew. This
//! side owns the **layout**: where a window is in the pane, what is on top, which one
//! the pointer is over. Neither derives the other's facts, which is why a window can be
//! dragged without the compositor knowing, and a surface can repaint without the layout
//! moving.
//!
//! The link between them is the [`SurfaceId`], made randomly when a window is admitted
//! and parked in its attached data. A window fills its cells with it, so the cells say
//! where the picture goes: a window this side has not drawn is a window the other side
//! does not place.
//!
//! # Everything goes through the traits
//!
//! A window is a `&dyn proto::windowing::Window<WindowData>` and a client is a
//! `&dyn proto::windowing::Client<(), WindowData>`. Nothing here names the wayland
//! backend, so the same compositor works on any backend that implements `proto`.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};

use codecs::CodecId;
use proto::windowing::Window as ProtoWindow;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, StatefulWidget, Widget as _};
use wire::message::{Message, SurfaceId};
use wire::transport::Connection;

use wayland::{Options, STEP_TIMEOUT, WaylandServer};

/// How long a program is given to open its first window.
///
/// A program that connects and never opens one leaves the pane empty with nothing to
/// explain it, which is indistinguishable from a program that is merely slow. After
/// this, the emptiness is an error the pane can say out loud.
///
/// Generous: a toolkit starts, loads its configuration and its icons, and only then
/// maps a window, and a slow machine is not a broken program.
const WINDOW_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// What this side parks on a client of the compositor.
///
/// Nothing: a client is a process, and the only fact this side needs about it — its pid
/// — the trait already carries.
pub type ClientData = ();

/// What this side parks on a window.
///
/// Only the surface. The layout is not here because the trait hands out `&W`, and a
/// window that is being dragged has to be written every frame: the layout therefore
/// lives in [`Compositor`], keyed by this surface, which is also where the decision
/// belongs.
pub struct WindowData {
    /// The cells this window draws into, made when it was admitted.
    pub surface: SurfaceId,
}

impl Default for WindowData {
    /// A fresh surface id, which is what makes every admitted window addressable
    /// without anyone having to hand one in.
    fn default() -> Self {
        Self {
            surface: random_surface(),
        }
    }
}

/// A window's place on screen, as this side decides it.
#[derive(Debug, Clone)]
struct Layout {
    /// The whole window, border included, in cells.
    area: Rect,
    /// What the title bar shows.
    title: String,
    /// The image id its frames are sent under, fixed when it is first seen so a later
    /// frame replaces the picture rather than making a new one.
    image: u32,
    /// Where it sits in the stacking order, higher in front.
    z: i32,
}

/// A drag in progress: where the pointer grabbed the window, and where the window was.
#[derive(Debug, Clone, Copy)]
struct Drag {
    surface: SurfaceId,
    /// The pointer's cell when the drag started.
    at: (u16, u16),
    /// The window's top left when the drag started.
    from: (u16, u16),
}

/// A program, or several, running in a compositor of ours.
pub struct Compositor {
    /// The session the programs run in. Dropping it ends them.
    server: WaylandServer<ClientData, WindowData>,
    /// The encoder, kept across frames: building one per frame is the expensive part,
    /// not the encoding.
    encoder: Box<dyn codecs::Encoder>,
    /// The codec, announced on the wire before any frame.
    codec: CodecId,
    /// The output's size in pixels, which is what a frame is encoded at.
    size: (u32, u32),
    /// Where every live window is, by the surface it draws into.
    windows: HashMap<SurfaceId, Layout>,
    /// The last frame sent for each window, so an idle one is not re-encoded and
    /// re-transmitted.
    last: HashMap<SurfaceId, Vec<u8>>,
    /// The windows the other side has been told about.
    announced: HashSet<SurfaceId>,
    /// The next image id and stacking rank to hand out.
    next_image: u32,
    next_z: i32,
    /// The drag in progress, if any.
    drag: Option<Drag>,
    /// The pane, in cells, as of the last render.
    pane: Rect,
    /// When the program was started, for the "no window at all" timeout.
    ///
    /// A program that connects and never opens a window looks exactly like one that is
    /// merely slow: the pane stays empty and nothing says why. After
    /// [`WINDOW_TIMEOUT`] the emptiness is reported instead.
    started: Option<std::time::Instant>,
}

impl Compositor {
    /// Start a compositor whose output is `size` pixels, ready to run programs.
    ///
    /// `pane` is the terminal's grid in cells, which is where the windows are laid
    /// out. It is taken here rather than left for the first render: a window that is
    /// admitted before anything has been drawn needs a rectangle, and a zero-sized one
    /// would be a window that exists and cannot be seen.
    pub fn new(size: (u32, u32), pane: Rect, codec: CodecId) -> std::io::Result<Self> {
        let server = WaylandServer::new(Options {
            resolution: size,
            dpi: 1.0,
            show_cursor: true,
            use_x11: false,
        })
        .map_err(std::io::Error::other)?;

        // Even dimensions: 4:2:0 chroma needs them, and a capture is padded to the
        // output's cells rather than the other way round.
        let (width, height) = (size.0 & !1, size.1 & !1);
        let encoder: Box<dyn codecs::Encoder> = match codec {
            CodecId::H264 => Box::new(
                codecs::h264::Encoder::new(width, height)
                    .map_err(|e| std::io::Error::other(format!("building the H.264 encoder: {e}")))?,
            ),
            CodecId::Jpeg => Box::new(
                codecs::jpeg::Encoder::new()
                    .map_err(|e| std::io::Error::other(format!("building the JPEG encoder: {e}")))?,
            ),
        };

        Ok(Self {
            server,
            encoder,
            codec,
            size,
            windows: HashMap::new(),
            last: HashMap::new(),
            announced: HashSet::new(),
            next_image: 1,
            next_z: 1,
            drag: None,
            pane,
            started: None,
        })
    }

    /// Run `path` in the compositor, and return at once.
    ///
    /// The program has not connected yet, so it has no window: the caller learns about
    /// both by rendering.
    pub fn spawn(&mut self, path: &Path) -> std::io::Result<()> {
        self.server
            .spawn(path)
            .map(|_| ())
            .map_err(std::io::Error::other)?;
        // The clock starts when the program does: what the timeout is about is a
        // program that never opens a window, not one that was never asked to.
        self.started = Some(std::time::Instant::now());
        Ok(())
    }

    /// Whether every program this compositor started has ended.
    ///
    /// Read through the trait: a client says whether its process is still running,
    /// which is the one fact that ends a session. A compositor with no program at all
    /// has not ended — it is waiting for one.
    pub fn finished(&self) -> bool {
        let Some(root) = self.server.root_client().and_then(|root| root.upgrade()) else {
            return false;
        };
        let mut running = false;
        let client: &dyn proto::windowing::Client<ClientData, WindowData> = &*root;
        any_running(client, &mut running);
        !running
    }
    /// Step the compositor, then send whatever changed.
    ///
    /// One turn: the server dispatches what the programs did, each window is captured
    /// and encoded, and whatever came out is sent. A window that has not changed
    /// produces nothing, which is what keeps an idle program from spending the whole
    /// budget on the same bytes.
    pub async fn frame(&mut self, link: &Connection) -> std::io::Result<()> {
        self.server
            .step(STEP_TIMEOUT)
            .map_err(std::io::Error::other)?;

        // Every window the compositor has, whether or not it has drawn yet, with the
        // pid that draws it and whether it belongs to the session's own program.
        //
        // The window is listed even when `capture` says `NotReady`: a window that has
        // not committed a buffer is still a window, and it is the thing the layout and
        // the announcement are about. Capturing first would have made a window that has
        // not painted yet invisible to everything downstream, including the draw that
        // is what eventually makes it paint.
        let mut seen: Vec<(u32, bool, SurfaceId, Option<Vec<u8>>)> = Vec::new();
        self.walk(&mut |pid, is_root, surface, window| {
            seen.push((pid, is_root, surface, window.capture().ok()));
        });

        // Windows that have appeared are announced before their first frame, so the
        // other side can address what arrives.
        //
        // The first window announced is the root client, whatever `is_root` says. A
        // session runs one program, and a program may open several connections: the one
        // that commits a buffer first is the one this side hears from first, and it is
        // still the session's own program. `is_root` only decides between "the program"
        // and "a helper" once a root exists.
        for (pid, is_root, surface, _) in &seen {
            self.admit(*surface);
            if !self.announced.insert(*surface) {
                continue;
            }
            let z = self.windows.get(surface).map_or(0, |layout| layout.z);
            let first = self.announced.len() == 1;
            let message = if *is_root || first {
                Message::InitClient {
                    pid: *pid,
                    surface: *surface,
                    z,
                }
            } else {
                Message::NewWindow {
                    pid: *pid,
                    surface: *surface,
                    z,
                }
            };
            link.send(&message).await?;
        }

        // The pictures are gathered above, so this is where "the program connected but
        // never opened a window" becomes a stated failure rather than an empty pane: a
        // session whose program has no window after the timeout has nothing to show and
        // nothing that will change on its own.
        if self.windows.is_empty() {
            if let Some(started) = self.started {
                let waited = started.elapsed();
                if waited > WINDOW_TIMEOUT {
                    return Err(std::io::Error::other(format!(
                        "the program has opened no window in {waited:?}"
                    )));
                }
            }
        } else {
            self.started = None;
        }

        // Then the pictures: only what has been drawn and changed.
        for (pid, _, surface, pixels) in seen {
            let Some(pixels) = pixels else {
                // Not drawn yet: announced, laid out and on screen as its cells, but
                // with no picture to send. The next frame that commits will carry one.
                continue;
            };
            if self.last.get(&surface) == Some(&pixels) {
                continue;
            }
            let encoded = self
                .encoder
                .encode(&pixels, self.size.0, self.size.1, false)
                .map_err(|e| std::io::Error::other(format!("encoding a frame: {e}")))?;
            self.last.insert(surface, pixels);
            if encoded.is_empty() {
                continue;
            }
            link.send(&Message::Frame {
                pid,
                surface,
                bytes: encoded,
            })
            .await?;
        }

        Ok(())
    }

    /// Take a window into the layout, the first time it is seen.
    ///
    /// The first window is given the whole pane; every later one takes a fixed box,
    /// because a floating layout is one where a window says how big it is. Both are
    /// remeasured on resize, which `render` is what notices.
    fn admit(&mut self, surface: SurfaceId) {
        if self.windows.contains_key(&surface) {
            return;
        }
        let area = if self.windows.is_empty() {
            self.pane
        } else {
            floating(self.pane, self.windows.len() as u16)
        };
        self.windows.insert(
            surface,
            Layout {
                area,
                title: format!("{}", surface.symbol),
                image: self.next_image,
                z: self.next_z,
            },
        );
        self.next_image += 1;
        self.next_z += 1;
    }

    /// Walk every window in the session, with the pid that draws it and whether it
    /// belongs to the session's own program.
    ///
    /// The trait's own structure, walked here rather than in the backend: a client
    /// offers its window and its helpers, a window offers its children. `is_root` comes
    /// from the client, so the first window the session was built around is announced
    /// as the root client and every other as a helper's.
    fn walk(&self, f: &mut dyn FnMut(u32, bool, SurfaceId, &dyn ProtoWindow<WindowData>)) {
        let Some(root) = self.server.root_client().and_then(|root| root.upgrade()) else {
            return;
        };
        let client: &dyn proto::windowing::Client<ClientData, WindowData> = &*root;
        walk_client(client, f);
    }

    /// Move a window by the pointer, in cells.
    ///
    /// Absolute from where the drag started rather than incremental: an event that is
    /// dropped then costs nothing but a frame of lag, where an incremental move would
    /// lose the motion for good.
    pub fn drag_to(&mut self, at: (u16, u16)) {
        let Some(drag) = self.drag else { return };
        let Some(layout) = self.windows.get_mut(&drag.surface) else {
            return;
        };
        let dx = at.0 as i32 - drag.at.0 as i32;
        let dy = at.1 as i32 - drag.at.1 as i32;
        let x = (drag.from.0 as i32 + dx).clamp(0, self.pane.right().saturating_sub(1) as i32);
        let y = (drag.from.1 as i32 + dy).clamp(0, self.pane.bottom().saturating_sub(1) as i32);
        layout.area.x = x as u16;
        layout.area.y = y as u16;
    }
}

/// Whether any client in this tree is still running, set into `running`.
fn any_running(
    client: &dyn proto::windowing::Client<ClientData, WindowData>,
    running: &mut bool,
) {
    if client.is_running() {
        *running = true;
    }
    client.with_children(&mut |child| any_running(child, running));
}

/// One client and everything under it, offered as windows.
fn walk_client(
    client: &dyn proto::windowing::Client<ClientData, WindowData>,
    f: &mut dyn FnMut(u32, bool, SurfaceId, &dyn ProtoWindow<WindowData>),
) {
    let pid = client.pid();
    let is_root = client.is_root();
    client.with_window(&mut |window| each_window(window, pid, is_root, f));
    client.with_children(&mut |child| walk_client(child, f));
}

/// A window and its children, offered with the pid that draws them.
fn each_window(
    window: &dyn ProtoWindow<WindowData>,
    pid: u32,
    is_root: bool,
    f: &mut dyn FnMut(u32, bool, SurfaceId, &dyn ProtoWindow<WindowData>),
) {
    window.with_attached(&mut |data| f(pid, is_root, data.surface, window));
    window.with_children(&mut |child| each_window(child, pid, is_root, f));
}

/// A window's box when it is not the first: `n`th in a cascade, a fixed size.
fn floating(pane: Rect, n: u16) -> Rect {
    const WIDTH: u16 = 40;
    const HEIGHT: u16 = 12;
    let offset = n.saturating_mul(2);
    Rect {
        x: (pane.x + offset).min(pane.right().saturating_sub(1)),
        y: (pane.y + offset).min(pane.bottom().saturating_sub(1)),
        width: WIDTH.min(pane.width),
        height: HEIGHT.min(pane.height),
    }
}

/// A short label for a surface, so a title bar says which window it is.
fn kind(surface: &SurfaceId) -> char {
    surface.symbol
}

/// A surface id, made fresh.
///
/// Random in all three fields so it cannot be mistaken for ordinary content and two
/// windows cannot collide by accident. The entropy comes from `RandomState`, which the
/// process seeds from the OS, mixed with a counter so two calls in the same nanosecond
/// still differ.
fn random_surface() -> SurfaceId {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};

    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u32(n);
    let a = hasher.finish();
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(a);
    let b = hasher.finish();

    // Glyphs that no program draws by accident: block elements and geometric shapes,
    // nowhere near the ASCII a terminal UI is made of.
    const SYMBOLS: &[char] = &[
        '░', '▒', '▓', '█', '▚', '▞', '◧', '◨', '◩', '◪', '⬒', '⬓', '⬔', '⬕', '▛', '▜',
    ];
    SurfaceId {
        symbol: SYMBOLS[(a % SYMBOLS.len() as u64) as usize],
        // 24-bit in each channel, kept away from 0 and 255 so a surface never looks
        // like a plain default colour.
        fg: rgb(b >> 8),
        bg: rgb(b),
    }
}

/// Three bytes of `seed` as a colour, avoiding the extremes.
fn rgb(seed: u64) -> (u8, u8, u8) {
    let channel = |shift: u32| {
        let byte = ((seed >> shift) & 0xff) as u8;
        // 1..=254: a surface is never a pure black or white cell.
        byte.clamp(1, 254)
    };
    (channel(0), channel(8), channel(16))
}

impl StatefulWidget for &mut Compositor {
    type State = Connection;

    /// One frame: advance the session, then draw where it got to.
    fn render(self, area: Rect, buf: &mut Buffer, _link: &mut Connection) {
        self.pane = area;

        // The windows are drawn in stacking order, and the cells of each are filled
        // with its surface: that fill is what the other side finds, so a window that
        // is not drawn is a window whose picture is not placed.
        let mut order: Vec<(SurfaceId, Layout)> = self
            .windows
            .iter()
            .map(|(surface, layout)| (*surface, layout.clone()))
            .collect();
        order.sort_by_key(|(_, layout)| layout.z);

        for (surface, layout) in order {
            let area = layout.area.intersection(area);
            if area.width < 3 || area.height < 3 {
                continue;
            }
            let block = Block::bordered().title(format!(" {} ✕ ", layout.title));
            let inner = block.inner(area);
            block.render(area, buf);

            // The content area carries the surface's own signature, so the other side
            // can find the rectangle by looking at the screen.
            let style = Style::new()
                .fg(Color::Rgb(surface.fg.0, surface.fg.1, surface.fg.2))
                .bg(Color::Rgb(surface.bg.0, surface.bg.1, surface.bg.2));
            buf.set_style(inner, style);
            for row in inner.y..inner.bottom() {
                for col in inner.x..inner.right() {
                    if let Some(cell) = buf.cell_mut((col, row)) {
                        cell.set_char(surface.symbol);
                    }
                }
            }
        }
    }
}
