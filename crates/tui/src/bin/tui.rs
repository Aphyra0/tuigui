//! `tui` — run a command in a pty we own and forward its bytes to this terminal.
//!
//! All the work lives in the `tui` library; this is only the command-line
//! wrapper, so the same session can also be started in-process by `tuigui`.

use clap::Parser as _;

use tui::Tui;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // `parse`, not `try_parse_from`: clap needs `argv[0]` to name itself in a
    // usage message, and skipping it made `--help` come out as the program name.
    let session = Tui::parse();
    session.run(std::io::stdin(), std::io::stdout()).await?;
    Ok(())
}
