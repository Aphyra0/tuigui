use tuigui_cage::CaptureConfig;
use tuigui_streamer::{FrameSource, FrameUpdate};
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cfg = CaptureConfig::new("/tmp/tuigui-e2e-runtime/wayland-0");
    let mut src = tuigui_cage::CageFrameSource::connect(&cfg)?;
    if let Some(FrameUpdate::Frame(f)) = src.next().await? {
        let w = f.metadata.width as usize;
        let h = f.metadata.height as usize;
        let stride = w * 4;
        // corner avg and center avg
        let av = |x0: usize, y0: usize, x1: usize, y1: usize| -> (u8, u8, u8, u8) {
            let mut r = 0u64;
            let mut g = 0u64;
            let mut b = 0u64;
            let mut a = 0u64;
            let mut n = 0u64;
            for y in y0..y1 {
                for x in x0..x1 {
                    let p = y * stride + x * 4;
                    r += f.data[p] as u64;
                    g += f.data[p + 1] as u64;
                    b += f.data[p + 2] as u64;
                    a += f.data[p + 3] as u64;
                    n += 1;
                }
            }
            ((r / n) as u8, (g / n) as u8, (b / n) as u8, (a / n) as u8)
        };
        println!(
            "corner tl {:?}  center {:?}",
            av(0, 0, 40, 40),
            av(w / 2 - 20, h / 2 - 20, w / 2 + 20, h / 2 + 20)
        );
    }
    Ok(())
}
