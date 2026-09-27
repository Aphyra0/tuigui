//! The stateful TGP session encoder.
//!
//! Consumes [`FrameSource`] updates and produces the byte stream a terminal
//! should receive: transmit/place for the first frame, then animation-frame
//! delta patches for damaged regions. The output is a
//! [`futures_core::Stream`] of byte chunks — nothing is written to stdout;
//! the caller owns the sink (PTY, SSH channel, file, ...).

use futures_core::Stream;
use tokio::sync::mpsc;

use crate::proto as tgp;
use tuigui_streamer::frame::{Frame, FrameMetadata, PixelFormat, Rect};
use tuigui_streamer::source::{FrameSource, FrameUpdate, SourceError};

/// Streaming strategy for the encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Strategy {
    /// Full re-transmit per frame. Simple, correct, bandwidth-hungry. Fine for
    /// small windows and as the correctness baseline.
    FullRetransmit,
    /// First frame transmitted + placed, then one `a=f` patch per damage rect
    /// against the base image. Bandwidth-friendly for UI-class damage.
    #[default]
    DeltaFrames,
}

/// What the encoder tells its consumer.
#[derive(Debug)]
pub enum EncoderEvent {
    /// Bytes to write to the terminal, in order.
    Bytes(Vec<u8>),
    /// The source ended.
    Ended,
}

/// Errors from the encoder loop.
#[derive(Debug, thiserror::Error)]
pub enum EncoderError {
    #[error("source error: {0}")]
    Source(#[from] SourceError),
    #[error("frame metadata changed without a Resized update")]
    MetadataMismatch,
}

/// Configuration for [`TgpEncoder`].
#[derive(Debug, Clone)]
pub struct EncoderConfig {
    pub strategy: Strategy,
    /// Consecutive frame number the first delta is based on. The base image
    /// itself is frame 1.
    pub first_delta_base_frame: u32,
    /// On-screen placement size in terminal cells. When both are `>0` every
    /// placed image is scaled to fill exactly this `columns x rows` area;
    /// without these the image is shown at one screen cell per source pixel,
    /// so a full-resolution frame overflows the terminal and only its top-left
    /// corner is visible. Set both from the TUI's pane size.
    pub placement_columns: u32,
    pub placement_rows: u32,
}

impl Default for EncoderConfig {
    fn default() -> Self {
        EncoderConfig {
            strategy: Strategy::default(),
            first_delta_base_frame: 1,
            placement_columns: 0,
            placement_rows: 0,
        }
    }
}

/// Converts a [`FrameSource`] into a stream of TGP bytes.
///
/// The encoder owns the session state: which image id is in use, whether the
/// base image has been placed, and the running frame counter.
pub struct TgpEncoder {
    config: EncoderConfig,
    image_id: u32,
}

impl TgpEncoder {
    pub fn new(config: EncoderConfig) -> Self {
        TgpEncoder {
            config,
            // Image ids are a shared namespace with other TGP programs; start
            // from a nonzero pseudorandom base to make collisions unlikely.
            image_id: 1 + (std::process::id() % 1000) * 7,
        }
    }

    pub fn image_id(&self) -> u32 {
        self.image_id
    }

