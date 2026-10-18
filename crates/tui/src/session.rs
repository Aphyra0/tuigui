//! Clients attached to this session, and what the server knows about each.
//!
//! Two things, deliberately separate:
//!
//! | | |
//! |---|---|
//! | [`Session`] | one client's link, owned by the task reading it |
//! | [`Sessions`] | the server's memory of every client |
//!
//! The split is not ceremony. A link can only be read by whoever holds it mutably,
//! so the reader task must own the `Session`. What the server needs to know *about*
//! that client — whether it answered the greeting, what it calls itself — has to
//! outlive the borrow, and a second reader would desynchronize the stream. So the
//! state is recorded where the server loop can see it, and the link stays with the
//! only thing allowed to touch it.
//!
//! ```text
//!   a dial succeeds ──▶ Session::greet() sends Ping(label)
//!                            │  the reader reads a Pong
//!                            ▼
//!                        Sessions records it as established
//! ```
//!
//! The distinction is not bookkeeping. A link that has been greeted but not
//! answered is a socket that is open, not a client that is with us; collapsing the
//! two would make "connected" and "drawing" the same statement, and the server
//! sends what a client should *display* only to an established one.

use std::collections::HashMap;
use std::io;
use std::sync::Arc;

use wire::message::Message;
use wire::transport::{Connection, Transport};

/// One client's link, owned by the task that reads it.
pub struct Session {
    connection: Connection,
    /// The transport that dialed this link.
    ///
    /// Held, not looked up again by name: framing and labeling live on the
    /// transport, and a link that had to find its own transport later could find a
    /// different one.
    transport: Arc<dyn Transport>,
}

impl Session {
    /// Wrap a *connected* link, before anything has been said on it.
    ///
    /// Returns `None` for a link that is not connected, rather than storing it and
    /// failing later on every read: a session that cannot be written to is not a
    /// session, and the caller should hear about it at construction.
    pub fn new(connection: Connection, transport: Arc<dyn Transport>) -> Option<Self> {
        connection
            .is_connected()
            .then_some(Self {
                connection,
                transport,
            })
    }

    /// Greet the client: this is who you are talking to, and you may start.
    ///
    /// The ping carries the transport's label, which is what the client puts on
    /// screen. Sent from here rather than by the caller because it is the first
    /// thing every session does, and a caller that forgot it would leave a client
    /// waiting for a greeting that never comes.
    pub async fn greet(&mut self) -> io::Result<()> {
        self.transport
            .send(&mut self.connection, &Message::Ping(self.transport.label()))
            .await
    }

    /// Read one message from the client.
    ///
    /// `None` means the client closed, which is the normal end of one. The
    /// meaning of a message — which one establishes the handshake — is decided by
    /// [`Sessions`], not here: it is a fact about the session as a whole, and this
    /// type only has this client's link.
    pub async fn recv(&mut self) -> io::Result<Option<Message>> {
        self.transport.recv(&mut self.connection).await
    }

    /// Who this client is, for looking it up in [`Sessions`].
    ///
    /// Total because a session is only ever built from a connected link, which is
    /// what makes the peer known; the expect message says so rather than leaving a
    /// reader to work it out.
    pub fn peer(&self) -> &str {
        self.connection
            .peer()
            .expect("a session is only built from a connected link")
    }

    /// Which transport carries this client. Not for branching on in the drawing
    /// path, only for logging.
    pub fn transport_name(&self) -> &'static str {
        self.connection
            .transport()
            .expect("a session is only built from a connected link")
    }
}

/// What the server remembers about one client.
///
/// A separate type from [`Session`] on purpose: this is the part that must be
/// readable while the link is being read, so it holds no link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    /// The transport that dialed this client, by name.
    pub transport: &'static str,
    /// What the client calls itself, for anything shown to a user.
    pub label: String,
    /// Whether the client answered the greeting.
    pub established: bool,
}

