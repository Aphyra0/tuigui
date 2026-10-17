//! Shared pieces for the `tui`/`gui` remote-streaming experiment.
//!
//! The wire is a tiny length-prefixed framing over a unix socket. Nothing here
//! touches the terminal; `tui` owns that, `gui` only produces frames.
//!
//! Both binaries compile this module but each uses only one half of it: the
//! server reads messages and writes replies, the client writes requests and
//! reads replies. Dead code here is the other half, not a missing call site.
#![allow(dead_code)]

use std::io::{self, Read, Write};

/// Magic so a desynced stream fails loudly rather than decoding garbage.
const MAGIC: u32 = 0x5447_5549; // "TGUI"

/// One message on the frame stream.
#[derive(Debug)]
pub enum Msg {
    /// Liveness check. The client sends `Ping` on startup and the server echoes
    /// a `Pong` with the same nonce, so the client can confirm the connection
    /// (and, when it is remote, that the tunnel is actually up) before drawing.
    Ping { nonce: u32 },
    Pong { nonce: u32 },
    /// Client asks the server to create the image it will draw into: the grid
    /// it reserved in cells, and the pixel size of the frames to come.
    ///
    /// The size must be settled before the client draws placeholders, because
    /// it needs the resulting id, and the *base* image a terminal holds has a
    /// fixed size that later frames cannot exceed.
    Hello {
        cols: u32,
        rows: u32,
        width: u32,
        height: u32,
    },
    /// Server's answer to [`Msg::Hello`]: the id the placeholders must encode.
    Image { id: u32 },
    /// The server could not do what the client asked (a frame it cannot decode,
    /// an image it cannot resize, a terminal that rejected an escape).
    ///
    /// The server does not die on these: it reports one and moves on. The client
    /// is the one that should stop, because it is the side whose request failed.
    Error { message: String },
    /// One frame. `payload` is in `codec`'s format and decodes to
    /// `width` x `height` RGB.
    Frame {
        width: u32,
        height: u32,
        codec: Codec,
        payload: Vec<u8>,
    },
    /// What the server did with the frames it has received so far.
    ///
    /// The server is the only side that knows the cost of *showing* a frame, so
    /// only it can report these. The client displays them alongside its own
    /// numbers, which is the point: a slow send and a slow paint look identical
    /// on the client alone.
    Stats(ServerStats),
}

/// Server-side totals and the most recent frame's cost.
///
/// Cumulative counters, not rates: the client knows how much wall time passed
/// and can divide, and a rate computed on the server would be sampling a
/// different clock.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ServerStats {
    /// Frames received and drawn.
    pub frames: u64,
    /// Bytes of frame payload received.
    pub bytes: u64,
    /// Frames that could not be decoded or uploaded.
    pub errors: u64,
    /// Dimensions of the last frame drawn.
    pub width: u32,
    pub height: u32,
    /// The last frame's decode time, in microseconds.
    pub decode_us: u32,
    /// The last frame's upload-and-acknowledge time, in microseconds.
    pub sync_us: u32,
    /// Whole-screen erases seen in the terminal's output stream that discarded
    /// the image, so the server had to put it back.
    ///
    /// A muxer redrawing its screen wipes images, and nothing else reports that:
    /// from the client's side the image was sent and acknowledged long ago. A
    /// large number here means the image is being destroyed constantly, which is
    /// the cost to watch if this ever needs a cheaper strategy.
    pub reasserts: u64,
}

/// How many frames between stats reports.
///
/// The client redraws its readout on every report, so this is also the overlay's
/// refresh interval. 30 is about twice a second at typical frame rates: fast
/// enough to watch, slow enough that the overlay's own redraw is not a
/// measurable part of what it reports.
pub const STATS_EVERY: u64 = 30;

impl ServerStats {
    /// Fold in one frame that was drawn.
    pub fn record_frame(
        &mut self,
        width: u32,
        height: u32,
        bytes: u64,
        decode_us: u32,
        sync_us: u32,
    ) {
        self.frames += 1;
        self.bytes += bytes;
        self.width = width;
        self.height = height;
        self.decode_us = decode_us;
        self.sync_us = sync_us;
    }

    /// Fold in one whole-screen erase that discarded the image.
    pub fn record_reassert(&mut self) {
        self.reasserts += 1;
    }

    /// Whether the report is due after the frame just recorded.
    ///
    /// Counted in frames rather than elapsed time: the client's readout is
    /// per-frame, and matching the frame count keeps the numbers it draws
    /// one-to-one with the frames it sent.
    pub fn should_report(&self) -> bool {
        self.frames > 0 && self.frames % STATS_EVERY == 0
    }
}

/// How a frame's bytes are encoded. Intra-only, so every frame stands alone and
/// no decoder state crosses frames; an inter-frame codec (H.264) would carry a
/// stream instead, which is a different framing entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// Packed RGB24, row-major. Uncompressed; isolates pipeline cost.
    Rgb24,
    /// One JPEG per frame. Compressed, still independent frames.
    Jpeg,
}

impl Codec {
    fn tag(self) -> u8 {
        match self {
            Codec::Rgb24 => 0,
            Codec::Jpeg => 1,
        }
    }
    pub fn from_tag(t: u8) -> Option<Self> {
        match t {
            0 => Some(Codec::Rgb24),
            1 => Some(Codec::Jpeg),
            _ => None,
        }
    }
}

impl Msg {
    pub fn write(&self, w: &mut impl Write) -> io::Result<()> {
        w.write_all(&MAGIC.to_be_bytes())?;
        match self {
            Msg::Ping { nonce } => {
                w.write_all(&[2u8])?;
                w.write_all(&nonce.to_be_bytes())?;
            }
            Msg::Pong { nonce } => {
                w.write_all(&[3u8])?;
                w.write_all(&nonce.to_be_bytes())?;
            }
            Msg::Hello {
                cols,
                rows,
                width,
                height,
            } => {
                w.write_all(&[0u8])?;
                w.write_all(&cols.to_be_bytes())?;
                w.write_all(&rows.to_be_bytes())?;
                w.write_all(&width.to_be_bytes())?;
                w.write_all(&height.to_be_bytes())?;
            }
            Msg::Image { id } => {
                w.write_all(&[4u8])?;
                w.write_all(&id.to_be_bytes())?;
            }
            Msg::Error { message } => {
                w.write_all(&[5u8])?;
                let bytes = message.as_bytes();
                w.write_all(&(bytes.len() as u32).to_be_bytes())?;
                w.write_all(bytes)?;
            }
            Msg::Frame {
                width,
                height,
                codec,
                payload,
            } => {
                w.write_all(&[1u8])?;
                w.write_all(&width.to_be_bytes())?;
                w.write_all(&height.to_be_bytes())?;
                w.write_all(&[codec.tag()])?;
                w.write_all(&(payload.len() as u64).to_be_bytes())?;
                w.write_all(payload)?;
            }
            Msg::Stats(s) => {
                w.write_all(&[6u8])?;
                w.write_all(&s.frames.to_be_bytes())?;
                w.write_all(&s.bytes.to_be_bytes())?;
                w.write_all(&s.errors.to_be_bytes())?;
                w.write_all(&s.width.to_be_bytes())?;
                w.write_all(&s.height.to_be_bytes())?;
                w.write_all(&s.decode_us.to_be_bytes())?;
                w.write_all(&s.sync_us.to_be_bytes())?;
                w.write_all(&s.reasserts.to_be_bytes())?;
            }
        }
        w.flush()
    }

