//! tuigui: run a GUI app in a headless cage session and stream it into this
//! terminal via TGP.
//!
//! Final shape: `tui ./path/to/binary`.

mod comp_bridge;
mod ui;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use futures::{Stream, StreamExt};
use ratatui::{backend::CrosstermBackend, Terminal};
use tokio::sync::mpsc;
use tuigui_comp::tui::{KeyEvent, KeyState, PointerEvent};
use tuigui_comp::Compositor;
use tuigui_streamer::{FrameSource, Mp4VideoSource, PinkFrameSource};
use tuigui_tgp::proto as tgp;
use tuigui_tgp::{EncoderConfig, EncoderEvent, EncoderError, TgpEncoder};

/// Run a GUI app headlessly and stream it into this terminal via TGP.
#[derive(Parser, Debug)]
#[command(name = "tuigui", version)]
struct Cli {
    /// Path to the app to run in the headless cage session.
    #[arg(value_name = "APP", required_unless_present_any = ["video", "debug_draw", "debug_streaming"])]
    app: Option<String>,

    /// Args passed to the app.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, value_name = "APP_ARGS")]
    app_args: Vec<String>,

    /// Stream frames from an mp4 file into the TUI (bypasses cage/Wayland).
    #[arg(long)]
    video: Option<String>,

    /// With --video, write the TGP byte stream to this file instead of the TUI.
    #[arg(long)]
    out: Option<String>,

    /// With an APP (cage path), write the TGP byte stream to this file instead
    /// of the TUI.
    #[arg(long, value_name = "FILE")]
    cage_out: Option<String>,

    /// With --video, loop the file forever.
    #[arg(long)]
    loop_video: bool,

    /// Paint the whole screen pink via a single TGP transmit+place (no loop).
    #[arg(long)]
    debug_draw: bool,

    /// Stream solid-pink frames continuously through the TGP encoder.
    #[arg(long, conflicts_with = "debug_draw")]
    debug_streaming: bool,

    /// Transmit frames as raw RGBA instead of PNG (`f=100`). PNG is the default;
    /// the terminal decodes it via wuffs, shrinking the bytes a fast sink moves.
    #[arg(long)]
    no_png: bool,

    /// Bits of color depth to keep per channel when encoding PNG (1-8, default
    /// 8 = lossless truecolor). Lower values drop low bits before encoding so
    /// flat UI content compresses into fewer bytes with no quantizer overhead.
    #[arg(long, value_name = "BITS", default_value = "8")]
    color_bits: u8,

    /// Maximum capture frame rate. Defaults to 30 fps; the capture source paces
    /// itself to stay at or below this so it doesn't outrun a slower sink.
    #[arg(long, default_value_t = 30)]
    max_fps: u32,

    /// Scale down the headless output resolution (1.0–10.0). The terminal's
    /// pixel size is divided by this to lower the client's render resolution,
    /// shrinking per-frame bytes. 1.0 keeps the full resolution.
    #[arg(long, default_value_t = 1.0)]
    scale: f32,

    /// Relative DPI for the client's content, independent of the render
    /// resolution (`--scale`). Compensates for `--scale` stretching the canvas
    /// to the pane: `ui_scale / scale` is set as the client's content scale so
    /// the app's UI stays a constant on-screen size. 1.5 is a 1.5x DPI hint.
    #[arg(long, default_value_t = 0.001)]
    ui_scale: f32,

    /// Capture a live screenshot of the caged session to a PNG file every
    /// interval (overwriting), for inspecting the rendered app headlessly.
    #[arg(long)]
    debug_capture: bool,

    /// With --debug-capture, the output PNG path.
    #[arg(long, default_value = "/tmp/tuigui-capture.png")]
    capture_out: String,

    /// With --debug-capture, seconds between screenshots.
    #[arg(long, default_value_t = 1.0)]
    capture_interval: f64,

    /// Don't draw the software cursor overlay on captured frames.
    #[arg(long)]
    no_cursor: bool,

    /// Split the canvas into an NxN block grid and stream changes via the
    /// kitty animation backend (`a=f` rect edits into one root frame).
    /// `10` (default) = 10x10 grid (100 blocks). The transmit+place backend
    /// was removed; this is the only encode path.
    #[arg(long, value_name = "N", default_value_t = 10)]
    blocks: u32,

    /// With --blocks, the downscale factor applied to the *loading* layer — a
    /// coarse copy of the canvas stacked in front (z=1) of the full-res sharp
    /// layer (z=0). A changing block is shown coarse here; once it settles its
    /// rect is cleared to transparent so the sharp layer shows through. `0`
    /// (default) disables the loading layer (single-layer animation backend).
    #[arg(long, value_name = "SCALE", default_value_t = 0)]
    loading_scale: u32,

    /// With --blocks, enable detail-adaptive resolution ceilings (on by default).
    /// Each block's largest neighbor color difference picks its ceiling on a
    /// linear ladder: a solid fill settles at the lowest resolution, a jump
    /// beyond the color-space-minus-dead-zone resolves at full resolution, and
    /// differences in between grade linearly across `--res-levels`. Pass
    /// --no-adaptive for the classic always-full ladder.
    #[arg(long, default_value_t = true)]
    detail_adaptive: bool,

    /// With --blocks --detail-adaptive, the flat band reserved at the top of
    /// the color space (0..255). A neighbor difference inside this band
    /// (diff > 255 - lod-dead-zone) is "delicate" and forces the block to full
    /// resolution; differences below it are graded linearly toward the lowest
    /// resolution (a solid block, diff == 0, sits at level 1).
    #[arg(long, value_name = "V", default_value_t = 32)]
    lod_dead_zone: u8,

