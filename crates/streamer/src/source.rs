//! The codec-agnostic abstraction: a [`FrameSource`] is anything that yields
//! decoded pixel frames.
//!
//! Implementations (future): cage/screencopy capture, video files, network
//! sources. The TGP emitter only ever talks to this trait, so the streaming
//! codec stays irrelevant downstream.

use std::time::Duration;

use async_trait::async_trait;

use crate::frame::{Frame, FrameMetadata};
/// Hint about how the source produces frames, used by the emitter to pick a
/// TGP streaming strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Cadence {
    /// A live, unbounded stream (capture, camera). Frames arrive as they are
    /// produced; no total count.
    #[default]
    Live,
    /// A bounded clip / file with known duration.
    Finite {
        /// Total playtime if known.
        duration: Option<Duration>,
    },
}

/// Where a stream stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamState {
    /// Streaming normally.
    Active,
    /// Source is alive but has nothing new (idle desktop, paused video).
    Idle,
    /// Source finished (file ended, window closed).
    Ended,
}

/// A non-blocking update from a [`FrameSource`].
#[derive(Debug)]
pub enum FrameUpdate {
    /// A new frame (or deltas against `base_frame`, if the source tracks it).
    Frame(Frame),
    /// Stream-level metadata changed (e.g. the window was resized). The next
    /// frame is guaranteed to use the new metadata.
    Resized(FrameMetadata),
    /// Nothing new right now.
    Idle,
    /// The stream is over.
    Ended,
}

/// Anything that can produce a stream of decoded pixel frames.
///
/// This is the boundary that makes the codec irrelevant: the TGP emitter is
/// written against `FrameSource`, never against a particular encoder.
#[async_trait]
pub trait FrameSource: Send {
    /// Static description of the stream. `Resized` updates may change it later.
    fn metadata(&self) -> FrameMetadata;

    /// Production pattern of this source.
    fn cadence(&self) -> Cadence {
        Cadence::Live
    }

    /// Pull the next update.
    ///
    /// Returns `Ok(None)` only on a transport error that ends the stream
    /// prematurely (the normal end is `Ok(Some(FrameUpdate::Ended))`).
    async fn next(&mut self) -> Result<Option<FrameUpdate>, crate::source::SourceError>;

    /// Convert into a [`futures_core::Stream`] of updates for stream-combinator
    /// style consumers. The default impl adapts `next` with a worker task.
    fn into_stream(self) -> FrameStream
    where
        Self: Sized + 'static,
    {
        FrameStream::from_source(self)
    }
}

/// Errors a source can surface.
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("source transport failed: {0}")]
    Transport(String),

    #[error("source produced an invalid frame: {0}")]
    InvalidFrame(String),
}

/// A [`futures_core::Stream`] of updates from a source.
pub struct FrameStream {
    inner: tokio_stream::wrappers::ReceiverStream<Result<FrameUpdate, SourceError>>,
    handle: tokio::task::JoinHandle<()>,
}

impl FrameStream {
    fn from_source<S: FrameSource + 'static>(mut source: S) -> Self {
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let handle = tokio::spawn(async move {
            loop {
                match source.next().await {
                    Ok(Some(update)) => {
                        let is_ended = matches!(update, FrameUpdate::Ended);
                        if tx.send(Ok(update)).await.is_err() {
                            break; // consumer dropped
                        }
                        if is_ended {
                            break;
                        }
                    }
                    Ok(None) => {
                        let _ = tx
                            .send(Err(SourceError::Transport("source died".into())))
                            .await;
                        break;
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e)).await;
                        break;
                    }
                }
            }
        });
        FrameStream {
            inner: tokio_stream::wrappers::ReceiverStream::new(rx),
            handle,
        }
    }

    /// Abort the background pump without draining.
    pub fn abort(&mut self) {
        self.handle.abort();
    }
}

impl futures_core::Stream for FrameStream {
    type Item = Result<FrameUpdate, SourceError>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        std::pin::pin!(&mut self.inner).poll_next(cx)
    }
}

impl Drop for FrameStream {
    fn drop(&mut self) {
        self.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{PixelFormat, Rect};

    /// A source that emits N full frames then ends.
    struct CountingSource {
        meta: FrameMetadata,
        remaining: u32,
    }

    #[async_trait]
    impl FrameSource for CountingSource {
        fn metadata(&self) -> FrameMetadata {
            self.meta.clone()
        }

        async fn next(&mut self) -> Result<Option<FrameUpdate>, SourceError> {
            if self.remaining == 0 {
                return Ok(Some(FrameUpdate::Ended));
            }
            self.remaining -= 1;
            let len =
                (self.meta.width * self.meta.height) as usize * self.meta.format.bytes_per_pixel();
            Ok(Some(FrameUpdate::Frame(Frame::full(
                self.meta.clone(),
                bytes::Bytes::from(vec![0u8; len]),
            ))))
        }
    }

    #[tokio::test]
    async fn stream_yields_frames_then_ends() {
        let src = CountingSource {
            meta: FrameMetadata {
                width: 2,
                height: 2,
                format: PixelFormat::Rgba32,
            },
            remaining: 3,
        };
        use std::pin::pin;
        use tokio_stream::StreamExt as _;

        let mut s = pin!(src.into_stream());
        let mut frames = 0;
        loop {
            match s.next().await {
                Some(Ok(FrameUpdate::Frame(_))) => frames += 1,
                Some(Ok(FrameUpdate::Ended)) => break,
                other => panic!("unexpected: {other:?}"),
            }
        }
        assert_eq!(frames, 3);
    }

    #[tokio::test]
    async fn damage_defaults_to_full_frame() {
        let r = Rect {
            x: 1,
            y: 1,
            width: 2,
            height: 2,
        };
        let f = Frame {
            metadata: FrameMetadata {
                width: 4,
                height: 4,
                format: PixelFormat::Rgb24,
            },
            data: bytes::Bytes::from(vec![0u8; 4 * 4 * 3]),
            presentation_timestamp: None,
            timing: None,
            damage: vec![r],
        };
        assert_eq!(f.effective_damage(), &[r]);
    }
}
