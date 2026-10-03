// This crate exposes primitives used by tuigui.

/// Represents the source to be captured.
pub trait TGCompositor {
    fn init(resolution: (u32, u32), dpi: u32, no_x: bool) -> Self;
    fn spawn(&mut self, command: std::process::Command) -> Res<impl TGClient>;
}

pub trait TGClient {
    fn snapshot(&self) -> Res<Frame>;
    fn forward_input();
    fn is_running(&self) -> bool;
    fn children(&self) -> Vec<&impl TGClient>;
}

pub trait Source {
    fn next(&mut self) -> Option<Frame>;
}

pub struct Frame {
    pub size: (u32, u32),
    pub rgba: Vec<u8>,
}

/// Backend that handles streaming the captured frames to an output.
pub trait TGBackend {
    fn render(&mut self, frame: Frame) -> Res<()>;
}

// pub trait TerminalGraphicsProtocolBackend: TGBackend {}

pub type Res<T> = Result<T, Err>;

#[derive(Debug, thiserror::Error)]
pub enum Err {
    #[error("unknown data store error")]
    Unknown,
}
