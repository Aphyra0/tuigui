# SURFACE — anchoring images by a cell signature, not by unicode placeholders

Status: plan. Nothing built.

This replaces the placeholder mechanism in `crates/tui/src/placeholder.rs` and the
byte-stream proxy in `crates/tui/src/proxy.rs`.

## Why

The current anchor is kitty's **unicode placeholder**: the client fills its pane
with `❋`, the proxy rewrites every one of those cells into `U+10EEEE` plus
combining diacritics carrying the image id, row and column, and the terminal
resolves the cells against a *virtual* placement.

That has three problems:

1. **It needs unicode placeholders.** A terminal without them shows a screen of
   `U+10EEEE` boxes, so the picture is not merely unplaced, the pane is garbage.
2. **The rewrite has to parse the stream.** It tracks the cursor across escape
   sequences and per-character state to know each cell's row and column, and it
   keeps a carry across reads for characters split between them. That parser is
   the fragile part: one escape form it does not model and every later row is
   anchored to the wrong place.
3. **The anchor is the client's fill.** The client has to know to draw `❋` at
   all, which is a protocol only this pair shares.

The replacement keeps an anchor that is *visible in the cells themselves*: a
signature the session can look for in a buffer, with no stream parsing and no
assumption about placeholder support.

## The decision

A **SurfaceId** is a cell signature: a symbol, a foreground colour and a
background colour, all three chosen at random per window when the window is
admitted. It is not an id number that cells refer to; the cell *is* the id.

```text
  SurfaceId { symbol: char, fg: Color, bg: Color }
```

Random in all three, so a window's cells are recognisable on screen and cannot
collide with ordinary content by accident, and so the extent can be found by
matching cells rather than by being told where the window is.

### Where it travels

The signature is decided by the client's compositor (it is the side that admits
windows) and sent up the link with the window:

```text
  Message::WindowCreated { id, client, size, surface: SurfaceId }
```

The server records it on its window record. Nothing else about placement is
negotiated: the server finds the cells, the server places the image.

### How the proxy changes

Today the proxy forwards the command's bytes and rewrites them on the way out,
which means it never holds a picture of the screen. That is what has to change.

**The model is `vt100`, not ratatui.** `vt100::Parser` is a real terminal state
machine: `process(&[u8])` consumes arbitrary bytes and maintains a screen grid,
cursor, scroll region, alternate screen, SGR, scrollback and input modes.
`Screen` exposes `cell(row, col) -> Option<&Cell>`, `cursor_position()`,
`alternate_screen()`, `contents_diff(prev)`, `state_diff(prev)`.

Ownership, which is what makes it simple: **the child's bytes go to `Parser` and
nowhere else**, and the terminal receives only what we render. There is no
parallel replay of the child's escapes, so there can be no divergence between the
model and the screen.

**The screen is persistent, and it is the proxy's whole state.** `Parser` owns it
and keeps it across reads and across frames: `process()` mutates it in place, and
`screen()` hands out a borrow of the current state. Nothing is rebuilt per frame.
A frame is a *read* of the screen, not a new one.

That settles the strip question: the model keeps the signature cells, and the
strip happens only in the emitted copy. This is not a preference, it is forced by
the API — verified against the 0.18.1 source:

| item | visibility |
| --- | --- |
| `Parser::screen()`, `screen_mut()` | public |
| `Screen::set_size`, `set_scrollback`, `clear_scrollback` | public |
| `Screen::grid_mut` | private |
| `Cell::set`, `clear`, `set_contents` | `pub(crate)` |
| `Cell::contents`, `fgcolor`, `bgcolor`, `bold`…`inverse` | public |

So `&mut Screen` exists but no cell can be written through it. And `Parser: Write`
is the wrong direction: `write()` feeds bytes *into* the model and `flush()` is
`Ok(())` — it is an input sink, not an escape emitter.

- per frame, the session borrows `&Screen`, walks the grid, records each
  `SurfaceId`'s rectangle, and places the images,
- the emission is **ours**, cell by cell: `MoveTo(col, row)` plus the cell's SGR
  and its text, with the signature cells written as a blank cell in the pane's
  background. Every getter it needs is public; the writer is the only thing we own.
- the diff is against **what we last emitted**, not against the model. `previous`
  is a second `Parser`/`Screen` pair advanced by feeding it our own rendered bytes,
  so a skipped cell is a cell that stayed blank in `previous` too and is therefore
  not re-emitted every frame.

`contents_diff`/`contents_formatted` are still useful as the fallback for a single
frame that must resynchronise in full (the passthrough case below), but they cannot
express the strip, which is why the normal path is our own emission.

Cost of the alternative: stripping *in* the model needs `Cell::set` and
`Screen::grid_mut` made public, which means a fork or a `[patch]`, and pins the
crate to a patched dependency. Not worth it for a writer we need anyway.

### Passthrough

`vt100` models almost everything, but not everything, and what it does not model
it reports rather than silently dropping: `Callbacks::unhandled_char`,
`unhandled_control`, `unhandled_escape`, `unhandled_csi`, `unhandled_osc`, plus
`audible_bell`, `set_window_title`, `copy_to_clipboard`, `resize`, `on_scroll`.
Those are the sequences we owe the terminal.

The rule:

- **Modelled spans** are consumed and rendered. Nothing of them is forwarded.
- **Unmodelled spans** are forwarded to the terminal **byte for byte**, and the
  frame that forwards one does a **full repaint** afterwards rather than a diff,
  because the terminal and the model may now disagree about something the model
  does not track. A full repaint resynchronises the screen; a diff would propagate
  the divergence.
