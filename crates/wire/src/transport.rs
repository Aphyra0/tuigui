//! The transport seam: how a client and a server reach each other.
//!
//! The seam is a **byte stream**, not a message channel. [`crate::proto`] owns
//! framing and runs over whatever stream a transport hands it, so a transport
//! never implements that framing itself — only "give me a stream". That is what
//! makes swapping channels cheap and keeps the wire format in one place.
//!
//! # What is here, and what is not
//!
//! This module is the **shape**: the trait, the link, the advert, the
//! capabilities. It names no channel and knows no address, so it is the same file
//! whichever side of the session is compiled.
//!
//! The implementations are **per side**, in the binary that needs them:
//!
//! | | |
//! |---|---|
//! | `tui/src/transport.rs` | the server's: it dials, so it implements `connect` |
//! | `gui/src/transport.rs` | the client's: it is dialed to, so it implements `listen` |
//!
//! They are separate files rather than one shared module because the two halves
//! of a session need different things from a channel, and a single shared
//! implementation has to satisfy both — which is how a client ends up carrying
//! code for dialing a peer it never dials. Each side also decides its own set of
//! channels and how to announce them, which is policy rather than protocol.

use std::collections::BTreeMap;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};

use crate::message::Message;

/// The sequence a client opens its announcement with. `ESC ] 3 0 0 0 1 ;`
///
/// The delimiters an announcement is written between, as they travel through a
/// terminal.
///
/// OSC because it has a numeric namespace and an explicit terminator, so it is
/// unambiguous to find and to delimit, and a terminal that does not recognize the
/// dispatch number ignores it rather than corrupting the screen. That is what
/// makes it safe to write into a pane at all.
///
/// Here, and not in a transport, because two halves of a session may be *different
/// transports*: the client announces over a herdr pane while the server scans a
/// unix pty, and both are looking for the same sequence in the same terminal. What
/// is inside the delimiters is each transport's own business — see
/// [`Transport::to_magic`] — but where it starts and ends is the one thing they
/// have to agree on.
pub const ANNOUNCE_OPEN: &[u8] = b"\x1b]30001;";
/// What ends an announcement. BEL, which is the short OSC terminator.
pub const ANNOUNCE_CLOSE: u8 = 0x07;

/// The longest announcement a scanner will hold while waiting for its terminator.
///
/// An unterminated one would otherwise grow without bound: the carry is fed by a
/// stream nobody controls, so an opener followed by a flood would grow it until
/// memory runs out. Past this the carry is dropped, which loses one announcement
/// and keeps the process alive.
pub const ANNOUNCE_MAX_PAYLOAD: usize = 8 * 1024;

/// Wrap a transport's own value in the announcement delimiters.
///
/// A transport calls this from [`Transport::to_magic`] and chooses what goes in
/// the middle. Base64 by default because the announcement travels through a
/// terminal, which is free to interpret control characters and line endings, and
/// the encoding has neither — but a transport whose own value is already plain
/// text can write it directly, since only it has to read it back.
pub fn wrap_announcement(payload: &str) -> String {
    format!(
        "{}{payload}{}",
        String::from_utf8_lossy(ANNOUNCE_OPEN),
        ANNOUNCE_CLOSE as char
    )
}

/// The value between the delimiters, for a transport reading its own announcement
/// back.
///
/// `None` for a sequence that is not an announcement at all, which is what makes
/// this safe to hand to every transport in turn: the ones that did not write it can
/// say so without inspecting anything else.
pub fn strip_announcement(magic: &str) -> Option<&str> {
    let bytes = magic.as_bytes();
    let inner = bytes.strip_prefix(ANNOUNCE_OPEN)?;
    let inner = inner.strip_suffix(&[ANNOUNCE_CLOSE])?;
    std::str::from_utf8(inner).ok()
}

/// A future that a transport returns, boxed so it can be a trait method.
///
/// Only the transport's own `async fn`s need this. The stream itself does not:
/// `AsyncRead`/`AsyncWrite` are poll-based and already object-safe.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A bidirectional byte stream.
///
/// A blanket implementation, never written by hand. It exists so that
/// `Box<dyn Stream>` reads as one type instead of three bounds.
pub trait Stream: AsyncRead + AsyncWrite + Send + Unpin {}

