//! TGP stress-test binary.
//!
//! Two modes:
//!
//! 1. Default: spawns a grid of independent TGP images, one per grid cell, and
//!    walks them through a synchronous fade: every tile starts white, ramps
//!    linearly toward black over `n_frames` (derived from the interval so the
//!    whole fade lasts `--duration-ms`, default 1s), and the last frame flashes
//!    them all bright red, holds it for 500ms regardless of pacing, then
//!    exits. Purpose: probe whether a terminal/sink
//!    keeps up — if it lags behind the sequence it won't land on red, exposing
//!    where the pipeline throttles. Reuse/merge/delta logic is deliberately
//!    absent (every frame re-transmits every tile), giving a ceiling for the raw
//!    upload path.
//!
//! 2. `--video`: stream a video file through the kitty *animation backend*,
//!    updating the entire root frame in place each frame (`a=f r=1 x=0 y=0
//!    s=<w> v=<h> X=1` — the whole rect, never replacing the base frame) under
//!    a single image id/placement. The path defaults to `CAPY_VID_PATH` (set by
//!    the dev shell to the public-domain capybara clip). `--loops` (default 1)
//!    plays the clip that many times start-to-finish before the 500ms red
//!    screen; `--duration-ms` still caps the run. The per-frame cadence stays
//!    configurable via `--interval-ms` (or `--fps`/`--frame-ms`, whichever is
//!    set last).

use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use clap::Parser;
use crossterm::{
    cursor, event, execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use tuigui_streamer::mp4::Mp4VideoSource;
use tuigui_streamer::source::FrameSource;

/// Grid + pacing configuration.
#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Cli {
    /// Grid columns (image cells across).
    #[arg(long, default_value_t = 8)]
    cols: u32,
    /// Grid rows (image cells down).
    #[arg(long, default_value_t = 8)]
    rows: u32,
    /// In --video, the number of cells per side for the tile grid. 0 uses
    /// `--cols x --rows`.
    #[arg(long)]
    blocks: Option<u32>,
    /// In --video, decode resolution divisor: 1 = full, 2 = half of the
    /// resolution, 3 = a third, and so on.
    #[arg(long, default_value_t = 1)]
    lod: u32,
    /// In --video, on-screen scale divisor: 1 = full screen, 2 = half of it,
    /// and so on. The image is physically smaller and takes less terminal space.
    #[arg(long, default_value_t = 1)]
    scale: u32,
    /// Milliseconds between each full-grid re-transmit (every tile recolors).
    #[arg(long, default_value_t = 1000)]
    interval_ms: u64,
    /// Source image pixel width of each tile (placement scales it to the cell).
    #[arg(long, default_value_t = 32)]
    tile_px: u32,
    /// Source image pixel height of each tile.
    #[arg(long, default_value_t = 32)]
    tile_py: u32,
    /// Run for this many total milliseconds, then exit immediately (regardless
    /// of tiles still in flight). Use with `--interval-ms 0` to flood as fast as
    /// possible and observe the point it becomes too much.
    #[arg(long, default_value_t = 1000)]
    duration_ms: u64,
    /// Fraction of tiles (0..=1) that stay frozen on their initial content.
    /// Grid mode: those tiles keep the frame-0 white shade for the whole run.
    /// Video mode: those tiles keep their FIRST frame's pixels until exit.
    /// 0 = everything animates, 1 = nothing but the base frame updates.
    #[arg(long, default_value_t = 0.0)]
    static_rate: f64,
    /// Stream a video through the animation backend instead of the grid. `-`
    /// means "read the path from $CAPY_VID_PATH".
    #[arg(long, value_name = "PATH")]
    video: Option<String>,
    /// With --video, frames per second. Overrides a later `--interval-ms`.
    #[arg(long, default_value_t = 30)]
    fps: u32,
    /// With --video, milliseconds per frame. Overrides a later `--interval-ms`.
    #[arg(long)]
    frame_ms: Option<u64>,
    /// In --video, how many times the video plays start-to-finish before the
    /// 500ms red screen. `--duration-ms` still caps the whole run.
    #[arg(long, default_value_t = 1)]
    loops: u32,
    /// Do all the work (decode, sample tiles, encode, assert) but write nothing
    /// to stdout — no draw commands escape. Useful to validate the pipeline
    /// without touching the terminal.
    #[arg(long)]
    dry_run: bool,
}

