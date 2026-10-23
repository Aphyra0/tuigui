//! JPEG, as one encoder and one decoder.
//!
//! The codec is libjpeg-turbo via the `turbojpeg` crate: SIMD, linked through
//! pkg-config against the dev shell's library, the same pair the project
//! streamed with before the H.264 path existed.
//!
//! A JPEG has no stream history: every frame is a complete picture on its own,
//! so nothing is kept between frames and a keyframe request means nothing.
//! What crosses the wire is one picture per frame, and what arrives back is
//! exactly that picture.

use turbojpeg::{PixelFormat, decompress};

use crate::{Error, Frame};

/// JPEG quality for a captured window.
///
/// The same value the video path uses, and for the same reason: these pixels are on their
/// way to a grid of terminal cells, where legibility is bought long before fidelity is.
const QUALITY: i32 = 80;

/// Encode B,G,R,A pixels into a JPEG picture.
pub struct Encoder {
    inner: turbojpeg::Compressor,
}

impl Encoder {
    /// Build the encoder.
    ///
    /// # Errors
    ///
    /// Fails when libjpeg-turbo cannot be reached, which a linked build does
    /// not do.
    pub fn new() -> Result<Self, Error> {
        let mut inner = turbojpeg::Compressor::new().map_err(Error::boxed)?;
        inner.set_quality(QUALITY).map_err(Error::boxed)?;
        Ok(Self { inner })
    }
}

impl crate::Encoder for Encoder {
    fn encode(
        &mut self,
        bgra: &[u8],
        width: u32,
        height: u32,
        _force_keyframe: bool,
    ) -> Result<Vec<u8>, Error> {
        assert!(
            bgra.len() == width as usize * height as usize * 4,
            "a capture is {}x{}x4 B,G,R,A, and this one is {} bytes",
            width,
            height,
            bgra.len()
        );
        // The capture's own layout: B, G, R and a fourth byte the encoder
        // ignores, so a frame is handed over as it was captured.
        self.inner
            .compress_to_vec(turbojpeg::Image {
                pixels: bgra,
                width: width as usize,
                pitch: width as usize * 4,
                height: height as usize,
                format: PixelFormat::BGRX,
            })
            .map_err(|e| Error::Encode {
                width,
                height,
                source: Box::new(e),
            })
    }
}

/// Decode a JPEG picture back into pixels.
///
/// Nothing is kept between pictures: a JPEG carries its whole shape, and the
/// free function is the whole of the decode.
pub struct Decoder;

impl Decoder {
    /// Build the decoder.
    ///
    /// # Errors
    ///
    /// Fails when libjpeg-turbo cannot be reached, which a linked build does
    /// not do.
    pub fn new() -> Result<Self, Error> {
        Ok(Self)
    }
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new().expect("a JPEG decoder")
    }
}

impl crate::Decoder for Decoder {
    #[tracing::instrument(name = "codec.decode", skip_all)]
    fn decode(&mut self, stream: &[u8]) -> Result<Option<Frame>, Error> {
        let image = decompress(stream, PixelFormat::RGB).map_err(|e| Error::Decode {
            len: stream.len(),
            source: Box::new(e),
        })?;
        Ok(Some(Frame {
            width: image.width as u32,
            height: image.height as u32,
            rgb: image.pixels,
        }))
    }
}
