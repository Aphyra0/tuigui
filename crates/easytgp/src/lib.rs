//! Terminal graphics protocol (TGP): the wire format used to stream
//! pixel data into a graphics-capable terminal.

use std::{
    collections::{HashMap, HashSet},
    fmt::Display,
    io::Write,
    ops::Deref,
    sync::{Arc, RwLock, RwLockReadGuard},
};

use derive_deref::{Deref, DerefMut};
use derive_getters::Getters;
use itertools::Itertools;

// TODO: https://sw.kovidgoyal.net/kitty/graphics-protocol/#unicode-placeholders
// TODO: https://sw.kovidgoyal.net/kitty/graphics-protocol/#relative-placements
// TODO: https://sw.kovidgoyal.net/kitty/graphics-protocol/#usage-hints
// TODO: Compression

mod id {
    use crate::Id;
    use idalloc::Slab;
    use static_init::dynamic;

    /// Locked lazy global slab. Access via `IDAl.write()` / `IDAl.read()`.
    #[dynamic]
    static mut IDAL: Slab<Id> = {
        // read current config
        // let mut t = rustix::termios::tcgetattr(std::io::stdin()).unwrap(); // any tty fd works: 0=stdin

        // // disable echo only (leave everything else)
        // t.local_modes.remove(rustix::termios::LocalModes::ECHO);
        // t.local_modes.remove(rustix::termios::LocalModes::ICANON);

        // // COMMIT — write it back to the fd (this is the missing line)
        // rustix::termios::tcsetattr(std::io::stdin(), rustix::termios::OptionalActions::Now, &t)
        //     .unwrap();

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
        IDAL.write().next()
    }

    /// Reclaim an id for reuse.
    pub(crate) fn free(id: Id) {
        IDAL.write().free(id);
    }
}

pub trait Restorable: std::fmt::Debug {
    fn restore(&mut self, renew_dependents: bool) -> Res<()>;
    // fn renew_one();
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

/// Optional store for the images. Can be used as a central registry for all images
pub struct ImageStore {
    images: HashMap<DynImageHandle, DynImage>,
}

impl ImageStore {
    pub fn store(&mut self, image: DynImage) -> DynImageHandle {
        let h = DynImageHandle {
            id: image.id.clone(),
        };
        self.images.insert(h.clone(), image);
        h
    }

    pub fn get(&mut self, handle: &DynImageHandle) -> Option<&mut DynImage> {
        self.images.get_mut(handle)
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
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
pub struct ImageRequest {
    format: Format,
    data: Vec<u8>,
}

// pub enum ImageRequest {
//     Png {
//         image: Ima

//     },

//     Rgb {

//     },

//     Rgba {

//     }
// }

impl ImageRequest {
    pub fn new(format: Format, data: Vec<u8>) -> Self {
        ImageRequest { format, data }
    }

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

    pub fn transmit(self, img_size: Option<(u32, u32)>) -> DynImage {
        let id = DynId::new(id::alloc());
        self.exec_transmit(id.assigned(), img_size);

        DynImage {
            id,
            request: self,
            placements: HashSet::new(),
            frames: HashMap::new(),
        }
    }

    fn exec_transmit(&self, id: Id, img_size: Option<(u32, u32)>) {
        // TODO: Store data already encoded.
        let payload = base64_turbo::STANDARD.encode(self.data.as_slice());
        let chunks = payload.bytes().chunks(4096);
        let mut iter = chunks.into_iter();
        let (mut first, mut cur_o) = (true, iter.next());

        // TODO: do not include control data but in the first chunk.
        while let Some(cur) = cur_o.take() {
            let payload = cur.collect_vec();
            (Action::new()
                .cd("i", id)
                .cd_opt(
                    "m",
                    if let Some(next) = iter.next() {
                        cur_o = Some(next);
                        Some(1)
                    } else if !first {
                        Some(0)
                    } else {
                        None
                    },
                )
                .cd_opt("s", img_size.map(|(s, _v)| s))
                .cd_opt("v", img_size.map(|(_s, v)| v))
                .push_cd(self.format.into())
                .payload(String::from_utf8(payload).unwrap()))
            .commit();
            first = false;
        }
    }
}

#[derive(Debug)]
pub struct DynImage {
    id: DynId,
    // Remember what placements are attached to this image.
    placements: HashSet<PlacementHandle>,
    frames: HashMap<FrameHandle, FrameIdx>,
    request: ImageRequest,
}

impl DynImage {
    pub fn place(&mut self, request: PlacementRequest) -> Res<PlacementHandle> {
        let h = request.place(self.id.assigned(), id::alloc(), "p");
        self.placements.insert(h.clone());
        Ok(h)
    }

    pub fn placements(&self) -> Vec<PlacementHandle> {
        for pl in &self.placements {}
        todo!()
    }

    // TODO:
    // pub fn replace(&mut self, handle: PlacementHandle) {}

    pub fn displace(&mut self, handle: PlacementHandle) -> Res<PlacementRequest> {
        if self.placements.remove(&handle) {
            Ok(handle.request.deref().clone())
        } else {
            todo!()
        }
    }

    pub fn restore(&mut self) {}

    pub fn delete(self) -> ImageRequest {
        Action::new()
            .cd("a", "d")
            .cd("i", self.id.assigned())
            .commit();
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

    pub fn new_frame(&mut self, config: NewFrameRequest) -> Res<FrameHandle> {
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
        edit: FrameEdit<ComposeFromSelector, Upload>,
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
            }) => Action::new()
                // Use frame composing api
                .cd("a", "c")
                // image context
                .cd("i", self.id.assigned())
                // source frame
                .cd("r", self.resolve_frame_selector(from_frame))
                // destination frame
                .cd("c", frame_idx)
                // size
                .cd("w", width)
                .cd("h", height)
                // source location
                .cd("X", src.0)
                .cd("Y", src.1)
                // destination location
                .cd("x", dst.0)
                .cd("y", dst.1),

            FrameEdit::Upload(Upload {
                x,
                y,
                width,
                height,
                image,
            }) => Action::new()
                // use standard frame action api
                .cd("a", "f")
                .cd("i", self.id.assigned())
                // "r" instead of "c" so it edits the frame instead of creating the new one
                .cd("r", frame_idx)
                .cd_opt("s", width)
                .cd_opt("v", height)
                .cd_opt("x", x)
                .cd_opt("y", y)
                .push_cd(image.format.into())
                .payload(base64_turbo::STANDARD.encode(image.data.as_slice())),
        }
        .commit();

        Ok(())
    }

