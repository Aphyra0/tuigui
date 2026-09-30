//! Area-weighted image resampling.
//!
//! One high-quality downscaler shared by every scaling site in the pipeline
//! (capture `--scale`, block LOD payloads, the loading layer). The previous
//! nearest-neighbor / integer box sampling alias when the scale is not an
//! exact integer, producing shimmering video and ragged text. The filter here
//! integrates a tent (bi)linear kernel over each destination pixel's exact
//! footprint in source space, so it is correct for any rational or fractional
//! ratio and degrades to an exact average for integer factors.

use bytes::Bytes;

/// Weight row: `(source start index, weights)` for one output row/column.
/// Weights are `i32` fixed-point (sum 256) so the horizontal pass accumulates
/// in `i32` without float rounding drift.
struct Row {
    start: u32,
    weights: Vec<i32>,
}

/// Precompute the filter taps mapping `src_len` -> `dst_len`.
///
/// Each destination index `i` covers the source interval `[i*src/dst,
/// (i+1)*src/dst)`; a source pixel contributes its overlap area with that
/// interval (a true box/area average, the exact downsampling filter). For an
/// integer downscale this is the plain mean of each block, for identity it is
/// exact passthrough, and fractional ratios blend both boundary pixels
/// instead of dropping one. Upscaling (interval narrower than a pixel) picks
/// the 1-2 covering pixels with linear-ish weights, avoiding nearest-neighbor
/// row duplication.
fn taps(src_len: u32, dst_len: u32) -> Vec<Row> {
    let dst_len = dst_len.max(1);
    let src_len = src_len.max(1);
    let (src, dst) = (src_len as f64, dst_len as f64);
    let scale = dst / src;
    (0..dst_len)
        .map(|i| {
            let lo = i as f64 / scale;
            let hi = (i + 1) as f64 / scale;
            let first = lo.floor().max(0.0) as u32;
            let last = hi.ceil().min(src).max(1.0) as u32 - 1;
            let footprint = (hi - lo).max(1e-9);
            let mut weights = Vec::with_capacity((last - first + 1) as usize);
            let mut norm = 0i64;
            for s in first..=last {
                let left = (s as f64).max(lo);
                let right = ((s + 1) as f64).min(hi);
                // Overlap fraction of this pixel within the destination's
                // footprint, normalized by the footprint so weights always
                // sum to 256 (downscale overlaps already sum to 1; upscale
                // overlaps sum to the footprint < 1).
                let w = (((right - left) / footprint) * 256.0).round().max(0.0) as i64;
                norm += w;
                weights.push(w as i32);
            }
            // Normalize to exactly 256 so the fixed-point passes need no
            // separate divisor. The rounding drift is spread one unit at a
            // time over the largest taps so no single tap is skewed.
            let drift = 256 - norm;
            if drift != 0 {
                let step = if drift > 0 { 1 } else { -1 };
                let mut remaining = drift.abs();
                while remaining > 0 {
                    let pick = (0..weights.len())
                        .max_by_key(|&i| weights[i])
                        .unwrap_or(0);
                    if weights[pick] == 0 {
                        break;
                    }
                    weights[pick] += step;
                    remaining -= 1;
                }
            }
            Row { start: first, weights }
        })
        .collect()
}

/// Horizontal pass: `u8` input -> normalized `i32` rows (0..=255, rounded).
/// The tap weights sum to 256, so each accumulator is divided back down here;
/// [`vert`] then re-weights rows the same way and the two passes compose.
/// `dst_w` may exceed `src_w` (upscale) — the same tent kernel handles both.
fn horz(src: &[u8], src_w: u32, dst_w: u32, bpp: usize, rows: usize) -> Vec<Vec<i32>> {
    let taps = taps(src_w, dst_w);
    let mut out = vec![vec![0i32; dst_w as usize * bpp]; rows];
    for (y, row) in out.iter_mut().enumerate() {
        let line = &src[y * src_w as usize * bpp..(y + 1) * src_w as usize * bpp];
        for (x, t) in taps.iter().enumerate() {
            let acc = &mut row[x * bpp..(x + 1) * bpp];
            for (k, &w) in t.weights.iter().enumerate() {
                if w == 0 {
                    continue;
                }
                let s = (t.start as usize + k) * bpp;
                for lane in 0..bpp {
                    acc[lane] += line[s + lane] as i32 * w;
                }
            }
            for a in acc.iter_mut() {
                *a = ((*a + 128) >> 8).clamp(0, 255);
            }
        }
    }
    out
}

/// Vertical pass: `i32` rows (scaled by 256) -> `u8` output. Accumulates the
/// full weighted sum per output pixel before a single rounding divide, so
/// partial taps never clip or overflow individually.
fn vert(rows: &[Vec<i32>], taps: &[Row], dst_h: u32, dst_w: u32, bpp: usize) -> Vec<u8> {
    let mut out = vec![0u8; (dst_w * dst_h) as usize * bpp];
    let mut acc = vec![0i32; (dst_w * dst_h) as usize * bpp];
    for (y, t) in taps.iter().enumerate() {
        for (k, &w) in t.weights.iter().enumerate() {
            if w == 0 {
                continue;
            }
            let src_row = &rows[(t.start as usize + k).min(rows.len() - 1)];
            let dst_off = y * dst_w as usize * bpp;
            for (i, &v) in src_row.iter().enumerate() {
                acc[dst_off + i] += v * w as i32;
            }
        }
    }
    for (o, &a) in out.iter_mut().zip(acc.iter()) {
        *o = ((a + 128) >> 8).clamp(0, 255) as u8;
    }
    out
}

