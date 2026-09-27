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
use tuigui_cage::{CageFrameSource, CageSession, CageSpec, CaptureConfig, HeadlessConfig};
use tuigui_streamer::{FrameSource, Mp4VideoSource, PinkFrameSource};
use tuigui_tgp::proto as tgp;
use tuigui_tgp::{EncoderConfig, EncoderEvent, EncoderError, Strategy, TgpEncoder};

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
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

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
    let source = CageFrameSource::connect(&CaptureConfig::new(session.wayland_socket().clone()))
        .context("connecting screencopy client")?;

    // 3. Encode to a TGP byte stream, scaled to fill the pane (in cells).
    let encoder = TgpEncoder::new(EncoderConfig {
        strategy: Strategy::DeltaFrames,
        placement_columns: pc,
        placement_rows: pr,
        ..EncoderConfig::default()
    });
    let mut tgp_stream = encoder.into_stream(source);

    // 4. Optionally dump raw TGP bytes to a file instead of the TUI, to
    //    inspect exactly what the capture+encoder produce.
    if let Some(out) = cli.cage_out {
        return drain_to_file(&mut tgp_stream, &out).await;
    }

    // 5. Run the ratatui shell.
    let terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    run_ui(terminal, &mut tgp_stream, &app_path, Some(session)).await
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

    let encoder = TgpEncoder::new(EncoderConfig {
        strategy: Strategy::FullRetransmit,
        ..EncoderConfig::default()
    });
    let stream = encoder.into_stream(src);
    drain_to_file(stream, out).await
}

/// Query the terminal's current size and report it in pixels, suitable for
/// sizing the cage virtual output. Reads the cell grid via crossterm and
/// multiplies by the cell size reported by the terminal if available.
fn terminal_pixel_size() -> Result<(u32, u32)> {
    let (cols, rows) = crossterm::terminal::size().context("querying terminal size")?;
    if cols == 0 || rows == 0 {
        return Ok((1280, 800)); // headless/unknown PTY; fall back to the default.
    }
    // Default to the classic 8x16 monospace cell. window_size() often reports
    // pixel dims of 0 on unix (unused), so only trust it when it gives a
    // nonzero pixel size consistent with the reported cell grid.
    let mut cell_w = 8u32;
    let mut cell_h = 16u32;
    if let Ok(ws) = crossterm::terminal::window_size() {
        if ws.width > 0 && ws.columns > 0 {
            cell_w = (ws.width as u32 / ws.columns as u32).max(1);
        }
        if ws.height > 0 && ws.rows > 0 {
            cell_h = (ws.height as u32 / ws.rows as u32).max(1);
        }
    }
    Ok((cols as u32 * cell_w, rows as u32 * cell_h))
}

/// The pane's inner area in terminal cells — the rectangle the TGP image is
/// placed into. Must match the layout in `ui::draw`: a 1-row header, a 1-row
/// footer, and a 1-cell border around the body.
fn pane_placement() -> Result<(u32, u32)> {
    let (cols, rows) = crossterm::terminal::size().context("querying terminal size")?;
    if cols <= 2 || rows <= 4 {
        return Ok((cols as u32, rows as u32));
    }
    Ok(((cols - 2) as u32, (rows - 4) as u32))
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

    let encoder = TgpEncoder::new(EncoderConfig {
        strategy: Strategy::FullRetransmit, // every frame is a fresh transmit+place
        ..EncoderConfig::default()
    });
    let mut tgp_stream = encoder.into_stream(src);

    let terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    run_ui(terminal, &mut tgp_stream, &format!("video: {path}"), None).await
}

async fn run_ui(
    mut terminal: Terminal<CrosstermBackend<std::io::Stdout>>,
    tgp_stream: &mut (impl Unpin + Stream<Item = Result<EncoderEvent, EncoderError>>),
    app_path: &str,
    session: Option<CageSession>,
) -> Result<()> {
    crossterm::terminal::enable_raw_mode()?;
    crossterm::execute!(std::io::stdout(), crossterm::terminal::EnterAlternateScreen)?;
    terminal.clear()?;

    let mut last_bytes: usize = 0;
    let mut last_event = "starting".to_string();
    let mut pane: Option<ratatui::layout::Rect> = None;
    let mut quit = false;

    while !quit {
        // Draw the shell; capture the pane's inner rect where the image lands.
        terminal.draw(|f| {
            pane = Some(ui::draw(f, app_path, &last_event, last_bytes));
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
        // Drain with a real await so the stream gets a proper waker. A plain
        // `futures::poll!` uses a noop waker: once the channel is momentarily
        // empty it returns Pending -> break and is never woken again, freezing
        // mid-frame. `timeout` relinquishes the loop to redraw + key polling
        // when no bytes arrive for ~4ms.
        loop {
            match tokio::time::timeout(std::time::Duration::from_millis(4), tgp_stream.next())
                .await
            {
                Ok(Some(Ok(EncoderEvent::Bytes(b)))) if !b.is_empty() => {
                    last_bytes = b.len();
                    written_chunks += 1;
                    written_bytes += b.len();
                    out.write_all(&b)?;
                }
                Ok(Some(Ok(EncoderEvent::Bytes(_)))) => {}
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
        out.flush()?;
        tracing::info!(
            pane = ?pane.map(|r| (r.x, r.y, r.width, r.height)),
            written_chunks,
            written_bytes,
            last_event = %last_event,
            "ui drain iteration"
        );

        // Input scaffold: 'q' quits; forwarding comes with input injection (M3).
        while crossterm::event::poll(std::time::Duration::from_millis(4))? {
            if let crossterm::event::Event::Key(k) = crossterm::event::read()? {
                if k.kind == crossterm::event::KeyEventKind::Press
                    && k.code == crossterm::event::KeyCode::Char('q')
                {
                    quit = true;
                }
            }
        }
    }

    crossterm::execute!(std::io::stdout(), crossterm::terminal::LeaveAlternateScreen)?;
    crossterm::terminal::disable_raw_mode()?;
    if let Some(s) = session {
        s.shutdown().await.ok();
    }
    Ok(())
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

    // Build a solid pink RGBA image. The headless pane is ~1280x800; TGP will
    // scale it into the placement region, so a generous source size fills the
    // visible screen crisply.
    let (w, h) = (1280u32, 800u32);
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

    let encoder = TgpEncoder::new(EncoderConfig {
        strategy: Strategy::DeltaFrames,
        ..EncoderConfig::default()
    });
    let mut tgp_stream = encoder.into_stream(src);

    let terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    run_ui(terminal, &mut tgp_stream, "debug-streaming: pink", None).await
}
