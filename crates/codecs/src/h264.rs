//! H.264, as one encoder and one decoder.
//!
//! One crate provides both halves — `rusty_h264`, pure Rust, SIMD on by
//! default — so the two halves of a session agree on the codec by sharing a
//! dependency, and nothing the machine runs the binary on is asked for
//! anything: no GPU, no driver, no C to link.
//!
//! The encoder runs `Fast`, because the budget this encoder exists inside is
//! a frame: the slow presets spend wall time on quality nothing on a terminal
//! grid can see. Lookahead is zero for the same reason — a preset that holds
//! a frame back to look at what follows it is a pane that lags.
//!
//! The color conversions around the codec are `yuvutils-rs`, SIMD and all:
//! the capture speaks B,G,R,A, the codec speaks I420, and that translation is
//! not this file's to hand-maintain.

use rusty_h264::Preset;
use yuvutils_rs::{
    BufferStoreMut, YuvConversionMode, YuvPlanarImage, YuvPlanarImageMut, YuvRange,
    YuvStandardMatrix, bgra_to_rgba, rgba_to_yuv420, yuv420_to_rgb,
};

use crate::{Error, Frame};

/// The stream's shape. The encoder runs at this and the wire assumes it.
///
// TODO: make these configurable — a session that wants a different framerate,
// bitrate or keyframe cadence has to be able to ask.
pub const FRAMERATE: f32 = 30.0;
pub const GOP: u32 = 30;
const BPS: u32 = 2_000_000;

/// Encode B,G,R,A pixels into an Annex-B H.264 stream.
pub struct Encoder {
    inner: rusty_h264::Encoder,
}

impl Encoder {
    /// Build the encoder for a stream of `width` x `height`, both even, because
    /// 4:2:0 chroma needs it.
    ///
    /// # Errors
    ///
    /// Fails when the codec refuses the settings, which for even dimensions and
    /// these parameters is a bug in the codec rather than in the caller.
    pub fn new(width: u32, height: u32) -> Result<Self, Error> {
        let mut config = rusty_h264::EncoderConfig::new(width as usize, height as usize);
        config.preset = Preset::Fast;
        config.bitrate = BPS;
        config.framerate = FRAMERATE;
        config.gop_size = GOP;
        config.lookahead = 0;
        let inner = rusty_h264::Encoder::new(config).map_err(|e| Error::Encode {
        width,
        height,
        source: Box::new(e),
        })?;
        Ok(Self { inner })
    }
}

impl crate::Encoder for Encoder {
    #[tracing::instrument(name = "codec.encode", skip_all)]
    fn encode(
        &mut self,
        bgra: &[u8],
        width: u32,
        height: u32,
        force_keyframe: bool,
    ) -> Result<Vec<u8>, Error> {
        assert!(
            bgra.len() == width as usize * height as usize * 4,
            "a capture is {}x{}x4 B,G,R,A, and this one is {} bytes",
            width,
            height,
            bgra.len()
        );
        if force_keyframe {
            self.inner.request_keyframe();
        }
        let frame = bgra_to_i420(bgra, width, height)?;
        Ok(self.inner.encode(&frame))
    }
}

/// Decode an Annex-B H.264 stream back into pixels.
///
/// A decoder that has decoded at least one picture never answers with nothing:
/// a stretch of stream that carries no new picture (headers, or a frame the
/// codec held back) is answered with the last picture, which is what the pane
/// is already showing and what a repaint wants anyway.
pub struct Decoder {
    inner: rusty_h264::Decoder,
    /// The last picture decoded, which is the answer to a stretch of stream
    /// that produced nothing new. `None` only before the first picture.
    last: Option<Frame>,
}

