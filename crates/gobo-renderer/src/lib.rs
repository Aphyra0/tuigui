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
use image::{GenericImageView, RgbImage};
use ndarray::{Array2, Array3, ArrayBase};
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

    /// Ask for downscaled frames at `w`x`h` (a second ffmpeg process). Respawns
    /// the scaler only when the target size changes.
    fn set_preview_size(&mut self, w: u32, h: u32);

    /// Next downscaled frame at the size previously requested.
    fn next_preview(&mut self) -> Option<RgbImage>;
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
    path: PathBuf,
    frames: FrameIter,
    preview: Option<FrameIter>,
    preview_size: Option<(u32, u32)>,
}

impl Mp4Stream {
    pub fn from_path(path: PathBuf) -> Self {
        Self {
            frames: spawn_raw(&path, None),
            path,
            preview: None,
            preview_size: None,
        }
    }
}

fn spawn_raw(path: &std::path::Path, scale: Option<(u32, u32)>) -> FrameIter {
    use ffmpeg_sidecar::command::FfmpegCommand;

    let mut cmd = FfmpegCommand::new();
    cmd.input(path.as_os_str());
    if let Some((w, h)) = scale {
        // FFmpeg does the downscale (its own threads) and hands back rgb24.
        cmd.args(["-vf", &format!("scale={w}:{h}")]);
    }
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

    fn set_preview_size(&mut self, w: u32, h: u32) {
        if self.preview_size == Some((w, h)) {
            return;
        }
        self.preview_size = Some((w, h));
        self.preview = Some(spawn_raw(&self.path, Some((w, h))));
    }

    fn next_preview(&mut self) -> Option<RgbImage> {
        let frame = self.preview.as_mut()?.next()?;
        image::RgbImage::from_raw(frame.width, frame.height, frame.data)
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
                image,
                frame,
                c,
            } => {
                // Decode the next frame on a worker thread so it overlaps the
                // sync wait below.
                let shape_cols = shape.0 as u32;
                let (next_frame, downscaled, decode, decode_preview) =
                    std::thread::scope(|scope| {
                        let stream = &mut state.stream;
                        let h = scope.spawn(move || {
                            // Full-res frame (TGP final tiles).
                            let t = std::time::Instant::now();
                            let full = stream.next_frame().unwrap();
                            let decode = t.elapsed();

                            // Downscaled frame (▀ preview cells), produced by a
                            // second ffmpeg process. Still blocks us, so time it
                            // separately as `scl`.
                            let w = (area.width as u32 / shape_cols) * shape_cols;
                            let hh = (w as f32 * (full.height() as f32 / full.width() as f32))
                                as u32
                                / 2;
                            stream.set_preview_size(w, hh * 2);
                            let t = std::time::Instant::now();
                            let preview = stream.next_preview().unwrap();
                            (full, preview, decode, t.elapsed())
                        });

                        let t = std::time::Instant::now();
                        block_on(image.sync()).unwrap();
                        state.stats.sync = t.elapsed();

                        h.join().unwrap()
                    });
                state.stats.decode = decode;
                state.stats.scale = decode_preview;
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
