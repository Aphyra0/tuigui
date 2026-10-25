//! Finding a client's announcement in the command's output.
//!
//! This is the rendezvous half of the design: the client cannot be dialed before the server
//! knows a channel to dial, so it says so through the only channel that always exists — the
//! pane's own output.
//!
//! The scan owns nothing. It holds the carry of what it has not resolved, and a caller
//! steps it with chunks and takes back whatever it found, which is what lets the server's
//! own loop do the scanning on the same task that waits on its clients.
//!
//! # Why the announcement is plain text
//!
//! ```text
//!   unix : /run/user/1000/tuigui.sock
//! ```
//!
//! It was a private OSC sequence first, which is unambiguously ours and invisible on
//! screen. That is wrong for the case this exists for. When a multiplexer owns the
//! terminal, a pane's output is not forwarded to the real terminal — it is parsed by the
//! multiplexer's own terminal emulator and re-drawn from the screen model. A sequence the
//! emulator does not model is consumed and dropped, so the announcement never reaches
//! anything reading the pane. Text is not dropped: it becomes cells, and cells are what the
//! multiplexer re-draws.
//!
//! So the announcement is text, and it is *visible*. That is the honest form of the trade:
//! it can be spoofed by a command that prints the same text, in exchange for working behind
//! the thing this whole design exists to work behind.
//!
//! # Why it is a search, and not a line
//!
//! The bytes arriving here are a *rendered* stream, not a log. A client drawing its pane
//! moves the cursor with escapes and never emits a newline, so "read a line, then parse it"
//! finds the announcement glued to the end of a whole screen dump, and no line ever contains
//! just the announcement. What is reliable is the text itself, so the scan looks for the
//! separator and reads what follows, whatever is around it.

use wire::transport::{announcement_prefix, Transport, ANNOUNCE_MAX_LEN};
use wire::unix::{self, UnixSocket};

/// Watches the command's output for an announcement.
pub struct Announcement {
    /// Text that may hold an announcement, and what has not been read of it yet.
    ///
    /// An announcement can split across two chunks at any offset, so the tail of a chunk is
    /// kept and examined again with the next one. Bounded, because an announcement is
    /// short: anything older cannot be part of one.
    carry: Vec<u8>,
    /// The names to look for, longest first.
    ///
    /// Held rather than looked up per byte: this runs on the path of every byte the command
    /// writes, and a scan that asked for the list each time would allocate per chunk.
    names: Vec<&'static str>,
    /// The addresses already handed over.
    ///
    /// An announcement is written once, but the text stays on screen and every redraw puts
    /// it out again — the same client, the same address, several times a second. Dialing
    /// each sighting would connect to one client over and over.
    seen: Vec<String>,
}

impl Announcement {
    /// A scanner for the transports this build can dial.
    pub fn new() -> Self {
        Self {
            carry: Vec::new(),
            names: unix::names(),
            seen: Vec::new(),
        }
    }

    /// Feed one chunk of the command's output, and take what it revealed.
    ///
    /// Must not block and must not allocate per chunk: this runs on the path of every byte
    /// the command writes. A transport it returns has not been dialed, so returning one
    /// costs a caller nothing it has to do now.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Box<dyn Transport>> {
        self.carry.extend_from_slice(bytes);
        let found = self.scan();
        self.trim();
        found
    }

    /// Look for `<name> : <address>` anywhere in what is held.
    ///
    /// At most one announcement per call: one is what a client writes, and the rest arrives
    /// on the next chunk.
    fn scan(&mut self) -> Vec<Box<dyn Transport>> {
        let Some((name, address, consumed)) = self.find_announcement() else {
            return Vec::new();
        };
        // Consumed whether or not it is dialed: a redraw puts the same text out again, and
        // what matters is whether the address has been seen, not whether the text is still
        // on screen.
        self.carry.drain(..consumed);
        if self.seen.iter().any(|previous| *previous == address) {
            // The same client, seen again. Not a second one, and not an error.
            return Vec::new();
        }
        self.seen.push(address.clone());
        match UnixSocket::from_magic(&name, &address) {
            Some(transport) => {
                tracing::info!("a client announced itself over {}", transport.name());
                vec![Box::new(transport) as Box<dyn Transport>]
            }
            None => Vec::new(),
        }
    }

    /// The first announcement in the carry, with how much of it that accounts for.
    ///
    /// `None` for text that is not an announcement, and for one whose address has not
    /// arrived in full: the caller keeps the carry either way, and the rest arrives with
    /// the next chunk.
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
            return Some((
                (*name).to_string(),
                address.to_string(),
                start + prefix.len() + end,
            ));
        }
        None
    }

    /// Drop everything a separator could no longer straddle.
    ///
    /// The window only has to reach back as far as the longest prefix plus the longest
    /// address, so an announcement cannot be missed by trimming. What is dropped is the
    /// command's own output, which is most of it.
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
/// An address ends at the first byte that cannot be part of one. That is not the same as the
/// end of a line: a rendered pane positions rows with escapes and never emits a newline, so
/// an announcement sits inside a run of padding and cursor moves. Reading to a newline would
/// swallow all of it, and the address would be a few hundred bytes of spaces — which is how
/// a valid path came to be rejected for being longer than a socket path can be.
///
/// So the address is the run of non-blank, non-control characters after the separator, and
/// anything else ends it: the renderer's padding, a carriage return, an escape, a newline.
///
/// `None` when no address has arrived yet, which has to be told apart from an empty one: a
/// chunk that ends just after the separator has the rest in the next chunk.
fn take_address(text: &str) -> Option<(&str, usize)> {
    let end = text
        .find(|c: char| c.is_whitespace() || c.is_control())
        .unwrap_or(text.len());
    if end == 0 {
        return None;
    }
    // A run this long is not an address. Giving up keeps the carry bounded on a stream
    // nobody controls.
    if end > ANNOUNCE_MAX_LEN {
        return None;
    }
    Some((&text[..end], end))
}
