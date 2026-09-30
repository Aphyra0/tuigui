# COMP — tuigui's own Wayland compositor

Status: in development (MVP, branch `dev`).

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

A `crates/comp` crate exposing the smallest API that replaces the cage path:

```rust
let comp = Compositor::init()?;                       // headless backend, private socket
let client = comp.spawn("path/to/client", (1280, 720))?; // run app, one window
let frame = client.capture()?;                        // BGRA/RGBA pixels + damage
client.key(Key::A, KeyState::Press)?;
client.mouse_move(640, 360)?;
client.click(Button::Left)?;
client.scroll(Delta::Vertical(2))?;
```

`spawn` launches the binary with `WAYLAND_DISPLAY` pointing at the
compositor's private socket (same trick `CageSession` uses today) and waits for
the first xdg_surface commit so the caller never sees a half-alive client.

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

`smithay::backend::renderer::glow` over EGL/GBM on a render node — the
headless setup Smithay's `smallvil` uses for the "no real display" case, but
driven without winit (a plain calloop event loop). The output is created once,
headless, at the size given to `Compositor::init`, with
`PresentationMode::Wait` as the frame-clock emulator so frame callbacks fire on
a sane cadence.

The `resolution` passed to `spawn` is the **client's configure size**; the MVP
fixes it to the output size, so one call site controls both.

### Capture

Headless GL renders the output into a linear-format GBM buffer that we mmap.
`capture()`:

1. render the current output state into the back buffer (Smithay's
   `draw_full_tree`-style render element pass),
2. `glReadPixels` into the caller-owned `Vec<u8>` — BGRA→RGBA swizzle happens
   in a tight loop,
3. report damage since the last capture as pixel-space `Rect`s.

The MVP starts **shm-first**: a shm-only client (weston demos) commits plain
`wl_shm` buffers, which we can read directly and skip the GL copy entirely;
the GL path is the fallback for dmabuf/EGL clients (glxgears) and for downscale
compositing.

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

## FrameSource integration

`CompFrameSource` (later, outside this crate's MVP) implements
`tuigui_streamer::FrameSource` on top of `client.capture()`, so `main.rs`
switches between cage and comp with a flag; the TGP encoder stays
codec-agnostic.
