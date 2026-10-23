//! The conversation between a server and its client.
//!
//! Deliberately small: this is the *shape* of the exchange — what either side may say —
//! and it should be readable in one screen. The bulk is a `Frame`; this carries the state.
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
//!
//! On the wire a message is its rkyv archive: the enum's own shape, serialized
//! and read back with validation. What used to be a hand-rolled tag byte is the
//! enum's variant index now, and nothing here describes how bytes travel — that
//! is the codec in [`crate::transport`].

use codecs::CodecId;
use rkyv::rancor;

/// One message between a server and its client.
#[derive(Debug, Clone, PartialEq, Eq, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
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
    /// Opaque here on purpose: a transport that had to understand a frame's
    /// internals could not carry one it had never heard of.
    Frame(Vec<u8>),
    /// Something failed.
    ///
    /// Carries a string rather than an [`io::Error`] because the peer that
    /// receives it is on another machine, and an `ErrorKind` from *there* would
    /// name a condition that does not exist *here*. The text is for a human.
    Error(String),
    /// The codec this session streams with, announced by the client that encodes.
    ///
    /// Sent once, right after the handshake: every `Frame` that follows is in
    /// that codec, and the decoder that reads them is built from this and kept.
    /// It exists so a client and a server agree on a codec without either
    /// assuming the other: a server that learns the codec here is one that can
    /// be written without knowing the client's source.
    InitStreaming {
        /// The codec every frame that follows is in.
        codec: CodecId,
    },
    /// The decoder on the server is gone; send a fresh keyframe.
    ///
    /// Sent by the server when its decoder can no longer follow the stream — a
    /// decode that failed, a reset. It asks for a picture a decoder that has
    /// lost all of its state can start from, which is a keyframe encoded now,
    /// not a replay of the last frame.
    ReinitStreaming,
}

impl Message {
    /// The bytes to put on the wire: this message's rkyv archive.
    ///
    /// # Errors
    ///
    /// Fails when rkyv cannot serialize the message, which a message made of a
    /// string and bytes does not do; the error is carried rather than swallowed
    /// because a serializer that fails is a bug worth naming.
    pub fn to_bytes(&self) -> Result<Vec<u8>, rancor::Error> {
        Ok(rkyv::to_bytes::<rancor::Error>(self)?.to_vec())
    }

    /// Parse the bytes of one message.
    ///
    /// Takes the whole message, not a slice of a stream: a partial one is not a
    /// message, and returning `Ok(None)` for it here would make every caller
    /// re-implement the same "am I holding enough yet" logic. That logic is the
    /// codec's now; this is what runs once the codec holds all of it.
    ///
    /// # Errors
    ///
    /// Fails when the bytes are not an archive of a message — garbage, a
    /// truncated one, or one from a peer speaking a different shape of this
    /// enum. Validation runs before the bytes are believed.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, rancor::Error> {
        rkyv::from_bytes::<Self, rancor::Error>(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every variant survives a round trip, body and all.
    #[test]
    fn messages_round_trip() {
        for message in [
            Message::Ping("unix-socket : 3f9a1c2b".into()),
            // An empty label is not the same as no label: the variant must still
            // survive, or a client would read it as a frame.
            Message::Ping(String::new()),
            Message::Pong,
            Message::Frame(b"raw frame bytes".to_vec()),
            // Not text, so a codec that assumed UTF-8 would show up here.
            Message::Frame(vec![0x00, 0xff, 0x1b, 0x9b]),
            Message::Error("the peer refused the frame".into()),
            Message::Error(String::new()),
        ] {
            let bytes = message.to_bytes().expect("messages serialize");
            let back = Message::from_bytes(&bytes).expect("archived messages decode");
            assert_eq!(back, message, "round trip lost something");
        }
    }

    /// The variants are distinguishable, which is the whole point of the archive's
    /// variant index.
    #[test]
    fn variants_do_not_collide() {
        let ping = Message::Ping(String::new()).to_bytes().expect("serializes");
        let pong = Message::Pong.to_bytes().expect("serializes");
        assert_ne!(ping, pong);
    }

    /// A truncated or garbage message is an error, not a guess.
    #[test]
    fn malformed_messages_are_rejected() {
        assert!(
            Message::from_bytes(&[]).is_err(),
            "empty input is not a message"
        );
        assert!(
            Message::from_bytes(&[0xff, 0x01]).is_err(),
            "bytes that are not an archive must be refused, not guessed at"
        );
        // A real message cut in half: the archive is complete-shaped, so a
        // truncated body is caught by validation rather than by chance.
        let mut full = Message::Ping("a label".into()).to_bytes().expect("serializes");
        full.truncate(full.len() / 2);
        assert!(
            Message::from_bytes(&full).is_err(),
            "half a message must not decode"
        );
    }
}
