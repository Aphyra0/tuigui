//! `tuigui` — start the `tui` server and the `gui` client as one unit.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = match tuigui::Config::from_args(std::env::args().skip(1)) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    tuigui::run(config)
}
