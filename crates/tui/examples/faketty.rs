//! A terminal that answers kitty-graphics acks, for testing `tuigui`.
//!
//! `script(1)` cannot stand in for a terminal here. The graphics protocol is a
//! request/ack protocol: `easytgp` waits for the terminal's reply before it
//! releases the shared memory object backing a transmit, and a harness that only
//! captures bytes never sends one, so every `sync` blocks until its timeout and
//! every frame is reported as a failure.
//!
//! It also decodes the placeholder grid out of the output, which is the one
//! measurement that says whether an image swap reached the cells: a cell names
//! its image by the id encoded in its foreground color, so counting cells per id
//! shows which image the grid is actually pointing at.
//!
//! Usage:
//!   faketty <log> -- <command> [args...]
//!
//! Exit status is the command's. A summary goes to stdout.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io::{self, Read as _, Write as _};
use std::os::unix::ffi::OsStrExt as _;
use std::process::Command;
use std::sync::{Arc, Mutex};

fn main() {
    if let Err(e) = run() {
        eprintln!("faketty: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let Some(log_path) = args.next() else {
        return return_usage();
    };
    // The separator is required: without it a command's own flags would be read
    // as faketty's, and there is no way to tell them apart.
    if args.next().as_deref() != Some("--") {
        return return_usage();
    }
    let command: Vec<String> = args.collect();
    if command.is_empty() {
        return Err("no command given".into());
    }

    let ws = rustix::termios::tcgetwinsize(std::io::stdin()).unwrap_or(rustix::termios::Winsize {
        ws_row: 45,
        ws_col: 142,
        ws_xpixel: 0,
        ws_ypixel: 0,
    });
    let (mut master, slave) = open_pty(ws)?;

    let stdin = std::fs::File::open(&slave)?;
    let stdout = std::fs::OpenOptions::new().write(true).open(&slave)?;
    let stderr = std::fs::OpenOptions::new().write(true).open(&slave)?;
    let mut child = Command::new(&command[0])
        .args(&command[1..])
        .stdin(stdin)
        .stdout(stdout)
        .stderr(stderr)
        .spawn()?;

    let mut log = std::fs::File::create(&log_path)?;
    let terminal = Arc::new(Mutex::new(Terminal::default()));

    // One reader of the pty, which both logs and answers. Splitting those across
    // threads would let one thread's ack land inside another's escape.
    //
    // A second handle is kept out here for key injection: the reader owns the
    // master, and writing input is a different direction from reading output.
    let injector = master.try_clone()?;
    let reader_terminal = terminal.clone();
    let reader = std::thread::spawn(move || {
        let mut buf = [0u8; 16384];
        let mut pending: Vec<u8> = Vec::new();
        loop {
            let n = match master.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            let _ = log.write_all(&buf[..n]);
            let _ = log.flush();

            pending.extend_from_slice(&buf[..n]);
            let replies = reader_terminal.lock().unwrap().feed(&pending);
            if !replies.is_empty() {
                let _ = master.write_all(&replies);
                let _ = master.flush();
            }
            // Only whole escapes are consumed. An unterminated one stays for the
            // next read, because a sequence can be split across reads and parsing
            // a partial one eagerly would parse it wrong.
            let start = reader_terminal.lock().unwrap().unterminated_from(&pending);
            pending.drain(..start);
        }
    });

    // Optional key injection, so a test can exercise what the pty's *user* does.
    // `TUI_FAKE_SEND=<seconds>:<text>` writes `text` to the master after the
    // delay, which is how a quit key reaches a program that reads its own stdin.
    if let Ok(spec) = std::env::var("TUI_FAKE_SEND") {
        if let Some((secs, text)) = spec.split_once(':') {
            let secs: f64 = secs.parse().unwrap_or(2.0);
            let text = text.to_string();
            std::thread::sleep(std::time::Duration::from_secs_f64(secs));
            let mut master_w = injector;
            master_w.write_all(text.as_bytes())?;
            master_w.flush()?;
        }
    }
    let status = child.wait()?;
    // The reader ends when the pty closes, which is when the last holder of the
    // slave exits; a grandchild that inherited it would keep it open, so it is
    // not waited on. The summary is taken from what has been parsed so far.
    drop(reader);

    let summary = terminal.lock().unwrap().summary();
    println!("{summary}");
    match status.code() {
        Some(code) => println!("exit {code}"),
        None => println!("killed by signal"),
    }
    Ok(())
}

fn return_usage() -> Result<(), Box<dyn std::error::Error>> {
    Err("usage: faketty <log> -- <command> [args...]".into())
}

fn open_pty(ws: rustix::termios::Winsize) -> io::Result<(std::fs::File, std::path::PathBuf)> {
    let master = rustix::pty::openpt(
        rustix::pty::OpenptFlags::RDWR | rustix::pty::OpenptFlags::NOCTTY,
    )
    .map_err(io::Error::from)?;
    rustix::pty::grantpt(&master).map_err(io::Error::from)?;
    rustix::pty::unlockpt(&master).map_err(io::Error::from)?;
    let name = rustix::pty::ptsname(&master, Vec::new()).map_err(io::Error::from)?;
    rustix::termios::tcsetwinsize(&master, ws).map_err(io::Error::from)?;
    let path = std::path::PathBuf::from(OsStr::from_bytes(name.as_bytes()));
    Ok((std::fs::File::from(master), path))
}

/// A kitty-graphics terminal, reduced to what a test needs to observe.
struct Terminal {
    /// Image ids named by a transmit, in order, as sent.
    transmits: Vec<String>,
    /// Image ids named by a `a=p`, with the placement id they used.
    placements: Vec<String>,
    /// Placement commands that carried `U=1`, i.e. virtual placements.
    virtual_placements: usize,
    /// Cells per image id, decoded from the grid's foreground colors.
    cells: BTreeMap<u32, usize>,
    /// The foreground color in effect, as packed 24-bit RGB.
    fg: u32,
    /// Bytes of the trailing SGR state, so a cell split across reads still
    /// resolves against the color that preceded it.
    sgr: Vec<u8>,
    /// Bytes of a UTF-8 scalar in progress, to detect a placeholder cell.
    pending_utf8: Vec<u8>,
    /// The next image id this terminal would assign to a create.
    next_id: u32,
}

impl Default for Terminal {
    fn default() -> Self {
        Self {
            transmits: Vec::new(),
            placements: Vec::new(),
            virtual_placements: 0,
            cells: BTreeMap::new(),
            fg: 0,
            sgr: Vec::new(),
            pending_utf8: Vec::new(),
            // Ids start at 1, as kitty's do, so `i=0` is never a valid image.
            next_id: 1,
        }
    }
}

impl Terminal {
    /// Consume whole escapes from `input`, returning the acks to write back.
    fn feed(&mut self, input: &[u8]) -> Vec<u8> {
        let mut replies = Vec::new();
        let mut i = 0;
        while i < input.len() {
            if input[i] == 0x1b {
                match input.get(i + 1) {
                    Some(b'_') => {
                        let Some(len) = find(&input[i..], b"\x1b\\") else {
                            break; // unterminated, kept for the next read
                        };
                        let seq = input[i + 2..i + len].to_vec();
                        self.ack(&seq, &mut replies);
                        i += len + 2;
                    }
                    Some(b'[') => {
                        let Some(len) = find_csi_end(&input[i..]) else {
                            break;
                        };
                        let csi = input[i + 2..i + len].to_vec();
                        self.sgr(&csi);
                        i += len + 1;
                    }
                    _ => i += 1,
                }
            } else {
                self.text(input[i]);
                i += 1;
            }
        }
        replies
    }

    /// Offset of the first byte that is part of an unterminated escape.
    fn unterminated_from(&self, input: &[u8]) -> usize {
        let Some(start) = input.iter().position(|b| *b == 0x1b) else {
            return input.len();
        };
        let closed = match input.get(start + 1) {
            Some(b'_') => find(&input[start..], b"\x1b\\").is_some(),
            Some(b'[') => find_csi_end(&input[start..]).is_some(),
            // A lone ESC, or ESC followed by something else: one more byte
            // decides it, so it is left pending.
            _ => false,
        };
        if closed {
            input.len()
        } else {
            start
        }
    }

    /// Answer a graphics command, and record what it asked for.
    fn ack(&mut self, seq: &[u8], replies: &mut Vec<u8>) {
        let Some(body) = seq.strip_prefix(b"G") else {
            return;
        };
        let split = body.iter().position(|b| *b == b';').unwrap_or(body.len());
        let keys = parse_cds(&body[..split]);
        let get = |k: char| keys.iter().find(|(key, _)| *key == k).map(|(_, v)| *v);

        if get('a') == Some("p") {
            let img = get('i').unwrap_or("-").to_string();
            let p = get('p').unwrap_or("-").to_string();
            self.placements.push(format!("{img}/{p}"));
            if get('U') == Some("1") {
                self.virtual_placements += 1;
            }
        }

        // A create names an image *number* (`I=`), and the terminal answers with
        // the *id* it assigned (`i=99,I=13`); every other command names an id
        // and is echoed. Reproducing that split matters: it is what lets a
        // caller tell a create from a re-transmit.
        match (get('i'), get('I')) {
            (Some(id), _) => {
                self.transmits.push(format!("i={id}"));
                replies.extend_from_slice(format!("\x1b_Gi={id};OK\x1b\\").as_bytes());
            }
            (None, Some(number)) => {
                if get('a') == Some("q") {
                    // The marker query names its own marker, not an image, and
                    // is not a create.
                    self.transmits.push(format!("q={number}"));
                    replies.extend_from_slice(format!("\x1b_Gi={number};OK\x1b\\").as_bytes());
                    return;
                }
                let id = self.next_id;
                self.next_id += 1;
                self.transmits.push(format!("I={number}->i={id}"));
                replies.extend_from_slice(
                    format!("\x1b_Gi={id},I={number};OK\x1b\\").as_bytes(),
                );
            }
            (None, None) => {}
        }
    }

    /// Track the SGR foreground, which is where a placeholder cell's image id
    /// lives: `ESC [ 38 ; 2 ; r ; g ; b m`.
    fn sgr(&mut self, csi: &[u8]) {
        self.sgr.extend_from_slice(csi);
        let Some(params) = std::str::from_utf8(&self.sgr).ok() else {
            return;
        };
        let Some(rest) = params.strip_prefix("38;2;") else {
            self.sgr.clear();
            return;
        };
        let mut it = rest.split(';');
        let (Some(r), Some(g), Some(b)) = (it.next(), it.next(), it.next()) else {
            return; // truncated, keep accumulating
        };
        if let (Ok(r), Ok(g), Ok(b)) = (r.parse::<u32>(), g.parse::<u32>(), b.parse::<u32>()) {
            self.fg = (r << 16) | (g << 8) | b;
        }
        self.sgr.clear();
    }

    /// Count a placeholder cell, which is a four-byte UTF-8 scalar.
    fn text(&mut self, byte: u8) {
        self.pending_utf8.push(byte);
        if let Some(id) = self.placeholder_id() {
            *self.cells.entry(id).or_default() += 1;
        }
    }

    /// The id of a completed placeholder scalar, if the bytes so far are one.
    fn placeholder_id(&mut self) -> Option<u32> {
        // `U+10EEEE` is `f4 8e bb ae`. The diacritics that follow are separate
        // scalars, so matching the four bytes is enough to identify a cell.
        const PLACEHOLDER: [u8; 4] = [0xf4, 0x8e, 0xbb, 0xae];
        if self.pending_utf8.ends_with(&PLACEHOLDER) {
            self.pending_utf8.clear();
            // The id is in the low 24 bits of the color. Which channel holds
            // which byte is the encoder's choice, but it must match
            // `p2p::id_to_color`, so the decode is the same one.
            return Some(id_from_color(self.fg));
        }
        if self.pending_utf8.len() > 4 {
            self.pending_utf8.clear();
        }
        None
    }

    fn summary(&self) -> String {
        let cells = self
            .cells
            .iter()
            .map(|(id, n)| format!("{id}:{n}"))
            .collect::<Vec<_>>()
            .join(" ");
        format!(
            "transmits {}\nplacements {}\nvirtual {}\ncells {}",
            self.transmits.len(),
            self.placements.len(),
            self.virtual_placements,
            if cells.is_empty() { "-".to_string() } else { cells },
        )
    }
}

/// Undo `p2p::id_to_color`: the low byte is in blue.
fn id_from_color(rgb: u32) -> u32 {
    let (r, g, b) = ((rgb >> 16) & 0xff, (rgb >> 8) & 0xff, rgb & 0xff);
    (r << 16) | (g << 8) | b
}

/// Parse `a=T,i=1,f=24` into pairs.
fn parse_cds(cds: &[u8]) -> Vec<(char, &str)> {
    cds.split(|b| *b == b',')
        .filter_map(|cd| {
            let eq = cd.iter().position(|b| *b == b'=')?;
            let key = *cd.first()? as char;
            let value = std::str::from_utf8(&cd[eq + 1..]).ok()?;
            Some((key, value))
        })
        .collect()
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// The length of a CSI sequence, including its introducer and final byte.
fn find_csi_end(input: &[u8]) -> Option<usize> {
    // `ESC [ <params> <final>`, where the final byte is 0x40..=0x7e.
    input
        .iter()
        .enumerate()
        .skip(2)
        .find(|(_, b)| (0x40..=0x7e).contains(*b))
        .map(|(i, _)| i + 1)
}
