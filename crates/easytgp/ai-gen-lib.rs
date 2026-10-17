//! `ai-gen-lib.rs` — an idiomatic, safe redesign of the TGP ownership model.
//!
//! This captures the ideas we settled on, as a self-contained (std-only) file
//! so it compiles and can be read independently of the in-progress `lib.rs`.
//!
//! Design:
//!
//! * **Handle = `Arc<RwLock<E>>`**, nothing more. Whether an entity is
//!   yankable is decided by *its* `E`: `Handle<Option<Placement>>` is
//!   yankable (write `None`), `Handle<Image>` is not.
//!
//! * **`Image` strongly owns its placements** (`Vec<PlacementHandle>`).
//!   Dropping the last strong `Image` handle cascades: every placement handle
//!   is written to `None` (the "yoink"), and a terminal delete is emitted so
//!   the image + its placements come off screen together. This matches what
//!   the terminal already does (images are evicted whole).
//!
//! * **Yoinking does not steal memory from the user.** The user keeps their
//!   `PlacementHandle` variables; the value inside simply becomes `None`, so
//!   every later method fails gracefully with `DeadHandle` instead of touching
//!   a corpse. This is the "we can't physically take it back, so we make it
//!   inert" resolution.
//!
//! * **`Action` + `.cd(...)`** is the single choke point every byte flows
//!   through; the only place that touches stdout.
//!
//! * Lock access recovers from poisoning instead of unwrapping, so this never
//!   panics on a poisoned lock.
//!
//! In the real crate, swap the tiny `base64` module here for `base64-turbo`,
//! and the `AtomicU32` id source for your slab/`static_init` scheme (minus the
//! screen-clear side effect, which was smelling).

use std::{
    fmt::Display,
    io::Write,
    sync::atomic::{AtomicU32, Ordering},
    sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard},
};

// ---------------------------------------------------------------------------
// Error + ids
// ---------------------------------------------------------------------------

/// The entity this handle pointed to has been deleted / evicted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeadHandle;

/// A minimal error type so methods can be fallible without leaking a lock.
/// (Expand with `Io(std::io::Error)` etc. once commit returns results.)
#[derive(Debug)]
pub enum Error {
    Dead,
    Io(std::io::Error),
}

impl PartialEq for Error {
    fn eq(&self, other: &Self) -> bool {
        matches!(
            (self, other),
            (Error::Dead, Error::Dead) | (Error::Io(_), Error::Io(_))
        )
    }
}
impl Eq for Error {}

pub type ID = u32;

/// Global monotonic id source. `fetch_add` is the safe replacement for the
/// screen-clearing `static_init` initializer: no side effects, no unsafe.
fn next_id() -> ID {
    static NEXT: AtomicU32 = AtomicU32::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Handle
// ---------------------------------------------------------------------------

/// A shared, thread-safe handle to a TGP entity.
///
/// `E` is chosen by the caller. For entities the owner may need to invalidate
/// (placements) use `Handle<Option<Placement>>`; writing `None` "yoinks" every
/// clone at once because they share the same `Arc`.
#[derive(Clone)]
pub struct Handle<E>(Arc<RwLock<E>>);

impl<E> Handle<E> {
    fn new(entity: E) -> Self {
        Self(Arc::new(RwLock::new(entity)))
    }
}

/// Poisoning only happens if a panic occurs *while a lock is held*. This library
/// never panics there, but if it ever does we still want the value back rather
/// than unwrapping. These two helpers encode that policy in one place.
fn read_guard<E>(h: &Handle<E>) -> RwLockReadGuard<'_, E> {
    h.0.read().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn write_guard<E>(h: &Handle<E>) -> RwLockWriteGuard<'_, E> {
    h.0.write().unwrap_or_else(|poisoned| poisoned.into_inner())
}

// Specialized operations for yankable (`Option`-wrapped) handles.
impl<T> Handle<Option<T>> {
    /// Whether the target entity is still alive (the `Option` is `Some`).
    pub fn alive(&self) -> bool {
        read_guard(self).is_some()
    }

    /// Yoink the entity: write `None`, invalidating every clone at once.
    /// The caller keeps their handle, but it is now inert.
    fn yoink(&self) {
        *write_guard(self) = None;
    }
}

// ---------------------------------------------------------------------------
// base64 (drop-in for base64-turbo; standard alphabet, padded)
// ---------------------------------------------------------------------------