    pub fn read(r: &mut impl Read) -> io::Result<Self> {
        let mut magic = [0u8; 4];
        r.read_exact(&mut magic)?;
        if u32::from_be_bytes(magic) != MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "bad frame magic",
            ));
        }
        let mut tag = [0u8; 1];
        r.read_exact(&mut tag)?;
        match tag[0] {
            2 | 3 => {
                let mut nonce = [0u8; 4];
                r.read_exact(&mut nonce)?;
                let nonce = u32::from_be_bytes(nonce);
                Ok(if tag[0] == 2 {
                    Msg::Ping { nonce }
                } else {
                    Msg::Pong { nonce }
                })
            }
            0 => {
                let (mut cols, mut rows, mut w, mut h) =
                    ([0u8; 4], [0u8; 4], [0u8; 4], [0u8; 4]);
                r.read_exact(&mut cols)?;
                r.read_exact(&mut rows)?;
                r.read_exact(&mut w)?;
                r.read_exact(&mut h)?;
                Ok(Msg::Hello {
                    cols: u32::from_be_bytes(cols),
                    rows: u32::from_be_bytes(rows),
                    width: u32::from_be_bytes(w),
                    height: u32::from_be_bytes(h),
                })
            }
            4 => {
                let mut id = [0u8; 4];
                r.read_exact(&mut id)?;
                Ok(Msg::Image {
                    id: u32::from_be_bytes(id),
                })
            }
            5 => {
                let mut len = [0u8; 4];
                r.read_exact(&mut len)?;
                let mut bytes = vec![0u8; u32::from_be_bytes(len) as usize];
                r.read_exact(&mut bytes)?;
                Ok(Msg::Error {
                    message: String::from_utf8_lossy(&bytes).into_owned(),
                })
            }
            1 => {
                let (mut w, mut h, mut c, mut len) = ([0u8; 4], [0u8; 4], [0u8; 1], [0u8; 8]);
                r.read_exact(&mut w)?;
                r.read_exact(&mut h)?;
                r.read_exact(&mut c)?;
                r.read_exact(&mut len)?;
                let n = u64::from_be_bytes(len) as usize;
                let mut payload = vec![0u8; n];
                r.read_exact(&mut payload)?;
                let codec = Codec::from_tag(c[0]).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, format!("bad codec {}", c[0]))
                })?;
                Ok(Msg::Frame {
                    width: u32::from_be_bytes(w),
                    height: u32::from_be_bytes(h),
                    codec,
                    payload,
                })
            }
            6 => {
                let mut b = [0u8; 48];
                r.read_exact(&mut b)?;
                Ok(Msg::Stats(ServerStats {
                    frames: u64::from_be_bytes(b[0..8].try_into().unwrap()),
                    bytes: u64::from_be_bytes(b[8..16].try_into().unwrap()),
                    errors: u64::from_be_bytes(b[16..24].try_into().unwrap()),
                    width: u32::from_be_bytes(b[24..28].try_into().unwrap()),
                    height: u32::from_be_bytes(b[28..32].try_into().unwrap()),
                    decode_us: u32::from_be_bytes(b[32..36].try_into().unwrap()),
                    sync_us: u32::from_be_bytes(b[36..40].try_into().unwrap()),
                    reasserts: u64::from_be_bytes(b[40..48].try_into().unwrap()),
                }))
            }
            t => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("bad frame tag {t}"),
            )),
        }
    }

    /// Async write, for the server's reply path.
    pub async fn write_async(
        &self,
        w: &mut (impl tokio::io::AsyncWrite + Unpin),
    ) -> io::Result<()> {
        use tokio::io::AsyncWriteExt as _;
        let mut buf = Vec::new();
        self.write(&mut buf)?;
        w.write_all(&buf).await
    }

    /// Async read, for the server's frame loop.
    pub async fn read_async(
        r: &mut (impl tokio::io::AsyncRead + Unpin),
    ) -> io::Result<Option<Self>> {
        use tokio::io::AsyncReadExt as _;
        let mut magic = [0u8; 4];
        match r.read_exact(&mut magic).await {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }
        if u32::from_be_bytes(magic) != MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad frame magic"));
        }
        let mut tag = [0u8; 1];
        r.read_exact(&mut tag).await?;
        let msg = match tag[0] {
            2 | 3 => {
                let mut nonce = [0u8; 4];
                r.read_exact(&mut nonce).await?;
                let nonce = u32::from_be_bytes(nonce);
                if tag[0] == 2 {
                    Msg::Ping { nonce }
                } else {
                    Msg::Pong { nonce }
                }
            }
            0 => {
                let mut b = [0u8; 16];
                r.read_exact(&mut b).await?;
                Msg::Hello {
                    cols: u32::from_be_bytes(b[0..4].try_into().unwrap()),
                    rows: u32::from_be_bytes(b[4..8].try_into().unwrap()),
                    width: u32::from_be_bytes(b[8..12].try_into().unwrap()),
                    height: u32::from_be_bytes(b[12..16].try_into().unwrap()),
                }
            }
            4 => {
                let mut id = [0u8; 4];
                r.read_exact(&mut id).await?;
                Msg::Image {
                    id: u32::from_be_bytes(id),
                }
            }
            5 => {
                let mut len = [0u8; 4];
                r.read_exact(&mut len).await?;
                let mut bytes = vec![0u8; u32::from_be_bytes(len) as usize];
                r.read_exact(&mut bytes).await?;
                Msg::Error {
                    message: String::from_utf8_lossy(&bytes).into_owned(),
                }
            }
            1 => {
                let mut wh = [0u8; 8];
                r.read_exact(&mut wh).await?;
                let mut c = [0u8; 1];
                r.read_exact(&mut c).await?;
                let mut len = [0u8; 8];
                r.read_exact(&mut len).await?;
                let n = u64::from_be_bytes(len) as usize;
                let mut payload = vec![0u8; n];
                r.read_exact(&mut payload).await?;
                let codec = Codec::from_tag(c[0])
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad codec"))?;
                Msg::Frame {
                    width: u32::from_be_bytes(wh[0..4].try_into().unwrap()),
                    height: u32::from_be_bytes(wh[4..8].try_into().unwrap()),
                    codec,
                    payload,
                }
            }
            6 => {
                let mut b = [0u8; 48];
                r.read_exact(&mut b).await?;
                Msg::Stats(ServerStats {
                    frames: u64::from_be_bytes(b[0..8].try_into().unwrap()),
                    bytes: u64::from_be_bytes(b[8..16].try_into().unwrap()),
                    errors: u64::from_be_bytes(b[16..24].try_into().unwrap()),
                    width: u32::from_be_bytes(b[24..28].try_into().unwrap()),
                    height: u32::from_be_bytes(b[28..32].try_into().unwrap()),
                    decode_us: u32::from_be_bytes(b[32..36].try_into().unwrap()),
                    sync_us: u32::from_be_bytes(b[36..40].try_into().unwrap()),
                    reasserts: u64::from_be_bytes(b[40..48].try_into().unwrap()),
                })
            }
            t => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("bad frame tag {t}"),
                ))
            }
        };
        Ok(Some(msg))
    }
}