    /// With --blocks, tint each transmitted block's payload red in proportion to
    /// how far below the original resolution it is: full-res blocks are
    /// untouched, lower-resolution (more downscaled) blocks are redder, the
    /// coarsest most. Visually flags which blocks were degraded.
    #[arg(long, default_value_t = false)]
    debug_lod: bool,

    /// Opt-in zlib compression of transmitted payloads via the Kitty `o=z`
    /// key (RFC 1950), which the terminal inflates before rendering. Off by
    /// default; `--no-zlib` is provided for symmetry / to force it off. When
    /// set it compresses both raw and PNG frames (PNG already deflates, so raw
    /// is where the win lives).
    #[arg(long, action = clap::ArgAction::SetTrue, default_value_t = false)]
    zlib: bool,

    /// Debuggy block grid: every other block (linear odd index) is filled with
    /// a per-block pseudo-random color that changes each frame; even blocks
    /// carry the real pixels. Visually proves the block grid and per-block
    /// diffing. Implies `--blocks 4` if not already set.
    #[arg(long, default_value_t = false)]
    debug_blocks: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Route tracing to a file (not stderr): the TUI's alternate screen is on
    // the terminal, and any stray log line (a WARN on spawn/drop, an Xwayland
    // notice) would corrupt the live drawing. Diagnostics land in the log file.
    let log_path = std::env::temp_dir().join(format!(
        "tuigui-{}.log",
        std::process::id()
    ));
    let file = std::fs::File::create(&log_path)?;
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "debug".into()),
        )
        .with_writer(std::sync::Mutex::new(file))
        .init();

    install_terminal_panic_hook();

    let cli = Cli::parse();

    if cli.debug_streaming {
        return run_pink_streaming().await;
    }
    if cli.debug_capture {
        return run_capture_png(&cli).await;
    }
    if cli.debug_draw {
        return run_debug_fill().await;
    }
    if let Some(path) = cli.video {
        // Dialect for --out: write raw TGP bytes to a file (no TUI).
        if let Some(out) = cli.out.clone() {
            let blocks = cli.blocks;
            return run_video_to_file(&path, &out, cli.loop_video, blocks, !cli.no_png).await;
        }
        return run_video_tui(&path, cli.blocks, !cli.no_png).await;
    }

    let app_path = cli.app.clone().unwrap();

    // Size the virtual display to the terminal's drawing area so the app's
    // pixels map ~1:1 to on-screen cells. `scale` shrinks the output
    // resolution directly (the client is created at the smaller size), so no
    // post-capture downscale work is needed.
    let (tcols, trows) = terminal_pixel_size()?;
    let scale = cli.scale.clamp(1.0, 10.0);
    let (cw, ch) = scaled_size(tcols, trows, scale);
    let (pc, pr) = pane_placement()?;
    // UI scale hint passed to the client. The smaller `--scale` canvas is
    // stretched to fill the same pane, so to keep the UI a constant physical
    // size the client must render proportionally smaller content: the hint is
    // `--ui-scale / --scale`. The compositor does NOT set a fractional wl_output
    // scale (that shrinks the window); it passes this via the TUIGUI_UI_SCALE
    // env var instead. A neutral output scale keeps the window full-screen.
    let ui_scale = cli.ui_scale.clamp(1.0, 4.0) / scale.max(1.0);

    // 1. Launch the in-process compositor and spawn the app into it. `Client`
    //    is the handle for capture + input; `Compositor` stays alive for the
    //    session and shuts the headless thread down on drop.
    let compositor = Compositor::init_with_options(
        (cw, ch),
        ui_scale as f64,
        tuigui_comp::CompositorOptions {
            show_cursor: !cli.no_cursor,
        },
    )
    .context("starting compositor")?;
    tracing::info!(socket = %compositor.wayland_socket().display(), "compositor up");
    let client = compositor
        .spawn(&app_path, (cw, ch))
        .context("spawning app into compositor")?;

    // 2. Attach a FrameSource that pulls committed frames. The TGP encoder is
    //    codec-agnostic and only sees frames.
    let source = comp_bridge::CompFrameSource::new(client.clone(), cli.max_fps);

    // 3. Encode to a TGP byte stream, scaled to fill the pane (in cells).
    //    Every frame is fully transmitted and placed; no delta/damage logic.
    let blocks_per_side =
        if cli.debug_blocks {
            cli.blocks.max(4)
        } else {
            cli.blocks
        };
    let detail = if cli.detail_adaptive {
        tuigui_streamer::blocks::DetailConfig {
            enabled: true,
            dead_zone: cli.lod_dead_zone,
        }
    } else {
        tuigui_streamer::blocks::DetailConfig::disabled()
    };
    let encoder = TgpEncoder::new(EncoderConfig {
        placement_columns: pc,
        placement_rows: pr,
        png: !cli.no_png,
        color_bits: cli.color_bits,
        blocks_per_side,
        loading_scale: cli.loading_scale,
        detail,
        debug_lod: cli.debug_lod,
        debug_blocks: cli.debug_blocks,
        pane_origin: Some((0, 0)),
        zlib: cli.zlib,
    });
    let mut tgp_stream = encoder.into_stream(source);

    // 4. Optionally dump raw TGP bytes to a file instead of the TUI, to
    //    inspect exactly what the capture+encoder produce.
    if let Some(out) = cli.cage_out {
        return drain_to_file(&mut tgp_stream, &out).await;
    }

    // 5. Run the ratatui shell.
    let terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;

    // Input pipeline: the render loop pushes events into a channel (never
    // blocking), and a dedicated std thread drains them into the compositor's
    // seat. This fully decouples input I/O (which can block on the cross-thread
    // channel) from the render loop, so flooding mouse events can never stall
    // the stream or the quit key.
    let (tx, rx) = mpsc::unbounded_channel::<InputMsg>();
    let writer_client = client.clone();
    std::thread::Builder::new()
        .name("tuigui-comp-input".into())
        .spawn(move || {
            comp_bridge::input_writer_loop(&writer_client, rx);
        })
        .context("spawning input writer thread")?;

    let res = run_ui(terminal, &mut tgp_stream, &app_path, (cw, ch), tx).await;
    // Drop tx is handled inside run_ui; the input thread observes the channel
    // close and exits. Shut the compositor thread down now that streaming is
    // over.
    compositor.shutdown();
    res
}

