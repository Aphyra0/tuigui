//! A view of the bytes a pty produces.
//!
//! The pty owns the command's output. Anything else that needs to look at those
//! bytes — today, the announcement scan — is handed a *view* of them rather than
//! a copy, because the bytes are already in hand on the forwarding path and
//! copying them would mean paying for the whole stream twice.
//!
//! ```text
//!   pty ──publish(&[u8])──▶ Bytes ──▶ views            (one slice, no copy)
//! ```
//!
//! This is the inverted form of "the pty is given a handle to its consumers".
//! The pty is the producer and does not know what is watching; a consumer is
//! given the producer's [`Bytes`] and subscribes. Nothing in the pty changes when
//! a consumer is added, which is what makes the direction of the dependency
//! right: the pty is not built around the server, the server subscribes to the
//! pty.
//!
//! # Cost
//!
//! One uncontended lock per *chunk*, not per byte, taken on the forwarding
//! thread. The chunk is up to 16 KiB by the time this is called, so the lock is
//! nothing against the scan and the write it accompanies. It is taken even with
//! no views registered, because that is what lets a view subscribe at any time
//! — including after the pump has started — rather than having to be in place
//! before the first byte, which would be a rule that is easy to break and silent
//! when broken.

use std::sync::{Arc, Mutex};

/// A pty's output, as something else in the process can look at.
#[derive(Clone)]
pub struct Bytes {
    /// Shared rather than owned, because the pty hands this out while it keeps
    /// publishing into it: it is one buffer of views behind two handles.
    views: Arc<Mutex<Vec<View>>>,
}

/// One consumer. Called with each chunk, in order, never concurrently.
type View = Box<dyn FnMut(&[u8]) + Send>;

impl Bytes {
    pub fn new() -> Self {
        Self {
            views: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Look at every chunk the pty produces, from now on.
    ///
    /// The view is called with the chunk that is already being forwarded, so it
    /// must not block: it runs on the path of every byte the command writes. It
    /// cannot alter the bytes either — it is handed a slice, never the buffer —
    /// which is the property that makes it safe to add one.
    ///
    /// A view with state that spans chunks (a matcher for a sequence that can
    /// split) is called one chunk at a time, in order, and never reentrantly, so
    /// that state does not need a lock of its own.
    ///
    /// Not from inside another view: the list is locked for the duration of a
    /// publish, so a view that subscribed would wait for itself. Nothing needs
    /// to, since the only views are the ones built before the pump starts.
    pub fn subscribe(&self, view: impl FnMut(&[u8]) + Send + 'static) {
        self.views().push(Box::new(view));
    }

    /// Hand one chunk to every view. Called by the pty's forwarding thread.
    ///
    /// Per *chunk*, not per byte: a chunk is whatever one read returned, so this
    /// is a handful of lock acquisitions a second against a stream that is
    /// already being read and written.
    pub(crate) fn publish(&self, chunk: &[u8]) {
        for view in self.views().iter_mut() {
            view(chunk);
        }
    }

    /// The views, holding the lock for as long as the borrow lives.
    ///
    /// A poisoned lock is taken back rather than propagated: a panicking view is
    /// one consumer's failure, and there is no half-written state to protect — the
    /// list of views is either the old one or the new one.
    fn views(&self) -> std::sync::MutexGuard<'_, Vec<View>> {
        self.views.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Default for Bytes {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Bytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // How many are watching, never what they are: a view is a closure and
        // cannot be printed, and the count is the only part a log needs.
        let views = self.views().len();
        f.debug_struct("Bytes").field("views", &views).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A view sees exactly the chunks the pty published, in order.
    #[test]
    fn a_view_sees_every_chunk_in_order() {
        let bytes = Bytes::new();
        let seen = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let sink = seen.clone();
        bytes.subscribe(move |chunk| sink.lock().unwrap().push(chunk.to_vec()));

        bytes.publish(b"first");
        bytes.publish(b"second");

        assert_eq!(&*seen.lock().unwrap(), &[b"first".to_vec(), b"second".to_vec()]);
    }

    /// A view may subscribe after publishing has started.
    ///
    /// The point of taking the lock per chunk rather than snapshotting the views
    /// once: a consumer that arrives late sees what happens next, instead of
    /// silently seeing nothing.
    #[test]
    fn a_view_may_subscribe_after_the_first_chunk() {
        let bytes = Bytes::new();
        // Published to nobody, which must not be a problem.
        bytes.publish(b"before");

        let seen = Arc::new(Mutex::new(Vec::<u8>::new()));
        let sink = seen.clone();
        bytes.subscribe(move |chunk| sink.lock().unwrap().extend_from_slice(chunk));
        bytes.publish(b"after");

        assert_eq!(&*seen.lock().unwrap(), b"after");
    }

    /// Several views all see the same bytes: subscribing does not consume.
    ///
    /// This is the property the old callback-on-the-proxy arrangement could not
    /// offer, and the reason a view is not a reader.
    #[test]
    fn several_views_each_see_the_same_chunk() {
        let bytes = Bytes::new();
        let first = Arc::new(Mutex::new(String::new()));
        let second = Arc::new(Mutex::new(String::new()));

        let a = first.clone();
        bytes.subscribe(move |chunk| a.lock().unwrap().push_str(&String::from_utf8_lossy(chunk)));
        let b = second.clone();
        bytes.subscribe(move |chunk| b.lock().unwrap().push_str(&String::from_utf8_lossy(chunk)));

        bytes.publish(b"shared");
        assert_eq!(&*first.lock().unwrap(), "shared");
        assert_eq!(&*second.lock().unwrap(), "shared");
    }

    /// A pty with nothing watching must publish without complaint.
    #[test]
    fn publishing_with_no_views_is_not_an_error() {
        let bytes = Bytes::new();
        bytes.publish(b"nobody is listening");
        assert_eq!(format!("{bytes:?}"), "Bytes { views: 0 }");
    }
}
