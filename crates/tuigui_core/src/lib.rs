// This crate exposes primitives used by tuigui.

/// Represents the source to be captured.
pub trait TGCompositor {
    fn spawn(command: std::process::Command) -> impl TGClient;
}

pub struct Snapshot {
    pub resolution: (u32, u32),
    pub bytes: Vec<u8>,
    pub damage: Option<Vec<()>>,
}

pub trait TGClient {
    fn snapshot() -> Snapshot;
    fn forward_input();
}

/// Backend that handles streaming the captured frames to an output.
pub trait TGBackend {}
