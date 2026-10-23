//! The transport seam: how a client and a server reach each other, and everything done
//! on that link.
//!
//! A transport **is** the thing a caller talks to. It is not a factory that hands out
//! something else to talk to: it holds its own connection, so `send` and `try_recv` need
//! no argument and a caller that has one has everything — the codec, the address, and
//! whatever state the channel keeps between messages.
//!
//! The two sides are peers, not a client and a server in the protocol sense. Either may
//! dial and either may accept, and which one does is the choice of whoever builds it: a
//! session between two processes on one machine needs no more than a socket at a known
//! path, and neither end has to be told which role it plays.

use std::future::Future;
use std::io;
use std::pin::Pin;

use bytes::{BufMut as _, BytesMut};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::codec::{Decoder, Encoder, FramedRead, FramedWrite};

use crate::message::Message;

/// What separates a transport's name from its address in an announcement.
///
/// An announcement is a transport's own name, this separator, and the address to dial:
///
/// ```text
///   unix : /run/user/1000/tuigui.sock
/// ```
///
/// Plain text, and not an escape sequence, because of what is above us. A multiplexer is a
/// terminal emulator as well as a relay: it parses a pane's output into a grid of cells
/// and re-draws that grid, so a sequence it does not model is consumed and never reaches
/// the real terminal. A private OSC announcement dies there. Text does not: it becomes
/// cells, and cells are what a multiplexer draws.
pub const ANNOUNCE_SEPARATOR: &str = " : ";

/// The longest address an announcement may carry.
///
/// A cap because the search runs over a stream nobody controls, and an address is short by
/// construction: a socket path, a pane id. Past it the candidate is dropped, which loses
/// one announcement and keeps the process alive.
pub const ANNOUNCE_MAX_LEN: usize = 1024;

/// An announcement, in the one place its format is defined.
pub fn announcement(name: &str, address: &str) -> String {
    format!("{name}{ANNOUNCE_SEPARATOR}{address}")
}

/// The text a scanner looks for to find `name`'s announcement.
///
/// Including the separator is what keeps a name that is also an ordinary word from
/// matching: only the name *followed by the separator* starts an announcement.
pub fn announcement_prefix(name: &str) -> String {
    announcement(name, "")
}

/// A future a transport returns, boxed so it can be a trait method.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A bidirectional byte stream.
///
/// A blanket implementation, never written by hand. It exists so that `Box<dyn Stream>`
/// reads as one type instead of three bounds.
pub trait Stream: AsyncRead + AsyncWrite + Send + Unpin {}

impl<T: AsyncRead + AsyncWrite + Send + Unpin> Stream for T {}

/// The longest message that will be read off a link.
///
/// A cap because a length prefix is a number the peer chose, and a peer that is
/// wrong — or not a peer at all — must not be able to ask for gigabytes with four
/// bytes. A frame is one encoded picture, so tens of megabytes is far past what a
/// session legitimately sends; past it the link is desynchronized anyway.
pub const MAX_MESSAGE_LEN: usize = 32 * 1024 * 1024;

/// The codec that turns a byte stream into messages and back.
///
/// Four bytes of big-endian length, then the message's rkyv archive. Reassembly
/// is the codec library's job: bytes arrive in pieces, and [`Decoder::decode`]
/// is asked only with what has actually arrived, so a stalled peer holds nothing.
/// The length cap is what keeps a corrupt prefix from being obeyed.
pub struct MessageCodec;

impl Encoder<Message> for MessageCodec {
    type Error = io::Error;

    #[tracing::instrument(name = "msg.encode", skip_all)]
    fn encode(&mut self, message: Message, dst: &mut BytesMut) -> io::Result<()> {
        let body = message
            .to_bytes()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("a message would not serialize: {e}")))?;
        dst.reserve(4 + body.len());
        dst.put_u32(body.len() as u32);
        dst.put_slice(&body);
        Ok(())
    }
}

impl Decoder for MessageCodec {
    type Item = Message;
    type Error = io::Error;

    #[tracing::instrument(name = "msg.decode", skip_all)]
    fn decode(&mut self, src: &mut BytesMut) -> io::Result<Option<Message>> {
        if src.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_be_bytes(src[..4].try_into().expect("checked above")) as usize;
        if len > MAX_MESSAGE_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("a peer announced a {len} byte message"),
            ));
        }
        if src.len() < 4 + len {
            return Ok(None);
        }
        let frame = src.split_to(4 + len);
        Message::from_bytes(&frame[4..]).map(Some).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("a {len} byte message did not decode: {e}"),
            )
        })
    }
}

/// One half of a byte stream, reading whole messages.
pub fn framed_read<S: AsyncRead>(stream: S) -> FramedRead<S, MessageCodec> {
    FramedRead::new(stream, MessageCodec)
}

/// One half of a byte stream, writing whole messages.
pub fn framed_write<S: AsyncWrite>(stream: S) -> FramedWrite<S, MessageCodec> {
    FramedWrite::new(stream, MessageCodec)
}

