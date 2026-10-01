//! Bridge from the in-process compositor (`tuigui-comp`) to the streaming and
//! input pipelines, mirroring the roles `CageFrameSource` / `InputSock` filled
//! for the external `cage` binary.

use std::time::{Duration, Instant};

use anyhow::Result;
use async_trait::async_trait;
use bytes::Bytes;
use tuigui_cage::PointerEvent;
use tuigui_comp::{Client, Rect, ScrollDelta};
use tuigui_streamer::{
    Cadence, Frame, FrameMetadata, FrameSource, FrameUpdate, PixelFormat, SourceError,
};
use tokio::sync::mpsc;

use crate::InputMsg;

/// A [`FrameSource`] that pulls frames from a `tuigui-comp` [`Client`] by
/// blocking `capture()` round-trips, paced to `max_fps`. The client is created
/// at the scaled resolution upstream, so captured frames need no downscale.
pub struct CompFrameSource {
    client: Client,
    max_fps: u32,
    last_dims: Option<(u32, u32)>,
    prev_emit: Option<Instant>,
    seen_content: bool,
    boot_start: Instant,
}

impl CompFrameSource {
    pub fn new(client: Client, max_fps: u32) -> Self {
        CompFrameSource {
            client,
            max_fps,
            last_dims: None,
            prev_emit: None,
            seen_content: false,
            boot_start: Instant::now(),
        }
    }
}

/// Convert `tuigui-comp::Rect` (pixel damage) into `tuigui-streamer::Rect`.
fn to_streamer_damage(damage: Vec<Rect>) -> Vec<tuigui_streamer::Rect> {
    damage
        .into_iter()
        .map(|r| tuigui_streamer::Rect {
            x: r.x,
            y: r.y,
            width: r.width,
            height: r.height,
        })
        .collect()
}

/// True when a frame is entirely (or almost entirely) black — the signature of
/// a pre-paint capture frame.
fn frame_blank(rgba: &[u8]) -> bool {
    const SAMPLE: usize = 64;
    let px = rgba.len() / 4;
    if px == 0 {
        return true;
    }
    let step = (px / SAMPLE).max(1);
    for (i, px0) in rgba.as_chunks::<4>().0.iter().enumerate().step_by(step) {
        if i >= SAMPLE {
            break;
        }
        if px0[0] > 8 || px0[1] > 8 || px0[2] > 8 {
            return false;
        }
    }
    true
}

#[async_trait]
impl FrameSource for CompFrameSource {
    fn metadata(&self) -> FrameMetadata {
        let (width, height) = self.last_dims.unwrap_or((0, 0));
        FrameMetadata {
            width,
            height,
            format: PixelFormat::Rgba32,
        }
    }

    fn cadence(&self) -> Cadence {
        Cadence::Live
    }

