# Content-Addressed Tile Cache (Tile Reuse)

## Problem

In block mode, the same content frequently appears across many tiles — a solid
UI background, identical glyph cells, or a repeated logo. Today each changed
tile is re-transmitted in full every time its slot differs from its baseline
(`crates/tgp/src/encoder.rs`), even if the exact same pixel payload was already
uploaded to the terminal moments earlier under a different id. That re-wastes
the terminal's decodes and our bytes for content the terminal already has.

## Model: a global hash → tile map

Maintain a **content-addressed cache** on the encoder side that mirrors the
terminal's stored-image namespace:

- A global `hash(tile payload) → image_id` map. The hash is over the exact
  bytes the terminal stores — the transmitted payload after PNG/color rounding
  — so that "two tiles are the same" means *byte-identical on the terminal*.
- When we want to schedule a tile for rendering, we *first* look it up by hash:
  - **Hit**: the terminal already holds an image with these exact bytes. We do
    **not** re-transmit; we reuse that stored id. All we send is a **place**
    command (`a=p`) at the block's on-screen cell: move the cursor to the
    block's top-left cell and place the existing image id there.
  - **Miss**: transmit the tile as usual, assign it a fresh id, and record
    `hash → id` so future identical tiles reuse it.
- **No tile is ever sent twice while it is live.** A cached id is only
  re-transmitted if it has been evicted from the terminal (see lifecycle).

## Why a payload hash instead of comparing to per-slot baselines

Per-slot baselines (`BlockGrid.prev_full`) only tell us "did *this* slot
change"; they cannot tell us "is this the *same bytes the terminal already has
under another slot*". A global content hash is the only way to dedupe across
slots and across frames, and it is cheap: hash the downscaled+rounded payload
array once per tile rather than comparing every candidate pair.

The hash covers **the stored bytes**: for PNG (`png=true`) that is the encoded
PNG (after `color_bits` rounding), for raw it is the rounded RGBA/RGB payload.
Different resolution levels are different payloads and therefore different
cache entries (a coarse tile and a full-res tile of the same region never
collide).

## Lifecycle and the "never sent twice" guarantee

The terminal's image store is bounded. Two eviction rules keep us from ever
re-using a dead id or leaking entries:

1. **Refcounting.** A map `image_id → number of on-screen placements currently
   using it`. When a block slot is re-rendered with different bytes, we release
   its old id (refcount--); when it drops to 0 the terminal still owns the
   image but no screen placement references it — a candidate for eviction, but
   *not* deleted immediately, because a future identical tile in another slot
   could reuse it for free.
2. **Bound the store.** When the cache grows past a per-grid budget (each entry
   is a grid-sized tile), evict least-recently-used entries: place a delete
   (`a=d`) for the id, remove it from `hash → id` and the refcount map, and mark
   the refcount dropped. A tile that is evicted may be re-transmitted later if
   it appears again (that is the only path that re-sends previously-seen bytes).

So the guarantee is: **at any instant, bytes that are cached are never
transmitted twice**; bytes that have been evicted may be re-sent because the
terminal no longer has them.

## On-screen dedupe

Two identical tiles rendered at once share one stored image id; each slot holds
a placement referencing it. Refcounts track all of them. When we place a reuse
ourselves we do not delete: the terminal repurposes the id for the new
placement and the old slot's placement is superseded by the same bytes.

## Disabling

`--no-tile-reuse` turns the cache off entirely: every changed tile is
re-transmitted under a fresh id, as before.

## Behavior summary

- `hash → id` map, hash over the exact stored bytes.
- Render a tile: lookup by hash → hit = place-only `a=p`; miss = transmit +
  record hash.
- Refcount each id by live placements; LRU-evict past a bounded store so the
  terminal image store never overflows and no id is ever reused after eviction
  without a fresh transmit.
- Never transmit the same live bytes twice.
- On by default; `--no-tile-reuse` disables.

## Hit-rate telemetry

Count, over the session, how many tile renders were satisfied by a cached id
(a reuse, no transmit) vs. how many had to transmit a payload. The footer
shows the accumulated reuse rate, e.g. `Tiles: 1.2k re-used · 340 sent ·
78% hit`. This is accumulated over time (not a per-frame rate), so it reflects
the steady-state benefit of dedupe.