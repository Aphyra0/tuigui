//! Run a real client in the compositor and write what it drew as a PNG.
//!
//! This is the tool for looking at the capture rather than only measuring it: the
//! pixel a test reads is one number, and "is the window there at all" is a question
//! only the picture answers.
//!
//! ```sh
//! cargo run -p compositor --example comp-debug -- <app> [out.png] [--keys]
//! ```
//!
//! `app` is any Wayland client (`foot`, `weston-editor`, ...). The default output is
//! `comp-debug.png` in the working directory. The window size is fixed at 640x480,
//! which is what the app is configured with, so the picture is that output.
//!
//! `--keys` types into the app first and captures after, which is how input injection
//! is checked without a terminal: an app that draws something different once it has
//! focus proves the key arrived.

use std::time::Duration;

use compositor::{Compositor, Key};

/// The output size, which is also the client's window size (the MVP fixes both).
const SIZE: (u32, u32) = (640, 480);

/// How long to let the app paint before a capture.
///
/// A client commits its first buffer when it is ready and `spawn` waits for exactly
/// that, so this is only for the frames *after* the first: a toolkit that draws a
/// background and then its widgets needs a moment for the second.
const PAINT: Duration = Duration::from_millis(800);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let app = args
        .next()
        .ok_or("usage: comp-debug <app> [out.png] [--keys]")?;
    let out = args.next().unwrap_or_else(|| "comp-debug.png".to_string());
    let type_keys = args.any(|a| a == "--keys");

    let compositor = Compositor::init(SIZE, 1.0)?;
    eprintln!("socket: {:?}", compositor.wayland_socket());
    let client = compositor.spawn(&app, SIZE)?;
    eprintln!("spawned {app}");

    std::thread::sleep(PAINT);
    let frame = client.capture()?;
    eprintln!("captured {}x{}", frame.width, frame.height);

    // Every pixel the same is a window that painted one colour, which for a toolkit
    // usually means it drew nothing. Reported because a PNG of one rectangle looks the
    // same whether the capture failed or the app drew a blank window.
    let solid = frame.rgba.chunks_exact(4).all(|px| px == &frame.rgba[..4]);
    let first = &frame.rgba[..4];
    eprintln!(
        "window is {} (first pixel b,g,r,a = {first:?})",
        if solid { "ONE COLOUR" } else { "painted" }
    );

    // A coarse map of how bright each part of the window is, printed as text. A PNG can
    // only be judged by looking at it, and this makes the same judgement possible
    // without an image viewer: a window that drew something has shapes in it, and one
    // that drew nothing is a flat rectangle.
    print_preview(&frame);

    write_png(&out, &frame)?;
    eprintln!("wrote {out}");

    if type_keys {
        // The compositor's key set is named keys plus characters, and two letters are
        // about the shortest input a GUI app generally shows a result for.
        for key in [Key::Char('h'), Key::Char('i')] {
            client.type_key(key)?;
        }
        std::thread::sleep(PAINT);
        let after = client.capture()?;
        eprintln!("typing changed the window: {}", after.rgba != frame.rgba);
        let typed = out.replace(".png", "-typed.png");
        write_png(&typed, &after)?;
        eprintln!("wrote {typed}");
    }

    drop(client);
    compositor.shutdown();
    Ok(())
}

/// Print a coarse brightness map of the frame, so the capture can be judged as text.
///
/// The window is divided into a grid of cells and each cell is averaged. What this
/// shows is layout: where the window drew something and where it did not. It cannot
/// show text or colour, and it is not meant to — the PNG is for that. It exists so a
/// run can be checked without an image viewer.
fn print_preview(frame: &compositor::Frame) {
    /// Cells across and down. Small enough that the whole map fits on a terminal.
    const COLS: u32 = 64;
    const ROWS: u32 = 24;

    // ASCII by brightness, dark to light, so the shape of the window is what the eye
    // picks out.
    const RAMP: &[u8] = b" .:-=+*#%@";
    let (w, h) = (frame.width.max(1), frame.height.max(1));
    for row in 0..ROWS {
        let y0 = row * h / ROWS;
        let y1 = ((row + 1) * h / ROWS).max(y0 + 1);
        let mut line = String::with_capacity(COLS as usize);
        for col in 0..COLS {
            let x0 = col * w / COLS;
            let x1 = ((col + 1) * w / COLS).max(x0 + 1);
            let (mut sum, mut n) = (0u64, 0u64);
            for y in y0..y1.min(h) {
                for x in x0..x1.min(w) {
                    let o = ((y * w + x) * 4) as usize;
                    if o + 2 < frame.rgba.len() {
                        // B, G, R as captured; brightness does not care about the order.
                        let (b, g, r) = (
                            frame.rgba[o] as u64,
                            frame.rgba[o + 1] as u64,
                            frame.rgba[o + 2] as u64,
                        );
                        sum += (r * 299 + g * 587 + b * 114) / 1000;
                        n += 1;
                    }
                }
            }
            let level = if n == 0 { 0 } else { (sum / n) as usize };
            let idx = (level * (RAMP.len() - 1) / 255).min(RAMP.len() - 1);
            line.push(RAMP[idx] as char);
        }
        eprintln!("{line}");
    }
}

/// Write an RGBA8 frame as a PNG.
/// The bytes are written as they arrived, without a channel swap. The capture is an
/// `Argb8888` Wayland buffer copied byte for byte, whose bytes are B, G, R, A, so the
/// file's red and blue are swapped relative to the app. What the file is for is *where*
/// the window is and whether it drew, and neither depends on the channel order; a
/// colour-exact capture is the dma-buf/EGL work, not a rename.
fn write_png(path: &str, frame: &compositor::Frame) -> Result<(), Box<dyn std::error::Error>> {
    let file = std::fs::File::create(path)?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), frame.width, frame.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(&frame.rgba)?;
    writer.finish()?;
    Ok(())
}
