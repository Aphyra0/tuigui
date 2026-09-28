//! Split a frame into an `N x N` grid of rectangular micro-blocks covering the
//! whole frame, and stream each with *dynamic resolution*: track what is on
//! screen at what resolution level, so downstream only retransmits a block when
//! it is new, changed, or coarser than it could be.
//!
//! `N` here is the number of blocks per side (e.g. [`DEFAULT_BLOCKS_PER_SIDE`]
//! = 4 splits the frame into 4 columns x 4 rows, i.e. 16 blocks), *not* a pixel
//! size. Each block covers `ceil(width / N) x ceil(height / N)` pixels; edge
//! blocks are clamped to the frame bounds.
//!
//! Each block also lives on a resolution ladder of [`DEFAULT_RES_LEVELS`]
//! levels (settable, default 4). Level `res_levels` (4 by default) is the full
//! source resolution of the block; level 1 is the coarsest downscale. The per
//! block state machine:
//!
//! - *Never seen before*: transmit at the full (max) resolution and record that
//!   level, remembering the block's full-resolution content as its baseline.
//! - *Same content as baseline* (its full-res bytes are unchanged from what we
//!   last stored) but currently shown coarser than max: transmit one rung
//!   higher and remember the new level. This is a static scene progressively
//!   sharpening; when already at max it is skipped entirely (terminal keeps the
//!   old pixels) so a settled frame costs nothing.
//! - *Content changed* (full-res bytes differ from baseline): reset: transmit
//!   at the coarsest level, store the new full-res bytes as the new baseline,
//!   and remember the level is `1`. It then climbs back to full res over the
//!   next few static frames.
//!
//! The idea: when something moves it is repainted cheaply and blurry, and when
//! it stops it sharpens to full detail over a couple frames — a classic
//! progressive/progressive-resolution scheme (TigerVNC-style).

use bytes::Bytes;

use crate::frame::Frame;

/// Default number of micro-blocks per side. 4 splits the frame into a 4x4 grid
/// (16 blocks total).
pub const DEFAULT_BLOCKS_PER_SIDE: u32 = 4;

/// Default number of resolution levels per block. Level 1 is the coarsest, and
/// level `DEFAULT_RES_LEVELS` is the block's full source resolution.
pub const DEFAULT_RES_LEVELS: u32 = 4;

/// Size of the block grid (blocks per side).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockGridDim {
    pub cols: u32,
    pub rows: u32,
}

/// One micro-block cropped out of a frame. The rectangle `(x, y, width,
/// height)` is the block's extent in *source pixel* coordinates and drives
/// where it lands on screen; `data` is the *downscaled* payload actually
/// transmitted, sized `payload_width x payload_height`, corresponding to its
/// resolution `level` (1 = coarsest, `res_levels` = full source resolution).
///
/// Edge blocks along the right/bottom border may be smaller than the nominal
/// block size; the payload is always scaled up by the terminal to fill the
/// block's on-screen cell rect.
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
    /// Width in full-resolution source pixels (used for on-screen placement).
    pub width: u32,
    /// Height in full-resolution source pixels (used for on-screen placement).
    pub height: u32,
    /// Resolution level this payload is transmitted at: `1` = coarsest,
    /// `res_levels` = the block's full source resolution.
    pub level: u8,
    /// Width of the transmitted payload in pixels (`<= width`, equal when
    /// `level == res_levels`).
    pub payload_width: u32,
    /// Height of the transmitted payload in pixels.
    pub payload_height: u32,
    /// Pixel payload of this block, `payload_width * payload_height *
    /// bytes_per_pixel` bytes.
    pub data: Bytes,
}

/// A stateful block differ: holds, per block, the last transmitted
/// full-resolution content (the "baseline") and the resolution level currently
/// shown, and on [`BlockGrid::diff`] returns only the blocks downstream must
/// (re)transmit, each at the resolution level its state machine selects.
#[derive(Debug)]
pub struct BlockGrid {
    cols: u32,
    rows: u32,
    /// Nominal block size in pixels (ceil w/cols, ceil h/rows).
    bw: u32,
    bh: u32,
    /// Number of resolution levels (1 = full-res only, up to a real ladder).
    res_levels: u32,
    /// Last transmitted full-resolution content of each block, `None` before
    /// first sight of the block. Comparing this against the current full-res
    /// content is the change signal; it doubles as the stored "hash".
    prev_full: Vec<Option<Bytes>>,
    /// Resolution level currently shown on screen for each block (1..=res_levels).
    shown_level: Vec<u8>,
    /// Frame dims the grid was built from.
    dims: BlockGridDim,
}

