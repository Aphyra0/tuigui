//! Sequential `a=q` round-trip benchmark for TGP markers.
//!
//! Sends N query markers one at a time, waiting for each ack before
//! sending the next. Measures the wall-clock time per request->ack and the
//! total. This is the metric that decides whether per-op markers are cheap
//! enough to drop batching entirely (each op self-attributed by its own
//! marker instead of a trailing group sentinel).
//!
//! The marker protocol is:
//!   \x1b_Ga=q,i=<id>,q=0,s=1,v=1,f=24;AQID\x1b\\
//!
//! where `AQID` is base64 of 3 RGB bytes (1 row, 1 column) - a *valid* tiny
//! image so terminals reply `OK` rather than decode-error (empty payloads get
//! EINVAL on ghostty/kitty/rio). The reply echoes `i=<id>;OK` (or
//! `i=<id>;ERROR:<reason>`), which is how we attribute each ack. We use
//! monotonic, collision-free ids (`BASE_ID + i`, deterministic rather than
//! cryptographically random) so every ack is exactly verifiable against the
//! set we sent.
//!
//! Run inside a real TGP terminal (kitty/ghostty/etc.):
//!   cargo run -p easytgp --example marker_bench        # sequential: one at a time
//!   cargo run -p easytgp --example marker_bench --burst  # all N at once
//!
//! `--burst` fires every marker back-to-back then counts unique acks by id,
//! measuring the marker overhead itself (the metric that decides whether to
//! drop batching). Default mode is sequential, measuring per request->ack RTT.
//! stdin must be switched to non-canonical (raw-ish) mode first, otherwise
//! read() blocks until a newline and ack bytes carry none.

use std::io::{Read, Write};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use rustix::termios::{LocalModes, OptionalActions};

const N: u32 = 10_000;
const BASE_ID: u32 = 0x1_0000; // offset so ids won't collide with real allocations
const ROUND_TRIP_TIMEOUT: Duration = Duration::from_millis(10_000);
const FIRST_PROBE_TIMEOUT: Duration = Duration::from_millis(2_000);
// 1x1 RGB image: 3 bytes -> base64 "AQID"; some *valid* payload so terminals
// reply `OK` rather than a decode error / EINVAL.
const PAYLOAD: &str = "AQID";

// Keep the first N raw reply bodies for dump-on-exit / when no acks arrive.
const DUMP: usize = 10;
const DEBUG: bool = true;

struct AckParser {
    buf: Vec<u8>,
    /// Escaped raw bodies of the first `DUMP` captures (for debugging what the
    /// terminal actually sends back).
    raw: Vec<String>,
}

/// A parsed reply. `id` is the `i=` the terminal echoed; `ok` is true when
/// the status was exactly `OK` (as opposed to `ERROR:<reason>`).
#[derive(Debug, PartialEq, Eq)]
struct Ack {
    id: u32,
    ok: bool,
}

impl AckParser {
    fn new() -> Self {
        AckParser {
            buf: Vec::new(),
            raw: Vec::with_capacity(DUMP),
        }
    }

    /// Feed bytes as they arrive and pull out any complete `\x1b_G...\x1b\\`
    /// captures, returning the parsed id + status. Handles arbitrary chunk
    /// boundaries.
    fn push(&mut self, bytes: &[u8]) -> Vec<Option<Ack>> {
        self.buf.extend_from_slice(bytes);
        let mut acks = Vec::new();
        loop {
            // Find the ESC that starts an APC.
            let Some(esc) = self.buf.iter().position(|&b| b == 0x1b) else {
                self.buf.clear();
                break;
            };
            let g = self.buf.get(esc + 1..esc + 3);
            if g != Some(&[b'_', b'G']) {
                self.buf.drain(..=esc);
                continue;
            }
            // Find its terminating ST `\x1b\\`.
            let mut st_esc = None;
            let mut i = esc + 3;
            while i + 1 < self.buf.len() && i < self.buf.len() {
                if self.buf[i] == 0x1b {
                    if self.buf.get(i + 1) == Some(&b'\\') {
                        st_esc = Some(i);
                    }
                    break;
                }
                i += 1;
            }
            let Some(st_esc) = st_esc else {
                break; // incomplete; wait for more bytes
            };
            // Body is everything between `_G` and the ST's ESC (exclusive),
            // i.e. `buf[esc+3 .. st_esc]`.
            let capture = &self.buf[esc + 3..st_esc];
            if self.raw.len() < DUMP {
                self.raw.push(escape(capture));
            }
            acks.push(parse_ack(capture));
            self.buf.drain(..=st_esc + 1); // through the backslash of the ST
        }
        acks
    }

