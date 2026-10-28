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
//! Two of them are about more than one window. `InitClient` tells the server what
//! its compositor admitted: which process, and which cells it draws into. The tree
//! that grows from those facts is the server's own business, built by matching a
//! helper's parent pid. `CompositorRequest` is the way back down: the server asks
//! the compositor to act, and the client that owns it forwards the request.
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
    /// internals could not carry one it had never heard of. What it does name is
    /// the window the frame belongs to, because a session holds more than one and
    /// a decoder is per window: without the surface there is no way to know which
    /// stream this stretch continues. The pid names which client's tree to look in,
    /// which is the other half of the same lookup.
    Frame {
        /// The process whose program draws this window, and so whose stream this is.
        pid: u32,
        /// The cells of the window this frame draws.
        surface: SurfaceId,
        /// The codec's bytes, opaque to the link.
        bytes: Vec<u8>,
    },
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
    /// A request from the server to the compositor inside the client.
    ///
    /// The compositor is what owns the window tree, so anything that has to
    /// reach a window — input aimed at one, a configure, a close — is asked for
    /// here rather than decided by the server. The variant carries the request;
    /// the client forwards it and the compositor answers.
    CompositorRequest(CompositorAddressible),
    /// A client connected to the client's compositor: the program itself, or a
    /// helper it started.
    ///
    /// Sent when the client's first window is ready to be drawn, not when the
    /// connection opens: a client with no window is a process the server has nothing
    /// to say to, and reporting it earlier would only be a fact that changes a
    /// moment later.
    InitClient {
        /// The process id, which is what a helper is matched to its parent by.
        pid: u32,
        /// The signature this client's root window draws into.
        surface: SurfaceId,
        /// Where that window sits in the stacking order, higher in front.
        z: i32,
    },
    /// A client's program opened another toplevel, beside its root window.
    ///
    /// Not a sub-client: the window belongs to a connection the server already
    /// knows, and is addressed under it.
    NewWindow {
        /// The process whose program opened it.
        pid: u32,
        /// The signature this window draws into, and the name it is placed under.
        surface: SurfaceId,
        /// Where it sits in the stacking order, higher in front.
        z: i32,
    },
    /// A window closed. Its cells are gone, so nothing has to be said about those.
    DeleteWindow {
        /// The signature the window drew into.
        surface: SurfaceId,
    },
    /// A window moved in the stacking order.
    ///
    /// Sent instead of a new placement: the pixels did not change, only which
    /// window is in front. A terminal re-orders what it already has, so this costs
    /// no image traffic.
    Restack {
        /// The signature the window draws into.
        surface: SurfaceId,
        /// Its new place in the stacking order, higher in front.
        z: i32,
    },
}

/// The signature a window's cells carry: a symbol, a foreground and a background.
///
/// The cell *is* the identity, which is what makes a window findable by looking at
/// the screen rather than by being told where it is. Random in all three, so it
/// cannot be mistaken for ordinary content and two windows cannot collide by
/// accident.
///
/// `Hash` because it is a map key on both sides: the gui looks a window's layout up by
/// it, and the server looks a decoder up by it.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
pub struct SurfaceId {
    /// The character drawn in every cell of the window.
    pub symbol: char,
    /// The foreground those cells are drawn in.
    pub fg: (u8, u8, u8),
    /// The background behind them.
    pub bg: (u8, u8, u8),
}

impl SurfaceId {
    /// A surface id, made fresh.
    ///
    /// Random in all three fields so it cannot be mistaken for ordinary content and two
    /// windows cannot collide by accident. The entropy comes from `RandomState`, which the
    /// process seeds from the OS, mixed with a counter so two calls in the same nanosecond
    /// still differ.
    pub fn random() -> Self {
        use std::collections::hash_map::RandomState;
        use std::hash::{BuildHasher, Hasher};

        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u32(n);
        let a = hasher.finish();
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u64(a);
        let b = hasher.finish();

        // Glyphs that no program draws by accident: block elements and geometric shapes,
        // nowhere near the ASCII a terminal UI is made of.
        const SYMBOLS: &[char] = &[
            '░', '▒', '▓', '█', '▚', '▞', '◧', '◨', '◩', '◪', '⬒', '⬓', '⬔', '⬕', '▛', '▜',
        ];
        // Two independent colours, each drawn from its own hash. Deriving one from the
        // other — `rgb(b)` and `rgb(b >> 8)` — leaves their bytes overlapping, and a
        // signature whose halves share bytes is one another window's cells can match.
        SurfaceId {
            symbol: SYMBOLS[(a % SYMBOLS.len() as u64) as usize],
            fg: colour(a ^ 0x9e37_79b9_7f4a_7c15),
            bg: colour(b),
        }
    }

    /// Whether `cell` is one of this surface's cells.
    ///
    /// Both colours must be true colour to compare: a palette index names whatever
    /// the terminal has in that slot, which is not a fact a surface carries, so an
    /// indexed or default colour matches nothing.
    pub fn matches(&self, cell: &cell_map::Cell) -> bool {
        let cell_map::Cell {
            symbol,
            fg: cell_map::CellColor::Rgb(fr, fg_, fb),
            bg: cell_map::CellColor::Rgb(br, bg_, bb),
        } = *cell
        else {
            return false;
        };
        self.symbol == symbol && self.fg == (fr, fg_, fb) && self.bg == (br, bg_, bb)
    }
}

/// A colour from `seed`, kept away from the extremes so a surface never looks like a
/// plain default cell.
fn colour(seed: u64) -> (u8, u8, u8) {
    use std::hash::{BuildHasher as _, Hasher as _};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(seed);
    let value = hasher.finish().to_le_bytes();
    // 1..=254: never a pure black or a pure white, both of which read as "no colour".
    (value[0].clamp(1, 254), value[1].clamp(1, 254), value[2].clamp(1, 254))
}

/// A request from the server down to the compositor.
///
/// The channel exists before its content does: the server is the side that
/// knows what a user asked for, and the compositor is the side that can act on
/// it, so the shape of the request belongs here and the handling belongs there.
/// Nothing is handled yet — the first request is the one a window-targeted input
/// needs, and it is added when the routing that uses it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub enum CompositorAddressible {
    /// A request with nothing behind it, for the path to be built on.
    Noop,
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
            Message::Frame {
                pid: 4242,
                surface: SurfaceId {
                    symbol: '❋',
                    fg: (1, 2, 3),
                    bg: (4, 5, 6),
                },
                bytes: b"raw frame bytes".to_vec(),
            },
            // Not text, so a codec that assumed UTF-8 would show up here.
            Message::Frame {
                pid: 1,
                surface: SurfaceId {
                    symbol: '\u{10eeee}',
                    fg: (0, 0, 0),
                    bg: (255, 255, 255),
                },
                bytes: vec![0x00, 0xff, 0x1b, 0x9b],
            },
            Message::InitClient {
                pid: 4242,
                surface: SurfaceId {
                    symbol: '❋',
                    fg: (9, 8, 7),
                    bg: (6, 5, 4),
                },
                z: -1_073_741_824,
            },
            Message::NewWindow {
                pid: 4242,
                surface: SurfaceId {
                    symbol: '✳',
                    fg: (1, 1, 1),
                    bg: (2, 2, 2),
                },
                z: 7,
            },
            Message::DeleteWindow {
                surface: SurfaceId {
                    symbol: '✳',
                    fg: (1, 1, 1),
                    bg: (2, 2, 2),
                },
            },
            Message::Restack {
                surface: SurfaceId {
                    symbol: '✳',
                    fg: (1, 1, 1),
                    bg: (2, 2, 2),
                },
                z: 9,
            },
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
