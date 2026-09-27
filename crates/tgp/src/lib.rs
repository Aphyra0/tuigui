//! tuigui-tgp: kitty graphics protocol (TGP) wire encoding + session encoder.
//!
//! - [`proto`]: pure escape-sequence construction (no state, no I/O).
//! - [`encoder`]: stateful session: consumes a
//!   [`tuigui_streamer::FrameSource`] and produces a `Stream` of TGP bytes.

pub mod encoder;
pub mod proto;

pub use encoder::{EncoderConfig, EncoderError, EncoderEvent, TgpEncoder};
pub use proto::Action;