/// Every client the server has dialed, in memory.
///
/// In memory and nowhere else: there is no session file, so nothing has to be
/// written, provisioned, cleaned up, or kept in step with the processes that are
/// actually running. A client is known exactly while it is connected.
#[derive(Debug, Default)]
pub struct Sessions {
    /// Keyed by peer, because that is what is distinct about a client from the
    /// server's side: two clients on two paths are two clients, and one that
    /// reconnects is the same one.
    by_peer: HashMap<String, State>,
}

impl Sessions {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a client the server has just dialed and greeted.
    ///
    /// Registered *before* the greeting is answered, with `established` false: the
    /// point of the flag is to tell those two cases apart, so a client must be
    /// known in the un-established state for it to mean anything.
    pub fn attach(&mut self, session: &Session, label: String) {
        self.by_peer.insert(
            session.peer().to_string(),
            State {
                transport: session.transport_name(),
                label,
                established: false,
            },
        );
    }

    /// Fold in one message from a client, returning whether this changed anything
    /// the server knows.
    ///
    /// `true` on the transition to established, which is the one moment worth
    /// telling a log about. Later pongs are not news.
    pub fn record(&mut self, peer: &str, message: &Message) -> bool {
        let Some(state) = self.by_peer.get_mut(peer) else {
            // A message from a client that was never dialed. Reporting it as
            // noise would hide a bug in the accept path; saying so is the point.
            eprintln!("tui: a message arrived from a client that is not attached: {peer}");
            return false;
        };
        match message {
            Message::Pong if !state.established => {
                state.established = true;
                true
            }
            // Anything else, including a late pong, leaves the state as it is:
            // a frame before the handshake is not a handshake, and treating any
            // traffic as an answer would defeat the point of having one.
            _ => false,
        }
    }

    /// Forget a client that disconnected.
    pub fn detach(&mut self, peer: &str) -> Option<State> {
        self.by_peer.remove(peer)
    }

    pub fn get(&self, peer: &str) -> Option<&State> {
        self.by_peer.get(peer)
    }

    pub fn len(&self) -> usize {
        self.by_peer.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_peer.is_empty()
    }

