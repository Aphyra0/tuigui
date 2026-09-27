//! The stateful TGP session encoder.
//!
//! Consumes [`FrameSource`] updates and produces the byte stream a terminal
//! should receive: a full transmit+place for every frame. The output is a
//! [`futures_core::Stream`] of byte chunks — nothing is written to stdout;
//! the caller owns the sink (PTY, SSH channel, file, ...).

use futures_core::Stream;
use std::time::Instant;
use tokio::sync::mpsc;

use crate::proto as tgp;
use tuigui_streamer::frame::{Frame, FrameMetadata, PixelFormat};
use tuigui_streamer::source::{FrameSource, FrameUpdate, SourceError};

/// What the encoder tells its consumer.
#[derive(Debug)]
pub enum EncoderEvent {
    /// A frame is queued for emission; carry its telemetry so the UI can show
    /// resolution and capture cost without parsing the bytes.
    Frame {
        width: u32,
        height: u32,
        /// Wall-clock time spent pulling this frame out of its source (the
        /// capture cost), measured around `source.next()`.
        capture_ms: f64,
        /// Wall-clock time spent turning the frame's pixels into TGP bytes
        /// (base64 + chunking), measured around `full_frame_commands`.
        encode_ms: f64,
        /// Per-phase capture breakdown, when the source reported it.
        timing: Option<tuigui_streamer::frame::CaptureTiming>,
    },
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
#[derive(Debug, Clone, Default)]
pub struct EncoderConfig {
    /// On-screen placement size in terminal cells. When both are `>0` every
    /// placed image is scaled to fill exactly this `columns x rows` area;
    /// without these the image is shown at one screen cell per source pixel,
    /// so a full-resolution frame overflows the terminal and only its top-left
    /// corner is visible. Set both from the TUI's pane size.
    pub placement_columns: u32,
    pub placement_rows: u32,
    /// Transmit frames as PNG (`f=100`) instead of raw RGBA/RGB. PNG is decoded
    /// by the terminal (wuffs), and can shrink an uncompressed frame ~5-20x —
    /// the dominant win when the terminal sink is the bottleneck.
    pub png: bool,
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
        let mut last_meta: Option<FrameMetadata> = None;
        // id of the image currently on screen, to delete after the next frame
        // replaces it.
        let mut prev_image_id: Option<u32> = None;

        // Control block for this frame's image. Placement rect (`c`,`r`) is
        // carried on the transmit+place so the image fills the pane on screen.
        // The image id is captured from the frame's current id by closure.
        let base_control = |m: &FrameMetadata, image_id: u32| {
            let mut c = vec![
                ('i', image_id.to_string()),
                ('s', m.width.to_string()),
                ('v', m.height.to_string()),
            ];
            if self.config.placement_columns > 0 && self.config.placement_rows > 0 {
                c.push(('c', self.config.placement_columns.to_string()));
                c.push(('r', self.config.placement_rows.to_string()));
            }
            c.push(('p', 1u32.to_string()));
            c
        };

