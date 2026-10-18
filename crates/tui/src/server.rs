//! The image server: a client tells it where it is, and it dials.
//!
//! The server does **not** listen. The client owns the channel and announces it;
//! this side's only job is to notice the announcement and dial back.
//!
//! ```text
//!   pty output ──▶ scan ──▶ a link ──▶ dial ──▶ greet/Pong ──▶ frames
//! ```
//!
//! Everything above the scan is asynchronous and runs on the server's own task,
//! never on the pty's forwarding thread: that thread runs on the path of every byte
//! the command writes, and blocking it on a dial would stall the command's output.

use std::io;
use std::sync::{Arc, Mutex};

use wire::message::Message;
use wire::transport::{Connection, Transport};

use crate::session::{Session, Sessions};
use crate::sink::Sink;

/// The server: the scan of what the pty forwards, and the clients it finds.
pub struct Server {
    /// The one writer to the screen, shared with the pty's proxy.
    ///
    /// Held but not yet written to: once frames are drawn they go out through
    /// this, which is what keeps a frame from landing in the middle of the
    /// command's output.
    sink: Sink,
    /// Where a found link arrives from the scan.
    ///
    /// Blocking, because the sender is the pty's forwarding thread: it is not a
    /// task and has nothing to await on. [`Server::run`] moves the waiting to a
    /// thread of its own for that reason.
    found: std::sync::mpsc::Receiver<Connection>,
    /// Every client the server has dialed.
    ///
    /// Shared because each client is read by its own task, and what the server
    /// knows about a client has to outlive the borrow of its link.
    sessions: Arc<Mutex<Sessions>>,
}

impl Server {
    pub fn new(sink: Sink, found: std::sync::mpsc::Receiver<Connection>) -> Self {
        Self {
            sink,
            found,
            sessions: Arc::new(Mutex::new(Sessions::new())),
        }
    }

    /// Dial every client the scan finds, until the channel closes.
    pub async fn run(self) -> io::Result<()> {
        // Not written yet, but the field is the reason a second writer to the
        // screen cannot exist: once frames are drawn they go through here.
        let _sink = self.sink;
        let sessions = self.sessions;

        // The scan's channel is blocking, so it gets a thread rather than a
        // runtime worker: waiting on it is not an await. A bounded channel back to
        // the runtime keeps the two apart, and `try_send` keeps this thread from
        // ever blocking on the server.
        let (found_tx, mut found) = tokio::sync::mpsc::channel::<Connection>(16);
        let scanning = self.found;
        std::thread::spawn(move || {
            while let Ok(connection) = scanning.recv() {
                if found_tx.try_send(connection).is_err() {
                    eprintln!("tui: not dialing a client; the server is behind");
                }
            }
        });

        while let Some(connection) = found.recv().await {
            dial(connection, sessions.clone());
        }
        Ok(())
    }
}

/// Dial a found link, then serve it.
///
/// Spawned rather than awaited: a slow dial must not stall the loop, so one
/// client's unreachable channel does not stop the next client from being found.
fn dial(connection: Connection, sessions: Arc<Mutex<Sessions>>) {
    tokio::spawn(async move {
        // The transport that recognized the announcement is the one holding the
        // framing, and it is what dials: a link does not know how to reach its own
        // kind of peer, and looking one up by name later could find a different
        // one.
        let Some(transport) = connection.dialing_transport() else {
            eprintln!("tui: a link arrived already dialed, which nothing produces");
            return;
        };
        match connection.connect().await {
            Ok(connected) => serve(transport, connected, sessions),
            Err(e) => eprintln!("tui: could not dial a client that announced itself: {e}"),
        }
    });
}

/// Serve one client, on its own task.
fn serve(transport: Arc<dyn Transport>, connection: Connection, sessions: Arc<Mutex<Sessions>>) {
    tokio::spawn(serve_client(transport, connection, sessions));
}

/// Greet a client, then read what it sends until it disconnects.
///
/// The greeting goes first, before any reading: a client draws nothing until it
/// hears a ping, so sending it late would leave a pane blank while the server
/// waits for a message the client is waiting to be told to send.
async fn serve_client(
    transport: Arc<dyn Transport>,
    connection: Connection,
    sessions: Arc<Mutex<Sessions>>,
) {
    let Some(mut session) = Session::new(connection, transport.clone()) else {
        eprintln!("tui: a transport handed over a link that is not connected");
        return;
    };
    let peer = session.peer().to_string();
    let label = transport.label();

    if let Err(e) = session.greet().await {
        eprintln!("tui: could not greet {peer}: {e}");
        return;
    }
    // Attached *after* the greeting went out, and before any answer: the point of
    // `established` is to tell a client that has answered from one that has not,
    // so a client has to be known while it still has not.
    sessions.lock().unwrap_or_else(|e| e.into_inner()).attach(&session, label);

    loop {
        match session.recv().await {
            Ok(Some(message)) => {
                let established = sessions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .record(&peer, &message);
                if established {
                    eprintln!("tui: {peer} is established over {}", session.transport_name());
                }
                report(&message);
            }
            // A disconnect is the normal end of a client, not a failure.
            Ok(None) => break,
            Err(e) => {
                eprintln!("tui: {peer} ended: {e}");
                break;
            }
        }
    }
    if sessions
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .detach(&peer)
        .is_some()
    {
        eprintln!("tui: {peer} disconnected");
    }
}

/// Say what a client sent, for the messages the server has nothing else to do
/// with yet.
///
/// `Pong` is not here: it is the handshake, and the registry already acts on it.
fn report(message: &Message) {
    match message {
        Message::Frame(body) => {
            eprintln!("tui: dropped a {} byte frame (no display yet)", body.len());
        }
        Message::Error(text) => eprintln!("tui: a client reported: {text}"),
        // A server does not ping a client, so hearing one back is a client
        // speaking a protocol we did not give it.
        Message::Ping(_) => eprintln!("tui: a client sent a ping, which only a server sends"),
        Message::Pong => {}
    }
}
