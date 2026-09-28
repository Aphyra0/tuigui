//! tuigui-streamer: the codec-agnostic streaming abstraction.
//!
//! The core type is [`FrameSource`]: anything that can produce a stream of
//! decoded pixel frames ([`Frame`]). The codec that produced those frames
//! (raw capture, tile deltas, a video decoder, ...) is irrelevant to
//! consumers: downstream code is written against `FrameSource`, never
//! against a particular encoder.
//!
//! Wire encoding lives in the separate `tuigui-tgp` crate.

pub mod blocks;
pub mod frame;
pub mod mp4;
pub mod pink;
pub mod source;

pub use blocks::{BlockGrid, BlockGridDim, MicroBlock, DEFAULT_BLOCKS_PER_SIDE};
pub use frame::{Frame, FrameMetadata, PixelFormat, Rect};
pub use mp4::Mp4VideoSource;
pub use pink::PinkFrameSource;
pub use source::{Cadence, FrameSource, FrameStream, FrameUpdate, SourceError};