    /// Encode the full lifetime of a source into events. Drives `source` to
    /// completion; use [`TgpEncoder::into_stream`] for the Stream API.
    pub async fn run<S: FrameSource>(
        &mut self,
        source: &mut S,
        mut on_event: impl FnMut(EncoderEvent) -> Result<(), EncoderError>,
    ) -> Result<(), EncoderError> {
        // The base image is established from the first real frame, not from
        // `source.metadata()` (which for a live capture is best-effort and may
        // differ from what the compositor actually announces).
        let mut placed = false;
        let mut frame_no = self.config.first_delta_base_frame.max(1);
        let mut last_meta: Option<FrameMetadata> = None;

        // Initial control block for this image. Placement rect (`c`,`r`) is
        // carried on the transmit+place so the image fills the pane on screen.
        let base_control = |m: &FrameMetadata, placed_| {
            let mut c = vec![
                ('i', self.image_id.to_string()),
                ('s', m.width.to_string()),
                ('v', m.height.to_string()),
            ];
            if self.config.placement_columns > 0 && self.config.placement_rows > 0 {
                c.push(('c', self.config.placement_columns.to_string()));
                c.push(('r', self.config.placement_rows.to_string()));
            }
            if placed_ {
                c.push(('p', 1u32.to_string()));
            }
            c
        };

        while let Some(update) = source.next().await.map_err(EncoderError::Source)? {
            match update {
                FrameUpdate::Resized(m) => {
                    last_meta = Some(m.clone());
                    // Re-transmit under the same id replaces data and wipes
                    // placements, so the next frame re-places.
                    placed = false;
                    if self.config.strategy == Strategy::DeltaFrames {
                        // The base is replaced by the next full frame.
                        frame_no = 1;
                    }
                    on_event(EncoderEvent::Bytes(quiet_probe_reposition()))?;
                }
                FrameUpdate::Idle => continue,
                FrameUpdate::Ended => {
                    on_event(EncoderEvent::Bytes(delete_image(self.image_id)))?;
                    on_event(EncoderEvent::Ended)?;
                    return Ok(());
                }
                FrameUpdate::Frame(frame) => {
                    if let Some(last) = &last_meta {
                        if frame.metadata.width != last.width
                            || frame.metadata.height != last.height
                            || frame.metadata.format != last.format
                        {
                            return Err(EncoderError::MetadataMismatch);
                        }
                    } else {
                        // First frame establishes the base image geometry.
                        last_meta = Some(frame.metadata.clone());
                    }
                    let data_len = frame.data.len();
                    let chunk_estimate = base64_len(data_len).div_ceil(4096);
                    tracing::debug!(
                        frame_no,
                        ?frame.metadata.format,
                        width = frame.metadata.width,
                        height = frame.metadata.height,
                        data_len,
                        chunk_estimate,
                        damage = frame.damage.len(),
                        "encoder frame start"
                    );
                    match self.config.strategy {
                        Strategy::FullRetransmit => {
                            for bytes in
                                full_frame_commands(&frame, &base_control(&frame.metadata, placed))
                            {
                                on_event(EncoderEvent::Bytes(bytes))?;
                            }
                            placed = true;
                        }
                        Strategy::DeltaFrames => {
                            if !placed {
                                for bytes in full_frame_commands(
                                    &frame,
                                    &base_control(&frame.metadata, placed),
                                ) {
                                    tracing::trace!("encoder emit chunk len={}", bytes.len());
                                    on_event(EncoderEvent::Bytes(bytes))?;
                                }
                                placed = true;
                            } else {
                                for rect in frame.effective_damage() {
                                    let bytes =
                                        delta_frame_command(self.image_id, frame_no, &rect, &frame);
                                    on_event(EncoderEvent::Bytes(bytes))?;
                                }
                                // Make the just-uploaded frame the displayed one.
                                on_event(EncoderEvent::Bytes(set_current_frame_command(
                                    self.image_id,
                                    frame_no,
                                )))?;
                            }
                            frame_no += 1;
                        }
                    }
                }
            }
        }
        Err(EncoderError::Source(SourceError::Transport(
            "source stream ended without Ended".into(),
        )))
    }

    /// Run against a source and return a [`Stream`] of byte chunks.
    pub fn into_stream<S>(
        mut self,
        mut source: S,
    ) -> impl Stream<Item = Result<EncoderEvent, EncoderError>>
    where
        S: FrameSource + 'static,
    {
        let (tx, rx) = mpsc::unbounded_channel::<Result<EncoderEvent, EncoderError>>();
        tokio::spawn(async move {
            let result = self
                .run(&mut source, |event| {
                    tx.send(Ok(event)).map_err(|_| {
                        EncoderError::Source(SourceError::Transport(
                            "encoder consumer dropped".into(),
                        ))
                    })
                })
                .await;
            if let Err(e) = result {
                let _ = tx.send(Err(e));
            }
        });
        EncoderStream {
            inner: tokio_stream::wrappers::UnboundedReceiverStream::new(rx),
        }
    }
}

/// Stream wrapper over the encoder's event channel.
pub struct EncoderStream {
    inner: tokio_stream::wrappers::UnboundedReceiverStream<Result<EncoderEvent, EncoderError>>,
}

impl Stream for EncoderStream {
    type Item = Result<EncoderEvent, EncoderError>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        std::pin::pin!(&mut self.inner).poll_next(cx)
    }
}

