//! Renders video frames into the terminal over TGP, using a single
//! persistent image with dirty-tile updates.
//!
//! One `DynImage` is transmitted once (a placeholder canvas). Every video
//! frame is divided into an X×Y grid of tiles; each tile is compared against
//! the cached copy and only changed tiles are re-uploaded as `a=f` frame
//! edits (`Upload` with x/y offsets). Unchanged tiles cost nothing on the
//! wire. The displayed frame is updated terminal-side in place, so there is
//! no flicker, no re-placement, no id churn.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;
use std::{io::stdin, sync::Arc};

use easytgp::{
    Controller, DynImage, FrameEdit, FrameHandle, FrameSelector, ImageRequest, NewFrameRequest,
    Upload,
};
use fast_image_resize::Resizer;
use image::GenericImageView;
use ndarray::{Array2, Array3, ArrayBase};

/// The frame type handed out by [`Streamer`], re-exported so implementors
/// don't need to depend on `image` themselves.
pub use image::RgbImage;
use pollster::block_on;
use ratatui::layout::Position;
use ratatui::widgets::StatefulWidget;
use rayon::prelude::*;

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    widgets::Widget,
};

pub trait Streamer: Send {
    /// Full-resolution next frame.
    fn next_frame(&mut self) -> Option<RgbImage>;
}

/// Per-frame timings, in milliseconds, split by stage.
#[derive(Default, Clone, Copy)]
pub struct Stats {
    pub total: std::time::Duration,
    /// Wall time since the previous render started: the true frame interval,
    /// including ratatui's diff write, the backend flush, and the event poll.
    pub interval: std::time::Duration,
    pub decode: std::time::Duration,
    /// Wait for the downscaled (▀ preview) frame from the second decoder.
    pub scale: std::time::Duration,
    /// Per-tile change detection against the previous frame.
    pub diff: std::time::Duration,
    pub encode: std::time::Duration,
    pub sync: std::time::Duration,
    pub uploads: u32,
}

impl Stats {
    fn add(self, other: &Stats) -> Stats {
        Stats {
            total: self.total + other.total,
            interval: self.interval + other.interval,
            decode: self.decode + other.decode,
            scale: self.scale + other.scale,
            diff: self.diff + other.diff,
            encode: self.encode + other.encode,
            sync: self.sync + other.sync,
            uploads: self.uploads + other.uploads,
        }
    }
}

/// True if the `w`x`h` region at `(x,y)` differs between `a` and `b`.
/// Compares row slices (memcmp) and bails on the first differing row.
fn tile_differs(a: &RgbImage, b: &RgbImage, x: u32, y: u32, w: u32, h: u32) -> bool {
    let (sa, sb) = (a.width() as usize * 3, b.width() as usize * 3);
    let row = w as usize * 3;
    let (ar, br) = (a.as_raw(), b.as_raw());
    for yy in 0..h as usize {
        let ao = (y as usize + yy) * sa + x as usize * 3;
        let bo = (y as usize + yy) * sb + x as usize * 3;
        if ar[ao..ao + row] != br[bo..bo + row] {
            return true;
        }
    }
    false
}

/// Point fd 2 (stderr) at a file for the rest of the process, so panics and
/// `dbg!` don't scribble over the terminal's escape stream.
pub fn redirect_stderr(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::fd::AsFd;
    let f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    rustix::stdio::dup2_stderr(f.as_fd())?;
    Ok(())
}

type FrameIter = Box<dyn Iterator<Item = ffmpeg_sidecar::event::OutputVideoFrame> + Send>;

pub struct Mp4Stream {
    frames: FrameIter,
}

impl Mp4Stream {
    pub fn from_path(path: PathBuf) -> Self {
        Self {
            frames: spawn_raw(&path),
        }
    }
}

fn spawn_raw(path: &std::path::Path) -> FrameIter {
    use ffmpeg_sidecar::command::FfmpegCommand;

    let mut cmd = FfmpegCommand::new();
    cmd.input(path.as_os_str());
    let frames = cmd
        .rawvideo()
        .spawn()
        .unwrap()
        .iter()
        .unwrap()
        .filter_frames();
    Box::new(frames)
}

impl Streamer for Mp4Stream {
    fn next_frame(&mut self) -> Option<RgbImage> {
        let frame = self.frames.next()?;
        image::RgbImage::from_raw(frame.width, frame.height, frame.data)
    }
}

/// SIMD + rayon downscaler for the ▀ preview grid. Kept across frames so the
/// resizer's filter kernels and the destination buffer are reused; the buffer
/// is only reallocated when the source or destination size changes.
pub struct Scaler {
    resizer: Resizer,
    dst: RgbImage,
    src_size: (u32, u32),
}

impl Scaler {
    fn new() -> Self {
        Self {
            resizer: Resizer::new(),
            dst: RgbImage::new(1, 1),
            src_size: (0, 0),
        }
    }