- **Unknown is forwarded**, never dropped: a swallowed query is a program waiting
  forever for a reply.

Two consequences worth stating plainly:

1. **Cheap frames are diffs, passthrough frames are repaints.** A query or a
   title change costs a screen of escapes. That is the price of not implementing
   those sequences ourselves, and it is bounded because they are rare.
2. **The input lane is demultiplexed.** Our image acknowledgements and the child's
   terminal replies arrive on one stream. Each transmit carries a unique marker, so
   only our own acks are swallowed; everything else goes back to the child verbatim.

Rendering needs no ratatui and no crossterm buffer: the grid is read cell by cell
and emitted as escapes. `vt100`'s own `contents_formatted`/`contents_diff` already
produce exactly that, so the only reason to walk cells at all is to find the
`SurfaceId`s and to leave their cells out.

### How the session places an image

For each frame, per window:

1. Walk the screen grid from the top left to the bottom right.
2. The first cell whose `(contents, fgcolor, bgcolor)` equals a known `SurfaceId`
   is the origin.
3. The last such cell, read as the most bottom-right one, gives the extent:
   `width = last.col - first.col + 1`, `height = last.row - first.row + 1`.
4. Place the image at that cell rectangle, in pixels, using the terminal's cell
   size.
5. Place it in the behind-everything band, ordered against other windows by its
   own `z` (see [Z-order](#z-order-answered)).

A window with no matching cells is not placed this frame: it is off screen,
scrolled away, or its process has not drawn yet, and all three mean the same thing
to the placement path.

## What this removes

- `crates/tui/src/placeholder.rs` and its cursor-tracking rewrite, in full.
- The per-cell diacritic encoding, and with it the dependence on a terminal
  supporting unicode placeholders.
- The virtual placement (`U=1`): a placement with a real position makes the
  anchor the cells' own rectangle, which is what the signature already gives.
- The client's `❋` fill as a protocol: a window fills its cells with its
  signature, and nothing else in the client has to know.

## What this adds

- `vt100` as a dependency of `tui`: the terminal state machine the proxy drives.
- `SurfaceId` in `wire` (it travels on the link), and a random constructor.
- A register of `SurfaceId → window` on the server's session.
- A render step in the proxy: consume the child's bytes into the model, hand the
  `Screen` to the session, write what comes back.

## Open questions

- **Occlusion.** A window whose cells are partly covered by another window yields
  a bounding box that also covers the covering content. Placing an image in that
  box puts the hidden part of the picture over something else. Options: shrink to
  the largest contiguous run per row (a ragged placement is not expressible), or
  treat any interruption as "not placed".
- **Scrolling.** A screen model makes scrollback cheap to hold, but the buffer is
  the visible screen. An image whose anchor scrolled off has no cells, so it is
  not placed; that is correct, and it is worth stating because the old path had
  no notion of it at all.
- **Frame rate.** Rebuilding the whole screen per frame costs more than forwarding
  a chunk. Whether the proxy emits only changed rows, and whether the image
  placement needs a redraw at all when nothing moved, is a measurement.
- **Cell size.** The image is placed in pixels at a cell rectangle, so the
  session needs the terminal's cell size in pixels. It is already read in `gui`
  for the output size; the server has its own copy for the placement maths.
- **Signature collisions.** Random in three dimensions makes them vanishingly
  unlikely, but "unlikely" is not "impossible", so a signature that matches a
  cell the command actually drew is a real (if remote) failure mode. A check
  against the buffer at admission time, or including the symbol's combining
  variants in the search, is the mitigation.

## Z-order: answered

The protocol gives the headroom, and the answer is in
`docs/kitty-graphics-protocol.rst:554-562`:

> Negative z-index values mean that the images will be drawn under the text. …
> Negative z-index values below `INT32_MIN/2` (-1,073,741,824) will be drawn
> under cells with non-default background colors.

So there are two bands below the text, and they nest:

```text
   z ≥ 0                  over the text
   INT32_MIN/2 ≤ z < 0    under the text, over cell backgrounds
   z < INT32_MIN/2        under the text AND under cell backgrounds
```

**Both requirements are satisfiable, and they use `z` for both.** All windows are
placed in the lower band (`z < INT32_MIN/2`), so every one of them is behind the
text and behind the backgrounds. The order *between* windows is `z` as well: each
window gets its own z inside that band, larger meaning nearer the front. The id
tiebreak (`lower id lower`) only decides between two placements that have the
**same** z, which this design never produces, so it plays no part.

```text
   window at the back   z = INT32_MIN        (lowest)
   window in front      z = INT32_MIN/2 - 1  (highest still in the lower band)
```

The band is `2^30` wide — over a billion distinct values between the two
boundaries — so a z per window is not a budget problem, and no id allocation
scheme is needed to carry stacking order. Stacking a window is a new `z` on its
placement; the image id stays what it always was, a name for the pixels.

The bands are also the answer to the occlusion question's first half: because the
picture sits under the backgrounds, an overlapping placement cannot cover another
window's text or background — and between two windows the higher `z` is in front,
which is the correct outcome rather than a compromise.

Open remainder: a partly occluded window still gets one rectangular placement, so
its hidden picture is drawn where the occluder's cells are. It cannot show over
the occluder's *text* (that is above), but it will show in the occluder's
background-coloured cells if those are default-coloured. Shrinking to the largest
contiguous run per row remains the only way to avoid that, at the cost of ragged
edges; it is not needed until two windows actually overlap.
