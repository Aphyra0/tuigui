//! Pieces the `tui` server and the `gui` client must agree on.
//!
//! Three layers, deliberately separate:
//!
//! - [`proto`] is the **message** format: frames, pings, stats. It knows nothing
//!   about how the bytes travel.
//! - [`transport`] is the **channel** seam: it produces a bidirectional byte
//!   stream, and the message layer runs over whatever it is handed.
//! - [`message`] is the **conversation**: who may say what, which is a different
//!   question from what a frame contains.
//!
//! Keeping them apart is what lets a channel be swapped without touching the
//! protocol. A transport implements "give me a stream", never framing, so adding
//! one is small and cannot desynchronize the wire format.

pub mod message;
pub mod proto;
pub mod transport;

/// The message protocol, by its old name.
///
/// An alias so the rename from `p2p` (which was never peer-to-peer, it was the
/// wire format) does not break callers mid-change. Remove once nothing imports
/// it.
pub use proto as p2p;