    /// Downscale `src` to `cols` x `rows * 2` pixels and return the result.
    pub fn scale(&mut self, src: &RgbImage, cols: u32, rows: u32) -> &RgbImage {
        let src_size = (src.width(), src.height());
        let dst_size = (cols, rows * 2);
        if self.dst.dimensions() != dst_size || src_size != self.src_size {
            self.dst = RgbImage::new(dst_size.0, dst_size.1);
            self.src_size = src_size;
        }
        self.resizer.resize(src, &mut self.dst, None).unwrap();
        &self.dst
    }
}

/// Your custom widget: draws the half-block cell grid (▀) into a rect.
#[derive(Default)]
pub struct TgpRendererWidget;

pub struct State {
    stream: Box<dyn Streamer + Send>,
    counter: u32,
    frame_time: std::time::Instant,
    last_frame_start: std::time::Instant,
    stats: Stats,
    history: std::collections::VecDeque<Stats>,
    inner: InnerState,
}

enum InnerState {
    Init,
    Running {
        last_area: Rect,
        shape: (usize, usize),
        tiles: ndarray::Array2<Tile>,
        /// Previous full-res frame, for change detection.
        prev_frame: RgbImage,
        /// Downscaler for the ▀ preview grid.
        scaler: Scaler,
        image: DynImage<easytgp::Png>,
        frame: FrameHandle,
        c: Arc<Controller>,
    },
}

#[derive(Default)]
struct Tile {
    state: TileState,
}

#[derive(Default, PartialEq, PartialOrd, Ord, Eq)]
enum TileState {
    #[default]
    Preview,
    Covering,
    Final,
}

impl State {
    pub fn new(mut streamer: impl Streamer + Send + 'static) -> Self {
        Self {
            counter: 1,
            frame_time: std::time::Instant::now(),
            last_frame_start: std::time::Instant::now(),
            stats: Stats::default(),
            history: std::collections::VecDeque::new(),
            stream: Box::new(streamer),
            inner: InnerState::Init,
        }
    }
}

impl StatefulWidget for TgpRendererWidget {
    type State = State;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        {
            let ms = |d: std::time::Duration| {
                d.as_secs_f64() * 1000.0 / state.history.len().max(1) as f64
            };
            let avg = state
                .history
                .iter()
                .fold(Stats::default(), |acc, s| acc.add(s));
            let s = &avg;
            let line = format!(
                "avg{}/10 itv {:.1} tot {:.1} | dec {:.1} | scl {:.1} | diff {:.1} | enc {:.1} | sync {:.1} | up {:.1}",
                state.history.len(),
                ms(s.interval),
                ms(s.total),
                ms(s.decode),
                ms(s.scale),
                ms(s.diff),
                ms(s.encode),
                ms(s.sync),
                s.uploads as f64 / state.history.len().max(1) as f64,
            );
            buf.set_string(
                area.x,
                area.y + area.height.saturating_sub(1),
                line,
                Style::new(),
            );
        }
        state.counter += 1;
        let now = std::time::Instant::now();
        let interval = now.duration_since(state.last_frame_start);
        state.last_frame_start = now;
        state.frame_time = now;
        state.stats = Stats {
            interval,
            ..Stats::default()
        };