/// Stream a video file's decoded frames through the TGP encoder into a file.
async fn run_video_to_file(path: &str, out: &str, loop_forever: bool, blocks: u32, png: bool) -> Result<()> {
    let explicit_ffmpeg = std::env::var("FFMPEG_BIN").ok();
    let explicit_ffprobe = std::env::var("FFPROBE_BIN").ok();
    tracing::info!(?explicit_ffmpeg, ?explicit_ffprobe, "video mode ffmpeg overrides");

    let src = Mp4VideoSource::open(
        PathBuf::from(path),
        explicit_ffmpeg,
        explicit_ffprobe,
        loop_forever,
    )
    .context("opening video source")?;
    let meta = src.metadata();
    tracing::info!(width = meta.width, height = meta.height, "video opened");

    let encoder = TgpEncoder::new(EncoderConfig {
        png,
        blocks_per_side: blocks,
        placement_columns: 80,
        placement_rows: 24,
        ..Default::default()
    });
    let stream = encoder.into_stream(src);
    drain_to_file(stream, out).await
}

/// Restore the terminal (leave alternate screen, disable raw mode, release the
/// mouse, show the cursor) on panic, so a crash anywhere — including inside the
/// spawned encoder task — never leaves the shell in raw mode with mouse capture
/// on, which would print mouse-report / escape garbage (e.g. `24M35`) instead
/// of a working prompt. Pure `crossterm` calls: the panic path must be
/// infallible.
fn install_terminal_panic_hook() {
    use std::panic;
    use std::sync::Once;
    static HOOK: Once = Once::new();
    HOOK.call_once(|| {
        let prev = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            let _ = crossterm::terminal::disable_raw_mode();
            let _ = crossterm::execute!(
                std::io::stdout(),
                crossterm::terminal::LeaveAlternateScreen,
                crossterm::event::DisableMouseCapture,
                crossterm::cursor::Show
            );
            prev(info);
        }));
    });
}

/// Query the terminal's current size and report it in pixels, suitable for
/// sizing the cage virtual output. Reads the cell grid via crossterm and
/// multiplies by the real cell size queried from the terminal (`CSI 16 t`).
///
/// The cell size cannot be assumed: hardcoding 8x16 gives a wrong resolution
/// on HiDPI, non-standard fonts, or ghostty/foot/herdr (which often report
/// zero pixels in `TIOCGWINSZ`). Querying the terminal directly is the only
/// reliable source.
fn terminal_pixel_size() -> Result<(u32, u32)> {
    let (cols, rows) = crossterm::terminal::size().context("querying terminal size")?;
    if cols == 0 || rows == 0 {
        return Err(anyhow::anyhow!("terminal reports zero cell grid size"));
    }
    let (cell_w, cell_h) = query_cell_size()?;
    Ok((cols as u32 * cell_w, rows as u32 * cell_h))
}

/// Ask the terminal for its cell size in pixels via `CSI 16 t` (returns
/// `CSI 4 ; height ; width t`), blocking up to `TIMEOUT`. Must run before raw
/// mode so crossterm's stdin read sees the reply. Falls back to the value
/// `TIOCGWINSZ` reports when the terminal doesn't answer.
fn query_cell_size() -> Result<(u32, u32)> {
    use std::io::Write as _;
    use std::time::{Duration, Instant};

    // Send the query, then read the reply from the controlling terminal.
    fn read_reply(deadline: Instant) -> Vec<u8> {
        use std::fs::OpenOptions;
        use std::io::Read as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        // Raw mode isn't on yet, so opening /dev/tty gives us the terminal
        // without tripping over crossterm's buffered stdin.
        let Ok(mut tty) = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open("/dev/tty")
        else {
            return Vec::new();
        };
        let mut buf = Vec::with_capacity(128);
        let mut tmp = [0u8; 64];
        loop {
            if Instant::now() >= deadline {
                break;
            }
            match tty.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
                Err(_) => std::thread::sleep(Duration::from_millis(5)),
            }
        }
        buf
    }

    let mut out = std::io::stdout();
    out.write_all(b"\x1b[16t").ok();
    out.flush().ok();

    let bytes = read_reply(Instant::now() + Duration::from_millis(300));
    // `CSI 16 t` replies `CSI 4 ; cell_height ; cell_width t` — the pixel
    // dimensions of a single cell, not the window. Use them directly.
    if let Ok(s) = std::str::from_utf8(&bytes) {
        if let Some(idx) = s.find("\x1b[4;") {
            let rest = &s[idx + 4..];
            let end = rest.find('t').unwrap_or(rest.len());
            let mut it = rest[..end].split(';');
            let cell_h: u32 = it.next().and_then(|v| v.trim().parse().ok()).unwrap_or(0);
            let cell_w: u32 = it.next().and_then(|v| v.trim().parse().ok()).unwrap_or(0);
            if cell_w > 0 && cell_h > 0 {
                return Ok((cell_w, cell_h));
            }
        }
    }

    // The terminal didn't answer `CSI 16 t`; fall back to the pixel dims the
    // kernel reports via TIOCGWINSZ, if any.
    if let Ok(ws) = crossterm::terminal::window_size() {
        if ws.width > 0 && ws.height > 0 && ws.columns > 0 && ws.rows > 0 {
            return Ok((
                (ws.width as u32 / ws.columns as u32).max(1),
                (ws.height as u32 / ws.rows as u32).max(1),
            ));
        }
    }
    Err(anyhow::anyhow!(
        "could not determine terminal cell size (CSI 16 t unanswered, TIOCGWINSZ empty)"
    ))
}

