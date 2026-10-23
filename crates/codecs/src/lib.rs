//! `codecs` — the codec pair a session streams with.
//!
//! The client encodes, the server decodes, and the two halves agree on the
//! codec by sharing this crate rather than by documenting one. Which codec
//! that is sits behind [`Encoder`] and [`Decoder`]: the traits say what a
//! session needs of a codec — bytes in, pixels out, pixels in, bytes out —
//! and nothing outside this crate may assume which one answers.
//!
//! A codec has state, and the state is the only thing a caller keeps: one
//! encoder for a session, one decoder for a session, both built once. What
//! crosses the wire is whatever [`Encoder`] produced, and a decoder that
//! cannot follow it is rebuilt from a fresh keyframe rather than from its
//! own history.

pub mod h264;
pub mod jpeg;

/// The codec a session streams with, named so both halves can agree.
///
/// Carried by [`wire::message::Message::InitStreaming`], which is the client
/// telling the server which decoder to build: the server has every codec
/// compiled in, and this is the fact that picks one. A third-party client that
/// speaks a codec of its own is one new variant away from being understood.
#[derive(Debug, Clone, Copy, PartialEq, Eq, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub enum CodecId {
    /// H.264, as Annex-B.
    H264,
    /// JPEG, one still frame per message.
    Jpeg,
}

/// Encode capture pixels into a stream the wire can carry, one frame at a time.
pub trait Encoder {
    /// Encode one frame of B,G,R,A pixels at `width` x `height`, and return the
    /// bytes to send: headers before the first picture, pictures after that.
    ///
    /// Empty output is not an error and not a frame: a codec may hold a frame
    /// back while it looks ahead, and a caller that got nothing has nothing to
    /// send. `force_keyframe` asks the next frame to be one the decoder can
    /// start from, which is the only answer to a decoder that lost its state.
    fn encode(
        &mut self,
        bgra: &[u8],
        width: u32,
        height: u32,
        force_keyframe: bool,
    ) -> Result<Vec<u8>, Error>;
}

/// Decode what an [`Encoder`] produced, back into pixels, one frame at a time.
pub trait Decoder {
    /// Decode one stretch of the stream, and return the pixels of the frame
    /// that came due, as R,G,B at the frame's own size.
    ///
    /// `None` is not an error and not the end: a stream opens with headers
    /// that produce no picture, and a codec may hold a frame back for
    /// reference. A caller that got nothing has nothing to show yet.
    fn decode(&mut self, stream: &[u8]) -> Result<Option<Frame>, Error>;
}

impl CodecId {
    /// Build the decoder for the codec the client announced, which the server
    /// keeps for the session.
    ///
    /// # Errors
    ///
    /// Fails for a codec this build does not carry, which is the server's to
    /// report back as an [`wire::message::Message::Error`] rather than a guess.
    /// Build the decoder for the codec the client announced, which the server
    /// keeps for the session.
    ///
    /// # Errors
    ///
    /// Fails for a codec this build does not carry, which is the server's to
    /// report back as an [`wire::message::Message::Error`] rather than a guess.
    pub fn to_decoder(self) -> Result<Box<dyn Decoder>, Error> {
        match self {
            Self::H264 => Ok(Box::new(h264::Decoder::new())),
            Self::Jpeg => Ok(Box::new(jpeg::Decoder::new()?)),
        }
    }
}

/// One decoded frame: R,G,B pixels at a size.
///
/// Tight and row-major, because a decoder that gives a stride makes the caller
/// carry the stride, and every consumer here paints whole frames.
#[derive(Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// `width * height * 3` bytes of R, G, B.
    pub rgb: Vec<u8>,
}

impl Frame {
    /// A frame of one solid color, for callers that have no picture yet but
    /// need pixels.
    ///
    /// Exists for the debug path that paints without a codec, which wants the
    /// same shape a real decode produces.
    pub fn solid(width: u32, height: u32, (r, g, b): (u8, u8, u8)) -> Self {
        Self {
            width,
            height,
            rgb: {
                let mut rgb = Vec::with_capacity(width as usize * height as usize * 3);
                for _ in 0..width as usize * height as usize {
                    rgb.extend_from_slice(&[r, g, b]);
                }
                rgb
            },
        }
    }
}

impl std::fmt::Debug for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Frame")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("rgb", &self.rgb.len())
            .finish()
    }
}

/// What can go wrong with a codec, as one type for both halves.
///
/// A caller cannot fix a codec error; what it does with one is decided by
/// whose side it is on, and the type exists so that decision is a match arm
/// rather than a string comparison. Each codec's own error travels as the
/// source, so its wording is not lost.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The encoder refused a frame.
    #[error("encoding a {width}x{height} frame: {source}")]
    Encode {
        width: u32,
        height: u32,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// The decoder refused a stretch of stream.
    #[error("decoding {len} bytes of stream: {source}")]
    Decode {
        len: usize,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// The codec itself could not be built.
    #[error("the codec could not be built: {0}")]
    Build(Box<dyn std::error::Error + Send + Sync>),

    /// A codec this build does not carry was asked for.
    #[error("the {0:?} codec is not implemented")]
    Unsupported(CodecId),

    /// A pixel conversion between the capture's colors and the codec's failed.
    #[error("converting pixels: {0}")]
    Conversion(#[from] yuvutils_rs::YuvError),
}

impl Error {
    /// A codec's own error, as the source of this one.
    pub fn boxed(error: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Build(Box::new(error))
    }
}
