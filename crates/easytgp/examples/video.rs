//! Decode the capybara `.mp4` (from `CAPY_VID_PATH`, provisioned in
//! flake.nix's dev shell) frame by frame to RGB8, driving the `ffmpeg` CLI as
//! a sidecar process. ffmpeg is on `PATH` in the dev shell; the sidecar
//! streams raw video on stdout, so no pure-Rust decoder is involved.
//!
//! Usage:
//!   CAPY_VID_PATH=... cargo run -p easytgp --example video
//!   CAPY_VID_PATH=... cargo run -p easytgp --example video -- --shm
//!   CAPY_VID_PATH=... cargo run -p easytgp --example video -- --every 10
//!
//! # `--direct` (default) vs `--shm`
//!
//! The default is easytgp's normal path: PNG-encode the frame, base64 it, and
//! send every byte in the escape stream.
//!
//! `--shm` transmits through the protocol's shared memory medium (`t=s`). The
//! pixels are staged once in a POSIX shared memory object and the wire command
//! carries only the object's *name*, so no image bytes are base64-encoded and
//! no compression pass runs. Both paths go through the same `Controller`,
//! `transmit()` and `sync()`, so the two are directly comparable and both wait
//! for the terminal's acknowledgement.
//!
//! # What is measured
//!
//! Each frame is split into stages, so a slow run says which one is responsible
//! rather than only that it was slow:
//!
//! | stage | what it covers |
//! |---|---|
//! | `decode` | pulling the next sample from the mp4 and decoding it to RGB8 |
//! | `stage` | turning those pixels into the wire payload: PNG encode + base64, or writing the shared memory object |
//! | `submit` | handing the command to the terminal and waiting for its acknowledgement |
//! | `other` | bookkeeping: the `RgbImage` wrapper, the frame/placement calls, reporting |
//!
//! `submit` is where the transport shows up. It contains the terminal parsing the
//! escape, and on the direct path it also contains the terminal base64-decoding
//! and inflating the whole image — which is why the two media are worth
//! comparing at all. On the shared memory path the terminal instead maps the
//! object, so `submit` measures the round trip rather than the pixel volume.
//!
//! Reported alongside the timings: `fps`, and `MiB/s` of *decoded* pixels, which
//! is independent of how many bytes the medium actually puts on the wire.
//!
//! The first [`WARMUP`] frames are excluded — they cover starting the reply
//! router and the first transmit's round trip.

use easytgp::{Controller, DynImage};
use std::path::Path;
use std::time::{Duration, Instant};

/// Frames pulled from the ffmpeg sidecar's stdout, decoded to RGB8.
type FrameIter = Box<dyn Iterator<Item = ffmpeg_sidecar::event::OutputVideoFrame> + Send>;

/// Spawn `ffmpeg -i <path> -f rawvideo -pix_fmt rgb24 -` and hand back its
/// decoded frames. ffmpeg runs as a child process; the iterator owns the
/// stdout pipe it writes to.
fn spawn_raw(path: &Path) -> FrameIter {
    use ffmpeg_sidecar::command::FfmpegCommand;

    let mut cmd = FfmpegCommand::new();
    cmd.input(path.as_os_str());
    let frames = cmd
        .rawvideo()
        .spawn()
        .expect("spawn ffmpeg")
        .iter()
        .expect("attach ffmpeg iterator")
        .filter_frames();
    Box::new(frames)
}

/// Frames to run before reporting starts, so startup cost is excluded.
const WARMUP: u32 = 30;

/// Per-frame stage timings, summed over a reporting window.
#[derive(Default, Clone, Copy)]
struct Stage {
    /// Decoding the next frame out of the mp4.
    decode: Duration,
    /// Building the wire payload: PNG + base64, or staging the shm object.
    stage: Duration,
    /// Sending and waiting for the terminal's acknowledgement.
    submit: Duration,
    /// `RgbImage` wrapper plus the frame/placement calls, which belong to no
    /// other category.
    overhead: Duration,
    /// Whole frame, measured over every stage at once.
    total: Duration,
}

