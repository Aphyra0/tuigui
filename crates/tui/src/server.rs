//! The image server: a client tells it where it is, and it dials.
//!
//! The server does **not** listen. The client owns the channel and announces it; this side's
//! only job is to notice the announcement and dial back.
//!
//! ```text
//!   pty output ──▶ scan ──▶ a transport ──▶ connect ──▶ Ping/Pong ──▶ frames
//! ```
//!
//! # One loop, two things to wait on
//!
//! The pty's output, where a client announces itself, and the clients already attached,
//! whose messages arrive whenever they do. Neither can wait on the other, so they are one
//! `select!`.
//!
//! The connect is in the arm that found the announcement, because that is where the
//! transport is: it was just built from the text a client wrote, and it is the only thing
//! that knows how to reach it. [`Transport::connect`] is what dials *and* greets, so what
//! comes back is a peer that has answered rather than a socket that opened.

use std::io;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use easytgp::Controller;
use tokio::sync::mpsc::Receiver;

use wire::message::Message;
use wire::transport::Transport;

use crate::announce::Announcement;
use crate::session::Session;

/// What the pty produces, and what the scan finds in it.
///
/// The two together because they are one thing from the server's side: the scanner stepts
/// over chunks, and a chunk is what `next` waits for. Splitting them would be a caller that
/// has to remember which of the two it is holding.
pub struct Announcements {
    /// The pty's output, read here so the loop that scans is the loop that waits.
    output: Receiver<Vec<u8>>,
    /// The scan of it.
    scanner: Announcement,
}

impl Announcements {
    pub fn new(output: Receiver<Vec<u8>>) -> Self {
        Self {
            output,
            scanner: Announcement::new(),
        }
    }

    /// Wait for the next client to announce itself.
    ///
    /// A transport, not a chunk: the chunks are how a client is found and are nobody else's
    /// business, so what comes out of here is the thing that knows how to reach one. Chunks
    /// that announce nobody are looped over and never seen — they are the command's own
    /// output, and there are a great many of them.
    ///
    /// `None` means the pty is gone, which is the end of the session rather than a failure.
    pub async fn next(&mut self) -> Option<Box<dyn Transport>> {
        loop {
            let chunk = self.output.recv().await?;
            if let Some(transport) = self.scanner.feed(&chunk).into_iter().next() {
                return Some(transport);
            }
        }
    }
}

/// One client the server has attached.
///
/// A [`Connection`] is one link; a `Session` is a client the server has attached, which is a
/// link and what has been learned about the far end — see [`crate::session`]. Every session is
/// established: the handshake happens inside [`Transport::connect`], so a connection only
/// exists once a peer has answered the greeting, and there is no half-attached client to be
/// told apart from a whole one.
pub struct Server {
    /// What the pty has produced, and where a client announces itself.
    announcements: Announcements,
    /// The grid a client's pty was created with, in cells.
    ///
    /// Handed to each session, because that is the pane a client draws into and the one piece
    /// of geometry the server has that a client does not: a pane's size is not passed down
    /// through its environment.
    cols: u32,
    rows: u32,
    /// Creates image requests, and owns the reply router behind them.
    ///
    /// On the server and not in a session because there is one terminal: the reply
    /// router reads the one keyboard descriptor and the image namespace is the
    /// terminal's, so both are per process and both outlive any client. It also
    /// owns where escapes are written, which is the screen this server shares with
    /// the pty's proxy.
    controller: Arc<Controller>,
    /// Every client attached to this session.
    ///
    /// Owned by the loop and nowhere else: reading a link needs it, so a link in a map
    /// beside the loop reading it would be a link in two places.
    sessions: Vec<Session>,
}

impl Server {
    /// Serve a pty's output, with the reply router this side's own escapes need.
    pub fn new(
        output: Receiver<Vec<u8>>,
        cols: u32,
        rows: u32,
        controller: Arc<Controller>,
    ) -> Self {
        Self {
            announcements: Announcements::new(output),
            cols,
            rows,
            controller,
            sessions: Vec::new(),
        }
    }

    /// How many clients are attached.
    pub fn clients(&self) -> usize {
        self.sessions.len()
    }

    /// Serve until the pty's output ends.
    pub async fn run(self) -> io::Result<()> {
        let Server {
            mut announcements,
            cols,
            rows,
            controller,
            mut sessions,
        } = self;

        loop {
            // What to do, decided here and acted on below. The block is load-bearing: the reads
            // borrow `sessions`, and the match under this needs it mutable. Scoping the reads
            // to this block is what ends one borrow before the other begins.
            //
            // Rebuilding the reads each pass costs nothing, and dropping the ones that did not
            // finish loses nothing: every read here is cancel-safe, so a socket keeps whatever
            // it has and the next pass finds it.
            // What to do, decided here and acted on below. The scoping is load-bearing: the
            // reads borrow `sessions` immutably, and the match under this needs it mutable, so
            // one borrow has to end before the other starts. Building the reads in a branch
            // rather than in a helper is what keeps that borrow inside it.
            let action = if sessions.is_empty() {
                // Nothing to read, so there is nothing to race: waiting on the announcement
                // alone is the same thing without an empty `select_all`, which asserts.
                Action::Announced(announcements.next().await)
            } else {
                let reads: Vec<_> = sessions
                    .iter()
                    .map(|session| {
                        Box::pin(session.connection.recv())
                            as Pin<Box<dyn Future<Output = io::Result<Option<Message>>> + Send + '_>>
                    })
                    .collect();

                tokio::select! {
                    // A client announced itself, and what came back knows how to reach it. The
                    // dial is what turns that into a client, and it greets while it dials, so a
                    // peer that never answers is a client that is not attached.
                    transport = announcements.next() => Action::Announced(transport),
                    // A client said something.
                    (outcome, index, _rest) = futures_util::future::select_all(reads) => {
                        Action::Said { outcome, index }
                    }
                }
            };

            match action {
                Action::Announced(Some(transport)) => match transport.connect().await {
                    Ok(connection) => sessions.push(Session::new(
                        connection,
                        cols,
                        rows,
                        controller.clone(),
                    )),
                    Err(e) => eprintln!("tui: could not reach an announced client: {e}"),
                },
                // The pty is gone, so the session is over and there is nothing left to serve.
                Action::Announced(None) => return Ok(()),
                Action::Said { outcome, index } => match outcome {
                    // What a client said is the session's business, not the loop's: the loop
                    // hands the message over and the session decides what it means. Painting a
                    // frame is slow enough to be worth not doing inside the borrow.
                    Ok(Some(message)) => sessions[index].handle_msg(message).await?,
                    // The client closed between messages, which is the normal end of one.
                    Ok(None) => {
                        sessions.remove(index);
                        eprintln!("tui: a client disconnected");
                    }
                    Err(e) => {
                        eprintln!("tui: client {index} ended: {e}");
                        sessions.remove(index);
                    }
                },
            }
        }
    }
}

/// What the loop decided to do, once the borrows the decision was made under are gone.
enum Action {
    /// The wait for an announcement finished, one way or another.
    Announced(Option<Box<dyn Transport>>),
    /// A client's read finished.
    Said {
        outcome: io::Result<Option<Message>>,
        index: usize,
    },
}
