//! The one writer to this terminal's stdout.

use std::io::{self, Write};
use std::sync::{Arc, RwLock};

/// A shared, serialized writer to one stream.
///
/// Cloning is the point: every part of `tui` that has something to put on the
/// screen holds one of these, and whichever gets there first writes a whole
/// batch before the next one starts. Serializing is not an optimization. Two
/// writers holding the stream directly would race, and an escape sequence split
/// by an unrelated write in the middle of it is a command the terminal obeys in
/// two halves.
///
/// `Sync` on the stream, not just `Send`: the whole point is that several
/// threads hold the same one.
#[derive(Clone)]
pub struct Sink(Arc<RwLock<Box<dyn Write + Send + Sync>>>);

impl Sink {
    pub fn new(stream: impl Write + Send + Sync + 'static) -> Self {
        Self(Arc::new(RwLock::new(Box::new(stream))))
    }

    /// Write a whole batch, held together under one lock.
    ///
    /// Overridden rather than left to the default `Write::write_all`, which
    /// loops over `write` and would take the lock once per chunk: a frame could
    /// then land in the middle of another writer's frame.
    pub fn write_all(&self, bytes: &[u8]) -> io::Result<()> {
        let mut stream = self.0.write().unwrap_or_else(|e| e.into_inner());
        stream.write_all(bytes)?;
        stream.flush()
    }
}

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write().unwrap_or_else(|e| e.into_inner()).write(buf)
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        Sink::write_all(self, buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.write().unwrap_or_else(|e| e.into_inner()).flush()
    }
}
