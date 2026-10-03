//! Terminal graphics protocol (TGP): the wire format used to stream
//! pixel data into a graphics-capable terminal.

use std::{io::Write, sync::Arc};

use derive_builder::Builder;
use idalloc::Slab;
use itertools::Itertools;
use static_init::dynamic;

// TODO: https://sw.kovidgoyal.net/kitty/graphics-protocol/#unicode-placeholders
// TODO: https://sw.kovidgoyal.net/kitty/graphics-protocol/#relative-placements
// TODO: https://sw.kovidgoyal.net/kitty/graphics-protocol/#usage-hints


mod id {
    use crate::ID;
    use idalloc::Slab;
    use static_init::dynamic;

    /// Locked lazy global slab. Access via `IDAl.write()` / `IDAl.read()`.
    #[dynamic]
    static mut IDAl: Slab<ID> = Slab::<ID>::new();

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

    pub fn transmit(self) -> Image {
        let payload = base64_turbo::STANDARD.encode(self.data.as_slice());
        let id = id::alloc();

        Action {
            cds: vec![
                cd("a", "t".to_string()),
            ],
            payload,
        }.commit();

        Image {
            id,
            request: self,
        }
    }

    // pub fn
    pub fn display(&self) {
        let payload = base64_turbo::STANDARD.encode(self.data.as_slice());

        // print!(
        //     "<ESC>_G{};{payload}<ESC>",
        //     // "<ESC>_G{},s=10,v=20;{payload}<ESC>",
        //     self.format.render().format()
        // )
        dbg!(payload.len());
        // print!(
        //     "{}_G{};{payload}{}",
        //     '\x1b',
        //     self.format.render().format(),
        //     "\x1b\\"
        // );

        // if payload.len() > 4096 {
        //     let mut chunks = payload.as_bytes().chunks(4096).enumerate();

        //     while let Some((i, chunk)) = chunks.next() {
        //         if i == 0 {
        //             // print!(
        //             //     "{}_G{},m=1;{}{}",
        //             //     '\x1b',
        //             //     chunk.to_stri,
        //             //     self.format.render().format(),
        //             //     "\x1b\\"
        //             // );

        //             continue;
        //         }
        //         if chunks.len() == 0 {
        //             // print!(
        //             //     "{}_G{};{chunk}{}",
        //             //     '\x1b',
        //             //     self.format.render().format(),
        //             //     "\x1b\\"
        //             // );
        //         } else {
        //         }
        //     }
        // } else {
        // print!(
        //     "{}_Ga=T,f=100;{}{}",
        //     '\x1b',
        //     // "escin",
        //     // self.format.render().format(),
        //     payload,
        //     // "escout\\" //
        //     "\x1b\\"
        // );
        // Action::transplace(payload).format(Format::PNG).commit();
        Action::transmit(payload, 33).format(Format::PNG).commit();
        Action::place(33).c(5).r(5).commit();
        // std::io::stdout().flush().unwrap();
        // }
        // for chunk in  {
        //     dbg!(chunk.len());
        // }
    }
}

pub struct Image {
    id: ID,
    request: NewImage,
}

impl Image {
    pub fn place_named(&self, request: NewPlacement) -> Placement {
        Placement {
            id: todo!(),
            request,
        }
        todo!();
    }

    pub fn place(&self, request: NewPlacement) {}

    pub fn destroy() {}
}

pub struct Animation {
    frames: Vec<Frame>,
    current: ID,
}

impl Animation {
    pub fn new() {}
}

pub struct NewPlacement {
    image: ID,
    parent: Option<ID>,
    offset_x: Option<u32>,
    offset_y: Option<u32>,
    cols: Option<u32>,
    rows: Option<u32>,
    x: Option<u32>,
    y: Option<u32>,
    z: Option<i32>,
    width: Option<u32>,
    height: Option<u32>,
    fix_cursor: Option<bool>,
    // TODO:
    // H, V
}

pub struct Placement {
    id: ID,
    request: NewPlacement,
}


pub struct ImageHandle {}

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

impl ThinStreamer {
    // pub fn new(base: TermImage) -> Self {
    // Self {
    //     root: base,
    //     // overlay: ,
    // }
    // }
}

pub struct NewFrame {
    base_frame: ID,
    edit: FrameEdit,
}

