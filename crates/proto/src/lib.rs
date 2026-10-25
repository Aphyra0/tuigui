//! proto: the platform-neutral foundation under the windowing backends.
//!
//! Nothing in here knows about Wayland, Quartz, or a particular process. It is
//! the vocabulary the backends and their callers share, so a caller can be
//! written once against [`windowing`] and run on whichever backend a platform
//! provides.
//!
//! - [`windowing`] is the shape of a window session: the client tree, the
//!   windows it holds, the inputs a holder may push, and the frames it reads
//!   back. `wayland` implements it for Linux; a macOS backend implements it
//!   against Quartz (spec/PLATFORMS.md).

pub mod windowing;

/// What a backend may require of the state a holder parks on a client or a
/// window.
///
/// `'static` because a backend's record is stored behind a type-erased trait
/// object (a Wayland connection slot, a window-server association) and found
/// again by its type; `Send + Sync` because such a slot is shareable, even when
/// the backend itself only ever touches it from one thread.
///
/// Blanket-implemented, so a holder never writes it: satisfying those three
/// bounds is the whole requirement.
///
/// The blanket impl is what makes this usable as a bound, and it is not
/// optional. A supertrait bound alone is a one-way rule ("implementors must be
/// `Send + Sync + 'static`"); it grants no implementors. Without the second
/// line, every type a holder parked would need a hand-written `impl Shared`,
/// and the alias would be friction instead of sugar.
///
/// Consequence: `impl Shared for MyType` cannot be written by hand (it would
/// overlap the blanket impl), so the alias can never be claimed by a type that
/// does not actually satisfy the bounds.
pub trait Shared: Send + Sync + 'static {}

impl<T: Send + Sync + 'static> Shared for T {}

