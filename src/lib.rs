//! `tuigui` — run a runnable here, locally, in one command.
//!
//! It starts the server (a pty plus the TGP plumbing that owns this terminal),
//! then starts the client inside it, which does the drawing, and stops when either
//! ends. Nothing about the protocol changes: the two halves meet only through the
//! wire. This module exists to make the common case a single command instead of
//! two terminals.
//!
//! Both halves being started by one command does not make them one process: the
//! client runs *inside* the server's pty, and finds the server the same way any
//! other client does — by announcing a channel in its own output. There is nothing
//! to pass between them out of band, which is why this file has no path, no socket
//! name, and no state to share.
//!
//! Their command-line options are composed rather than duplicated. Each half declares
//! its own [`tui::DebugOptions`] and [`gui::DebugOptions`], and those are flattened
//! into [`TuiGui`], so a switch defined on either side is accepted here without being
//! re-declared — and a switch added to one side cannot fail to appear.

use std::path::{Path, PathBuf};

/// Everything `tuigui` decides, assembled from the two halves it starts.
///
/// The switches are the two halves' own, flattened in: what the server can change and
/// what the client can change are different sets, and neither is copied here. The only
/// thing written out by hand is which of the client's switches have to be turned back
/// into arguments, because the client is a separate process and its options arrive as a
/// command line rather than as a struct.
#[derive(Debug, Clone, clap::Parser)]
#[command(
    name = "tuigui",
    about = "Run the tui server and its gui client as one command"
)]
pub struct TuiGui {
    /// The runnable to draw, which is what produces the frames.
    pub runnable: PathBuf,
    /// The server's switches, used by the server this command starts.
    #[command(flatten)]
    pub tui: tui::DebugOptions,
    /// The codec the session streams with, chosen here and announced on the wire.
    #[command(flatten)]
    pub streaming: gui::StreamingOptions,
    /// The client's switches, passed on to the client this command starts.
    #[command(flatten)]
    pub gui: gui::DebugOptions,
}

impl TuiGui {
    /// The client's switches, as the arguments it will be started with.
    ///
    /// Spelled out field by field, because what the client receives is a command line
    /// rather than a struct. This is the one place that has to know a flag's spelling,
    /// and it sits next to the code that passes it on.
    fn client_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        // The codec is a choice the client process is told about, because the
        // announcement on the wire is its to make. Neither flag means the
        // client's own default, which is JPEG.
        if self.streaming.h264 {
            args.push("--h264".into());
        }
        if self.streaming.jpeg {
            args.push("--jpeg".into());
        }
        if self.gui.loop_video {
            args.push("--dbg-loop-video".into());
        }
        if self.gui.dummy_windowing {
            args.push("--dbg-dummy-windowing".into());
        }
        #[cfg(feature = "tracy")]
        if self.gui.trace_gui {
            args.push("--dbg-trace-gui".into());
        }
        args
    }

    /// Start the session: the server owns this terminal, and the command inside it is
    /// this same binary re-entered as the client.
    ///
    /// The client has to be *inside* the pty, not beside the server on the same
    /// terminal. The server owns the real terminal's input and output, and its rewrite
    /// of what the pane draws only sees what comes out of the pty: a client drawing
    /// directly would have its bytes delivered unrewritten, so the terminal would show
    /// the raw text and never an image. Piping the client through the pty also makes
    /// the server the only reader of fd 0, so nothing steals a keystroke.
    pub async fn run(&self) -> Result<(), Box<dyn std::error::Error>> {
        // One profiler per session, and one half to look at. The two flags name
        // the same thing from either end — which half is being profiled — so
        // both at once is a contradiction rather than a request for more.
        //
        // Checked here rather than with clap's `conflicts_with`, which names the
        // other argument and panics on a command that has no such argument: `tui`
        // and `gui` each see only their own flag. Only this command sees both.
        #[cfg(feature = "tracy")]
        if self.tui.trace_tui && self.gui.trace_gui {
            return Err("--dbg-trace-tui and --dbg-trace-gui name the same choice; pass one"
                .into());
        }
        let session = tui::Tui {
            command: server_command(&self.runnable, &self.client_args()),
            debug: self.tui.clone(),
        };
        // Nothing is drawn from this thread: the client runs inside the pty, which is
        // what puts its drawing through the rewrite. This only waits for the session to
        // end, which happens when the client inside it exits.
        //
        // `std::io`'s handles, not tokio's: the session takes plain streams and puts the
        // blocking reads on their own thread, so there is nothing async about them to
        // wrap.
        Ok(session.run(std::io::stdin(), std::io::stdout()).await?)
    }
}

/// The command the server runs: this same binary, re-entered as the client for
/// `runnable`.
///
/// The client's own flags go after the re-entry flag and before the runnable, which is
/// where `gui` expects them. Position does not actually matter to clap, but putting them
/// here keeps the runnable last, so the shape of the command reads the same however many
/// flags are added.
fn server_command(runnable: &Path, client_args: &[String]) -> Vec<String> {
    let mut cmd = vec![std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "tuigui".into())];
    cmd.push(INTERNAL_CLIENT_FLAG.into());
    cmd.extend_from_slice(client_args);
    cmd.push(runnable.to_string_lossy().into_owned());
    cmd
}

/// The flag `tuigui` uses to re-enter itself as the pty's client.
///
/// A flag rather than an environment variable: the pty's command line is what the
/// server execs, and an environment marker would be inherited by anything the
/// client itself spawns, making the re-entry ambiguous.
pub const INTERNAL_CLIENT_FLAG: &str = "--client";