fn main() -> std::io::Result<()> {
    let cli = Cli::parse();
    let mut out = std::io::stdout();
    enable_raw_mode()?;
    execute!(out, EnterAlternateScreen, cursor::Hide)?;

    if cli.video.is_some() {
        let mut report = Vec::new();
        let res = run_video(&mut out, &cli, &mut report);
        crossterm::execute!(out, cursor::Show)?;
        crossterm::execute!(out, event::DisableMouseCapture)?;
        execute!(out, LeaveAlternateScreen)?;
        disable_raw_mode()?;
        let _ = out.flush();
        for line in report {
            println!("{line}");
        }
        res
    } else {
        // collect() the run so the report can be printed AFTER the alternate
        // screen is left; stdout text written inside the alt buffer disappears
        // on exit.
        let mut report = Vec::new();
        let res = run(&mut out, &cli, &mut report);
        crossterm::execute!(out, cursor::Show)?;
        crossterm::execute!(out, event::DisableMouseCapture)?;
        execute!(out, LeaveAlternateScreen)?;
        disable_raw_mode()?;
        let _ = out.flush();
        for line in report {
            println!("{line}");
        }
        res
    }
}

/// Resolve the video path: `-` reads `$CAPY_VID_PATH`, anything else is used
/// verbatim. Errors if neither is set.
fn video_path(cli_path: &str) -> std::io::Result<PathBuf> {
    if cli_path != "-" {
        return Ok(PathBuf::from(cli_path));
    }
    match std::env::var("CAPY_VID_PATH") {
        Ok(p) if !p.is_empty() => Ok(PathBuf::from(p)),
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "--video - given but $CAPY_VID_PATH is not set (run inside `nix develop`)",
        )),
    }
}