impl<T: AsyncRead + AsyncWrite + Send + Unpin> Stream for T {}

/// One message, framed on a plain byte stream.
///
/// A stream has no message boundaries, so the reader has to be told how much to
/// read: one tag byte and a body, with the body's length in a 4-byte prefix.
/// [`Message::encode`] alone would rely on the write boundary surviving, which is
/// true of a datagram and false of `SOCK_STREAM`.
///
/// Generic over the stream rather than taking a link, so a test with one half of
/// a pair can drive it directly.
pub fn write_message<'a, S>(stream: &'a mut S, message: &Message) -> BoxFuture<'a, io::Result<()>>
where
    S: AsyncWrite + Send + Unpin + ?Sized,
{
    // Encoded now rather than inside the future: the message is only needed for
    // its bytes, and taking them up front means the returned future borrows the
    // stream alone.
    let body = message.encode();
    Box::pin(async move {
        let len = (body.len() as u32).to_be_bytes();
        stream.write_all(&len).await?;
        stream.write_all(&body).await?;
        stream.flush().await
    })
}

/// The other half of [`write_message`].
pub fn read_message<S>(stream: &mut S) -> BoxFuture<'_, io::Result<Option<Message>>>
where
    S: AsyncRead + Send + Unpin + ?Sized,
{
    Box::pin(async move {
        let mut len = [0u8; 4];
        match stream.read_exact(&mut len).await {
            Ok(_) => {}
            // A clean close between messages is the end of a client, not a
            // corruption: nothing was half-read.
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }
        let len = u32::from_be_bytes(len) as usize;
        let mut body = vec![0u8; len];
        stream.read_exact(&mut body).await?;
        Ok(Some(Message::decode(&body)?))
    })
}

/// A link between two ends, and which of the three states it is in.
///
/// The states exist because a connection is *negotiated*, not just opened. A
/// client learns it should connect from an advert, which names a transport; the
/// transport is what knows how to dial it. So the request carries the transport
/// rather than a bare address, and dialing is a step that can fail on its own.
///
/// ```text
///   Requested { transport, advert }   asked for, not yet dialed
///          │  .connect()
///          ▼
///   Connected { stream, transport, peer }
///          │  .close()
///          ▼
///   Closed
/// ```
///
/// Reading and writing work only in `Connected`; anything else is an error, not a
/// silent no-op. A caller that writes to a link that was never dialed has a bug,
/// and it should hear about it rather than have bytes vanish.
pub enum Connection {
    /// Asked for, not dialed. Holds the transport that will dial it.
    ///
    /// `dyn Transport` rather than a name or an address: the transport is the
    /// only thing that knows how to reach its own kind of peer, and looking it up
    /// again by name later would let the two disagree.
    Requested {
        transport: Arc<dyn Transport>,
        advert: Advert,
    },
    /// Dialed and usable.
    Connected {
        stream: Box<dyn Stream>,
        transport: &'static str,
        peer: String,
    },
    /// Ended. Terminal: a closed link is not reopened, a new one is requested.
    Closed,
}

/// Shows the state and, when connected, where it came from.
///
/// Never the stream's contents.
impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Requested { transport, advert } => f
                .debug_struct("Connection::Requested")
                .field("transport", &transport.name())
                .field("advert", advert)
                .finish(),
            Self::Connected {
                transport, peer, ..
            } => f
                .debug_struct("Connection::Connected")
                .field("transport", transport)
                .field("peer", peer)
                .finish_non_exhaustive(),
            Self::Closed => f.write_str("Connection::Closed"),
        }
    }
}

impl Connection {
    /// A link that has been asked for but not dialed.
    pub fn requested(transport: Arc<dyn Transport>, advert: Advert) -> Self {
        Self::Requested { transport, advert }
    }

    /// A link that is up.
    pub fn connected(
        stream: impl Stream + 'static,
        transport: &'static str,
        peer: impl Into<String>,
    ) -> Self {
        Self::Connected {
            stream: Box::new(stream),
            transport,
            peer: peer.into(),
        }
    }

