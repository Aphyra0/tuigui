//! Raw TGP escape-sequence construction.
//!
//! Pure functions: bytes in, bytes out. No I/O, no state. The stateful
//! session layer lives in [`crate::encoder`].
//!
//! Reference: `docs/kitty-graphics-protocol.rst`.

/// Action byte of a graphics command (the `a=` key).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Transmit pixel data.
    Transmit,
    /// Transmit and immediately place at the cursor.
    TransmitAndPlace,
    /// Place a previously transmitted image.
    Place,
    /// Upload an animation frame (delta) for an image.
    Frame,
    /// Compose: blit a rect from one frame to another.
    Compose,
    /// Control animation playback.
    Animate,
    /// Delete images/placements.
    Delete,
    /// Query support (does not store or replace anything).
    Query,
}

impl Action {
    pub fn as_char(self) -> char {
        match self {
            Action::Transmit => 't',
            Action::TransmitAndPlace => 'T',
            Action::Place => 'p',
            Action::Frame => 'f',
            Action::Compose => 'c',
            Action::Animate => 'a',
            Action::Delete => 'd',
            Action::Query => 'q',
        }
    }
}

/// Build one complete TGP escape sequence from a control block and payload.
///
/// `control` entries are emitted as `key=value` pairs joined by commas, in the
/// given order. `payload` is raw bytes; they are base64-encoded here. An empty
/// payload produces the key-only form.
pub fn command(control: &[(char, String)], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(32 + payload.len() / 3 * 4);
    out.extend_from_slice(b"\x1b_G");
    // q=2 quiet mode: suppress responses/replies from the terminal to this
    // command. We never read graphics replies (the terminal would otherwise
    // echo `Gi=<id>;OK\x1b\` back onto the stream for any command carrying an
    // id, which shows up as garbage text and gets misread as input).
    out.extend_from_slice(b"q=2");
    for (k, v) in control.iter() {
        out.push(b',');
        out.push(*k as u8);
        out.push(b'=');
        out.extend_from_slice(v.as_bytes());
    }
    if !payload.is_empty() {
        out.push(b';');
        out.extend_from_slice(base64_encode(payload).as_bytes());
    }
    out.extend_from_slice(b"\x1b\\");
    out
}

/// Chunk a payload into TGP-transmittable command sequences (direct medium).
///
/// Chunks are <= 4096 base64 chars; only the first carries `extra_control`;
/// continuation chunks carry `m=1`, the last `m=0`. The raw payload bytes are
/// sliced here and passed to [`command`], which performs the single base64
/// encoding (do NOT pre-encode here, or you'd double-encode).
pub fn chunked_transmit(
    fmt: u32,
    mut extra_control: Vec<(char, String)>,
    payload: &[u8],
) -> Vec<Vec<u8>> {
    const MAX_B64: usize = 4096;
    // Bytes that fit in 4096 base64 chars (4 b64 chars per 3 bytes).
    let bytes_per_chunk = MAX_B64 / 4 * 3;
    let mut out = Vec::new();
    let mut offset = 0usize;
    let mut first = true;
    loop {
        let end = (offset + bytes_per_chunk).min(payload.len());
        let chunk = &payload[offset..end];
        let is_last = end == payload.len();
        let mut control: Vec<(char, String)> = Vec::with_capacity(extra_control.len() + 2);
        if first {
            control.append(&mut extra_control);
            control.push(('f', fmt.to_string()));
        }
        control.push(('m', if is_last { "0" } else { "1" }.to_string()));
        out.push(command(&control, chunk));
        first = false;
        offset = end;
        if is_last {
            break;
        }
    }
    out
}

/// RFC 4648 base64 (standard alphabet, padded). Delegates to the fast SIMD
/// encoder from the `base64` crate rather than a hand-rolled per-byte loop,
/// which was the per-frame hotspot (each TGP frame is several MB of pixels).
pub fn base64_encode(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn command_shape() {
        let out = command(
            &[
                ('a', "T".into()),
                ('f', "24".into()),
                ('s', "2".into()),
                ('v', "1".into()),
            ],
            &[0xff, 0x00, 0x00, 0x00, 0xff, 0x00],
        );
        let s = String::from_utf8(out).unwrap();
        // q=2 quiet mode is prepended so the terminal sends no replies.
        assert_eq!(s, "\x1b_Gq=2,a=T,f=24,s=2,v=1;/wAAAP8A\x1b\\");
    }

    #[test]
    fn command_is_quiet_so_terminal_does_not_reply() {
        // Any command carrying an id (transmit/place/delete) would make the
        // terminal emit `Gi=<id>;OK\x1b\` unless quiet mode is set. We never
        // read replies, so every command must be q=2.
        let out = command(&[('i', "7".into()), ('a', "d".into()), ('d', "i".into())], &[]);
        let s = String::from_utf8(out).unwrap();
        assert!(s.starts_with("\x1b_Gq=2,"), "must request quiet mode, got: {s}");
        assert!(s.contains("i=7"));
    }

    #[test]
    fn chunking_first_and_last_flags() {
        // 12288 bytes -> 16384 base64 chars -> 4 chunks
        let payload = vec![7u8; 12288];
        let chunks = chunked_transmit(32, vec![('i', "1".into())], &payload);
        assert_eq!(chunks.len(), 4);
        let first = String::from_utf8(chunks[0].clone()).unwrap();
        assert!(first.contains("f=32"));
        assert!(first.contains("i=1"));
        assert!(first.contains("m=1"));
        let last = String::from_utf8(chunks.last().unwrap().clone()).unwrap();
        assert!(last.contains("m=0"));
        assert!(!last.contains("f=32"));
    }
}
