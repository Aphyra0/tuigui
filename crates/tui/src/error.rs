//! What can go wrong in `tui`, as types rather than strings.
//!
//! Every failure here was previously an `io::Error::other(format!(...))`, which has
//! two costs: the caller cannot tell one failure from another without reading English
//! out of a message, and the `io::ErrorKind` is always `Other`, which says nothing.
//! A type per case is what lets a caller decide — a dropped frame is worth continuing
//! past, a failed transmit is not — and it is where the message belongs, next to the
//! code that knows what actually happened.
//!
//! `io::Error` is still the currency at the boundaries, because that is what the
//! streams, the runtime and the wire all speak. The conversion at the end is what
//! keeps the detail visible: `#[from]` carries the original error as a source, so a
//! chain printed by the binary still names the underlying cause.

/// A failure while serving a session.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The frame that arrived is not a JPEG, or not one this decoder accepts.
    ///
    /// Carries the frame's length because it is the one fact available when the bytes
    /// themselves cannot be shown: a truncated frame and a corrupted header look
    /// identical otherwise, and the length tells them apart.
    #[error("decoding a {len} byte frame: {source}")]
    Decode {
        len: usize,
        #[source]
        source: turbojpeg::Error,
    },

    /// The decoder returned pixels that do not match the size it declared.
    ///
    /// Not possible with a working decoder, which is why it is worth its own variant:
    /// if this is ever reached, the decoder's contract is the thing that broke, and a
    /// generic message would hide that.
    #[error("a decoded frame is {width}x{height}, which its buffer does not match")]
    FrameSize { width: u32, height: u32 },

    /// The image could not be turned into a request for its medium.
    ///
    /// Raised by staging the pixels for the terminal — a shared memory object that
    /// cannot be created, a PNG that cannot be encoded — so it is the medium's failure
    /// rather than the frame's.
    #[error("staging a frame for the terminal: {0}")]
    Stage(#[source] easytgp::TGPError),

    /// The graphics protocol rejected an operation.
    ///
    /// The terminal's own answer, passed through: it is the side that knows why it
    /// refused, and replacing its wording with ours would lose that.
    #[error("the graphics protocol failed: {0}")]
    Graphics(#[from] easytgp::TGPError),

    /// The image was not acknowledged by the terminal in time.
    ///
    /// A terminal that never answers will never draw the frame, so waiting past the
    /// point where a working one would have replied is waiting for nothing. Named
    /// separately from a protocol error because the terminal did not refuse: it said
    /// nothing at all, which is a different fault with a different cause.
    #[error("the terminal did not acknowledge a transmit within {0:?}")]
    TransmitTimeout(std::time::Duration),

    /// Writing to the screen failed.
    ///
    /// The terminal is gone, or the stream is not writable. Separate from [`Error::Io`]
    /// because the caller knows which stream it was writing to, and that is the fact
    /// worth reporting.
    #[error("writing to the screen: {0}")]
    Screen(#[source] std::io::Error),

    /// A stream failed.
    ///
    /// The catch-all, and deliberately last: anything with a name of its own above is
    /// something a caller might act on.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// The `Result` every fallible operation in this crate returns.
pub type Result<T> = std::result::Result<T, Error>;

/// What to do about a failed attempt to tell a client that something failed.
///
/// The reason this uses `futures-retry` rather than a fixed backoff strategy: these errors
/// are not the same kind of thing. A peer that is gone will still be gone in 20ms, so
/// retrying it only delays telling the user; a write that failed against a full pipe is
/// exactly the case that clears on its own. Retrying everything is what a strategy does,
/// and half of it would be waiting for nothing.
///
/// A named handler rather than a closure because the decision needs the attempt number, and
/// `futures-retry`'s blanket impl for closures passes only the error — the attempt is
/// available on the `ErrorHandler` trait and nowhere else.
#[derive(Debug, Default)]
pub(crate) struct ReportFailure;

impl futures_retry::ErrorHandler<std::io::Error> for ReportFailure {
    type OutError = std::io::Error;

    fn handle(
        &mut self,
        attempt: usize,
        e: std::io::Error,
    ) -> futures_retry::RetryPolicy<std::io::Error> {
        // More than one, because the first failure is often the transient one: a write that
        // raced a full pipe, a peer that had not started reading yet.
        const ATTEMPTS: usize = 3;
        // Short: this is a local write to a socket or a pipe, so a failure that clears will
        // clear almost immediately. A longer pause would only delay the pane saying what broke.
        const BACKOFF: std::time::Duration = std::time::Duration::from_millis(20);

        match (e.kind(), attempt) {
            // Nothing to wait for: the link is not there at all.
            (std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::NotConnected, _) => {
                futures_retry::RetryPolicy::ForwardError(e)
            }
            (_, attempt) if attempt >= ATTEMPTS => futures_retry::RetryPolicy::ForwardError(e),
            _ => futures_retry::RetryPolicy::WaitRetry(BACKOFF),
        }
    }
}

/// What a caller outside this crate gets.
///
/// `io::Error` is what the binary reports on and what `Tui::run` has always returned, so
/// the typed error is converted back at that one boundary rather than changing the public
/// signature.
///
/// The conversion flattens the message rather than carrying the error, because a graphics
/// error is not `Send + Sync` — it can hold a `Box<dyn Restorable>`, which is not — and
/// `io::Error::other` requires both. Inside the crate the typed error and its source are
/// intact, so a caller that wants to match on the cause has it; only this last hop to
/// `io::Error` loses the chain.
impl From<Error> for std::io::Error {
    fn from(error: Error) -> Self {
        std::io::Error::other(error.to_string())
    }
}