    /// Dial a requested link, yielding a connected one.
    ///
    /// The transport does the work; this only moves between states. Connecting an
    /// already-connected link returns it unchanged rather than redialing, so a
    /// caller need not track which state it is in.
    pub async fn connect(self) -> io::Result<Self> {
        match self {
            Self::Requested { transport, advert } => {
                let dialed = transport.connect(&advert).await?;
                // The transport must hand back something connected: a
                // `Requested` from `connect` would be a transport that returned
                // the work it was asked to do.
                match dialed {
                    Self::Connected { .. } => Ok(dialed),
                    other => Err(io::Error::other(format!(
                        "{} returned {other:?} instead of a connected link",
                        transport.name()
                    ))),
                }
            }
            // Already dialed: nothing to do, and redialing would drop the stream.
            other => Ok(other),
        }
    }

    /// The transport that will dial this link, if it has not been dialed yet.
    ///
    /// Cloned rather than taken, because the link still needs it to dial. A
    /// caller that must keep sending *after* the dial wants its own handle:
    /// [`Transport::send`] and [`Transport::recv`] live on the transport, and a
    /// connected link no longer carries it.
    pub fn dialing_transport(&self) -> Option<Arc<dyn Transport>> {
        match self {
            Self::Requested { transport, .. } => Some(transport.clone()),
            _ => None,
        }
    }

    /// End the link. Idempotent, and safe on a link that never connected.
    pub fn close(&mut self) {
        *self = Self::Closed;
    }

    pub fn is_connected(&self) -> bool {
        matches!(self, Self::Connected { .. })
    }

    /// The transport that produced this link, once there is one.
    ///
    /// `None` while requested: the transport is known then too, but it is the one
    /// that *will* dial, not the one that did, and reporting it as provenance
    /// would claim a connection that does not exist yet.
    pub fn transport(&self) -> Option<&'static str> {
        match self {
            Self::Connected { transport, .. } => Some(transport),
            _ => None,
        }
    }

    /// Who is on the other end, for logs. Not an address: a peer-to-peer link has
    /// a key rather than a host, and a socket's path is not what a user acts on.
    pub fn peer(&self) -> Option<&str> {
        match self {
            Self::Connected { peer, .. } => Some(peer),
            _ => None,
        }
    }

    /// The stream, or an error naming the state that has none.
    fn stream(&mut self) -> io::Result<&mut Box<dyn Stream>> {
        match self {
            Self::Connected { stream, .. } => Ok(stream),
            Self::Requested { .. } => Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "the link was requested but never dialed",
            )),
            Self::Closed => Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "the link is closed",
            )),
        }
    }
}

impl AsyncRead for Connection {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut().stream() {
            Ok(stream) => Pin::new(&mut **stream).poll_read(cx, buf),
            Err(e) => Poll::Ready(Err(e)),
        }
    }
}

impl AsyncWrite for Connection {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        match self.get_mut().stream() {
            Ok(stream) => Pin::new(&mut **stream).poll_write(cx, buf),
            Err(e) => Poll::Ready(Err(e)),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut().stream() {
            Ok(stream) => Pin::new(&mut **stream).poll_flush(cx),
            Err(e) => Poll::Ready(Err(e)),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut().stream() {
            Ok(stream) => Pin::new(&mut **stream).poll_shutdown(cx),
            Err(e) => Poll::Ready(Err(e)),
        }
    }
}

/// What a transport promises.
///
/// One struct rather than a method per question, because a caller that has to
/// choose between channels needs the whole picture at once, and a
/// transport-by-transport table elsewhere would drift from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// Both ends run in one process.
    pub in_process: bool,
    /// Traffic never leaves this machine.
    pub local: bool,
    /// Opaque to anyone without the peer's key.
    pub encrypted: bool,
    /// Establishing the link may need a third-party relay.
    pub may_relay: bool,
    /// Told to the user when this transport is the one that worked.
    ///
    /// `None` means the user does not need to know. Anything surprising
    /// (traffic leaving the machine, a relay in the path) sets this, because a
    /// user should choose a surprising route rather than discover one.
    pub warn: Option<&'static str>,
}

impl Capabilities {
    /// Lower is better. Derived, so adding a transport needs no ordering table
    /// anywhere else.
    pub fn rank(&self) -> u8 {
        match (self.in_process, self.local, self.may_relay) {
            (true, _, _) => 0,
            (_, true, _) => 1,
            (_, _, false) => 2,
            (_, _, true) => 3,
        }
    }
}

