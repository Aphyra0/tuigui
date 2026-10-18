//! The conversation between a server and its client.
//!
//! Deliberately small and separate from [`crate::proto`], which is the frame
//! encoding: this is the *shape* of the exchange — what either side may say —
//! and it should be readable in one screen. `proto` carries the bulk; this
//! carries the state.
//!
//! Three things can be said:
//!
//! ```text
//!   server ──▶ Ping(label)   the link is up: here is who you are talking to
//!   client ──▶ Pong          I got it, and this session is one I am part of
//!   server ──▶ Frame         one frame of whatever the peer asked to see
//!   either ──▶ Error         something failed; the peer decides what to do
//! ```
//!
//! The pair is a handshake, not a formality. `Ping` is how the client learns its
//! request was *accepted* — a different fact from its socket being open, since a
//! transport can connect and the session still refuse it — and it carries the
//! label so the client can say which session it landed in. `Pong` is what makes
//! the server's side of the link *established*: a client that never answers is
//! attached but not part of the session, and the server can tell the two apart.

use std::io;

/// One message between a server and its client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// The server accepted this client and it may start.
    ///
    /// Carries the transport's own label for the link ("unix-socket : 3f9a1c2b"),
    /// which the client shows so a user can tell one session from another. It is
    /// a label, not an address: the server writes what its transport calls itself
    /// rather than reformatting anything here.
    Ping(String),
    /// The client's answer, once it is drawing.
    ///
    /// Says nothing itself: the fact that a `Pong` arrived is the whole content,
    /// and it is what tells the server this client is really in the session rather
    /// than a connection that went quiet.
    Pong,
    /// One frame, in whatever the transport carries.
    ///
    /// Opaque here on purpose: framing belongs to [`crate::proto`], and a
    /// transport that had to understand a frame's internals could not carry one
    /// it had never heard of.
    Frame(Vec<u8>),
    /// Something failed.
    ///
    /// Carries a string rather than an [`io::Error`] because the peer that
    /// receives it is on another machine, and an `ErrorKind` from *there* would
    /// name a condition that does not exist *here*. The text is for a human.
    Error(String),
}

impl Message {
    /// The bytes to put on the wire.
    ///
    /// One leading tag byte, then the body. A tag rather than a length prefix
    /// because there are four cases and no nesting: a reader needs to know which
    /// of them it is holding before it knows how to read the rest.
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Self::Ping(label) => tagged(TAG_PING, label.as_bytes()),
            Self::Pong => vec![TAG_PONG],
            Self::Frame(body) => tagged(TAG_FRAME, body),
            Self::Error(text) => tagged(TAG_ERROR, text.as_bytes()),
        }
    }

    /// Parse the bytes of one message, tag and all.
    ///
    /// Takes the whole message, not a slice of a stream: a partial one is not a
    /// message, and returning `Ok(None)` for it here would make every caller
    /// re-implement the same "am I holding enough yet" logic.
    pub fn decode(bytes: &[u8]) -> io::Result<Self> {
        let (tag, body) = bytes
            .split_first()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "empty message"))?;
        match *tag {
            TAG_PING => Ok(Self::Ping(utf8(body))),
            TAG_PONG => Ok(Self::Pong),
            TAG_FRAME => Ok(Self::Frame(body.to_vec())),
            TAG_ERROR => Ok(Self::Error(utf8(body))),
            other => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown message tag {other:#04x}"),
            )),
        }
    }
}

const TAG_PING: u8 = b'P';
const TAG_PONG: u8 = b'p';
const TAG_FRAME: u8 = b'F';
const TAG_ERROR: u8 = b'E';

/// One tag byte, then the body.
fn tagged(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + body.len());
    out.push(tag);
    out.extend_from_slice(body);
    out
}

/// A text body, read leniently.
///
/// Lossy rather than strict: a body is produced by the peer's own encoder, so
/// bytes that are not UTF-8 mean the peer is not ours, and a readable line beats
/// dropping the message. The client would otherwise have nothing to show.
fn utf8(body: &[u8]) -> String {
    String::from_utf8_lossy(body).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every variant survives a round trip, body and all.
    #[test]
    fn messages_round_trip() {
        for message in [
            Message::Ping("unix-socket : 3f9a1c2b".into()),
            // An empty label is not the same as no label: the tag must still
            // survive, or a client would read it as a frame.
            Message::Ping(String::new()),
            Message::Pong,
            Message::Frame(b"raw frame bytes".to_vec()),
            // Not text, so a codec that assumed UTF-8 would show up here.
            Message::Frame(vec![0x00, 0xff, 0x1b, 0x9b]),
            Message::Error("the peer refused the frame".into()),
            Message::Error(String::new()),
        ] {
            let bytes = message.encode();
            let back = Message::decode(&bytes).expect("encoded messages decode");
            assert_eq!(back, message, "round trip lost something");
        }
    }

    /// The variants are distinguishable, which is the whole point of the tag.
    #[test]
    fn variants_do_not_collide() {
        let ping = Message::Ping(String::new()).encode();
        let pong = Message::Pong.encode();
        let frame = Message::Frame(b"".to_vec()).encode();
        let error = Message::Error(String::new()).encode();
        assert_ne!(ping, pong);
        assert_ne!(pong, frame);
        assert_ne!(frame, error);
        // A frame whose body happens to be a tag byte must not read as that
        // variant: the tag is only the first byte, never the body.
        let tricky = Message::Frame(vec![TAG_PING]).encode();
        assert_eq!(
            Message::decode(&tricky).unwrap(),
            Message::Frame(vec![TAG_PING])
        );
        // A label that repeats a tag byte is a label, not a second message.
        let label = Message::Ping(String::from_utf8(vec![TAG_FRAME]).unwrap()).encode();
        assert_eq!(
            Message::decode(&label).unwrap(),
            Message::Ping(String::from_utf8(vec![TAG_FRAME]).unwrap())
        );
    }

    /// A truncated or unknown message is an error, not a guess.
    #[test]
    fn malformed_messages_are_rejected() {
        assert!(Message::decode(&[]).is_err(), "empty input has no tag");
        assert!(
            Message::decode(&[0xff, 0x01]).is_err(),
            "an unknown tag must not be guessed at"
        );
    }
}