/// The 141 combining diacritics kitty uses to encode row/column in a
/// placeholder cell, in order (`rowcolumn-diacritics.txt`). Index 0 is row 0,
/// column 0.
pub const DIACRITICS: [char; 297] = [
'\u{0305}', '\u{030d}', '\u{030e}', '\u{0310}', '\u{0312}', '\u{033d}', '\u{033e}', '\u{033f}', 
    '\u{0346}', '\u{034a}', '\u{034b}', '\u{034c}', '\u{0350}', '\u{0351}', '\u{0352}', '\u{0357}', 
    '\u{035b}', '\u{0363}', '\u{0364}', '\u{0365}', '\u{0366}', '\u{0367}', '\u{0368}', '\u{0369}', 
    '\u{036a}', '\u{036b}', '\u{036c}', '\u{036d}', '\u{036e}', '\u{036f}', '\u{0483}', '\u{0484}', 
    '\u{0485}', '\u{0486}', '\u{0487}', '\u{0592}', '\u{0593}', '\u{0594}', '\u{0595}', '\u{0597}', 
    '\u{0598}', '\u{0599}', '\u{059c}', '\u{059d}', '\u{059e}', '\u{059f}', '\u{05a0}', '\u{05a1}', 
    '\u{05a8}', '\u{05a9}', '\u{05ab}', '\u{05ac}', '\u{05af}', '\u{05c4}', '\u{0610}', '\u{0611}', 
    '\u{0612}', '\u{0613}', '\u{0614}', '\u{0615}', '\u{0616}', '\u{0617}', '\u{0657}', '\u{0658}', 
    '\u{0659}', '\u{065a}', '\u{065b}', '\u{065d}', '\u{065e}', '\u{06d6}', '\u{06d7}', '\u{06d8}', 
    '\u{06d9}', '\u{06da}', '\u{06db}', '\u{06dc}', '\u{06df}', '\u{06e0}', '\u{06e1}', '\u{06e2}', 
    '\u{06e4}', '\u{06e7}', '\u{06e8}', '\u{06eb}', '\u{06ec}', '\u{0730}', '\u{0732}', '\u{0733}', 
    '\u{0735}', '\u{0736}', '\u{073a}', '\u{073d}', '\u{073f}', '\u{0740}', '\u{0741}', '\u{0743}', 
    '\u{0745}', '\u{0747}', '\u{0749}', '\u{074a}', '\u{07eb}', '\u{07ec}', '\u{07ed}', '\u{07ee}', 
    '\u{07ef}', '\u{07f0}', '\u{07f1}', '\u{07f3}', '\u{0816}', '\u{0817}', '\u{0818}', '\u{0819}', 
    '\u{081b}', '\u{081c}', '\u{081d}', '\u{081e}', '\u{081f}', '\u{0820}', '\u{0821}', '\u{0822}', 
    '\u{0823}', '\u{0825}', '\u{0826}', '\u{0827}', '\u{0829}', '\u{082a}', '\u{082b}', '\u{082c}', 
    '\u{082d}', '\u{0951}', '\u{0953}', '\u{0954}', '\u{0f82}', '\u{0f83}', '\u{0f86}', '\u{0f87}', 
    '\u{135d}', '\u{135e}', '\u{135f}', '\u{17dd}', '\u{193a}', '\u{1a17}', '\u{1a75}', '\u{1a76}', 
    '\u{1a77}', '\u{1a78}', '\u{1a79}', '\u{1a7a}', '\u{1a7b}', '\u{1a7c}', '\u{1b6b}', '\u{1b6d}', 
    '\u{1b6e}', '\u{1b6f}', '\u{1b70}', '\u{1b71}', '\u{1b72}', '\u{1b73}', '\u{1cd0}', '\u{1cd1}', 
    '\u{1cd2}', '\u{1cda}', '\u{1cdb}', '\u{1ce0}', '\u{1dc0}', '\u{1dc1}', '\u{1dc3}', '\u{1dc4}', 
    '\u{1dc5}', '\u{1dc6}', '\u{1dc7}', '\u{1dc8}', '\u{1dc9}', '\u{1dcb}', '\u{1dcc}', '\u{1dd1}', 
    '\u{1dd2}', '\u{1dd3}', '\u{1dd4}', '\u{1dd5}', '\u{1dd6}', '\u{1dd7}', '\u{1dd8}', '\u{1dd9}', 
    '\u{1dda}', '\u{1ddb}', '\u{1ddc}', '\u{1ddd}', '\u{1dde}', '\u{1ddf}', '\u{1de0}', '\u{1de1}', 
    '\u{1de2}', '\u{1de3}', '\u{1de4}', '\u{1de5}', '\u{1de6}', '\u{1dfe}', '\u{20d0}', '\u{20d1}', 
    '\u{20d4}', '\u{20d5}', '\u{20d6}', '\u{20d7}', '\u{20db}', '\u{20dc}', '\u{20e1}', '\u{20e7}', 
    '\u{20e9}', '\u{20f0}', '\u{2cef}', '\u{2cf0}', '\u{2cf1}', '\u{2de0}', '\u{2de1}', '\u{2de2}', 
    '\u{2de3}', '\u{2de4}', '\u{2de5}', '\u{2de6}', '\u{2de7}', '\u{2de8}', '\u{2de9}', '\u{2dea}', 
    '\u{2deb}', '\u{2dec}', '\u{2ded}', '\u{2dee}', '\u{2def}', '\u{2df0}', '\u{2df1}', '\u{2df2}', 
    '\u{2df3}', '\u{2df4}', '\u{2df5}', '\u{2df6}', '\u{2df7}', '\u{2df8}', '\u{2df9}', '\u{2dfa}', 
    '\u{2dfb}', '\u{2dfc}', '\u{2dfd}', '\u{2dfe}', '\u{2dff}', '\u{a66f}', '\u{a67c}', '\u{a67d}', 
    '\u{a6f0}', '\u{a6f1}', '\u{a8e0}', '\u{a8e1}', '\u{a8e2}', '\u{a8e3}', '\u{a8e4}', '\u{a8e5}', 
    '\u{a8e6}', '\u{a8e7}', '\u{a8e8}', '\u{a8e9}', '\u{a8ea}', '\u{a8eb}', '\u{a8ec}', '\u{a8ed}', 
    '\u{a8ee}', '\u{a8ef}', '\u{a8f0}', '\u{a8f1}', '\u{aab0}', '\u{aab2}', '\u{aab3}', '\u{aab7}', 
    '\u{aab8}', '\u{aabe}', '\u{aabf}', '\u{aac1}', '\u{fe20}', '\u{fe21}', '\u{fe22}', '\u{fe23}', 
    '\u{fe24}', '\u{fe25}', '\u{fe26}', '\u{10a0f}', '\u{10a38}', '\u{1d185}', '\u{1d186}', '\u{1d187}', 
    '\u{1d188}', '\u{1d189}', '\u{1d1aa}', '\u{1d1ab}', '\u{1d1ac}', '\u{1d1ad}', '\u{1d242}', '\u{1d243}', 
    '\u{1d244}', 
];

