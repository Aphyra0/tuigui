# Level-of-Detail Resolution Targeting

## Problem

The terminal's rendering/decode is the bottleneck, so we want each block of a
streamed frame transmitted at the lowest resolution that still looks correct.
The classic always-full ladder over-transmits flat content; a detail-adaptive
scheme must cap a block's resolution by how much detail it actually carries.

## Model

Two knobs drive a block's ceiling:

- `--lod-dead-zone` (`offset`): a flat band at the *small*-difference end of
  the color space. Neighbor differences inside this band are considered
  "not that much" — delicate detail that must be preserved at full resolution.
- `--res-levels`: how many steps the remaining color space is carved into,
  i.e. how many resolution rungs a block can climb before capping.

The full color space is `S = 255` (8-bit channel). The dead-zone reserves the
top of that space; the ladder spans what is left.

For each block, measure the largest difference between any neighboring pixel
pair, collapsed to one scaler via the channel metric:

```
diff = max(|ΔR|, |ΔG|, |ΔB|)
```

Then:

- **`diff == 0`** → **level 1** (lowest res). A solid fill has no detail to
  sharpen; it is fully represented at the coarsest rung.
- **`diff > S − dead_zone`** → **level = res_levels`** (full res). A jump that
  large is guaranteed real detail, never downscaled.
- **`0 < diff ≤ S − dead_zone`** → graded linearly. With
  `step = (S − dead_zone) / res_levels`, the level is
  `level = clamp(round(diff / step), 1, res_levels)`.

The dead-zone is thus an *offset reserve at the top*: a diff that is "high, but
not quite high enough" to naturally land on `res_levels` is pulled up so
borderline-high detail still resolves to full resolution rather than being
clipped short. Put another way, fine differences get full fidelity; only
coarse jumps that survive averaging are downsized, and they are downsized
linearly with the size of the jump.

## Summary of behavior

| largest neighbor diff `d` | level |
|---|---|
| `d == 0` (solid) | 1 (coarsest) |
| `0 < d ≤ S − dead_zone` | `round(d / ((S − dead_zone) / res_levels))`, clamped 1..=res_levels |
| `d > S − dead_zone` | `res_levels` (full) |

## Channel metric

`diff = max(|ΔR|, |ΔG|, |ΔB|)`. The channel with the largest single-channel
jump dominates (a visible jump in any channel is visible), and the metric
stays within the `S = 255` ladder. Luminance or an RGB sum are rejected as
they blur worst-channel detail and skew the range the ladder measures.

## Open question (resolved)

The earlier design differences (color-count, top-down vs bottom-up, dead-zone
as noise-floor) are settled above: the dead-zone is a top reserve, and the
level is a linear function of the largest single-channel neighbor difference.