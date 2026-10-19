//! Terminal graphics protocol (TGP): the wire format used to stream
//! pixel data into a graphics-capable terminal.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    fmt::Display,
    future::Future,
    io::{BufReader, Cursor, Read, Write},
    ops::Deref,
    pin::pin,
    sync::{Arc, Mutex, RwLock, RwLockReadGuard},
    task::Waker,
    thread::{sleep, JoinHandle},
    time::Duration,
};

use derive_deref::{Deref, DerefMut};
use derive_getters::Getters;
use idalloc::Slab;
use image::{PixelWithColorType, RgbImage};
use itertools::Itertools;

// TODO: https://sw.kovidgoyal.net/kitty/graphics-protocol/#unicode-placeholders
// TODO: https://sw.kovidgoyal.net/kitty/graphics-protocol/#relative-placements
// TODO: https://sw.kovidgoyal.net/kitty/graphics-protocol/#usage-hints
// TODO: Compression
// TODO: User-implemented full-recovery crate
// TODO: Notifications of the bottleneck.
// TODO: Better api to create fast placements
// TODO: Edit images
// TODO: Unicode placeholders

mod id {
    use crate::Id;
    use idalloc::Slab;
    use static_init::dynamic;

    /// The id slab, and a screen clear at first use.
    ///
    /// The termios work that used to live here has moved to [`Controller`], which is
    /// the one call where a caller hands over its terminal; doing it here meant it
    /// ran on whatever thread first touched the slab.
    ///
    /// The clear stays, because it is what makes a graphics program's own output
    /// legible rather than appended to whatever was on the screen. There is nothing
    /// to fail at, which is the difference from the termios code: a clear cannot
    /// turn into an abort on a stdin that is not a terminal.
    #[dynamic]
    static mut I: Slab<Id> = {
        use std::io::Write as _;
        print!("\x1b[2J\x1b[3J\x1b[H");
        let _ = std::io::stdout().flush();

        let mut slab = Slab::<Id>::new();
        // skip 0 as an id.
        slab.next();
        slab
    };

    /// Allocate the next unique image/placement id.
    pub(crate) fn alloc() -> Id {
        I.write().next()
    }

    /// Reclaim an id for reuse.
    pub(crate) fn free(id: Id) {
        I.write().free(id);
    }
}

/// Dedicated TGP writer: consumes fully-formed escape sequences off a channel
/// and writes + flushes them. Keeping this off the calling thread means the
/// pty-write stall (terminal decoding slower than we produce) can't block frame
/// encoding/comparison — the two overlap. One thread + one channel preserves
/// FIFO order, which kitty needs for its single-loading-slot rule.
mod writer {
    use static_init::dynamic;
    use std::io::{self, Write};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc::{self, Sender};
    use std::sync::{Arc, Mutex, OnceLock};

    /// Escape sequences produced but not yet flushed to the pty. Rising =
    /// the terminal can't keep up and the queue is piling up.
    pub(crate) static IN_FLIGHT: AtomicU64 = AtomicU64::new(0);

    /// Where escapes go. Defaults to this process's stdout, which is right when
    /// the renderer owns the terminal. A wrapper that serializes its own output
    /// under a lock (`tui`) installs its sink here instead, so escapes interleave
    /// with the wrapped command's bytes rather than racing them via a second
    /// independent writer to the same fd.
    static SINK: OnceLock<Arc<Mutex<dyn Write + Send>>> = OnceLock::new();

    /// Direct every later escape to `sink`. Returns false if a sink was already
    /// installed: moving it mid-stream would split one image's escapes across
    /// two destinations, so the first caller wins.
    pub fn set_sink(sink: Arc<Mutex<dyn Write + Send>>) -> bool {
        SINK.set(sink).is_ok()
    }

    /// The sink a caller that owns the terminal gets: this process's stdout.
    ///
    /// The same type as any other sink, so the default is not a special case in the
    /// writer below: an owning caller installs this and a sharing one installs its
    /// own locked writer, and the thread that drains the queue does one thing.
    pub fn stdout() -> Arc<Mutex<dyn Write + Send>> {
        Arc::new(Mutex::new(io::stdout()))
    }

    fn write_all(out: &mut dyn Write, bytes: &[u8]) {
        match SINK.get() {
            Some(sink) => {
                if let Ok(mut s) = sink.lock() {
                    let _ = s.write_all(bytes);
                    let _ = s.flush();
                }
            }
            None => {
                let _ = out.write_all(bytes);
                let _ = out.flush();
            }
        }
    }

    #[dynamic]
    static mut W_TX: Sender<Vec<u8>> = {
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            // Only used when no sink is installed.
            let mut out = io::stdout();
            loop {
                // Block for the first escape, then drain whatever else is
                // queued in the same batch. One write_all + one flush per
                // batch instead of per escape: fewer syscalls, and nothing
                // can pile up behind a flush mid-frame.
                let first = match rx.recv() {
                    Ok(m) => m,
                    Err(_) => break, // channel disconnected, all senders dropped
                };
                write_all(&mut out, &first);
                IN_FLIGHT.fetch_sub(1, Ordering::Relaxed);
                while let Ok(more) = rx.try_recv() {
                    write_all(&mut out, &more);
                    IN_FLIGHT.fetch_sub(1, Ordering::Relaxed);
                }
            }
        });
        tx
    };

    pub(crate) fn send(bytes: Vec<u8>) {
        let tx = W_TX.write();
        IN_FLIGHT.fetch_add(1, Ordering::Relaxed);
        tx.send(bytes).expect("tgp writer thread died");
        drop(tx);
    }
}

/// Send every TGP escape to `sink` instead of this process's stdout.
///
/// For a wrapper that owns multiple writers of one terminal: install its locked
/// stdout here so escapes and the wrapped command's output interleave correctly.
/// Returns false if a sink was already set.
pub fn set_output_sink(sink: Arc<Mutex<dyn std::io::Write + Send>>) -> bool {
    writer::set_sink(sink)
}

/// Number of escape sequences produced but not yet flushed to the pty.
/// Rising values mean the terminal is decoding slower than we produce.
pub fn writer_in_flight() -> u64 {
    writer::IN_FLIGHT.load(std::sync::atomic::Ordering::Relaxed)
}

pub trait Restorable: std::fmt::Debug {
    fn restore(&mut self, renew_dependents: bool) -> Res<()>;
    // fn renew_one();
}

#[static_init::dynamic]
static mut R: ReplyRouter = ReplyRouter {
    ..Default::default()
};

type Marker = u32;

struct PendingReady {
    pending: HashMap<ImgNumber, Waker>,
    ready: HashMap<ImgNumber, Option<Message>>,
}

#[derive(Default)]
struct ReplyRouter {
    pending: HashMap<Marker, Waker>,
    ready: HashMap<Marker, Option<Message>>,
}

impl ReplyRouter {
    fn start(stdin: impl std::io::Read + Send + 'static) -> JoinHandle<()> {
        Self::start_forwarding(stdin, std::io::sink())
    }

    /// Like [`ReplyRouter::start`], but the bytes that are *not* a reply are
    /// written to `forward` instead of being dropped.
    ///
    /// A terminal's replies arrive on the same descriptor as the keyboard, so a
    /// wrapper that owns that descriptor cannot simply hand it over: its own
    /// acknowledgements would be read as keystrokes by the command. Parsing here
    /// and passing on everything else is what splits one stream into the two
    /// things it actually carries, and `io::sink()` is what makes the ordinary
    /// path the same code with nowhere to put the extra bytes.
    fn start_forwarding(
        stdin: impl std::io::Read + Send + 'static,
        mut forward: impl std::io::Write + Send + 'static,
    ) -> JoinHandle<()> {
        std::thread::spawn(move || {
            let mut buf = BufReader::new(stdin)
                .bytes()
                .map_while(Result::ok)
                .peekable();

            let mut last = None;
            while let Some(reply) = Message::parse_next_with(&mut buf, &mut forward) {
                let iv = reply.get_from_key('i');
                if let Some(marker) = &iv {
                    let marker: u32 = marker.parse().unwrap();
                    let mut r = R.write();

                    if let Some(waker) = r.pending.remove(&marker) {
                        let attached = last.take();
                        r.ready.insert(marker, attached);
                        waker.wake();
                        id::free(marker);
                        continue;
                    }
                }
                last = Some(reply);
            }
        })
    }
}

pub struct Reply {
    // ident: CommandIdent,
    message: Option<Message>,
}

impl From<Reply> for Result<Reply, TGPError> {
    fn from(mut value: Reply) -> Self {
        if let Some(ref mut msg) = value.message {
            match str::from_utf8(msg.payload.take().unwrap().as_slice())? {
                "OK" => (),
                error => return Err(TGPError::Unknown(error.to_string())),
            }
        }

        Ok(value)
    }
}

/// Report a reply this library could not interpret.
///
/// The reply path cannot return an error: a [`PendingTransfer`] is a `Future`
/// whose `Output` is an image, so a malformed reply cannot become an `Err`
/// without changing the type of every transmit. Writing to stderr keeps it
/// visible, which matters because the alternative is an image whose cells
/// silently draw nothing.
fn log_reply_error(message: &str) {
    eprintln!("easytgp: {message}");
}

pub struct PendingTransfer<F: Format> {
    reply: ReplyFuture,
    returns: Option<Box<dyn FnOnce(Reply) -> DynImage<F>>>,
}

impl<F: Format> PendingTransfer<F> {
    fn new<C: FnOnce(Reply) -> DynImage<F> + 'static>(reply: ReplyFuture, returns: C) -> Self {
        Self {
            reply,
            returns: Some(Box::new(returns)),
        }
    }
}

#[derive(Debug)]
pub struct ReplyFuture {
    ident: ActionIdent,
    action_number: Marker,
}

#[derive(Debug)]
enum ActionIdent {
    ImageNumber { number: usize },
    ImageId { id: Id },
}