impl BlockGrid {
    /// A differ that splits any frame into a `cols x rows` grid of blocks, each
    /// covering `ceil(w/cols) x ceil(h/rows)` pixels, with `res_levels`
    /// resolution levels per block. `res_levels == 0` falls back to
    /// [`DEFAULT_RES_LEVELS`]; `1` disables the ladder (always full res).
    pub fn new(cols: u32, rows: u32, res_levels: u32) -> Self {
        let res_levels = if res_levels == 0 { DEFAULT_RES_LEVELS } else { res_levels.max(1) };
        BlockGrid {
            cols: cols.max(1),
            rows: rows.max(1),
            bw: 0,
            bh: 0,
            res_levels,
            prev_full: Vec::new(),
            shown_level: Vec::new(),
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

    /// Number of resolution levels configured for this grid.
    pub fn res_levels(&self) -> u32 {
        self.res_levels
    }

    /// Split `frame` into blocks and decide, per block, whether and at what
    /// resolution to (re)transmit it. Returns only the blocks that need work.
    ///
    /// When `debug_odd` is set, odd linear-indexed blocks (`(row * cols +
    /// col) % 2 == 1`) are filled with a pseudo-random color derived from
    /// `seed`. Because that color changes whenever `seed` advances, those
    /// blocks read as perpetually-changed and are always retransmitted at the
    /// coarsest level, while even blocks carry the real pixels and idle to
    /// full res then go quiet.
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
        if self.prev_full.len() != n {
            self.prev_full.clear();
            self.prev_full.resize(n, None);
            self.shown_level.resize(n, 1);
        }

        let mut changed = Vec::new();
        for by in 0..self.rows {
            for bx in 0..self.cols {
                let x = bx * self.bw;
                let y = by * self.bh;
                let bw = self.bw.min(w - x);
                let bh = self.bh.min(h - y);
                let idx = (by * self.cols + bx) as usize;

                // Full-resolution content of this block (the change baseline).
                let mut full = vec![0u8; (bw * bh) as usize * bpp];
                for r in 0..bh {
                    let src_off = ((y + r) * w + x) as usize * bpp;
                    let dst_off = (r * bw) as usize * bpp;
                    full[dst_off..dst_off + (bw as usize) * bpp]
                        .copy_from_slice(&frame.data[src_off..src_off + (bw as usize) * bpp]);
                }
                let content = Bytes::from(full);
                let cur = if debug_odd && (by * self.cols + bx) % 2 == 1 {
                    colorize(&content, bpp, seed, idx as u64)
                } else {
                    content
                };

                // Pick the resolution level to transmit, updating per-block
                // state (baseline content + shown level).
                let level: u8 = match &self.prev_full[idx] {
                    // First sight of this block: full resolution, become the baseline.
                    None => {
                        self.prev_full[idx] = Some(cur.clone());
                        self.shown_level[idx] = self.res_levels as u8;
                        self.res_levels as u8
                    }
                    // Same content as baseline. If already shown at max res,
                    // nothing to do; otherwise climb one rung toward full res.
                    Some(p) if p.as_ref() == cur.as_ref() => {
                        if self.shown_level[idx] as u32 >= self.res_levels {
                            continue;
                        }
                        self.shown_level[idx] += 1;
                        self.shown_level[idx]
                    }
                    // Content changed: reset to the coarsest level and re-baseline.
                    Some(_) => {
                        self.prev_full[idx] = Some(cur.clone());
                        self.shown_level[idx] = 1;
                        1
                    }
                };

                // Downscale to the chosen level's payload. factor 1 = full res.
                let factor = (self.res_levels + 1 - level as u32).max(1);
                let (data, pw, ph) = downscale_block(&cur, bw, bh, bpp, factor);

                changed.push(MicroBlock {
                    col: bx,
                    row: by,
                    x,
                    y,
                    width: bw,
                    height: bh,
                    level,
                    payload_width: pw,
                    payload_height: ph,
                    data,
                });
            }
        }
        changed
    }
}

/// Downscale `data` (an `w x h` image of `bpp`-byte pixels) by an integer
/// `factor`, averaging each `factor x factor` source region into one output
/// pixel (edge regions clamped). `factor <= 1` returns the input unchanged.
fn downscale_block(data: &[u8], w: u32, h: u32, bpp: usize, factor: u32) -> (Bytes, u32, u32) {
    if factor <= 1 {
        return (Bytes::copy_from_slice(data), w, h);
    }
    let pw = w.div_ceil(factor);
    let ph = h.div_ceil(factor);
    let mut out = vec![0u8; (pw * ph) as usize * bpp];
    for oy in 0..ph {
        let y0 = oy * factor;
        let y1 = (y0 + factor).min(h);
        for ox in 0..pw {
            let x0 = ox * factor;
            let x1 = (x0 + factor).min(w);
            let count = (x1 - x0) * (y1 - y0);
            let dst = ((oy * pw + ox) as usize) * bpp;
            for lane in 0..bpp {
                let mut s: u32 = 0;
                for sy in y0..y1 {
                    for sx in x0..x1 {
                        s += data[(((sy * w + sx) as usize) * bpp) + lane] as u32;
                    }
                }
                out[dst + lane] = (s / count) as u8;
            }
        }
    }
    (Bytes::from(out), pw, ph)
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
    fn first_frame_emits_every_block_at_full_resolution() {
        let mut g = BlockGrid::new(4, 4, 4); // 4x4 = 16 blocks, 4 res levels
        let f = solid(100, 100, [1, 2, 3, 255]);
        let blocks = g.diff(&f, false, 0);
        assert_eq!(blocks.len(), 16);
        assert_eq!(g.dims(), BlockGridDim { cols: 4, rows: 4 });
        assert_eq!(blocks[0].width, 25);
        assert_eq!(blocks[0].height, 25);
        // First sight transmits at the max resolution (payload = full size).
        assert_eq!(blocks[0].level, 4);
        assert_eq!(blocks[0].payload_width, 25);
        assert_eq!(blocks[0].payload_height, 25);
        // Last block (3,3) is clamped to frame edge.
        let last = blocks.last().unwrap();
        assert_eq!(last.col, 3);
        assert_eq!(last.row, 3);
        assert_eq!(last.x + last.width, 100);
        assert_eq!(last.y + last.height, 100);
    }

    #[test]
    fn identical_frame_at_max_level_emits_nothing() {
        let mut g = BlockGrid::new(4, 4, 4);
        let f = solid(80, 60, [9, 9, 9, 255]);
        g.diff(&f, false, 0);
        let again = g.diff(&f, false, 1);
        assert!(again.is_empty());
    }

    #[test]
    fn changing_one_block_emits_only_it_at_lowest_level() {
        let mut g = BlockGrid::new(4, 4, 4);
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
        // A change resets to the coarsest level: 20x20 block, res_levels 4 ->
        // factor 4, so a 5x5 payload.
        assert_eq!(changed[0].level, 1);
        assert_eq!(changed[0].payload_width, 5);
        assert_eq!(changed[0].payload_height, 5);
    }

    #[test]
    fn static_block_climbs_one_rung_per_frame_then_quiets() {
        let mut g = BlockGrid::new(1, 1, 4); // single block over the whole frame
        let f = solid(40, 40, [7, 7, 7, 255]);
        // First sight: full res.
        let first = g.diff(&f, false, 0);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].level, 4);
        // Static, already at max: quiet.
        assert!(g.diff(&f, false, 1).is_empty());
        // Change content: reset to level 1.
        let f2 = solid(40, 40, [30, 30, 30, 255]);
        let c1 = g.diff(&f2, false, 2);
        assert_eq!(c1.len(), 1);
        assert_eq!(c1[0].level, 1);
        assert_eq!(c1[0].payload_width, 10); // 40 / 4
        // Same content now: climbs 1 -> 2 -> 3 -> 4, then quiet.
        let c2 = g.diff(&f2, false, 3);
        assert_eq!(c2[0].level, 2);
        assert_eq!(c2[0].payload_width, 14); // ceil(40/3)
        let c3 = g.diff(&f2, false, 4);
        assert_eq!(c3[0].level, 3);
        assert_eq!(c3[0].payload_width, 20); // 40 / 2
        let c4 = g.diff(&f2, false, 5);
        assert_eq!(c4[0].level, 4);
        assert_eq!(c4[0].payload_width, 40);
        assert!(g.diff(&f2, false, 6).is_empty());
    }

    #[test]
    fn debug_odd_colorizes_odd_blocks_and_forces_retransmit() {
        let mut g = BlockGrid::new(4, 4, 4);
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
        // Same frame, new seed: odd blocks are colorized anew (content-change
        // branch -> retransmitted at the coarsest level), even blocks are
        // unchanged at full res and stay quiet.
        let next = g.diff(&f, true, 1);
        let odd_cnt = next.iter().filter(|b| (b.row * 4 + b.col) % 2 == 1).count();
        let even_cnt = next.iter().filter(|b| (b.row * 4 + b.col) % 2 == 0).count();
        assert_eq!(odd_cnt, 8);
        assert_eq!(even_cnt, 0);
        for b in next.iter().filter(|b| (b.row * 4 + b.col) % 2 == 1) {
            assert_eq!(b.level, 1);
        }
    }
}