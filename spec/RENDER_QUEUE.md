# Rendering Queue with a Per-Frame Pixel Budget

## Problem

In block mode the encoder currently retransmits *every* changed block as soon
as it is diffed (`crates/tgp/src/encoder.rs`): `grid.diff()` returns a
`Vec<MicroBlock>` and each is emitted inline that same frame as its own
transmit+place. A frame with many detailed (high-resolution) changed blocks
therefore uploads a burst of pixels with no ceiling. When the terminal is the
bottleneck, we want to cap how many pixels a single frame hands the decoder and
let the excess spill over to the next frame(s) — a classic backpressure queue.

## Model: a rendering queue drained by a pixel budget

Everything that wants to upload a tile is **enqueued** rather than emitted
inline. Each frame, after the block diff, we drain that queue and transmit
tiles from the front until we reach a per-frame **pixel budget**; everything
that does not fit stays queued for the next frame.

- **The queue is keyed by grid slot.** A tile is identified by its `(row, col)`
  grid position. When the dispatcher wants to schedule a render for a slot that
  is already queued, it **replaces** the queued entry in place (keeping the
  newest payload) instead of appending a duplicate. Stale work is never
  rendered.
- **The drain is measured in pixels, not elements.** The cost of a tile is its
  transmitted payload area (`payload_width × payload_height`), which is a
  function of the resolution level it settled at. Higher-level (detailed) tiles
  cost far more of the budget than coarsse ones.

### The pixel budget

Per frame, the queue is allowed to drain at most:

```
budget = pixels of one full screen at the lowest target resolution
```

which the user specified equivalently as:

```
budget = (pixels of one tile at the lowest resolution level) × (number of all tiles on screen)
```

The "lowest target resolution" is the coarsest rung of the block ladder — the
resolution a tile is transmitted at level `1` (/ its largest downscale factor).
So the budget is the whole frame's worth of pixels at the coarsest resolution,
regardless of how spiky the actual changed content is. In practice:

- A frame where every changed tile is at level 1 (coarse) fits the whole screen
  in one budget — its drain completes normally.
- A frame with high-resolution (detailed) changed tiles consumes the budget much
  faster, so fewer of them drain; the rest stay queued and drain over the next
  few frames.

### Flow (each frame)

1. `grid.diff()` produces this frame's changed tiles.
2. **Enqueue** each changed tile: if a newer update for the same slot is already
   queued, replace it; otherwise append.
3. **Drain** from the front of the queue, accumulating each tile's payload pixel
   area, until adding the next tile would exceed the budget. Transmit the
   drained tiles (transmit+place) and free the slot ids they displace; leave the
   rest for the next frame.

Because `diff()`'s own state machine (baseline re-store and rung-climb) advances
independent of actual transmission, a tile that stays queued can be
re-superseded by a sharper version the next frame — the terminal only ever sees
the final drained payload, never the intermediate rungs.

## Behavior summary

- One tile → find its slot in the queue → replace, or append if absent.
- Drain stops when the accumulated `payload_width × payload_height` would exceed
  the frame budget.
- Budget = the whole frame's pixels at the coarsest (lowest) resolution level.
- Undrained tiles persist across frames.
- On a resize (geometry change) the queue is cleared: old placements are invalid.

## Decisions locked

- Budget denominator is the **lowest** resolution level (coarsest rung), not the
  full source resolution or any middle rung.
- Budget counts **payload pixels** (`payload_width × payload_height`), i.e. the
  resolution the tile is actually being transmitted at, not its full-res extent.
- Deduplication happens by **grid slot** (`(row, col)`).