//! Pieces the `tui` server and the `gui` client must agree on.
//!
//! Three layers, deliberately separate:
//!
//! - [`message`] is the **conversation**: who may say what. `Ping` and `Pong` establish a
//!   session, a frame carries pixels, an error says something failed.
//! - [`transport`] is the **seam**: what a channel is, and the framing every channel shares.
//!   It names no channel, so nothing in it changes when one is added.
//! - [`unix`] is the first **channel**: one path, both halves. Here rather than in a binary
//!   because both ends of a session use it, and the two ends are separate crates.
//!
//! Keeping them apart is what lets a channel be added without touching the conversation, and
//! the conversation change without touching any channel.

pub mod message;
pub mod transport;
pub mod unix;
