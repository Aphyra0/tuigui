//! `tuigui` — start the `tui` server and the `gui` client as one unit.

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Internal re-entry: this process *is* the client, started by the server
    // inside its pty. Checked first so the client's own arguments (a video path,
    // say) are never parsed as this command's.
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() == Some(tuigui::INTERNAL_CLIENT_FLAG) {
        let command = gui::command_from_args(args)?;
        return gui::run(command);
    }

    let config = tuigui::Config::from_args(std::env::args().skip(1))?;
    tuigui::run(config).await
}
