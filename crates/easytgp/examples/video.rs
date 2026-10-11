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

use easytgp::{Controller, DynImage, DynImageHandle, ImageRequest};
use image::{ImageBuffer, ImageFormat, Rgb};
use std::{
    io::{stdin, Cursor},
    path::Path,
};
use yscv_video::Mp4VideoReader;

/// A single decoded frame: RGBA8, row-major, top-left origin.
struct RgbaFrame {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::var("CAPY_VID_PATH")?;

    let mut r = Mp4VideoReader::open(Path::new(&path))?;

    let mut dimg: Option<DynImage<_>> = None;
    while let Some(frame) = r.next_frame()? {
        // frame.rgb8_data
        let imgbuf = image::RgbImage::from_raw(
            frame.width as u32,
            frame.height as u32,
            frame.rgb8_data.clone(),
        )
        .unwrap();

        // buf.save_with_format("/tmp/test.png", image::ImageFormat::Png)
        //     .unwrap();

        // dbg!(png_bytes.len());

        let c = Controller::init(stdin());
        let imgreq = c.new_png(imgbuf).unwrap();

        if let Some(dimg) = dimg.as_mut() {
            dimg.new_frame(easytgp::NewFrameRequest {
                image: imgreq,
                ..Default::default()
            })
            .unwrap();
            dimg.display_last_frame();
        } else {
            let mut img = imgreq.transmit().await?;
            dbg!("img");
            img.place(easytgp::PlacementRequest {
                // image: Some(img.clone()),
                cols: Some(100),
                fix_cursor: true,
                // width: Some(dbg!(10 as u32)),
                // height: Some(dbg!(10 as u32)),
                // width: Some(dbg!(frame.width as u32)),
                // height: Some(dbg!(frame.height as u32)),
                // rows: Some(10),
                // width: Some(140),
                ..Default::default()
            })
            .await
            .unwrap();

            dimg = Some(img.into());
            // break;
        }
    }

    let mut img = dimg.unwrap();

    // img.display_frame(easytgp::FrameSelector::First);

    std::thread::sleep(std::time::Duration::from_millis(1000));

    // img.delete();
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
    //
    // 

    Ok(())
}

/** 

// Image is a future that can be awaited, but it also already avail, it's almost certainly safe to assume it's correct.
let img: DynImage = c.new()?.await;
let pl: PendingPlacement = img.place()?;


let f: PendingFrame = img.new_frame()?;
let e: PendingEdit = f.edit()?;


let mut img = c.new().transmit()?;

loop {
    match img.ready().await {
        Err::Evicted(restorable) => {restorable.restore() } // or custom logic for animations, or whatever you want
        Err::Unknown(e) => panic!(e)
    }

    img.place()?;

    ...
}

**/