impl ReplyFuture {
    fn new(ident: ActionIdent) -> Self {
        // TODO: fix this nonesense
        let marker = id::alloc() + 99999;

        // Register eagerly: the ack can arrive before the future is ever
        // polled (sync() may not run until much later). Without this the
        // router sees no pending marker and drops the ack on the floor.
        //
        // if router calls on noop waker before the future is awaited the wake is ignored,
        // however the first poll on the future (from .await) will resolve to the ready
        R.write().pending.insert(marker, Waker::noop().clone());

        // TODO: Probably move inside the .send
        // Send marker as well, marking everything received before that to this action.
        Message::new()
            .cd('a', 'q')
            .cd('i', marker)
            .cd('f', 24)
            .cd('s', 1)
            .cd('v', 1)
            .payload("AAAA")
            .send_raw(false);

        ReplyFuture {
            ident,
            action_number: marker,
        }
    }
}

impl<F: Unpin + Format> Future for PendingTransfer<F> {
    type Output = Res<DynImage<F>>;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let s = self.get_mut(); // TODO: Should be zero-copy, but worth to verify

        std::pin::Pin::new(&mut s.reply)
            .poll(cx)
            .map(Into::<Res<Reply>>::into)
            .map_ok(|r| s.returns.take().unwrap()(r))
    }
}

impl Future for ReplyFuture {
    type Output = Reply;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let mut r = R.write();

        if let Some(reply) = r.ready.remove(&self.action_number) {
            return std::task::Poll::Ready(Reply { message: reply });
        }
        r.pending.insert(self.action_number, cx.waker().clone());

        std::task::Poll::Pending
    }
}

/// Take this terminal out of its line discipline, so replies can be read at all.
///
/// The reply router reads the terminal's answers off the caller's stdin, and the
/// line discipline decides when those bytes arrive. With `ICANON` on, input is held
/// until a newline — and a graphics reply has no newline, so the router never sees
/// one and every transmit waits forever. With `ECHO` on, whatever is typed comes
/// back into the stream being parsed.
///
/// Called by [`Controller::init_with_sink`], so no caller has to know that a
/// graphics library needs raw mode. A caller that wants its modes restored still
/// has to do that itself: this is a plug, not a guard.
///
/// Not a failure when there is no terminal. A pipe, a pty with no discipline, or a
/// test running with stdin from `/dev/null` are all legitimate, and they have no
/// modes to set — which is why nothing here is `unwrap`ped or returned. The version
/// of this that lived in a global initializer aborted the process on exactly those
/// cases.
pub fn take_terminal_raw() {
    let stdin = std::io::stdin();
    let Ok(mut modes) = rustix::termios::tcgetattr(&stdin) else {
        // Not a terminal, so there is no line discipline to turn off.
        return;
    };
    modes.make_raw();
    if let Err(e) =
        rustix::termios::tcsetattr(&stdin, rustix::termios::OptionalActions::Now, &modes)
    {
        log_reply_error(&format!("could not take the terminal out of line mode: {e}"));
    }
}

pub struct Controller {
    router_handle: JoinHandle<()>,
    num_alloc: Slab<ImgNumber>,
}

impl Controller {
    /// Read the terminal's replies off `stdin`, writing escapes to stdout.
    ///
    /// The path for a caller that owns the terminal outright: the renderer is the
    /// only writer, so there is nothing to share and nothing to strip out of the
    /// input.
    pub fn init(stdin: impl std::io::Read + Send + 'static) -> Arc<Controller> {
        Self::init_with_sink(stdin, std::io::sink(), writer::stdout())
    }

    /// [`Controller::init`], that also hands back the input the replies are mixed with.
    ///
    /// A terminal's answers and the user's keystrokes arrive on one descriptor, and
    /// only this library can tell them apart. A wrapper that owns that descriptor
    /// therefore hands it here and gets the input back on `forward`: everything that
    /// is not one of our replies, in order, with nothing else changed.
    ///
    /// So the ordinary path is this path with `io::sink()` for `forward`, which is
    /// what [`Controller::init`] is.
    pub fn init_with_forwarding(
        stdin: impl std::io::Read + Send + 'static,
        forward: impl std::io::Write + Send + 'static,
    ) -> Arc<Controller> {
        Self::init_with_sink(stdin, forward, writer::stdout())
    }

    /// [`Controller::init_with_forwarding`], with somewhere else to write escapes.
    ///
    /// `sink` receives every escape as one whole batch, which is what lets a wrapper
    /// put it under the same lock as its own output: an escape that landed in the
    /// middle of a line another writer was printing would be two commands the
    /// terminal obeys in halves.
    ///
    /// Call it **once per process**. The destination is global because
    /// [`Message::send`] can be reached without a controller — it is `Message` that
    /// knows how to encode itself, and a `Message` holds no reference to whoever
    /// made it — so there is one writer and one place to configure it. A second call
    /// returns `false` and changes nothing; the first caller wins, since moving the
    /// destination mid-stream would split one image's escapes across two.
    pub fn init_with_sink(
        stdin: impl std::io::Read + Send + 'static,
        forward: impl std::io::Write + Send + 'static,
        sink: Arc<Mutex<dyn std::io::Write + Send>>,
    ) -> Arc<Controller> {
        // TODO: this belongs to `Controller::new` once a `Message` can reach the
        // controller that made it, which is what removes the need for a global.
        if !writer::set_sink(sink) {
            log_reply_error("a graphics sink was already installed; the first one is in use");
        }
        // The router is about to read replies off this terminal, which it cannot do
        // while the line discipline is holding them.
        take_terminal_raw();
        Arc::new(Controller {
            router_handle: ReplyRouter::start_forwarding(stdin, forward),
            num_alloc: {
                let mut slab = Slab::new();
                slab.next();
                slab
            },
        })
    }

    /// Takes image::RgbaImage or image::RgbImage
    pub fn new_png(
        &self,
        image: image::ImageBuffer<impl AnyFormat, Vec<u8>>,
    ) -> Res<ImageRequest<Png>> {
        // TODO: allocate with the correct capacity.
        let mut bytes = vec![];
        image.write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)?;
        Ok(self.new_from_raw(Png, bytes, None))
    }

    pub fn new_rgb(&self, image: image::RgbImage, size: ImageSize) -> ImageRequest<Rgb> {
        self.new_from_raw(Rgb, image.into_vec(), Some(size))
    }

    pub fn new_rgba(&self, image: image::RgbaImage, size: ImageSize) -> ImageRequest<Rgba> {
        self.new_from_raw(Rgba, image.into_vec(), Some(size))
    }

    /// Create an image request from raw parts.
    pub fn new_from_raw<F: Format>(
        &self,
        format: F,
        bytes: Vec<u8>,
        size: Option<ImageSize>,
    ) -> ImageRequest<F> {
        ImageRequest {
            format,
            data: base64_turbo::STANDARD.encode(bytes).into_bytes(),
            size,
            shm: None,
        }
    }

    /// Stage packed RGB pixels in a shared memory object and return a request
    /// that transmits them with the `t=s` medium.
    ///
    /// The object is filled once and the wire command carries only its name, so
    /// no image bytes are base64-encoded or compressed. Compare
    /// [`Controller::new_rgb`], which encodes and inlines every pixel.
    ///
    /// Only use this when the terminal shares this machine's memory: across a
    /// network the object is on the wrong side of the connection. The terminal
    /// is expected to unlink the object once it has read it; the frame is held
    /// by the request until then.
    pub fn new_shm_rgb(&self, image: image::RgbImage) -> Res<ImageRequest<Shm>> {
        let size = ImageSize {
            width: image.width(),
            height: image.height(),
        };
        // The image buffer moves into the object rather than being copied: at
        // frame sizes a copy here is several megabytes of memcpy per frame.
        let frame = Arc::new(ShmFrame::create_owned(image.into_vec())?);
        Ok(ImageRequest {
            format: Shm,
            // The payload for this medium is the object's name, not pixels.
            data: frame.payload(),
            size: Some(size),
            shm: Some(frame),
        })
    }

    // TODO: route errors here.
    // The only way for the user to run action directly is through this
    pub fn raw_action(&self) -> Message {
        Message::new()
    }
}

impl PlacementHandle {
    /// Deletes the existing placement and returns a new one instead.
    pub fn replace(self, request: PlacementRequest) -> Self {
        todo!()
    }

    pub fn delete(self) {
        todo!()
    }
}

// controller creates and image
// image can place itself and preserve the placements
// pla
//
// vs
//
// ImageRequest -> ImageHandle;
//
// PlaceRequest -> PlacementHandle;
//
// can drop the image request or the placement request handles. They are linked in the double linked list.
//
// fh.edit();
//
//
//
//

// impl Renewable for Placement {
//     fn renew(self, c: &mut Controller) -> Self {}
// }

pub type Res<R> = Result<R, TGPError>;
pub type Id = u32;
pub type ImgNumber = u32;
pub type FrameIdx = usize;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct DynImageHandle {
    id: DynId,
}

// /// Optional store for the images. Can be used as a central registry for all images
// pub struct ImageStore {
//     images: HashMap<DynImageHandle, DynImage>,
// }

// impl ImageStore {
//     pub fn store(&mut self, image: DynImage) -> DynImageHandle {
//         let h = DynImageHandle {
//             id: image.id.clone(),
//         };
//         self.images.insert(h.clone(), image);
//         h
//     }

//     pub fn get(&mut self, handle: &DynImageHandle) -> Option<&mut DynImage> {
//         self.images.get_mut(handle)
//     }
// }

#[derive(Debug, Eq, Hash, PartialEq)]
pub struct PlacementHandle {
    id: DynId,
    request: Arc<PlacementRequest>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct FrameHandle {
    base_image: Id,
    index: usize,
    // request: Arc<NewFrameRequest>,
}

#[derive(Clone, Debug, Hash, Eq, PartialEq)]
enum DynId {
    Assigned(Arc<Id>),
    Waiting(),
}

impl DynId {
    pub fn new(id: Id) -> Self {
        Self::Assigned(Arc::new(id))
    }