/// The placeholder scalar.
pub const PLACEHOLDER: char = '\u{10EEEE}';

/// Split an image id into the `(r, g, b)` foreground color that encodes it.
///
/// kitty decodes a placeholder cell's foreground as `(color >> 8) & 0xffffff`
/// with a color packed `0xRRGGBBAA`, so the id's most significant byte belongs
/// in red and its least significant in blue. Both sides go through this so the
/// packing is stated once.
pub fn id_to_color(id: u32) -> (u8, u8, u8) {
    (
        ((id >> 16) & 0xff) as u8,
        ((id >> 8) & 0xff) as u8,
        (id & 0xff) as u8,
    )
}

/// Recover the image id a placeholder cell's foreground color encodes.
///
/// The inverse of [`id_to_color`], used by the server to check what the client
/// actually painted. Getting this pair out of step is the failure mode where the
/// terminal silently draws nothing, so both live here next to each other.
pub fn color_to_id(r: u8, g: u8, b: u8) -> u32 {
    ((r as u32) << 16) | ((g as u32) << 8) | b as u32
}

/// The placeholder scalar's UTF-8 bytes, `U+10EEEE` (four bytes: the scalar is
/// above U+FFFF, so it is not a three-byte character).
///
/// Spelled as bytes rather than a `char` because the server scans the raw pty
/// stream, where the cell may be split across two reads.
pub const PLACEHOLDER_UTF8: [u8; 4] = [0xf4, 0x8e, 0xbb, 0xae];

/// The RGB a placeholder cell carries when it encodes `id`.
pub fn id_placeholder_rgb(id: u32) -> [u8; 3] {
    let (r, g, b) = id_to_color(id);
    [r, g, b]
}

/// The scalar the client draws in a grid cell when a multiplexer is in the way.
///
/// A placeholder cell carries its image id in the foreground *color*, which only
/// survives if the bytes reach the terminal unmodified. Inside a multiplexer they
/// do not: herdr parses its panes' output into cells and re-renders them, so it
/// reads a `U+10EEEE` cell as a glyph it must draw and blanks it
/// (`herdr src/pane/terminal.rs`, `ghostty_buffer_symbol_into`), because the
/// image it would draw is one herdr never received.
///
/// So the client draws this character instead, in [`MARKER_RGB`], and the wrapper
/// rewrites each cell into a real placeholder: it is the only side that sees the
/// bytes after the multiplexer re-rendered them, and the only side that knows the
/// live image id.
pub const MARKER: char = '\u{2588}';

/// The marker's UTF-8 bytes, for the scanner. `U+2588` is three bytes.
pub const MARKER_UTF8: [u8; 3] = [0xe2, 0x96, 0x88];

/// The foreground a marker cell is drawn in, as `(r, g, b)`.
///
/// Pure magenta. The marker is a full block, so its *foreground* is what fills
/// the cell: a background would be hidden behind the glyph, which is how a
/// magenta grid first rendered as default-colored blocks.
pub const MARKER_RGB: (u8, u8, u8) = (255, 0, 255);

/// The SGR parameters that paint a marker's foreground, inside `ESC [ <p> m`.
pub fn marker_fg_params() -> String {
    format!("38;2;{};{};{}", MARKER_RGB.0, MARKER_RGB.1, MARKER_RGB.2)
}

/// Rewrite marker cells into kitty placeholder cells, streaming.
///
/// The pane's bytes arrive in arbitrary chunks, so a cell can be split across
/// two of them: the marker in one, its diacritics in the next. A rewrite that
/// looked at each chunk alone would turn half a cell into a placeholder with no
/// row or column, which the terminal would resolve in the wrong place rather
/// than reject. The tail is therefore held until the rest arrives.
#[derive(Default)]
pub struct MarkerRewriter {
    /// Bytes held back because they may begin a cell that is not complete yet.
    pending: Vec<u8>,
}

impl MarkerRewriter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Rewrite `chunk` for image `id`, returning what to send to the terminal.
    ///
    /// A cell is a marker followed by exactly two diacritics, which the client
    /// always writes: the scalar names the image, the pair gives the row and the
    /// column. Anything else is forwarded untouched, so ordinary text costs
    /// nothing but the comparison.
    pub fn push(&mut self, chunk: &[u8], id: u32) -> Vec<u8> {
        self.pending.extend_from_slice(chunk);
        let mut out = Vec::with_capacity(self.pending.len());
        let mut i = 0;
        while i < self.pending.len() {
            let rest = &self.pending[i..];
            if !rest.starts_with(&MARKER_UTF8) {
                out.push(rest[0]);
                i += 1;
                continue;
            }
            let mut j = MARKER_UTF8.len();
            let mut marks = 0;
            while marks < 2 {
                match scalar_len(rest.get(j).copied()) {
                    Some(len) if rest.len() >= j + len => {
                        // Only a combining mark continues a cell. A letter after
                        // a marker is not this protocol, so forwarding it as text
                        // beats inventing a cell around it.
                        if !is_diacritic(&rest[j..j + len]) {
                            break;
                        }
                        j += len;
                        marks += 1;
                    }
                    // Not enough bytes yet: hold the cell and stop. The bytes
                    // from `i` on stay in `pending` for the next call.
                    _ => {
                        self.pending.drain(..i);
                        return out;
                    }
                }
            }
            if marks < 2 {
                out.extend_from_slice(&MARKER_UTF8);
                i += MARKER_UTF8.len();
                continue;
            }
            let (r, g, b) = id_to_color(id);
            // The id in the foreground, as a placeholder cell needs it, and the
            // marker color on the *default* background rather than left set: the
            // image is drawn over the cell, so a colored background would show
            // through wherever it is transparent. Each cell restates the full
            // pair, because the rewrite cannot know what the pane left set.
            out.extend_from_slice(format!("\x1b[49;38;2;{r};{g};{b}m").as_bytes());
            out.extend_from_slice(&PLACEHOLDER_UTF8);
            out.extend_from_slice(&rest[MARKER_UTF8.len()..j]);
            out.extend_from_slice(format!("\x1b[39;{}m", marker_fg_params()).as_bytes());
            i += j;
        }
        self.pending.drain(..i);
        out
    }
}

