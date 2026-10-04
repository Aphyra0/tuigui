//! Terminal graphics protocol (TGP): the wire format used to stream
//! pixel data into a graphics-capable terminal.

use std::{
    fmt::Display,
    io::{Read, Write},
    sync::Arc,
};

use derive_builder::Builder;
use idalloc::Slab;
use itertools::Itertools;
use rustix::termios::LocalModes;
use static_init::dynamic;

// TODO: https://sw.kovidgoyal.net/kitty/graphics-protocol/#unicode-placeholders
// TODO: https://sw.kovidgoyal.net/kitty/graphics-protocol/#relative-placements
// TODO: https://sw.kovidgoyal.net/kitty/graphics-protocol/#usage-hints

mod id {
    use crate::ID;
    use crossterm::terminal;
    use idalloc::Slab;
    use static_init::dynamic;

    /// Locked lazy global slab. Access via `IDAl.write()` / `IDAl.read()`.
    #[dynamic]
    static mut IDAl: Slab<ID> = {
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

        let mut slab = Slab::<ID>::new();
        // skip 0 as an id.
        slab.next();
        slab
    };

    /// Allocate the next unique image/placement id.
    pub(crate) fn alloc() -> ID {
        IDAl.write().next()
    }

    /// Reclaim an id for reuse.
    pub(crate) fn free(id: ID) {
        IDAl.write().free(id);
    }
}

pub type ID = u32;

pub struct NewImage {
    format: Format,
    data: Vec<u8>,
}

impl NewImage {
    pub fn new(format: Format, data: Vec<u8>) -> Self {
        NewImage { format, data }
    }

    pub fn transplace(self, placement: NewPlacement) -> AnonymousImage {
        let payload = base64_turbo::STANDARD.encode(self.data.as_slice());
        let mut a = Action::new()
            .cd("a", "T")
            .cd(
                "f",
                match self.format {
                    Format::RGB => 24,
                    Format::RGBA => 32,
                    Format::PNG => 100,
                },
            )
            .payload(payload);

        placement.add_cds(&mut a);
        a.commit();

        AnonymousImage { request: self }
    }

    pub fn transmit(self) -> Image {
        let payload = base64_turbo::STANDARD.encode(self.data.as_slice());
        let id = id::alloc();

        Action::new()
            .cd("i", id)
            .cd(
                "f",
                match self.format {
                    Format::RGB => 24,
                    Format::RGBA => 32,
                    Format::PNG => 100,
                },
            )
            .payload(payload)
            .commit();

        Image { id, request: self }
    }
}

pub struct Image {
    id: ID,
    request: NewImage,
}

pub struct AnonymousImage {
    request: NewImage,
}

impl Image {
    pub fn place_named(&self, request: NewPlacement) -> Placement {
        let id = id::alloc();

        let mut a = Action::new() //
            .cd("a", "p")
            .cd("i", self.id)
            .cd("p", id);

        request.add_cds(&mut a);
        a.commit();

        Placement { id, request }
    }

    pub fn place(&self, request: NewPlacement) {
        todo!()
    }

    pub fn delete(self) {
        todo!()
    }
}

/// Animation based on a single image
/// and with the single frame
///
/// all changes are placed on the overlay.
///
/// Updates are not atomics, but it doesn't create new frames.
pub struct ThinStreamer {
    root: Image,
    overlay: Frame,
}

impl ThinStreamer {}

pub struct AtomicStreamer {
    animation: Animation,
    pending: bool,
}

impl AtomicStreamer {
    pub fn update(&mut self, patch: NewImage, offset: (u32, u32)) {
        if !self.pending {
            // create new frame
            self.animation.new_frame(NewFrame {
                base_frame: FrameSelector::Last,
                image: patch,
                width: None,
                height: None,
                x: Some(offset.0),
                y: Some(offset.1),
            });

            self.pending = true;
        } else {
            // Edit frame instead
            let f = self.animation.frame(FrameSelector::Last).unwrap();
        }
    }

    pub fn present(&mut self) {}
}

// TODO: Better sync
pub struct Animation {
    base_image: Image,
    frames: Vec<Frame>,
}

impl From<Image> for Animation {
    fn from(value: Image) -> Self {
        Animation::new(value)
    }
}

impl Animation {
    pub fn new(base_image: Image) -> Self {
        Self {
            frames: Default::default(),
            base_image,
        }
    }

    pub fn frame(&mut self, selector: FrameSelector) -> Option<&Frame> {
        use FrameSelector::*;

        match selector {
            Last => self.frames.last(),
            First => todo!(),
            ID(_) => todo!(),
        }
    }

    pub fn new_frame(
        &mut self,
        NewFrame {
            base_frame,
            width,
            height,
            x,
            y,
            image,
        }: NewFrame,
    ) -> &Frame {
        Action::new()
            .cd("a", "f")
            .cd("i", self.base_image.id)
            .cd(
                "c",
                match base_frame {
                    FrameSelector::Last => self.frames.len() + 1,
                    FrameSelector::First => 1,
                    FrameSelector::ID(id) => id,
                },
            )
            .cd_opt("s", width)
            .cd_opt("v", height)
            .cd_opt("x", x)
            .cd_opt("y", y)
            .cd(
                "f",
                match image.format {
                    Format::RGB => 24,
                    Format::RGBA => 32,
                    Format::PNG => 100,
                },
            )
            .payload(base64_turbo::STANDARD.encode(image.data.as_slice()))
            .commit();

        let prev = self.frames.last().unwrap_or(&Frame {
            index: 1,
            base_image: 0,
        });

        self.frames.push(Frame {
            index: prev.index + 1,
            base_image: self.base_image.id,
        });

        self.frames.last().unwrap()
    }