/// Drive the grid: every `interval_ms`, recolor each tile to a deterministic
/// pseudo-random color and `a=T` re-transmit it under its own stable id.
fn run(out: &mut std::io::Stdout, cli: &Cli, report: &mut Vec<String>) -> std::io::Result<()> {
    let (term_cols, term_rows) = crossterm::terminal::size()?;
    let (term_cols, term_rows) = (term_cols as u32, term_rows as u32);
    let cell_w = term_cols / cli.cols.max(1);
    let cell_h = term_rows / cli.rows.max(1);
    if cell_w == 0 || cell_h == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!(
                "grid {}x{} bigger than terminal {}x{}",
                cli.cols, cli.rows, term_cols, term_rows
            ),
        ));
    }
    // Enable key events so `q` can exit (raw mode already gives them; mouse
    // capture isn't needed).
    crossterm::execute!(std::io::stdout(), event::EnableMouseCapture)?;

    // LCG kept for parity but unused now (uniform fade replaces per-tile rand).
    #[allow(unused_mut)]
    let mut _rng: u64 = Instant::now().elapsed().as_nanos() as u64;
    let start = Instant::now();
    let mut frame_work = Duration::ZERO; // cumulative in-frame transmit/flush time

    // Number of frames so the whole fade lasts ~duration (default 1s). Derived
    // from the interval; a 0 interval floods at a fixed ~8ms cadence.
    let total_ms = cli.duration_ms.max(1);
    let n_frames: u64 = if cli.interval_ms > 0 {
        (total_ms / cli.interval_ms).max(1)
    } else {
        120
    };
    let frame_target = if cli.interval_ms > 0 {
        Duration::from_millis(cli.interval_ms)
    } else {
        Duration::from_millis(1000 / n_frames.max(1))
    };
    let _ = frame_target; // grid path paces via the poll loop below
    let mut tick = 0u64;

    // Reused solid RGBA buffer: recolor its RGB, alpha stays 255. Static tiles
    // get a fixed gray (frame-0 shade) held for the whole run; dynamic tiles
    // follow the white->black fade and the final red flash.
    let mut buf = vec![0u8; (cli.tile_px * cli.tile_py * 4) as usize];

    // Fraction of tiles that never change color (--static). Static tiles always
    // render as the initial white shade so they stay constant across the flash.
    let static_rate = cli.static_rate.clamp(0.0, 1.0);
    let static_cells = ((cli.rows * cli.cols) as f64 * static_rate).round() as u32;

    // Fade is white -> black; the red flash is a separate frame appended after
    // the loop so it always exists (even when the fade is a single frame) and
    // holds 500ms regardless of pacing.
    for frame in 0..n_frames {
        let t0 = Instant::now();

        let (cr, cg, cb): (u8, u8, u8) = {
            let denom = (n_frames.saturating_sub(1)).max(1) as f64;
            let t = frame as f64 / denom; // 0 .. 1
            let v = (255.0 * (1.0 - t)).round().clamp(0.0, 255.0) as u8;
            (v, v, v)
        };
        // Static tiles keep their frame-0 shade (white) for the entire run.
        let static_color: (u8, u8, u8) = (255, 255, 255);
        let mut cell_idx = 0u32;
        for r in 0..cli.rows {
            for c in 0..cli.cols {
                let (x, y) = (c * cell_w, r * cell_h);
                let is_static = cell_idx < static_cells;
                cell_idx += 1;
                let (fr, fg, fb) = if is_static { static_color } else { (cr, cg, cb) };
                for px in buf.chunks_exact_mut(4) {
                    px[0] = fr;
                    px[1] = fg;
                    px[2] = fb;
                    px[3] = 0xff;
                }
                execute!(out, cursor::MoveTo(x as u16, y as u16))?;
                let id = 100 + (r * cli.cols + c);
                let chunks = tuigui_tgp::proto::chunked_transmit(
                    32,
                    vec![
                        ('a', "T".into()),
                        ('C', "1".into()),
                        ('i', id.to_string()),
                        ('s', cli.tile_px.to_string()),
                        ('v', cli.tile_py.to_string()),
                        ('c', cell_w.to_string()),
                        ('r', cell_h.to_string()),
                    ],
                    &buf,
                    false,
                );
                for ch in chunks {
                    out.write_all(&ch)?;
                }
            }
        }
        std::io::stdout().flush()?;
        let frame_ms = t0.elapsed();
        frame_work += frame_ms;
        tick += 1;
        report.push(format!(
            "frame {tick}/{n_frames}: rgb({cr},{cg},{cb}) in {:.2} ms",
            t0.elapsed().as_secs_f64() * 1000.0
        ));
        // Pace to the target while watching for `q`.
        let target = Duration::from_millis(if cli.interval_ms > 0 {
            cli.interval_ms
        } else {
            1000 / n_frames.max(1)
        });
        loop {
            let elapsed = t0.elapsed();
            if elapsed >= target {
                break;
            }
            let step = (target - elapsed).min(Duration::from_millis(50));
            std::thread::sleep(step);
            if let Some(true) = poll_quit() {
                report.push(format!("quitting early at frame {tick}"));
                report_elapsed(report, start, frame_work, tick, total_ms, cli.interval_ms);
                return Ok(());
            }
        }
    }

    // Final red flash: separate frame after the fade, held for 500ms starting
    // only after it has been fully flushed, so it stays visible exactly that
    // long regardless of pacing or transmit time.
    {
        let t0 = Instant::now();
        let (cr, cg, cb): (u8, u8, u8) = (255, 0, 0);
        let static_color: (u8, u8, u8) = (255, 255, 255);
        let mut cell_idx = 0u32;
        for r in 0..cli.rows {
            for c in 0..cli.cols {
                let (x, y) = (c * cell_w, r * cell_h);
                let is_static = cell_idx < static_cells;
                cell_idx += 1;
                let (fr, fg, fb) = if is_static { static_color } else { (cr, cg, cb) };
                for px in buf.chunks_exact_mut(4) {
                    px[0] = fr;
                    px[1] = fg;
                    px[2] = fb;
                    px[3] = 0xff;
                }
                execute!(out, cursor::MoveTo(x as u16, y as u16))?;
                let id = 100 + (r * cli.cols + c);
                let chunks = tuigui_tgp::proto::chunked_transmit(
                    32,
                    vec![
                        ('a', "T".into()),
                        ('C', "1".into()),
                        ('i', id.to_string()),
                        ('s', cli.tile_px.to_string()),
                        ('v', cli.tile_py.to_string()),
                        ('c', cell_w.to_string()),
                        ('r', cell_h.to_string()),
                    ],
                    &buf,
                    false,
                );
                for ch in chunks {
                    out.write_all(&ch)?;
                }
            }
        }
        std::io::stdout().flush()?;
        let frame_ms = t0.elapsed();
        frame_work += frame_ms;
        tick += 1;
        report.push(format!(
            "frame {tick} (red flash): rgb(255,0,0) in {:.2} ms",
            t0.elapsed().as_secs_f64() * 1000.0
        ));
        let hold_start = Instant::now();
        loop {
            let elapsed = hold_start.elapsed();
            if elapsed >= Duration::from_millis(500) {
                break;
            }
            let step = (Duration::from_millis(500) - elapsed).min(Duration::from_millis(50));
            std::thread::sleep(step);
            if let Some(true) = poll_quit() {
                report.push(format!("quitting early during red hold at frame {tick}"));
                report_elapsed(report, start, frame_work, tick, total_ms, cli.interval_ms);
                return Ok(());
            }
        }
    }
    report_elapsed(report, start, frame_work, tick, total_ms, cli.interval_ms);
    Ok(())
}

