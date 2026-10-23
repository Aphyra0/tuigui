//! JPEG, tested end to end: encode with the crate's encoder, decode with the
//! crate's decoder, and compare pixels.

use codecs::{CodecId, Decoder, Encoder};

/// The whole JPEG pair on a real picture: a picture in, the same picture out.
///
/// JPEG is lossy, so the assertion is a solid color close enough to what was
/// sent, and the frame arrives as a complete picture the first time — a JPEG
/// has no stream history to wait for.
#[test]
fn jpeg_encode_then_decode_round_trip() {
    let mut encoder = codecs::jpeg::Encoder::new().expect("an encoder");
    let bgra = vec![0u8, 0, 255, 255].repeat(16 * 16);
    let jpeg = encoder.encode(&bgra, 16, 16, false).expect("encode");
    assert!(!jpeg.is_empty(), "a JPEG picture is one frame");

    let mut decoder = codecs::jpeg::Decoder::new().expect("a decoder");
    let frame = decoder.decode(&jpeg).expect("decodes").expect("a picture");
    assert_eq!(frame.width, 16);
    assert_eq!(frame.height, 16);
    for pixel in frame.rgb.chunks_exact(3) {
        assert!(pixel[0] > 150, "red stayed red: {pixel:?}");
        assert!(pixel[1] < 110, "red stayed red: {pixel:?}");
        assert!(pixel[2] < 110, "red stayed red: {pixel:?}");
    }
}

/// A JPEG picture is its own keyframe: a repaint of a stream that said nothing
/// new still has the picture.
#[test]
fn the_last_frame_is_the_answer_to_a_quiet_stream() {
    let mut encoder = codecs::jpeg::Encoder::new().expect("an encoder");
    let bgra = vec![0u8, 0, 255, 255].repeat(16 * 16);
    let jpeg = encoder.encode(&bgra, 16, 16, false).expect("encode");

    let mut decoder = codecs::jpeg::Decoder::new().expect("a decoder");
    let first = decoder.decode(&jpeg).expect("decodes").expect("a picture");
    let again = decoder.decode(&jpeg).expect("decodes");
    assert_eq!(
        again.as_ref().map(|f| f.rgb.as_slice()),
        Some(first.rgb.as_slice())
    );
}

/// The announced codec builds the decoder; each codec this build carries has
/// one, and it is built before any frame arrives.
#[test]
fn the_announced_codec_picks_the_implementations() {
    assert!(CodecId::H264.to_decoder().is_ok());
    assert!(CodecId::Jpeg.to_decoder().is_ok());
}