/// The streamed image's area in terminal cells — the rectangle the TGP image
/// is placed into. Must match the layout in `ui::draw`: no header, three status
/// footer rows, and no border around the body.
fn pane_placement() -> Result<(u32, u32)> {
    let (cols, rows) = crossterm::terminal::size().context("querying terminal size")?;
    if rows <= 3 {
        return Ok((cols as u32, rows as u32));
    }
    Ok((cols as u32, (rows - 3) as u32))
}

/// Divide a full-resolution pixel size by `scale` (>=1), floors, and never
/// lets a dimension drop below 1. The compositor is created at this lower
/// resolution so the captured frames stream directly without a downscale step.
fn scaled_size(w: u32, h: u32, scale: f32) -> (u32, u32) {
    let s = scale.max(1.0);
    (
        ((w as f32) / s).floor().max(1.0) as u32,
        ((h as f32) / s).floor().max(1.0) as u32,
    )
}

/// Drain any TGP stream writing raw bytes to `out` until Ended.
async fn drain_to_file<S>(mut stream: S, out: &str) -> Result<()>
where
    S: Unpin + Stream<Item = Result<EncoderEvent, EncoderError>>,
{
    use std::io::Write as _;
    let mut file = std::io::BufWriter::new(std::fs::File::create(out)?);
    let mut chunks = 0usize;
    let mut bytes = 0usize;
    while let Some(ev) = stream.next().await {
        match ev? {
            EncoderEvent::Bytes(b) => {
                chunks += 1;
                bytes += b.len();
                file.write_all(&b)?;
            }
            EncoderEvent::Frame { .. } => {}
            EncoderEvent::Ended => break,
        }
    }
    file.flush()?;
    tracing::info!(chunks, bytes, out, "video encode finished");
    Ok(())
}

/// Stream the decoded frames of `path` through the TGP encoder into the live
/// ratatui TUI shell (no cage, no Wayland): validates the display path a
/// TGP-capable terminal sees when consuming our byte stream.
async fn run_video_tui(path: &str, blocks: u32, png: bool) -> Result<()> {
    let explicit_ffmpeg = std::env::var("FFMPEG_BIN").ok();
    let explicit_ffprobe = std::env::var("FFPROBE_BIN").ok();
    let src = Mp4VideoSource::open(
        PathBuf::from(path),
        explicit_ffmpeg,
        explicit_ffprobe,
        true, // loop forever
    )
    .context("opening video source")?;
    let meta = src.metadata();
    tracing::info!(width = meta.width, height = meta.height, "video opened in TUI (looping)");

    // Animation backend with PNG: one root frame, per-block a=f edits. Use the
    // same pane placement as the app path (body minus the footer rows) so the
    // image is exactly as large as ui::draw's body rect.
    let (pc, pr) = pane_placement()?;
    let encoder = TgpEncoder::new(EncoderConfig {
        png,
        blocks_per_side: blocks,
        placement_columns: pc,
        placement_rows: pr,
        ..Default::default()
    });
    let mut tgp_stream = encoder.into_stream(src);

    let terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    // No Wayland session here; forward inputs into a discard sink.
    let (tx, _rx) = mpsc::unbounded_channel::<InputMsg>();
    run_ui(terminal, &mut tgp_stream, &format!("video: {path}"), (0, 0), tx).await
}

/// A queued input event for the writer task: keyboard or pointer.
enum InputMsg {
    Key(KeyEvent),
    Pointer(PointerEvent),
}

