//! Terminal graphics protocol (TGP): the wire format used to stream
//! pixel data into a graphics-capable terminal.

use std::{
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

impl Restorable for DynImageHandle {
    fn restore(&mut self, renew_dependents: bool) -> Res<()> {
        let mut img = self.read().unwrap().deref().clone();

        // Renew image but don't replace this one.
        // TODO: Should re-use the id;
        // TODO: Should not call drop that would destroy the image.
        img.request.exec_transmit(img.id);

        for placement in img.placements.iter_mut() {
            placement.restore(false)?;
        }

        todo!()
        // controller.img_transmit(self.request)
    }
}

impl Restorable for PlacementHandle {
    fn restore(&mut self, renew_dependents: bool) -> Res<()> {
        if renew_dependents {
            let mut p = self.write().unwrap();

            if let Some(p) = &mut *p {
                p.image.restore(false)?;
            } else {
                // TODO
                return Ok(());
            }
        } else {
            let Some(l) = self.read().unwrap().clone() else {
                return Ok(());
            };

            l.config.exec_place(l.id, "p");
        }
        Ok(())
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
pub type DynImageHandle = Handle<DynImage>;
pub type FrameHandle = Handle<Frame>;
pub type PlacementHandle = Handle<Option<Placement>>;
pub type FrameIdx = usize;

#[derive(Clone, Deref, DerefMut, Debug)]
pub struct Handle<I>(Arc<RwLock<I>>);

// pub enum Handle<I> {
//     MaybeValid(Arc<RwLock<I>>),

//     // It's invalid but you can still restore it
//     Invalid(Arc<I>),
// }

// 1. User:
// I have an DynImage, I have a placement for it.
// 1. I delete img, placements I have are invalid and I know about it, but I would rather use .maybe_swallow instead of deleting?
// 2. Other program is being deleted and it's placements
//
// So, we are trying to decide if we need a handle to be another gate that displays if the item is invalid or not.
//
// Errors will give user a destinct idea of what handles are dead.
// User doesn't really know what handles are valid unless polled.
//
// But user been warned about the contract which they might have missed or modelled incorrectly.
// Without the invalidation checks, the user is user is forced to handle this blindly.
// Which means they need to find out all invalidated handles according to the contract and then drop them.
//
// If they don't, the wrong operation on invalid handle will return an error.
//
// It also can get unobvious with relative placements that I might add later.
//
// Btw, Placement handles aren't necesserely invalidated on image deletion?
// You can still just replace_with the different image.
//
// Frames are specific to the images and I don't see the need for them to be reused anywhere.
// Placement has the position, sizing and scale, while frames don't. They have an attached image and the index, this is it
// everything else is the data which would get invalidated anyway on the image drop. So no need for frames.
//
// So, when we delete an image, we say that all placements aren't invalidated but rather are detached and user can reattach them.
//
// User can detach the placement from the image directly on the placement, in this case image will not be deleted, only placement.
//
// Problem: User can clone the handle to either a placement or the image, on manual deletion of the image,
// the other handle should also be invalidated.
//
// Solutions:
// 1. Don't allow user clonning handle
// in that case it wouldn't be a handle, it would be an object instead.
//
// Let's think when user needs multiple handles to the item.
//
// 2. Think about user handling the invalidated handle
// the user is presented with the contract that if one image is being deleted, all other handles become invalid automatically
//
//
// So the solution:
// 1. handles aren't clonnable
// if you drop the placement it's stashed inside the dynimage, which means you can aquire it,
// you can in fact aquire and drop placements as many times as you'd like, it just doesn't allow multiple user-held references to it.
//
// If image deleted all stashed placements are also automatically deleted and ids are freed.
//
// 2. The deletion model is reduced. You can delete through the handles, but you can no longer query-delete.
//
// 3. Placements handles now resolve to an enum of Attached and detached placements, with different operations on each side.
//
// User can delete an image making the placement handle they hold detached, so the next time they use it they would see it's detached and it needs different logic.
//
// 4. As frames are mere index and the base image, we drop the frame handle, and instead add clonable FrameIdx,
// it can be used to call frame_edit on image handle with the given FrameIdx.
//
// This makes the model safe, user is present with the image handle which it can always assume is valid unless is evicted
// or deleted by the other program, but it should be impossible for the user to shoot in the
// leg by deleting an entity but still having the handle to it. In other words user-space bugs should be comptime checkable which this approach is exactly doing.
//
// But by the bug I consider when the user deletes something but still holds a handle that they can try to use and then maybe recover from the previous deletion.
//
// Q: Why not use ImageId, PlacementId and a global Session?
// A: It changes the design drastically, for example it would use s.get(img_id) -> DynImage
//
// There is also the third way:
//
// Let user own DynImage, let DynImage own placements and give stable ids for all placements. Then
// img.read(PlacementHandle) -> &Placement;
// img.read(FrameHandle) -> &Frame;
// img.displace(PlacementHandle) -> PlacementRequest;
// img.place(PlacementRequest);
//
// But also, can a single placement have multiple images? Per spec no, but what if we don't really care?
//
// It's a bit complicated, let's just assume placement is one
//
//

impl<I> Handle<I> {
    pub fn new(entity: I) -> Self {
        Handle(Arc::new(RwLock::new(entity)))
    }
}

#[derive(Clone, Debug)]
pub struct ImageRequest {
    format: Format,
    data: Vec<u8>,
}

impl ImageRequest {
    pub fn new(format: Format, data: Vec<u8>) -> Self {
        ImageRequest { format, data }
    }

    pub fn transplace(self, placement: PlacementRequest) -> DynImageHandle {
        let payload = base64_turbo::STANDARD.encode(self.data.as_slice());
        let a = Action::new()
            .cd("a", "T")
            .push_cd(self.format.into())
            .payload(payload);

        placement.add_cds(a).commit();
        // AnonymousImage { request: self }
        todo!()
    }

    pub fn transmit(self) -> Handle<DynImage> {
        let id = id::alloc();
        self.exec_transmit(id);

        Handle::new(DynImage {
            id,
            request: self,
            placements: vec![],
            frames: vec![],
        })
    }

    pub fn exec_transmit(&self, id: Id) {
        // TODO: Store data already encoded.
        let payload = base64_turbo::STANDARD.encode(self.data.as_slice());
        Action::new()
            .cd("i", id)
            .push_cd(self.format.into())
            .payload(payload)
            .commit();
    }
}

#[derive(Clone, Debug, Getters)]
pub struct DynImage {
    id: Id,
    placements: Vec<PlacementHandle>,
    frames: Vec<FrameHandle>,
    request: ImageRequest,
}

/// Simple image, can't be directly deleted, can't be animated. only supports restore.
#[derive(Clone, Debug)]
pub struct AnonymousImage {
    data: ImageRequest,
    place: PlacementRequest,
}

impl Restorable for AnonymousImage {
    fn restore(&mut self, renew_dependents: bool) -> Res<()> {
        self.data.clone().transplace(self.place.clone());
        Ok(())
    }
}

impl DynImageHandle {
    pub fn id(&self) -> Id {
        self.read().unwrap().id
    }

    pub fn place_named(&mut self, config: PlacementRequest) -> PlacementHandle {
        let h = config.place(id::alloc(), "p");
        self.write().unwrap().placements.push(h.clone());
        h
    }

    pub fn place(&mut self, request: PlacementRequest) {
        todo!()
    }

    pub fn delete(self) {
        todo!()
    }

    fn resolve_frame_selector(&self, selector: FrameSelector) -> usize {
        use FrameSelector::*;
        match selector {
            Last => self.read().unwrap().frames().len() + 1,
            First => 1,
            ID(id) => id,
        }
    }

    pub fn new_frame(&mut self, config: NewFrameRequest) -> Res<FrameHandle> {
        let base_frame = config.from_frame;
        let handle = config.create(
            self.clone(),
            self.resolve_frame_selector(base_frame),
            self.read().unwrap().frames.len() + 2,
        )?;

        self.write().unwrap().frames.push(handle.clone());
        Ok(handle)
    }

    pub fn display_last_frame(&self) {
        self.display_frame(FrameSelector::Last);
    }

    // TODO: Use selector, rename to just display
    pub fn display_frame(&self, frame: FrameSelector) {
        Action::new()
            .cd("a", "a")
            .cd("i", self.id())
            .cd("c", self.resolve_frame_selector(frame))
            .commit();
    }

    // TODO: Terminal driven animations
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
        base_image: DynImageHandle,

        // compose from the frame at the given index
        from_frame_idx: FrameIdx,

        // frame index that is given to the new frame
        new_frame_idx: FrameIdx,
    ) -> Res<FrameHandle> {
        Action::new()
            .cd("a", "f")
            .cd("i", base_image.id())
            .cd("c", from_frame_idx)
            .cd_opt("s", self.width)
            .cd_opt("v", self.height)
            .cd_opt("x", self.x)
            .cd_opt("y", self.y)
            .push_cd(self.image.format.into())
            .payload(base64_turbo::STANDARD.encode(self.image.data.as_slice()))
            .commit();

        Ok(Handle::new(Frame {
            base_image,
            index: new_frame_idx,
        }))
    }
}

#[derive(Clone, Copy)]
pub enum FrameSelector {
    First,
    Last,
    ID(usize),
    // TODO?: Previous
}

#[derive(Clone, Debug)]
pub struct Frame {
    base_image: DynImageHandle,
    index: usize,
    // request: FrameRequest,
}

impl FrameHandle {
    pub fn edit(&self, edit: FrameEdit<ComposeFromSelector, Upload>) -> Res<()> {
        let Frame {
            base_image: img,
            index: frame_idx,
        } = self.read().unwrap().clone();

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
                .cd("i", img.id())
                // source frame
                .cd("r", img.resolve_frame_selector(from_frame))
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
                .cd("i", img.id())
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
}

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

#[derive(Debug, Clone, Default)]
pub struct PlacementRequest {
    // TODO: Do not option this
    // TODO: DO we need to store an image here?
    pub image: Option<DynImageHandle>,
    pub parent: Option<DynImageHandle>,
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
    pub fix_cursor: Option<()>,
    // TODO:
    // H, V
}

impl PlacementRequest {
    pub fn place(self, id: Id, action: &'static str) -> PlacementHandle {
        self.exec_place(id, action);

        Handle::new(Some(Placement {
            id,
            image: self.image.clone().unwrap(),
            config: self,
        }))
    }

    pub fn exec_place(&self, id: Id, action: &'static str) {
        Action::new() //
            .cd("a", action)
            .cd(
                "i",
                self.image
                    .as_ref()
                    .expect("image to be there")
                    .read()
                    .unwrap()
                    .id,
            )
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
            .cd_if(self.fix_cursor.is_some(), "C", 1)
            .commit();
    }

    /// Adds attribute related cds
    fn add_cds(&self, a: Action) -> Action {
        a // add cds
            .cd("i", self.image.as_ref().unwrap().read().unwrap().id)
            .cd_opt("X", self.offset_x)
            .cd_opt("Y", self.offset_y)
            .cd_opt("c", self.cols)
            .cd_opt("r", self.rows)
            .cd_opt("x", self.x)
            .cd_opt("y", self.y)
            .cd_opt("z", self.z)
            .cd_opt("w", self.width)
            .cd_opt("h", self.height)
            .cd_if(self.fix_cursor.is_some(), "C", 1)
    }
}

#[derive(Debug, Clone)]
pub struct Placement {
    id: Id,
    image: DynImageHandle,
    config: PlacementRequest,
}

#[derive(Debug, Clone)]
pub enum Placement2 {
    Attached(AttachedPlacement),
    Detached(DetachedPlacement),
}

#[derive(Debug, Clone)]
pub struct AttachedPlacement {}

#[derive(Debug, Clone)]
pub struct DetachedPlacement {}

// #[derive(Default, Builder, Debug)]
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

    // /// a=d
    // pub fn delete(query: DeleteQuery) -> Self {
    //     let mut a = TGPAction {
    //         cds: vec![ControlData("a", "d".to_string())],
    //         payload: None,
    //     };

    //     use DeleteQuery::*;

    //     // TODO: refactor
    //     match query {
    //         All => a.cds.push(ControlData {
    //             key: "d",
    //             value: "a".to_string(),
    //         }),

    //         Id { image, placement } => {
    //             a.cds.push(ControlData {
    //                 key: "d",
    //                 value: "i".to_string(),
    //             });

    //             a.cds.push(ControlData {
    //                 key: "i",
    //                 value: image.to_string(),
    //             });

    //             if let Some(id) = placement {
    //                 a.cds.push(ControlData {
    //                     key: "p",
    //                     value: id.to_string(),
    //                 });
    //             }
    //         }
    //         Cursor => todo!(),
    //         Frames => todo!(),
    //         Placements { x, y } => todo!(),
    //         PlacementsWithZ { x, y, z } => todo!(),
    //         IdsRange { from, to } => todo!(),
    //         Column(_) => todo!(),
    //         Row(_) => todo!(),
    //         Z(_) => todo!(),
    //     };

    //     a
    // }

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

        println!("\x1b_G{cds};{payload}\x1b\\");
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

#[derive(Clone, Copy, Debug)]
pub enum Format {
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

pub enum DeleteQuery {
    /// Delete all placements visible on screen
    ///
    /// (a or A)
    All,

    /// Delete all images with the specified id, specified using the i key.
    /// If you specify a p key for the placement id as well,
    /// then only the placement with the specified image id and placement id will be deleted.
    ///
    /// (i or I)
    Id { image: u32, placement: Option<u32> },

    // TODO:
    // Newest {}
    /// Delete all placements that intersect with the current cursor position.
    ///
    /// (c or C)
    Cursor,

    /// Delete animation frames.
    ///
    /// (f or F)
    Frames,

    /// Delete all placements that intersect a specific cell, the cell is specified using the x and y keys
    ///
    /// (p or P)
    Placements { x: u32, y: u32 },

    /// Delete all placements that intersect a specific cell having a specific z-index. The cell and z-index is specified using the x, y and z keys.
    ///
    /// (q or Q)
    PlacementsWithZ { x: u32, y: u32, z: i32 },

    /// Delete all images whose id is greater than or equal to the value of the x key and less than or equal to the value of the y (added in kitty version 0.33.0).
    ///
    /// (r or R)
    IdsRange { from: u32, to: u32 },

    /// Delete all placements that intersect the specified column, specified using the x key.
    ///
    /// (x or X)
    Column(u32),

    /// Delete all placements that intersect the specified row, specified using the y key.
    ///
    /// (y or Y)
    Row(u32),

    /// Delete all placements that have the specified z-index, specified using the z key.
    ///
    /// (z or Z)
    Z(i32),
}

enum Medium {
    Direct,
    File,
    TempFile,
    Shm,
}

// TODO:
// The PNG format is specified using the f=100 key. The width and height of the image will be read from the PNG data itself. Note that if you use both PNG and compression, then you must provide the S key with the size of the PNG data.
struct ControlData(&'static str, String);

impl ControlData {
    fn format(&self) -> String {
        format!("{}={}", self.0, self.1)
    }
}