pub enum FrameEdit {
    Copy {
        frame: ID,
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

pub struct Frame {
    id: ID,
}

impl Frame {
    pub fn edit() {}
    pub fn display() {}
}

// #[derive(Default, Builder, Debug)]
pub struct Action {
    cds: Vec<ControlData>,
    payload: Option<String>,
}


impl Action {
    pub fn new() -> Action {
        Action {
            cds: vec![],
            payload
        }

        
    }
    fn cd(self, key: &'static str, value: String) -> Self {
        self.push(ControlData { key, value })
    }
    // a=T
    pub fn transplace(payload: String) -> Self {
        Action {
            cds: vec![ControlData {
                key: "a",
                value: "T".to_owned(),
            }],
            payload,
        }
    }

    // a=t
    pub fn transmit(payload: String, id: u32) -> Self {
        Action {
            cds: vec![ControlData {
                key: "i",
                value: id.to_string(),
            }],
            payload,
        }
    }

    // TODO: Placement id
    // a=p
    pub fn place(id: u32) -> Self {
        Action {
            cds: vec![
                ControlData {
                    key: "a",
                    value: "p".to_owned(),
                },
                ControlData {
                    key: "i",
                    value: id.to_string(),
                },
            ],
            payload: "OK".to_string(),
        }
    }

    /// a=f
    pub fn frame(payload: String) -> Self {
        Action {
            cds: vec![ControlData {
                key: "a",
                value: "f".to_owned(),
            }],
            payload,
        }
    }

    /// a=a
    pub fn switch_frame(id: u32, frame_id: u32) -> Self {
        todo!()
    }

    pub fn new_frame() {}
    pub fn edit_frame() {}

    /// a=d
    pub fn delete(query: DeleteQuery) -> Self {
        let mut a = Action {
            cds: vec![ControlData {
                key: "a",
                value: "d".to_string(),
            }],
            payload: "OK".to_string(),
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
    pub fn commit(self) {
        let cds = self
            .cds
            .iter()
            .map(|cdv| cdv.format())
            .collect_vec()
            .join(",");

        print!("{}_G{cds};{}{}", '\x1b', self.payload, "\x1b\\");
        std::io::stdout().flush().unwrap();
    }

    /// m=1
    pub fn chunk(self) -> Self {
        self.push(ControlData {
            key: "m",
            value: 1.to_string(),
        })
    }

    /// X=<px>
    pub fn off_x(self, px: u32) -> Self {
        self.push(ControlData {
            key: "X",
            value: px.to_string(),
        })
    }

    // TODO: Add checks for the currect px values.
    /// Y=<px>
    pub fn off_y(self, px: u32) -> Self {
        self.push(ControlData {
            key: "Y",
            value: px.to_string(),
        })
    }

    /// y=<px>
    pub fn y(self, px: u32) -> Self {
        self.push(ControlData {
            key: "y",
            value: px.to_string(),
        })
    }

    /// x=<px>
    pub fn x(self, px: u32) -> Self {
        self.push(ControlData {
            key: "x",
            value: px.to_string(),
        })
    }

    /// w=<px>
    pub fn w(self, px: u32) -> Self {
        self.push(ControlData {
            key: "w",
            value: px.to_string(),
        })
    }

    /// h=<px>
    pub fn h(self, px: u32) -> Self {
        self.push(ControlData {
            key: "h",
            value: px.to_string(),
        })
    }

    /// number of columns (horizontal) | c=<u>
    pub fn c(self, u: u32) -> Self {
        self.push(ControlData {
            key: "c",
            value: u.to_string(),
        })
    }

    /// number of rows (vertical) | c=<u>
    pub fn r(self, u: u32) -> Self {
        self.push(ControlData {
            key: "r",
            value: u.to_string(),
        })
    }

    /// z index | z=<idx>
    pub fn z(self, idx: i32) -> Self {
        self.push(ControlData {
            key: "z",
            value: idx.to_string(),
        })
    }

    /// C=1
    pub fn fix_cursor(self) -> Self {
        self.push(ControlData {
            key: "C",
            value: 1.to_string(),
        })
    }

    /// m=0
    pub fn last_chunk(self) -> Self {
        self.push(ControlData {
            key: "m",
            value: 0.to_string(),
        })
    }

    /// f=<format>
    pub fn format(self, format: Format) -> Self {
        self.push(ControlData {
            key: "f",
            value: match format {
                Format::RGB => 24,
                Format::RGBA => 32,
                Format::PNG => 100,
            }
            .to_string(),
        })
    }

    /// s=<px>
    pub fn width(self, px: u32) -> Self {
        self.push(ControlData {
            key: "s",
            value: px.to_string(),
        })
    }

    /// v=<px>
    pub fn height(self, px: u32) -> Self {
        self.push(ControlData {
            key: "v",
            value: px.to_string(),
        })
    }

    /// o=z
    pub fn compressed(self) -> Self {
        self.push(ControlData {
            key: "o",
            value: "z".to_owned(),
        })
    }

    pub fn medium(&mut self, medium: Medium) {
        todo!()
        // self.cds.push(CDVal {
        //     key: "o",
        //     value: "z".to_owned(),
        // });
    }

    // TODO: a=q
    // i=q
    // pub fn probe(&mut self) {
    //     self.cds.push(CDVal {
    //         key: "i",
    //         value: todo!(),
    //     });
    // }
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