    async fn next(&mut self) -> Result<Option<FrameUpdate>, SourceError> {
        // Pace the producer to the configured frame interval so it can't
        // outrun the sink (mirrors the cage path's rate limiter).
        if let Some(prev) = self.prev_emit {
            let elapsed = prev.elapsed();
            let interval = Duration::from_secs_f32(1.0 / self.max_fps.max(1) as f32);
            if elapsed < interval {
                tokio::time::sleep(interval - elapsed).await;
            }
        }

        // Real-time frame budget: a frame that takes longer than this to be
        // captured/composited is dropped instead of blocking the stream. The
        // caller (TGP encoder) keeps running, so a slow first frame at startup
        // can't starve every tokio worker and freeze the UI.
        const FRAME_BUDGET: Duration = Duration::from_millis(500);
        // Run the blocking capture round-trip on the blocking pool so it never
        // ties up a token-driven worker thread, then apply the budget as a
        // hard timeout. A saturated wl thread can legitimately exceed the
        // budget; dropping beats wedging the runtime.
        let client = self.client.clone();
        // Time the whole capture round-trip: the blocking pool handoff plus the
        // wl-thread read. This is where a headless capture's wall-clock lives.
        let t0 = std::time::Instant::now();
        let capture = tokio::time::timeout(FRAME_BUDGET, tokio::task::spawn_blocking(move || {
            client.capture()
        }))
        .await;
        let capture_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let frame = match capture {
            Ok(Ok(Ok(f))) => f,
            Ok(Ok(Err(e))) => {
                tracing::warn!(err = %e, "comp capture failed; treating as idle");
                self.prev_emit = Some(Instant::now());
                return Ok(Some(FrameUpdate::Idle));
            }
            // Budget exceeded (or the blocking task was cancelled): drop the
            // frame rather than let it stall the stream.
            Ok(Err(_)) | Err(_) => {
                tracing::warn!("comp capture exceeded {}ms frame budget; dropping", FRAME_BUDGET.as_millis());
                self.prev_emit = Some(Instant::now());
                return Ok(Some(FrameUpdate::Idle));
            }
        };
        self.prev_emit = Some(Instant::now());

        let timing = tuigui_streamer::frame::CaptureTiming {
            announce_ms: 0.0,
            setup_ms: 0.0,
            copy_ms: capture_ms,
            readout_ms: 0.0,
            downscale_ms: 0.0,
        };

        // The client is created at the scaled resolution in main.rs, so the
        // captured frame is already the size we stream — no resample needed.
        let (nw, nh) = (frame.width, frame.height);
        let data = Bytes::from(frame.rgba);
        self.last_dims = Some((nw, nh));

        let streamer_frame = Frame {
            metadata: FrameMetadata {
                width: nw,
                height: nh,
                format: PixelFormat::Rgba32,
            },
            data,
            presentation_timestamp: None,
            timing: Some(timing),
            damage: to_streamer_damage(frame.damage),
        };

        // Skip blank boot frames until real content appears (or a budget
        // elapses, so genuinely unlit apps still show), mirroring the cage path.
        let blank = frame_blank(&streamer_frame.data);
        if !self.seen_content {
            const BLANK_BUDGET: Duration = Duration::from_secs(5);
            if blank && self.boot_start.elapsed() < BLANK_BUDGET {
                return Ok(Some(FrameUpdate::Idle));
            }
        }
        self.seen_content = true;
        Ok(Some(FrameUpdate::Frame(streamer_frame)))
    }
}

/// Convert a cage `KeyEvent` (evdev code + state) into a `tuigui-comp` `Key`.
/// Returns `None` for keys the comp client can't represent yet.
pub fn comp_key_from_evdev(code: u32, pressed: bool) -> Option<(tuigui_comp::Key, tuigui_comp::KeyState)> {
    use tuigui_comp::Key;
    let key = match code {
        28 => Key::Return,
        15 => Key::Tab,
        14 => Key::Backspace,
        111 => Key::Delete,
        1 => Key::Escape,
        57 => Key::Space,
        105 => Key::Left,
        106 => Key::Right,
        103 => Key::Up,
        108 => Key::Down,
        102 => Key::Home,
        107 => Key::End,
        104 => Key::PageUp,
        109 => Key::PageDown,
        _ => return to_printable_key(code),
    };
    let state = if pressed {
        tuigui_comp::KeyState::Pressed
    } else {
        tuigui_comp::KeyState::Released
    };
    Some((key, state))
}

/// Map an evdev code in the letter/number/symbol ranges to a printable char
/// (matches `char_evdev` in `tuigui-comp`). Returns its `Key::Char`.
fn to_printable_key(code: u32) -> Option<(tuigui_comp::Key, tuigui_comp::KeyState)> {
    let c = match code {
        2 => '1', 3 => '2', 4 => '3', 5 => '4', 6 => '5', 7 => '6', 8 => '7', 9 => '8', 10 => '9',
        11 => '0',
        12 => '-', 13 => '=', 26 => '[', 27 => ']', 39 => ';', 40 => '\'', 41 => '`',
        43 => '\\', 51 => ',', 52 => '.', 53 => '/',
        30..=38 => (b'a' + (code - 30) as u8) as char,
        _ => return None,
    };
    Some((tuigui_comp::Key::Char(c), tuigui_comp::KeyState::Pressed))
}