    pub fn assigned(&self) -> Id {
        match self {
            DynId::Assigned(rw_lock) => **rw_lock,
            DynId::Waiting() => todo!(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ImageRequest<F: Format> {
    format: F,
    size: Option<ImageSize>,
    // Encoded in base64
    data: Vec<u8>,
    /// The shared memory object backing `data` when the medium is `t=s`.
    ///
    /// Held here rather than in the caller so it lives exactly as long as the
    /// image does: the terminal reads it when it parses the escapes, and the
    /// protocol has the terminal unlink what it reads.
    shm: Option<Arc<ShmFrame>>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ImageSize {
    pub width: u32,
    pub height: u32,
}

/// The `s`/`v` a frame command must carry, given whatever the caller supplied.
///
/// A frame with a raw format is sized from these values by the terminal, which
/// computes the payload length as `width * height * bytes_per_pixel` and rejects
/// a zero-sized frame outright. An explicit request wins; otherwise the image
/// knows its own size, and falling back to it is what makes a caller that passes
/// `..Default::default()` work.
fn resolve_dims(
    requested: Option<u32>,
    requested_other: Option<u32>,
    image: Option<ImageSize>,
) -> (Option<u32>, Option<u32>) {
    (
        requested.or(image.map(|s| s.width)),
        requested_other.or(image.map(|s| s.height)),
    )
}

// #[derive(Clone, Copy, Debug, Default)]
// pub enum Format {
//     #[default]
//     RGB,
//     RGBA,
//     PNG,
// }

// impl From<Format> for ControlData {
//     fn from(value: Format) -> Self {
//         ControlData(
//             "f",
//             match value {
//                 Format::RGB => 24,
//                 Format::RGBA => 32,
//                 Format::PNG => 100,
//             }
//             .to_string(),
//         )
//     }
// }

/// The pixel format a frame is transmitted in.
///
/// Public so a caller can be generic over the medium: the choice of [`Png`] or
/// [`Shm`] is the caller's, and a caller that wants one function for either cannot
/// name the bound otherwise.
///
/// `Unpin` is part of the contract rather than a bound the caller adds, because a
/// transmission is a `Future` and awaiting one needs it: a format that could not be
/// sent would be a format that implements this trait and nothing else.
pub trait Format: Copy + Clone + Default + Unpin + 'static {
    fn to_cd(self) -> ControlData;

    /// Build a request for this medium from decoded pixels.
    ///
    /// The constructor for the medium, named so that a caller generic over [`Format`]
    /// can build a request without branching on the type. Without it, a caller that
    /// wants one piece of code for either medium has to choose between
    /// [`Controller::new_png`] and [`Controller::new_shm_rgb`] at every frame, which
    /// is the branch the type parameter exists to remove.
    ///
    /// Required rather than defaulted: a new medium has to say how pixels reach the
    /// terminal, and a default would silently pick one of the two.
    fn request(controller: &Controller, pixels: RgbImage) -> Res<ImageRequest<Self>>;

    /// The wire byte for the protocol's `t` key: the transmission medium.
    ///
    /// Direct sends the pixel data inline in the escape. Shared memory (`s`)
    /// sends only an object *name*, and the terminal maps the pixels out of it.
    /// A parser that does not recognize a value treats it as direct.
    fn medium(self) -> &'static str {
        "d"
    }

    /// Whether `data` is payload bytes rather than an object name.
    ///
    /// Chunking only makes sense for inline payloads; a local medium is a single
    /// command naming an object, so it must never be split across `m=1` chunks.
    fn is_inline(self) -> bool {
        true
    }

    /// Whether a transmitted frame must state its pixel dimensions.
    ///
    /// The terminal sizes a raw frame's payload as
    /// `width * height * bytes_per_pixel`, and reports "Zero width/height not
    /// allowed" when those are absent. PNG instead carries its own size in its
    /// header, so declaring one is pointless there — and actively harmful: the
    /// declared size is still checked against the image, so a stale value would
    /// be rejected where omitting it was fine.
    fn needs_dimensions(self) -> bool {
        true
    }
}

#[derive(Clone, Copy, Default)]
pub struct Png;
impl Format for Png {
    fn to_cd(self) -> ControlData {
        ControlData('f', 100.to_string())
    }
    /// A PNG describes its own dimensions, so none are declared.
    fn needs_dimensions(self) -> bool {
        false
    }
    /// Encoded here, then carried inline: the pixels are in the escape itself.
    fn request(controller: &Controller, pixels: RgbImage) -> Res<ImageRequest<Self>> {
        controller.new_png(pixels)
    }
}

#[derive(Clone, Copy, Default)]
pub struct Rgb;
impl Format for Rgb {
    fn to_cd(self) -> ControlData {
        ControlData('f', 24.to_string())
    }
    /// Raw pixels have to be declared with the image's own size.
    fn request(controller: &Controller, pixels: RgbImage) -> Res<ImageRequest<Self>> {
        let size = ImageSize {
            width: pixels.width(),
            height: pixels.height(),
        };
        Ok(controller.new_rgb(pixels, size))
    }
}

#[derive(Clone, Copy, Default)]
pub struct Rgba;
impl Format for Rgba {
    fn to_cd(self) -> ControlData {
        ControlData('f', 32.to_string())
    }
    fn request(controller: &Controller, pixels: RgbImage) -> Res<ImageRequest<Self>> {
        // The alpha channel is not in the caller's buffer, so it is added opaque:
        // `new_rgba` takes RGBA and every pixel here is fully opaque.
        let width = pixels.width();
        let height = pixels.height();
        let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
        for pixel in pixels.pixels() {
            rgba.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 0xff]);
        }
        let image = image::RgbaImage::from_raw(width, height, rgba)
            .expect("the buffer is exactly width * height * 4 by construction");
        Ok(controller.new_rgba(
            image,
            ImageSize { width, height },
        ))
    }
}

/// RGB pixels delivered through a POSIX shared memory object (`t=s`).
///
/// The payload is the base64 of the object's *name*, not of the pixels: the
/// terminal opens the name, maps it, reads, and then unlinks it. That is the
/// point of the medium — no base64 of the image and no compression, so a frame
/// costs one pass over the pixels instead of two passes and a third more bytes.
///
/// Only meaningful when the terminal is on this machine; across a network the
/// object would be on the wrong side of the connection.
#[derive(Clone, Copy, Default)]
pub struct Shm;
impl Format for Shm {
    fn to_cd(self) -> ControlData {
        ControlData('f', 24.to_string())
    }
    fn medium(self) -> &'static str {
        "s"
    }
    fn is_inline(self) -> bool {
        false
    }
    /// Staged in a shared memory object: the escape carries the object's name, and
    /// the terminal maps it rather than decoding anything.
    fn request(controller: &Controller, pixels: RgbImage) -> Res<ImageRequest<Self>> {
        controller.new_shm_rgb(pixels)
    }
}

/// A POSIX shared memory object holding one image's pixels.
///
/// On Linux a POSIX shared memory object *is* a file under `/dev/shm`:
/// `shm_open("/name", …)` is `open("/dev/shm/name", …)`. Plain file I/O at that
/// path therefore produces an object the terminal's own `shm_open` resolves,
/// which avoids needing a libc binding here for a single syscall.
///
/// *Lifetime:* the specification makes the terminal responsible for unlinking
/// an object once it has read it, so this must outlive the transmit call and
/// must not unlink eagerly — racing the emulator to the name loses silently,
/// because the replies that would report it are suppressed. [`Drop`] removes
/// any name still present, which covers both a terminal that never read it and
/// a process that is going away.
pub struct ShmFrame {
    /// The name as the protocol wants it: a leading `/`, no other `/`.
    name: String,
    path: std::path::PathBuf,
    /// The pixels the object was filled from.
    ///
    /// The terminal unlinks the object once it has read it, so the name is only
    /// usable until the command is processed and cannot be re-sent. Keeping the
    /// bytes is what makes a second transmission possible, and it costs nothing
    /// extra: the caller's own copy is dropped after this, so the data moves
    /// here rather than being duplicated.
    data: Vec<u8>,
}

impl ShmFrame {
    /// The bytes this object was filled with.
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// Create an object from bytes the caller no longer needs.
    ///
    /// The owned form exists so the hot path does not copy a frame twice: with
    /// [`ShmFrame::create`] the caller's `Vec` becomes a slice and is cloned into
    /// the object, which for a full frame is several megabytes of memcpy.
    pub fn create_owned(data: Vec<u8>) -> Res<Self> {
        Self::build(data)
    }

    /// Create an object of `len` bytes and fill it with `data`.
    ///
    /// Fails on a platform with no `/dev/shm`, or when the name is already
    /// taken (a stale object from a dead process whose pid was reused).
    pub fn create(data: &[u8]) -> Res<Self> {
        Self::build(data.to_vec())
    }

    fn build(data: Vec<u8>) -> Res<Self> {
        let dir = std::path::Path::new(SHM_DIR);
        if !dir.is_dir() {
            return Err(TGPError::Shm(format!(
                "{SHM_DIR} is not available; the shared memory medium needs a POSIX shm filesystem"
            )));
        }
        // Names must fit the platform's limit (macOS caps them at 31 bytes
        // including the slash) and must contain no `/` after the first.
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let serial = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let name = format!("/easytgp{:x}-{:x}", std::process::id(), serial);
        let path = dir.join(name.trim_start_matches('/'));

        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| TGPError::Shm(format!("creating {}: {e}", path.display())))?;
        file.set_len(data.len() as u64)
            .and_then(|()| {
                use std::io::{Seek as _, Write as _};
                file.rewind()?;
                file.write_all(&data)
            })
            .map_err(|e| {
                // Never leave a partial object behind under a fresh name.
                let _ = std::fs::remove_file(&path);
                TGPError::Shm(format!("writing {}: {e}", path.display()))
            })?;
        Ok(Self {
            name,
            path,
            data,
        })
    }

    /// Create a fresh object holding the same bytes.
    ///
    /// The previous object is gone by now: the terminal unlinks what it reads,
    /// so a re-send has to name a new one. The bytes are already here, so this
    /// is one write and no pixel work.
    pub fn recreate(&self) -> Res<Self> {
        Self::create(&self.data)
    }

    /// The object's name, as the `t=s` payload is base64 of.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Re-transmit this frame as a fresh object with the same bytes.
    ///
    /// For putting an image back after a terminal discarded it: a whole-screen
    /// erase frees the terminal's *copy* of the image, and its own unlink has
    /// already removed ours, so a new object is needed. The bytes are already
    /// here, so this costs one write and no pixel work, where re-encoding from
    /// the caller's frame would cost a full frame of memcpy.
    pub fn resend(&self, size: ImageSize) -> Res<ImageRequest<Shm>> {
        let frame = Arc::new(self.recreate()?);
        Ok(ImageRequest {
            format: Shm,
            data: frame.payload(),
            size: Some(size),
            shm: Some(frame),
        })
    }

