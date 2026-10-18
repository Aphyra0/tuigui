//! The unix socket transport: one path, both sides of a session.
//!
//! ```text
//!   one side: bind(path) ──▶ writes "unix : path" into its output ──▶ the other reads it
//!   the other: from_magic("unix", path) ──▶ connect()
//! ```
//!
//! The side that binds chooses the path, because nothing else can: a peer cannot write into
//! its filesystem, and a path provisioned from outside is a path two clients would share. It
//! picks under `XDG_RUNTIME_DIR` — private to the user, cleared on logout — and says so in
//! the output the other half is already reading.
//!
//! # Both halves are here
//!
//! [`UnixSocket`] dials *and* accepts. Either side of a session may take either role, and the
//! two roles are less different than they look: both end in a greeting and an answer, and
//! only the order differs — the one that dialed greets, and the one that was dialed answers.
//! A type that could only do one of them would be a type the other side cannot use, and the
//! server and the client are the same program's two ends rather than two kinds of thing.

use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::io::AsyncReadExt;
use tokio::sync::Mutex;

use crate::message::Message;
use crate::transport::{write_message, BoxFuture, Connection, Framing, Transport};

/// Stable key for this transport, as it appears in an announcement.
pub const NAME: &str = "unix";

/// The names of the transports this build can dial.
///
/// A scan is built from these, so a transport is added once: implement it, and put its name
/// here. Everything else — what it announces, how the text is found, which one answers —
/// follows from that.
pub fn names() -> Vec<&'static str> {
    vec![NAME]
}

/// A unix socket at a known path.
///
/// Cloneable, and the clone is the same socket: the listener and the link are behind `Arc`s,
/// so a [`Connection`] can hold one handle while the caller keeps another for the next
/// accept.
#[derive(Clone)]
pub struct UnixSocket {
    path: PathBuf,
    /// The bound listener, once bound.
    ///
    /// `None` until [`UnixSocket::bind`], and kept afterwards so a second call is not a
    /// rebind. An `Arc` because a listener is shared with the accept that uses it and cannot
    /// be cloned itself: `tokio`'s `UnixListener` has no `try_clone`, unlike its std
    /// counterpart.
    listener: Arc<Mutex<Option<Arc<tokio::net::UnixListener>>>>,
    /// The link, once there is one.
    ///
    /// A tokio mutex because reading and writing are awaited while it is held, so the lock
    /// must be `Send`.
    link: Arc<Mutex<Option<Link>>>,
}

/// The connected half: the stream, and what has arrived on it.
struct Link {
    stream: tokio::net::UnixStream,
    framing: Framing,
}

impl UnixSocket {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            listener: Arc::new(Mutex::new(None)),
            link: Arc::new(Mutex::new(None)),
        }
    }

    /// The path this socket is at.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Bind the socket, so that the address this announces is one that answers.
    ///
    /// Separate from [`Transport::connect`] and [`Transport::accept`] because it has to
    /// happen *before* either: the address is written into an announcement, and a peer that
    /// dials the address of a socket nobody has bound dials nothing.
    ///
    /// A second call is not a rebind, which is what lets a caller be careless about it.
    pub async fn bind(&self) -> io::Result<()> {
        let mut listener = self.listener.lock().await;
        if listener.is_none() {
            // A stale file from a previous run would make the bind fail and look like a
            // permissions problem. Removing it is what `sshd` does with
            // `StreamLocalBindUnlink`.
            match std::fs::remove_file(&self.path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(io::Error::new(
                        e.kind(),
                        format!("removing stale {}: {e}", self.path.display()),
                    ))
                }
            }
            if let Some(dir) = self.path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            *listener = Some(Arc::new(tokio::net::UnixListener::bind(&self.path)?));
        }
        Ok(())
    }

    /// Adopt a stream as this socket's link.
    ///
    /// Shared by dialing and accepting, which differ only in where the stream came from.
    async fn adopt(&self, stream: tokio::net::UnixStream) {
        *self.link.lock().await = Some(Link {
            stream,
            framing: Framing::new(),
        });
    }
}