    /// Debug dump of the first raw reply bodies.
    fn dump(&self) {
        if self.raw.is_empty() {
            eprintln!("[debug] no replies captured at all");
            return;
        }
        eprintln!("[debug] first {} raw reply bodies:", self.raw.len());
        for (i, body) in self.raw.iter().enumerate() {
            eprintln!("  {i}: \"{body}\"");
        }
    }
}

/// Render raw capture bytes with control chars escaped so they print readably.
fn escape(bytes: &[u8]) -> String {
    let mut out = String::new();
    for &b in bytes {
        match b {
            0x1b => out.push_str("\\x1b"),
            0x0d => out.push_str("\\r"),
            0x0a => out.push_str("\\n"),
            0x09 => out.push_str("\\t"),
            b' ' => out.push(' '),
            0x20..=0x7e => out.push(b as char),
            other => out.push_str(&format!("\\x{other:02x}")),
        }
    }
    out
}

/// Parse an `Ack` from the reply body (the part between `_G` and ST), e.g.
/// `o=ci,i=123;OK` or `i=123;ERROR:cannot-parse-image`. Returns `None` if it
/// cannot find an `i=` field.
fn parse_ack(capture: &[u8]) -> Option<Ack> {
    let s = std::str::from_utf8(capture).ok()?;
    let body = s.split_once(';').map(|(h, t)| (h, t)).unwrap_or((s, ""));
    let (header, tail) = body;
    let mut id = None;
    for chunk in header.split(',') {
        if let Some(rest) = chunk.strip_prefix("i=") {
            let digits: &str = &rest[..rest
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(rest.len())];
            id = if digits.is_empty() { None } else { digits.parse().ok() };
            break;
        }
    }
    let id = id?;
    let ok = tail == "OK";
    Some(Ack { id, ok })
}

/// Verifies completeness, correctness, and FIFO delivery order.
struct Verifier {
    // Set of ids we sent that have NOT yet been acknowledged as OK.
    pending: std::collections::HashSet<u32>,
    // Any in-range id that received a reply (OK or ERROR). Used by the
    // sequential loop to stop waiting for the current id on an error reply.
    replied: std::collections::HashSet<u32>,
    errors: u32,
    duplicates: u32,
    unexpected: u32,
    // Largest id that has been freshly acked so far. Since burst sends ids in
    // ascending send order, FIFO delivery yields ascending acks; a fresh ack
    // below this is a reorder.
    max_acked: Option<u32>,
    out_of_order: u32,
}

impl Verifier {
    fn new() -> Self {
        Verifier {
            pending: (0..N).map(|i| BASE_ID + i).collect(),
            replied: std::collections::HashSet::new(),
            errors: 0,
            duplicates: 0,
            unexpected: 0,
            max_acked: None,
            out_of_order: 0,
        }
    }

