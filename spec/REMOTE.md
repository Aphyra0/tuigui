# REMOTE — streaming a remote GUI into the local terminal through a peer tunnel

Status: proposal (design only, nothing built).

## Problem

We can already run a GUI app on a headless display head, capture it, and stream
it into the local terminal as pixels over TGP (`spec/DESIGN.md`). What we cannot
do yet is run the *app* on a different machine from the *terminal* and have the
pixels appear with muxer-class latency and no flicker.

Two things make that hard:

- **Shared memory does not cross the network.** The fast medium (`t=s`) names a
  POSIX object under `/dev/shm`; it is only meaningful when the terminal is on
  the same machine. Across machines the wire must carry *encoded* bytes, and
  something local must own the shm object and the transmit.
- **A multiplexer sits in the way.** If the app runs inside tmux/herdr/zellij,
  the graphics escapes have to survive that muxer. Relying on passthrough is
  fragile and terminal-dependent.

## Core decision: the server wraps the multiplexer, not the other way around

The server is the **parent of the muxer's pty**. It is not a pane inside the
muxer, and it does not ask the muxer to pass graphics through. It owns the fd to
the real terminal and writes its own TGP escapes there directly. The muxer only
ever sees and emits **text**; the server never forwards graphics escapes through
it.

```
   local machine                                    remote machine
 ┌───────────────────────────────────────────┐   ┌──────────────────────────┐
 │ server                                     │   │ gui <app>                │
 │                                            │   │  ├─ headless compositor  │
 │  ┌─────────┐  keys/mouse   ┌────────────┐  │   │  ├─ damage capture       │
 │  │ herdr   │ ────────────▶ │            │  │   │  └─ encode (hw, h264/av1) │
 │  │ (panes) │ ◀──────────── │  outer     │  │   └───────────┬──────────────┘
 │  └─────────┘  herdr output │  pty mux   │  │               │
 │       ▲                    │            │  │      peer tunnel (QUIC)
 │       │ keys               │            │  │   frames + control, drop-on-late
 │       │                    └─────┬──────┘  │               │
 │       │                          │ raw escapes (herdr's own   │
 │       │                          │ + server's TGP)           │
 │       │                          ▼                           │
 │       │                    ┌───────────┐                     │
 │       │                    │ shm stage │ ◀───────────────────┘
 │       │                    │ + decode  │
 │       │                    └───────────┘
 │       │                          │ TGP transmit / animation
 │       └── kitty ◀────────────────┘
 └───────────────────────────────────────────┘
```

The consequences of this one decision are what the rest of the spec is about:

- **No passthrough gamble.** herdr moves `U+10EEEE` placeholder text like any
  glyph. The server streams the frames out-of-band. Whether a muxer "supports
  images" stops mattering.
- **Content and position separate cleanly.** The remote app prints a *static*
  placeholder grid that anchors the image to cells; the server keeps that one
  image current with animation-frame data. See "Display protocol" below.
- **shm stays local.** Encode remote, decode local, stage in `/dev/shm`,
  transmit on the outer fd. The tunnel carries bytes, never pixel buffers and
  never object names.