        match &mut state.inner {
            s @ InnerState::Init => {
                let t = std::time::Instant::now();
                let next_frame = state.stream.next_frame().unwrap();
                state.stats.decode = t.elapsed();
                let (width_px, height_px) = (next_frame.width(), next_frame.height());

                let c = Controller::init(stdin());
                let request = c.new_png(next_frame).unwrap();

                let mut image = block_on(request.clone().transmit()).unwrap();

                let shape = (16, 8);
                let (nwidth, nheight) = (
                    (area.width as u32 / shape.0 as u32) * shape.0 as u32,
                    (area.height as u32 / shape.1 as u32) * shape.1 as u32,
                );

                image.place(easytgp::PlacementRequest {
                    cols: Some(nwidth),
                    // rows: Some(nheight),
                    z: Some(i32::MIN),
                    ..Default::default()
                });

                *s = InnerState::Running {
                    last_area: area.clone(),
                    shape,
                    tiles: Array2::default(shape),
                    prev_frame: RgbImage::new(width_px, height_px),
                    scaler: Scaler::new(),
                    frame: image.new_frame(NewFrameRequest {
                        from_frame: FrameSelector::Last,
                        image: request,
                        ..Default::default()
                    }),
                    image: {
                        image.display_last_frame();
                        image
                    },
                    c,
                }
            }
            InnerState::Running {
                last_area,
                shape,
                tiles,
                prev_frame,
                scaler,
                image,
                frame,
                c,
            } => {
                // Decode the next frame on a worker thread so it overlaps the
                // sync wait below.
                let shape_cols = shape.0 as u32;
                let (next_frame, decode) = std::thread::scope(|scope| {
                    let stream = &mut state.stream;
                    let h = scope.spawn(move || {
                        let t = std::time::Instant::now();
                        let full = stream.next_frame().unwrap();
                        (full, t.elapsed())
                    });

                    let t = std::time::Instant::now();
                    block_on(image.sync()).unwrap();
                    state.stats.sync = t.elapsed();

                    h.join().unwrap()
                });
                state.stats.decode = decode;

                // Downscale for the ▀ cell grid. The box is the widget area
                // rounded down to whole tiles, with an even pixel height so
                // every cell gets an upper and a lower pixel.
                let w = (area.width as u32 / shape_cols) * shape_cols;
                let rows = (w as f32 * (next_frame.height() as f32 / next_frame.width() as f32))
                    as u32
                    / 2;
                let t = std::time::Instant::now();
                let downscaled = scaler.scale(&next_frame, w, rows);
                state.stats.scale = t.elapsed();

                let (width_px, height_px) = (next_frame.width(), next_frame.height());
                let (width_u, height_half_u) = (downscaled.width(), downscaled.height() / 2);
                let (tile_width_u, tile_half_height_u) =
                    (width_u / shape.0 as u32, height_half_u / shape.1 as u32);
                if state.counter == 500 {
                    panic!("done");
                }

                // let mut handles = vec![];
                let mut budget = 32;
                for ((tx, ty), tile) in tiles.indexed_iter_mut() {
                    let (tile_width_px, tile_height_px) =
                        (width_px / shape.0 as u32, height_px / shape.1 as u32);

                    let x = tx as u32 * tile_width_px;
                    let y = ty as u32 * tile_height_px;
                    let width = tile_width_px;
                    let height = tile_height_px;

                    // Compare the tile against the previous frame, bailing on
                    // the first differing row (no per-pixel hash).
                    let t = std::time::Instant::now();
                    let mut changed = tile_differs(prev_frame, &next_frame, x, y, width, height);
                    state.stats.diff += t.elapsed();

                    let mut draw_anyway = false;
                    if (changed || tile.state == TileState::Preview) && budget > 0 {
                        budget -= 1;
                        let t = std::time::Instant::now();
                        let next_frame_tile = next_frame.view(x, y, width, height).to_image();
                        image.edit_frame(
                            frame,
                            FrameEdit::Upload(Upload {
                                image: c.new_png(next_frame_tile).unwrap(),
                                x: Some(x),
                                y: Some(y),
                                width: Some(width),
                                height: Some(height),
                            }),
                        );
                        state.stats.encode += t.elapsed();
                        state.stats.uploads += 1;

                        if tile.state == TileState::Preview {
                            draw_anyway = true;
                        }

                        changed = false;

                        tile.state = TileState::Final;
                    }

                    if draw_anyway || changed || tile.state == TileState::Preview {
                        let x_off_u = tx as u32 * tile_width_u;
                        let y_off_u = ty as u32 * tile_half_height_u;
                        for x in x_off_u..(x_off_u + tile_width_u) {
                            for y in y_off_u..(y_off_u + tile_half_height_u) {
                                let ucol = downscaled.get_pixel(x as u32, y as u32 * 2);
                                let bcol = downscaled.get_pixel(x as u32, y as u32 * 2 + 1);

                                let cell = buf
                                    .cell_mut(Position::new(area.x + x as u16, area.y + y as u16))
                                    .unwrap();

                                let fg = Color::Rgb(ucol[0], ucol[1], ucol[2]);
                                let bg = Color::Rgb(bcol[0], bcol[1], bcol[2]);

                                if fg != cell.fg {
                                    cell.set_symbol("▀").set_fg(fg);
                                }

                                if bg != cell.bg {
                                    cell.set_bg(bg);
                                }
                            }
                        }

                        if !draw_anyway {
                            tile.state = TileState::Preview;
                        }
                    }
                }

                *prev_frame = next_frame;

                state.stats.total = state.frame_time.elapsed();

                if state.history.len() == 10 {
                    state.history.pop_front();
                }
                state.history.push_back(state.stats);

                // std::thread::sleep(Duration::from_millis(500));
                // // TODO: use method for iterating.
                // for tx in 0..shape.0 {
                //     for ty in 0..shape.0 {
                //         let Tile { hash, state } = tiles.get_mut((tx, ty)).unwrap();

                //         let w = nwidth / shape.0 as u32;
                //         let h = nheight / shape.1 as u32;

                //         let view = img.view(w * tx as u32, h * ty as u32, w, h);

                //         let new_hash = {
                //             let mut hand = DefaultHasher::new();
                //             for (_, _, p) in view.pixels() {
                //                 p.hash(&mut hand);
                //             }
                //             hand.finish() as u32
                //         };

                //         match state {
                //             TileState::Preview => {}
                //             TileState::Final => todo!(),
                //         }

                //         if *hash != new_hash {}
                //     }
                // }
            }
        }
    }
}