        loop {
            let t0 = Instant::now();
            let update = source.next().await.map_err(EncoderError::Source)?;
            let Some(update) = update else {
                // Preserve the old `while let` behavior: Ok(None) ends the
                // stream without an Ended update, surfacing as a transport
                // error below.
                break;
            };
            let poll_ms = t0.elapsed().as_secs_f64() * 1000.0;
            match update {
                FrameUpdate::Resized(_m) => {
                    on_event(EncoderEvent::Bytes(quiet_probe_reposition()))?;
                }
                FrameUpdate::Idle => continue,
                FrameUpdate::Ended => {
                    // Delete the image currently on screen (the last placed one).
                    if let Some(prev) = prev_image_id {
                        on_event(EncoderEvent::Bytes(delete_image(prev)))?;
                    }
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
                        ?frame.metadata.format,
                        width = frame.metadata.width,
                        height = frame.metadata.height,
                        data_len,
                        chunk_estimate,
                        damage = frame.damage.len(),
                        "encoder frame start"
                    );
                    // Use a fresh image id every frame. Re-transmitting the
                    // same id deletes the on-screen placement (spec), which
                    // blanks the pane for the whole transmit -> flicker. A
                    // new id transmits and places over the old image with no
                    // delete gap; the prior image is deleted once it's covered.
                    let cur_id = self.image_id;
                    self.image_id = self.image_id.wrapping_add(1);
                    // Announce the frame before its payload so the consumer can
                    // timestamp it for capture/latency telemetry. The bytes are
                    // produced inline, so time the encoding here.
                    let t_encode = Instant::now();
                    for bytes in full_frame_commands(
                        &frame,
                        &base_control(&frame.metadata, cur_id),
                        self.config.png,
                    ) {
                        on_event(EncoderEvent::Bytes(bytes))?;
                    }
                    let encode_ms = t_encode.elapsed().as_secs_f64() * 1000.0;
                    on_event(EncoderEvent::Frame {
                        width: frame.metadata.width,
                        height: frame.metadata.height,
                        capture_ms: poll_ms,
                        encode_ms,
                        timing: frame.timing,
                    })?;
                    if let Some(prev) = prev_image_id.take() {
                        on_event(EncoderEvent::Bytes(delete_image(prev)))?;
                    }
                    prev_image_id = Some(cur_id);
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
        // Unbounded buffer is fine: the consumer's drain loop caps bytes per
        // iteration, so it always yields to input polling regardless of how
        // fast this producer runs.
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
fn full_frame_commands(frame: &Frame, control: &[(char, String)], png: bool) -> Vec<Vec<u8>> {
    // TGP format id: 100 for PNG (terminal decodes), else raw pixel formats.
    let fmt = if png {
        100u32
    } else {
        match frame.metadata.format {
            PixelFormat::Rgb24 => 24,
            PixelFormat::Rgba32 => 32,
        }
    };
    let mut ctrl: Vec<(char, String)> = vec![('a', "T".into()), ('C', "1".into())];
    ctrl.extend(control.iter().cloned());
    // a=T places at the cursor; but we do not manage the cursor here, the
    // host TUI does. Placement keys live in `control` via base_control().
    let payload = if png {
        encode_png(&frame.data, frame.metadata.width, frame.metadata.height)
    } else {
        frame.data.clone()
    };
    let mut out = Vec::new();
    for c in tgp::chunked_transmit(fmt, ctrl, &payload) {
        out.push(c);
    }
    out
}

/// Compress raw RGBA/RGB pixel data into a PNG byte string (in-memory).
fn encode_png(data: &[u8], width: u32, height: u32) -> bytes::Bytes {
    let mut buf = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut buf, width, height);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        // Keep memory/CPU bounded for a live stream; terminal decodes PNG via
        // wuffs, and a fast stream matters more than near-lossless size.
        enc.set_compression(png::Compression::Fast);
        let mut w = enc.write_header().expect("png header");
        w.write_image_data(data).expect("png data");
    }
    bytes::Bytes::from(buf)
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
            let data = vec![0u8; len];
            let f = Frame::full(self.meta.clone(), data.into());
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
        // 2 frames × (transmit+place), delete-of-previous after frame 2, then
        // delete of the last image on Ended.
        assert_eq!(strings.len(), 4);
        assert!(strings[0].starts_with("\x1b_Ga=T"));
        assert!(strings[0].contains("a=T"));
        assert!(strings[3].starts_with("\x1b_Ga=d"));
    }

    /// Extract the `i=<id>` value from a TGP command's control block.
    fn id_of(s: &str) -> &str {
        for k in s.split(',') {
            if let Some(v) = k.strip_prefix("i=") {
                return v;
            }
        }
        ""
    }

    #[tokio::test]
    async fn every_frame_retransmits_and_places() {
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
        // 2 frames: transmit+place each, then delete-of-previous after frame 2,
        // then delete of the last image on Ended -> 4 events.
        assert_eq!(strings.len(), 4);
        assert!(strings[0].starts_with("\x1b_Ga=T"));
        assert!(strings[0].contains("a=T"));
        assert!(strings[1].starts_with("\x1b_Ga=T"), "frame 2 must retransmit");
        assert!(strings[1].contains("a=T"));
        // Each frame must use a distinct image id (no delete-gap flicker).
        let id0 = id_of(&strings[0]);
        let id1 = id_of(&strings[1]);
        assert_ne!(id0, id1, "each frame must transmit under a fresh image id");
        // frame 2's delete targets the previous first-frame id.
        assert!(strings[2].starts_with("\x1b_Ga=d"));
        assert!(strings[2].contains(&format!("i={id0}")));
        // final delete removes the last placed image on Ended.
        assert!(strings[3].starts_with("\x1b_Ga=d"));
        assert!(strings[3].contains(&format!("i={id1}")));
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
        let mut stream = TgpEncoder::new(EncoderConfig::default()).into_stream(src);
        let mut base_bytes = 0usize;
        let mut saw_m0 = false;
        let mut full_transmits = 0;
        let mut ended = false;
        while let Some(item) = stream.next().await {
            match item.unwrap() {
                EncoderEvent::Bytes(b) => {
                    let s = String::from_utf8_lossy(&b);
                    if s.contains("a=T") {
                        full_transmits += 1;
                    }
                    base_bytes += b.len();
                    let is_m0 = s.trim_end().ends_with("m=0\x1b\\") || s.contains("m=0");
                    if is_m0 {
                        saw_m0 = true;
                    }
                }
                EncoderEvent::Frame { .. } => {}
                EncoderEvent::Ended => ended = true,
            }
        }
        assert!(ended, "stream must end with Ended");
        assert!(full_transmits >= 2, "each frame must be a full transmit");
        assert!(
            saw_m0,
            "base transmit must terminate with an explicit m=0 chunk"
        );
        assert!(
            base_bytes > 4_000_000,
            "full 1280x720 base should be multi-MB, got {base_bytes}"
        );
    }
}