impl Decoder {
    /// Build the decoder. It learns the stream's shape from its first headers,
    /// which is why one is built before any frame arrives.
    pub fn new() -> Self {
        Self {
            inner: rusty_h264::Decoder::new(),
            last: None,
        }
    }
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl crate::Decoder for Decoder {
    #[tracing::instrument(name = "codec.decode", skip_all)]
    fn decode(&mut self, stream: &[u8]) -> Result<Option<Frame>, Error> {
        match self.inner.decode(stream) {
            Ok(Some(yuv)) => {
                let (width, height) = (yuv.width as u32, yuv.height as u32);
                let frame = Frame {
                    width,
                    height,
                    rgb: i420_to_rgb(&yuv.y, &yuv.u, &yuv.v, width, height)?,
                };
                self.last = Some(frame.clone());
                Ok(Some(frame))
            }
            // Nothing new in this stretch of stream, and something already
            // shown: the last picture is the answer.
            Ok(None) => Ok(self.last.clone()),
            Err(e) => Err(Error::Decode {
                len: stream.len(),
                source: Box::new(e),
            }),
        }
    }
}

/// B,G,R,A capture pixels into the codec's I420 frame.
///
/// The capture is a Wayland `Argb8888` buffer copied byte for byte, so its
/// bytes are B, G, R, A on a little-endian machine. The conversion library
/// speaks R,G,B,A, so the channel order is traded first; the color math and
/// the 4:2:0 subsample are its SIMD code, not ours.
#[tracing::instrument(name = "codec.bgra_to_i420", skip_all)]
fn bgra_to_i420(bgra: &[u8], width: u32, height: u32) -> Result<rusty_h264::YuvFrame, Error> {
    let mut rgba = vec![0u8; bgra.len()];
    bgra_to_rgba(bgra, width * 4, &mut rgba, width * 4, width, height)?;

    let (w, h) = (width as usize, height as usize);
    let (y, u, v) = (
        vec![0u8; w * h],
        vec![0u8; w * h / 4],
        vec![0u8; w * h / 4],
    );
    let mut image = YuvPlanarImageMut {
        y_plane: BufferStoreMut::Owned(y),
        y_stride: width,
        u_plane: BufferStoreMut::Owned(u),
        u_stride: width / 2,
        v_plane: BufferStoreMut::Owned(v),
        v_stride: width / 2,
        width,
        height,
    };
    rgba_to_yuv420(
        &mut image,
        &rgba,
        width * 4,
        YuvRange::Full,
        YuvStandardMatrix::Bt601,
        YuvConversionMode::Balanced,
    )?;
    // The planes were constructed owned, so what came back is the buffer, and
    // the codec's frame takes it without a copy.
    let (y, u, v) = match (image.y_plane, image.u_plane, image.v_plane) {
        (BufferStoreMut::Owned(y), BufferStoreMut::Owned(u), BufferStoreMut::Owned(v)) => (y, u, v),
        _ => unreachable!("the planes were constructed owned"),
    };
    Ok(rusty_h264::YuvFrame {
        width: w,
        height: h,
        y,
        u,
        v,
    })
}

/// The codec's I420 planes into R,G,B pixels.
///
/// The conversion library's own way back lands in R,G,B,A, and the alpha is
/// discarded: a frame painted into the terminal is opaque, so the fourth byte
/// would be 255 in every pixel and is not carried.
#[tracing::instrument(name = "codec.i420_to_rgb", skip_all)]
fn i420_to_rgb(y: &[u8], u: &[u8], v: &[u8], width: u32, height: u32) -> Result<Vec<u8>, Error> {
    let mut rgb = vec![0u8; width as usize * height as usize * 3];
    yuv420_to_rgb(
        &YuvPlanarImage {
            y_plane: y,
            y_stride: width,
            u_plane: u,
            u_stride: width / 2,
            v_plane: v,
            v_stride: width / 2,
            width,
            height,
        },
        &mut rgb,
        width * 3,
        YuvRange::Full,
        YuvStandardMatrix::Bt601,
    )?;
    Ok(rgb)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Decoder as _;

    /// A decoder that has shown a picture keeps showing it when the stream
    /// says nothing new.
    #[test]
    fn the_last_frame_is_the_answer_to_a_quiet_stream() {
        let mut decoder = Decoder::new();
        let mut encoder = Encoder::new(16, 16).expect("an encoder");
        let bgra = vec![0u8, 0, 255, 255].repeat(16 * 16);
        let stream = crate::Encoder::encode(&mut encoder, &bgra, 16, 16, false)
            .expect("encode a frame");
        assert!(!stream.is_empty(), "the first frame carries its picture");

        let first = decoder.decode(&stream).expect("decodes").expect("a picture");
        let again = decoder.decode(&stream).expect("decodes");
        assert_eq!(again.as_ref().map(|f| f.rgb.as_slice()), Some(first.rgb.as_slice()));
    }
}
