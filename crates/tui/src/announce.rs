//! Finding a client's announcement in the command's output.
//!
//! This is the rendezvous half of the design: the client cannot be dialed before
//! the server knows a channel to dial, so it says so through the only channel that
//! always exists — the pane's own output.
//!
//! The scanner does **not** own that output: the pty does, and hands out a view of
//! it ([`crate::bytes::Bytes`]). The server subscribes, so the scanner is the
//! server's and owns its own carry. The pty is the producer and never learns that a
//! scan exists.
//!
//! # Why the announcement is plain text
//!
//! ```text
//!   unix : /run/user/1000/tuigui-1234.sock
//! ```
//!
//! It was a private OSC sequence first, which is unambiguously ours and invisible on
//! screen. That is wrong for the case this exists for. When a multiplexer owns the
//! terminal, a pane's output is not forwarded to the real terminal — it is parsed by
//! the multiplexer's own terminal emulator and re-drawn from the screen model. A
//! sequence the emulator does not model is consumed and dropped, so the announcement
//! never reaches anything reading the pane. Text is not dropped: it becomes cells,
//! and cells are what the multiplexer re-draws.
//!
//! So the announcement is text, and it is *visible*. That is the honest form of the
//! trade: it can be spoofed by a command that prints the same text, in exchange for
//! working behind the thing this whole design exists to work behind.
//!
//! # Why it is a search, and not a line
//!
//! The bytes arriving here are a *rendered* stream, not a log. A client drawing its
//! pane moves the cursor with escapes and never emits a newline — so "read a line,
//! then parse it" finds the announcement glued to the end of a whole screen dump,
//! and no line ever contains just the announcement. What is reliable is the text
//! itself: a name, the separator, and an address. So the scan looks for the
//! separator and reads what follows, whatever is around it.

use std::sync::mpsc::Sender;

use wire::transport::{announcement_prefix, Connection, ANNOUNCE_MAX_LEN};

use crate::transport::for_magic;

/// Watches the command's output for an announcement.
pub struct Announcement {
    /// The end of the window that may still contain a separator.
    ///
    /// A separator can split across two reads, so the last few bytes of a chunk are
    /// kept and examined again with the next one. Bounded, because an announcement
    /// is short: anything older than that cannot be part of one.
    carry: Vec<u8>,
    /// Where a found link is offered.
    ///
    /// A `std::sync::mpsc::Sender` rather than tokio's: the view is called from the
    /// pty's blocking forwarding thread, which is not a task and has no runtime to
    /// await on. `send` on the standard channel does not block, which is what keeps
    /// that thread off the network.
    found: Sender<Connection>,
    /// The names to look for, longest first.
    ///
    /// Held rather than looked up per byte: this runs on the path of every byte the
    /// command writes, and a scan that asked a registry each time would allocate per
    /// chunk.
    names: Vec<&'static str>,
    /// The announcements already turned into links.
    ///
    /// An announcement is written once, but the text stays on screen and every
    /// redraw puts it out again — the same client, the same address, several times
    /// per second if it is redrawing. Dialing each sighting would connect to one
    /// client over and over, so a sighting of an address already dialed is the same
    /// client and is ignored.
    ///
    /// Only the addresses, and never removed: a client that reconnects announces the
    /// same address, and re-dialing a socket that is already served would add a
    /// second link to the same peer.
    dialed: Vec<String>,
}

impl Announcement {
    /// A scanner for the transports this build can dial.
    pub fn new(found: Sender<Connection>) -> Self {
        let mut names = crate::transport::names();
        // Longest first, so a name that is a prefix of another does not match the
        // shorter one's address.
        names.sort_by_key(|name| std::cmp::Reverse(name.len()));
        Self {
            carry: Vec::new(),
            found,
            names,
            dialed: Vec::new(),
        }
    }