    pub fn display_last(&mut self) {
        if let Some(f) = self.frames.last() {
            self.display_frame(f.index);
        }
    }

    // TODO: Use selector, rename to just display
    pub fn display_frame(&mut self, frame: usize) {
        Action::new()
            .cd("a", "a")
            .cd("i", self.base_image.id)
            .cd("c", frame)
            .commit();
    }

    // TODO: Terminal driven animations
}

pub struct NewFrame {
    pub base_frame: FrameSelector,
    pub image: NewImage,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub x: Option<u32>,
    pub y: Option<u32>,
}

pub enum FrameSelector {
    Last,
    First,
    ID(usize),
}

pub struct Frame {
    base_image: ID,
    index: usize,
}

impl Frame {
    pub fn edit(&self, edit: FrameEdit) {
        use FrameEdit::*;
        match edit {
            Compose {
                from_frame,
                width,
                height,
                src,
                dst,
            } => Action::new()
                .cd("a", "c")
                // image context
                .cd("i", self.base_image)
                // source frame
                .cd("r", from_frame)
                // destination frame
                .cd("c", self.index)
                // size
                .cd("w", width)
                .cd("h", height)
                // source location
                .cd("X", src.0)
                .cd("Y", src.1)
                // destination location
                .cd("x", dst.0)
                .cd("y", dst.1),
            Upload {
                x,
                y,
                width,
                height,
                image,
            } => todo!(),
        }
        .commit();
    }
}

pub enum FrameEdit {
    Compose {
        from_frame: usize,
        // TODO: Make optionals
        width: u32,
        height: u32,
        src: (u32, u32),
        dst: (u32, u32),
    },
    Upload {
        x: u32,
        y: u32,
        width: Option<u32>,
        height: Option<u32>,
        image: Image,
    },
}

#[derive(Default, Builder, Debug)]
#[builder(setter(into))]
pub struct NewPlacement {
    pub image: Option<ID>,
    pub parent: Option<ID>,
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

impl NewPlacement {
    /// Adds attribute related cds
    fn add_cds(&self, a: &mut Action) {
        // if let Some(parent)

        if let Some(id) = self.image {
            a.add_cd("i", id);
        }

        if let Some(px) = self.offset_x {
            a.add_cd("X", px);
        }

        if let Some(px) = self.offset_y {
            a.add_cd("Y", px);
        }

        if let Some(u) = self.cols {
            a.add_cd("c", u);
        }

        if let Some(u) = self.rows {
            a.add_cd("r", u);
        }

        if let Some(u) = self.x {
            a.add_cd("x", u);
        }

        if let Some(u) = self.y {
            a.add_cd("y", u);
        }

        if let Some(u) = self.z {
            a.add_cd("z", u);
        }

        if let Some(px) = self.width {
            a.add_cd("w", px);
        }

        if let Some(px) = self.height {
            a.add_cd("h", px);
        }

        if let Some(_) = self.fix_cursor {
            a.add_cd("C", 1);
        }
    }
}

#[derive(Debug)]
pub struct Placement {
    id: ID,
    request: NewPlacement,
}

pub struct ImageHandle {}

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
        self.push(ControlData {
            key,
            value: value.to_string(),
        })
    }

    fn cd_opt(self, key: &'static str, value_o: Option<impl Display>) -> Self {
        if let Some(value) = value_o {
            self.push(ControlData {
                key,
                value: value.to_string(),
            })
        } else {
            self
        }
    }

    /// Takes a reference
    fn add_cd(&mut self, key: &'static str, value: impl Display) {
        self.cds.push(ControlData {
            key,
            value: value.to_string(),
        })
    }

    /// a=d
    pub fn delete(query: DeleteQuery) -> Self {
        let mut a = Action {
            cds: vec![ControlData {
                key: "a",
                value: "d".to_string(),
            }],
            payload: None,
        };

        use DeleteQuery::*;

        // TODO: refactor
        match query {
            All => a.cds.push(ControlData {
                key: "d",
                value: "a".to_string(),
            }),

            Id { image, placement } => {
                a.cds.push(ControlData {
                    key: "d",
                    value: "i".to_string(),
                });

                a.cds.push(ControlData {
                    key: "i",
                    value: image.to_string(),
                });

                if let Some(id) = placement {
                    a.cds.push(ControlData {
                        key: "p",
                        value: id.to_string(),
                    });
                }
            }
            Cursor => todo!(),
            Frames => todo!(),
            Placements { x, y } => todo!(),
            PlacementsWithZ { x, y, z } => todo!(),
            IdsRange { from, to } => todo!(),
            Column(_) => todo!(),
            Row(_) => todo!(),
            Z(_) => todo!(),
        };

        a
    }

    fn push(mut self, cd: ControlData) -> Self {
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

// #[derive(this)]
pub enum TGPError {}

pub enum Format {
    RGB,
    RGBA,
    PNG,
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
struct ControlData {
    key: &'static str,
    // TODO: Use proper value type
    value: String,
}

impl ControlData {
    fn format(&self) -> String {
        format!("{}={}", self.key, self.value)
    }
}