async fn run_ui(
    mut terminal: Terminal<CrosstermBackend<std::io::Stdout>>,
    tgp_stream: &mut (impl Unpin + Stream<Item = Result<EncoderEvent, EncoderError>>),
    app_path: &str,
    output_px: (u32, u32),
    tx: mpsc::UnboundedSender<InputMsg>,
) -> Result<()> {
    crossterm::terminal::enable_raw_mode()?;
    crossterm::execute!(
        std::io::stdout(),
        crossterm::terminal::EnterAlternateScreen,
        crossterm::event::EnableMouseCapture
    )?;
    terminal.clear()?;

    // Restore the terminal on every exit path (including early `?` returns and
    // panics) so raw mode and mouse capture are never left on — otherwise after
    // a mid-loop error the shell prints mouse-report escapes (e.g. `24M35`)
    // instead of a working prompt.
    struct TermGuard;
    impl Drop for TermGuard {
        fn drop(&mut self) {
            let _ = crossterm::terminal::disable_raw_mode();
            let _ = crossterm::execute!(
                std::io::stdout(),
                crossterm::terminal::LeaveAlternateScreen,
                crossterm::event::DisableMouseCapture,
                crossterm::cursor::Show
            );
        }
    }
    let _termon = TermGuard;

    let mut last_event = "starting".to_string();
    let mut pane: Option<ratatui::layout::Rect> = None;
    let mut quit = false;

    // Input listening runs on its own thread so the render loop never blocks
    // on `crossterm::event::poll`/`read`. The thread blocks in read() forever
    // (raw mode is on) and pushes crossterm events into a channel; the loop
    // below drains it with a non-blocking try_recv, removing the ~4ms poll
    // toll per frame. It defines the `q` key, forwarded to the main loop.
    let (input_tx, input_rx) = std::sync::mpsc::channel::<crossterm::event::Event>();
    let inputs_thread = std::thread::Builder::new()
        .name("inputs_thread".into())
        .spawn(move || {
            loop {
                match crossterm::event::read() {
                    Ok(ev) => {
                        if input_tx.send(ev).is_err() {
                            break; // render loop gone
                        }
                    }
                    Err(_) => break,
                }
            }
        })
        .context("spawning inputs_thread")?;

    // Metrics for the status bar.
    // Seed the resolution from the terminal's pixel size we sized the headless
    // output to, so the footer shows it even before the first frame event (and
    // regardless of whether the source reports per-frame dims).
    let mut stats = ui::Stats {
        resolution: output_px,
        ..ui::Stats::default()
    };
    // Rolling one-second window for per-frame measurements.
    let mut t_prev = std::time::Instant::now();
    let mut frame_bytes_accum = 0usize;
    let mut cap_ms_accum = 0f64;
    let mut pacing_ms_accum = 0f64;
    let mut encode_ms_accum = 0f64;
    let mut write_ms_accum = 0f64;
    let mut scale_ms_accum = 0f64;
    let mut frame_count_accum = 0usize;
    let mut announce_accum = 0f64;
    let mut setup_accum = 0f64;
    let mut copy_accum = 0f64;
    let mut readout_accum = 0f64;
    let mut draw_ms_accum = 0f64;

    while !quit {
        // Draw the shell; capture the pane's inner rect where the image lands.
        let draw_t0 = std::time::Instant::now();
        terminal.draw(|f| {
            pane = Some(ui::draw(f, app_path, &last_event, &stats));
        })?;
        draw_ms_accum += draw_t0.elapsed().as_secs_f64() * 1000.0;

        // Drain encoder output without blocking the UI loop. The whole batch
        // this iteration belongs to one pane region, so park the cursor at the
        // pane origin once, then stream every drained chunk before a single
        // flush. The terminal rasterizes these bytes into the live-capture
        // region; `C=1` on the transmit command keeps the cursor put, so each
        // chunk lands exactly at the pane's top-left cell.
        use std::io::Write as _;
        // Wrap stdout in an explicit BufWriter so `Sink:` measures real
        // userspace buffering costs instead of LineWriter's all-or-nothing
        // pass-through: LineWriter flushes on every newline and large chunks
        // bypass its buffer entirely, making write_all near-free and the stat
        // useless. A 256 KiB buffer accumulates the whole drain batch, so the
        // single flush at the end is where the PTY handoff cost lands.
        let mut out = std::io::BufWriter::with_capacity(256 * 1024, std::io::stdout().lock());
        if let Some(rect) = pane {
            crossterm::execute!(out, crossterm::cursor::MoveTo(rect.x, rect.y))?;
        }
        let mut written_chunks = 0usize;
        // Peak tiles edited / total observed across this drain batch. The
        // encoder can flood many source frames (most near-identical tails) per
        // UI iteration, so the *last* Frame event would usually read ~0%; take
        // the max so a real change spike survives to the next draw.
        let mut batch_tiles_edited: u32 = 0;
        let mut batch_tiles_total: u32 = 0;
        let mut written_bytes = 0usize;
        // `Sink:` measures the handoff of this batch to the terminal: from the
        // moment the first byte is written to when the flush hands the whole
        // batch to the PTY and blocks until the terminal consumes it. One timer
        // spans the entire drain + flush, so userspace buffering in the
        // BufWriter isn't over-counted per chunk and terminal backpressure
        // after the flush is fully included.
        let mut sink_t0: Option<std::time::Instant> = None;
        // Bail out after this many bytes per frame so the UI loop always
        // yields to redraw + key polling, even when a fast encoder floods the
        // (now bounded) channel every iteration.
        const MAX_DRAIN_BYTES: usize = 1024 * 1024;
        // Drain with a real await so the stream gets a proper waker. A plain
        // `futures::poll!` uses a noop waker: once the channel is momentarily
        // empty it returns Pending -> break and is never woken again, freezing
        // mid-frame. `timeout` relinquishes the loop to redraw + key polling
        // when no bytes arrive for ~4ms.
        loop {
            if written_bytes >= MAX_DRAIN_BYTES {
                break;
            }
            match tokio::time::timeout(std::time::Duration::from_millis(4), tgp_stream.next())
                .await
            {
                Ok(Some(Ok(EncoderEvent::Bytes(b)))) if !b.is_empty() => {
                    written_chunks += 1;
                    written_bytes += b.len();
                    // Start the batch's sink timer on the first byte written.
                    if sink_t0.is_none() {
                        sink_t0 = Some(std::time::Instant::now());
                    }
                    out.write_all(&b)?;
                }
                // Empty batch means nothing more to drain right now; stop and
                // return control to the UI loop so keys/mouse stay responsive
                // even when the encoder idles between frames. Spinning on
                // empty chunks would busy-loop and starve input polling.
                Ok(Some(Ok(EncoderEvent::Bytes(_)))) => break,
                Ok(Some(Ok(EncoderEvent::Frame { width, height, capture_ms, pacing_ms, encode_ms, timing, tiles_edited, tiles_total, downscale_ms }))) => {
                    // Record the frame's resolution and capture cost into the
                    // rolling window.
                    frame_count_accum += 1;
                    cap_ms_accum += capture_ms;
                    pacing_ms_accum += pacing_ms;
                    encode_ms_accum += encode_ms;
                    scale_ms_accum += downscale_ms;
                    stats.resolution = (width, height);
                    batch_tiles_edited = batch_tiles_edited.max(tiles_edited);
                    batch_tiles_total = batch_tiles_total.max(tiles_total);
                    if let Some(t) = timing {
                        announce_accum += t.announce_ms;
                        setup_accum += t.setup_ms;
                        copy_accum += t.copy_ms;
                        readout_accum += t.readout_ms;
                    }
                }
                Ok(Some(Ok(EncoderEvent::Ended))) => {
                    last_event = "capture ended".into();
                    quit = true;
                    break;
                }
                Ok(Some(Err(e))) => {
                    last_event = format!("encoder error: {e}");
                }
                Ok(None) => {
                    last_event = "encoder finished".into();
                    quit = true;
                    break;
                }
                Err(_elapsed) => break,
            }
        }
        // Fold this batch's peak tile-edit fraction into displayed stats.
        stats.tiles_edited = batch_tiles_edited;
        stats.tiles_total = batch_tiles_total;
        out.flush()?;
        // The flush is the handoff: it pushes the buffered batch to the PTY
        // and blocks until the terminal consumes it. Measure the whole batch
        // (first write through flush) so userspace buffering doesn't inflate
        // per-chunk writes and terminal backpressure is included.
        if let Some(t0) = sink_t0 {
            write_ms_accum += t0.elapsed().as_secs_f64() * 1000.0;
        }

        // Accrue this iteration's transmitted bytes into the rolling window.
        frame_bytes_accum += written_bytes;
        // Fold the accumulated per-frame counters into the displayed averages
        // roughly once per second (the render loop iterates far faster).
        let now = std::time::Instant::now();
        let elapsed = now.saturating_duration_since(t_prev);
        if frame_count_accum > 0 && elapsed >= std::time::Duration::from_secs(1) {
            let secs = elapsed.as_secs_f64().max(1e-9);
            let n = frame_count_accum as f64;
            stats.fps = frame_count_accum as f64 / secs;
            stats.bandwidth = frame_bytes_accum as f64 / secs;
            stats.capture_ms = cap_ms_accum / n;
            stats.pacing_ms = pacing_ms_accum / n;
            stats.encode_ms = encode_ms_accum / n;
            stats.sink_ms = write_ms_accum / n;
            stats.downscale_ms = scale_ms_accum / n;
            stats.announce_ms = announce_accum / n;
            stats.setup_ms = setup_accum / n;
            stats.copy_ms = copy_accum / n;
            stats.readout_ms = readout_accum / n;
            stats.draw_ms = draw_ms_accum / n;
            t_prev = now;
            frame_count_accum = 0;
            frame_bytes_accum = 0;
            cap_ms_accum = 0.0;
            pacing_ms_accum = 0.0;
            encode_ms_accum = 0.0;
            write_ms_accum = 0.0;
            scale_ms_accum = 0.0;
            announce_accum = 0.0;
            setup_accum = 0.0;
            copy_accum = 0.0;
            readout_accum = 0.0;
            draw_ms_accum = 0.0;
        }
        tracing::info!(
            pane = ?pane.map(|r| (r.x, r.y, r.width, r.height)),
            written_chunks,
            written_bytes,
            last_event = %last_event,
            "ui drain iteration"
        );

        // Forward input: read whatever the inputs_thread already parsed and
        // push it to the session's input sink. Non-blocking try_recv — the
        // polling happens on `inputs_thread`, so this loop iteration never
        // stalls waiting for a key or mouse event.
        while let Ok(ev) = input_rx.try_recv() {
            match ev {
                crossterm::event::Event::Key(k) => {
                    if k.kind == crossterm::event::KeyEventKind::Press
                        && k.code == crossterm::event::KeyCode::Char('q')
                    {
                        quit = true;
                    }
                    if let Some(kev) = crossterm_key_to_key(k) {
                        stats.input_state = "key";
                        let _ = tx.send(InputMsg::Key(kev));
                    }
                }
                crossterm::event::Event::Mouse(m) => {
                    let pevs = mouse_to_pointer(m, pane, output_px);
                    for pev in &pevs {
                        match pev {
                            PointerEvent::Motion { x, y } => {
                                stats.cursor_x = *x;
                                stats.cursor_y = *y;
                                stats.input_state = "motion";
                            }
                            PointerEvent::Button { state, .. } => {
                                stats.input_state = if *state == KeyState::Press {
                                    "click"
                                } else {
                                    "click-released"
                                };
                            }
                            PointerEvent::Axis { .. } => {
                                stats.input_state = "scroll";
                            }
                        }
                        let _ = tx.send(InputMsg::Pointer(pev.clone()));
                    }
                }
                _ => {}
            }
        }
    }

    crossterm::execute!(std::io::stdout(), crossterm::terminal::LeaveAlternateScreen)?;
    crossterm::terminal::disable_raw_mode()?;
    // Close the input channel so the writer task's recv() returns None and it
    // exits; otherwise the runtime waits on it forever and the binary hangs.
    drop(tx);
    // Drop the receiver so the inputs_thread's send breaks; it also can't be
    // joined cleanly (it blocks in read() until raw mode drops, then exits).
    drop(input_rx);
    let _ = inputs_thread;
    Ok(())
}