`gui <app>` is the remote command: prefix it to whatever you would have run to
start a remote session (ssh, herdr's remote machine, tmux over ssh). The server
runs locally and wraps the local muxer.

## Display protocol: placeholder text for position, animation frames for content

This is the part that makes it flicker-free and cheap, and it is built entirely
from existing kitty protocol features (`docs/kitty-graphics-protocol.rst`).

**Position — the remote app's job, once per layout.** The app prints a grid of
`U+10EEEE` placeholder cells for one negotiated image id
(`:585-590`, `:601-605`). These are ordinary text: the muxer reflows, scrolls,
and re-lays-out them without knowing anything about graphics. The image id is
encoded in the cell's foreground color, with the optional MSB diacritic
(`:636`). A `U=1` virtual placement (`:690-700`) sizes the image to the grid.
This grid is written **once** and is not repainted per frame.

**Content — the server's job, per frame.** The server owns that id and drives it
with the animation protocol (`:898-906`, `:907+`):

- `a=f,i=<id>` transmits new frame data (optionally a sub-rect via `x,y,s,v`).
- `a=a,i=<id>,c=<n>` switches the current frame. This is an **atomic** swap:
  no delete, no re-place, no text repaint, no flicker.

Because the placeholder cells never change, the *terminal-facing* cost of a new
video frame is one animation control escape, regardless of how large the image
is. The bytes that scale with the image travel over the tunnel as compressed
video, not as terminal escapes.

**Why not a fresh image id per frame.** That would force the placeholder's
foreground color to change every frame, i.e. a full-grid text repaint through
the muxer each frame. It relocates the flicker and the bandwidth instead of
removing them. One id, animated, is the point.

**Why not `a=p` re-placement.** Re-putting the same image+placement id replaces
in place without flicker (`:516-520`), but it still requires the server to
re-emit a placement and, for a new id, a delete of the old one. The animation
frame switch is strictly cheaper and atomic, so it is the default.

## Frame pipeline

1. **Compositor** (remote): headless head, one client surface, damage-driven
   capture. This is the `spec/DESIGN.md` pipeline; nothing here changes it.
2. **Encode** (remote): hardware-accelerated H.264/AV1 (VAAPI/NVENC/VideoToolbox)
   so the remote's CPU stays free for the app. Encode is per-frame and
   rate-limited by the tunnel's send budget, not by the app's redraw rate.
3. **Tunnel** (both): peer QUIC, separate streams for frames and control.
   Frames use **drop-on-late** semantics: a superseded frame is discarded, never
   queued behind a newer one. This is a *datagram* concern, not a stream concern.
4. **Decode** (local): hardware-accelerated, into a local buffer, then staged
   into the `/dev/shm` object the TGP transmit reads. This is the existing
   `ShmFrame` path in `easytgp` (`crates/easytgp/src/lib.rs`), the same one the
   `--shm` mode of `crates/easytgp/examples/video.rs` exercises.
5. **Present** (local): `a=f` the decoded frame into the live image, `a=a,c=` to
   make it current. Backpressure: wait for the terminal's ack before the next
   frame, exactly as `dimg.sync()` does in the video example.

The remote and local halves are pipelines that can each drop intermediate
frames; only the *latest* frame is ever presented. This is what keeps latency
bounded when either the network or the terminal is the bottleneck.

## The wrapper responsibilities (the genuinely new work)

Being a terminal toward herdr is not a pipe. Four jobs:

1. **Synthesize terminal replies.** herdr boots by querying its terminal: DA1
   (`CSI c`), `XTWINOPS`, `CSI 18 t`, kitty-support probes, color queries. The
   server must answer as a capable terminal before herdr will behave. This is
   the cost of wrapping rather than passing through, and it is the largest new
   component.
2. **Demux kitty's input.** The server's own TGP replies and herdr's keystrokes
   arrive on the same fd. The server must consume its replies and forward only
   keys/mouse to herdr. This is the single-reader split from
   `docs/DESIGN.md:129-130`, and getting it wrong looks like dropped keys.
3. **Serialize the output fd.** herdr→kitty bytes and the server's own escapes
   share one writer. An escape must land between herdr's writes, never inside
   one. One writer thread, FIFO, bounded queue.
4. **Resize.** `SIGWINCH` from kitty relayouts the outer screen *and* must set
   `TIOCSWINSZ` on herdr's pty so herdr reflows. Two coupled size domains; the
   placeholder grid must be re-negotiated when the pane geometry changes.

## Handshake / negotiation

The placeholder grid and the animation frames must agree on an image id and a
cell rectangle. On connect (and on every resize):

1. Client offers: screen size in cells, cell pixel size, a candidate image id,
   and the pane's cell rectangle.
2. App draws the placeholder grid for that id and rectangle into its surface.
3. Server creates the `U=1` virtual placement sized to the rectangle, then
   begins `a=f` frames, then `a=a,c=1` to reveal the first one.

Image ids are a **global namespace** on the terminal. One id per streamed
surface, allocated and negotiated, never hardcoded, so two surfaces cannot
collide.

## Transport

The tunnel is a swappable boundary, not a foundation:

```rust
trait Transport {
    async fn send_frame(&mut self, frame: EncodedFrame) // drop-on-late
    async fn send_control(&mut self, msg: Control)
    async fn recv_control(&mut self) -> Control
}
```

- **iroh** (QUIC, NAT traversal, relay fallback, encryption) is the right choice
  when the two machines are true peers with no existing trusted channel, and it
  gives datagram drop-on-late for free.
- **SSH / localhost forwarding** is the right choice when you already `ssh` into
  the remote box, because that channel is already authenticated and pierced. It
  should be the first implementation: a day of work, no new dependency.
- The design must not assume either. Anything that needs shm or a local fd lives
  on the client side of the boundary regardless.

## Reuse from the current repo

- **`easytgp`** supplies the reply router, the `ShmFrame` local medium, the
  animation-frame path, and the `sync()` backpressure the video example uses.
- **`crates/easytgp/examples/video.rs`** is already a working local pipeline:
  ffmpeg-sidecar decode → stage → transmit → sync. The `--shm` mode is the
  display half of this design, minus the tunnel.
- **The compositor/capture side** (`spec/COMP.md`, `spec/DESIGN.md`) is the
  remote half; it needs an encoder stage bolted on where capture currently feeds
  the tile store.

## Risks and fallbacks

- **Placeholder text must survive the muxer intact.** If herdr sanitizes
  `U+10EEEE` or its diacritics, the anchor is lost. Verify early; it is the one
  assumption the whole display model rests on.
- **Double render.** kitty matches placeholder cells left-to-right per line
  (`:658-686`). If herdr copies placeholder cells into a second view (sidebar,
  master view) at different coordinates, the image can render again or in the
  wrong place. If this bites, **plan B**: the server computes absolute cells and
  does a real placement (`a=p,x,y`) rather than relying on placeholders. The
  server already owns the outer fd, so it can position the image anywhere.
- **Quota.** Animation frames live under a separate terminal quota
  (`:1000`, kitty ~5× the base storage). The server must rotate frames and treat
  `ENOSPC` as `ResourcePressure` (`docs/DESIGN.md:95-97`), shedding and retrying.
- **`q=2` has no verdict.** A suppressed reply cannot confirm the transmit
  landed, so any ordering guarantee must come from an explicit ack or from
  FIFO, per `docs/DESIGN.md:42-45`.

## Milestones

1. **Passthrough probe.** From inside a herdr pane, print a `U=1` virtual
   placement plus `U+10EEEE` cells; confirm kitty renders it. Cheap, decides the
   display model.
2. **Wrapper skeleton.** Outer pty, input demux, query synthesis, output
   serialization. Success = `herdr` runs correctly and unmodified behind it.
3. **Local display loop.** Feed the video example's `--shm` path from a canned
   frame source, driven by the animation protocol instead of image-per-frame.
   Success = flicker-free in-place playback.
4. **Tunnel.** SSH/localhost first, then iroh behind the `Transport` trait.
5. **End to end.** Remote compositor + encode, local decode + present, resize
   and re-negotiation, drop-on-late under load.
