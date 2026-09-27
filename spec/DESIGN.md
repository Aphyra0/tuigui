# tuigui — GUI apps in the terminal

A program that runs real GUI applications on a headless display server and streams
their windows into a terminal at native resolution, using the kitty graphics
protocol (TGP).

## Problem statement

Terminals are text grids, but modern ones can display exact pixels through TGP.
The gap is glue: nothing today combines a headless display server, damage-based
capture, and a TGP delta emitter into one tool. tuigui is that tool.

Goal: run any X11 or Wayland app; see it pixel-accurate at terminal resolution;
interact with it from the terminal over SSH at interface-class bandwidth
(1–5 Mbit/s) with no external dependencies on the client beyond a TGP terminal.

## Architecture

```
                 ┌─────────────────────────────────────────────┐
                 │ host process                                │
 app ──────►     │                                             │
 (X11/Wayland)   │  display head ──► capture ──► tile store ──►│──► TGP emitter ──► PTY
                 │  (Xvfb / wlroots   (damage-driven)          │    (tiles, blits,      stdout
 input ◄─────────│  headless)       pixel diffs in 64px tiles  │     pacing)            terminal
 (PTX → XTEST /  │                                             │
  virtual seat)  └─────────────────────────────────────────────┘
```

Pipeline stages:

1. **Display head** — app runs against a private headless display server.
   - X11: `Xvfb`, private `:N`.
   - Wayland: headless wlroots backend (headless output, `wlr-screencopy`).
2. **Capture with damage** — never full screenshots; subscribe to damage events.
   - X11: `XDamage` + `MIT-SHM` pixmaps for zero-copy reads.
   - Wayland: `wlr-screencopy` damage events.
   - Window-level capture with offset bookkeeping so a moved window shifts its
     tile map instead of invalidating everything.
3. **Tile store** — the window framebuffer divided into 64×64 px tiles
   (grid-snapped). Each tile keeps: last transmitted copy, a small XOR history,
   and a dirty counter. Scroll support: keep a 64px granular scroll history per
   tile column so a scrolled terminal converges in ~1–2 frames.
4. **TGP emitter** — converts tile decisions into escape sequences, gated by
   protocol pacing (see Scheduling).

## Pixel channels

Each window is one TGP image, from one 32-bit RGBA framebuffer.

- **Surface lifecycle**: app create window → 1 image id, `c=,r=` sized to its
  cell rect. Window move → re-place with same `(i,p)`. Window resize →
  re-transmit the image (`i=` re-transmit wipes placements), re-place.
  Window destroy → `a=d,d=i,i=ID,A` (release data too).
- **Flicker**: every visible draw happens either as an in-place frame patch or a
  same-`(i,p)` re-placement, so window content never blinks; only the first
  placement of a new window appears (optionally pre-covered by a "starting…"
  tile from the tile map).
- **Z**: window stack maps to TGP z values (10·layer + index, negative for the
  wallpaper). Overlapping semi-transparent windows alpha-blend, matching the
  compositor's output pixel-exactly.
- **Sub-cell positioning**: window origin = cursor cell + `X=`,`Y=` pixel offset.
- **Cursor (window decoration)**: composed by the host compositor into the
  framebuffer, so it's part of the stream. The *mouse pointer* is not composited
  by headless backends — overlay it as a separate 1-image sprite, or clone the
  X11 core cursor; Wayland headless wlroots has no HW cursor plane, so streaming
  it separately is the plan.

## Renderer (the codec)

There is no codec. TGP accepts raw RGBA, zlib, or PNG payloads; the terminal
cannot decode anything else. The "codec" is: tile the framebuffer, only ever
transmit *changed tiles*, deflate them, and use TGP animations as the delta
mechanism. Lossless-only: no DCT, no motion compensation.

### Tile stage

- Every invalidation is snapped to the 64×64 tile grid.
- Adjacent dirty tiles coalesce into one rect: escape overhead (~30 bytes per
  command + base64 rounding) only amortizes over reasonably chunky rects.
- Adjacent-frame identical tiles are never re-sent.
- Per-tile XOR against last transmitted copy, then deflate (`o=z`). Empty XOR =
  skip (zero cost).

### Scroll

Scrolls are rare full-height events; simplest implementation today: the big
blit (`a=c` row-shift) is not implemented in v0.1 — scroll invalidates all
tiles it touches. If profiling later shows scroll as the bandwidth hot path,
the documented optimization is: `a=c` blit rows [dy..H]→[0..H−dy] server-side,
then one `a=f` patch for the revealed strip. Deferred.

### Streaming modes

Two supported streaming modes, selected per stream:

- **Frame-chain (mode A)** — gapless `a=f,c=<prev>` delta frames accumulate;
  terminal paces via `a=a,s=2` (loading mode). Periodic keyframe: full
  re-transmit with the same `i` (TGP re-transmit wipes old data + placements;
  this is the keyframe operation), re-place with the same `(i,p)`. Keyframes
  every N seconds (default 10 s) and on quota pressure (kitty: 320 MB image
  quota per buffer; animated frames get 5×, spilled to disk; LRU eviction of
  images without placements under pressure).
- **In-place edit (mode B)** — single frame, edited in place via
  `a=f,i=ID,c=1,r=1,x,y,s,v`. Constant memory, no keyframe problem. Conceptually
  clean; first prototype must verify on real kitty/ghostty that in-place edits
  trigger correct repaints (untested against real terminals — known risk).

### Scheduling / backpressure

- Emitter keeps an in-flight budget of tiles (bounded memory) and gates on TGP
  acks (`i=…;OK`) with `q` unset on pacing-critical commands.