    /// How many clients have answered the greeting.
    ///
    /// The number a caller wants: "three clients connected" is not the same as
    /// "three clients attached", and only this one means the session is in use.
    pub fn established(&self) -> usize {
        self.by_peer.values().filter(|s| s.established).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wire::transport::{
        read_message, strip_announcement, wrap_announcement, write_message, Advert, BoxFuture,
        Capabilities, Listener,
    };

    /// A transport whose only job is to be one.
    struct Stub;

    impl Transport for Stub {
        fn name(&self) -> &'static str {
            "stub"
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                in_process: false,
                local: true,
                encrypted: false,
                may_relay: false,
                warn: None,
            }
        }
        fn label(&self) -> String {
            "unix : /tmp/client.sock".into()
        }
        fn advert(&self) -> Option<Advert> {
            Some(Advert::new("stub"))
        }
        fn to_magic(&self) -> String {
            wrap_announcement("stub")
        }
        fn from_magic(magic: &str) -> Option<Self> {
            (strip_announcement(magic) == Some("stub")).then_some(Stub)
        }
        fn connect<'a>(&'a self, _advert: &'a Advert) -> BoxFuture<'a, io::Result<Connection>> {
            Box::pin(async { Err(io::Error::other("the stub does not dial")) })
        }
        fn listen<'a>(&'a self) -> BoxFuture<'a, io::Result<Option<Listener>>> {
            Box::pin(async { Ok(None) })
        }
        fn send<'a>(
            &'a self,
            link: &'a mut Connection,
            message: &Message,
        ) -> BoxFuture<'a, io::Result<()>> {
            write_message(link, message)
        }
        fn recv<'a>(
            &'a self,
            link: &'a mut Connection,
        ) -> BoxFuture<'a, io::Result<Option<Message>>> {
            read_message(link)
        }
    }

    /// A session on one end of a pair, the client's end, and the transport.
    ///
    /// The client's end is wrapped in a `Connection` so the framing helpers can be
    /// used on it: it is not a session, because it is not the server's side.
    fn pair() -> (Session, Connection, Arc<Stub>) {
        let (ours, theirs) = wire::transport::in_process(256);
        let transport = Arc::new(Stub);
        let session = Session::new(
            Connection::connected(ours, "stub", "/tmp/client.sock"),
            transport.clone(),
        )
        .expect("a connected link is a session");
        (
            session,
            Connection::connected(theirs, "stub", "server"),
            transport,
        )
    }

    /// A link that never connected is not a session.
    #[tokio::test]
    async fn a_session_refuses_a_link_that_never_connected() {
        assert!(Session::new(Connection::Closed, Arc::new(Stub)).is_none());
    }

    /// The greeting carries the transport's label, and the client's answer is what
    /// establishes the session.
    ///
    /// This is the whole handshake: before the answer the server has a link, after
    /// it the server has a client.
    #[tokio::test]
    async fn a_pong_establishes_the_session() {
        let (mut session, mut client, _transport) = pair();
        let mut sessions = Sessions::new();
        sessions.attach(&session, "unix : /tmp/client.sock".into());
        assert_eq!(sessions.established(), 0, "nothing has been said yet");

        session.greet().await.expect("the greeting goes out");
        assert_eq!(
            read_message(&mut client).await.unwrap(),
            Some(Message::Ping("unix : /tmp/client.sock".into())),
            "the greeting must name this session"
        );

        write_message(&mut client, &Message::Pong).await.unwrap();
        let message = session.recv().await.unwrap().expect("the pong arrived");
        assert!(
            sessions.record(session.peer(), &message),
            "the transition is worth reporting"
        );
        assert_eq!(sessions.established(), 1);
        // A second answer is not news, and must not be reported as a transition.
        assert!(!sessions.record(session.peer(), &Message::Pong));
        assert_eq!(sessions.established(), 1);
    }

    /// Anything that is not an answer leaves the session unestablished.
    ///
    /// A frame arriving before the handshake is not a handshake: treating any
    /// traffic as an answer would defeat the point of having one.
    #[tokio::test]
    async fn other_messages_do_not_establish_the_session() {
        let (mut session, mut client, _transport) = pair();
        let mut sessions = Sessions::new();
        sessions.attach(&session, "label".into());

        write_message(&mut client, &Message::Frame(b"early".to_vec()))
            .await
            .unwrap();
        let message = session.recv().await.unwrap().expect("the frame arrived");
        assert!(!sessions.record(session.peer(), &message));
        assert_eq!(sessions.established(), 0, "a frame is not a pong");
    }

    /// A message from a client the server never attached is not recorded.
    ///
    /// Attachment is what makes a peer known; a message from outside it means the
    /// attach path is wrong, and silently inventing a state for it would hide that.
    #[test]
    fn a_message_from_an_unknown_client_is_not_recorded() {
        let mut sessions = Sessions::new();
        assert!(!sessions.record("/tmp/nobody.sock", &Message::Pong));
        assert!(sessions.is_empty());
        assert_eq!(sessions.established(), 0);
    }

    /// A disconnected client is forgotten, and its state goes with it.
    #[test]
    fn detaching_forgets_the_client() {
        let mut sessions = Sessions::new();
        sessions.by_peer.insert(
            "/tmp/client.sock".into(),
            State {
                transport: "stub",
                label: "unix : /tmp/client.sock".into(),
                established: true,
            },
        );
        assert_eq!(sessions.len(), 1);
        assert!(sessions.detach("/tmp/client.sock").is_some());
        assert!(sessions.is_empty());
        assert_eq!(sessions.established(), 0);
    }

    /// A client that closes is an ordinary end, not a failure.
    #[tokio::test]
    async fn a_closed_client_reads_as_none() {
        let (mut session, client, _transport) = pair();
        drop(client);
        assert_eq!(session.recv().await.unwrap(), None);
    }
}
