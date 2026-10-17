//! `tui` — run a command in a pty we own and let clients paint frames onto the
//! real terminal. All the work lives in the `tuigui-tui` library; this is only
//! the command-line wrapper, so the same server can also be started in-process
//! by `tuigui`.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = match tuigui_tui::Config::from_args(std::env::args().skip(1)) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    tuigui_tui::run_blocking(config)
}
