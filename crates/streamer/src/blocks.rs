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

/// Detail-adaptive resolution targeting.
///
/// A low-cost pass over each block measures the largest difference between any
/// neighboring pixel pair and uses it to pick how far up the resolution ladder
/// the block may climb. On by default; see [`DetailConfig::disabled`].
///
/// The model is a linear ladder over difference magnitude (see `spec/LOD.md`):
///
/// - **`diff == 0`** (a solid fill) → level 1, the coarsest rung. A uniform
///   region is fully represented by a tiny payload the terminal scales up,
///   which is the whole win — terminal decode is the bottleneck.
/// - **`diff > color_space − dead_zone`** → full resolution (`res_levels`). A
///   jump that large is guaranteed real detail, never downscaled.
/// - **in between** → graded linearly. The dead-zone reserves the top of the
///   color space so that a high-but-not-quite-max diff (delicate detail) is
///   preserved at full res rather than clipped short.
///
/// `color_space = 255` (8-bit channel); `diff = max(|ΔR|, |ΔG|, |ΔB|)`.
#[derive(Debug, Clone, Copy)]
pub struct DetailConfig {
    /// Run adaptive ceilings. `false` (classic) has every block target the full
    /// source resolution. Defaults to `true`.
    pub enabled: bool,
    /// The flat band at the top of the color space (`0..255`) that is reserved
    /// so fine differences (a jump that is "high, but not quite high enough")
    /// are preserved at full resolution. Neighbor differences inside this band
    /// (i.e. `diff > color_space − dead_zone`) force the block to full res;
    /// differences below it are graded linearly down to the coarsest level at
    /// `diff == 0`.
    pub dead_zone: u8,
}

impl DetailConfig {
    /// Classic behavior: no adaptive classification; every block targets the
    /// full source resolution.
    pub const fn disabled() -> Self {
        DetailConfig {
            enabled: false,
            dead_zone: 32,
        }
    }

    /// Adaptive classification (on by default) with the default tuning knobs.
    pub const fn enabled() -> Self {
        DetailConfig {
            enabled: true,
            ..DetailConfig::disabled()
        }
    }
}

impl Default for DetailConfig {
    fn default() -> Self {
        // Adaptive by default: solid/rough classification is the whole point.
        DetailConfig::enabled()
    }
}