/// Translate a crossterm key code into a [`KeyEvent`] for the session, or
/// `None` if it maps to nothing we forward (control/function keys so far). Only
/// presses are forwarded; auto-repeat is dropped by kind.
fn crossterm_key_to_key(k: crossterm::event::KeyEvent) -> Option<KeyEvent> {
    use crossterm::event::KeyEventKind;
    if k.kind != KeyEventKind::Press {
        return None;
    }
    use crossterm::event::KeyCode::{
        Backspace, Char, Delete, Down, End, Enter, Esc, Home, Left, Right, Tab, Up,
    };
    let code = match k.code {
        Char(c) => char_to_evdev(c)?,
        Enter => 28,     // KEY_ENTER
        Tab => 15,       // KEY_TAB
        Backspace => 14, // KEY_BACKSPACE
        Esc => 1,        // KEY_ESC
        Left => 105,     // KEY_LEFT
        Right => 106,    // KEY_RIGHT
        Up => 103,       // KEY_UP
        Down => 108,     // KEY_DOWN
        Home => 102,     // KEY_HOME
        End => 107,      // KEY_END
        Delete => 111,   // KEY_DELETE
        _ => return None,
    };
    Some(KeyEvent { code, state: KeyState::Press })
}

/// Map a displayable ASCII char to its Linux evdev keycode. Returns `None` for
/// non-printable/uppercase-modified keys; callers may treat that as "skip".
fn char_to_evdev(c: char) -> Option<u32> {
    // Lowercase ASCII maps to the known layout-independent range.
    let c = c.to_ascii_lowercase();
    let code = match c {
        'a'..='z' => 30 + (c as u8 - b'a') as u32,
        '0'..='9' => 27 + (c as u8 - b'0') as u32,
        ' ' => 57,   // KEY_SPACE
        '-' => 12,   // KEY_MINUS
        '=' => 13,   // KEY_EQUAL
        '[' => 26,   // KEY_LEFTBRACE
        ']' => 27,   // KEY_RIGHTBRACE
        ';' => 39,   // KEY_SEMICOLON
        '\'' => 40,  // KEY_APOSTROPHE
        '`' => 41,   // KEY_GRAVE
        '\\' => 43,  // KEY_BACKSLASH
        ',' => 51,   // KEY_COMMA
        '.' => 52,   // KEY_DOT
        '/' => 53,   // KEY_SLASH
        '\n' => 28,  // KEY_ENTER
        _ => return None,
    };
    Some(code)
}