/// Drain an `mpsc::UnboundedReceiver<InputMsg>` on a dedicated std thread and
/// forward each event into the `Client`. Returns when the channel closes.
///
/// Motion is coalesced so a mouse flick (which can burst dozens of motion
/// events per drag through the terminal) collapses into a single `move_mouse`
/// round-trip per drain; discrete events (keys, clicks, scrolls) are dispatched
/// immediately as received rather than queued behind the motion flood. Without
/// this, a fast drag starves the discrete events behind many blocking
/// `move_mouse` calls and the app feels unresponsive to clicks/scrolling.
pub fn input_writer_loop(client: &Client, mut rx: mpsc::UnboundedReceiver<InputMsg>) {
    // Latest pending pointer position for the current batch (None = none queued).
    let mut pending_pos: Option<(f64, f64)> = None;

    loop {
        let msg = match rx.blocking_recv() {
            Some(m) => m,
            None => return, // channel closed
        };

        // Handle the first message, then drain whatever else is already queued
        // (non-blocking) into the same batch so we can coalesce motion and keep
        // discrete events snappy.
        let mut batch = vec![msg];
        while let Ok(m) = rx.try_recv() {
            batch.push(m);
        }
        for msg in batch {
            match msg {
                InputMsg::Key(kev) => {
                    // Discrete event: dispatch immediately, don't batch it.
                    let pressed = kev.state == tuigui_cage::KeyState::Press;
                    if let Some((key, state)) = comp_key_from_evdev(kev.code, pressed) {
                        if let Err(e) = client.key(key, state) {
                            tracing::warn!(err = %e, "comp key failed");
                        }
                    }
                }
                InputMsg::Pointer(PointerEvent::Motion { x, y }) => {
                    // Coalesce: keep only the latest position this batch.
                    pending_pos = Some((x, y));
                }
                InputMsg::Pointer(ev) => {
                    // Discrete pointer event (button/axis): the click/scroll is
                    // delivered at the *last flushed* position, so push the
                    // coalesced motion first so `pointer_loc` is current before
                    // the click lands — otherwise clicks lag a drag behind.
                    if let Some((x, y)) = pending_pos.take() {
                        if let Err(e) = client.move_mouse(x, y) {
                            tracing::warn!(err = %e, "comp mouse move failed");
                        }
                    }
                    comp_pointer(client, ev);
                }
            }
        }

        // Flush the coalesced motion once at the end of the batch.
        if let Some((x, y)) = pending_pos.take() {
            if let Err(e) = client.move_mouse(x, y) {
                tracing::warn!(err = %e, "comp mouse move failed");
            }
        }
    }
}

/// Translate a discrete [`PointerEvent`] (button, axis) into blocking `Client`
/// calls. Motion is handled separately by the writer loop's coalescing logic.
fn comp_pointer(client: &Client, ev: PointerEvent) {
    match ev {
        PointerEvent::Motion { x, y } => {
            // Motion is coalesced by the writer loop's batch logic and should
            // never reach here; handle defensively so dropping it can't leave
            // the pointer stuck.
            if let Err(e) = client.move_mouse(x, y) {
                tracing::warn!(err = %e, "comp mouse move failed");
            }
        }
        PointerEvent::Button { code, state } => {
            // `Client::click` already sends press+release. Only act on the
            // Press half of a real click; the matching Release is redundant
            // (forwarding both would make one physical click fire twice).
            if state != tuigui_cage::KeyState::Press {
                return;
            }
            let button = match code {
                0x110 => Some(tuigui_comp::Button::Left),
                0x111 => Some(tuigui_comp::Button::Right),
                0x112 => Some(tuigui_comp::Button::Middle),
                _ => None,
            };
            if let Some(button) = button {
                if let Err(e) = client.click(button) {
                    tracing::warn!(err = %e, "comp click failed");
                }
            }
        }
        PointerEvent::Axis { dx, dy } => {
            let delta = ScrollDelta {
                vertical: dy.round() as i32,
                horizontal: dx.round() as i32,
            };
            if let Err(e) = client.scroll(delta) {
                tracing::warn!(err = %e, "comp scroll failed");
            }
        }
    }
}