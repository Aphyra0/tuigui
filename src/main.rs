//! tuigui: run a GUI app in a headless cage session and stream it into this
//! terminal via TGP.
//!
//! Final shape: `tui ./path/to/binary`.

mod ui;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use futures::{Stream, StreamExt};
use ratatui::{backend::CrosstermBackend, Terminal};
use tokio::sync::mpsc;
use tuigui_cage::{
    CageFrameSource, CageSession, CageSpec, CaptureConfig, HeadlessConfig, InputSink, InputSock,
    KeyEvent, KeyState, PointerEvent,
};
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

    /// Scale down the headless output resolution (1.0–4.0). The terminal's
    /// pixel size is divided by this to lower the capture resolution, shrinking
    /// per-frame bytes. 1.0 keeps the full resolution.
    #[arg(long, default_value_t = 1.0)]
    scale: f32,

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

    /// Stream as NxN-pixel micro-blocks instead of one full-frame image.
    /// `0` (default) disables block streaming. When set, the encoder diffs each
    /// block against the previous frame and retransmits only the changed ones.
    #[arg(long, value_name = "N", default_value_t = 0)]
    blocks: u32,

    /// With --blocks, the number of resolution levels on each block's ladder
    /// (default 4). A changed block repaints at the coarsest level and sharpens
    /// one rung per static frame up to the block's ceiling; 1 disables the
    /// ladder (always full res).
    #[arg(long, value_name = "LEVELS", default_value_t = 0)]
    res_levels: u32,

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

    /// Debuggy block grid: every other block (linear odd index) is filled with
    /// a per-block pseudo-random color that changes each frame; even blocks
    /// carry the real pixels. Visually proves the block grid and per-block
    /// diffing. Implies `--blocks 4` if not already set.
    #[arg(long, default_value_t = false)]
    debug_blocks: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
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
        if let Some(out) = cli.out {
            return run_video_to_file(&path, &out, cli.loop_video).await;
        }
        return run_video_tui(&path).await;
    }

    let app_path = cli.app.clone().unwrap();

    let spec = CageSpec::new(&app_path);
    // Size the virtual display to the terminal's drawing area so the app's
    // pixels map ~1:1 to on-screen cells; avoids resampling the capture.
    let (tcols, trows) = terminal_pixel_size()?;
    let scale = cli.scale.clamp(1.0, 4.0);
    let (pc, pr) = pane_placement()?;
    let cfg = HeadlessConfig {
        width: tcols,
        height: trows,
        ..HeadlessConfig::default()
    };

    // 1. Launch the caged session.
    let session = CageSession::spawn(spec, cfg)
        .await
        .context("spawning headless cage session")?;
    tracing::info!(socket = %session.wayland_socket().display(), "session up");

    // 2. Attach capture. The capture feed doubles as a tuigui-streamer
    //    FrameSource; the TGP encoder is codec-agnostic and only sees frames.
    //    `scale` shrinks each captured frame so the encoder sees fewer pixels.
    let capture = CaptureConfig::new(session.wayland_socket().clone())
        .max_fps(cli.max_fps)
        .scale(scale);
    let source = CageFrameSource::connect(&capture).context("connecting screencopy client")?;

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
        res_levels: cli.res_levels,
        detail,
        debug_lod: cli.debug_lod,
        debug_blocks: cli.debug_blocks,
        pane_origin: Some((0, 0)),
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
    // blocking), and a dedicated writer task drains them into the Wayland
    // virtual pointer. This fully decouples input I/O (which can block on a
    // full socket) from the render loop, so flooding mouse events can never
    // stall the stream or the quit key.
    let (tx, mut rx) = mpsc::unbounded_channel::<InputMsg>();
    let sock_path = session.wayland_socket().clone();
    let (tw, th) = (tcols, trows);
    let writer = tokio::spawn(async move {
        // Connect with a timeout so a slow/refusing server just disables input.
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), async move {
            let mut ins = InputSock::connect(&sock_path)?;
            ins.set_frame_size(tw, th);
            Ok::<_, anyhow::Error>(ins)
        })
        .await;
        let mut sink: Option<Box<dyn InputSink>> = match result {
            Ok(Ok(ins)) => Some(Box::new(ins)),
            Ok(Err(e)) => {
                tracing::warn!(err = %e, "input injection unavailable");
                None
            }
            Err(_) => {
                tracing::warn!("input connect timed out; input disabled");
                None
            }
        };
        // Drain queued events into the sink until the channel closes (render
        // loop exit). Blocking sends live here, off the render loop.
        while let Some(msg) = rx.recv().await {
            if let Some(s) = sink.as_mut() {
                match msg {
                    InputMsg::Key(kev) => {
                        let _ = s.send_key(kev).await;
                    }
                    InputMsg::Pointer(pev) => {
                        let _ = s.send_pointer(pev).await;
                    }
                }
            }
        }
    });

    let res = run_ui(
        terminal,
        &mut tgp_stream,
        &app_path,
        Some(session),
        (tcols, trows),
        tx,
    )
    .await;
    // The writer task holds a clone of runtime resources (its rx channel). If
    // it is parked inside a blocking Wayland flush it won't observe tx closing
    // via rx.recv() returning None, so it would keep the tokio runtime alive
    // and the process would hang after `q`. Abort it now that the UI loop is
    // done.
    writer.abort();
    res
}