    /// Record one parsed reply. Returns true when all expected acks are in.
    fn record(&mut self, ack: Ack) -> bool {
        match ack {
            Ack { id, ok: true } if self.pending.contains(&id) => {
                self.pending.remove(&id);
                self.replied.insert(id);
                // Fresh ack: check FIFO (ascending id) order.
                if let Some(m) = self.max_acked {
                    if id < m {
                        self.out_of_order += 1;
                    }
                }
                self.max_acked = Some(match self.max_acked {
                    Some(m) if m > id => m,
                    _ => id,
                });
            }
            Ack { id, ok: true } if BASE_ID <= id && id < BASE_ID + N => {
                // An in-range id we already saw: duplicate OK.
                self.duplicates += 1;
                self.replied.insert(id);
            }
            Ack { id: _, ok: true } => {
                self.unexpected += 1;
            }
            Ack { ok: false, id } => {
                self.errors += 1;
                if id >= BASE_ID && id < BASE_ID + N {
                    self.replied.insert(id);
                }
            }
        }
        self.pending.is_empty()
    }

    fn ok_count(&self) -> u32 {
        N - self.pending.len() as u32
    }
}

fn main() {
    // ---- 0. Flag selection ---------------------------------------------------
    let burst = std::env::args().any(|a| a == "--burst");

    // ---- 1. Switch stdin to non-canonical raw-ish mode (no ECHO, no ICANON) --
    use rustix::fd::BorrowedFd;
    // SAFETY: fd 0 is stdin and lives for the whole program.
    let stdin_fd = unsafe { BorrowedFd::borrow_raw(0) };
    let original = rustix::termios::tcgetattr(stdin_fd).expect("tcgetattr");
    let mut raw = original.clone();
    raw.local_modes.remove(LocalModes::ECHO);
    raw.local_modes.remove(LocalModes::ICANON);
    rustix::termios::tcsetattr(stdin_fd, OptionalActions::Now, &raw).expect("tcsetattr");

    struct Restore(rustix::termios::Termios);
    impl Drop for Restore {
        fn drop(&mut self) {
            let fd = unsafe { BorrowedFd::borrow_raw(0) };
            let _ = rustix::termios::tcsetattr(fd, OptionalActions::Now, &self.0);
        }
    }
    let _restore = Restore(original);

    // ---- 2. Reader thread: drains stdin into a channel for timeout-friendly reads
    let (tx, rx): (mpsc::Sender<Vec<u8>>, Receiver<Vec<u8>>) = mpsc::channel();
    std::thread::spawn(move || {
        loop {
            let mut buf = [0u8; 4096];
            match std::io::stdin().read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    });

    // ---- 3. Request->ack loop (mode-aware) -------------------------------------
    let mut stdout = std::io::stdout().lock();
    let start = Instant::now();
    let mut parser = AckParser::new();
    let mut verifier = Verifier::new();
    let mut per_req = Vec::with_capacity(N as usize);

    if burst {
        // Burst: fire all N markers back-to-back, then collect acks.
        let tx = Instant::now();
        for i in 0..N {
            let id = BASE_ID + i;
            let req = format!("\x1b_Ga=q,i={id},q=0,s=1,v=1,f=24;{PAYLOAD}\x1b\\");
            stdout.write_all(req.as_bytes()).expect("write marker");
        }
        stdout.flush().expect("flush markers");
        let sent_at = tx.elapsed();
        eprintln!("wrote {N} markers in {sent_at:.3?}");

        // Collect until every expected id is OK, or the timeout elapses.
        let deadline = Instant::now() + ROUND_TRIP_TIMEOUT;
        while !verifier.pending.is_empty() && Instant::now() < deadline {
            match rx.try_recv() {
                Ok(bytes) => {
                    for got in parser.push(&bytes) {
                        if let Some(ack) = got {
                            verifier.record(ack);
                        }
                    }
                }
                Err(TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(TryRecvError::Disconnected) => {
                    eprintln!("stdin reader ended; terminal may not be TGP-capable");
                    break;
                }
            }
        }

        // Fast-fail probe: nothing replied at all.
        if verifier.ok_count() == 0 {
            eprintln!(
                "no `a=q` replies within {ROUND_TRIP_TIMEOUT:?}; \
                 terminal likely does not support TGP. aborting."
            );
            if DEBUG {
                parser.dump();
            }
            print_report_burst(&start, &verifier);
            return;
        }
        print_report_burst(&start, &verifier);
    } else {
        // Sequential: one at a time, waiting for each ack before the next.
        for i in 0..N {
            let id = BASE_ID + i;
            let req = format!("\x1b_Ga=q,i={id},q=0,s=1,v=1,f=24;{PAYLOAD}\x1b\\");
            let t0 = Instant::now();
            stdout.write_all(req.as_bytes()).expect("write marker");
            stdout.flush().expect("flush marker");

            // Fast-fail on the very first marker: if nothing replies well past
            // the normal round-trip, the terminal probably doesn't do TGP.
            let timeout = if i == 0 { FIRST_PROBE_TIMEOUT } else { ROUND_TRIP_TIMEOUT };

            // Read until this marker's id is verified (a console ACK), until it has
            // replied (even with an error), or the timeout elapses.
            while t0.elapsed() < timeout
                && verifier.pending.contains(&id)
                && !verifier.replied.contains(&id)
            {
                match rx.try_recv() {
                    Ok(bytes) => {
                        for got in parser.push(&bytes) {
                            if let Some(ack) = got {
                                verifier.record(ack);
                            }
                        }
                    }
                    Err(TryRecvError::Empty) => {
                        std::thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(TryRecvError::Disconnected) => {
                        eprintln!("stdin reader ended; terminal may not be TGP-capable");
                        print_report(&per_req, &start, &verifier);
                        return;
                    }
                }
            }

            per_req.push(t0.elapsed());
        }

        // Final fast-fail: no reply at all on the first marker.
        if verifier.ok_count() == 0 {
            eprintln!(
                "no `a=q` replies within {FIRST_PROBE_TIMEOUT:?}; \
                 terminal likely does not support TGP. aborting."
            );
            if DEBUG {
                parser.dump();
            }
            print_report(&per_req, &start, &verifier);
            return;
        }
        print_report(&per_req, &start, &verifier);
    }
}

fn print_report_burst(start: &Instant, verifier: &Verifier) {
    eprintln!();
    let ok = verifier.ok_count();
    eprintln!(
        "burst: sent {N} `a=q` markers, received {ok} unique `OK` acks"
    );
    eprintln!(
        "  errors: {}, duplicates: {}, unexpected ids: {}",
        verifier.errors, verifier.duplicates, verifier.unexpected
    );
    eprintln!(
        "  missing (no OK, not echoed): {}",
        verifier.pending.len()
    );
    eprintln!("  PASS: {N}/{N} acked with the exact correct ids: {}", ok == N);
    eprintln!("  FIFO: {} out-of-order acks", verifier.out_of_order);
    let total = start.elapsed();
    eprintln!("total (write + collect): {total:.2?}");
    if ok > 0 {
        eprintln!("avg per marker (write+collect)/N: {total:?} / {N} = {:.3?}", total / N);
    }
}

fn print_report(per_req: &[Duration], start: &Instant, verifier: &Verifier) {
    eprintln!();
    let ok = verifier.ok_count();
    eprintln!(
        "sent {N} sequential `a=q` markers, received {ok} unique `OK` acks"
    );
    eprintln!(
        "  errors: {}, duplicates: {}, unexpected ids: {}",
        verifier.errors, verifier.duplicates, verifier.unexpected
    );
    eprintln!(
        "  missing (no OK, not echoed): {}",
        verifier.pending.len()
    );
    eprintln!("  PASS: {N}/{N} acked with the exact correct ids: {}", ok == N);
    eprintln!("  FIFO: {} out-of-order acks", verifier.out_of_order);
if !per_req.is_empty() {
        let avg = per_req.iter().sum::<Duration>()
            / u32::try_from(per_req.len()).unwrap_or(1);
        let max = per_req.iter().copied().max().unwrap_or_default();
        eprintln!("avg request->ack: {avg:.2?}");
        eprintln!("max request->ack: {max:.2?}");
    }
    eprintln!("total: {:.2?}", start.elapsed());
}