    /// Feed one chunk of the command's output.
    ///
    /// Should not block and must not allocate per chunk: it runs on the thread that
    /// forwards every byte the command writes.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.carry.extend_from_slice(bytes);
        self.scan();
        self.trim();
    }

    /// Look for `<name> : <address>` anywhere in what is held.
    ///
    /// At most one announcement per call: one is what a client writes, and doing the
    /// rest on the next chunk keeps this to a single pass with nothing borrowed
    /// across the mutation that follows.
    fn scan(&mut self) {
        let Some((name, address, consumed)) = self.find_announcement() else {
            return;
        };
        // Consumed whether or not it is dialed: a redraw puts the same text out
        // again, and what matters is whether the address has been dialed, not
        // whether the text is still on screen.
        self.carry.drain(..consumed);
        if self.dialed.iter().any(|seen| *seen == address) {
            // The same client, seen again. Not a second one, and not an error.
            return;
        }
        self.dialed.push(address.clone());
        self.deliver(&name, &address);
    }

    /// The first announcement in the carry, with how much of it that accounts for.
    ///
    /// `None` for text that is not an announcement, and for one whose address has
    /// not arrived in full: the caller keeps the carry either way, and the rest
    /// arrives with the next chunk.
    fn find_announcement(&self) -> Option<(String, String, usize)> {
        let text = std::str::from_utf8(&self.carry).ok()?;
        for name in &self.names {
            let prefix = announcement_prefix(name);
            let Some(start) = text.find(&prefix) else {
                continue;
            };
            let rest = &text[start + prefix.len()..];
            let Some((address, end)) = take_address(rest) else {
                continue;
            };
            return Some(((*name).to_string(), address.to_string(), start + prefix.len() + end));
        }
        None
    }

    /// Turn a name and address into a link, and offer it.
    fn deliver(&mut self, name: &str, address: &str) {
        let Some(transport) = for_magic(name, address) else {
            return;
        };
        let Some(advert) = transport.advert() else {
            return;
        };
        eprintln!("tui: a client announced itself over {}", transport.name());
        if self
            .found
            .send(Connection::requested(transport.into(), advert))
            .is_err()
        {
            eprintln!("tui: the server is gone; dropping a client");
        }
    }

    /// Drop everything that a separator could no longer straddle.
    ///
    /// The window only has to reach back as far as the longest prefix plus the
    /// longest address, so an announcement cannot be missed by trimming. What is
    /// dropped is the command's own output, which is most of it.
    fn trim(&mut self) {
        let keep = self
            .names
            .iter()
            .map(|name| announcement_prefix(name).len())
            .max()
            .unwrap_or(0)
            + ANNOUNCE_MAX_LEN;
        if self.carry.len() > keep {
            let from = self.carry.len() - keep;
            self.carry.drain(..from);
        }
    }
}

