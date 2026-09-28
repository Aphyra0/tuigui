//! Split a frame into an `N x N` grid of rectangular micro-blocks covering the
//! whole frame, and diff each against the previous frame so downstream can
//! retransmit only the blocks that actually changed.
//!
//! `N` here is the number of blocks per side (e.g. [`DEFAULT_BLOCKS_PER_SIDE`]
//! = 4 splits the frame into 4 columns x 4 rows, i.e. 16 blocks), *not* a pixel
//! size. Each block covers `ceil(width / N) x ceil(height / N)` pixels; edge
//! blocks are clamped to the frame bounds. Blocks whose content is identical to
//! last time are omitted entirely — the terminal keeps rendering the block from
//! the previous frame, so a static scene costs almost nothing.

use bytes::Bytes;

use crate::frame::Frame;

/// Default number of micro-blocks per side. 4 splits the frame into a 4x4 grid
/// (16 blocks total).
pub const DEFAULT_BLOCKS_PER_SIDE: u32 = 4;

/// Size of the block grid (blocks per side).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockGridDim {
    pub cols: u32,
    pub rows: u32,
}

/// One micro-block cropped out of a frame, positioned in *source pixel*
/// coordinates. Edge blocks along the right/bottom border may be smaller than
/// the nominal block size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MicroBlock {
    /// Block column index in the grid (0..cols).
    pub col: u32,
    /// Block row index in the grid (0..rows).
    pub row: u32,
    /// Top-left source pixel X.
    pub x: u32,
    /// Top-left source pixel Y.
    pub y: u32,
    /// Width in source pixels.
    pub width: u32,
    /// Height in source pixels.
    pub height: u32,
    /// Pixel payload of this block, `width * height * bytes_per_pixel` bytes.
    pub data: Bytes,
}

/// A stateful block differ: holds the previous content of every block and, on
/// [`BlockGrid::diff`], returns only the blocks that changed since last time.
#[derive(Debug)]
pub struct BlockGrid {
    cols: u32,
    rows: u32,
    /// Nominal block size in pixels (ceil w/cols, ceil h/rows).
    bw: u32,
    bh: u32,
    /// Previous content of each block, `None` before the first frame in which
    /// the block is seen (so every block counts as changed on frame one).
    prev: Vec<Option<Bytes>>,
    /// Frame dims the grid was built from.
    dims: BlockGridDim,
}

impl BlockGrid {
    /// A differ that splits any frame into a `cols x rows` grid of blocks, each
    /// covering `ceil(w/cols) x ceil(h/rows)` pixels.
    pub fn new(cols: u32, rows: u32) -> Self {
        BlockGrid {
            cols: cols.max(1),
            rows: rows.max(1),
            bw: 0,
            bh: 0,
            prev: Vec::new(),
            dims: BlockGridDim { cols, rows },
        }
    }

    /// Grid dimensions (blocks per side).
    pub fn dims(&self) -> BlockGridDim {
        self.dims
    }

    pub fn cols(&self) -> u32 {
        self.cols
    }

    pub fn rows(&self) -> u32 {
        self.rows
    }

    /// Split `frame` into blocks and diff each against the previously stored
    /// content. Returns every block whose content changed (or was first seen).
    ///
    /// When `debug_odd` is set, odd linear-indexed blocks (`(row * cols +
    /// col) % 2 == 1`) are filled with a pseudo-random color derived from
    /// `seed` (so, when `seed` advances per frame, they change every frame and
    /// are always retransmitted), while even blocks carry the real pixels. This
    /// visually proves the grid and per-block diffing: even blocks on a static
    /// frame stay silent, odd blocks keep streaming.
    pub fn diff(&mut self, frame: &Frame, debug_odd: bool, seed: u64) -> Vec<MicroBlock> {
        let w = frame.metadata.width;
        let h = frame.metadata.height;
        let bpp = frame.metadata.format.bytes_per_pixel();
        self.cols = self.cols.max(1);
        self.rows = self.rows.max(1);
        self.bw = w.div_ceil(self.cols);
        self.bh = h.div_ceil(self.rows);
        // Rebuild previous storage if frame dims (and thus block raster) changed.
        let n = (self.cols * self.rows) as usize;
        if self.prev.len() != n {
            self.prev.clear();
            self.prev.resize(n, None);
        }

        let mut changed = Vec::new();
        for by in 0..self.rows {
            for bx in 0..self.cols {
                let x = bx * self.bw;
                let y = by * self.bh;
                let bw = self.bw.min(w - x);
                let bh = self.bh.min(h - y);
                let idx = (by * self.cols + bx) as usize;

                let mut data = vec![0u8; (bw * bh) as usize * bpp];
                for r in 0..bh {
                    let src_off = ((y + r) * w + x) as usize * bpp;
                    let dst_off = (r * bw) as usize * bpp;
                    data[dst_off..dst_off + (bw as usize) * bpp]
                        .copy_from_slice(&frame.data[src_off..src_off + (bw as usize) * bpp]);
                }

                // The block we would actually transmit: real pixels, or (in
                // debug-odd mode) a per-block random color.
                let bytes = Bytes::from(data);
                let out_data = if debug_odd && (by * self.cols + bx) % 2 == 1 {
                    colorize(&bytes, bpp, seed, idx as u64)
                } else {
                    bytes.clone()
                };

                let first_or_changed = match &self.prev[idx] {
                    None => true,
                    Some(p) => p.as_ref() != out_data.as_ref(),
                };
                // Store what was last transmitted so the next diff compares
                // against the on-screen content.
                self.prev[idx] = Some(out_data.clone());

                if first_or_changed {
                    changed.push(MicroBlock {
                        col: bx,
                        row: by,
                        x,
                        y,
                        width: bw,
                        height: bh,
                        data: out_data,
                    });
                }
            }
        }
        changed
    }
}