    /// The base64 payload this medium carries in place of pixel data.
    fn payload(&self) -> Vec<u8> {
        base64_turbo::STANDARD.encode(self.name.as_bytes()).into_bytes()
    }
}

impl std::fmt::Debug for ShmFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShmFrame").field("name", &self.name).finish()
    }
}

impl Drop for ShmFrame {
    fn drop(&mut self) {
        // The terminal normally got there first, in which case this is a no-op.
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Where Linux keeps POSIX shared memory objects.
const SHM_DIR: &str = "/dev/shm";

pub trait AnyFormat: image::Pixel<Subpixel = u8> + image::PixelWithColorType {}
impl AnyFormat for image::Rgb<u8> {}
impl AnyFormat for image::Rgba<u8> {}

impl<F: Format + 'static> ImageRequest<F> {
    // pub fn transplace(self, placement: PlacementRequest) -> DynImageHandle {
    //     let payload = base64_turbo::STANDARD.encode(self.data.as_slice());
    //     let a = Action::new()
    //         .cd("a", "T")
    //         .push_cd(self.format.into())
    //         .payload(payload);

    //     placement.add_cds(a).commit();
    //     // AnonymousImage { request: self }
    //     todo!()
    // }

    pub fn transmit(self) -> PendingTransfer<F> {
        let id = DynId::new(id::alloc());

        PendingTransfer::new(self.exec_transmit('I', id.assigned()), |r| {
            // The id comes from the terminal, so this is the only place that
            // learns it. A missing one used to panic, which took the whole
            // process down over a reply this layer failed to interpret. `DynId`
            // has no "unknown" state and `PendingTransfer` is a `Future` whose
            // `Output` is an image, so there is no `Err` to return: id 0 is
            // reported instead, which names no image and so draws nothing rather
            // than drawing the wrong thing.
            let id: u32 = r
                .message
                .as_ref()
                .and_then(|m| m.get_from_key('i'))
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            if id == 0 {
                log_reply_error("transmit: the terminal's reply carried no image id");
            }

            DynImage {
                id: DynId::Assigned(Arc::new(id)),
                request: self,
                placements: HashSet::new(),
                frames: HashMap::new(),
                pl_id_alloc: {
                    let mut slab = Slab::new();
                    slab.next();
                    slab
                },
                held: None,
                frame_shm: vec![],
                pending: vec![],
            }
        })
    }

    /// Transmit under an id that already exists, replacing its data.
    ///
    /// Named with `i=`, the image *id*. `I=` is the image *number*, which is a
    /// different namespace: the terminal answers a numbered transmit by creating
    /// a brand-new image and reporting its id, so re-sending under `I=` would
    /// leave the original pixels untouched and leak one image per frame until
    /// the storage quota evicted them.
    ///
    /// For a stream where the terminal may discard the image at any time: a
    /// fresh id each frame would invalidate every placeholder cell referencing
    /// the old one, because the id lives in the cell's color. Reusing the id
    /// means a re-send can be unconditional and the client never repaints.
    pub fn transmit_as(self, id: Id) -> PendingTransfer<F> {
        // Moved into the closure rather than captured by reference: the closure
        // is boxed as `'static`, since the reply may arrive much later.
        PendingTransfer::new(self.exec_transmit('i', id), move |_r| DynImage {
            id: DynId::Assigned(Arc::new(id)),
            request: self,
            placements: HashSet::new(),
            frames: HashMap::new(),
            pl_id_alloc: {
                let mut slab = Slab::new();
                slab.next();
                slab
            },
            held: None,
            frame_shm: vec![],
            pending: vec![],
        })
    }

    fn exec_transmit(&self, key: char, id: Id) -> ReplyFuture {
        // The `t` key must precede the payload: for a shared memory medium the
        // payload is an object name, not pixels.
        let medium = self.format.medium();
        let inline = self.format.is_inline();
        Message::new()
            .cd(key, id)
            .cd_opt('s', self.size.map(|s| s.width))
            .cd_opt('v', self.size.map(|s| s.height))
            .push_cd(self.format.to_cd())
            .cd('t', medium)
            // TODO: I don't like clone here
            .payload(self.data.clone())
            .send_medium(inline)
    }
}

pub struct DynImage<F: Format> {
    id: DynId,
    // Remember what placements are attached to this image.
    placements: HashSet<PlacementHandle>,
    frames: HashMap<FrameHandle, FrameIdx>,
    request: ImageRequest<F>,
    /// Shared memory objects backing frames that have been sent but not yet
    /// acknowledged.
    ///
    /// A frame transmission carries the object's *name*, and the terminal reads
    /// the object after it has parsed that command. Holding the request here for
    /// the first frame is not enough: every later frame is created from a fresh
    /// request, which would be dropped at the end of `new_frame`/`edit_frame`
    /// and unlink the object before the terminal ever opened it. Keeping the
    /// object alive until the reply arrives is what makes the medium usable for
    /// animation rather than only for a single image.
    frame_shm: Vec<Arc<ShmFrame>>,
    /// The object backing the most recently sent frame, kept after its reply
    /// lands.
    ///
    /// A terminal frees its own copy of the image on a whole-screen erase, and
    /// the object here is the only remaining copy of those pixels: the caller
    /// streams and does not repeat a frame. Holding it lets the image be sent
    /// again without re-encoding, which is the difference between one command
    /// and a full frame of memcpy per restore. See [`DynImage::last_object`].
    held: Option<Arc<ShmFrame>>,
    pl_id_alloc: Slab<Id>,
    pending: Vec<ReplyFuture>,
}

pub struct SyncData {
    /// How long the
    took: Duration,
}

#[derive(Debug)]
pub struct PendingSync<'a> {
    pending: &'a mut Vec<ReplyFuture>,
    /// Objects backing frames already sent.
    ///
    /// Released only once every pending op has been acknowledged: the reply is
    /// the terminal telling us it processed the command, and it reads the object
    /// while processing. Dropping one earlier would unlink it before the
    /// terminal opened it, which is the failure this whole lifetime exists to
    /// avoid.
    frame_shm: &'a mut Vec<Arc<ShmFrame>>,
    /// Where the last object goes instead of being dropped, so the image can be
    /// sent again after a terminal erases it.
    held: &'a mut Option<Arc<ShmFrame>>,
}

impl<'a> Future for PendingSync<'a> {
    type Output = Res<()>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        use std::task::Poll::*;
        let mut i = 0;
        let mut still_pending = false;
        while i < self.pending.len() {
            match pin!(&mut self.pending[i]).poll(cx) {
                Ready(r) => {
                    if let Err(e) = Into::into(r) {
                        return Ready(Err(e));
                    }
                    self.pending.swap_remove(i); // resolved → remove; don't bump i
                }
                Pending => {
                    // Still in flight: the inner future has registered our
                    // waker, so the router will re-poll us when it's done.
                    // Remember and keep it.
                    still_pending = true;
                    i += 1;
                }
            }
        }
        // True backpressure: only resolve once every op has been acked.
        // Otherwise we produce frames faster than the terminal can apply
        // them and the pipe floods.
        if still_pending {
            Pending
        } else {
            // Every op is acknowledged, so every object has been read. Releasing
            // them here is what keeps a long animation from accumulating a full
            // frame's pixels per frame.
            //
            // The newest one is kept: a terminal frees its own copy of the image
            // on a whole-screen erase, and this object is then the only copy of
            // those pixels left, since the caller streams and does not repeat a
            // frame. The vector is newest-first because frames are pushed in
            // order, so the last element was sent most recently.
            if let Some(last) = self.frame_shm.pop() {
                *self.held = Some(last);
            }
            self.frame_shm.clear();
            Ready(Ok(()))
        }
    }
}

impl<F: Format> DynImage<F> {
    /// The image's id on the wire.
    ///
    /// Needed by callers that must name the image in their own escapes, the
    /// main case being Unicode placeholders: the id is encoded in the
    /// placeholder cell's foreground color, so the client has to know it.
    pub fn id(&self) -> Id {
        self.id.assigned()
    }

