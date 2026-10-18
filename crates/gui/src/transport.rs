//! The client's transport: how it makes itself findable.
//!
//! The client **listens**. It is the half that cannot be reached — it may be in a
//! multiplexer's pane, on another machine, or started by something whose
//! environment nobody controls — so it binds a channel of its own choosing and
//! writes that choice into its output, which the server is already reading. The
//! server then dials it.
//!
//! ```text
//!   listen(path) ──▶ write the path into stdout ──▶ pty ──▶ server dials it
//! ```
//!
//! So this side implements [`Transport::listen`] and not `connect`: a client has
//! nobody to dial, and a `connect` that could never be called is a method nobody
//! should be able to reach for.
//!
//! # Why the path is the client's to choose
//!
//! Nothing else can choose it. A server cannot write into a client's filesystem,
//! and a path provisioned from outside is a path two clients would share. The
//! client picks under `XDG_RUNTIME_DIR` — private to the user, cleared on logout —
//! and says so in the one place the other half is guaranteed to read.

use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use wire::message::Message;
use wire::transport::{read_message, write_message, Advert, BoxFuture, Capabilities, Connection, Listener, Transport, Accept};

/// `Advert` key: the path this client is listening on.
pub const PATH: &str = "socket";

/// A socket this client owns and accepts a server on.
pub struct ListenOnPath {
    path: PathBuf,
    /// Created once by [`ListenOnPath::listen`], so a second call is not a
    /// rebind.
    ///
    /// `Arc` because a [`Listener`] outlives the call that made it and needs its
    /// own handle: `UnixListener` has no `try_clone`, unlike its std counterpart.
    listener: Mutex<Option<Arc<tokio::net::UnixListener>>>,
}

impl ListenOnPath {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            listener: Mutex::new(None),
        }
    }
}

impl Transport for ListenOnPath {
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
    /// and the text it dials by are the same fact, so a label that disagreed with an
    /// announcement would be a client showing one address and listening on another.
    ///
    /// Plain text, not an escape sequence, because of what the output passes
    /// through: see [`wire::transport::ANNOUNCE_SEPARATOR`].
    fn to_magic(&self) -> String {
        self.label()
    }

    /// The socket an announcement describes, if it is one of ours.
    ///
    /// Present because the trait requires both halves, and honest rather than
    /// stubbed: a client handed an announcement it wrote would parse it, and this is
    /// the code that has to be right if that ever happens.
    fn from_magic(name: &str, address: &str) -> Option<Self> {
        (name == NAME).then(|| Self::new(address))
    }

    fn connect<'a>(&'a self, _advert: &'a Advert) -> BoxFuture<'a, io::Result<Connection>> {
        // A client has nobody to dial: the server dials it. A caller that reached
        // this has asked the wrong side.
        Box::pin(async { Err(io::Error::other("a client does not dial; the server dials it")) })
    }

    fn listen<'a>(&'a self) -> BoxFuture<'a, io::Result<Option<Listener>>> {
        Box::pin(async move {
            let mut slot = self
                .listener
                .lock()
                .map_err(|_| io::Error::other("the socket transport was poisoned"))?;
            if slot.is_none() {
                // A stale file from a previous run would make the bind fail and
                // look like a permissions problem. Removing it is the same thing
                // `sshd` does with `StreamLocalBindUnlink`.
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
                *slot = Some(Arc::new(tokio::net::UnixListener::bind(&self.path)?));
            }
            let listener = slot.as_ref().expect("just bound").clone();
            Ok(Some(Listener::new(Acceptor { listener }, NAME)))
        })
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

/// Carries an accept loop past the borrow of the transport.
struct Acceptor {
    listener: Arc<tokio::net::UnixListener>,
}

impl Accept for Acceptor {
    fn accept(&self) -> BoxFuture<'_, io::Result<Connection>> {
        Box::pin(async move {
            let (stream, _) = self.listener.accept().await?;
            Ok(Connection::connected(stream, NAME, "local"))
        })
    }
}

/// Stable key for this transport, as it appears in an advert.
pub const NAME: &str = "unix";
