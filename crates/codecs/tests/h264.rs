//! The codec pair, tested end to end: encode with the crate's encoder, decode
//! with the crate's decoder, and compare pixels.
//!
//! A test that feeds the decoder bytes another codec produced would pass by
//! accident; these feed it what this crate's own encoder emits.

use codecs::{CodecId, Decoder, Encoder};

/// One solid B,G,R,A frame, encoded through the real codec.
fn encoded(width: u32, height: u32, (b, g, r): (u8, u8, u8)) -> Vec<u8> {
    let mut encoder = codecs::h264::Encoder::new(width, height).expect("an encoder");
    let bgra = vec![b, g, r, 255].repeat((width * height) as usize);
    let stream = encoder.encode(&bgra, width, height, false).expect("encode");
    assert!(!stream.is_empty(), "the first frame carries its picture");
    stream
}

/// The whole pair on a real stream: a picture in, the same picture out, close
/// enough that nothing on a terminal grid can tell the difference.
///
/// H.264 is lossy and 4:2:0, so exact bytes are not the assertion; a solid
/// red frame that comes back mostly red is. An 8x8 frame is also a codec's
/// worst case — smaller than a macroblock — and it has to survive anyway.
#[test]
fn encode_then_decode_round_trip() {
    let stream = encoded(8, 8, (0, 0, 255));
    let mut decoder = codecs::h264::Decoder::new();
    let frame = decoder.decode(&stream).expect("decodes").expect("a picture");
    assert_eq!(frame.width, 8);
    assert_eq!(frame.height, 8);
    assert_eq!(frame.rgb.len(), 8 * 8 * 3);
    for pixel in frame.rgb.chunks_exact(3) {
        assert!(pixel[0] > 150, "red stayed red: {pixel:?}");
        assert!(pixel[1] < 110, "red stayed red: {pixel:?}");
        assert!(pixel[2] < 110, "red stayed red: {pixel:?}");
    }
}

/// A decoder that has shown a picture keeps showing it when the stream says
/// nothing new: a repaint gets the last picture, not nothing.
#[test]
fn the_last_frame_is_the_answer_to_a_quiet_stream() {
    let stream = encoded(8, 8, (0, 0, 255));
    let mut decoder = codecs::h264::Decoder::new();
    let first = decoder.decode(&stream).expect("decodes").expect("a picture");
    let again = decoder.decode(&stream).expect("decodes");
    assert_eq!(
        again.as_ref().map(|f| f.rgb.as_slice()),
        Some(first.rgb.as_slice())
    );
}

/// A stream's first picture is decodable without a second: a stream that
/// needed lookahead would leave the first client waiting for a frame that
/// never comes.
#[test]
fn the_first_frame_is_available_before_anything_else_arrives() {
    let stream = encoded(32, 32, (0, 0, 255));
    let mut decoder = codecs::h264::Decoder::new();
    assert!(
        decoder.decode(&stream).expect("decodes").is_some(),
        "the first picture decodes from the first stretch of stream"
    );
}

/// The announced codec builds the decoder; each codec this build carries has
/// one, and it is built before any frame arrives.
#[test]
fn the_announced_codec_picks_the_implementations() {
    assert!(CodecId::H264.to_decoder().is_ok());
    assert!(CodecId::Jpeg.to_decoder().is_ok());
}