/// How a transport says it can be reached.
///
/// `params` is open on purpose: a socket needs a path, a peer-to-peer transport
/// needs a key and possibly a relay, and neither should force the other's
/// vocabulary into a shared struct.
///
/// `transport` is owned rather than `&'static str` because an advert travels: it
/// is written into a pane's output and read back by a different process, and a
/// name that came from a stream cannot borrow from a constant. A
/// [`Transport::name`] is still `&'static str`: that side is compile-time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advert {
    /// The [`Transport::name`] this describes.
    pub transport: String,
    pub params: BTreeMap<String, String>,
}

impl Advert {
    pub fn new(transport: impl Into<String>) -> Self {
        Self {
            transport: transport.into(),
            params: BTreeMap::new(),
        }
    }

    pub fn with(mut self, key: &str, value: impl Into<String>) -> Self {
        self.params.insert(key.to_string(), value.into());
        self
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.params.get(key).map(String::as_str)
    }

    /// The lines this advert becomes.
    ///
    /// `transport=` first, then that transport's own keys. The order is not
    /// cosmetic: the reader treats `transport=` as the start of a new advert, so
    /// a param emitted before it would attach to the previous one.
    pub fn lines(&self) -> String {
        let mut body = format!("transport={}\n", self.transport);
        for (key, value) in &self.params {
            body.push_str(&format!("{key}={value}\n"));
        }
        body
    }

    /// Parse the same lines back into an advert.
    ///
    /// One parser for the announcement a client emits and any advert a caller
    /// holds, so the two halves cannot describe a transport differently.
    pub fn parse(text: &str) -> Option<Self> {
        let mut lines = text.lines().filter(|line| !line.trim().is_empty());
        let transport = lines.next()?.split_once('=')?;
        if transport.0.trim() != "transport" {
            return None;
        }
        let mut advert = Self::new(transport.1.trim());
        for line in lines {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            advert
                .params
                .insert(key.trim().to_string(), value.trim().to_string());
        }
        Some(advert)
    }
}

/// A transport: the factory that produces [`Connection`]s, and the framing they
/// carry.
///
/// `listen` returns `Option` because not every transport can accept, and which of
/// `connect`/`listen` a side actually uses is that side's business: the trait
/// describes what a channel *can* do, not what this end of it is for.
pub trait Transport: Send + Sync {
    /// Stable key. Matched against an [`Advert`], and how a caller selects this
    /// transport by name.
    fn name(&self) -> &'static str;

    /// What this transport promises.
    fn capabilities(&self) -> Capabilities;

    /// What this transport calls itself on the peer's screen.
    ///
    /// A label, not an address: the peer shows it so a user can tell one session
    /// from another, so it has to be short, readable, and distinct.
    fn label(&self) -> String;

    /// How this transport says it can be reached.
    ///
    /// `None` means there is nothing to announce, which makes the transport
    /// undialable: a channel nothing can find is a channel with no peer. A
    /// transport that exists must therefore answer this.
    fn advert(&self) -> Option<Advert>;

    /// The bytes this transport writes into a pane so a peer can find it.
    ///
    /// **The transport's own**, both halves: it builds the value here and it
    /// parses one back with [`Transport::from_magic`]. Nothing outside either
    /// looks inside, so a transport is free to put anything in there — a unix
    /// socket puts a path (one channel for every instance, distinguished by the
    /// id in the advert), while a transport with several channels puts whatever
    /// it needs to tell them apart.
    ///
    /// A string rather than bytes because it goes into a pane, which is a text
    /// channel by the time anything sees it.
    fn to_magic(&self) -> String;

    /// The transport a peer's announcement describes, if it is one of ours.
    ///
    /// The other half of [`Transport::to_magic`], and the only place an
    /// announcement is understood. `magic` is exactly what was written into the
    /// pane, delimiters and all, because only the transport knows where its own
    /// value starts and ends.
    ///
    /// Returns a transport rather than a bare address because the instance is what
    /// carries the routing: two peers can announce the same kind of channel and
    /// still have to be told apart, and whatever distinguishes them is written
    /// here and nowhere else.
    ///
    /// `None` for an announcement that is not this transport's, which is not an
    /// error: a peer may be announcing a channel we do not speak, and a scanner
    /// has to try the others.
    fn from_magic(magic: &str) -> Option<Self>
    where
        Self: Sized;