/// Resample an image between arbitrary sizes with an area-weighted tent
/// filter. `bpp` is 3 (RGB) or 4 (RGBA); the alpha lane is weighted like any
/// other channel (non-premultiplied input/output).
///
/// Straightens out the two classic artifacts of the integer box sampler:
/// fractional `--scale` values no longer drop/duplicate rows unevenly, and
/// uneven edges (`w.div_ceil(factor)` raggedness) are gone because every
/// destination pixel integrates its exact source footprint.
pub fn resample(
    data: &[u8],
    src_w: u32,
    src_h: u32,
    bpp: usize,
    dst_w: u32,
    dst_h: u32,
) -> (Bytes, u32, u32) {
    debug_assert!(bpp == 3 || bpp == 4);
    let (dst_w, dst_h) = (dst_w.max(1), dst_h.max(1));
    if (src_w, src_h) == (dst_w, dst_h) {
        return (Bytes::copy_from_slice(data), dst_w, dst_h);
    }
    // Cheap early out: 1x1 source smears to a flat fill.
    if src_w == 0 || src_h == 0 {
        return (Bytes::copy_from_slice(data), dst_w, dst_h);
    }
    let rows = horz(data, src_w, dst_w, bpp, src_h as usize);
    let vtaps = taps(src_h, dst_h);
    let out = vert(&rows, &vtaps, dst_h, dst_w, bpp);
    (Bytes::from(out), dst_w, dst_h)
}

/// Resample where the caller already knows the target bpp from `data.len()`.
pub fn resample_auto(data: &[u8], src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> (Bytes, u32, u32) {
    let bpp = if data.len() == (src_w as usize) * (src_h as usize) * 4 { 4 } else { 3 };
    resample(data, src_w, src_h, bpp, dst_w, dst_h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_factor_is_exact_average() {
        // 2x2 -> 1x1: all four pixels must average.
        let data = [0u8, 0, 0, 255, 100, 0, 0, 255, 200, 0, 0, 255, 255, 0, 0, 255];
        let (out, w, h) = resample(&data, 2, 2, 4, 1, 1);
        assert_eq!((w, h), (1, 1));
        // (0+100+200+255)/4 = 138.75 -> 139 (proper rounding).
        assert_eq!(&out[..4], &[139, 0, 0, 255]);
    }

    #[test]
    fn identity_is_byte_exact() {
        let data: Vec<u8> = (0..16u8).collect();
        let (out, w, h) = resample(&data, 2, 2, 4, 2, 2);
        assert_eq!((w, h), (2, 2));
        assert_eq!(&out[..], &data[..]);
    }

    #[test]
    fn fractional_ratio_averages_both_source_pixels() {
        // 3px wide -> 2px wide. The old sampler dropped one column outright;
        // the tent kernel must blend both endpoints in each output pixel.
        // Columns: 0, 255, 0 (single row, RGB).
        let data = [0u8, 0, 0, 255, 255, 255, 0, 0, 0];
        let (out, w, h) = resample(&data, 3, 1, 3, 2, 1);
        assert_eq!((w, h), (2, 1));
        // First output covers ~[0,1.5): mostly col0 plus some col1.
        assert!(out[0] > 0 && out[0] < 255, "left edge must blend, got {}", out[0]);
        // Second output covers ~[1.5,3): some col1 plus mostly col2.
        assert!(out[3] > 0 && out[3] < 255, "right edge must blend, got {}", out[3]);
    }

    #[test]
    fn upscales_smaller_than_integer_without_dropping_rows() {
        // 2 -> 3 rows: no row may repeat wholesale; interior output must
        // interpolate. Destination rows cover [0,0.667), [0.667,1.333),
        // [1.333,2): the middle row blends both source rows 50/50 (-> 128),
        // while the edge rows fall wholly inside one source row.
        let data = [0u8, 0, 0, 255, 255, 255];
        let (out, w, h) = resample(&data, 1, 2, 3, 1, 3);
        assert_eq!((w, h), (1, 3));
        assert_eq!(out[0], 0);
        assert!(out[3] > 0 && out[3] < 255, "middle row must interpolate, got {}", out[3]);
        assert_eq!(out[6], 255);
    }

    #[test]
    fn rgba_alpha_is_weighted_like_color() {
        let data = [0u8, 0, 0, 0, 0, 0, 0, 255];
        let (out, _, _) = resample(&data, 2, 1, 4, 1, 1);
        // (0+255)/2 = 127.5 -> 128 with proper rounding.
        assert_eq!(out[3], 128, "alpha must average like any channel");
    }
}