/// Stream a video file's decoded frames through the TGP encoder into a file.
async fn run_video_to_file(path: &str, out: &str, loop_forever: bool) -> Result<()> {
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

    let encoder = TgpEncoder::new(EncoderConfig::default());
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
/// is placed into. Must match the layout in `ui::draw`: no header, two status
/// footers, and no border around the body.
fn pane_placement() -> Result<(u32, u32)> {
    let (cols, rows) = crossterm::terminal::size().context("querying terminal size")?;
    if rows <= 2 {
        return Ok((cols as u32, rows as u32));
    }
    Ok((cols as u32, (rows - 2) as u32))
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
async fn run_video_tui(path: &str) -> Result<()> {
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

    let encoder = TgpEncoder::new(EncoderConfig::default());
    let mut tgp_stream = encoder.into_stream(src);

    let terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    // No Wayland session here; forward inputs into a discard sink.
    let (tx, _rx) = mpsc::unbounded_channel::<InputMsg>();
    run_ui(
        terminal,
        &mut tgp_stream,
        &format!("video: {path}"),
        None,
        (0, 0),
        tx,
    )
    .await
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
    session: Option<CageSession>,
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
    let mut flush_ms_accum = 0f64;
    let mut frame_count_accum = 0usize;
    let mut announce_accum = 0f64;
    let mut setup_accum = 0f64;
    let mut copy_accum = 0f64;
    let mut readout_accum = 0f64;

    while !quit {
        // Draw the shell; capture the pane's inner rect where the image lands.
        terminal.draw(|f| {
            pane = Some(ui::draw(f, app_path, &last_event, &stats));
        })?;

        // Drain encoder output without blocking the UI loop. The whole batch
        // this iteration belongs to one pane region, so park the cursor at the
        // pane origin once, then stream every drained chunk before a single
        // flush. The terminal rasterizes these bytes into the live-capture
        // region; `C=1` on the transmit command keeps the cursor put, so each
        // chunk lands exactly at the pane's top-left cell.
        use std::io::Write as _;
        let mut out = std::io::stdout();
        if let Some(rect) = pane {
            crossterm::execute!(out, crossterm::cursor::MoveTo(rect.x, rect.y))?;
        }
        let mut written_chunks = 0usize;
        let mut written_bytes = 0usize;
        // Time the buffered writes (bytes into userspace) separately from the
        // flush that pushes them to the terminal. `stdout` is a LineWriter, so
        // `write_all` only memcpys into a buffer; the kernel/PTY handoff happens
        // on `flush`. Reporting them apart shows where the sink cost actually
        // lands.
        let write_t0 = std::time::Instant::now();
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
                    out.write_all(&b)?;
                }
                // Empty batch means nothing more to drain right now; stop and
                // return control to the UI loop so keys/mouse stay responsive
                // even when the encoder idles between frames. Spinning on
                // empty chunks would busy-loop and starve input polling.
                Ok(Some(Ok(EncoderEvent::Bytes(_)))) => break,
                Ok(Some(Ok(EncoderEvent::Frame { width, height, capture_ms, pacing_ms, encode_ms, timing }))) => {
                    // Record the frame's resolution and capture cost into the
                    // rolling window.
                    frame_count_accum += 1;
                    cap_ms_accum += capture_ms;
                    pacing_ms_accum += pacing_ms;
                    encode_ms_accum += encode_ms;
                    stats.resolution = (width, height);
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
        let write_ms = write_t0.elapsed().as_secs_f64() * 1000.0;
        let flush_t0 = std::time::Instant::now();
        out.flush()?;
        let flush_ms = flush_t0.elapsed().as_secs_f64() * 1000.0;
        // Accrue both the buffered writes and the flush into the per-second
        // window so `Sink:` and `Flush:` show separately.
        write_ms_accum += write_ms;
        flush_ms_accum += flush_ms;

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
            stats.flush_ms = flush_ms_accum / n;
            stats.announce_ms = announce_accum / n;
            stats.setup_ms = setup_accum / n;
            stats.copy_ms = copy_accum / n;
            stats.readout_ms = readout_accum / n;
            t_prev = now;
            frame_count_accum = 0;
            frame_bytes_accum = 0;
            cap_ms_accum = 0.0;
            pacing_ms_accum = 0.0;
            encode_ms_accum = 0.0;
            write_ms_accum = 0.0;
            flush_ms_accum = 0.0;
            announce_accum = 0.0;
            setup_accum = 0.0;
            copy_accum = 0.0;
            readout_accum = 0.0;
        }
        tracing::info!(
            pane = ?pane.map(|r| (r.x, r.y, r.width, r.height)),
            written_chunks,
            written_bytes,
            last_event = %last_event,
            "ui drain iteration"
        );

        // Forward input: keys and mouse to the session's input sink (if any).
        // The pane is the live-capture region; convert cursor cell coords to
        // the virtual frame's pixels before injecting. The sink appears on a
        // background task once its Wayland connection is ready; the render
        // loop only ever does a non-blocking `tx.send`, so flooding mouse
        // events can never stall the stream. Events are also capped per
        // iteration so `q` and the render loop always get a slice of the loop,
        // even under continuous mouse motion.
        while crossterm::event::poll(std::time::Duration::from_millis(4))? {
            let ev = crossterm::event::read()?;
            match ev {
                crossterm::event::Event::Key(k) => {
                    if k.kind == crossterm::event::KeyEventKind::Press
                        && k.code == crossterm::event::KeyCode::Char('q')
                    {
                        quit = true;
                    }
                    if let Some(kev) = crossterm_key_to_key(k) {
                        let _ = tx.send(InputMsg::Key(kev));
                    }
                }
                crossterm::event::Event::Mouse(m) => {
                    for pev in mouse_to_pointer(m, pane, output_px) {
                        let _ = tx.send(InputMsg::Pointer(pev));
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
    if let Some(s) = session {
        s.shutdown().await.ok();
    }
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

/// `--debug-capture`: spawn the caged session exactly like the real path, but
/// instead of streaming the frames to the TGP encoder, write a live screenshot
/// of the app to a PNG file every `interval` seconds (overwriting the previous
/// one). Use it to inspect what the headless app actually renders.
async fn run_capture_png(cli: &Cli) -> Result<()> {
    use tuigui_cage::{CaptureConfig, PageSource};

    let app_path = cli.app.clone().unwrap();
    let spec = CageSpec::new(&app_path);
    let (tcols, trows) = terminal_pixel_size()?;
    let cfg = HeadlessConfig {
        width: tcols,
        height: trows,
        ..HeadlessConfig::default()
    };

    let session = CageSession::spawn(spec, cfg)
        .await
        .context("spawning headless cage session")?;
    tracing::info!(socket = %session.wayland_socket().display(), "session up");

    let mut source = PageSource::connect(&CaptureConfig::new(session.wayland_socket().clone()))
        .context("connecting screencopy client")?;
    let interval = std::time::Duration::from_secs_f64(cli.capture_interval.max(0.05));

    tracing::info!(
        out = %cli.capture_out,
        ?interval,
        "debug-capture: overwriting PNG every interval"
    );
    loop {
        match source.capture_once() {
            Ok(shot) => {
                shot.write_png(&cli.capture_out)?;
                tracing::info!(
                    out = %cli.capture_out,
                    width = shot.width,
                    height = shot.height,
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
        None,
        (0, 0),
        tx,
    )
    .await
}