/// Peel one address off the front of `text`.
///
/// An address ends at the first byte that cannot be part of one. That is not the
/// same as the end of a line: a rendered pane positions rows with escapes and never
/// emits a newline, so an announcement sits inside a run of padding and cursor
/// moves. Reading to a newline swallows all of it, and the address becomes a few
/// hundred bytes of spaces — which is exactly how a valid path came to be rejected
/// for being longer than a socket path can be.
///
/// So the address is the run of non-blank, non-control characters after the
/// separator, and anything else ends it: the renderer's padding, a carriage return,
/// an escape, a newline.
///
/// `None` when no address has arrived yet, which has to be told apart from an empty
/// one: a chunk that ends just after the separator has the rest in the next chunk.
fn take_address(text: &str) -> Option<(&str, usize)> {
    let end = text
        .find(|c: char| c.is_whitespace() || c.is_control())
        .unwrap_or(text.len());
    if end == 0 {
        return None;
    }
    // A run this long is not an address. Giving up keeps the carry bounded on a
    // stream nobody controls.
    if end > ANNOUNCE_MAX_LEN {
        return None;
    }
    Some((&text[..end], end))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wire::transport::Transport as _;

    /// What a scanner saw, without a running server.
    fn scan(chunks: &[&[u8]]) -> Vec<Connection> {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut announcement = Announcement::new(tx);
        for chunk in chunks {
            announcement.feed(chunk);
        }
        drop(announcement);
        rx.try_iter().collect()
    }

    /// The text a client writes for itself, from the transport that owns it.
    ///
    /// Through [`crate::transport::InfluxByPath::to_magic`] rather than a literal:
    /// the text is the transport's own, so a test that spelled it out would keep
    /// passing after the transport changed, which is exactly the failure it exists to
    /// catch.
    fn announced() -> Vec<u8> {
        crate::transport::InfluxByPath::new("/tmp/tuigui.sock")
            .to_magic()
            .into_bytes()
    }

    /// Ordinary output is not an announcement.
    #[test]
    fn plain_output_is_ignored() {
        assert!(scan(&[b"hello, world\n"]).is_empty());
        // A colon that is not the separator, and a name without one.
        assert!(scan(&[b"note:/tmp/something\n"]).is_empty());
        assert!(scan(&[b"unix\n"]).is_empty());
        // An article using the transport's name as an ordinary word.
        assert!(scan(&[b"the unix way of sockets\n"]).is_empty());
    }

    /// An announcement split across reads is still found.
    ///
    /// This is the property that matters: a pty delivers arbitrary chunks, so one can
    /// split at any offset, including inside the separator. A chunk-at-a-time scan
    /// would miss every split one, and the client would silently never connect.
    #[test]
    fn an_announcement_split_across_chunks_is_found() {
        let whole = announced();
        for cut in 1..whole.len() {
            let (a, b) = whole.split_at(cut);
            let seen = scan(&[a, b]);
            assert_eq!(seen.len(), 1, "a split at {cut} must still be found");
        }
    }

    /// An announcement at the end of a rendered screen is found.
    ///
    /// This is the real shape of the input: a client that draws its pane emits cursor
    /// moves and colours with no newline anywhere, so the announcement is glued to the
    /// end of a whole screen dump. A scanner that required a line to itself would find
    /// nothing, which is exactly what happened.
    #[test]
    fn an_announcement_after_a_rendered_screen_is_found() {
        let mut stream = Vec::new();
        // Rows drawn by cursor positioning, as a terminal UI does.
        for row in 1..=3 {
            stream.extend_from_slice(format!("\x1b[{row};1H\x1b[39;48;2;255;105;180m").as_bytes());
            stream.extend_from_slice(&vec![b' '; 80]);
        }
        // No newline before the announcement, because the renderer never wrote one.
        stream.extend_from_slice(&announced());

        let seen = scan(&[&stream]);
        assert_eq!(seen.len(), 1, "the announcement must be found after a screen");
    }

    /// The same announcement twice on one stream is one client, not two.
    ///
    /// A client writes its announcement once, but the bytes around it are re-read as
    /// the window slides. Finding it twice would dial twice.
    #[test]
    fn a_second_sighting_is_not_a_second_client() {
        let mut twice = announced();
        twice.extend_from_slice(&announced());
        assert_eq!(scan(&[&twice]).len(), 1);
    }

    /// An overlong run without a terminator is bounded.
    ///
    /// The carry is fed by a stream nobody controls, so a command printing without
    /// any break would otherwise grow it until memory runs out.
    #[test]
    fn an_overlong_line_is_bounded() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut announcement = Announcement::new(tx);
        announcement.feed(b"unix : ");
        announcement.feed(&vec![b'A'; ANNOUNCE_MAX_LEN * 4]);
        assert!(
            announcement.carry.len() <= ANNOUNCE_MAX_LEN * 3,
            "the carry must stay bounded, was {}",
            announcement.carry.len()
        );
    }

    /// The announcement becomes a link naming the transport that recognized it.
    ///
    /// This is the property the matching exists for: the text has to turn into
    /// something that can dial, not into a name a different layer looks up again.
    #[test]
    fn an_announcement_becomes_a_link() {
        let seen = scan(&[&announced()]);
        assert_eq!(seen.len(), 1, "a whole announcement must produce a link");
        let transport = seen[0]
            .dialing_transport()
            .expect("a find carries the transport that will dial it");
        assert_eq!(transport.name(), "unix");
        assert_eq!(transport.label(), "unix : /tmp/tuigui.sock");
    }

    /// The address ends at the renderer's padding, not at the end of the stream.
    ///
    /// This is the bug that made a valid path undialable. A pane is drawn as fixed
    /// rows: the announcement is followed by spaces to the edge of the area, then a
    /// cursor move, then the next row, and there is no newline anywhere. An address
    /// read to a newline is therefore the path plus hundreds of bytes of spaces, and
    /// the dial fails on a path that is far longer than a socket path can be.
    #[test]
    fn an_address_ends_at_the_renderers_padding() {
        let mut stream = announced();
        stream.extend_from_slice(&vec![b' '; 200]);
        stream.extend_from_slice(b"\x1b[24;1H");

        let seen = scan(&[&stream]);
        assert_eq!(seen.len(), 1, "the padded announcement must still be found");
        let transport = seen[0].dialing_transport().expect("a find carries one");
        assert_eq!(transport.label(), "unix : /tmp/tuigui.sock");
    }

    /// An announcement for a transport we do not have produces no link.
    ///
    /// Not an error: there is nothing to dial, so there is nothing for the server to
    /// do. A link with a transport it cannot use would fail later and read as a
    /// client that broke.
    #[test]
    fn an_announcement_for_an_unknown_transport_is_ignored() {
        assert!(scan(&[b"nonexistent : /tmp/x\n"]).is_empty());
    }
}