- Viewport tiles are sent first. Damage accumulated while a tile is queued
  coalesces (never serialize stale frames behind old ones).
- RTT from ack latency sets the tile budget: target ≤ 200 ms of undelivered
  pixels in flight; when over budget, coalesce more aggressively and skip
  intermediate states (final-state-only redraws).

## Input

Decoded from the PTY on stdin; injected into the headless display server.

- **Keyboard**: kitty keyboard protocol (`CSI u`) preferred: keysyms, modifiers,
  press/release — complete encoding. Fallback ECMA-48/legacy sequences.
- **Mouse**: SGR mouse mode for buttons/wheel/drag, cell-resolution. Optional
  SGR-Pixels (mode 1016) where supported, for pixel coordinates. These numbers
  are applied to *placement geometry*, not pixels (cell × cell_size + X,Y).
- **Injection**: X11: `XTEST`. Wayland: virtual input (libei or
  `zwp_virtual_keyboard`/pointer-manager).
- **Clipboard**: OSC 52 set/get (get needs terminal opt-in), bridged to
  `xclip`/`wl-copy` in the head display.
- **IME**: provided by the host terminal; by the time bytes reach the PTY they
  are final text/keysyms, so the head display server needs no IME.

## Encoding details

- Pixel data: 32-bit RGBA (`f=32`), NOT pre-multiplied alpha, once zlib
  decompressed. `o=z` zlib-deflate applied before base64.
- Direct transport (`t=d`) over SSH with 4096-char base64 chunks (`m=1`/`m=0`).
  Chunks of a command are contiguous; no other graphics command interleaved.
- Probe via `a=q` + `CSI c` (DA1): graphics ack ⇒ TGP present; only DA1 ⇒ no.
- Geometry via `TIOCGWINSZ` (`ws_xpixel/ws_ypixel`) and `CSI 16t` cell size.
- Tempo of images: id space is per-window images (`i=1..`); patches use
  `a=f,i=ID,c=1,r=1` (mode B) or chain `c=<prev>` (mode A).

## Unicode placeholders / multiplexer survival

- Primary mode: direct placements; a multiplexer in the path breaks TGP.
- Fallback: virtual placements (`U=1`, quiet `q=2`) + `U+10EEEE` placeholder
  cells (fg = image id, underline = placement id, diacritics = row/col) so
  tmux/herdr move the placements as text redraws happen.

## Scope and non-goals

- One host binary; host must run on the same machine as the apps.
- Client requirements: terminal with TGP (kitty, ghostty, wezterm, Konsole).
  No client binary — everything rides the existing SSH session's PTY.
- Non-goals for v1: pixel-precise hover for apps needing sub-cell mouse
  accuracy, audio, drag-and-drop, passthrough of legacy sixel.

## Milestones

- **M0 probe** — capability probe + TGP smoke test (send-png style), geometry
  detection. Acceptance: PNG displays in kitty and ghostty; probe works.
- **M1 capture** — Xvfb + XDamage + MIT-SHM capture loop; frame dump to PNG on
  damage. Acceptance: typing in an xterm under Xvfb produces damage events.
- **M2 delta emitter** — tile store, XOR+deflate tile deltas, gapless `a=f`
  chain, keyframing, ack-gated scheduler. Acceptance: a text editor streamed
  over a rate-limited link runs < 1.5 Mbit/s sustained.
- **M3 input** — kitty keyboard protocol + SGR mouse → XTEST injection; wheel,
  drag, and modifier correctness. Acceptance: GUI app fully operable from
  the terminal.
- **M4 windowing** — multi-window app composition, z-order, per-window streams,
  move/resize (same-`(i,p)` re-placement), window sprite overlay.
- **M5 Wayland head** — headless wlroots backend via `wlr-screencopy`, virtual
  input (libei). Acceptance: a GTK4 app runs and is interactive.
- **M6 robustness** — placeholders (tmux/herdr survival), quota keyframes,
  mode B in-place edits, re-probe on terminal reset, logging.

## Risks / open questions

- **In-place edits (mode B) repaint semantics on real terminal** — verify early
  (M2 spike); fall back to frame-chain mode A otherwise. Risk: moderate.
- **High-entropy content**: video/game footage exceeds lossless capacity by
  30–100×; interface content is the target. Document, don't solve. Risk: none
  (scope boundary, not a defect).
- **Quota**: kitty 320 MB image quota per buffer; streaming must keyframe on
  pressure. Risk: low after keyframing.
- **Terminal quirks**: TGP implementations differ (ghostty/wezterm/Konsole);
  the acceptable set for M0–M6 is kitty + ghostty; others best-effort.
- **Mouse granularity**: cell coordinates only, unless mode 1016 available.
  Hover-precision apps (sliders, drawing) are degraded. Mitigation: document.
- **Terminal reset wipes placements**: re-probe and rebuild the scene after
  reset (detect via a periodic quiet probe or DA1 re-send).

## Prior art

- **term.everything** — Wayland compositor outputting to terminal (closest prior
  art); inputs handled inside the compositor.
- **sixel-streamer** — Xvfb + ffmpeg x11grab → sixel over TCP; proves the
  capture→encode→terminal pipeline, palette-limited output.
- **notcurses** — damage-tracked rect updates for kitty/sixel at library scale.
- **mpv vo_kitty** — full-frame TGP streaming; shows the naive approach's cost.
- **kitty graphics protocol spec** — `docs/kitty-graphics-protocol.rst` in this
  repo is the canonical reference.