    /// Wait until all pending operations related to the image are done and their status collected
    /// In cases of errors, the first one is being returned
    pub fn sync<'a>(&'a mut self) -> PendingSync<'a> {
        PendingSync {
            pending: &mut self.pending,
            frame_shm: &mut self.frame_shm,
            held: &mut self.held,
        }
    }

    /// The shared memory object backing the most recently displayed frame.
    ///
    /// Held past its reply so the frame can be transmitted again without
    /// re-encoding it, which is what a terminal needs after a whole-screen erase
    /// frees its own copy of the image.
    pub fn last_object(&self) -> Option<Arc<ShmFrame>> {
        self.held.clone()
    }

    pub fn place(&mut self, request: PlacementRequest) -> PlacementHandle {
        let id = request.placement_id.unwrap_or_else(id::alloc);
        let (fut, handle) = request.place(self.id.assigned(), id, "p");
        self.placements.insert(PlacementHandle {
            id: handle.id.clone(),
            request: handle.request.clone(),
        });

        self.pending.push(fut);

        handle
    }

    // TODO:
    // pub fn replace(&mut self, handle: PlacementHandle) {}

    pub fn displace(&mut self, handle: PlacementHandle) -> Res<PlacementRequest> {
        if self.placements.remove(&handle) {
            // ^ Drops the value, reducing reference counter to just one.
            // Now we can just safely unwrap making it zero-copy
            Ok(Arc::try_unwrap(handle.request).expect("reference counter to be at one"))
            // Note: because the handles provided to the user can't be cloned, nor user can request more
            // we can assume the counter is always at 2.
        } else {
            todo!()
        }
    }

    pub fn restore(&mut self) {}

    pub fn delete(self) {
        Message::new()
            .cd('a', "d")
            .cd('i', self.id.assigned())
            .send();
    }

    fn resolve_frame_selector(&self, selector: FrameSelector) -> usize {
        use FrameSelector::*;
        match selector {
            Last => self.frames.len() + 1,
            First => 1,
            ID(id) => id,
        }
    }

    pub fn new_frame(&mut self, config: NewFrameRequest<F>) -> FrameHandle {
        let base_frame = config.from_frame;
        let idx = self.frames.len() + 2;
        // First frame: no base canvas (c=0/absent — the frame IS the content).
        // Later frames: compose on top of the last existing frame.
        let base_idx = if self.frames.is_empty() {
            0
        } else {
            self.resolve_frame_selector(base_frame)
        };
        // The object outlives this call: the terminal reads it after parsing the
        // command this creates, so the image keeps it until the reply lands.
        if let Some(frame) = config.image.shm.clone() {
            self.frame_shm.push(frame);
        }
        let (handle, fut) = config.create(self.id.assigned(), base_idx, idx);

        self.frames.insert(handle.clone(), idx);
        self.pending.push(fut);
        handle
    }

    pub fn edit_frame(
        &mut self,
        handle: &FrameHandle,
        edit: FrameEdit<ComposeFromSelector, Upload<F>>,
    ) {
        let Some(frame_idx) = self.frames.get(handle) else {
            todo!()
        };

        let (message, inline) = match edit {
            FrameEdit::Compose(ComposeFromSelector {
                from_frame,
                width,
                height,
                src,
                dst,
            }) => (
                Message::new()
                    // Use frame composing api
                    .cd('a', "c")
                    // image context
                    .cd('i', self.id.assigned())
                    // source frame (base canvas) — spec: c = base data
                    .cd('c', self.resolve_frame_selector(from_frame))
                    // destination frame being edited — spec: r = edited frame
                    .cd('r', frame_idx)
                    // size
                    .cd('w', width)
                    .cd('h', height)
                    // source location
                    .cd('X', src.0)
                    .cd('Y', src.1)
                    // destination location
                    .cd('x', dst.0)
                    .cd('y', dst.1),
                true,
            ),

            FrameEdit::Upload(Upload {
                x,
                y,
                width,
                height,
                image,
            }) => {
                // A raw upload is sized from `s`/`v` by the terminal, so fall
                // back to the image's own dimensions rather than sending none.
                // A self-describing format declares nothing.
                let upload_dims = if image.format.needs_dimensions() {
                    resolve_dims(width, height, image.size)
                } else {
                    (None, None)
                };
                // The object must outlive this call, exactly as in `new_frame`.
                if let Some(frame) = image.shm.clone() {
                    self.frame_shm.push(frame);
                }
                (
                    Message::new()
                    // use standard frame action api
                    .cd('a', "f")
                    .cd('i', self.id.assigned())
                    // "r" instead of "c" so it edits the frame instead of creating the new one
                    .cd('r', frame_idx)
                    // A raw upload is sized from these, so fall back to the
                    // image's own dimensions rather than sending none; a
                    // self-describing format declares nothing, as above.
                    .cd_opt('s', upload_dims.0)
                    .cd_opt('v', upload_dims.1)
                    .cd_opt('x', x)
                    .cd_opt('y', y)
                    .push_cd(image.format.to_cd())
                    // Same as a frame transmission: name the medium, and do not
                    // chunk a payload that is an object name.
                    .cd('t', image.format.medium())
                    .payload(image.data.as_slice()),
                    image.format.is_inline(),
                )
            }
        };

        let reply = message.send_medium(inline);

        self.pending.push(reply);
        // PendingTransfer::new(reply, |_| ())
    }

    pub fn display_last_frame(&mut self) {
        self.display_frame(FrameSelector::Last)
    }

    // TODO: Use selector, rename to just display
    pub fn display_frame(&mut self, frame: FrameSelector) {
        let reply = Message::new()
            .cd('a', "a")
            .cd('i', self.id.assigned())
            .cd('c', self.resolve_frame_selector(frame))
            .send();

        self.pending.push(reply);
    }
}

// /// Simple image, can't be directly deleted, can't be animated. only supports restore.
// #[derive(Clone, Debug)]
// pub struct AnonymousImage {
//     data: ImageRequest,
//     place: PlacementRequest,
// }

/// Animation based on a single image
/// and with the single frame
///
/// all changes are placed on the overlay.
///
/// Updates are not atomics, but it doesn't create new frames.
// pub struct ThinStreamer<F: Format> {
//     img: DynImage<F>,
// }

// impl ThinStreamer {
//     // pub fn new(base: ImageRequest) -> Self {
//     //     // ThinStreamer {
//     //     //     animation: base.transmit().into(),
//     //     // }
//     // }

//     pub fn update(&mut self, edit: FrameEdit<ComposeFromPrevious, Upload>) {
//         self.img.edit_frame(todo!(), edit.into());
//     }
// }

// pub struct AtomicStreamer {
//     animation: Animation,
//     pending: bool,
// }

// impl AtomicStreamer {
//     pub fn update(&mut self, patch: NewImage, offset: (u32, u32)) {
//         if !self.pending {
//             // create new frame
//             self.animation.new_frame(NewFrame {
//                 base_frame: FrameSelector::Last,
//                 image: patch,
//                 width: None,
//                 height: None,
//                 x: Some(offset.0),
//                 y: Some(offset.1),
//             });

//             self.pending = true;
//         } else {
//             // Edit frame instead
//             self.animation
//                 .frame(FrameSelector::Last)
//                 .unwrap()
//                 .edit(FrameEdit::Upload {
//                     x: (),
//                     y: (),
//                     width: (),
//                     height: (),
//                     image: (),
//                 });
//         }
//     }

//     pub fn present(&mut self) {}
// }

#[derive(Default)]
pub struct NewFrameRequest<F: Format> {
    pub from_frame: FrameSelector,
    pub image: ImageRequest<F>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub x: Option<u32>,
    pub y: Option<u32>,
}

impl<F: Format> NewFrameRequest<F> {
    fn create(
        self,
        // image where the change is happning
        base_image: Id,

        // compose from the frame at the given index
        from_frame_idx: FrameIdx,

        // frame index that is given to the new frame
        new_frame_idx: FrameIdx,
    ) -> (FrameHandle, ReplyFuture) {
        // A raw frame is sized from these by the terminal, which computes the
        // payload length as `width * height * bytes_per_pixel` and rejects a
        // zero-sized one. A self-describing format declares nothing: the size is
        // still checked against the image, so a wrong value would be worse than
        // none at all.
        let (width, height) = if self.image.format.needs_dimensions() {
            resolve_dims(self.width, self.height, self.image.size)
        } else {
            (None, None)
        };
        let reply = Message::new()
            .cd('a', "f")
            .cd('i', base_image)
            .cd_if(from_frame_idx > 0, 'c', from_frame_idx)
            .cd_opt('s', width)
            .cd_opt('v', height)
            .cd_opt('x', self.x)
            .cd_opt('y', self.y)
            .push_cd(self.image.format.to_cd())
            // The medium must be named on a frame transmission too, or an
            // object name would be read as inline pixel data.
            .cd('t', self.image.format.medium())
            .payload(self.image.data)
            .send_medium(self.image.format.is_inline());

        (
            FrameHandle {
                base_image,
                index: new_frame_idx,
            },
            reply,
        )
    }
}

#[derive(Clone, Copy, Default)]
pub enum FrameSelector {
    #[default]
    Last,
    First,
    ID(usize),
    // TODO?: Previous
}

impl FrameHandle {}

pub enum FrameEdit<C, U> {
    Compose(C),
    Upload(U),
}

pub struct ComposeFromSelector {
    from_frame: FrameSelector,
    // TODO: Make optionals
    width: u32,
    height: u32,
    src: (u32, u32),
    dst: (u32, u32),
}

pub struct ComposeFromPrevious {
    width: u32,
    height: u32,
    src: (u32, u32),
    dst: (u32, u32),
}

pub struct Upload<F: Format> {
    pub x: Option<u32>,
    pub y: Option<u32>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub image: ImageRequest<F>,
}

// impl<U> From<FrameEdit<ComposeFromPrevious, U>> for FrameEdit<ComposeFromSelector, U> {
//     fn from(value: FrameEdit<ComposeFromPrevious, U>) -> Self {
//         match value {
//             FrameEdit::Compose(ComposeFromPrevious {
//                 width,
//                 height,
//                 src,
//                 dst,
//             }) => FrameEdit::Compose(ComposeFromSelector {
//                 from_frame: FrameSelector::Last,
//                 width,
//                 height,
//                 src,
//                 dst,
//             }),
//             FrameEdit::Upload(u) => FrameEdit::Upload(u),
//         }
//     }
// }

#[derive(Debug, Clone, Default, Eq, Hash, PartialEq)]
pub struct PlacementRequest {
    pub offset_x: Option<u32>,
    pub offset_y: Option<u32>,
    // TODO: make this an enum
    pub cols: Option<u32>,
    pub rows: Option<u32>,
    pub x: Option<u32>,
    pub y: Option<u32>,
    pub z: Option<i32>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// After placing an image on the screen the cursor must be moved to the right by the number of cols in the image placement rectangle and down by the number of rows in the image placement rectangle.
    /// This option sets the cursor movement policy to no movement for placing the current image.
    pub fix_cursor: bool,
    /// Create a *virtual* placement (`U=1`): a prototype that has no screen
    /// position of its own. Unicode placeholder cells resolve against it, which
    /// is how an image is anchored to text a multiplexer is free to move.
    pub virtual_placement: bool,
    /// A placement id to reuse instead of allocating one (`p=`).
    ///
    /// Placement ids are scoped to their image, so the same value on two images
    /// is two placements, not a conflict. Naming it lets a caller move an image
    /// swap onto a known placement id rather than one that changes per call.
    pub placement_id: Option<u32>,
    // TODO:
    // H, V
}

impl PlacementRequest {
    pub fn place(
        self,
        image_id: Id,
        id: Id,
        action: &'static str,
    ) -> (ReplyFuture, PlacementHandle) {
        (
            self.exec_place(image_id, id, action),
            PlacementHandle {
                id: DynId::new(id),
                request: Arc::new(self),
            },
        )
    }

