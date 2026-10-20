# COMP — tuigui's own Wayland compositor

Status: built, in `crates/compositor`. It runs one client's window and captures it;
XWayland is wired up but not exercised, and the underlying capture is shm-only.

## Why

`crates/cage` today runs the external wlroots-based `cage` binary in headless
mode and attaches to it from the outside:

- a **separate Wayland client** (`capture.rs`) binds `wlr-screencopy` and copies
  the output into a shm pool every frame,
- **input** goes through `wlr-virtual-pointer` / `zwp-virtual-keyboard`.

That is a process boundary with protocol round-trips for both directions, and
the capture path copies pixel data twice (compositor → shm → us). Owning the
compositor deletes the boundary: the frame being presented is already in our
address space, so "capture" is a buffer read, and input injection is a direct
event dispatch into the seat.

## Goal

A `crates/compositor` crate exposing the smallest API that replaces the cage path:

```rust
let comp = Compositor::init((1280, 720), 1.0)?;  // headless, private socket
let client = comp.spawn("path/to/client", (1280, 720))?; // run app, one window
let frame = client.capture()?;                        // RGBA pixels + damage
client.type_key(Key::Char('a'))?;
client.move_mouse(640.0, 360.0)?;
client.click(Button::Left)?;
client.scroll(ScrollDelta::vertical(2))?;
```

`spawn` launches the binary with `WAYLAND_DISPLAY` pointing at the
compositor's private socket and waits for the first commit, so the caller never
sees a half-alive client.

## Who uses it

`gui` is the only caller: given a path that is not a clip, it starts a compositor
and runs the program inside it, then sends the captured window as frames
(`crates/gui/src/compositor.rs`). The video path is unchanged and selected by the
`.mp4` extension.

## What the capture can and cannot read

`capture()` copies **wl_shm buffers**. A client that renders through EGL/dmabuf
(kitty, foot's GL path, `weston-flower`) commits a dma-buf instead, which is not
readable this way: the window runs, but every captured frame is the empty output.
There is no GL capture path yet — `OUTPUT_FORMAT` and the GL comment in the crate
root are where it would go.

## Window policy (the hard rule)

**The compositor serves exactly one client surface.** When a client asks for a
second xdg_toplevel, the compositor **rejects the request**:

- log a warning,
- send the client a close event for that surface,
- never map it.

Rationale: in the future, spawning another client means the compositor itself
will `spawn` a second app and manage it as a second *client handle* in-process
— the compositor will handle its own multi-client story, and the current API
contract is one surface per handle. Anything beyond that gets decided later;
rejection keeps the surface bookkeeping honest instead of silently stacking
windows on top of each other.

## Architecture

### Backend

Software compositing only — there is no GPU renderer. Smithay's compositor,
xdg-shell, shm, seat and XWayland frontends are driven by a plain `calloop` event
loop on a dedicated thread. The output is created once, headless, at the size
given to `Compositor::init`, with a frame-callback emulator firing `done()` each
turn so clients keep drawing.

The `resolution` passed to `spawn` is the **client's configure size**; the MVP
fixes it to the output size, so one call site controls both.

### Capture

There is no render pass and no GL read-back. On every commit, the client's
committed buffer is copied straight into an owned `Vec<u8>` in `commit()`
(`crates/compositor/src/state.rs`), and `capture()` clones it:

1. `map_rgba` copies the `wl_shm` buffer into the window's pixel store, row by
   row (shm pools may be sparse, so the stride is never assumed to make rows
   contiguous),
2. damage since the last capture is taken as pixel-space `Rect`s,
3. a software cursor disc is drawn over the result.

This is the **shm-first MVP**. A dmabuf/EGL client's buffer is not read at all,
so its window captures as black; the GL path is the work that would fix that.
The output is `Argb8888`, whose bytes are B, G, R, A — the `Frame` is named
RGBA because that was the intent, and `gui` encodes it as `BGRX` to match what
is actually there.

### Input

We own the seat, so injection is direct:

- keyboard: synthesize `KeyPress/Release` keycodes (evdev codes + 8) through
  Smithay's `KeyboardHandle` input flow,
- pointer: `pointer_motion_absolute` + `pointer_button` + axis events through
  `PointerHandle`,
- scroll: pointer axis events, `AxisSource::Wheel`.

### Rejection path

xdg_shell `get_xdg_toplevel` for a second surface → warn + immediate `close`.

### Threading

Smithay state lives on a dedicated thread with its own event loop
(`calloop`). `Compositor` (init/spawn) and `Client` (capture/input) are
channels to it; `capture()` is a synchronous, blocking round-trip so the API
stays simple.

## Frame source integration

`crates/gui/src/compositor.rs` is that integration: it starts the compositor,
spawns the program, captures on a thread, encodes each changed frame as JPEG and
hands it to the link, and routes this client's keyboard into `Client::type_key`.
It is chosen by the runnable's extension, not by a flag: `.mp4` is a clip, anything
else is a program.

Input is the piece that is thin. Keys are decoded from the terminal's sequences in
`gui` and injected, so `foot` accepts typing; pointer motion and clicks reach the
compositor's API but nothing in the client calls them yet, which needs mouse
reporting to be enabled on the terminal.
