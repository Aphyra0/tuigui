use std::{str::FromStr, time::Duration};

use gobo_renderer::{Mp4Stream, State, TgpRendererWidget};
use ratatui::crossterm::event;

/// Prints the total wall-clock runtime on drop, so it fires on a normal return
/// and on a panic unwind.
struct Timer(std::time::Instant);

impl Drop for Timer {
    fn drop(&mut self) {
        println!("total runtime: {:.3}s", self.0.elapsed().as_secs_f64());
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _timer = Timer(std::time::Instant::now());
    gobo_renderer::redirect_stderr(std::path::Path::new("log.txt"))?;

    let mut state = State::new(Mp4Stream::from_path(
        std::path::PathBuf::from_str(std::env::var("VID_PATH").unwrap().as_str()).unwrap(),
    ));

    // Raw mode without the alternate screen: the alt screen has its own
    // graphics storage and can be torn down on resize, which strands the TGP
    // image.
    let mut terminal = ratatui::try_init_with_options(ratatui::TerminalOptions {
        viewport: ratatui::Viewport::Fullscreen,
    })?;

    let res = (|| {
        loop {
            terminal.draw(|f| {
                let area = f.area();
                f.render_stateful_widget(TgpRendererWidget, area, &mut state);
            })?;
        }
        #[allow(unreachable_code)]
        Ok::<(), Box<dyn std::error::Error>>(())
    })();

    ratatui::restore();
    res
}