impl Stage {
    /// Everything not in a named stage: `overhead` plus whatever the clocks did
    /// not catch. The four printed figures are meant to sum to [`Stage::total`],
    /// so this is a remainder rather than a category.
    fn other(self) -> Duration {
        self.total
            .saturating_sub(self.decode + self.stage + self.submit)
    }

    fn add(self, other: Stage) -> Stage {
        Stage {
            decode: self.decode + other.decode,
            stage: self.stage + other.stage,
            submit: self.submit + other.submit,
            overhead: self.overhead + other.overhead,
            total: self.total + other.total,
        }
    }

    /// Mean of each stage over `frames`.
    fn per_frame(self, frames: u32) -> Stage {
        let n = u32::max(frames, 1);
        Stage {
            decode: self.decode / n,
            stage: self.stage / n,
            submit: self.submit / n,
            overhead: self.overhead / n,
            total: self.total / n,
        }
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// A percentage of `total`, guarding the zero-length window.
fn pct(part: Duration, total: Duration) -> f64 {
    100.0 * part.as_secs_f64() / total.as_secs_f64().max(f64::EPSILON)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let use_shm = args.iter().any(|a| a == "--shm");
    if args.iter().any(|a| a == "--direct") && use_shm {
        eprintln!("--shm and --direct are mutually exclusive");
        std::process::exit(2);
    }
    // Frames per printed line.
    let report_every: u32 = args
        .iter()
        .position(|a| a == "--every")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(WARMUP);
    let mode = if use_shm { "shm" } else { "direct" };

    let path = std::env::var("CAPY_VID_PATH")?;
    let mut frames_iter = spawn_raw(Path::new(&path));

    let c = Controller::init(std::io::stdin());

    // The two media produce different image types, so each keeps its own
    // animation chain. Everything else about the loop is identical.
    let mut direct: Option<DynImage<easytgp::Png>> = None;
    let mut shm: Option<DynImage<easytgp::Shm>> = None;

    let mut frames: u32 = 0;
    let mut window = Stage::default();
    let mut window_frames: u32 = 0;
    let mut overall = Stage::default();
    let mut overall_frames: u32 = 0;
    let mut reported_dims = false;
    // Pixel size of a frame, for the throughput figure.
    let mut dims = (0u32, 0u32);

    loop {
        // The whole frame is timed from here, decode included, so the stages
        // sum to the frame rather than to a fraction of it.
        let t_total = Instant::now();

        // Decoding is its own stage: it is the one part of the loop that does
        // not depend on the transport, so it must be separable from it.
        let t = Instant::now();
        let Some(frame) = frames_iter.next() else { break };
        let decode = t.elapsed();

        let (w, h) = (frame.width, frame.height);
        let mut stage = Stage {
            decode,
            ..Stage::default()
        };

        if !reported_dims {
            dims = (w, h);
            eprintln!(
                "frame {w}x{h}, {:.1} MiB/frame, mode {mode}, warmup {WARMUP}, report every {report_every}",
                (w * h * 3) as f64 / (1024.0 * 1024.0),
            );
            reported_dims = true;
        }

        // The decoder hands back an owned buffer, so `from_raw` adopts it and
        // the pixels are never copied.
        let t = Instant::now();
        let imgbuf = image::RgbImage::from_raw(w, h, frame.data).expect("frame dims match");
        stage.overhead += t.elapsed();

        if use_shm {
            let t = Instant::now();
            let imgreq = c.new_shm_rgb(imgbuf)?;
            stage.stage = t.elapsed();

            if let Some(dimg) = shm.as_mut() {
                // Wait for the previous frame's ack before replacing it, so the
                // terminal has read and unlinked the old object by now.
                let t = Instant::now();
                dimg.sync().await?;
                stage.submit = t.elapsed();

                let t = Instant::now();
                dimg.new_frame(easytgp::NewFrameRequest {
                    image: imgreq,
                    ..Default::default()
                });
                dimg.display_last_frame();
                stage.overhead += t.elapsed();
            } else {
                let t = Instant::now();
                let mut img = imgreq.transmit().await?;
                stage.submit = t.elapsed();

                let t = Instant::now();
                img.place(easytgp::PlacementRequest {
                    cols: Some(140),
                    fix_cursor: true,
                    ..Default::default()
                });
                stage.overhead += t.elapsed();
                shm = Some(img.into());
            }
        } else {
            let t = Instant::now();
            let imgreq = c.new_png(imgbuf)?;
            stage.stage = t.elapsed();

            if let Some(dimg) = direct.as_mut() {
                let t = Instant::now();
                dimg.sync().await?;
                stage.submit = t.elapsed();

                let t = Instant::now();
                dimg.new_frame(easytgp::NewFrameRequest {
                    image: imgreq,
                    ..Default::default()
                });
                dimg.display_last_frame();
                stage.overhead += t.elapsed();
            } else {
                let t = Instant::now();
                let mut img = imgreq.transmit().await?;
                stage.submit = t.elapsed();

                let t = Instant::now();
                img.place(easytgp::PlacementRequest {
                    cols: Some(40),
                    fix_cursor: true,
                    ..Default::default()
                });
                stage.overhead += t.elapsed();
                direct = Some(img.into());
            }
        }

        stage.total = t_total.elapsed();
        frames += 1;

        // Warmup frames are excluded from every figure.
        if frames <= WARMUP {
            continue;
        }
        window = window.add(stage);
        overall = overall.add(stage);
        window_frames += 1;
        overall_frames += 1;

        if window_frames % report_every == 0 {
            report_line(mode, &window, window_frames, frames, dims);
            window = Stage::default();
            window_frames = 0;
        }
    }

    if overall_frames > 0 {
        eprintln!();
        report_summary(mode, &overall, overall_frames, dims);
    }

    // Give the terminal a beat to consume the last frame's object before the
    // drop chain removes any name it did not take.
    std::thread::sleep(Duration::from_millis(1000));
    Ok(())
}

/// One line of the running average for the window just completed.
fn report_line(mode: &str, w: &Stage, n: u32, frames: u32, dims: (u32, u32)) {
    let per = w.per_frame(n);
    let fps = n as f64 / w.total.as_secs_f64().max(f64::EPSILON);
    let mib = (dims.0 as f64 * dims.1 as f64 * 3.0) / (1024.0 * 1024.0);
    // The stages cover the whole frame, so the sum should match `ms/frame`.
    // Printing it makes a drift in the instrumentation visible rather than
    // silently loading the difference onto `other`.
    let accounted = ms(per.decode) + ms(per.stage) + ms(per.submit) + ms(per.other());
    eprintln!(
        "{frames:>6} | {fps:>6.1} fps | {:>6.1} ms/frame | {:>6.1} MiB/s | decode {:>5.1} + stage {:>5.1} + submit {:>5.1} + other {:>5.1} = {accounted:>5.1} ({mode})",
        ms(per.total),
        fps * mib,
        ms(per.decode),
        ms(per.stage),
        ms(per.submit),
        ms(per.other()),
    );
}

/// The whole run, with each stage as a share of the total.
fn report_summary(mode: &str, overall: &Stage, frames: u32, dims: (u32, u32)) {
    let per = overall.per_frame(frames);
    let fps = frames as f64 / overall.total.as_secs_f64().max(f64::EPSILON);
    let mib = (dims.0 as f64 * dims.1 as f64 * 3.0) / (1024.0 * 1024.0);

    eprintln!("summary ({mode}), {frames} frames after warmup:");
    eprintln!(
        "  {:.1} fps, {:.1} ms/frame, {:.1} MiB/s of pixels",
        fps,
        ms(per.total),
        fps * mib
    );
    for (label, d) in [
        ("decode ", per.decode),
        ("stage  ", per.stage),
        ("submit ", per.submit),
        ("other  ", per.other()),
    ] {
        eprintln!(
            "  {label} {:>7.2} ms/frame   {:>5.1}%",
            ms(d),
            pct(d, per.total)
        );
    }
    let wire = if mode == "shm" {
        "the wire carried an object name, not pixels"
    } else {
        "the wire carried base64 PNG of every frame"
    };
    eprintln!("  ({wire})");
}