impl Transport for UnixSocket {
    fn name(&self) -> &'static str {
        NAME
    }

    fn address(&self) -> Option<String> {
        Some(self.path.to_string_lossy().into_owned())
    }

    fn from_magic(name: &str, address: &str) -> Option<Self> {
        (name == NAME).then(|| Self::new(address))
    }

    /// Dial the path this socket holds, greet, and wait for the answer.
    ///
    /// The greeting is here and not in the caller because the two halves of the handshake
    /// only make sense together: the side that dialed is the side that knows the link is new,
    /// so it greets, and a caller that forgot to would leave the other end waiting forever.
    fn connect(&self) -> BoxFuture<'_, io::Result<Connection>> {
        Box::pin(async move {
            let stream = tokio::net::UnixStream::connect(&self.path).await?;
            self.adopt(stream).await;

            self.send(&Message::Ping(self.label())).await?;
            match self.recv().await? {
                Some(Message::Pong) => Ok(Connection::new(self.clone())),
                Some(other) => Err(io::Error::other(format!(
                    "a peer answered the greeting with {other:?}"
                ))),
                None => Err(io::Error::other("a peer never answered the greeting")),
            }
        })
    }

    /// Wait for a peer to dial, answer its greeting, and return the link.
    ///
    /// The mirror of [`Transport::connect`]. The order is fixed here rather than left to a
    /// caller: answering before the greeting arrives would be answering nothing, and reading
    /// after it would be reading a greeting already consumed.
    fn accept(&self) -> BoxFuture<'_, io::Result<Connection>> {
        Box::pin(async move {
            let listener = {
                let guard = self.listener.lock().await;
                guard
                    .as_ref()
                    .ok_or_else(|| io::Error::other("this socket is not bound"))?
                    .clone()
            };
            let (stream, _) = listener.accept().await?;
            self.adopt(stream).await;

            match self.recv().await? {
                // The greeting. Answering it is what completes the handshake, and until it is
                // answered the other end is still waiting.
                Some(Message::Ping(_)) => {
                    self.send(&Message::Pong).await?;
                    Ok(Connection::new(self.clone()))
                }
                Some(other) => Err(io::Error::other(format!(
                    "a peer's first message was {other:?}, not a greeting"
                ))),
                None => Err(io::Error::other("a peer connected but never greeted")),
            }
        })
    }

    fn send<'a>(&'a self, message: &'a Message) -> BoxFuture<'a, io::Result<()>> {
        Box::pin(async move {
            let mut link = self.link.lock().await;
            let link = link
                .as_mut()
                .ok_or_else(|| io::Error::other("this socket has no link yet"))?;
            write_message(&mut link.stream, message).await
        })
    }

    /// Read one message, waiting for a whole one.
    ///
    /// A read that returns the instant it is asked would ask before the peer has had a chance
    /// to write, and every handshake would fail on a peer that was merely not instant. So this
    /// waits, and `None` means the peer closed rather than "nothing yet".
    ///
    /// The lock is held across the read, which is what keeps two readers from interleaving on
    /// one stream.
    fn recv(&self) -> BoxFuture<'_, io::Result<Option<Message>>> {
        Box::pin(async move {
            let mut guard = self.link.lock().await;
            let link = guard
                .as_mut()
                .ok_or_else(|| io::Error::other("this socket has no link yet"))?;
            loop {
                let mut buf = [0u8; 8192];
                let n = link.stream.read(&mut buf).await?;
                if n == 0 {
                    // A clean close between messages is the normal end of a peer, not a
                    // failure: nothing was half-read.
                    return Ok(None);
                }
                // `push` returns the messages this read completed, and takes them out of the
                // framing: reading its return is the whole point, because `next` after it would
                // find nothing and the loop below would wait forever on a socket with nothing
                // left to say.
                if let Some(message) = link.framing.push(&buf[..n])?.into_iter().next() {
                    return Ok(Some(message));
                }
            }
        })
    }
}

impl UnixSocket {
    /// What this transport calls itself, for the greeting.
    ///
    /// The path, because that is what the other end would need to say which socket this is.
    /// The label is logs and nothing else: the name and address are what identify a
    /// transport.
    fn label(&self) -> String {
        self.path.to_string_lossy().into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bind, accept, and connect against each other, and get a link.
    ///
    /// The handshake is the part that cannot be tested by inspection: both ends wait for the
    /// other, so a wrong order is a hang rather than a failure, and a hang under a timeout is
    /// the only way to see it.
    #[tokio::test]
    async fn the_handshake_completes_from_both_sides() {
        let dir = std::env::temp_dir().join(format!("wire-unix-{}", std::process::id()));
        let path = dir.join("s.sock");
        let _ = std::fs::remove_dir_all(&dir);

        let bound = UnixSocket::new(&path);
        bound.bind().await.expect("bind");

        // The announcer: it knows the path, so it is the address a peer arrives at.
        let dialer = UnixSocket::new(&path);

        let accept = bound.accept();
        let connect = dialer.connect();
        let (accepted, dialed) = tokio::join!(accept, connect);

        let accepted = accepted.expect("the accepting side establishes");
        let dialed = dialed.expect("the dialing side establishes");

        // Both ends are links, so both can send and be read at the other end.
        dialed.send(&Message::Ping("hi".into())).await.expect("send");
        assert_eq!(
            accepted.recv().await.expect("recv"),
            Some(Message::Ping("hi".into()))
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