    /// Dial the peer an advert describes.
    fn connect<'a>(&'a self, advert: &'a Advert) -> BoxFuture<'a, io::Result<Connection>>;

    /// Start accepting peers, if this transport can.
    ///
    /// `Ok(None)` means "this transport is dial-only", which is a fact about the
    /// transport rather than a failure.
    fn listen<'a>(&'a self) -> BoxFuture<'a, io::Result<Option<Listener>>>;

    /// Send one message over `link`.
    ///
    /// On the transport rather than on [`Connection`] because framing is a
    /// property of how the bytes travel, not of the link's state: a transport
    /// that is a plain byte stream frames one way, and one that is a sequence of
    /// requests frames another. A `Connection` is only ever the enum.
    ///
    /// One message per call, so the peer always has a whole one to read.
    fn send<'a>(&'a self, link: &'a mut Connection, message: &Message)
        -> BoxFuture<'a, io::Result<()>>;

    /// Receive one message, blocking until a whole one arrives.
    ///
    /// `None` means the peer closed, which is the normal end of a client rather
    /// than an error.
    fn recv<'a>(&'a self, link: &'a mut Connection) -> BoxFuture<'a, io::Result<Option<Message>>>;
}

/// An accepting side of a transport.
pub struct Listener {
    inner: Box<dyn Accept>,
    transport: &'static str,
}

impl Listener {
    pub fn new(inner: impl Accept + 'static, transport: &'static str) -> Self {
        Self {
            inner: Box::new(inner),
            transport,
        }
    }

    pub fn transport(&self) -> &'static str {
        self.transport
    }

    /// Wait for the next peer.
    pub fn accept(&self) -> BoxFuture<'_, io::Result<Connection>> {
        self.inner.accept()
    }
}

/// The boxable half of a [`Listener`].
///
/// A trait object needs one method with no generic parameters, so the concrete
/// listeners implement this and `Listener` wraps it.
pub trait Accept: Send + Sync {
    fn accept(&self) -> BoxFuture<'_, io::Result<Connection>>;
}