/// A link to one peer: the transport carrying it, and who is at the other end.
///
/// A struct rather than an enum with states. The states existed because dialing is a step
/// that can fail on its own; that step belongs to [`Transport::connect`], which includes
/// the handshake, so what a caller gets back is a link that is *established* — there is no
/// half-dialed link to pass around and nothing for a state machine to describe.
///
/// Sending and receiving go through the transport, which holds the codec and any
/// routing: a socket writes a length prefix, a transport carrying several peers decides
/// which one a message is for. Nothing here decides either.
pub struct Connection {
    transport: Box<dyn Transport>,
}

impl Connection {
    pub fn new(transport: impl Transport + 'static) -> Self {
        Self {
            transport: Box::new(transport),
        }
    }

    /// The transport carrying this link.
    pub fn transport(&self) -> &dyn Transport {
        &*self.transport
    }

    /// Send one message.
    pub async fn send(&self, message: &Message) -> io::Result<()> {
        self.transport.send(message).await
    }

    /// Read one message, waiting for a whole one.
    ///
    /// `None` means the peer closed between messages, which is the normal end of one.
    pub async fn recv(&self) -> io::Result<Option<Message>> {
        self.transport.recv().await
    }
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The transport and where it goes, never the bytes.
        f.debug_struct("Connection")
            .field("transport", &self.transport.name())
            .field("address", &self.transport.address())
            .finish()
    }
}

/// A transport: one channel, and everything a caller does over it.
///
/// A transport that carries more than one peer — an ssh connection multiplexing several
/// clients — puts the routing inside itself. That is why sending and receiving are methods
/// here rather than on the link: they have to consult the routing, and the routing is the
/// transport's.
pub trait Transport: Send + Sync {
    /// Stable key. Matched against an announcement, and how a caller selects this
    /// transport by name.
    fn name(&self) -> &'static str;

    /// Where this transport is reachable, as the text that goes in an announcement.
    ///
    /// `None` means it cannot be announced, which makes it unreachable: a channel nothing
    /// can find is a channel with no peer.
    fn address(&self) -> Option<String>;

    /// The announcement this transport writes so a peer can find it.
    ///
    /// The transport's own, both halves: it builds the text and reads one back with
    /// [`Transport::from_magic`]. Nothing outside looks inside.
    fn to_magic(&self) -> Option<String> {
        self.address().map(|address| announcement(self.name(), &address))
    }

    /// The transport a peer's announcement describes, if it is one of ours.
    ///
    /// The other half of [`Transport::to_magic`], and the only place an announcement is
    /// understood. `None` for a name that is not ours, which is not an error: most output
    /// belongs to the command, and a scanner offers the same name to every transport.
    fn from_magic(name: &str, address: &str) -> Option<Self>
    where
        Self: Sized;

    /// Connect to a peer, and establish the link.
    ///
    /// The whole handshake, not just a socket: this dials, greets with a [`Message::Ping`],
    /// and waits for the [`Message::Pong`] answer. Only a link that came back established is
    /// returned, so a caller gets either a peer it can talk to or the reason it cannot —
    /// never a link that is up but unproven, which is a state every caller would otherwise
    /// have to check for.
    ///
    /// The greeting is here and not in the caller because it is not optional: a peer that
    /// never answers is not a peer, and the half that owns the channel is the half that knows
    /// how to wait on it.
    fn connect(&self) -> BoxFuture<'_, io::Result<Connection>>;

    /// Wait for a peer to connect, and establish the link.
    ///
    /// The other end of [`Transport::connect`], and its mirror: this waits for the greeting,
    /// answers it with a [`Message::Pong`], and returns only a link that has been established.
    /// A caller gets a peer that has proven itself or the reason there is none.
    ///
    /// The answering is here for the same reason the greening is there: a channel whose
    /// greeting was never answered is a channel the other end is still waiting on, and the
    /// half that holds the link is the half that must not leave it hanging.
    fn accept(&self) -> BoxFuture<'_, io::Result<Connection>>;

    /// Send one message.
    ///
    /// One message per call, so the peer always has a whole one to read.
    ///
    /// The message gets its own lifetime rather than sharing `&self`'s: the returned future
    /// borrows both, and one `'_` for two inputs is the ambiguity that makes an implementor's
    /// `async move` fail to name what it returns.
    fn send<'a>(&'a self, message: &'a Message) -> BoxFuture<'a, io::Result<()>>;

    /// Read one message, waiting for a whole one.
    ///
    /// **Blocking**, and that is the point rather than an oversight: a peer has to *send* a
    /// message, so a read that returns the instant it is asked asks before the peer has had a
    /// chance to write. There is no delay to poll around — a message either arrives or the
    /// peer is gone — so this waits, and a caller that cannot wait is the one that is wrong.
    ///
    /// `Ok(None)` means the peer closed between messages, which is the normal end of one.
    fn recv(&self) -> BoxFuture<'_, io::Result<Option<Message>>>;
}

/// An in-process pair of streams, for a session with both halves in one program.
///
/// Here and not in a transport because there is no channel and no address: it is two ends
/// of one buffer.
pub fn in_process(capacity: usize) -> (impl Stream, impl Stream) {
    tokio::io::duplex(capacity)
}