/// Stream `cli.video` frame by frame. Each frame is transmitted+placed as a
/// brand-new image (fresh id per frame); the terminal's image LRU reclaims the
/// previous frame's image when it ages out, so we never need in-place `a=f`
/// edits. A fixed tile region (grid slot 2,2) is repainted solid pink on every
/// frame as a probe: if you see a pink square tracking on top of the video, the
/// transmit+place pipeline is live end to end. `--static-rate 1` freezes the
/// whole image on the first frame (nothing is re-transmitted). `q` quits early.
fn run_video(out: &mut std::io::Stdout, cli: &Cli, report: &mut Vec<String>) -> std::io::Result<()> {
    let path = video_path(cli.video.as_deref().unwrap_or("-"))?;

    let explicit_ffmpeg = std::env::var("FFMPEG_BIN").ok();
    let explicit_ffprobe = std::env::var("FFPROBE_BIN").ok();
    let mut src = Mp4VideoSource::open(path.clone(), explicit_ffmpeg, explicit_ffprobe, false)
        .map_err(|e| std::io::Error::other(format!("opening {}: {e}", path.display())))?;
    let meta = src.metadata();
    let (fw, fh) = (meta.width, meta.height);

    let (term_cols, term_rows) = crossterm::terminal::size()?;
    let (term_cols, term_rows) = (term_cols as u32, term_rows as u32);

    crossterm::execute!(std::io::stdout(), event::EnableMouseCapture)?;

    let start = Instant::now();
    let id_base = 7000u32 + (std::process::id() % 1000);

    // In --dry-run, discard all draw bytes instead of writing to stdout; the
    // encode/assert work still runs so the pipeline is validated without
    // touching the terminal.
    let mut sink: Box<dyn Write> = if cli.dry_run {
        Box::new(std::io::sink())
    } else {
        Box::new(std::io::BufWriter::new(&mut *out))
    };

    // `--lod` divides the decode resolution (2 = half, 3 = a third, ...).
    let lod = cli.lod.max(1);
    let (fw, fh) = (fw.max(1) / lod, fh.max(1) / lod);

    // The tile grid geometry (in lod-resolution pixels): the frame is split
    // into a `div x div` grid. `--blocks` overrides `--cols`; the probe paints
    // grid slot (2,2) solid pink on every transmitted frame.
    let div = cli
        .blocks
        .unwrap_or(cli.cols)
        .max(1)
        .min(fw.max(1));
    let bw = fw.div_ceil(div);
    let bh = fh.div_ceil(div);
    let bx = 2u32;
    let by = 2u32;
    let tx = bx * bw;
    let ty = by * bh;
    let tw = bw.min(fw - tx);
    let th = bh.min(fh - ty);

    // `--scale` shrinks the on-screen placement (2 = half the terminal, ...).
    let scale = cli.scale.max(1);
    let place_cols = term_cols.max(1) / scale;
    let place_rows = term_rows.max(1) / scale;

    let rt = tokio::runtime::Runtime::new()
        .map_err(|e| std::io::Error::other(format!("tokio runtime: {e}")))?;

    #[derive(Default)]
    struct Stage {
        total: Duration,
        count: u64,
        max: Duration,
    }
    impl Stage {
        fn record(&mut self, d: Duration) {
            self.total += d;
            self.count += 1;
            if d > self.max {
                self.max = d;
            }
        }
    }
    #[derive(Default)]
    struct Timings {
        decode: Stage,
        paint: Stage,
        encode: Stage,
        write: Stage,
        frame: Stage,
        /// Longest idle gap between consecutive frame timings (a proxy for
        /// where the pipeline stalls / the terminal backpressure).
        max_gap: Duration,
    }

    let loops_target = cli.loops.max(1);

    let run_probe: std::io::Result<(u64, usize, u32, Timings)> = rt.block_on(async move {
        let mut frames = 0u64;
        let mut bytes_total = 0usize;
        let total = Duration::from_millis(cli.duration_ms.max(1));
        let mut timings = Timings::default();
        let mut last_frame_end: Option<Instant> = None;
        let mut loops_done = 0u32;

        loop {
            if start.elapsed() >= total {
                break;
            }
            let t_frame = Instant::now();

            // Measure the idle gap before this frame (decode wait / backpressure).
            if let Some(end) = last_frame_end {
                let gap = t_frame.duration_since(end);
                if gap > timings.max_gap {
                    timings.max_gap = gap;
                }
            }

            let t_decode = Instant::now();
            let next = src.next().await;
            let frame = match next {
                Ok(Some(tuigui_streamer::source::FrameUpdate::Frame(f))) => f,
                Ok(Some(_)) | Ok(None) => {
                    loops_done += 1;
                    if loops_done >= loops_target || start.elapsed() >= total {
                        break;
                    }
                    src = Mp4VideoSource::open(
                        path.clone(),
                        std::env::var("FFMPEG_BIN").ok(),
                        std::env::var("FFPROBE_BIN").ok(),
                        false,
                    )
                    .map_err(|e| {
                        std::io::Error::other(format!("reopening {}: {e}", path.display()))
                    })?;
                    continue;
                }
                Err(e) => return Err(std::io::Error::other(format!("video read: {e}"))),
            };
            timings.decode.record(t_decode.elapsed());

            // Downscale this RGB24 frame to the lod resolution, then paint the tile.
            let t_paint = Instant::now();
            let (sw, sh) = (frame.metadata.width as usize, frame.metadata.height as usize);
            let mut px = vec![0u8; (fw as usize) * (fh as usize) * 3];
            for dy in 0..fh {
                // Nearest box-sample source row (same divisor as the decode).
                let sy = ((dy as u64) * (sh as u64) / (fh as u64)) as usize * sw * 3;
                for dx in 0..fw {
                    let sx = ((dx as u64) * (sw as u64) / (fw as u64)) as usize * 3;
                    let d = (dy as usize * fw as usize + dx as usize) * 3;
                    px[d..d + 3].copy_from_slice(&frame.data[sy + sx..sy + sx + 3]);
                }
            }
            // Paint grid slot (2,2) pink in lod-resolution pixel space.
            for r in 0..th {
                let row = ((ty + r) * fw + tx) as usize * 3;
                for c in 0..tw {
                    let off = row + c as usize * 3;
                    px[off] = 255;
                    px[off + 1] = 105;
                    px[off + 2] = 180;
                }
            }
            timings.paint.record(t_paint.elapsed());

            // Transmit+place this frame as a brand-new image. A fresh id per
            // frame means the terminal's LRU cleans up old frames for us and
            // we never do in-place edits. `--scale` shrinks the placement.
            let fid = id_base.wrapping_add(frames as u32);
            let ctrl: Vec<(char, String)> = vec![
                ('a', "T".into()),
                ('C', "1".into()),
                ('i', fid.to_string()),
                ('s', fw.to_string()),
                ('v', fh.to_string()),
                ('c', place_cols.to_string()),
                ('r', place_rows.to_string()),
                ('p', "1".into()),
            ];
            let t_encode = Instant::now();
            let png = encode_png(&px, fw, fh);
            timings.encode.record(t_encode.elapsed());

            let t_write = Instant::now();
            for ch in tuigui_tgp::proto::chunked_transmit(100u32, ctrl, &png, false) {
                sink.write_all(&ch)?;
                bytes_total += ch.len();
            }
            sink.flush()?; // push the frame's TGP bytes out before the next decode
            timings.write.record(t_write.elapsed());

            frames += 1;
            timings.frame.record(t_frame.elapsed());
            last_frame_end = Some(Instant::now());

            if poll_quit() == Some(true) {
                return Ok((frames, bytes_total, loops_done, timings));
            }
        }
        Ok((frames, bytes_total, loops_done, timings))
    });
    let (frames, bytes_total, loops_done, timings) = run_probe?;

    // Final red flash, same hold rule as grid mode: 500ms starting only after
    // the frame is fully flushed, regardless of fps/pacing. Same geometry as
    // playback so it lands where the video was.
    let red = encode_png(&{
        let mut red = vec![0u8; (fw as usize) * (fh as usize) * 3];
        for px in red.chunks_exact_mut(3) {
            px[0] = 255;
            px[1] = 0;
            px[2] = 0;
        }
        red
    }, fw, fh);
    let red_ctrl: Vec<(char, String)> = vec![
        ('a', "T".into()),
        ('C', "1".into()),
        ('i', (id_base.wrapping_add(u32::MAX / 2)).to_string()),
        ('s', fw.to_string()),
        ('v', fh.to_string()),
        ('c', place_cols.to_string()),
        ('r', place_rows.to_string()),
        ('p', "1".into()),
    ];
    let t_red = Instant::now();
    if !cli.dry_run {
        for ch in tuigui_tgp::proto::chunked_transmit(100u32, red_ctrl, &red, false) {
            out.write_all(&ch)?;
        }
        out.flush()?;
    }
    let red_ms = t_red.elapsed();
    report.push(format!(
        "red flash sent in {:.2} ms; holding 500 ms",
        red_ms.as_secs_f64() * 1000.0
    ));
    let hold_start = Instant::now();
    loop {
        let elapsed = hold_start.elapsed();
        if elapsed >= Duration::from_millis(500) {
            break;
        }
        std::thread::sleep((Duration::from_millis(500) - elapsed).min(Duration::from_millis(50)));
        if poll_quit() == Some(true) {
            break;
        }
    }

    let wall = start.elapsed();
    let wall_s = wall.as_secs_f64().max(0.001);
    let bytes_mb = bytes_total as f64 / (1024.0 * 1024.0);
    let fps = frames as f64 / wall.as_secs_f64().max(0.001);
    let avg_ms = wall.as_secs_f64() * 1000.0 / (frames as f64).max(1.0);
    let ns = |st: &Stage| -> String {
        let sum_ms = st.total.as_secs_f64() * 1000.0;
        let a = if st.count > 0 { sum_ms / st.count as f64 } else { 0.0 };
        format!(
            "{:9.1} ms avg | {:9.1} ms max | {:4} samples ({:6.2}% of wall)",
            a,
            st.max.as_secs_f64() * 1000.0,
            st.count,
            st.total.as_secs_f64() / wall_s * 100.0,
        )
    };
    report.push(format!(
        "video stress: {frames} frames over {loops_done}/{loops_target} loop(s) in {wall_s:.1}s \
         ({:.2} MB, {:.1} MB/s, {:.1} fps, {avg_ms:.1} ms/frame) \
         src {fw}x{fh} term {term_cols}x{term_rows}",
        bytes_mb,
        bytes_mb / wall_s,
        fps,
    ));
    report.push("--- per-stage timing (per frame) ---".into());
    report.push(format!("  decode   {}", ns(&timings.decode)));
    report.push(format!("  paint    {}", ns(&timings.paint)));
    report.push(format!("  encode   {}", ns(&timings.encode)));
    report.push(format!("  write    {}", ns(&timings.write)));
    report.push(format!("  frame    {}", ns(&timings.frame)));
    report.push(format!(
        "  total encode time {:.1}s, total write time {:.1}s",
        timings.encode.total.as_secs_f64(),
        timings.write.total.as_secs_f64(),
    ));
    report.push(format!(
        "  max idle gap between frames: {:.1} ms",
        timings.max_gap.as_secs_f64() * 1000.0
    ));
    Ok(())
}

