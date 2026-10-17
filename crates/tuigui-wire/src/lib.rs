//! Pieces the `tui` server and the `gui` client must agree on: the frame wire
//! format and the session file that tells a client where to connect.
//!
//! Kept in its own crate so neither side owns the other's copy. Both use only
//! half of it (the server decodes requests, the client writes them), which is
//! why the modules allow dead code rather than growing a second definition.

pub mod p2p;
pub mod session;