/// Transmit + place commands for a full frame.
fn full_frame_commands(frame: &Frame, control: &[(char, String)]) -> Vec<Vec<u8>> {
    let fmt = match frame.metadata.format {
        PixelFormat::Rgb24 => 24u32,
        PixelFormat::Rgba32 => 32,
    };
    let mut ctrl: Vec<(char, String)> = vec![('a', "T".into()), ('C', "1".into())];
    ctrl.extend(control.iter().cloned());
    // a=T places at the cursor; but we do not manage the cursor here, the
    // host TUI does. Placement keys live in `control` via base_control().
    let _ = fmt; // used via chunked_transmit below
    let mut out = Vec::new();
    for c in tgp::chunked_transmit(fmt, ctrl, &frame.data) {
        out.push(c);
    }
    out
}

/// One `a=f` patch for a damage rect, composed onto the base image (frame 1).
fn delta_frame_command(image_id: u32, frame_no: u32, rect: &Rect, frame: &Frame) -> Vec<u8> {
    let _fmt = match frame.metadata.format {
        PixelFormat::Rgb24 => 24u32,
        PixelFormat::Rgba32 => 32,
    };
    let payload = extract_rect(frame, rect);
    tgp::command(
        &[
            ('a', "f".into()),
            ('i', image_id.to_string()),
            ('c', "1".into()),
            ('r', frame_no.to_string()),
            ('x', rect.x.to_string()),
            ('y', rect.y.to_string()),
            ('s', rect.width.to_string()),
            ('v', rect.height.to_string()),
            ('X', "1".into()),
            ('z', "-1".into()),
        ],
        &payload,
    )
}

/// Ask the terminal to make `frame_no` the current frame of the animation, so a
/// placement showing this image renders that frame. Without this an `a=f`
/// delta is stored but never displayed.
fn set_current_frame_command(image_id: u32, frame_no: u32) -> Vec<u8> {
    tgp::command(
        &[
            ('a', "a".into()),
            ('i', image_id.to_string()),
            ('c', frame_no.to_string()),
        ],
        &[],
    )
}

/// Copy a pixel-space rect out of a frame's row-major buffer.
fn extract_rect(frame: &Frame, rect: &Rect) -> Vec<u8> {
    let bpp = frame.bytes_per_pixel();
    let stride = frame.metadata.width as usize * bpp;
    let x0 = rect.x as usize;
    let y0 = rect.y as usize;
    let w = rect.width as usize;
    let h = rect.height as usize;
    let mut out = Vec::with_capacity(w * h * bpp);
    for row in y0..(y0 + h) {
        let start = row * stride + x0 * bpp;
        out.extend_from_slice(&frame.data[start..start + w * bpp]);
    }
    out
}

fn delete_image(image_id: u32) -> Vec<u8> {
    tgp::command(
        &[
            ('a', "d".into()),
            ('d', "i".into()),
            ('i', image_id.to_string()),
            ('A', "1".into()),
        ],
        &[],
    )
}

fn quiet_probe_reposition() -> Vec<u8> {
    // Placeholder for re-placement after resize: the TUI layer owns cursor
    // management, so this is a no-op marker for now.
    Vec::new()
}

