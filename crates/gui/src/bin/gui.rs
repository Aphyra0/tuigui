//! `gui` — send frames to a running `tui` server and draw the pane that shows
//! them. All the work lives in the `gui` library; this is only the command-line
//! wrapper, so the same client can also be started in-process by `tuigui`.

use clap::Parser as _;

use gui::Gui;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let gui = Gui::parse();
    gui.run(std::io::stdin(), std::io::stdout()).await?;
    Ok(())
}
