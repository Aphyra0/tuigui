//! A pty's output, as something else in the process can read it.
//!
//! ```text
//!   pty ──publish(chunk)──▶ Bytes ──▶ one queue per subscriber
//! ```
//!
//! The pty owns the command's output and does not know who is reading it. It hands out a
//! [`Bytes`] — a handle to what it produces — and a reader subscribes to that handle.
//! Nothing in the pty changes when a reader is added, which is the direction of the
//! dependency: the pty is not built around the server, the server reads the pty.
//!
//! # Why a queue per subscriber
//!
//! The pty publishes on its *forwarding* thread, which is blocking and owns no runtime. A
//! subscriber is asynchronous and has to be able to wait on a chunk alongside everything
//! else it is doing. Those are two different worlds, so chunks cross between them on a
//! queue — one copy per chunk, taken at the boundary where it is unavoidable.
//!
//! What the copy buys is the property the design needs: a subscriber that can `select!` on
//! the pty's output instead of being called back from it. A callback would run the
//! subscriber's work *on* the forwarding thread and leave it unable to wait on anything
//! else, which is exactly what the server has to do.
//!
//! # Fan-out, and what it costs
//!
//! Every subscriber gets every chunk, because the two readers here want different things
//! from the same bytes: the scan looks for an address, and the terminal wants the whole
//! stream. One queue shared between them would let whichever read first consume bytes the
//! other never saw, which is the bug this shape exists to prevent.
//!
//! A subscriber that stops reading fills its own queue and loses chunks in it, and only
//! its own. That is deliberate: losing output is bad, and stalling the command that
//! produced it is worse.

use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

/// One subscriber's queue, as the publisher holds it.
///
/// A `Sender` per subscriber, in a list the publisher owns: the pty does not know what its
/// subscribers are, only that there are some and where to put a chunk for each.
type Subscriber = mpsc::Sender<Vec<u8>>;

/// A pty's output.
#[derive(Clone)]
pub struct Bytes {
    /// Shared rather than owned, because the pty hands this out while it keeps publishing
    /// into it: one list of subscribers behind two handles.
    subscribers: Arc<Mutex<Vec<Subscriber>>>,
}

impl Bytes {
    pub fn new() -> Self {
        Self {
            subscribers: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Read every chunk the pty produces, from now on.
    ///
    /// Each subscriber has its own queue, so subscribing never takes a chunk from anyone
    /// else. The capacity is generous because a full queue costs a chunk: a terminal's
    /// output arrives in bursts of a few KiB, and a subscriber is normally parked waiting
    /// for exactly this.
    ///
    /// A subscription cannot be removed, which is right for both readers here: neither
    /// stops wanting the output while the session lasts.
    pub fn subscribe(&self) -> mpsc::Receiver<Vec<u8>> {
        let (chunks, stream) = mpsc::channel(256);
        self.views().push(chunks);
        stream
    }

    /// Offer one chunk to every subscriber. Called by the pty's forwarding thread.
    ///
    /// Never blocks and never waits: this runs on the path of every byte the command
    /// writes, and stalling there would stop the command's output. A subscriber that is
    /// behind loses chunks in its own queue rather than holding up the others or the
    /// command.
    pub fn publish(&self, chunk: &[u8]) {
        let mut views = self.views();
        // A send that fails because the queue is full is a chunk lost, and a send that fails
        // because the receiver is gone is a subscriber that has finished. The second is worth
        // forgetting, the first is not: dropping a slow subscriber would silently stop
        // serving it rather than merely lagging.
        views.retain(|view| match view.try_send(chunk.to_vec()) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => true,
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        });
    }

    /// How many readers are subscribed, for a caller that wants to report it.
    pub fn subscribers(&self) -> usize {
        self.views().len()
    }

    /// The subscriber list, holding the lock for as long as the borrow lives.
    ///
    /// A poisoned lock is taken back rather than propagated: the list is either the old
    /// one or the new one, so there is no half-written state to protect.
    fn views(&self) -> std::sync::MutexGuard<'_, Vec<Subscriber>> {
        self.subscribers.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Default for Bytes {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Bytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // How many are reading, never what they are: a subscriber is a channel, and the
        // count is the only part a log needs.
        f.debug_struct("Bytes")
            .field("subscribers", &self.subscribers())
            .finish()
    }
}
