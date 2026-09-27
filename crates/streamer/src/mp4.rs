//! A [`FrameSource`] that decodes an `.mp4` with the system `ffmpeg` binary
//! and yields the decoded `Rgb24` frames. This path deliberately bypasses
//! Wayland/cage entirely, so it isolates the TGP streaming + display pipeline
//! from capture issues.
//!
//! Decoding is driven by shelling out to `ffmpeg` writing raw `rgb24` to a
//! pipe, plus `ffprobe` for geometry. `ffmpeg`/`ffprobe` must be on `PATH` (or
//! in the `devShell`).

use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;

use crate::frame::{Frame, FrameMetadata, PixelFormat};
use crate::source::{Cadence, FrameSource, FrameUpdate, SourceError};

pub fn probe_bin() -> String {
        std::env::var("FFPROBE").unwrap_or_else(|_| "ffprobe".into())
    }

    fn ffmpeg_bin() -> String {
        std::env::var("FFMPEG").unwrap_or_else(|_| "ffmpeg".into())
    }

/// Decode geometry from `ffprobe` csv output. Accepts `"W,H"`.
fn parse_geometry(out: &str) -> Option<(u32, u32)> {
    let line = out.lines().next()?.trim();
    let mut it = line.split(',');
    let w: u32 = it.next()?.trim().parse().ok()?;
    let h: u32 = it.next()?.trim().parse().ok()?;
    if w > 0 && h > 0 && w <= 8192 && h <= 8192 {
        Some((w, h))
    } else {
        None
    }
}

/// A source over a single local video file.
pub struct Mp4VideoSource {
    path: PathBuf,
    width: u32,
    height: u32,
    child: Option<std::process::Child>,
    /// Restart the decode from the top when the file ends.
    loop_forever: bool,
    ffmpeg: String,
}

/// Build the ffmpeg args that decode `path` to raw rgb24 on stdout.
fn decode_args(path: &std::path::Path) -> Vec<std::ffi::OsString> {
    use std::ffi::OsString;
    [
        OsString::from("-v"),
        OsString::from("error"),
        OsString::from("-i"),
        path.into(),
        OsString::from("-f"),
        OsString::from("rawvideo"),
        OsString::from("-pix_fmt"),
        OsString::from("rgb24"),
        OsString::from("pipe:1"),
    ]
    .to_vec()
}

impl Mp4VideoSource {
    /// Probe the file and spawn the decode pipe.
    ///
    /// `explicit_ffmpeg` / `explicit_ffprobe` override the binaries found on
    /// the environment (some dev shells don't put them on `PATH`).
    /// `loop_forever` restarts playback at the end.
    pub fn open(
        path: PathBuf,
        explicit_ffmpeg: Option<String>,
        explicit_ffprobe: Option<String>,
        loop_forever: bool,
    ) -> Result<Self, SourceError> {
        let ffmpeg = explicit_ffmpeg.unwrap_or_else(ffmpeg_bin);
        let ffprobe = explicit_ffprobe.unwrap_or_else(probe_bin);

        // Probe geometry first so we can size our frame buffers.
        let probe_out = std::process::Command::new(&ffprobe)
            .arg("-v")
            .arg("error")
            .arg("-select_streams")
            .arg("v:0")
            .arg("-show_entries")
            .arg("stream=width,height")
            .arg("-of")
            .arg("csv=p=0")
            .arg(&path)
            .output()
            .map_err(|e| SourceError::Transport(format!("ffprobe spawn: {e}")))?;
        if !probe_out.status.success() {
            return Err(SourceError::Transport(format!(
                "ffprobe failed: {}",
                String::from_utf8_lossy(&probe_out.stderr)
            )));
        }
        let (width, height) = parse_geometry(&String::from_utf8_lossy(&probe_out.stdout))
            .ok_or_else(|| SourceError::InvalidFrame("could not parse ffprobe geometry".into()))?;

        // Spawn ffmpeg decoding the file to raw rgb24 on our stdout pipe.
        let umt_path = path.clone();
        let child = std::process::Command::new(&ffmpeg)
            .args(decode_args(&umt_path))
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| SourceError::Transport(format!("ffmpeg spawn: {e}")))?;

        Ok(Mp4VideoSource {
            path,
            width,
            height,
            child: Some(child),
            loop_forever,
            ffmpeg,
        })
    }

    /// (Re)start the decode subprocess, discarding any previous child.
    fn spawn_decoder(&self, path: &std::path::Path) -> Result<std::process::Child, SourceError> {
        std::process::Command::new(&self.ffmpeg)
            .args(decode_args(path))
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| SourceError::Transport(format!("ffmpeg spawn: {e}")))
    }
}

#[async_trait]
impl FrameSource for Mp4VideoSource {
    fn metadata(&self) -> FrameMetadata {
        FrameMetadata {
            width: self.width,
            height: self.height,
            format: PixelFormat::Rgb24,
        }
    }

    fn cadence(&self) -> Cadence {
        Cadence::Finite { duration: None }
    }

    async fn next(&mut self) -> Result<Option<FrameUpdate>, SourceError> {
        let frame_size = (self.width as usize) * (self.height as usize) * 3;
        loop {
            // Take the child out so nothing borrows `self` while we may
            // reassign `self.child` (for looping) or return it.
            let mut child = match self.child.take() {
                Some(c) => c,
                None => return Ok(Some(FrameUpdate::Ended)),
            };
            let stdout = match child.stdout.as_mut() {
                Some(s) => s,
                None => return Ok(Some(FrameUpdate::Ended)),
            };
            let mut buf = vec![0u8; frame_size];

            let mut filled = 0usize;
            let eof = loop {
                match stdout.read(&mut buf[filled..]) {
                    Ok(0) => break true,
                    Ok(n) => {
                        filled += n;
                        if filled == frame_size {
                            break false;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        return Err(SourceError::Transport(format!("ffmpeg read: {e}")));
                    }
                }
            };

            if eof {
                if self.loop_forever {
                    match self.spawn_decoder(&self.path) {
                        Ok(new_child) => {
                            self.child = Some(new_child);
                            continue;
                        }
                        Err(_) => return Ok(Some(FrameUpdate::Ended)),
                    }
                }
                return Ok(Some(FrameUpdate::Ended));
            }

            if filled != frame_size {
                // Partial frame: hand it out, then (if looping) keep going.
                let f = Frame {
                    metadata: self.metadata(),
                    data: Bytes::from(buf[..filled].to_vec()),
                    presentation_timestamp: None,
                    timing: None,
                    damage: Vec::new(),
                };
                self.child = Some(child);
                return Ok(Some(FrameUpdate::Frame(f)));
            }
            self.child = Some(child);
            return Ok(Some(FrameUpdate::Frame(Frame {
                metadata: self.metadata(),
                data: Bytes::from(buf),
                presentation_timestamp: Some(std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or(Duration::ZERO)),
                timing: None,
                damage: Vec::new(),
            })));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ffprobe_csv() {
        assert_eq!(parse_geometry("320,240\n"), Some((320, 240)));
        assert_eq!(parse_geometry("1280,720"), Some((1280, 720)));
        assert_eq!(parse_geometry("not a number\n"), None);
        assert_eq!(parse_geometry(""), None);
    }
}