/// An in-process pair of streams, for a session with both halves in one program.
///
/// Here rather than beside an implementation because there is no channel and no
/// address: it is two ends of one buffer, and the only thing it needs from this
/// module is [`Stream`].
pub fn in_process(capacity: usize) -> (impl Stream, impl Stream) {
    tokio::io::duplex(capacity)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use crate::message::Message;

    /// Bytes written at one end come out the other, unmodified.
    ///
    /// The property the whole abstraction exists to preserve: whatever carries
    /// them, the caller sees a byte stream. Asserted through `Connection`, so the
    /// delegate impls are exercised too, not just the inner stream.
    #[tokio::test]
    async fn bytes_cross_a_connection_unchanged() {
        let (a, b) = in_process(1024);
        let mut b = Connection::connected(b, "test", "other");
        let mut a = Connection::connected(a, "test", "self");
        // Not newline-terminated and not text: a transport that normalized either
        // would show up here.
        let input: &[u8] = &[0x00, 0xff, 0x1b, b'[', 0x9b, 0x07];
        a.write_all(input).await.unwrap();
        a.flush().await.unwrap();

        let mut got = vec![0u8; input.len()];
        b.read_exact(&mut got).await.unwrap();
        assert_eq!(got, input);
    }

    /// Framing puts one whole message at a time on a stream, and reads it back.
    ///
    /// A stream has no message boundaries, so the property to pin down is that two
    /// messages in one write still arrive as two, with nothing of one inside the
    /// other.
    #[tokio::test]
    async fn framed_messages_survive_being_packed_together() {
        let (a, b) = in_process(1024);
        let mut sender = Connection::connected(a, "test", "self");
        let mut receiver = Connection::connected(b, "test", "other");

        // Written back to back with no yield between them, so the reader sees one
        // buffer holding both.
        write_message(&mut sender, &Message::Ping("a".into()))
            .await
            .unwrap();
        write_message(&mut sender, &Message::Ping("b".into()))
            .await
            .unwrap();

        assert_eq!(
            read_message(&mut receiver).await.unwrap(),
            Some(Message::Ping("a".into()))
        );
        assert_eq!(
            read_message(&mut receiver).await.unwrap(),
            Some(Message::Ping("b".into()))
        );
    }

    /// A peer that closes between messages reads as the end of a client, not a
    /// corruption.
    #[tokio::test]
    async fn a_clean_close_reads_as_none() {
        let (a, b) = in_process(64);
        let mut receiver = Connection::connected(b, "test", "other");
        drop(a);
        assert_eq!(read_message(&mut receiver).await.unwrap(), None);
    }

    /// Ranking is derived from capabilities, so the order is not a table to keep
    /// in sync by hand.
    #[test]
    fn rank_orders_in_process_first_and_relayed_last() {
        let memory = caps(true, true, false);
        let unix = caps(false, true, false);
        let direct = caps(false, false, false);
        let relayed = caps(false, false, true);

        assert!(memory.rank() < unix.rank());
        assert!(unix.rank() < direct.rank());
        assert!(direct.rank() < relayed.rank());
    }

    /// An advert and its lines agree, whichever direction they are read in.
    ///
    /// The client writes these lines into its output and the server reads them
    /// back, so a round trip that lost a param would be a client that announced
    /// something other than what it listens on.
    #[test]
    fn adverts_round_trip_through_their_own_lines() {
        let advert = Advert::new("unix")
            .with("socket", "/tmp/tuigui.sock")
            .with("session", "3f9a1c2b");
        let parsed = Advert::parse(&advert.lines()).expect("its own lines parse");
        assert_eq!(parsed, advert);
    }

    /// A body that is not an advert is rejected rather than guessed at.
    #[test]
    fn a_body_that_is_not_an_advert_is_rejected() {
        assert!(Advert::parse("").is_none());
        assert!(Advert::parse("socket=/tmp/x\n").is_none(), "no transport=");
        // `transport=` last is not an advert either: the first line is the name.
        assert!(Advert::parse("socket=/tmp/x\ntransport=unix\n").is_none());
    }

    /// A stub transport, so the state machine is tested without a real channel.
    struct Stub {
        caps: Capabilities,
        /// How many times `connect` was called, to prove a dial really happened.
        attempts: Arc<AtomicUsize>,
        works: bool,
    }

    impl Transport for Stub {
        fn name(&self) -> &'static str {
            "stub"
        }
        fn capabilities(&self) -> Capabilities {
            self.caps
        }
        fn label(&self) -> String {
            "stub : 00000000".into()
        }
        fn advert(&self) -> Option<Advert> {
            Some(Advert::new("stub"))
        }
        fn to_magic(&self) -> String {
            wrap_announcement("stub")
        }
        fn from_magic(magic: &str) -> Option<Self> {
            (strip_announcement(magic) == Some("stub")).then(|| Self {
                caps: caps(false, true, false),
                attempts: Arc::new(AtomicUsize::new(0)),
                works: true,
            })
        }
        fn connect<'a>(&'a self, _advert: &'a Advert) -> BoxFuture<'a, io::Result<Connection>> {
            Box::pin(async move {
                self.attempts.fetch_add(1, Ordering::SeqCst);
                if !self.works {
                    return Err(io::Error::other("stub refused"));
                }
                let (ours, _theirs) = in_process(64);
                Ok(Connection::connected(ours, "stub", "stub"))
            })
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

    fn stub(works: bool) -> (Arc<Stub>, Arc<AtomicUsize>) {
        let attempts = Arc::new(AtomicUsize::new(0));
        (
            Arc::new(Stub {
                caps: caps(false, true, false),
                attempts: attempts.clone(),
                works,
            }),
            attempts,
        )
    }

    /// A requested link dials into a connected one.
    #[tokio::test]
    async fn a_requested_link_becomes_connected_when_dialed() {
        let (transport, attempts) = stub(true);
        let requested = Connection::requested(transport, Advert::new("stub"));
        assert!(!requested.is_connected(), "a request is not a link yet");

        let connected = requested.connect().await.expect("the stub accepts");
        assert!(connected.is_connected());
        assert_eq!(connected.transport(), Some("stub"));
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    /// A link that has not been dialed exposes the transport that will dial it,
    /// so a caller can keep it to frame the messages it is about to send.
    #[test]
    fn a_requested_link_hands_out_the_transport_that_will_dial_it() {
        let (transport, _) = stub(true);
        let requested = Connection::requested(transport, Advert::new("stub"));
        assert_eq!(
            requested.dialing_transport().map(|t| t.name()),
            Some("stub")
        );

        // A closed link has no dialing left to do, and says so rather than
        // claiming the transport that produced it.
        let mut closed = Connection::Closed;
        assert!(closed.dialing_transport().is_none());
        closed.close();
        assert!(closed.dialing_transport().is_none());
    }

    /// Dialing a link that is already up does not redial it.
    ///
    /// Redialing would replace the stream, dropping whatever was buffered on the
    /// one the caller already holds. A caller should not have to know which state
    /// it is in to be safe.
    #[tokio::test]
    async fn connecting_a_connected_link_is_a_no_op() {
        let (transport, attempts) = stub(true);
        let connected = Connection::requested(transport, Advert::new("stub"))
            .connect()
            .await
            .expect("first dial");
        let again = connected.connect().await.expect("second connect is free");
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            1,
            "the transport must not be asked twice"
        );
        assert!(again.is_connected());
    }

    /// A dial that fails leaves no link behind.
    #[tokio::test]
    async fn a_failed_dial_does_not_produce_a_link() {
        let (transport, _) = stub(false);
        let requested = Connection::requested(transport, Advert::new("stub"));
        let err = requested.connect().await.expect_err("the stub refuses");
        assert_eq!(err.kind(), io::ErrorKind::Other);
    }

    /// Reading and writing a link that is not connected is an error, not a no-op.
    ///
    /// A caller that writes into a link that was never dialed has a bug. Silently
    /// discarding the bytes would hide it.
    #[tokio::test]
    async fn io_on_an_unconnected_link_is_an_error() {
        let (transport, _) = stub(true);

        let mut requested = Connection::requested(transport, Advert::new("stub"));
        let err = requested
            .write_all(b"nobody is listening")
            .await
            .expect_err("a request has no stream");
        assert_eq!(err.kind(), io::ErrorKind::NotConnected);

        let mut closed = Connection::Closed;
        let err = closed
            .write_all(b"nobody is listening")
            .await
            .expect_err("a closed link has no stream");
        assert_eq!(err.kind(), io::ErrorKind::NotConnected);
        assert!(!closed.is_connected());
    }

    /// Closing is idempotent and works on a link that never connected.
    #[test]
    fn closing_is_idempotent() {
        let mut link = Connection::Closed;
        link.close();
        assert!(!link.is_connected());
        assert_eq!(link.transport(), None);
        assert_eq!(link.peer(), None);
    }

    /// The delimiters are what a scanner looks for, and the value between them is
    /// the transport's own.
    ///
    /// Framing only: what goes inside is a transport's business, so the shape to
    /// pin down is that the delimiters are there and can be peeled off again.
    #[test]
    fn an_announcement_round_trips_through_the_delimiters() {
        let payload = "anything the transport likes";
        let wrapped = wrap_announcement(payload);
        assert!(wrapped.starts_with("\x1b]30001;"), "the opener comes first");
        assert_eq!(wrapped.as_bytes().last(), Some(&ANNOUNCE_CLOSE));
        assert_eq!(strip_announcement(&wrapped), Some(payload));
    }

    /// Something that is not an announcement has no value to peel off.
    ///
    /// This is what makes it safe to offer every transport the same bytes: one
    /// that did not write them can say so without looking any further.
    #[test]
    fn a_sequence_that_is_not_an_announcement_is_rejected() {
        assert_eq!(strip_announcement("hello"), None);
        assert_eq!(strip_announcement("\x1b]30001;unterminated"), None);
        // The opener without the terminator, and the terminator without the opener.
        assert_eq!(strip_announcement("\x1b]30001;"), None);
        assert_eq!(strip_announcement("\x07"), None);
    }

    fn caps(in_process: bool, local: bool, may_relay: bool) -> Capabilities {
        Capabilities {
            in_process,
            local,
            encrypted: !local,
            may_relay,
            warn: None,
        }
    }
}
