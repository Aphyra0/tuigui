use tuigui_cage::CaptureConfig;
use tuigui_streamer::{FrameSource, FrameUpdate};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let sock = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/tuigui-e2e-runtime/wayland-0".into());
    let out = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "/tmp/shot.png".into());
    let cfg = CaptureConfig::new(&sock);
    let mut src = tuigui_cage::CageFrameSource::connect(&cfg)?;
    if let Some(FrameUpdate::Frame(f)) = src.next().await? {
        // correctness checks on the captured pixels
        let mut nonzero = 0u64;
        let data = &f.data;
        let step = 4;
        for px in data.chunks(step) {
            if px[0] != 0 || px[1] != 0 || px[2] != 0 {
                nonzero += 1;
            }
        }
        // count distinct-ish blocks: sample 1/1024 px
        let mut prev: (u8, u8, u8) = (0, 0, 0);
        let mut changed = 0u64;
        for px in data.chunks(step).step_by(1024) {
            let cur = (px[0], px[1], px[2]);
            if cur != prev {
                changed += 1;
                prev = cur;
            }
        }
        // write PNG
        let mut outf = std::fs::File::create(&out)?;
        let mut enc = png::Encoder::new(&mut outf, f.metadata.width, f.metadata.height);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut w = enc.write_header()?;
        w.write_image_data(&data[..(f.metadata.width as usize * f.metadata.height as usize * 4)])?;
        drop(w);
        println!(
            "wrote {} {}x{} nonzero_px={} distinct_samples={} bytes={} avgA={}",
            out,
            f.metadata.width,
            f.metadata.height,
            nonzero,
            changed,
            data.len(),
            data.iter()
                .skip(3)
                .step_by(4)
                .fold(0u64, |a, b| a + *b as u64)
                / ((data.len() / 4).max(1) as u64)
        );
    }
    Ok(())
}
