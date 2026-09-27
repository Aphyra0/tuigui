//! Decoded frames: the currency exchanged between codecs and the TGP emitter.

use std::time::Duration;

use bytes::Bytes;

/// Layout of the pixel data inside a [`Frame`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// 3 bytes per pixel, row-major, top-left origin, sRGB.
    Rgb24,
    /// 4 bytes per pixel, row-major, top-left origin, non-premultiplied sRGB + alpha.
    Rgba32,
}

impl PixelFormat {
    pub fn bytes_per_pixel(self) -> usize {
        match self {
            PixelFormat::Rgb24 => 3,
            PixelFormat::Rgba32 => 4,
        }
    }
}

/// Static description of a stream of frames, sent once when a stream starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameMetadata {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
}

/// Optional per-phase capture profiling, filled in by live capturers. Purely
/// informational; the TGP emitter ignores it but the UI surfaces it.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct CaptureTiming {
    /// Wall-clock until the compositor announced dims/format (`buffer` event).
    pub announce_ms: f64,
    /// Setting up the shared buffer (create shm pool + buffer + copy request).
    pub setup_ms: f64,
    /// Waiting for the compositor to finish copying into the buffer (`ready`).
    pub copy_ms: f64,
    /// Reading the pixels out of the shared buffer into an owned RGBA Vec.
    pub readout_ms: f64,
}

impl CaptureTiming {
    pub fn total_ms(&self) -> f64 {
        self.announce_ms + self.setup_ms + self.copy_ms + self.readout_ms
    }
}

/// One decoded frame in a [`FrameSource`] stream.
///
/// The pixel payload is opaque to the TGP emitter: it is whatever the codec
/// produced after decoding. Damage-tracking callers that want to stream only
/// changed regions fill in `damage`; a plain capture or decoder leaves it empty
/// and the emitter treats the whole frame as changed.
#[derive(Debug, Clone)]
pub struct Frame {
    pub metadata: FrameMetadata,
    /// Row-major pixel data, `width * height * bytes_per_pixel` bytes.
    pub data: Bytes,
    /// Which frames this one builds on: a decoder-produced PTS, or a capture
    /// monotonic timestamp. Purely informational for the emitter.
    pub presentation_timestamp: Option<Duration>,
    /// Per-phase capture cost breakdown, when produced by a live capturer.
    pub timing: Option<CaptureTiming>,
    /// Damaged regions in *pixel* coordinates, row-major top-left origin.
    /// Empty means "the whole frame changed".
    pub damage: Vec<Rect>,
}

impl Frame {
    /// A full-frame frame (no damage tracking).
    pub fn full(metadata: FrameMetadata, data: Bytes) -> Self {
        assert_eq!(
            data.len(),
            (metadata.width as usize)
                * (metadata.height as usize)
                * metadata.format.bytes_per_pixel(),
            "frame data size does not match metadata"
        );
        Frame {
            metadata,
            data,
            presentation_timestamp: None,
            timing: None,
            damage: Vec::new(),
        }
    }

    /// Rectangles that actually changed, defaulting to the full frame.
    pub fn effective_damage(&self) -> Vec<Rect> {
        if self.damage.is_empty() {
            vec![Rect {
                x: 0,
                y: 0,
                width: self.metadata.width,
                height: self.metadata.height,
            }]
        } else {
            self.damage.clone()
        }
    }

    pub fn bytes_per_pixel(&self) -> usize {
        self.metadata.format.bytes_per_pixel()
    }
}

/// Axis-aligned pixel-space rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(w: u32, h: u32) -> FrameMetadata {
        FrameMetadata {
            width: w,
            height: h,
            format: PixelFormat::Rgba32,
        }
    }

    #[test]
    fn effective_damage_defaults_to_full_frame() {
        let f = Frame::full(meta(4, 2), Bytes::from(vec![0u8; 4 * 2 * 4]));
        assert_eq!(
            f.effective_damage(),
            vec![Rect {
                x: 0,
                y: 0,
                width: 4,
                height: 2
            }]
        );
    }

    #[test]
    fn full_rejects_mismatched_data() {
        assert!(std::panic::catch_unwind(|| {
            Frame::full(meta(4, 2), Bytes::from(vec![0u8; 10]))
        })
        .is_err());
    }
}
