# MULTIWINDOW — who holds a window, and who draws it

Status: decided. Nothing built. This records the split between the compositor and
`gui` for more than one window, and why the compositor does not flatten.

## The decision

The compositor **hands windows up**; it does not composite them into one picture.

- The compositor holds the **registry**: which surfaces exist, their kind (toplevel,
  popup), their parent, and their pixels. It originates nothing about layout.
- `gui` holds the **layout**: placement, z-order, hit-testing, focus. It is the
  authority on where a window is and who is on top, and it forwards input to the
  window it hit-tested.
- Detach is a **predicate over the registry**, not a second path: "this surface
  renders to its own window instead of into the composite."

## Why not flatten

Flatten means detach needs a per-window path beside the composite path. Both must
decide what a window *is* and where it *is*, over the same surfaces, and stay in
step. That is the permanent version of the coordinate-drift bug already seen at
`crates/compositor/src/state.rs:436`: a fact derived twice, reconciled by hand.

The split that makes hand-up clean is that registry and layout are *different
facts*, so one on each side is not duplication. Flatten-plus-detach duplicates
**layout**: the flatten layout and the detached layout are the same information
computed twice. Hand-up has one tracker, and detach is a filter over the list it
already has.

A second reason, for the H.264 path: a flattened frame re-encodes the whole scene on
every window move, and a move larger than the motion-search range leaves residual
artifacts in both the moved region and the region it revealed, visible until the
next intra-refresh. Separate streams make a move a re-placement with zero bits and
no artifacts.

## The one-way residual

The compositor still needs each surface's current placement, for two things it alone
can do:

- convert an incoming click (surface-local, sent by `gui`) into what the seat
  expects, and
- send a configure size to the client.

`gui` originates that placement and tells the compositor, the way a compositor tells
a client its configure. Single owner, one direction. There is no second derivation,
because the compositor never computes a placement: it receives the current one and
uses it.

## What is wrong today

The split above describes nothing that exists. Today the compositor is worse than
flattening; it has no scene at all:

- any child surface of the admitted window is copied into the window's pixel store
  **at the origin**, so a subsurface or popup stomps the main buffer instead of
  sitting at its offset (`state.rs:174`),
- a second `xdg_toplevel` is rejected outright (`state.rs:248`),
- a popup is dismissed on sight, so no menu or dropdown can open (`state.rs:296`).

So popups are the first real requirement — they are the common case — and a second
toplevel is the workspace case.

## Wire

The registry must be visible to `gui`, so the wire carries a **window id** in both
directions:

- up with each surface: id, parent id, kind, pixels;
- down with input: which window the event targets.

Pixels stay opaque, so `easytgp` is unaffected. The per-terminal image id is the
part that is not finished: `IMAGE_ID` is a fixed 99
(`crates/tui/src/session.rs:31`) and two panes already collide, so ids must be
allocated per window per session.

## Detach

"Detach" names three different things, and they are not one feature:

1. a window rendered in the same pane, on its own — a renderer choice;
2. a window rendered in a second pane, its own session and image — needs the
   per-session ids above;
3. a window rendered as a native window on the viewer's desktop — a real window with
   real input events, not a placement at all.

Under flatten, (3) needs the second path this document rejects. Under hand-up it is
the predicate applied to one surface.

## Open

- Does a detached window leave a hole in the pane, or float over it?
- Native OS window events for a detached window, or terminal input routed through
  `gui` as for any other?
- Does `gui` poll the registry, or receive surface-appeared events?
- Where placement lives in cell space vs pixel space, and the one function that
  converts for both the draw and the hit-test path.
- Decoder topology on the H.264 path: one decoder per window in `tui`, or `gui`
  composites decoded frames and sends one stream.