    fn exec_place(&self, image_id: Id, id: Id, action: &'static str) -> ReplyFuture {
        Message::new() //
            .cd('a', action)
            .cd('i', image_id)
            .cd('p', id)
            .cd_opt('X', self.offset_x)
            .cd_opt('Y', self.offset_y)
            .cd_opt('c', self.cols)
            .cd_opt('r', self.rows)
            .cd_opt('x', self.x)
            .cd_opt('y', self.y)
            .cd_opt('z', self.z)
            .cd_opt('w', self.width)
            .cd_opt('h', self.height)
            .cd_if(self.fix_cursor, 'C', 1)
            .cd_if(self.virtual_placement, 'U', 1)
            .send()
    }

    // /// Adds attribute related cds
    // fn add_cds(&self, a: Action) -> Action {
    //     a // add cds
    //         .cd("i", self.image.as_ref().unwrap().read().unwrap().id)
    //         .cd_opt("X", self.offset_x)
    //         .cd_opt("Y", self.offset_y)
    //         .cd_opt("c", self.cols)
    //         .cd_opt("r", self.rows)
    //         .cd_opt("x", self.x)
    //         .cd_opt("y", self.y)
    //         .cd_opt("z", self.z)
    //         .cd_opt("w", self.width)
    //         .cd_opt("h", self.height)
    //         .cd_if(self.fix_cursor.is_some(), "C", 1)
    // }
}

#[derive(Debug, Clone)]
/// Kitty Application Programming Command.
pub struct Message {
    cds: Vec<ControlData>,
    payload: Option<Vec<u8>>,
}

impl Message {
    // returns value for either I, or i. If both specified defaults to I.
    pub fn ident(&self) -> ActionIdent {
        // TODO: refactor
        self.cds
            .iter()
            .find_map(|cd| {
                if cd.0 == 'I' {
                    Some(ActionIdent::ImageNumber {
                        number: cd.1.parse().unwrap(),
                    })
                } else if cd.0 == 'i' {
                    Some(ActionIdent::ImageId {
                        id: cd.1.parse().unwrap(),
                    })
                } else {
                    None
                }
            })
            .unwrap()
    }
    // TODO: use async so this function isn't blocking forever
    /// Takes a byte iterator returns first parsed action consuming bytes along the way.
    /// If none found will block until the end of the iterator (usually on the program exit) and returns None
    fn parse_next(buf: &mut std::iter::Peekable<impl Iterator<Item = u8>>) -> Option<Message> {
        Self::parse_next_with(buf, &mut std::io::sink())
    }

    /// [`Message::parse_next`], which also passes on the bytes that are not a reply.
    ///
    /// The descriptor a terminal answers on is the same one the keyboard arrives
    /// on, so what is *not* a reply is the user's input and has to be written
    /// somewhere. Nothing else changes: the same intro is matched, the same control
    /// data is parsed, the same message comes back.
    ///
    /// A byte that looks like the start of an intro but does not become one is
    /// input and is forwarded, which is why the candidate is held rather than
    /// written immediately: an `ESC` alone is the beginning of every sequence a
    /// keyboard sends, and forwarding it the moment it is read would split a key.
    ///
    /// TODO: the search for an intro is a byte-at-a-time loop, which is what makes
    /// it obviously correct and not fast. A real one scans with a `memchr` and
    /// copies the runs between replies in bulk.
    fn parse_next_with(
        buf: &mut std::iter::Peekable<impl Iterator<Item = u8>>,
        forward: &mut impl std::io::Write,
    ) -> Option<Message> {
        const ESC: u8 = b'\x1b';
        while let Some(b) = buf.next() {
            // Only start parsing with the correct initial sequence
            if b == ESC {
                // The two bytes after an `ESC` decide whether this is a reply or a
                // key. They are read here either way, so the ones that were not an
                // intro go on rather than being eaten: an arrow key starts with
                // `ESC` too, and dropping what was read would type `\x1b` and lose
                // the rest of the key.
                let second = buf.next()?;
                let third = buf.next()?;
                if second == b'_' && third == b'G' {
                    let mut a = Message::new();
                    // Protocol doesn't have zero control data operations,
                    // so it's safe to assume in a valid reply at least one ControlData will be parsed
                    while let Some(cd) = ControlData::parse(buf) {
                        a = a.push_cd(cd);

                        match buf.next()? {
                            b';' => break,
                            b',' => continue,
                            _ => return None,
                        };
                    }

                    let mut payload = String::new();
                    while let Some(b) = buf.next() {
                        if b == ESC && buf.next()? == b'\\' {
                            return Some(a.payload(payload));
                        }

                        payload.push(b as char);
                    }
                } else {
                    // Not a reply, so these three bytes are input.
                    let _ = forward.write_all(&[b, second, third]);
                    let _ = forward.flush();
                }
            } else {
                // Not an intro, so it is the user's input and goes straight on. The
                // error is dropped rather than propagated because this returns an
                // `Option`: a forward with nowhere to go is not a parse failure.
                // Flushed per byte because the reader is an interactive program
                // waiting on a keystroke, and a held byte is a key that was not typed.
                let _ = forward.write_all(&[b]);
                let _ = forward.flush();
            }
        }

        None
    }
    // fn parse_next(
    //     // TODO: maybe use generics
    //     bytes: &mut std::io::Bytes<BufReader<impl Read + Send + 'static>>,
    // ) -> Option<Action> {
    //     todo!()
    // }

    // This is private, users are supposed to use Controller for the manual actions, this guarantees that the router is configured.
    // TODO: make this take initial action keys
    fn new() -> Message {
        Message {
            cds: vec![],
            payload: Some(vec![]),
        }
    }

    pub fn payload(mut self, payload: impl Into<Vec<u8>>) -> Self {
        self.payload.replace(payload.into());
        self
    }

    fn get_from_key(&self, key: char) -> Option<String> {
        self.cds
            .iter()
            .find(|cd| cd.0 == key)
            .map(|cd| cd.1.clone())
    }

    // TODO: Narrow the scope of the value
    /// Takes ownership
    fn cd(self, key: char, value: impl Display) -> Self {
        self.push_cd(ControlData(key, value.to_string()))
    }

    fn cd_opt(self, key: char, value_o: Option<impl Display>) -> Self {
        if let Some(value) = value_o {
            self.push_cd(ControlData(key, value.to_string()))
        } else {
            self
        }
    }

    fn cd_if(self, condition: bool, key: char, value: impl Display) -> Self {
        if condition {
            self.push_cd(ControlData(key, value.to_string()))
        } else {
            self
        }
    }

    fn push_cd(mut self, cd: ControlData) -> Self {
        self.cds.push(cd);
        self
    }

    // TODO: implement chunking
    /// execute the command
    pub fn send(mut self) -> ReplyFuture {
        self.send_medium(true)
    }

    /// Execute the command, chunking the payload only when it is inline.
    ///
    /// A local medium (`t=s`, `t=f`) carries an object name rather than data, so
    /// it is one command by construction: the `m` key is meaningless for it and
    /// splitting it would hand the terminal a fragment of a name. `inline` says
    /// which of the two this is.
    fn send_medium(mut self, inline: bool) -> ReplyFuture {
        const CHUNK_SIZE: usize = 16 * 1024;
        const LOCK: Mutex<()> = Mutex::new(());

        // TODO: might be missed because of mismatching spaces
        let is_frame_edit = self.cds.contains(&ControlData('a', "f".to_string()));

        let payload = self.payload.take().unwrap_or_default();
        let aident = self.ident();

        if !inline {
            // One command, no chunk state, and no `m` key. The lock is taken as
            // a binding so it lives to the end of the block.
            let lock = LOCK;
            let guard = lock.lock().unwrap();
            self.payload(payload).send_raw(false);
            drop(guard);
            return ReplyFuture::new(aident);
        }

        let mut chunks = payload.chunks(CHUNK_SIZE).into_iter().peekable();

        // Critical section goes here
        {
            let lock = LOCK;
            let l = lock.lock().unwrap();

            self.payload(chunks.next().unwrap_or_default())
                .cd_if(chunks.peek().is_some(), 'm', 1)
                .send_raw(false);

            while let Some(chunk) = chunks.next() {
                Message::new()
                    .cd('m', if chunks.peek().is_some() { 1 } else { 0 })
                    .cd_if(is_frame_edit, 'a', "f")
                    .payload(chunk.to_vec())
                    .send_raw(false);
            }

            drop(l)
        }

        // TODO: maybe do in the critical section.
        ReplyFuture::new(aident)
    }

    fn send_raw(mut self, no_reply: bool) {
        // CRITICAL: this is not supposed to be here.
        //
        // Every caller passes `false`, so every command asks the terminal for a
        // reply, and nothing in `tui` reads those replies: the router would have
        // to read this process's stdin, which is the keyboard the proxy forwards,
        // so reading it would eat the user's keystrokes. The acks are therefore
        // expected to be consumed by whatever is between us and the terminal --
        // a multiplexer feeding our own pane's input -- and dropped. That is a
        // gamble, and a rework of the reply path is what replaces it.
        self = self.cd_if(no_reply, 'q', 2);
        let bytes = self.into_escape();
        // Through the writer, not straight to fd 1. A wrapper that serializes
        // several writers of one terminal (`tui`) installs a sink here, and
        // writing around it would race the wrapped command's bytes: an escape
        // could land in the middle of a line the command is printing.
        writer::send(bytes);
    }

    /// The escape sequence this message encodes to, exactly as it goes on the wire.
    ///
    /// Separated from the write so the encoding can be asserted without a
    /// terminal: `\x1b_G<cds>;<payload>\x1b\\`.
    fn into_escape(self) -> Vec<u8> {
        let cds = self
            .cds
            .iter()
            .map(|cdv| cdv.format())
            .collect_vec()
            .join(",");
        // The payload is base64 (or a fragment of it), so it is already ASCII.
        let payload = String::from_utf8(self.payload.unwrap_or_default()).unwrap();
        format!("\x1b_G{cds};{payload}\x1b\\").into_bytes()
    }
}
#[derive(thiserror::Error, Debug)]
pub enum TGPError {
    #[error("Entity was evicted")]
    Evicted(Box<dyn Restorable>),
    #[error("Entity you are trying to use was indirectly or directly deleted")]
    Deleted(Box<dyn Restorable>),
    #[error("Terminal emulator returned an error: {0}")]
    Unknown(String),