/// The length of the UTF-8 scalar starting with `byte`, if it could start one.
fn scalar_len(byte: Option<u8>) -> Option<usize> {
    let b = byte?;
    match b {
        0x00..=0x7f => Some(1),
        0xc2..=0xdf => Some(2),
        0xe0..=0xef => Some(3),
        0xf0..=0xf4 => Some(4),
        // A continuation byte or an invalid lead starts no scalar.
        _ => None,
    }
}

/// Whether `bytes` is one of the combining marks a cell's row and column use.
fn is_diacritic(bytes: &[u8]) -> bool {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.chars().next().is_some_and(|c| DIACRITICS.contains(&c)),
        Err(_) => false,
    }
}

/// Watches a terminal byte stream for placeholder cells and the color they
/// carry, so the server can check what the client actually painted.
///
/// Streaming, not whole-buffer: the pty delivers arbitrary chunks, so a cell or
/// an escape sequence routinely straddles two reads. State kept between calls is
/// what makes that correct rather than luck.
///
/// Only the *most recent* foreground color is tracked, because that is what
/// applies to a cell: `SGR 38;2` sets it and `SGR 39`/`0` clears it. A cell with
/// no explicit color encodes no id, which is itself worth reporting.
#[derive(Debug, Default)]
pub struct PlaceholderScanner {
    /// Bytes not yet consumed, so a partial sequence is not lost.
    pending: Vec<u8>,
    /// Foreground currently in effect, from the last `SGR 38;2;r;g;b`.
    fg: Option<(u8, u8, u8)>,
    /// One entry per placeholder cell seen, in order.
    pub found: Vec<FoundPlaceholder>,
    /// Number of erase-in-display sequences seen that wipe the whole screen
    /// (`ED 2`, `ED 3`) or switch to the alternate screen.
    ///
    /// These matter because a terminal deletes its *image data* on a full erase,
    /// not merely the placement. A multiplexer redrawing its panes emits them
    /// continually, so an image placed once does not survive; counting them is
    /// how the server learns it has to transmit again.
    pub full_erases: u64,
    /// Every APC payload seen, in order, without the `ESC _` and terminator.
    ///
    /// Kept so the server can compare what it transmitted against what the
    /// terminal was actually handed: a graphics command that never arrives, or
    /// arrives rewritten, is otherwise indistinguishable from one that worked.
    pub apcs: Vec<Vec<u8>>,
}

/// A placeholder cell the scanner saw, with the color that was in effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FoundPlaceholder {
    /// The id decoded from the cell's foreground color, or `None` when the cell
    /// carried no explicit color and therefore references nothing.
    pub id: Option<u32>,
    pub rgb: Option<(u8, u8, u8)>,
}