/// Encode `data` (`w` x `h` pixels, 3 bpp RGB or 4 bpp RGBA) into a PNG byte
/// string for TGP `f=100` transmission. The terminal (wuffs) decodes it on
/// receipt.
fn encode_png(data: &[u8], w: u32, h: u32) -> Vec<u8> {
    let rgba = data.len() == (w as usize) * (h as usize) * 4;
    let mut buf = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut buf, w, h);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Fast);
        enc.set_color(if rgba { png::ColorType::Rgba } else { png::ColorType::Rgb });
        let mut wr = enc.write_header().expect("png header");
        wr.write_image_data(data).expect("png data");
    }
    buf
}

/// Emit the run summary into `report` (printed after the alt screen is left):
/// wall-clock elapsed plus how much of it was spent doing transmit+flush work
/// inside frames (vs sleeping/pacing).
fn report_elapsed(
    report: &mut Vec<String>,
    start: Instant,
    frame_work: Duration,
    tick: u64,
    total_ms: u64,
    interval_ms: u64,
) {
    let wall = start.elapsed();
    let frame_ms = frame_work.as_secs_f64() * 1000.0;
    let work_pct = if wall.as_millis() > 0 {
        frame_work.as_millis() as f64 / wall.as_millis() as f64 * 100.0
    } else {
        100.0
    };
    report.push(format!(
        "done: {tick} frames in {} ms (wall), target was {} ms \
         ({:.1}% spent in transmit+flush; avg {:.1} ms/frame)",
        wall.as_millis(),
        total_ms,
        work_pct,
        frame_ms / tick.max(1) as f64
    ));
    let _ = interval_ms;
}

/// Non-blocking: true if `q` or `Ctrl+C` was pressed.
fn poll_quit() -> Option<bool> {
    use crossterm::event::{Event, KeyCode, KeyEventKind};
    while event::poll(Duration::from_millis(0)).ok()? {
        match event::read().ok()? {
            Event::Key(k)
                if k.kind == KeyEventKind::Press
                    && (k.code == KeyCode::Char('q')
                        || k.code == KeyCode::Char('c') && k.modifiers.contains(crossterm::event::KeyModifiers::CONTROL)) =>
            {
                return Some(true);
            }
            _ => {}
        }
    }
    Some(false)
}