mod base64 {
    const TABLE: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub fn encode(data: &[u8]) -> String {
        let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
        for chunk in data.chunks(3) {
            let (b0, b1, b2) = (chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0));
            out.push(TABLE[(b0 >> 2) as usize] as char);
            out.push(TABLE[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
            out.push(if chunk.len() >= 2 {
                TABLE[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char
            } else {
                '='
            });
            out.push(if chunk.len() == 3 { TABLE[(b2 & 0x3f) as usize] as char } else { '=' });
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Protocol wire format
// ---------------------------------------------------------------------------

/// A single `key=value` control datum. Kept as a plain tuple like yours.
#[derive(Clone, Debug)]
pub struct ControlData(&'static str, String);

impl ControlData {
    fn format(&self) -> String {
        format!("{}={}", self.0, self.1)
    }
}

impl From<Format> for ControlData {
    fn from(value: Format) -> Self {
        let code = match value {
            Format::Rgb => 24,   // f=24
            Format::Rgba => 32,  // f=32
            Format::Png => 100,  // f=100 (dims read from the data)
        };
        ControlData("f", code.to_string())
    }
}

/// Builds a single APC escape. Owns its control data and payload; the only
/// place in this library that talks to stdout (`commit`).
pub struct Action {
    cds: Vec<ControlData>,
    payload: Option<String>,
    /// `q` value. `1` suppresses OK replies but *keeps* failure replies, so
    /// ENOENT/ENOSPC still surface — fixing the hard-coded `q=2` smell that
    /// used to blind the library to eviction. Set to `2` with `.silent()`.
    suppress_errors: bool,
}

impl Action {
    pub fn new() -> Self {
        Self { cds: Vec::new(), payload: None, suppress_errors: false }
    }

    pub fn payload(mut self, payload: String) -> Self {
        self.payload = Some(payload);
        self
    }

    /// Add a control datum by key + Display-able value.
    pub fn cd(mut self, key: &'static str, value: impl Display) -> Self {
        self.cds.push(ControlData(key, value.to_string()));
        self
    }

    /// Add a control datum only if `Some`.
    pub fn cd_opt(mut self, key: &'static str, value: Option<impl Display>) -> Self {
        if let Some(v) = value {
            self.cds.push(ControlData(key, v.to_string()));
        }
        self
    }

    /// Add a control datum only if `condition` holds.
    pub fn cd_if(mut self, condition: bool, key: &'static str, value: impl Display) -> Self {
        if condition {
            self.cds.push(ControlData(key, value.to_string()));
        }
        self
    }

    /// Same as `cd`, but takes an already-built `ControlData` (e.g. `Format`).
    pub fn push(mut self, cd: ControlData) -> Self {
        self.cds.push(cd);
        self
    }

    /// Suppress failure replies too (`q=2`). Default (unsuppressed failures)
    /// is what a library that wants to recover from eviction should use.
    pub fn silent(mut self) -> Self {
        self.suppress_errors = true;
        self
    }

    /// Serialize and emit the escape. Reads nothing yet; ack handling on stdin
    /// can be added later without changing this surface.
    pub fn commit(self) {
        let mut cds = self.cds;
        // q: 1 = suppress OK, 2 = also suppress failures.
        cds.push(ControlData("q", (if self.suppress_errors { 2 } else { 1 }).to_string()));

        let cds = cds
            .into_iter()
            .map(|cd| cd.format())
            .collect::<Vec<_>>()
            .join(",");
        let payload = self.payload.unwrap_or_default();

        let mut stdout = std::io::stdout().lock();
        let _ = write!(stdout, "\x1b_G{cds};{payload}\x1b\\");
        let _ = stdout.flush();
    }
}

// ---------------------------------------------------------------------------
// Placement
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct PlacementConfig {
    pub offset_x: Option<u32>,
    pub offset_y: Option<u32>,
    pub cols: Option<u32>,
    pub rows: Option<u32>,
    pub x: Option<u32>,
    pub y: Option<u32>,
    pub z: Option<i32>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fix_cursor: bool,
}

#[derive(Clone)]
pub struct Placement {
    id: ID,
    image_id: ID,
    config: PlacementConfig,
}

pub type PlacementHandle = Handle<Option<Placement>>;

fn add_place_cds(a: Action, c: &PlacementConfig) -> Action {
    a.cd_opt("X", c.offset_x)
        .cd_opt("Y", c.offset_y)
        .cd_opt("c", c.cols)
        .cd_opt("r", c.rows)
        .cd_opt("x", c.x)
        .cd_opt("y", c.y)
        .cd_opt("z", c.z)
        .cd_opt("w", c.width)
        .cd_opt("h", c.height)
        .cd_if(c.fix_cursor, "C", 1)
}

impl PlacementHandle {
    /// Reposition / reconfigure this placement in place. Same
    /// (image_id, placement_id) pair replaces instead of stacking, so no ghost.
    pub fn update(&self, config: PlacementConfig) -> Result<(), Error> {
        let (image_id, id) = {
            let guard = read_guard(self);
            let p = guard.as_ref().ok_or(Error::Dead)?;
            (p.image_id, p.id)
        };

        Action::new()
            .cd("a", "p")
            .cd("i", image_id)
            .cd("p", id)
            .pipe(|a| add_place_cds(a, &config))
            .commit();

        write_guard(self).as_mut().expect("checked alive above").config = config;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Image (the strong owner)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct ImageRequest {
    format: Format,
    data: Vec<u8>,
}

pub type NewTImage = ImageRequest;

impl ImageRequest {
    pub fn new(format: Format, data: impl Into<Vec<u8>>) -> Self {
        Self { format, data: data.into() }
    }

    /// Transmit this image to the terminal and return a live handle. The
    /// terminal allocates the placement/image namespace; we only reserve a
    /// client-side id for bookkeeping.
    pub fn transmit(self) -> ImageHandle {
        let id = next_id();
        Action::new()
            .cd("i", id)
            .push(self.format.into())
            .payload(base64::encode(&self.data))
            .commit();

        ImageHandle::new(Image { id, request: self, placements: Vec::new() })
    }
}

pub struct Image {
    id: ID,
    request: ImageRequest,
    /// Strong ownership of this image's placements. Dropping `Image` (via the
    /// last `ImageHandle`) cascades: every placement is yoinked to `None`.
    placements: Vec<PlacementHandle>,
}

pub type ImageHandle = Handle<Image>;

impl ImageHandle {
    /// Place this image at the given geometry, returning a yankable handle.
    /// `self` stays alive (a placement is subordinate to its image).
    pub fn place_named(&self, config: PlacementConfig) -> PlacementHandle {
        let p = PlacementConfig::clone(&config);
        let id = next_id();
        let image_id = read_guard(self).id;

        Action::new()
            .cd("a", "p")
            .cd("i", image_id)
            .cd("p", id)
            .pipe(|a| add_place_cds(a, &p))
            .commit();

        let handle = PlacementHandle::new(Some(Placement { id, image_id, config: p }));
        write_guard(self).placements.push(handle.clone());
        handle
    }

    /// Number of currently-live placements under this image.
    pub fn placed_count(&self) -> usize {
        let guard = read_guard(self);
        guard.placements.iter().filter(|p| p.alive()).count()
    }

    /// Explicitly delete this image: yoink its placements and tell the
    /// terminal to remove the image (which removes its placements too).
    pub fn delete(&self) {
        for p in &read_guard(self).placements {
            p.yoink();
        }
        let id = read_guard(self).id;
        Action::new().cd("a", "d").cd("d", "i").cd("i", id).commit();
    }
}

impl Drop for Image {
    fn drop(&mut self) {
        // Cascade: invalidate every placement the user may still hold.
        for p in &self.placements {
            p.yoink();
        }
        // Tell the terminal to drop the image + its placements as one unit.
        Action::new()
            .cd("a", "d")
            .cd("d", "i")
            .cd("i", self.id)
            .silent() // unimportant during teardown
            .commit();
    }
}

// ---------------------------------------------------------------------------
// Thin animation streamer (kept minimal; shows the model composes)
// ---------------------------------------------------------------------------

/// A single-image, frame-editing streamer. The base image is the root frame;
/// each `new_frame` appends an animation frame, `display` shows it.
pub struct TAnimation {
    base: ImageHandle,
    frames: usize,
}

impl From<ImageHandle> for TAnimation {
    fn from(base: ImageHandle) -> Self {
        Self { base, frames: 0 }
    }
}

impl TAnimation {
    pub fn new_frame(
        &mut self,
        base_frame: FrameSelector,
        image: ImageRequest,
        x: Option<u32>,
        y: Option<u32>,
    ) {
        let base_num = match base_frame {
            FrameSelector::Previous => self.frames + 1, // frame indices are 1-based
            FrameSelector::First => 1,
            FrameSelector::ID(n) => n,
        };

        Action::new()
            .cd("a", "f")
            .cd("i", read_guard(&self.base).id)
            .cd("c", base_num)
            .cd_opt("x", x)
            .cd_opt("y", y)
            .push(image.format.into())
            .payload(base64::encode(&image.data))
            .commit();

        self.frames += 1;
    }

    pub fn display_last(&self) {
        if self.frames > 0 {
            Action::new()
                .cd("a", "a")
                .cd("i", read_guard(&self.base).id)
                .cd("c", self.frames + 1) // frames are 1-based: base=1, first added=2 ...
                .commit();
        }
    }
}

#[derive(Clone, Copy)]
pub enum FrameSelector {
    Previous,
    First,
    ID(usize),
}

// Tiny helper so the builder pipelines read left-to-right.
trait Pipe: Sized {
    fn pipe(self, f: impl FnOnce(Self) -> Self) -> Self {
        f(self)
    }
}
impl Pipe for Action {}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yoink_invalidates_all_clones() {
        let img = ImageRequest::new(Format::Png, vec![1, 2, 3]).transmit();
        let p = img.place_named(PlacementConfig::default());

        let clone = p.clone();
        assert!(p.alive() && clone.alive());

        p.yoink(); // owner invalidates

        assert!(!p.alive());
        assert!(!clone.alive());
        assert_eq!(p.update(PlacementConfig::default()).unwrap_err(), Error::Dead);
    }

    #[test]
    fn dropping_image_cascades_to_placements() {
        let img = ImageRequest::new(Format::Png, vec![1, 2, 3]).transmit();
        let p = img.place_named(PlacementConfig::default());
        assert!(p.alive());

        drop(img); // last strong image handle gone -> cascade

        assert!(!p.alive());
    }
}