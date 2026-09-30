//! TGP stress-test binary.
//!
//! Two modes:
//!
//! 1. Default: spawns a grid of independent TGP images, one per grid cell, and
//!    walks them through a synchronous fade: every tile starts white, ramps
//!    linearly toward black over `n_frames` (derived from the interval so the
//!    whole fade lasts `--duration-ms`, default 1s), and the last frame flashes
//!    them all bright red before exiting. Purpose: probe whether a terminal/sink
//!    keeps up — if it lags behind the sequence it won't land on red, exposing
//!    where the pipeline throttles. Reuse/merge/delta logic is deliberately
//!    absent (every frame re-transmits every tile), giving a ceiling for the raw
//!    upload path.
//!
//! 2. `--video`: stream a video file through the kitty *animation backend*,
//!    updating the entire root frame in place each frame (`a=f r=1 x=0 y=0
//!    s=<w> v=<h> X=1` — the whole rect, never replacing the base frame) under
//!    a single image id/placement. The path defaults to `CAPY_VID_PATH` (set by
//!    the dev shell to the public-domain capybara clip). `--duration-ms` caps
//!    the run; if the file ends before that, playback loops from the top. The
//!    per-frame cadence stays configurable via `--interval-ms` (or
//!    `--fps`/`--frame-ms`, whichever is set last).

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
        let res = run_video(&mut out, &cli);
        crossterm::execute!(out, cursor::Show)?;
        crossterm::execute!(out, event::DisableMouseCapture)?;
        execute!(out, LeaveAlternateScreen)?;
        disable_raw_mode()?;
        let _ = out.flush();
        return res;
    }

    // collect() the run so the report can be printed AFTER the alternate screen
    // is left; stdout text written inside the alt buffer disappears on exit.
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

    // Pre-compute the frame color once per frame (shared by all dynamic tiles).
    for frame in 0..n_frames {
        let t0 = Instant::now();
        // Last dynamic frame flashes bright red; otherwise fade white -> black.
        let (cr, cg, cb): (u8, u8, u8) = if frame + 1 == n_frames {
            (255, 0, 0)
        } else {
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
    report_elapsed(report, start, frame_work, tick, total_ms, cli.interval_ms);
    Ok(())
}

/// Stream `cli.video`'s FIRST frame as the animation root (`a=T`), then a tiny
/// probe: one random tile is repainted solid pink via an in-place `a=f r=1`
/// edit. If pink shows, the terminal renders in-place animation edits.
/// `--static-rate 1` freezes the whole image on the first frame (no edits at
/// all), isolating the root transmit from the edit path. `q` quits early.
fn run_video(out: &mut std::io::Stdout, cli: &Cli) -> std::io::Result<()> {
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
    let id = 7000u32 + (std::process::id() % 1000);

    // In --dry-run, discard all draw bytes instead of writing to stdout; the
    // encode/assert work still runs so the pipeline is validated without
    // touching the terminal.
    let mut sink: Box<dyn Write> = if cli.dry_run {
        Box::new(std::io::sink())
    } else {
        Box::new(std::io::BufWriter::new(out))
    };

    let rt = tokio::runtime::Runtime::new()
        .map_err(|e| std::io::Error::other(format!("tokio runtime: {e}")))?;
    let run_probe: std::io::Result<(u64, usize)> = rt.block_on(async move {
        // Pull ONLY the first frame from the video as the root; ignore the rest.
        let mut frames = 0u64;
        let mut bytes_total = 0usize;
        let first = loop {
            match src.next().await {
                Ok(Some(tuigui_streamer::source::FrameUpdate::Frame(f))) => break f,
                Ok(Some(_)) | Ok(None) => {
                    return Err(std::io::Error::other("no frames in video"));
                }
                Err(e) => return Err(std::io::Error::other(format!("video read: {e}"))),
            }
        };

        // Root transmit+place of the first frame, scaled by the terminal size (`c`/
        // `r`) so the full frame is visible regardless of video aspect ratio.
        let ctrl: Vec<(char, String)> = vec![
            ('a', "T".into()),
            ('C', "1".into()),
            ('i', id.to_string()),
            ('s', fw.to_string()),
            ('v', fh.to_string()),
            ('c', term_cols.to_string()),
            ('r', term_rows.to_string()),
            ('p', "1".into()),
        ];
        let png = encode_png(&first.data, fw, fh);
        for ch in tuigui_tgp::proto::chunked_transmit(100u32, ctrl, &png, false) {
            sink.write_all(&ch)?;
            bytes_total += ch.len();
        }
        frames += 1;

        // Repaint one random tile solid pink via an in-place `a=f r=1` edit.
        // With `--static-rate >= 1` skip the edit and just hold the root.
        let pink_edit: Option<Vec<Vec<u8>>> = if cli.static_rate >= 1.0 {
            None
        } else {
            // Pick the capybara tile grid resolution.
            let div = cli.cols.max(1).min(fw.max(1));
            let bw = fw.div_ceil(div);
            let bh = fh.div_ceil(div);

            // Destination: the tile at grid slot (2,2), offset from the top-left
            // by two tile steps in both axes (clamped to stay in-bounds).
            let bx = 2u32;
            let by = 2u32;
            let x = bx * bw;
            let y = by * bh;
            let w = bw.min(fw - x);
            let h = bh.min(fh - y);

            // Tile placed at dest tile (2,2): the TOP-LEFT capy tile (slot 0,0), mostly
            // unaltered except the last 1/5 of pixels, which are overwritten with
            // solid pink. RGBA, like the pink path.
            let sx = 0u32;
            let sy = 0u32;
            let total_px = (w * h) as usize;
            let mut tile = vec![0u8; total_px * 4]; // RGBA, exactly like pink
            for r in 0..h {
                let src_off = ((sy + r) * fw + sx) as usize * 3; // capy frame is Rgb24
                for c in 0..w {
                    let i = (r * w + c) as usize;
                    let d = i * 4;
                    if i >= total_px - total_px / 5 {
                        // keep the original capy pixel unaltered (last 1/5)
                        let s = src_off + (c as usize) * 3;
                        tile[d] = first.data[s];
                        tile[d + 1] = first.data[s + 1];
                        tile[d + 2] = first.data[s + 2];
                    } else {
                        // overwrite this pixel with solid pink
                        tile[d..d + 3].copy_from_slice(&[255, 105, 180]);
                    }
                    tile[d + 3] = 255;
                }
            }

            let ctrl: Vec<(char, String)> = vec![
                ('a', "f".into()),
                ('i', id.to_string()),
                ('r', "1".into()),
                ('x', x.to_string()),
                ('y', y.to_string()),
                ('s', w.to_string()),
                ('v', h.to_string()),
                ('X', "1".into()),
            ];
            let png = encode_png(&tile, w, h);
            Some(tuigui_tgp::proto::chunked_transmit(100u32, ctrl, &png, false))
        };

        // Hold the image on screen for `--duration-ms`, re-issuing the edit on
        // the `--interval-ms` cadence (default 1s) so it stays live.
        let total = Duration::from_millis(cli.duration_ms.max(1));
        let edit_cycle = Duration::from_millis(cli.interval_ms.max(1));
        let mut next_edit = Instant::now();
        loop {
            if start.elapsed() >= total {
                break;
            }
            if let Some(edit) = &pink_edit {
                if Instant::now() >= next_edit {
                    for ch in edit {
                        sink.write_all(ch)?;
                        bytes_total += ch.len();
                    }
                    sink.flush()?;
                    next_edit = Instant::now() + edit_cycle;
                }
            }
            let step = Duration::from_millis(20);
            std::thread::sleep(step.min(total.saturating_sub(start.elapsed())));
            if poll_quit() == Some(true) {
                frames += 1;
                return Ok((frames, bytes_total));
            }
        }
        frames += 1;
        Ok((frames, bytes_total))
    });
    let (frames, bytes_total) = run_probe?;

    let wall = start.elapsed();
    let bytes_mb = bytes_total as f64 / (1024.0 * 1024.0);
    eprintln!(
        "video stress: {frames} frames in {:.1}s ({:.2} MB, {:.1} MB/s, {:.1} fps) \
         src {}x{} term {}x{}",
        wall.as_secs_f64(),
        bytes_mb,
        bytes_mb / wall.as_secs_f64().max(0.001),
        frames as f64 / wall.as_secs_f64().max(0.001),
        fw,
        fh,
        term_cols,
        term_rows,
    );
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
        enc.set_compression(png::Compression::Default);
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