/// Length in bytes of the base64 encoding of `n` bytes (padded).
fn base64_len(n: usize) -> usize {
    n.div_ceil(3) * 4
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use tuigui_streamer::source::{FrameSource, SourceError};

    struct TwoFrameSource {
        meta: FrameMetadata,
        n: u32,
    }

    #[async_trait]
    impl FrameSource for TwoFrameSource {
        fn metadata(&self) -> FrameMetadata {
            self.meta.clone()
        }
        async fn next(&mut self) -> Result<Option<FrameUpdate>, SourceError> {
            if self.n == 0 {
                return Ok(Some(FrameUpdate::Ended));
            }
            self.n -= 1;
            let len = (self.meta.width * self.meta.height) as usize * 4;
            let mut data = vec![0u8; len];
            if self.n == 0 {
                // second frame: mark a 1px region changed
                data[0] = 0xff;
            }
            let mut f = Frame::full(self.meta.clone(), data.into());
            if self.n == 0 {
                f.damage = vec![Rect {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                }];
            }
            Ok(Some(FrameUpdate::Frame(f)))
        }
    }

    fn meta() -> FrameMetadata {
        FrameMetadata {
            width: 2,
            height: 2,
            format: PixelFormat::Rgba32,
        }
    }

    #[tokio::test]
    async fn full_retransmit_emits_transmit_place_then_delete() {
        let mut enc = TgpEncoder::new(EncoderConfig {
            strategy: Strategy::FullRetransmit,
            ..Default::default()
        });
        let mut src = TwoFrameSource { meta: meta(), n: 2 };
        let mut events = Vec::new();
        enc.run(&mut src, |e| {
            events.push(e);
            Ok(())
        })
        .await
        .unwrap();
        let strings: Vec<String> = events
            .into_iter()
            .filter_map(|e| match e {
                EncoderEvent::Bytes(b) => Some(String::from_utf8(b).unwrap()),
                _ => None,
            })
            .collect();
        // 2 frames × (transmit+place), then delete
        assert_eq!(strings.len(), 3);
        assert!(strings[0].starts_with("\x1b_Ga=T"));
        assert!(strings[0].contains("a=T"));
        assert!(strings[2].starts_with("\x1b_Ga=d"));
    }

    #[tokio::test]
    async fn delta_frames_place_once_then_patch() {
        let mut enc = TgpEncoder::new(EncoderConfig::default());
        let mut src = TwoFrameSource { meta: meta(), n: 2 };
        let mut events = Vec::new();
        enc.run(&mut src, |e| {
            events.push(e);
            Ok(())
        })
        .await
        .unwrap();
        let strings: Vec<String> = events
            .into_iter()
            .filter_map(|e| match e {
                EncoderEvent::Bytes(b) => Some(String::from_utf8(b).unwrap()),
                _ => None,
            })
            .collect();
        // first frame: 1 command; second frame: 1 delta; then delete
        assert_eq!(strings.len(), 3);
        assert!(strings[0].starts_with("\x1b_Ga=T"));
        assert!(strings[1].starts_with("\x1b_Ga=f"));
        assert!(strings[1].contains("a=f"));
        assert!(strings[1].contains("c=1"));
        assert!(strings[2].starts_with("\x1b_Ga=d"));
    }

    #[tokio::test]
    async fn delta_stream_terminates_with_last_chunk() {
        use tokio_stream::StreamExt as _;
        // A large frame forces chunking: 1280x720 RGBA -> ~3.68 MB -> ~900
        // chunks. Every chunk belongs to the single transmit; the final chunk
        // must carry `m=0`. The e2e regression this guards: a real capture
        // stream was being cut short before the terminating chunk, leaving the
        // terminal holding a partial upload.
        let meta = FrameMetadata {
            width: 1280,
            height: 720,
            format: PixelFormat::Rgba32,
        };
        let src = TwoFrameSource {
            meta: meta.clone(),
            n: 2,
        };
        let mut stream = TgpEncoder::new(EncoderConfig {
            strategy: Strategy::DeltaFrames,
            ..Default::default()
        })
        .into_stream(src);
        // Frame 1 = the full-base transmit (chunked). Frame 2 patches one rect.
        // The base transmit must begin and end correctly and NOT straddle the
        // first delta.
        let mut receive_base = true;
        let mut base_bytes = 0usize;
        let mut saw_m0 = false;
        let mut deltas = 0;
        let mut ended = false;
        while let Some(item) = stream.next().await {
            match item.unwrap() {
                EncoderEvent::Bytes(b) => {
                    let s = String::from_utf8_lossy(&b);
                    if s.contains("a=T") {
                        receive_base = true;
                    } else if s.contains("a=f") {
                        receive_base = false;
                        deltas += 1;
                    }
                    if receive_base {
                        base_bytes += b.len();
                        let is_m0 = s.trim_end().ends_with("m=0\x1b\\") || s.contains("m=0");
                        if is_m0 {
                            saw_m0 = true;
                        }
                    }
                }
                EncoderEvent::Ended => ended = true,
            }
        }
        assert!(ended, "stream must end with Ended");
        assert!(deltas >= 1, "frame 2 must produce a delta");
        assert!(
            saw_m0,
            "base transmit must terminate with an explicit m=0 chunk"
        );
        assert!(
            base_bytes > 4_000_000,
            "full 1280x720 base should be multi-MB, got {base_bytes}"
        );
    }

    #[test]
    fn extract_rect_rows() {
        let f = Frame {
            metadata: FrameMetadata {
                width: 3,
                height: 2,
                format: PixelFormat::Rgb24,
            },
            data: (0u8..18).collect::<Vec<u8>>().into(),
            presentation_timestamp: None,
            damage: vec![],
        };
        let r = extract_rect(
            &f,
            &Rect {
                x: 1,
                y: 1,
                width: 2,
                height: 1,
            },
        );
        assert_eq!(r, vec![12, 13, 14, 15, 16, 17]);
    }
}