/// Convert a crossterm mouse event into virtual-pointer events, mapping cursor
/// cell coords (relative to the pane) into virtual-frame pixels via the pane
/// rect and the virtual output's pixel size. Pure: returns the events to send.
fn mouse_to_pointer(
    m: crossterm::event::MouseEvent,
    pane: Option<ratatui::layout::Rect>,
    output_px: (u32, u32),
) -> Vec<PointerEvent> {
    let mut out = Vec::new();
    use crossterm::event::{MouseButton as B, MouseEventKind as K};
    // Require a real pane rect to map cells -> pixels. With no pane there is
    // no drawing surface, so no pointer events to inject: return early rather
    // than fabricate coordinates from silent defaults.
    let pane = match pane {
        Some(r) if r.width > 0 && r.height > 0 => r,
        _ => return Vec::new(),
    };
    let (px, py, pw, ph) = (pane.x, pane.y, pane.width, pane.height);
    // cell (relative to pane) -> normalized [0,1] -> virtual-frame pixel.
    let (fw, fh) = (output_px.0 as f64, output_px.1 as f64);
    let to_px = |col: u16, row: u16| {
        let nx = ((col as f64 - px as f64) / pw as f64).clamp(0.0, 1.0);
        let ny = ((row as f64 - py as f64) / ph as f64).clamp(0.0, 1.0);
        (nx * fw, ny * fh)
    };
    match m.kind {
        K::Moved => {
            let (x, y) = to_px(m.column, m.row);
            out.push(PointerEvent::Motion { x, y });
        }
        K::Drag(b) => {
            let (x, y) = to_px(m.column, m.row);
            out.push(PointerEvent::Motion { x, y });
            let _ = b;
        }
        K::Down(b) => {
            let code = match b {
                B::Left => 0x110,
                B::Right => 0x111,
                B::Middle => 0x112,
            };
            let (x, y) = to_px(m.column, m.row);
            out.push(PointerEvent::Motion { x, y });
            out.push(PointerEvent::Button {
                code,
                state: KeyState::Press,
            });
        }
        K::Up(b) => {
            let code = match b {
                B::Left => 0x110,
                B::Right => 0x111,
                B::Middle => 0x112,
            };
            out.push(PointerEvent::Button {
                code,
                state: KeyState::Release,
            });
        }
        K::ScrollDown => {
            out.push(PointerEvent::Axis { dx: 0.0, dy: -1.0 });
        }
        K::ScrollUp => {
            out.push(PointerEvent::Axis { dx: 0.0, dy: 1.0 });
        }
        _ => {}
    }
    out
}

