//! Finding a client's announcement in the command's output.
//!
//! This is the rendezvous half of the design: the client cannot be dialed before
//! the server knows a channel to dial, so it says so through the only channel that
//! always exists — the pane's own output.
//!
//! The scanner does **not** own that output: the pty does, and hands out a view of
//! it ([`crate::bytes::Bytes`]). The server subscribes, so the scanner is the
//! server's and owns its own carry. The pty is the producer and never learns that
//! a scan exists.
//!
//! The framing is [`wire::transport`]'s: the delimiters and the payload encoding
//! live with the channel's shape, not with either side, because a framing two
//! halves disagree about is a client that silently never connects.
//!
//! # What this does and does not do
//!
//! It **notices**; it does not consume. The view hands the same slice onward, so
//! the announcement reaches the terminal like any other OSC. Stripping it is a
//! separate decision and is not made here: it belongs in the output path where the
//! write happens, not in a scanner that cannot modify what it is given.

use std::sync::mpsc::Sender;

use wire::transport::{Connection, ANNOUNCE_CLOSE, ANNOUNCE_MAX_PAYLOAD, ANNOUNCE_OPEN};

use crate::transport::for_magic;

/// Watches the command's output for an announcement.
pub struct Announcement {
    /// Bytes of a possible announcement that have not resolved yet.
    ///
    /// Holds a partial opener, or a complete opener and a partial payload. Only
    /// ever one at a time, because an announcement cannot nest.
    carry: Vec<u8>,
    /// Where a found link is offered.
    ///
    /// A `std::sync::mpsc::Sender` rather than tokio's: the view is called from
    /// the pty's blocking forwarding thread, which is not a task and has no
    /// runtime to await on. `try_send` is what keeps that thread off the network.
    found: Sender<Connection>,
}

impl Announcement {
    pub fn new(found: Sender<Connection>) -> Self {
        Self {
            carry: Vec::new(),
            found,
        }
    }