/// How much detail a block's content carries, driving its resolution ceiling.
#[derive(Debug, Clone, Copy, PartialEq)]
struct BlockDetail {
    /// True when every pixel is identical (a solid fill) — the `diff == 0`
    /// case that caps at the coarsest rung.
    solid: bool,
    /// The largest difference (per the channel metric) between any neighboring
    /// pixel pair in the block.
    max_diff: u8,
}

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
    /// Detail-adaptive resolution targeting (ceilings and classification).
    detail: DetailConfig,
    /// Debug: tint each transmitted block's payload red in proportion to how
    /// far below the original resolution it is. Full res (the original) is
    /// untouched; lower resolution levels are redder, the coarsest most. Visually
    /// proves which blocks were degraded, not the crisp ones.
    debug_lod: bool,
    /// Last transmitted full-resolution content of each block, `None` before
    /// first sight of the block. Comparing this against the current full-res
    /// content is the change signal; it doubles as the stored "hash".
    prev_full: Vec<Option<Bytes>>,
    /// Per-block content class, used to pick each block's resolution ceiling.
    block_class: Vec<BlockDetail>,
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
    /// `detail` governs adaptive resolution ceilings (see [`DetailConfig`]);
    /// [`DetailConfig::disabled`] keeps the classic always-full ladder.
    /// `debug_lod` tints block payloads by their resolution level (see the
    /// field doc).
    pub fn new(cols: u32, rows: u32, res_levels: u32, detail: DetailConfig, debug_lod: bool) -> Self {
        let res_levels = if res_levels == 0 { DEFAULT_RES_LEVELS } else { res_levels.max(1) };
        BlockGrid {
            cols: cols.max(1),
            rows: rows.max(1),
            bw: 0,
            bh: 0,
            res_levels,
            detail,
            debug_lod,
            prev_full: Vec::new(),
            block_class: Vec::new(),
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
            self.block_class.clear();
            self.block_class.resize(n, BlockDetail { solid: false, max_diff: 0 });
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

                // Measure the block's detail to pick its resolution ceiling. When
                // adaptive is disabled, force the full-res case (max_diff pinned
                // to the top of the space) so every block targets full res.
                let detail = if self.detail.enabled {
                    classify_block(&cur, bw, bh, bpp)
                } else {
                    BlockDetail { solid: false, max_diff: 255 }
                };
                self.block_class[idx] = detail;
                // Ceiling: the highest ladder rung this block may climb to,
                // computed from its largest neighbor difference.
                //
                //   diff == 0 (solid)          -> level 1
                //   0 < diff <= S - dead_zone   -> round(diff / ((S - dead_zone) / res_levels))
                //   diff > S - dead_zone        -> res_levels  (delicate detail preserved)
                //   (S = 255, an 8-bit channel)
                let space = 255u32;
                let dz = u32::from(self.detail.dead_zone).min(space);
                let ladder = space.saturating_sub(dz); // color space below the dead-zone
                let max_level: u8 = if detail.solid {
                    1
                } else {
                    let d = detail.max_diff as u32;
                    if d > ladder {
                        self.res_levels.min(255) as u8
                    } else if ladder == 0 {
                        self.res_levels.min(255) as u8
                    } else {
                        let steps = self.res_levels.max(1);
                        // d / ladder in [0,1], scaled to steps, clamp 1..steps.
                        let lvl = 1 + (d.saturating_mul(steps - 1)) / ladder;
                        lvl.min(steps).max(1) as u8
                    }
                };

                // Pick the resolution level to transmit, updating per-block
                // state (baseline content + shown level + class).
                let level: u8 = match &self.prev_full[idx] {
                    // First sight of this block: become the baseline and transmit
                    // at the ceiling (full res for detailed, coarsest for solid,
                    // the smooth cap for smooth).
                    None => {
                        self.prev_full[idx] = Some(cur.clone());
                        self.shown_level[idx] = max_level;
                        max_level
                    }
                    // Same content as baseline. If already shown at the ceiling,
                    // nothing to do; otherwise climb one rung toward the ceiling.
                    Some(p) if p.as_ref() == cur.as_ref() => {
                        if self.shown_level[idx] >= max_level {
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
                // Debug LOD tint: push red into the payload in proportion to how
                // far below the original resolution this block is. Full res
                // (level == res_levels) is untouched; the coarsest level
                // (level 1, most downscaled) is most red. This flags the blocks
                // that were degraded, not the crisp ones.
                let data = if self.debug_lod {
                    let below = self.res_levels.saturating_sub(level as u32);
                    let amt = if self.res_levels > 1 {
                        (below * 255 / (self.res_levels - 1)) as u8
                    } else {
                        0
                    };
                    tint_red(&data, bpp, amt)
                } else {
                    data
                };

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

/// Measure `data` (an `w x h` image of `bpp`-byte pixels) for resolution
/// targeting:
///
/// - Sets [`BlockDetail::solid`] when every pixel is identical and
///   [`BlockDetail::max_diff`] to 0.
/// - Else scans east + south neighbors and sets [`BlockDetail::max_diff`] to
///   the largest single-channel difference (`max(|ΔR|,|ΔG|,|ΔB|)`) found.
///   O(pixels) — `W*(H-1) + (W-1)*H` pairs.
fn classify_block(data: &[u8], w: u32, h: u32, bpp: usize) -> BlockDetail {
    // Solid check: compare every pixel to the first.
    let first = &data[..bpp];
    let solid = data.chunks_exact(bpp).all(|px| px == first);
    if solid {
        return BlockDetail { solid: true, max_diff: 0 };
    }
    // Edge sweep over east + south neighbours, tracking the largest jump.
    let at = |x: u32, y: u32| -> &[u8] {
        &data[((y * w + x) as usize) * bpp..((y * w + x) as usize) * bpp + bpp]
    };
    // Max single-channel difference between two pixels.
    let diff = |a: &[u8], b: &[u8]| -> u8 {
        let mut m = 0u8;
        for i in 0..bpp {
            let d = a[i].abs_diff(b[i]);
            if d > m {
                m = d;
            }
        }
        m
    };
    let mut max_diff = 0u8;
    for y in 0..h {
        for x in 0..w {
            let p = at(x, y);
            if x + 1 < w {
                max_diff = max_diff.max(diff(p, at(x + 1, y)));
            }
            if y + 1 < h {
                max_diff = max_diff.max(diff(p, at(x, y + 1)));
            }
        }
    }
    BlockDetail { solid: false, max_diff }
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

/// Add a red tint to every pixel of an `bpp`-byte RGBA/RGB payload: the red
/// channel is pushed up by `amount` (saturating). Green/blue (and alpha) are
/// Shift every pixel's RGB toward red by `amount`: red channel pushed up and
/// green/blue pulled down by the same amount (all saturating/clamping). This
/// works on white too — pure white becomes pink — which a red-only boost could
/// not express. Alpha is left untouched, and RGB are handled separately so
/// either `Rgb24` or `Rgba32` payloads are fine.
fn tint_red(data: &[u8], bpp: usize, amount: u8) -> Bytes {
    let mut out = data.to_vec();
    let amt = amount as i16;
    for px in out.chunks_exact_mut(bpp) {
        let r = (px[0] as i16 + amt).clamp(0, 255) as u8;
        let g = (px[1] as i16 - amt).clamp(0, 255) as u8;
        let b = (px[2] as i16 - amt).clamp(0, 255) as u8;
        px[0] = r;
        px[1] = g;
        px[2] = b;
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
        let mut g = BlockGrid::new(4, 4, 4, DetailConfig::disabled(), false); // 4x4 = 16 blocks, 4 res levels
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
        let mut g = BlockGrid::new(4, 4, 4, DetailConfig::disabled(), false);
        let f = solid(80, 60, [9, 9, 9, 255]);
        g.diff(&f, false, 0);
        let again = g.diff(&f, false, 1);
        assert!(again.is_empty());
    }

    #[test]
    fn changing_one_block_emits_only_it_at_lowest_level() {
        let mut g = BlockGrid::new(4, 4, 4, DetailConfig::disabled(), false);
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
        let mut g = BlockGrid::new(1, 1, 4, DetailConfig::disabled(), false); // single block over the whole frame
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
        let mut g = BlockGrid::new(4, 4, 4, DetailConfig::disabled(), false);
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

    #[test]
    fn solid_tile_targets_lowest_resolution_under_adaptive() {
        // Correctness of the adaptive classifier: a uniform block has no detail
        // to sharpen, so it must land at the coarsest rung (level 1, the
        // smallest payload), not the full resolution.
        let mut g = BlockGrid::new(1, 1, 4, DetailConfig::enabled(), false);
        let f = solid(40, 40, [7, 7, 7, 255]);
        let first = g.diff(&f, false, 0);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].level, 1, "solid block must ceiling at the coarsest level");
        assert_eq!(first[0].payload_width, 10); // 40 / factor(4)
        assert_eq!(first[0].payload_height, 10);
        // Same solid content: already at ceiling, stays quiet.
        assert!(g.diff(&f, false, 1).is_empty());
    }

    #[test]
    fn detailed_tile_still_targets_full_resolution() {
        // A block with rough edges (an edge on every sampled pair) is classified
        // Detailed and must still climb to the full source resolution.
        let mut g = BlockGrid::new(1, 1, 4, DetailConfig::enabled(), false);
        // Build a checkerboard: alternating black/white pixels -> max edges.
        let w = 16u32;
        let mut px = vec![0u8; (w * w) as usize * 4];
        for r in 0..w {
            for c in 0..w {
                let v = if (r + c) % 2 == 0 { 255u8 } else { 0u8 };
                let b = (r * w + c) as usize * 4;
                px[b..b + 4].copy_from_slice(&[v, v, v, 255]);
            }
        }
        let f = Frame::full(meta(w, w), Bytes::from(px));
        let first = g.diff(&f, false, 0);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].level, 4, "detailed block must ceiling at full resolution");
        assert_eq!(first[0].payload_width, 16);
        assert_eq!(first[0].payload_height, 16);
    }

    #[test]
    fn debug_lod_tints_degraded_blocks_red_but_leaves_full_res_untinted() {
        // debug_lod must push red into payloads in proportion to how far below
        // the original resolution the block is: full res (level==res_levels)
        // untouched, coarsest (level 1) most red.
        let w = 16u32;
        let brightness = 100u8;

        // Full-res emit (level 4): must be completely untinted.
        let mut g = BlockGrid::new(1, 1, 4, DetailConfig::disabled(), true);
        let mut px = vec![0u8; (w * w) as usize * 4];
        for p in px.chunks_exact_mut(4) {
            p.copy_from_slice(&[brightness, brightness, brightness, 255]);
        }
        let blocks = g.diff(&Frame::full(meta(w, w), Bytes::from(px)), false, 0);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].level, 4, "disabled detail -> full res on first sight");
        assert_eq!(blocks[0].data[0], brightness, "full res must be untinted");
        assert_eq!(blocks[0].data[1], brightness);
        assert_eq!(blocks[0].data[2], brightness);

        // Coarsest emit (level 1): most red. below = 4-1 = 3, amt = 3*255/3 = 255.
        // Brightness 100 -> R saturates to 255, G and B clamp to 0.
        let mut g2 = BlockGrid::new(1, 1, 4, DetailConfig::enabled(), true);
        let s2 = g2.diff(&solid(16, 16, [brightness, brightness, brightness, 255]), false, 0);
        assert_eq!(s2.len(), 1);
        assert_eq!(s2[0].level, 1, "solid block under adaptive ceilings at level 1");
        assert_eq!(s2[0].data[0], 255, "red pushed to max");
        assert_eq!(s2[0].data[1], 0, "green pulled to 0");
        assert_eq!(s2[0].data[2], 0, "blue pulled to 0");
    }

    #[test]
    fn debug_lod_tints_white_pink_instead_of_staying_white() {
        // Regression: pure white (all 255) must tint to pink, not stay white.
        // A red-only boost was a no-op on white; shifting G/B down fixes it.
        let mut g = BlockGrid::new(1, 1, 4, DetailConfig::enabled(), true);
        let blk = g.diff(&solid(16, 16, [255, 255, 255, 255]), false, 0);
        assert_eq!(blk.len(), 1);
        assert_eq!(blk[0].level, 1, "solid white caps at level 1");
        // amt = 255; R stays 255, G/B drop to 0 (pure red).
        assert_eq!(blk[0].data[0], 255);
        assert_eq!(blk[0].data[1], 0);
        assert_eq!(blk[0].data[2], 0);
    }

    #[test]
    fn lod_ladder_grades_ceiling_by_max_diff_and_dead_zone() {
        // Dead-zone model (spec/LOD.md): the ceiling is a linear ladder over
        // the largest neighbor diff, using the grid's res_levels.
        // S = 255, but the model grades over ladder = S - dead_zone with all 4
        // steps. diff = S (full jump) "> dead_zone"? We use strict: d > ladder
        // means d in the top dead_zone band -> full res. With dead_zone 0 the
        // top band is empty, so full res only at d == ladder.
        let w = 16u32;
        let make = |val| -> Bytes {
            let mut px = vec![0u8; (w * w) as usize * 4];
            for p in px.chunks_exact_mut(4) {
                p.copy_from_slice(&[val, val, val, 255]);
            }
            Bytes::from(px)
        };

        // dead_zone 0, res_levels 4, ladder = 255. d == 255 -> NOT > 255, so
        // level = 1 + 255*3/255 = 4. Full res for a max contrast split.
        let d0 = DetailConfig { enabled: true, dead_zone: 0 };
        let mut g = BlockGrid::new(1, 1, 4, d0, false);
        // Half black/half white: max neighbor diff is 255 at the seam.
        let mut split = vec![0u8; (w * w) as usize * 4];
        for r in 0..w {
            for c in 0..w {
                let v = if c < 8 { 0u8 } else { 255u8 };
                let b = (r * w + c) as usize * 4;
                split[b..b + 4].copy_from_slice(&[v, v, v, 255]);
            }
        }
        let blk = g.diff(&Frame::full(meta(w, w), Bytes::from(split)), false, 0);
        assert_eq!(blk.len(), 1);
        assert_eq!(blk[0].level, 4, "max diff must grade to full res");

        // A small fixed diff (say 64) with dead_zone 0 grades to
        // 1 + 64*3/255 = 1 (64*3=192, /255 = 0) -> level 1. So subtle diff at
        // res_levels 4 stays lowest. With dead_zone small this is still 1.
        let mut g2 = BlockGrid::new(1, 1, 4, d0, false);
        // Two-tone block: 0 and 64 are the only colors => max_diff 64.
        let mut subtle = vec![0u8; (w * w) as usize * 4];
        for r in 0..w {
            for c in 0..w {
                let v = if c < 8 { 0u8 } else { 64u8 };
                let b = (r * w + c) as usize * 4;
                subtle[b..b + 4].copy_from_slice(&[v, v, v, 255]);
            }
        }
        let blk2 = g2.diff(&Frame::full(meta(w, w), Bytes::from(subtle)), false, 0);
        assert_eq!(blk2.len(), 1);
        assert!(blk2[0].level <= 2, "a low diff should grade low, got level {}", blk2[0].level);

        // Solid -> level 1 always.
        let mut g3 = BlockGrid::new(1, 1, 4, d0, false);
        let s = g3.diff(&Frame::full(meta(w, w), make(200)), false, 0);
        assert_eq!(s[0].level, 1, "solid always level 1");
    }
}