/// `--debug-capture`: spawn the app inside the compositor exactly like the
/// real path, but instead of streaming the frames to the TGP encoder, write a
/// live screenshot of the app to a PNG file every `interval` seconds
/// (overwriting the previous one). Use it to inspect what the headless app
/// actually renders.
async fn run_capture_png(cli: &Cli) -> Result<()> {
    let app_path = cli.app.clone().unwrap();
    let (tcols, trows) = terminal_pixel_size()?;

    let compositor = Compositor::init_with_options(
        (tcols, trows),
        1.0,
        tuigui_comp::CompositorOptions {
            show_cursor: !cli.no_cursor,
        },
    )
    .context("starting compositor")?;
    let client = compositor
        .spawn(&app_path, (tcols, trows))
        .context("spawning app into compositor")?;
    let interval = std::time::Duration::from_secs_f64(cli.capture_interval.max(0.05));

    tracing::info!(
        out = %cli.capture_out,
        ?interval,
        "debug-capture: overwriting PNG every interval"
    );
    loop {
        match client.capture() {
            Ok(frame) => {
                let mut file = std::io::BufWriter::new(std::fs::File::create(&cli.capture_out)?);
                {
                    let mut enc = png::Encoder::new(&mut file, frame.width, frame.height);
                    enc.set_color(png::ColorType::Rgba);
                    enc.set_depth(png::BitDepth::Eight);
                    let mut writer = enc.write_header()?;
                    writer.write_image_data(&frame.rgba)?;
                }
                tracing::info!(
                    out = %cli.capture_out,
                    width = frame.width,
                    height = frame.height,
                    "screenshot written"
                );
            }
            Err(e) => tracing::error!("capture failed: {e}"),
        }
        tokio::time::sleep(interval).await;
    }
}

/// `--debug-draw`: paint the whole screen pink through the TGP wire protocol.
/// This exercises the same transmit+place path the streamer uses, but with a
/// single static pink image (no streaming loop) so it's a clean minimal render
/// test.
async fn run_debug_fill() -> Result<()> {
    use std::io::Write as _;

    // Query the real cell size before entering raw mode (we need line-read
    // access to stdin for the CSI 16 t reply).
    let (cell_w, cell_h) =
        query_cell_size().unwrap_or((8u32, 16u32)); // last-resort fallback for a debug helper.
    crossterm::terminal::enable_raw_mode()?;
    crossterm::execute!(std::io::stdout(), crossterm::terminal::EnterAlternateScreen)?;
    crossterm::execute!(std::io::stdout(), crossterm::cursor::Hide)?;

    let mut out = std::io::stdout();
    let (cols, rows) = crossterm::terminal::size()?;
    // Some PTYs report 0x0; fall back to a default so --debug still paints.
    let (cols, rows) = if cols == 0 || rows == 0 {
        (80, 24)
    } else {
        (cols, rows)
    };
    tracing::info!(cols, rows, "debug fill pink via TGP");

    // Size the source to the terminal's real pixel area so the placement is
    // crisp instead of the old hardcoded 1280x800 guess.
    let (w, h) = (cols as u32 * cell_w, rows as u32 * cell_h);
    let n = (w * h) as usize;
    let mut data = vec![0u8; n * 4];
    for px in data.chunks_mut(4) {
        px.copy_from_slice(&[255, 105, 180, 255]);
    }

    // Emit a transmit+place TGP image at the current cursor.
    let chunks = tgp::chunked_transmit(
        32,
        vec![
            ('a', "T".into()),
            ('C', "1".into()),
            ('i', "777".into()),
            ('s', w.to_string()),
            ('v', h.to_string()),
        ],
        &data,
        false,
    );
    for c in chunks {
        out.write_all(&c)?;
    }
    // T places at the cursor; also emit an explicit placement covering the
    // whole screen at the top-left for robustness.
    let place = tgp::command(
        &[
            ('a', "p".into()),
            ('i', "777".into()),
            ('c', "0".into()),
            ('q', "1".into()),
            ('x', "0".into()),
            ('y', "0".into()),
            ('z', "0".into()),
        ],
        &[],
    );
    out.write_all(&place)?;
    out.flush()?;

    // Hold a moment, then wait for a keypress to leave.
    std::thread::sleep(std::time::Duration::from_millis(2000));
    let _ = crossterm::event::read(); // swallow any pending key

    crossterm::execute!(std::io::stdout(), crossterm::cursor::Show)?;
    crossterm::execute!(std::io::stdout(), crossterm::terminal::LeaveAlternateScreen)?;
    crossterm::terminal::disable_raw_mode()?;
    Ok(())
}

/// `--debug-streaming`: stream solid-pink frames through the real TGP encoder
/// (transmit + place) into the TUI. Synthetic [`PinkFrameSource`], so nothing
/// depends on cage/Wayland/video — this isolates TGP placement itself.
async fn run_pink_streaming() -> Result<()> {
    let (w, h) = terminal_pixel_size()?;
    let src = PinkFrameSource::infinite(w, h);
    tracing::info!(width = w, height = h, "pink streaming frame source");

    let encoder = TgpEncoder::new(EncoderConfig::default());
    let mut tgp_stream = encoder.into_stream(src);

    let terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    let (tx, _rx) = mpsc::unbounded_channel::<InputMsg>();
    run_ui(
        terminal,
        &mut tgp_stream,
        "debug-streaming: pink",
        (0, 0),
        tx,
    )
    .await
}
