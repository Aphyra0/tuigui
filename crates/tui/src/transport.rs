//! The server's transport: how it reaches a client that told it where to be found.
//!
//! The server **dials**. The client is the half that listens, because the client
//! is the half that cannot be reached: it may be in a multiplexer's pane, on
//! another machine, or started by something whose environment nobody controls. Its
//! own output is the only channel it is guaranteed to have, so it says there where
//! it is listening, and the server acts on what it reads.
//!
//! ```text
//!   client: listen(path) ──▶ writes the path into its output ──▶ pty ──▶ server
//!   server: reads it ──▶ connect(path)
//! ```
//!
//! So this side implements [`Transport::connect`] and not `listen`: a server has
//! no address of its own, and a `listen` that could never be called is a method
//! nobody should be able to reach for. There is therefore nothing to bind here and
//! nothing to clean up.

use std::io;
use std::path::PathBuf;

use wire::message::Message;
use wire::transport::{
    read_message, write_message, Advert, BoxFuture, Capabilities, Connection, Listener, Transport,
};

/// `Advert` key: the path a client is listening on.
pub const PATH: &str = "socket";

/// A connection to a path a client announced.
///
/// The server's half of the socket transport. Named for what it does from this
/// side — it is an influx by path, a channel arriving from a path — rather than
/// for the mechanism, because the mechanism is also the client's and the two
/// halves are not the same thing.
#[derive(Debug)]
pub struct InfluxByPath {
    path: PathBuf,
}

impl InfluxByPath {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The channel an advert describes.
    ///
    /// Total: an advert that names this transport and has no `socket` key still
    /// describes a path, just an empty one, and the dial is where that becomes an
    /// error. Rejecting it here would make the failure depend on which caller
    /// asked.
    pub fn from_advert(advert: &Advert) -> Self {
        Self::new(advert.get(PATH).unwrap_or_default())
    }

    /// The path this will dial.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Transport for InfluxByPath {
    fn name(&self) -> &'static str {
        NAME
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            in_process: false,
            local: true,
            // Not encrypted, but also not exposed: the socket is a file with the
            // owner's permissions, and nothing crosses a network.
            encrypted: false,
            may_relay: false,
            warn: None,
        }
    }

    fn label(&self) -> String {
        format!("unix : {}", self.path.display())
    }

    fn advert(&self) -> Option<Advert> {
        Some(Advert::new(NAME).with(PATH, self.path.to_string_lossy().into_owned()))
    }

    /// This transport's own announcement: its name, then the path to dial.
    ///
    /// The [`Transport::label`], and not a separate format: the text a peer shows
    /// and the text it dials by are the same fact, so a label that disagreed with
    /// an announcement would be a client showing one address and listening on
    /// another.
    fn to_magic(&self) -> String {
        self.label()
    }

    /// The socket an announcement describes, if it is one of ours.
    ///
    /// `name` is the transport named in the announcement and `address` what
    /// follows it, already split by the scanner. A name that is not ours returns
    /// `None` rather than being rejected, so the scanner can offer the same line to
    /// every transport in turn.
    fn from_magic(name: &str, address: &str) -> Option<Self> {
        (name == NAME).then(|| Self::new(address))
    }

    fn connect<'a>(&'a self, advert: &'a Advert) -> BoxFuture<'a, io::Result<Connection>> {
        Box::pin(async move {
            let path = advert
                .get(PATH)
                .ok_or_else(|| io::Error::other(format!("advert has no `{PATH}`")))?;
            let stream = tokio::net::UnixStream::connect(path).await?;
            Ok(Connection::connected(stream, NAME, path.to_string()))
        })
    }

    fn listen<'a>(&'a self) -> BoxFuture<'a, io::Result<Option<Listener>>> {
        // A server has nothing to listen on: the client owns the channel. A caller
        // that reached this has asked the wrong side.
        Box::pin(async { Ok(None) })
    }

    fn send<'a>(
        &'a self,
        link: &'a mut Connection,
        message: &Message,
    ) -> BoxFuture<'a, io::Result<()>> {
        write_message(link, message)
    }

    fn recv<'a>(&'a self, link: &'a mut Connection) -> BoxFuture<'a, io::Result<Option<Message>>> {
        read_message(link)
    }
}

/// Stable key for this transport, as it appears in an advert.
pub const NAME: &str = "unix";

/// The names of the transports this build can dial.
///
/// The scanner's search is built from these, so a new transport is added once:
/// implement it, and put its name here. Everything else — what it announces, how the
/// text is found, which one answers — follows from that.
pub fn names() -> Vec<&'static str> {
    vec![NAME]
}

/// The transport that can dial the client an announcement describes.
///
/// The **only** place on this side that knows which channels exist, and it does not
/// look inside the announcement itself: each channel is asked whether it is the one
/// that wrote it, and the one that answers is the one that knows how to dial it.
/// That is what lets a channel decide its own announcement's shape — a socket
/// carries a path, a relay carries a pane — without anything here changing.
///
/// A channel this build does not have returns `None` without complaint, because an
/// announcement for a transport we do not speak is not an error, only something we
/// cannot use — and a newer client against an older server is exactly that.
pub fn for_magic(name: &str, address: &str) -> Option<Box<dyn Transport>> {
    InfluxByPath::from_magic(name, address).map(|t| Box::new(t) as Box<dyn Transport>)
}
