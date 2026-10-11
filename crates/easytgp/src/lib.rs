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
use image::{PixelWithColorType, RgbImage};
use itertools::Itertools;

// TODO: https://sw.kovidgoyal.net/kitty/graphics-protocol/#unicode-placeholders
// TODO: https://sw.kovidgoyal.net/kitty/graphics-protocol/#relative-placements
// TODO: https://sw.kovidgoyal.net/kitty/graphics-protocol/#usage-hints
// TODO: Compression
// TODO: User-implemented full-recovery crate
// TODO: Notifications of the bottleneck.

mod id {
    use crate::Id;
    use idalloc::Slab;
    use static_init::dynamic;

    /// Locked lazy global slab. Access via `IDAl.write()` / `IDAl.read()`.
    #[dynamic]
    static mut I: Slab<Id> = {
        // read current config
        let mut t = rustix::termios::tcgetattr(std::io::stdin()).unwrap(); // any tty fd works: 0=stdin

        // // disable echo only (leave everything else)
        // t.local_modes.remove(rustix::termios::LocalModes::ECHO);
        t.local_modes.remove(rustix::termios::LocalModes::ICANON);

        // // COMMIT — write it back to the fd (this is the missing line)
        rustix::termios::tcsetattr(std::io::stdin(), rustix::termios::OptionalActions::Now, &t)
            .unwrap();

        // option 2 — also clear scrollback (kitty/xterm support this)

        // println!("\x1b[2J\x1b[H");

        use std::io::Write;
        print!("\x1b[2J\x1b[3J\x1b[H");
        std::io::stdout().flush().unwrap();

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

pub trait Restorable: std::fmt::Debug {
    fn restore(&mut self, renew_dependents: bool) -> Res<()>;
    // fn renew_one();
}

#[static_init::dynamic]
static mut R: ReplyRouter = ReplyRouter {
    last_img: 0,
    pending: HashMap::new(),
    ready: HashMap::new(),
    last_marker: 1,
};

type Marker = u16;
struct ReplyRouter {
    last_img: Id,
    pending: HashMap<Marker, Waker>,
    ready: HashMap<Marker, Option<Message>>,
    last_marker: Marker,
}

impl ReplyRouter {
    fn next_marker(&mut self) -> Marker {
        // TODO: track what if the next marker is anywhere in pending or ready.
        self.last_marker = self.last_marker.wrapping_add(1).clamp(1, Marker::MAX);
        self.last_marker
    }

    fn start(stdin: impl std::io::Read + Send + 'static) -> JoinHandle<()> {
        std::thread::spawn(move || {
            let mut buf = BufReader::new(stdin)
                .bytes()
                .map_while(Result::ok)
                .peekable();

            let mut last = None;
            while let Some(reply) = Message::parse_next(&mut buf) {
                if let Some(marker) = reply.get_from_key('i') {
                    // let ActionIdent::ImageId { id: marker } = reply.ident() else {
                    //     unreachable!()
                    // };

                    let mut r = R.write();
                    let marker = marker.parse().unwrap();
                    if let Some(waker) = r.pending.remove(&marker) {
                        r.ready.insert(marker, last.take());
                        waker.wake();
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

pub struct PendingOperation<R> {
    reply: ReplyFuture,
    returns: Option<Box<dyn FnOnce(Reply) -> R>>,
}

impl<R> PendingOperation<R> {
    fn new<C: FnOnce(Reply) -> R + 'static>(reply: ReplyFuture, returns: C) -> Self {
        Self {
            reply,
            returns: Some(Box::new(returns)),
        }
    }
}

pub struct ReplyFuture {
    ident: ActionIdent,
    action_number: Marker,
}

enum ActionIdent {
    ImageNumber { number: usize },
    ImageId { id: Id },
}

impl ReplyFuture {
    fn new(ident: ActionIdent) -> Self {
        let mut r = R.write();
        let marker = r.next_marker();

        // Send marker as well, marking everything received before that to this action.
        Message::new()
            .cd('a', 'q')
            .cd('i', marker)
            .cd('f', 24)
            .cd('s', 1)
            .cd('v', 1)
            .payload("AAAA")
            .send_raw();
        ReplyFuture {
            ident,
            action_number: marker,
        }
    }
}

impl<R: Unpin> Future for PendingOperation<R> {
    type Output = Res<R>;

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
        } else if !r.pending.contains_key(&self.action_number) {
            r.pending.insert(self.action_number, cx.waker().clone());
        }

        std::task::Poll::Pending
    }
}

pub struct Controller {
    router_handle: JoinHandle<()>,
}

impl Controller {
    pub fn init(stdin: impl std::io::Read + Send + 'static) -> Arc<Controller> {
        Arc::new(Controller {
            router_handle: ReplyRouter::start(stdin),
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
        }
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
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ImageSize {
    pub width: u32,
    pub height: u32,
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

trait Format: Copy + Clone + Default {
    fn to_cd(self) -> ControlData;
}

#[derive(Clone, Copy, Default)]
pub struct Png;
impl Format for Png {
    fn to_cd(self) -> ControlData {
        ControlData('f', 100.to_string())
    }
}

#[derive(Clone, Copy, Default)]
pub struct Rgb;
impl Format for Rgb {
    fn to_cd(self) -> ControlData {
        ControlData('f', 24.to_string())
    }
}

#[derive(Clone, Copy, Default)]
pub struct Rgba;
impl Format for Rgba {
    fn to_cd(self) -> ControlData {
        ControlData('f', 32.to_string())
    }
}

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

    pub fn transmit(self) -> PendingOperation<DynImage<F>> {
        let id = DynId::new(id::alloc());

        PendingOperation::new(self.exec_transmit(id.assigned()), |r| {
            let id: u32 = r
                .message
                .expect("reply with allocated i")
                .get_from_key('i')
                .unwrap()
                .parse()
                .unwrap();

            DynImage {
                id: DynId::Assigned(Arc::new(id)),
                request: self,
                placements: HashSet::new(),
                frames: HashMap::new(),
            }
        })
    }

    fn exec_transmit(&self, id: Id) -> ReplyFuture {
        Message::new()
            .cd('I', id)
            .cd_opt('s', self.size.map(|s| s.width))
            .cd_opt('v', self.size.map(|s| s.height))
            .push_cd(self.format.to_cd())
            // TODO: I don't like clone here
            .payload(self.data.clone())
            .send()
    }
}

#[derive(Debug)]
pub struct DynImage<F: Format> {
    id: DynId,
    // Remember what placements are attached to this image.
    placements: HashSet<PlacementHandle>,
    frames: HashMap<FrameHandle, FrameIdx>,
    request: ImageRequest<F>,
}

impl<F: Format> DynImage<F> {
    pub fn place(&mut self, request: PlacementRequest) -> Res<PlacementHandle> {
        let h = request.place(self.id.assigned(), id::alloc(), "p");
        self.placements.insert(PlacementHandle {
            id: h.id.clone(),
            request: h.request.clone(),
        });

        Ok(h)
    }

    // TODO:
    pub fn placements(&self) -> Vec<PlacementHandle> {
        for pl in &self.placements {}
        todo!()
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

    pub fn delete(self) -> ImageRequest<F> {
        Message::new()
            .cd('a', "d")
            .cd('i', self.id.assigned())
            .send();
        self.request
    }

    fn resolve_frame_selector(&self, selector: FrameSelector) -> usize {
        use FrameSelector::*;
        match selector {
            Last => self.frames.len() + 1,
            First => 1,
            ID(id) => id,
        }
    }

    pub fn new_frame(&mut self, config: NewFrameRequest<F>) -> Res<FrameHandle> {
        let base_frame = config.from_frame;
        let idx = self.frames.len() + 2;
        let handle = config.create(
            self.id.assigned(),
            self.resolve_frame_selector(base_frame),
            idx,
        )?;

        self.frames.insert(handle.clone(), idx);
        Ok(handle)
    }

    pub fn edit_frame(
        &self,
        handle: &FrameHandle,
        edit: FrameEdit<ComposeFromSelector, Upload<F>>,
    ) -> Res<()> {
        let Some(frame_idx) = self.frames.get(handle) else {
            todo!()
        };

        match edit {
            FrameEdit::Compose(ComposeFromSelector {
                from_frame,
                width,
                height,
                src,
                dst,
            }) => Message::new()
                // Use frame composing api
                .cd('a', "c")
                // image context
                .cd('i', self.id.assigned())
                // source frame
                .cd('r', self.resolve_frame_selector(from_frame))
                // destination frame
                .cd('c', frame_idx)
                // size
                .cd('w', width)
                .cd('h', height)
                // source location
                .cd('X', src.0)
                .cd('Y', src.1)
                // destination location
                .cd('x', dst.0)
                .cd('y', dst.1),

            FrameEdit::Upload(Upload {
                x,
                y,
                width,
                height,
                image,
            }) => Message::new()
                // use standard frame action api
                .cd('a', "f")
                .cd('i', self.id.assigned())
                // "r" instead of "c" so it edits the frame instead of creating the new one
                .cd('r', frame_idx)
                .cd_opt('s', width)
                .cd_opt('v', height)
                .cd_opt('x', x)
                .cd_opt('y', y)
                .push_cd(image.format.to_cd())
                .payload(base64_turbo::STANDARD.encode(image.data.as_slice())),
        }
        .send();

        Ok(())
    }

    pub fn display_last_frame(&self) {
        self.display_frame(FrameSelector::Last);
    }

    // TODO: Use selector, rename to just display
    pub fn display_frame(&self, frame: FrameSelector) {
        Message::new()
            .cd('a', "a")
            .cd('i', self.id.assigned())
            .cd('c', self.resolve_frame_selector(frame))
            .send();
    }
}

// /// Simple image, can't be directly deleted, can't be animated. only supports restore.
// #[derive(Clone, Debug)]
// pub struct AnonymousImage {
//     data: ImageRequest,
//     place: PlacementRequest,
// }

// /// Animation based on a single image
// /// and with the single frame
// ///
// /// all changes are placed on the overlay.
// ///
// /// Updates are not atomics, but it doesn't create new frames.
// pub struct ThinStreamer {
//     img: ,
// }

// impl ThinStreamer {
//     pub fn new(base: ImageRequest) -> Self {
//         todo!()
//         // ThinStreamer {
//         //     animation: base.transmit().into(),
//         // }
//     }

//     pub fn update(&mut self, edit: FrameEdit<ComposeFromPrevious, Upload>) {
//         self.animation
//             .frame(FrameSelector::Previous)
//             .unwrap()
//             .edit(edit.into());
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
    ) -> Res<FrameHandle> {
        Message::new()
            .cd('a', "f")
            .cd('i', base_image)
            .cd('c', from_frame_idx)
            .cd_opt('s', self.width)
            .cd_opt('v', self.height)
            .cd_opt('x', self.x)
            .cd_opt('y', self.y)
            .push_cd(self.image.format.to_cd())
            .payload(self.image.data)
            .send();

        Ok(FrameHandle {
            base_image,
            index: new_frame_idx,
        })
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
    x: Option<u32>,
    y: Option<u32>,
    width: Option<u32>,
    height: Option<u32>,
    image: ImageRequest<F>,
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
    // TODO:
    // H, V
}

impl PlacementRequest {
    pub fn place(self, image_id: Id, id: Id, action: &'static str) -> PlacementHandle {
        self.exec_place(image_id, id, action);

        PlacementHandle {
            id: DynId::new(id),
            request: Arc::new(self),
        }
    }

    pub fn exec_place(&self, image_id: Id, id: Id, action: &'static str) {
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
            .send();
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
        const ESC: u8 = b'\x1b';
        while let Some(b) = buf.next() {
            // Only start parsing with the correct initial sequence
            if b == ESC && buf.next()? == b'_' && buf.next()? == b'G' {
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
        const CHUNK_SIZE: usize = 4 * 1024;
        const LOCK: Mutex<()> = Mutex::new(());

        // TODO: might be missed because of mismatching spaces
        let is_frame_edit = self.cds.contains(&ControlData('a', "f".to_string()));

        let payload = self.payload.take().unwrap_or_default();
        let mut chunks = payload.chunks(CHUNK_SIZE).into_iter().peekable();
        let aident = self.ident();

        // Critical section goes here
        {
            let lock = LOCK;
            let l = lock.lock().unwrap();

            self.payload(chunks.next().unwrap_or_default())
                .cd_if(chunks.peek().is_some(), 'm', 1)
                .send_raw();

            while let Some(chunk) = chunks.next() {
                Message::new()
                    .cd('m', if chunks.peek().is_some() { 1 } else { 0 })
                    .cd_if(is_frame_edit, 'a', "f")
                    .payload(chunk.to_vec())
                    .send_raw();
            }

            drop(l)
        }

        // TODO: maybe do in the critical section.
        ReplyFuture::new(aident)
    }

    fn send_raw(mut self) {
        // TODO: Handle properly
        // self = self.cd('q', 2);

        let cds = self
            .cds
            .iter()
            .map(|cdv| cdv.format())
            .collect_vec()
            .join(",");

        let payload = String::from_utf8(self.payload.unwrap()).unwrap();
        print!("\x1b_G{cds};{payload}\x1b\\");
        std::io::stdout().flush().unwrap();
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
}