    #[error(transparent)]
    ImageProcessing(#[from] image::ImageError),

    #[error(transparent)]
    Utf8Error(#[from] std::str::Utf8Error),

    #[error("shared memory: {0}")]
    Shm(String),
}

impl TGPError {
    pub fn restore_restorable(self) -> Res<()> {
        match self {
            TGPError::Evicted(mut entity) =>
            // try renewing everything we can.
            {
                entity.restore(true)
            }
            e => Err(e),
        }
    }
}

// TODO:
// enum Medium {
//     Direct,
//     File,
//     TempFile,
//     Shm,
// }

// TODO:
// The PNG format is specified using the f=100 key. The width and height of the image will be read from the PNG data itself. Note that if you use both PNG and compression, then you must provide the S key with the size of the PNG data.
#[derive(Debug, Clone, PartialEq)]
struct ControlData(char, String);

impl ControlData {
    fn format(&self) -> String {
        format!("{}={}", self.0, self.1.trim())
    }

    /// Takes an iter and consumes first <k>=<v> terminated by either `,` or `;` (terminators aren't consumed)
    /// returns parsed struct
    fn parse(buf: &mut std::iter::Peekable<impl Iterator<Item = u8>>) -> Option<Self> {
        let k = buf.next()?;

        if !matches!(buf.next(), Some(b'=')) ||
            // assert at least one byte for the value
            buf.peek().is_none()
        {
            return None;
        };

        let mut v = String::new();
        while let Some(b) = buf.next() {
            v.push(b as char);

            let peek = *buf.peek()?;
            if peek == b';' || peek == b',' {
                break;
            }
        }

        Some(ControlData(k as char, v))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Option<Message> {
        Message::parse_next(&mut s.bytes().peekable())
    }

    fn cds(reply: &Message) -> Vec<(char, &str)> {
        reply.cds.iter().map(|cd| (cd.0, cd.1.as_str())).collect()
    }

    #[test]
    fn single_control_data_with_empty_payload() {
        let r = parse("\x1b_Gi=31;\x1b\\").unwrap();
        assert_eq!(cds(&r), [('i', "31")]);
        assert_eq!(r.payload.as_deref(), Some(&[] as &[u8]));
    }

    #[test]
    fn multiple_control_data_kept_in_order() {
        let r = parse("\x1b_Gi=31,q=1,m=0;\x1b\\").unwrap();
        assert_eq!(cds(&r), [('i', "31"), ('q', "1"), ('m', "0")]);
    }

    #[test]
    fn payload_after_terminator_is_captured() {
        let r = parse("\x1b_Gi=31;AAEC\x1b\\").unwrap();
        assert_eq!(cds(&r), [('i', "31")]);
        assert_eq!(r.payload.as_deref(), Some("AAEC".as_bytes()));
    }

    #[test]
    fn noise_before_the_intro_is_skipped() {
        let r = parse("junk\x1b[31m\x1b_Gi=31;\x1b\\").unwrap();
        assert_eq!(cds(&r), [('i', "31")]);
    }

    /// The object a `t=s` command names has to exist, hold exactly the pixels,
    /// and be resolvable the way a terminal's `shm_open` resolves it.
    #[test]
    fn shm_frame_holds_the_pixels_under_the_name_it_reports() {
        let pixels: Vec<u8> = (0..=255u8).cycle().take(300).collect();
        let frame = ShmFrame::create(&pixels).expect("create shm object");

        // The protocol requires a leading slash and no other slash, and name
        // lengths must fit the platform limit (macOS: 31 bytes).
        assert!(frame.name().starts_with('/'), "{}", frame.name());
        assert_eq!(frame.name().matches('/').count(), 1, "{}", frame.name());
        assert!(frame.name().len() <= 31, "{}", frame.name());

        // A terminal resolves the name through shm_open, which on Linux is the
        // same file under /dev/shm.
        let path = std::path::Path::new(SHM_DIR).join(frame.name().trim_start_matches('/'));
        assert_eq!(std::fs::read(&path).expect("readable"), pixels);

        // The payload is base64 of the name, not of the pixels: that is the
        // whole difference between this medium and the inline one.
        let decoded = base64_turbo::STANDARD
            .decode(frame.payload())
            .expect("payload decodes");
        assert_eq!(decoded, frame.name().as_bytes());
        assert!(decoded.len() < pixels.len() / 4, "payload must be the name");

        // Dropping removes the object so a long session does not leak names.
        let leaked = path.to_path_buf();
        drop(frame);
        assert!(std::fs::read(&leaked).is_err(), "object outlived the frame");
    }

    #[test]
    fn shm_format_declares_the_shared_memory_medium() {
        assert_eq!(Shm.medium(), "s");
        assert!(!Shm.is_inline(), "a name payload must never be chunked");
        // RGB, so the terminal interprets the object as packed pixels.
        assert_eq!(Shm.to_cd().format(), "f=24");
    }

    #[test]
    fn inline_formats_stay_on_the_direct_medium() {
        // Each is a distinct type, so they are asserted one at a time.
        assert_eq!(Png.medium(), "d");
        assert!(Png.is_inline());
        assert_eq!(Png.to_cd().format(), "f=100");

        assert_eq!(Rgb.medium(), "d");
        assert!(Rgb.is_inline());
        assert_eq!(Rgb.to_cd().format(), "f=24");

        assert_eq!(Rgba.medium(), "d");
        assert!(Rgba.is_inline());
        assert_eq!(Rgba.to_cd().format(), "f=32");
    }

    /// The wire command a shared memory transmit produces: the medium is named,
    /// the payload is the object's name, and there is no `m` key because a name
    /// is never chunked.
    #[test]
    fn shm_transmit_puts_the_medium_and_a_name_on_the_wire() {
        let pixels = vec![7u8; 4 * 4 * 3];
        let frame = ShmFrame::create(&pixels).expect("create shm object");
        let name_payload = frame.payload();

        // What ImageRequest::exec_transmit assembles for an Shm format.
        let escape = Message::new()
            .cd('I', 1)
            .cd_opt('s', Some(4u32))
            .cd_opt('v', Some(4u32))
            .push_cd(Shm.to_cd())
            .cd('t', Shm.medium())
            .payload(name_payload.clone())
            .into_escape();

        let text = String::from_utf8(escape).expect("escape is ASCII");
        assert!(text.starts_with("\x1b_G"), "{text:?}");
        assert!(text.ends_with("\x1b\\"), "{text:?}");
        assert!(text.contains(",f=24"), "packed RGB: {text:?}");
        // `t` is the last key before the payload, so it is terminated by `;`.
        assert!(text.contains(",t=s;"), "the shared memory medium: {text:?}");
        assert!(!text.contains(",m="), "a name must never be chunked: {text:?}");
        assert!(
            !text.contains("t=d"),
            "must not fall back to the inline medium: {text:?}"
        );

        // The payload is the name, not the pixels.
        let payload = text
            .split_once(';')
            .and_then(|(_, rest)| rest.strip_suffix("\x1b\\"))
            .expect("payload");
        assert_eq!(
            base64_turbo::STANDARD.decode(payload).unwrap(),
            frame.name().as_bytes()
        );
    }

    /// The saving the medium exists for: at any real frame size the payload is
    /// the object's name, so it does not grow with the image at all.
    #[test]
    fn shm_payload_does_not_scale_with_the_image() {
        // A 1920x1080 RGB frame, the size the medium is worth using at.
        let pixels = vec![0u8; 1920 * 1080 * 3];
        let frame = ShmFrame::create(&pixels).expect("create shm object");
        let payload = frame.payload();

        // The inline medium would carry base64 of every pixel.
        let inline = base64_turbo::STANDARD.encode(&pixels).len();
        assert!(
            payload.len() * 1000 < inline,
            "payload {} should be orders of magnitude below the inline {}",
            payload.len(),
            inline
        );
        // Still exactly the object's name.
        assert_eq!(
            base64_turbo::STANDARD.decode(&payload).unwrap(),
            frame.name().as_bytes()
        );
    }

    /// A raw frame must carry its dimensions: the terminal sizes an `f=24`
    /// frame's payload as `width * height * 3` and rejects a zero-sized one with
    /// "ERROR: EINVAL:Zero width/height not allowed". PNG escaped this because it
    /// describes its own size, so the omission stayed invisible until a raw
    /// medium existed.
    #[test]
    fn a_raw_frame_falls_back_to_the_image_size() {
        let image_size = Some(ImageSize {
            width: 1692,
            height: 1220,
        });

        // Unset, as `..Default::default()` leaves them: the image's own size is
        // used, which is what keeps a raw frame from being zero-sized.
        assert_eq!(resolve_dims(None, None, image_size), (Some(1692), Some(1220)));

        // An explicit size always wins (the tiled renderer passes one).
        assert_eq!(
            resolve_dims(Some(64), Some(32), image_size),
            (Some(64), Some(32))
        );

        // Mixed: each axis falls back independently.
        assert_eq!(
            resolve_dims(Some(64), None, image_size),
            (Some(64), Some(1220))
        );

        // No size anywhere stays None, leaving the terminal to report it.
        assert_eq!(resolve_dims(None, None, None), (None, None));
    }

    /// A self-describing format must declare nothing: the terminal still checks
    /// a declared size against the image, so a stale dimension would be rejected
    /// where omitting it was fine. This is what keeps the PNG path unchanged.
    #[test]
    fn a_self_describing_format_declares_no_dimensions() {
        assert!(
            !Png.needs_dimensions(),
            "PNG carries its own size in its header"
        );
        assert!(Rgb.needs_dimensions(), "raw RGB is sized from s/v");
        assert!(Rgba.needs_dimensions());
        assert!(Shm.needs_dimensions(), "the shm medium carries raw RGB");

        // The rule the call sites apply.
        let declared = |needs: bool, requested: Option<u32>, image: Option<ImageSize>| {
            if needs {
                resolve_dims(requested, requested, image).0
            } else {
                None
            }
        };
        let size = Some(ImageSize {
            width: 1692,
            height: 1220,
        });
        assert_eq!(declared(Png.needs_dimensions(), None, size), None);
        assert_eq!(declared(Shm.needs_dimensions(), None, size), Some(1692));
    }

    /// A frame's object must outlive the send that named it.
    ///
    /// This is the bug that produced "EBADF:Failed to open file for graphics
    /// transmission ... No such file or directory": the request was dropped at
    /// the end of `new_frame`, whose `Drop` unlinked the object before the
    /// terminal could open it. The image now holds it across the send.
    #[test]
    fn a_transmitted_frame_keeps_its_object_alive() {
        let pixels = vec![5u8; 8 * 8 * 3];
        let frame = Arc::new(ShmFrame::create(&pixels).expect("create shm object"));
        let path =
            std::path::Path::new(SHM_DIR).join(frame.name().trim_start_matches('/'));

        let mut image: DynImage<Shm> = DynImage {
            id: DynId::Assigned(Arc::new(1)),
            placements: HashSet::new(),
            frames: HashMap::new(),
            // The base image's own object: held for the image's whole life, so
            // it is deliberately not one of the ones `sync` releases.
            request: ImageRequest {
                format: Shm,
                size: Some(ImageSize {
                    width: 8,
                    height: 8,
                }),
                data: vec![],
                shm: None,
            },
            held: None,
            frame_shm: vec![],
            pl_id_alloc: {
                let mut slab = Slab::new();
                slab.next();
                slab
            },
            pending: vec![],
        };

        // A frame created from a request hands its object to the image rather
        // than letting the request's drop remove it.
        image.frame_shm.push(frame.clone());
        assert_eq!(std::fs::read(&path).expect("readable"), pixels);

        // Dropping the request's own handle must not unlink while the image
        // still holds one: this is the ownership the fix establishes.
        drop(frame);
        assert!(
            std::fs::read(&path).is_ok(),
            "the object must outlive the sending call"
        );

        // With nothing in flight, `sync` resolves at once and releases the
        // objects, so an animation does not accumulate a frame per frame. There
        // is no runtime here, but an empty pending list makes it ready on the
        // first poll, so a noop waker is enough.
        let mut sync = image.sync();
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(&waker);
        assert!(matches!(
            std::pin::Pin::new(&mut sync).poll(&mut cx),
            std::task::Poll::Ready(Ok(()))
        ));
        drop(sync);
        assert!(image.frame_shm.is_empty(), "objects are freed once acked");
        // The newest one is not freed outright: it moves into `held`, which is
        // what lets the image be sent again after the terminal erases it
        // without re-encoding a frame the caller has already moved past.
        assert!(
            image.last_object().is_some(),
            "the newest object is kept for a re-send"
        );
        assert!(
            std::fs::read(&path).is_ok(),
            "the held object must stay on disk"
        );
    }

    /// The inline path is unchanged: direct medium, and still chunked when the
    /// payload is larger than one command can carry.
    #[test]
    fn inline_transmit_keeps_the_direct_medium_and_chunks() {
        let escape = Message::new()
            .cd('I', 1)
            .push_cd(Rgb.to_cd())
            .cd('t', Rgb.medium())
            .payload(vec![b'A'; 10])
            .into_escape();
        let text = String::from_utf8(escape).unwrap();
        assert!(text.contains(",t=d;"), "{text:?}");
        assert!(text.contains(",f=24"), "{text:?}");
    }

    #[test]
    fn consecutive_replies_are_parsed_one_by_one() {
        let mut buf = "\x1b_Gi=31;\x1b\\\x1b_Gq=1;OK\x1b\\".bytes().peekable();

        let first = Message::parse_next(&mut buf).unwrap();
        assert_eq!(cds(&first), [('i', "31")]);

        let second = Message::parse_next(&mut buf).unwrap();
        assert_eq!(cds(&second), [('q', "1")]);
        assert_eq!(second.payload.as_deref(), Some("OK".as_bytes()));

        assert!(Message::parse_next(&mut buf).is_none());
    }

    #[test]
    fn value_may_contain_equals_sign() {
        let r = parse("\x1b_Ga=b=c;\x1b\\").unwrap();
        assert_eq!(cds(&r), [('a', "b=c")]);
    }

    #[test]
    fn truncated_replies_return_none() {
        for s in ["\x1b_G", "\x1b_Gi", "\x1b_Gi=3", "\x1b_Gi=31;AA"] {
            assert!(parse(s).is_none(), "expected None for {s:?}");
        }
    }

    /// Parse `input`, and report both what came back and what was forwarded.
    ///
    /// The two together, because the whole point of the forwarding path is that a
    /// reply is *removed* from the stream and everything else is *kept*: asserting
    /// only one of the two would pass on an implementation that dropped input or
    /// left a reply in it.
    fn parse_forwarding(input: &str) -> (Vec<Message>, String) {
        let mut buf = input.bytes().peekable();
        let mut forwarded = Vec::new();
        let mut messages = Vec::new();
        while let Some(message) = Message::parse_next_with(&mut buf, &mut forwarded) {
            messages.push(message);
        }
        (messages, String::from_utf8(forwarded).expect("forwarded input is text"))
    }

    /// Input with no reply in it is passed through, byte for byte.
    #[test]
    fn plain_input_is_forwarded_unchanged() {
        let (messages, forwarded) = parse_forwarding("hello");
        assert!(messages.is_empty(), "there was no reply to parse");
        assert_eq!(forwarded, "hello");
    }

    /// A reply is consumed and the input around it is forwarded, in order.
    #[test]
    fn a_reply_is_removed_and_the_input_around_it_survives() {
        let (messages, forwarded) = parse_forwarding("ls\x1b_Gi=31;\x1b\\-la");
        assert_eq!(messages.len(), 1);
        assert_eq!(cds(&messages[0]), [('i', "31")]);
        // The command is not run, but as far as the parse is concerned the bytes
        // before the reply and after it are one continuous stream of input.
        assert_eq!(forwarded, "ls-la");
    }

    /// Several replies, with input between them, each removed once.
    #[test]
    fn each_reply_is_removed_from_a_mixed_stream() {
        let (messages, forwarded) =
            parse_forwarding("a\x1b_Gi=1;OK\x1b\\b\x1b_Gi=2;OK\x1b\\c");
        assert_eq!(messages.len(), 2);
        assert_eq!(cds(&messages[0]), [('i', "1")]);
        assert_eq!(cds(&messages[1]), [('i', "2")]);
        assert_eq!(forwarded, "abc");
    }

    /// An `ESC` that begins a key rather than a reply is input, not a reply.
    ///
    /// This is the case that makes the naive version wrong: an arrow key also
    /// starts with `ESC`, and treating it as a truncated reply would eat it.
    #[test]
    fn an_escape_that_is_not_a_reply_is_forwarded() {
        let (messages, forwarded) = parse_forwarding("\x1b[A");
        assert!(messages.is_empty(), "an arrow key is not a reply");
        assert_eq!(forwarded, "\x1b[A");
    }

    /// A truncated reply is not forwarded as input either.
    ///
    /// Half a reply has nowhere to go: it is not a message the router can use, and
    /// handing it to the command would type the terminal's own answer into it.
    #[test]
    fn a_truncated_reply_is_neither_parsed_nor_forwarded() {
        let (messages, forwarded) = parse_forwarding("\x1b_Gi=31;AA");
        assert!(messages.is_empty());
        assert_eq!(forwarded, "");
    }

    /// The ordinary entry point behaves exactly like the forwarding one with a sink.
    #[test]
    fn parsing_without_forwarding_matches_the_forwarding_path() {
        let input = "key\x1b_Gi=7;OK\x1b\\more";
        let (with, _) = parse_forwarding(input);
        let plain = {
            let mut buf = input.bytes().peekable();
            let mut messages = Vec::new();
            while let Some(message) = Message::parse_next(&mut buf) {
                messages.push(message);
            }
            messages
        };
        assert_eq!(with.len(), plain.len());
        assert_eq!(cds(&with[0]), cds(&plain[0]));
    }

    /// A controller routes the replies out of a stream and forwards the rest.
    ///
    /// Synthetic input rather than a terminal: the point is the splitting, and a
    /// real terminal would only add the latencies that make such a test flaky. The
    /// keystrokes must arrive on the forward pipe *while* the router runs, which is
    /// the property that lets both halves live in one process.
    #[test]
    fn a_controller_routes_replies_and_forwards_the_input_between_them() {
        /// Collects everything written to it, so the test can look at it after.
        struct Collect(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Collect {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        // One keystroke, one reply, another keystroke. The reply is the ack for the
        // liveness probe this library sends when a transfer is awaited, so it is
        // also what the router is looking for.
        let forwarded = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let input = b"ab\x1b_Gi=100001;OK\x1b\\cd".to_vec();
        let _controller = Controller::init_with_forwarding(
            std::io::Cursor::new(input),
            Collect(forwarded.clone()),
        );

        // The router runs on its own thread, so the input arrives asynchronously.
        // A short wait is enough: the whole stream is in memory and needs no IO.
        for _ in 0..100 {
            if forwarded.lock().unwrap().len() >= 4 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        // Both keystrokes survive, in order, with the reply removed from between
        // them. If the reply were forwarded the command would be typed the
        // terminal's own answer, and if the intro were treated as input the two
        // `ESC _ G` bytes would show up here too.
        let got = forwarded.lock().unwrap().clone();
        assert_eq!(String::from_utf8(got).expect("input is text"), "abcd");
    }

    /// The escape destination cannot be moved once it is set.
    ///
    /// A second destination would split one image's escapes across two writers, so
    /// the installer refuses. That the *installed* one receives an escape is not
    /// asserted here: the destination is process-global and the first test to run
    /// wins it, which makes routing order-dependent in a test binary. It is checked
    /// where it matters instead, by a real session showing the escapes on the
    /// terminal.
    #[test]
    fn the_escape_destination_cannot_be_moved() {
        let _ = writer::set_sink(std::sync::Arc::new(std::sync::Mutex::new(
            std::io::sink(),
        )));
        let second = writer::set_sink(std::sync::Arc::new(std::sync::Mutex::new(
            std::io::sink(),
        )));
        assert!(!second, "a second destination must be refused");
    }
}