/// Replace a block's pixels with a solid pseudo-random color deterministic in
/// `(seed, salt)`. Compatible with both `Rgb24` and `Rgba32` payloads.
fn colorize(data: &[u8], bpp: usize, seed: u64, salt: u64) -> Bytes {
    let (r, g, b) = (rand_byte(seed, salt, 0), rand_byte(seed, salt, 1), rand_byte(seed, salt, 2));
    let mut out = vec![0u8; data.len()];
    let px: &[u8] = if bpp >= 4 {
        &[r, g, b, 255]
    } else {
        &[r, g, b]
    };
    for chunk in out.chunks_exact_mut(bpp) {
        chunk.copy_from_slice(&px[..bpp.min(px.len())]);
    }
    Bytes::from(out)
}

/// A deterministic, dependency-free pseudo-random byte from a mixed `(seed,
/// salt)` pair (splitmix64-style). Different every frame when `seed` advances,
/// and stable within a frame per `salt`, so each odd block keeps a distinct
/// color.
fn rand_byte(seed: u64, salt: u64, lane: u64) -> u8 {
    let z = seed
        .wrapping_mul(0x9E3779B97F4A7C15)
        .wrapping_add(salt.wrapping_mul(0xBF58476D1CE4E5B9))
        .wrapping_add(lane.wrapping_mul(0x94D049BB133111EB));
    ((z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9) ^ (123456 + lane)) as u8
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{Frame, FrameMetadata, PixelFormat};

    fn meta(w: u32, h: u32) -> FrameMetadata {
        FrameMetadata {
            width: w,
            height: h,
            format: PixelFormat::Rgba32,
        }
    }

    fn solid(w: u32, h: u32, px: [u8; 4]) -> Frame {
        let mut data_vec = vec![0u8; (w * h) as usize * 4];
        for c in data_vec.chunks_exact_mut(4) {
            c.copy_from_slice(&px);
        }
        Frame::full(meta(w, h), data_vec.into())
    }

    #[test]
    fn first_frame_emits_every_block_of_grid() {
        let mut g = BlockGrid::new(4, 4); // 4x4 = 16 blocks
        let f = solid(100, 100, [1, 2, 3, 255]);
        let blocks = g.diff(&f, false, 0);
        assert_eq!(blocks.len(), 16);
        // Each block is 25x25 (ceil(100/4)).
        assert_eq!(g.dims(), BlockGridDim { cols: 4, rows: 4 });
        assert_eq!(blocks[0].width, 25);
        assert_eq!(blocks[0].height, 25);
        // Last block (3,3) is clamped to frame edge.
        let last = blocks.last().unwrap();
        assert_eq!(last.col, 3);
        assert_eq!(last.row, 3);
        assert_eq!(last.x + last.width, 100);
        assert_eq!(last.y + last.height, 100);
    }

    #[test]
    fn identical_frame_emits_nothing() {
        let mut g = BlockGrid::new(4, 4);
        let f = solid(80, 60, [9, 9, 9, 255]);
        g.diff(&f, false, 0);
        let again = g.diff(&f, false, 1);
        assert!(again.is_empty());
    }

    #[test]
    fn changing_one_block_emits_only_it() {
        let mut g = BlockGrid::new(4, 4);
        let f0 = solid(80, 80, [5, 5, 5, 255]);
        g.diff(&f0, false, 0);
        // Paint only the top-left block (col 0, row 0) a different color. Its
        // extent is 20x20 (ceil(80/4)).
        let mut px = vec![0u8; 80 * 80 * 4];
        for b in 0..(80 * 80) {
            px[b * 4..b * 4 + 4].copy_from_slice(&[5, 5, 5, 255]);
        }
        for r in 0..20 {
            for c in 0..20 {
                let base = (r * 80 + c) as usize * 4;
                px[base..base + 4].copy_from_slice(&[9, 9, 9, 255]);
            }
        }
        let f1 = Frame::full(meta(80, 80), Bytes::from(px));
        let changed = g.diff(&f1, false, 1);
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].col, 0);
        assert_eq!(changed[0].row, 0);
    }

    #[test]
    fn debug_odd_colorizes_odd_blocks_and_forces_retransmit() {
        let mut g = BlockGrid::new(4, 4);
        let f = solid(80, 80, [0, 0, 0, 255]); // 16 blocks
        let blocks = g.diff(&f, true, 0);
        assert_eq!(blocks.len(), 16);
        for b in &blocks {
            let odd = (b.row * 4 + b.col) % 2 == 1;
            if odd {
                assert_ne!(b.data[0..4], [0, 0, 0, 255][..]);
            } else {
                assert_eq!(b.data[0..4], [0, 0, 0, 255][..]);
            }
        }
        // Same frame, new seed: odd blocks have new random colors (retransmit),
        // even blocks are unchanged and stay quiet.
        let next = g.diff(&f, true, 1);
        let odd_cnt = next.iter().filter(|b| (b.row * 4 + b.col) % 2 == 1).count();
        let even_cnt = next.iter().filter(|b| (b.row * 4 + b.col) % 2 == 0).count();
        assert_eq!(odd_cnt, 8);
        assert_eq!(even_cnt, 0);
    }
}