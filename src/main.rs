//! `tuigui` — start the `tui` server and the `gui` client as one unit.

use clap::Parser as _;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Internal re-entry: this process *is* the client, started by the server
    // inside its pty. Checked first so the runnable's own path is never parsed as
    // this command's.
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() == Some(tuigui::INTERNAL_CLIENT_FLAG) {
        let gui = gui::Gui::parse_from(args);
        gui.run(std::io::stdin(), std::io::stdout()).await?;
        return Ok(());
    }

    let config = tuigui::Config::from_args(std::env::args().skip(1))?;
    tuigui::run(config).await
}
