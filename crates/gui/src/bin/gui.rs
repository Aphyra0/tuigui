//! `gui` — send frames to a running `tui` server and draw the placeholder grid
//! that resolves them. All the work lives in the `gui` library; this is only the
//! command-line wrapper, so the same client can also be started in-process by
//! `tuigui`.

use gui::{command_from_args, run};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let command = match command_from_args(std::env::args().skip(1)) {
        Ok(command) => command,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    run(command)
}