    /// Feed one chunk of the command's output.
    ///
    /// Must not block and must not allocate per chunk: it runs on the thread that
    /// forwards every byte the command writes.
    pub fn feed(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            // Byte at a time, because an announcement can split across chunks at
            // any offset, including inside the opener. A chunk-at-a-time scan
            // would miss every split one.
            self.push(byte);
        }
    }

    fn push(&mut self, byte: u8) {
        if self.carry.is_empty() && byte != ANNOUNCE_OPEN[0] {
            // The overwhelmingly common case: ordinary output. Returning here is
            // what keeps the hot path to one comparison.
            return;
        }
        self.carry.push(byte);

        if self.carry.len() <= ANNOUNCE_OPEN.len() {
            // Still possibly the opener.
            if !ANNOUNCE_OPEN.starts_with(&self.carry) {
                self.reset();
            }
            return;
        }

        if byte == ANNOUNCE_CLOSE {
            let whole = String::from_utf8_lossy(&self.carry).into_owned();
            self.reset();
            self.deliver(&whole);
            return;
        }

        if self.carry.len() > ANNOUNCE_OPEN.len() + ANNOUNCE_MAX_PAYLOAD {
            // Unterminated and unbounded. Dropping it loses this announcement;
            // keeping it would lose the process.
            self.reset();
        }
    }

    /// Hand a found announcement to the server.
    ///
    /// `send` on a std channel does not block, which is what keeps this thread off
    /// the network: dialing happens on the server's own task, and a full channel
    /// costs one client rather than stalling the command's output.
    fn deliver(&self, magic: &str) {
        match self.decode(magic) {
            Some(connection) => {
                if self.found.send(connection).is_err() {
                    eprintln!("tui: the server is gone; dropping a client");
                }
            }
            None => eprintln!("tui: could not decode an announcement; ignoring"),
        }
    }

    /// Turn a whole announcement into a link, by asking the transports whether
    /// they recognize it.
    ///
    /// The scanner does **not** look inside: it found the delimiters, and what is
    /// between them belongs to whichever transport wrote it. Each is asked in turn
    /// and the one that answers is the one that knows how to dial it, which is why
    /// a transport may put anything it likes in its own announcement.
    ///
    /// `None` for an announcement naming a transport we do not have, which is not
    /// an error: there is nothing to dial, so there is nothing to offer.
    fn decode(&self, magic: &str) -> Option<Connection> {
        let transport = for_magic(magic)?;
        let advert = transport.advert()?;
        eprintln!("tui: a client announced itself over {}", transport.name());
        Some(Connection::requested(transport.into(), advert))
    }

    fn reset(&mut self) {
        self.carry.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wire::transport::Transport as _;

    /// What a scanner saw, without a running server.
    ///
    /// The scanner's own channel, not the server's: what is being tested is the
    /// finding, and the server is not on this path until a link is in hand.
    fn scan(chunks: &[&[u8]]) -> Vec<Connection> {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut announcement = Announcement::new(tx);
        for chunk in chunks {
            announcement.feed(chunk);
        }
        drop(announcement);
        rx.try_iter().collect()
    }

    /// An announcement a client would write, framed by the transport that owns it.
    ///
    /// Through [`InfluxByPath::to_magic`] rather than a literal: the value inside
    /// the delimiters is the transport's own, so a test that spelled it out would
    /// keep passing after the transport changed, which is exactly the failure it
    /// exists to catch. The socket in the path is the same one [`UNIX`] names.
    fn encoded(advert: &str) -> Vec<u8> {
        let transport = crate::transport::InfluxByPath::new("/tmp/tuigui.sock");
        // Sanity: the test's own advert and the transport's must describe the same
        // channel, or the assertions below would be about two different things.
        assert_eq!(transport.advert().unwrap().lines(), advert);
        transport.to_magic().into_bytes()
    }

    /// A whole unix advert, which is what a real announcement carries.
    const UNIX: &str = "transport=unix\nsocket=/tmp/tuigui.sock\n";

    /// Ordinary output is not an announcement.
    #[test]
    fn plain_output_is_ignored() {
        assert!(scan(&[b"hello, world\n"]).is_empty());
        // An OSC that is not ours must not match either: the dispatch number is
        // what makes this namespace ours.
        assert!(scan(&[b"\x1b]0;title\x07"]).is_empty());
    }

    /// An announcement split across reads is still found.
    ///
    /// This is the property that matters and the reason the scan is byte-at-a-
    /// time: a pty delivers arbitrary chunks, so an announcement can split at
    /// any offset, including inside the opener. A chunk-at-a-time scan would miss
    /// every split one, and the client would silently never connect.
    #[test]
    fn an_announcement_split_across_chunks_is_found() {
        let whole = encoded(UNIX);
        // Every split point, not a sample: a scanner that only handles the
        // common one is a scanner that fails occasionally.
        for cut in 1..whole.len() {
            let (a, b) = whole.split_at(cut);
            let seen = scan(&[a, b]);
            assert_eq!(seen.len(), 1, "a split at {cut} must still be found");
        }
    }

    /// The opener alone is not an announcement, and a partial opener is not lost
    /// when the rest arrives.
    #[test]
    fn a_partial_opener_is_held_until_it_resolves() {
        // The first byte of the opener, then ordinary text: not an announcement.
        assert!(scan(&[&ANNOUNCE_OPEN[..1], b"not an announcement"]).is_empty());
        // The whole opener in one chunk, then a real payload in the next: the
        // scanner must keep the opener rather than discard it for being alone.
        let whole = encoded(UNIX);
        let (opener, rest) = whole.split_at(ANNOUNCE_OPEN.len());
        assert_eq!(scan(&[opener, rest]).len(), 1);
    }

    /// A sequence that never terminates is dropped rather than accumulated.
    ///
    /// The carry is fed by a stream we do not control, so an unterminated opener
    /// followed by a flood would otherwise grow it until memory runs out.
    #[test]
    fn an_unterminated_announcement_is_bounded() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut announcement = Announcement::new(tx);
        announcement.feed(ANNOUNCE_OPEN);
        // Far more than the cap, with no terminator anywhere.
        let flood = vec![b'A'; ANNOUNCE_MAX_PAYLOAD * 2];
        announcement.feed(&flood);
        assert!(
            announcement.carry.len() <= ANNOUNCE_MAX_PAYLOAD + ANNOUNCE_OPEN.len(),
            "the carry must stay bounded, was {}",
            announcement.carry.len()
        );
    }

    /// The announcement becomes a link naming the transport that recognized it,
    /// with the announcer's own advert.
    ///
    /// This is the property the matching exists for: `transport=unix` has to turn
    /// into something that can dial a unix socket, not into a name that a
    /// different layer looks up again.
    #[test]
    fn an_announcement_becomes_a_link_for_the_transport_that_recognized_it() {
        let seen = scan(&[&encoded(UNIX)]);
        assert_eq!(seen.len(), 1, "a whole announcement must produce a link");
        let transport = seen[0]
            .dialing_transport()
            .expect("a find carries the transport that will dial it");
        assert_eq!(transport.name(), "unix");
        // The label names the path from the announcement, which is the only address
        // the server was told.
        assert_eq!(transport.label(), "unix : /tmp/tuigui.sock");
    }

    /// An announcement for a transport we do not have produces no link.
    ///
    /// Not an error: there is nothing to dial, so there is nothing for the
    /// server to do. A link with a transport it cannot use would fail later and
    /// read as a client that broke.
    #[test]
    fn an_announcement_for_an_unknown_transport_is_ignored() {
        let unknown = "transport=nonexistent\nsocket=/tmp/x\n";
        let framed = wire::transport::wrap_announcement(&base64_turbo::encode(unknown));
        assert!(scan(&[framed.as_bytes()]).is_empty());
        // Malformed is the same case: an announcement whose value is not an advert.
        let junk = wire::transport::wrap_announcement("not an advert");
        assert!(scan(&[junk.as_bytes()]).is_empty());
    }
}