    pub fn display_last_frame(&self) {
        self.display_frame(FrameSelector::Last);
    }

    // TODO: Use selector, rename to just display
    pub fn display_frame(&self, frame: FrameSelector) {
        Action::new()
            .cd("a", "a")
            .cd("i", self.id.assigned())
            .cd("c", self.resolve_frame_selector(frame))
            .commit();
    }
}

/// Simple image, can't be directly deleted, can't be animated. only supports restore.
#[derive(Clone, Debug)]
pub struct AnonymousImage {
    data: ImageRequest,
    place: PlacementRequest,
}

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
pub struct NewFrameRequest {
    pub from_frame: FrameSelector,
    pub image: ImageRequest,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub x: Option<u32>,
    pub y: Option<u32>,
}

impl NewFrameRequest {
    fn create(
        self,
        // image where the change is happning
        base_image: Id,

        // compose from the frame at the given index
        from_frame_idx: FrameIdx,

        // frame index that is given to the new frame
        new_frame_idx: FrameIdx,
    ) -> Res<FrameHandle> {
        Action::new()
            .cd("a", "f")
            .cd("i", base_image)
            .cd("c", from_frame_idx)
            .cd_opt("s", self.width)
            .cd_opt("v", self.height)
            .cd_opt("x", self.x)
            .cd_opt("y", self.y)
            .push_cd(self.image.format.into())
            .payload(base64_turbo::STANDARD.encode(self.image.data.as_slice()))
            .commit();

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

pub struct Upload {
    x: Option<u32>,
    y: Option<u32>,
    width: Option<u32>,
    height: Option<u32>,
    image: ImageRequest,
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
        Action::new() //
            .cd("a", action)
            .cd("i", image_id)
            .cd("p", id)
            .cd_opt("X", self.offset_x)
            .cd_opt("Y", self.offset_y)
            .cd_opt("c", self.cols)
            .cd_opt("r", self.rows)
            .cd_opt("x", self.x)
            .cd_opt("y", self.y)
            .cd_opt("z", self.z)
            .cd_opt("w", self.width)
            .cd_opt("h", self.height)
            .cd_if(self.fix_cursor, "C", 1)
            .commit();
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

#[derive(Debug)]
pub struct Action {
    cds: Vec<ControlData>,
    payload: Option<String>,
}

impl Action {
    // TODO: make this take initial action keys
    pub fn new() -> Action {
        Action {
            cds: vec![],
            payload: None,
        }
    }

    pub fn payload(mut self, payload: String) -> Self {
        self.payload.replace(payload);
        self
    }

    /// Takes ownership
    fn cd(self, key: &'static str, value: impl Display) -> Self {
        self.push_cd(ControlData(key, value.to_string()))
    }

    fn cd_opt(self, key: &'static str, value_o: Option<impl Display>) -> Self {
        if let Some(value) = value_o {
            self.push_cd(ControlData(key, value.to_string()))
        } else {
            self
        }
    }

    fn cd_if(self, condition: bool, key: &'static str, value: impl Display) -> Self {
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

    /// execute the command
    // pub fn commit(self) -> Result<(), TGPError> {
    pub fn commit(mut self) {
        // TODO: Handle properly
        self = self.cd("q", 2);

        let cds = self
            .cds
            .iter()
            .map(|cdv| cdv.format())
            .collect_vec()
            .join(",");

        let payload = self.payload.unwrap_or_default();

        print!("\x1b_G{cds};{payload}\x1b\\");
        std::io::stdout().flush().unwrap();
        // TODO: Handle errors propertly.
    }
}

#[derive(thiserror::Error, Debug)]
pub enum TGPError {
    #[error("Entity was evicted")]
    Evicted(Box<dyn Restorable>),
    #[error("Entity you are trying to use was indirectly or directly deleted")]
    Deleted(Box<dyn Restorable>),
    #[error("unknown error")]
    Unknown,
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

#[derive(Clone, Copy, Debug, Default)]
pub enum Format {
    #[default]
    RGB,
    RGBA,
    PNG,
}

impl From<Format> for ControlData {
    fn from(value: Format) -> Self {
        ControlData(
            "f",
            match value {
                Format::RGB => 24,
                Format::RGBA => 32,
                Format::PNG => 100,
            }
            .to_string(),
        )
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
#[derive(Debug)]
struct ControlData(&'static str, String);

impl ControlData {
    fn format(&self) -> String {
        format!("{}={}", self.0, self.1)
    }
}