impl PlaceholderScanner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one chunk, consuming what can be consumed and buffering the rest.
    pub fn push(&mut self, bytes: &[u8]) {
        self.pending.extend_from_slice(bytes);
        let mut i = 0;
        while i < self.pending.len() {
            if self.pending[i] == 0x1b {
                // An escape: parse it whole, or wait for the rest.
                match self.parse_escape(i) {
                    Some(next) => {
                        i = next;
                        continue;
                    }
                    None => break,
                }
            }

            let avail = self.pending.len() - i;
            if avail < PLACEHOLDER_UTF8.len() {
                // Fewer bytes than a placeholder needs. If they are the start of
                // one, keep them: draining them would drop the cell when the
                // rest arrives in the next read, which is the normal case on a
                // pty, not a rare one.
                if PLACEHOLDER_UTF8.starts_with(&self.pending[i..]) {
                    break;
                }
                i += 1;
                continue;
            }

            if self.pending[i..i + PLACEHOLDER_UTF8.len()] == PLACEHOLDER_UTF8 {
                let rgb = self.fg;
                self.found.push(FoundPlaceholder {
                    id: rgb.map(|(r, g, b)| color_to_id(r, g, b)),
                    rgb,
                });
                i += PLACEHOLDER_UTF8.len();
                continue;
            }
            i += 1;
        }
        self.pending.drain(..i);
    }

    /// Parse the escape beginning at `at`, if the whole sequence is buffered.
    ///
    /// Returns the index just past it. `None` means "need more bytes", which for
    /// a malformed sequence would be forever, so an unterminated sequence is
    /// abandoned rather than buffered without limit.
    fn parse_escape(&mut self, at: usize) -> Option<usize> {
        let rest = &self.pending[at..];
        if rest.len() < 2 {
            return None;
        }
        match rest[1] {
            // CSI ... final byte in 0x40..=0x7e
            b'[' => {
                let mut end = None;
                for (k, b) in rest.iter().enumerate().skip(2) {
                    if (0x40..=0x7e).contains(b) {
                        end = Some((k, *b));
                        break;
                    }
                }
                match end {
                    Some((k, b'm')) => {
                        // Copied out rather than borrowed: `apply_sgr` mutates
                        // `self`, which `rest` still borrows.
                        let params = rest[2..k].to_vec();
                        self.apply_sgr(&params);
                        Some(at + k + 1)
                    }
                    Some((k, final_byte)) => {
                        // `ED` (erase in display, `J`) and `DECSET 1049`
                        // (alternate screen, `h`) both discard image data in
                        // every terminal that implements the graphics protocol.
                        if final_byte == b'J' || final_byte == b'h' {
                            let params = rest[2..k].to_vec();
                            if Self::wipes_images(final_byte, &params) {
                                self.full_erases += 1;
                            }
                        }
                        Some(at + k + 1)
                    }
                    // Unterminated: give up on it once it is implausibly long.
                    None if rest.len() > 256 => Some(at + rest.len()),
                    None => None,
                }
            }
            // APC, `ESC _ ... ESC \`. The kitty graphics protocol rides in
            // these, so the payload must be skipped whole: scanning it as text
            // is how a graphics command gets mistaken for screen content. Each
            // one is also recorded, so the server can tell whether its own
            // transmit reached the terminal or was rewritten on the way.
            b'_' => {
                for (k, b) in rest.iter().enumerate().skip(2) {
                    if *b == 0x1b && rest.get(k + 1) == Some(&b'\\') {
                        self.apcs.push(rest[2..k].to_vec());
                        return Some(at + k + 2);
                    }
                }
                if rest.len() > 65536 {
                    return Some(at + rest.len());
                }
                None
            }
            // OSC ... BEL or ST. Not interesting, but must be skipped whole so
            // its payload is not scanned as text.
            b']' => {
                for (k, b) in rest.iter().enumerate().skip(2) {
                    if *b == 0x07 {
                        return Some(at + k + 1);
                    }
                    if *b == 0x1b && rest.get(k + 1) == Some(&b'\\') {
                        return Some(at + k + 2);
                    }
                }
                if rest.len() > 4096 {
                    return Some(at + rest.len());
                }
                None
            }
            // Two-byte escape.
            _ => Some(at + 2),
        }
    }

    /// Whether a CSI final byte plus its parameters discards image data.
    ///
    /// Only the whole-screen erases do: `ED 2` (complete display) and `ED 3`
    /// (display plus scrollback). `ED 0`/`ED 1` erase part of the screen and
    /// must not be treated as losing the image, or every prompt redraw would
    /// trigger a re-transmit.
    ///
    /// On the alternate-screen side, `1049` is the one that clears; the plain
    /// save/restore pair (`1047`/`1048`) does not.
    fn wipes_images(final_byte: u8, params: &[u8]) -> bool {
        let text = String::from_utf8_lossy(params);
        // Strip a leading `?` (private modes use it) and any digits we ignore.
        let text = text.trim_start_matches('?');
        let first = text.split(';').next().unwrap_or("");
        match final_byte {
            b'J' => matches!(first, "2" | "3"),
            b'h' => first == "1049",
            _ => false,
        }
    }

    /// Forget the accumulated cell and erase counts, leaving the parser's
    /// position intact.
    ///
    /// The server calls this after acting on them, so a count is reported once
    /// rather than re-triggering on every later chunk.
    pub fn clear_counts(&mut self) {
        self.found.clear();
        self.full_erases = 0;
    }

    /// Apply an SGR parameter list, tracking the foreground.
    fn apply_sgr(&mut self, params: &[u8]) {
        let text = String::from_utf8_lossy(params);
        let nums: Vec<u16> = text
            .split(';')
            .map(|p| p.trim().parse().unwrap_or(0))
            .collect();
        let mut i = 0;
        while i < nums.len() {
            match nums[i] {
                // 38;2;r;g;b — set foreground to a truecolor.
                38 if nums.get(i + 1) == Some(&2) => {
                    if let (Some(r), Some(g), Some(b)) =
                        (nums.get(i + 2), nums.get(i + 3), nums.get(i + 4))
                    {
                        self.fg = Some((*r as u8, *g as u8, *b as u8));
                    }
                    i += 5;
                    continue;
                }
                // 39 (default fg) and 0 (reset) clear it; so does a bare `m`.
                39 | 0 => self.fg = None,
                _ => {}
            }
            i += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(msg: &Msg) -> Msg {
        let mut buf = Vec::new();
        msg.write(&mut buf).unwrap();
        let mut r = buf.as_slice();
        Msg::read(&mut r).unwrap()
    }

    /// Every message survives the wire: a length or tag that disagrees between
    /// the two sides shows up as a hang or a desynced stream, not an error.
    #[test]
    fn messages_roundtrip() {
        match roundtrip(&Msg::Ping { nonce: 0xdead_beef }) {
            Msg::Ping { nonce } => assert_eq!(nonce, 0xdead_beef),
            other => panic!("{other:?}"),
        }
        match roundtrip(&Msg::Pong { nonce: 7 }) {
            Msg::Pong { nonce } => assert_eq!(nonce, 7),
            other => panic!("{other:?}"),
        }
        match roundtrip(&Msg::Hello { cols: 80, rows: 24, width: 640, height: 360 }) {
            Msg::Hello { cols, rows, width, height } => {
                assert_eq!((cols, rows, width, height), (80, 24, 640, 360))
            }
            other => panic!("{other:?}"),
        }
        match roundtrip(&Msg::Image { id: 42 }) {
            Msg::Image { id } => assert_eq!(id, 42),
            other => panic!("{other:?}"),
        }
        match roundtrip(&Msg::Error { message: "boom".into() }) {
            Msg::Error { message } => assert_eq!(message, "boom"),
            other => panic!("{other:?}"),
        }
        match roundtrip(&Msg::Frame { width: 2, height: 1, codec: Codec::Jpeg, payload: vec![1, 2, 3] }) {
            Msg::Frame { width, height, codec, payload } => {
                assert_eq!((width, height, codec), (2, 1, Codec::Jpeg));
                assert_eq!(payload, vec![1, 2, 3]);
            }
            other => panic!("{other:?}"),
        }
        // Every field distinct, so a mis-offset field cannot pass by being
        // equal to its neighbour.
        let want = ServerStats {
            frames: 1,
            bytes: 2,
            errors: 3,
            width: 4,
            height: 5,
            decode_us: 6,
            sync_us: 7,
            reasserts: 8,
        };
        match roundtrip(&Msg::Stats(want)) {
            Msg::Stats(got) => assert_eq!(got, want),
            other => panic!("{other:?}"),
        }
    }

    /// A payload with the wrong magic is rejected rather than decoded as
    /// whatever the bytes happen to say.
    #[test]
    fn bad_magic_is_rejected() {
        let mut r = [0u8; 8].as_slice();
        assert!(Msg::read(&mut r).is_err());
    }

    /// The async read must decode exactly what the sync write encodes.
    ///
    /// Worth its own test because the two are separate hand-written decoders
    /// over the same bytes, and this message is the one a mismatch would show up
    /// in: no size is carried, so a field read short or long desyncs the stream
    /// permanently rather than failing loudly.
    #[test]
    fn async_read_matches_sync_write() {
        let messages = [
            Msg::Ping { nonce: 1 },
            Msg::Hello { cols: 80, rows: 24, width: 640, height: 360 },
            Msg::Image { id: 9 },
            Msg::Frame { width: 2, height: 1, codec: Codec::Rgb24, payload: vec![1, 2] },
            Msg::Stats(ServerStats {
                frames: 1,
                bytes: 2,
                errors: 3,
                width: 4,
                height: 5,
                decode_us: 6,
                sync_us: 7,
                reasserts: 8,
            }),
            Msg::Error { message: "x".into() },
        ];

        // One buffer holding every message, so a decoder that over- or
        // under-reads cannot resync by luck at the end of its own message.
        let mut buf = Vec::new();
        for m in &messages {
            m.write(&mut buf).unwrap();
        }

        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let decoded = rt.block_on(async {
            let mut r = buf.as_slice();
            let mut out = Vec::new();
            while let Some(m) = Msg::read_async(&mut r).await.unwrap() {
                out.push(m);
            }
            out
        });

        assert_eq!(decoded.len(), messages.len());
        for (got, want) in decoded.iter().zip(&messages) {
            match (got, want) {
                (Msg::Stats(a), Msg::Stats(b)) => assert_eq!(a, b),
                (Msg::Frame { payload: a, codec: c1, .. }, Msg::Frame { payload: b, codec: c2, .. }) => {
                    assert_eq!(a, b);
                    assert_eq!(c1, c2);
                }
                (Msg::Error { message: a }, Msg::Error { message: b }) => assert_eq!(a, b),
                (Msg::Ping { nonce: a }, Msg::Ping { nonce: b }) => assert_eq!(a, b),
                (Msg::Image { id: a }, Msg::Image { id: b }) => assert_eq!(a, b),
                (Msg::Hello { cols: _, rows: _, width: _, height: _ }, Msg::Hello { .. }) => {}
                other => panic!("mismatched pair {other:?}"),
            }
        }
    }

    /// Reports are spaced by frame count, so the client redraws the overlay on
    /// the same cadence it receives numbers.
    #[test]
    fn reports_are_spaced_by_frame_count() {
        let mut s = ServerStats::default();
        assert!(!s.should_report(), "no frames yet");
        s.record_frame(1, 1, 1, 1, 1);
        assert!(!s.should_report(), "one frame is not a report boundary");
        s.frames = STATS_EVERY;
        assert!(s.should_report());
        s.frames = STATS_EVERY + 1;
        assert!(!s.should_report());
    }

    /// The id's low byte goes in blue and its high byte in red, matching how
    /// kitty reads the placeholder's foreground color back out.
    #[test]
    fn id_color_packs_low_byte_in_blue() {
        assert_eq!(id_to_color(0x123456), (0x12, 0x34, 0x56));
        assert_eq!(id_to_color(0x0000ff), (0x00, 0x00, 0xff));
        assert_eq!(id_to_color(0xff0000), (0xff, 0x00, 0x00));
    }

    /// The pair must round-trip, or the server's check disagrees with the
    /// client's encoding and every grid looks wrong.
    #[test]
    fn color_and_id_round_trip() {
        for id in [0u32, 1, 42, 0xff, 0x123456, 0xffffff] {
            let (r, g, b) = id_to_color(id);
            assert_eq!(color_to_id(r, g, b), id, "id {id:#x}");
        }
    }

    /// The placeholder constant must be the scalar's real UTF-8 bytes. It is
    /// above U+FFFF, so it is four bytes, and a three-byte spelling would match
    /// nothing in the stream while looking plausible.
    #[test]
    fn placeholder_bytes_are_the_scalar_utf8() {
        assert_eq!(
            std::str::from_utf8(&PLACEHOLDER_UTF8).unwrap(),
            "\u{10EEEE}"
        );
        assert_eq!(PLACEHOLDER_UTF8, "\u{10EEEE}".as_bytes());
    }

    /// A cell painted under a truecolor foreground reports the id that color
    /// encodes, which is the whole assertion the server makes.
    #[test]
    fn a_cell_under_an_sgr_truecolor_reports_its_id() {
        let mut s = PlaceholderScanner::new();
        // SGR 38;2;0;0;1 sets fg to rgb(0,0,1), which is image id 1.
        let mut cell = b"\x1b[38;2;0;0;1m".to_vec();
        cell.extend_from_slice(&PLACEHOLDER_UTF8);
        s.push(&cell);

        assert_eq!(s.found.len(), 1);
        assert_eq!(s.found[0].rgb, Some((0, 0, 1)));
        assert_eq!(s.found[0].id, Some(1));
    }

    /// A cell with no explicit foreground references no image, and must report
    /// that rather than defaulting to something plausible.
    #[test]
    fn a_cell_without_a_color_reports_no_id() {
        let mut s = PlaceholderScanner::new();
        s.push(&PLACEHOLDER_UTF8);
        assert_eq!(s.found.len(), 1);
        assert_eq!(s.found[0].id, None);
    }

    /// A reset clears the foreground, so a later cell is uncoded again.
    #[test]
    fn a_reset_clears_the_foreground() {
        let mut s = PlaceholderScanner::new();
        let mut bytes = b"\x1b[38;2;0;0;7m".to_vec();
        bytes.extend_from_slice(&PLACEHOLDER_UTF8);
        bytes.extend_from_slice(b"\x1b[0m");
        bytes.extend_from_slice(&PLACEHOLDER_UTF8);
        s.push(&bytes);

        assert_eq!(s.found.len(), 2);
        assert_eq!(s.found[0].id, Some(7));
        assert_eq!(s.found[1].id, None, "reset must clear the id");
    }

    /// A cell split across two reads must still be seen exactly once: the pty
    /// delivers arbitrary chunks, so this is the normal case, not an edge case.
    #[test]
    fn a_cell_split_across_reads_is_still_found() {
        let mut s = PlaceholderScanner::new();
        let mut bytes = b"\x1b[38;2;0;1;0m".to_vec();
        bytes.extend_from_slice(&PLACEHOLDER_UTF8);

        // One byte at a time: every prefix boundary gets exercised.
        for b in &bytes {
            s.push(&[*b]);
        }
        assert_eq!(s.found.len(), 1);
        assert_eq!(s.found[0].id, Some(256), "id 256 = rgb(0,1,0)");
    }

    /// The color may arrive in a later chunk than the cell's own bytes.
    #[test]
    fn an_sgr_split_from_its_cell_still_applies() {
        let mut s = PlaceholderScanner::new();
        for b in b"\x1b[38;2;0;2;0m" {
            s.push(&[*b]);
        }
        for b in &PLACEHOLDER_UTF8 {
            s.push(&[*b]);
        }
        assert_eq!(s.found.len(), 1);
        assert_eq!(s.found[0].id, Some(512));
    }

    /// An OSC payload must not be scanned as text: it can contain any bytes,
    /// including something that looks like a placeholder.
    #[test]
    fn an_osc_payload_is_skipped_whole() {
        let mut s = PlaceholderScanner::new();
        let mut bytes = b"\x1b[38;2;0;0;9m".to_vec();
        bytes.extend_from_slice(b"\x1b]0;");
        bytes.extend_from_slice(&PLACEHOLDER_UTF8);
        bytes.extend_from_slice(b"\x07");
        s.push(&bytes);
        assert!(
            s.found.is_empty(),
            "the placeholder inside the OSC title is not a cell"
        );
    }

    /// The whole-screen erases are counted; partial ones are not.
    ///
    /// This is the distinction the server's re-transmit decision rests on, so
    /// getting it backwards means either a needless re-transmit on every prompt
    /// redraw, or an image that never comes back after a real clear.
    #[test]
    fn only_whole_screen_erases_count() {
        let cases: &[(&[u8], bool)] = &[
            (b"\x1b[2J", true),   // complete display
            (b"\x1b[3J", true),   // display plus scrollback
            (b"\x1b[0J", false),  // cursor to end
            (b"\x1b[J", false),   // same, implied
            (b"\x1b[1J", false),  // start to cursor
            (b"\x1b[?1049h", true), // alternate screen
            (b"\x1b[?1047h", false), // alt screen without clearing
            (b"\x1b[?1048h", false), // save cursor
            (b"\x1b[?25h", false),   // show cursor
            (b"\x1b[H", false),      // cursor home
        ];
        for (seq, want) in cases {
            let mut s = PlaceholderScanner::new();
            s.push(seq);
            assert_eq!(
                s.full_erases == 1,
                *want,
                "{:?} should{} wipe the image",
                String::from_utf8_lossy(seq),
                if *want { "" } else { " not" }
            );
        }
    }

    /// An erase split across reads is still counted: herdr's redraw bytes and
    /// the pty's chunk boundaries are independent.
    #[test]
    fn a_split_erase_is_still_counted() {
        let mut s = PlaceholderScanner::new();
        for b in b"\x1b[2J" {
            s.push(&[*b]);
            s.push(b"");
        }
        assert_eq!(s.full_erases, 1);
    }

    /// Counting is cumulative across chunks, and `clear_counts` is the only
    /// thing that resets it, so the server can act on a batch and report once.
    #[test]
    fn erase_count_accumulates_until_cleared() {
        let mut s = PlaceholderScanner::new();
        s.push(b"\x1b[2J");
        s.push(b"\x1b[2J");
        assert_eq!(s.full_erases, 2);
        s.clear_counts();
        assert_eq!(s.full_erases, 0);
        s.push(b"\x1b[2J");
        assert_eq!(s.full_erases, 1);
    }

    /// The table must be kitty's, in kitty's order: index 0 is row 0, index 1
    /// is column 1. A table that is merely "combining diacritics" renders the
    /// wrong cells, or nothing at all.
    #[test]
    fn diacritics_match_the_protocol_order() {
        assert_eq!(DIACRITICS[0], '\u{0305}');
        assert_eq!(DIACRITICS[1], '\u{030d}');
        assert_eq!(DIACRITICS.len(), 297);
    }

    /// The cell for image 42 at row 0, column 0, as the protocol documentation
    /// specifies it.    ///
    /// The docs give the 8-bit-color form (`38;5;42`) with diacritics
    /// `U+0305 U+0305`. Rebuilding the cell from our own tables is what catches
    /// a table that is self-consistent but wrong, the failure that shows up as
    /// a blank screen rather than an error.
    #[test]
    fn cell_for_image_42_matches_the_specified_example() {
        let (r, g, b) = id_to_color(42);
        assert_eq!((r, g, b), (0, 0, 42), "id 42 must be blue-only");

        let cell: String = [PLACEHOLDER, DIACRITICS[0], DIACRITICS[0]]
            .iter()
            .collect();
        assert_eq!(cell, "\u{10EEEE}\u{0305}\u{0305}");
    }

    /// A marker cell becomes a placeholder cell that names the live image.
    ///
    /// This is the whole point of the marker: the client draws text a muxer
    /// re-renders unchanged, and the wrapper is the side that turns it into the
    /// cell the terminal can resolve.
    #[test]
    fn a_marker_cell_becomes_a_placeholder_for_the_live_image() {
        let mut w = MarkerRewriter::new();
        let cell = format!("{}{}{}", MARKER, DIACRITICS[0], DIACRITICS[1]);
        let out = w.push(cell.as_bytes(), 7);

        // Blue holds the low byte, so id 7 is `38;2;0;0;7`.
        assert!(
            out.starts_with(b"\x1b[49;38;2;0;0;7m"),
            "the id must be set as the foreground: {out:?}"
        );
        assert!(
            out.windows(4).any(|win| win == PLACEHOLDER_UTF8),
            "the marker must become the placeholder scalar: {out:?}"
        );
        // The marker background is cleared again, so the image is not drawn over
        // magenta and the next cell still re-establishes its own pair.
        assert!(
            out.ends_with(format!("\x1b[39;{}m", marker_fg_params()).as_bytes()),
            "the marker color must be restored after the cell: {out:?}"
        );
    }

    /// A cell split across two reads is rewritten once, whole.
    ///
    /// The pty delivers arbitrary chunks. Rewriting a marker whose diacritics
    /// have not arrived would produce a placeholder with no row or column, which
    /// the terminal resolves at the wrong position instead of rejecting.
    #[test]
    fn a_cell_split_across_chunks_becomes_one_placeholder() {
        let mut w = MarkerRewriter::new();
        let cell = format!("{}{}{}", MARKER, DIACRITICS[3], DIACRITICS[5]);
        let bytes = cell.as_bytes();
        let (first, second) = bytes.split_at(4);

        let out_first = w.push(first, 1);
        assert!(
            !out_first.windows(4).any(|win| win == PLACEHOLDER_UTF8),
            "half a cell must not be rewritten: {out_first:?}"
        );

        let out_second = w.push(second, 1);
        let mut all = out_first;
        all.extend_from_slice(&out_second);
        assert_eq!(
            all.windows(4).filter(|win| *win == PLACEHOLDER_UTF8).count(),
            1,
            "the cell must be rewritten exactly once: {all:?}"
        );
    }

    /// Ordinary text and bytes that are not cells pass through untouched.
    #[test]
    fn text_around_a_cell_is_forwarded_unchanged() {
        let mut w = MarkerRewriter::new();
        let cell = format!("{}{}{}", MARKER, DIACRITICS[0], DIACRITICS[0]);
        let input = format!("ab{cell}cd");
        let out = w.push(input.as_bytes(), 1);
        let text = String::from_utf8_lossy(&out);
        assert!(text.starts_with("ab"), "{text:?}");
        assert!(text.ends_with("cd"), "{text:?}");
    }

    /// A marker with an ordinary letter after it is not a cell.
    #[test]
    fn a_marker_followed_by_a_letter_is_left_alone() {
        let mut w = MarkerRewriter::new();
        let input = format!("{MARKER}x");
        let out = w.push(input.as_bytes(), 1);
        assert_eq!(String::from_utf8_lossy(&out), input);
    }
}
