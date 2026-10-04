//! Decode the capybara `.mp4` (from `CAPY_VID_PATH`, provisioned in
//! flake.nix's dev shell) frame by frame to RGBA, using `yscv-video` — a
//! pure-Rust H.264/HEVC decoder with MP4 parsing. No FFmpeg, no system libs.
//!
//! Usage:
//!   cargo run -p gobo-tgp --example video
//!
//! Each decoded frame is `rgb8_data` (RGB8, row-major); we convert it to RGBA
//! so the caller can hand it straight to the TGP encoder. The `for` loop over
//! `reader` yields every frame in order.

use easytgp::{Animation, NewPlacement};
use image::{ImageBuffer, ImageFormat, Rgb};
use std::{io::Cursor, path::Path};
use yscv_video::Mp4VideoReader;

/// A single decoded frame: RGBA8, row-major, top-left origin.
struct RgbaFrame {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::var("CAPY_VID_PATH")?;

    let mut r = Mp4VideoReader::open(Path::new(&path))?;

    let mut animation: Option<Animation> = None;
    while let Some(frame) = r.next_frame()? {
        // frame.rgb8_data
        let buf =
            image::RgbImage::from_raw(frame.width as u32, frame.height as u32, frame.rgb8_data)
                .unwrap();

        // buf.save_with_format("/tmp/test.png", image::ImageFormat::Png)
        //     .unwrap();

        let mut png_bytes: Vec<u8> = Vec::new();
        buf.write_to(&mut Cursor::new(&mut png_bytes), ImageFormat::Png)?;

        let nimg = easytgp::NewImage::new(easytgp::Format::PNG, png_bytes);

        if let Some(anim) = animation.as_mut() {
            anim.new_frame(easytgp::NewFrame {
                base_frame: easytgp::FrameSelector::Last,
                image: nimg,
                width: Some(1),
                height: Some(5),
                x: Some(1000),
                y: Some(600),
            });

            anim.display_last();
        } else {
            let img = nimg.transmit();

            img.place_named(NewPlacement {
                cols: Some(70),
                fix_cursor: Some(true),
                // rows: Some(10),
                // width: Some(140),
                ..Default::default()
            });
            animation = Some(img.into());
        }
    }
    std::thread::sleep(std::time::Duration::from_millis(1000));
    // easytgp::NewImage::new(easytgp::Format::PNG, png_bytes)
    //     .transmit()
    //     .place_named(NewPlacement {
    //         cols: Some(100),
    //         fix_cursor: Some(true),
    //         // rows: Some(10),
    //         // width: Some(140),
    //         ..Default::default()
    //     });
    // let buf = image::RgbImage::from_pixel(100, 100, Rgb([32, 250, 0]));
    // let mut png_bytes: Vec<u8> = Vec::new();
    // buf.write_to(&mut Cursor::new(&mut png_bytes), ImageFormat::Png)?;

    // let mut img = easytgp::NewImage::new(easytgp::Format::PNG, png_bytes);

    // img.transmit().place_named(NewPlacement {
    //     cols: Some(10),
    //     rows: Some(10),
    //     // width: Some(140),
    //     ..Default::default()
    // });

    // img.transplace(NewPlacement {
    //     width: Some(140),
    //     ..Default::default()
    // });
    // let img = TermImage::new(easytgp::Format::PNG, png_bytes);
    // img.display();

    Ok(())
}
