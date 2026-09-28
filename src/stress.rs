//! TGP stress-test binary.
//!
//! Spawns a grid of independent TGP images, one per grid cell, and walks them
//! through a synchronous fade: every tile starts white, ramps linearly toward
//! black over `n_frames` (derived from the interval so the whole fade lasts
//! `--duration-ms`, default 1s), and the last frame flashes them all bright red
//! before exiting. Purpose: probe whether a terminal/sink keeps up — if it lags
//! behind the sequence it won't land on red, exposing where the pipeline
//! throttles. Reuse/merge/delta logic is deliberately absent (every frame
//! re-transmits every tile), giving a ceiling for the raw upload path.

use std::io::{stdout, Write};
use std::time::{Duration, Instant};

use clap::Parser;
use crossterm::{
    cursor, event, execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use tuigui_tgp::proto::chunked_transmit;

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
    /// Fraction of tiles (0..=1) that keep their frame-0 color and never change.
    /// The rest follow the white->black fade and the final red flash.
    #[arg(long, default_value_t = 0.0)]
    static_rate: f64,
}

fn main() -> std::io::Result<()> {
    let cli = Cli::parse();
    let mut out = stdout();
    enable_raw_mode()?;
    execute!(out, EnterAlternateScreen, cursor::Hide)?;

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
                let chunks = chunked_transmit(
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
        out.flush()?;
        let frame_ms = t0.elapsed();
        frame_work += frame_ms;
        tick += 1;
        report.push(format!(
            "frame {tick}/{n_frames}: rgb({cr},{cg},{cb}) in {:.2} ms",
            t0.elapsed().as_secs_f64() * 1000.0
        ));
        // Pace to the target while watching for `q`.
        loop {
            let elapsed = t0.elapsed();
            if elapsed >= frame_target {
                break;
            }
            let step = (frame_target - elapsed).min(Duration::from_millis(50));
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

/// Non-blocking: true if `q` was pressed.
fn poll_quit() -> Option<bool> {
    use crossterm::event::{Event, KeyCode, KeyEventKind};
    while event::poll(Duration::from_millis(0)).ok()? {
        if let Event::Key(k) = event::read().ok()? {
            if k.kind == KeyEventKind::Press && k.code == KeyCode::Char('q') {
                return Some(true);
            }
        }
    }
    Some(false)
}