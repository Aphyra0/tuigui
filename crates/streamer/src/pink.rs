//! A [`FrameSource`] that emits full pink frames.
//!
//! Useful for debugging the TGP display pipeline without a camera, video file,
//! or a running compositor: every frame is a solid pink (255,105,180) RGBA
//! image of the chosen size. `--debug`/`--debug-streaming` use this.

use async_trait::async_trait;
use bytes::Bytes;

use crate::frame::{Frame, FrameMetadata, PixelFormat};
use crate::source::{Cadence, FrameSource, FrameUpdate, SourceError};

/// Emit `count` solid-pink frames, then `Ended`. `None` means emit forever
/// (a live, never-ending stream).
pub struct PinkFrameSource {
    width: u32,
    height: u32,
    remaining: Option<u64>,
}

impl PinkFrameSource {
    pub fn new(width: u32, height: u32, count: Option<u64>) -> Self {
        PinkFrameSource {
            width,
            height,
            remaining: count,
        }
    }

    /// A never-ending pink stream.
    pub fn infinite(width: u32, height: u32) -> Self {
        Self::new(width, height, None)
    }

    fn frame(&self) -> Frame {
        let n = (self.width as usize) * (self.height as usize);
        // RGBA pink: R=255 G=105 B=180 A=255.
        let mut data = vec![0u8; n * 4];
        for px in data.chunks_mut(4) {
            px.copy_from_slice(&[255, 105, 180, 255]);
        }
        Frame::full(
            FrameMetadata {
                width: self.width,
                height: self.height,
                format: PixelFormat::Rgba32,
            },
            Bytes::from(data),
        )
    }
}

#[async_trait]
impl FrameSource for PinkFrameSource {
    fn metadata(&self) -> FrameMetadata {
        FrameMetadata {
            width: self.width,
            height: self.height,
            format: PixelFormat::Rgba32,
        }
    }

    fn cadence(&self) -> Cadence {
        match self.remaining {
            Some(_) => Cadence::Finite { duration: None },
            None => Cadence::Live,
        }
    }

    async fn next(&mut self) -> Result<Option<FrameUpdate>, SourceError> {
        match self.remaining {
            Some(0) => Ok(Some(FrameUpdate::Ended)),
            Some(n) => {
                self.remaining = Some(n - 1);
                Ok(Some(FrameUpdate::Frame(self.frame())))
            }
            None => Ok(Some(FrameUpdate::Frame(self.frame()))),
        }
    }
}