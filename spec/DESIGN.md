# DESIGN — what tuigui is

Status: partly built. This is the shape of the whole thing; what exists today is
the transport and the session between its two ends. What each piece is, and
whether it is built, is in [Where this stands](#where-this-stands).

## Problem statement

Terminals are text grids, but modern ones can display exact pixels through the
kitty graphics protocol. The gap is glue: nothing combines a headless display
server, damage-based capture, and a TGP delta emitter into one tool that works in
a pane — including a pane inside a multiplexer, and including over ssh.

Goal: run any X11 or Wayland app; see it pixel-accurate; interact with it from the
terminal at interface-class bandwidth with nothing on the client side but a
terminal that speaks TGP.

## Architecture

```
   the app's machine                          the terminal
 ┌──────────────────────────────────────┐   ┌──────────────────────────────┐
 │ compositor ──▶ capture ──▶ tiles ─────┼───┼─▶ TGP escapes ──▶ screen       │
 │ (headless,                            │   │   (owned by `tui`)            │
 │  Xwayland for X11)                    │   │                               │
 │                                       │   │                               │
 │ input ◄───────────────────────────────┼───┼── keys and mouse              │
 └──────────────────────────────────────┘   └──────────────────────────────┘
                          `gui` announces itself; `tui` dials it
```

The two ends are **client** and **server**, and they are not on opposite sides of
a client/server boundary in the protocol sense: either may dial and either may
accept, and the exchange is the same with the order reversed.

- **`tui`** owns the terminal. It runs a command on a pty it creates, forwards
  every byte of that pty to the real terminal, and reads it for one thing: the
  client's announcement. It paints the pixels, because it is the side holding the
  fd to the real screen and it is the only side that can.
- **`gui`** runs on the app's side. It binds a channel, announces it into its own
  output, waits to be dialed, and produces frames.
- **`easytgp`** is the pixel layer both use: the TGP encoding itself.
- **`wire`** is what the two agree on: the messages, the channel, and the framing.
- **`tuigui`** is one command that starts the server and the client inside it, for
  the common case where both are on this machine.

Putting the server *outside* the multiplexer is the decision everything else
follows from. `tui` is the parent of the muxer's pty, not a pane inside it, so it
writes its own graphics escapes to the real terminal directly and the muxer only
ever sees text. That is what makes graphics survive a muxer: not passthrough,
which is fragile and terminal-dependent, but never needing it.

## The pieces

### The channel

A transport owns its connection and everything done over it; see
[TRANSPORT.md](TRANSPORT.md) for why it is shaped that way and why a read waits
rather than polls.

Today there is exactly one, `UnixSocket`: both ends on one machine. The
announcement and the handshake are in [CONNECT.md](CONNECT.md).

The channel carries **frames as opaque bytes**. It does not know what a tile is,
which is what lets the pixel layer change without the session changing.

### `easytgp` — the pixels

Every pixel decision lives here, and both ends use it: the producer to transmit
frames, the server to put them on the screen.

- **Images and placements.** An image id names pixel data; a placement puts it at
  a cell, optionally offset within it. One id, many placements.
- **The id namespace is positional.** A tile's id says where it sits rather than
  when it was drawn, so a resize does not renumber the picture and the terminal
  can keep what it already holds.
- **Re-transmitting an id replaces its data and its placements.** That is how a
  tile is updated with no delete traffic, and what makes a frame a sequence of
  transmits rather than a scene.
- **Moves are placements.** A picture that only moved costs a placement per tile
  and no pixels.
- **Shared memory (`t=s`)** carries the pixels locally without base64 or
  compression, and it is one object per tile: a terminal reads an object and
  unlinks it, so an object can be placed exactly once. One object per frame was
  tried twice and does not work.
- **Atomic frames** (`?2026h` … `?2026l`) so a frame made of many commands commits
  at once rather than tearing, and one write per frame.
- **Replies and acks are accounted for**, which is what makes pacing possible
  rather than guessed.

### The compositor

An in-process Wayland compositor on a headless backend, with **Xwayland** inside
it so X11 applications run too. Both app kinds then reach the same path: a client
surface, damage, pixels in our own address space.

Owning it rather than shelling out deletes a process boundary and a protocol
round-trip in each direction, and it means "capture" is a buffer read rather than
a screencopy through a shm pool. It is built (`crates/compositor`); what it
captures today is `wl_shm` buffers only. [COMP.md](COMP.md) has the details.

### The terminal side

`tui` runs the command on a pty and forwards it verbatim. The command is a normal
terminal program and cannot tell that it is wrapped.

The pty's output is **published**, not pushed: the pty hands out a handle to what
it produces and readers subscribe. Two do — the scan, and the terminal — and each
gets every chunk, so neither can consume bytes the other never saw. A callback
would run a reader's work on the forwarding thread and leave it unable to wait on
anything else, and the server's loop has to wait on its clients too.

The one writer to the screen is a single serialized `Sink`, which is what stops a
frame landing in the middle of the command's output.

### The app side

`gui` binds a socket, draws its own address into the pane, waits to be dialed,
answers the greeting, and then produces frames. It draws to the writer its caller
handed it rather than to the process's stdout, which is what lets `tuigui` start
it inside the server's pty.

## Bandwidth

The target is interface-class content: windows, text, menus. High-entropy content
(video, games) exceeds any lossless scheme by an order of magnitude. That is a
scope boundary rather than a defect, and it is why the tile decision is lossless
and has no DCT in it.

## Where this stands

**Built**

- the transport, the announcement, the handshake, and the session between the two
  ends: a client draws its pane, announces itself, a server finds it and dials it,
  and both ends know the link is live
- `tui`: pty ownership, byte forwarding, the announcement scan, the server loop,
  and the one serialized writer to the screen
- `gui`: argument handling, binding and announcing, answering the greeting, and
  the terminal surface it draws on
- the frame path: `gui` decodes a clip with ffmpeg (or captures a program run in
  `crates/compositor`), JPEG-encodes it, and `tui` transmits and places it as one
  TGP image that the client's placeholder cells resolve against
- `compositor`: an in-process headless Wayland session, one window, capture and
  input injection. shm clients only; a dma-buf client captures black
- `tuigui`: both halves as one command
- `easytgp`: the TGP encoding, exercised by its own examples

**Not built**

- **dma-buf capture.** The compositor reads `wl_shm` buffers, so a client that
  renders through EGL (kitty, `weston-flower`) runs but captures as black. See
  [COMP.md](COMP.md).
- **pointer input.** Keys reach an app run in the compositor; motion, clicks and
  scroll do not reach it from the terminal yet.
- **any transport but a local socket.** The remote case needs a second one; see
  [CONNECT.md](CONNECT.md). The shape of it is in [REMOTE.md](REMOTE.md) and
  [SSH_SEAM.md](SSH_SEAM.md).

## Related

| | |
|---|---|
| [COMP.md](COMP.md) | the compositor, with Xwayland. Built. |
| [REMOTE.md](REMOTE.md) | the app on another machine from the terminal. Planned. |
| [SSH_SEAM.md](SSH_SEAM.md) | attaching a forward to an ssh we do not own, for that. Planned. |
| [REVERSE_SSH.md](REVERSE_SSH.md) | the pane ssh-ing back to the user, instead. An idea. |
| [EMULATOR.md](EMULATOR.md) | a terminal that accepts a compressed stream. An idea to hand to a terminal, not a plan here. |
| [MCP.md](MCP.md) | letting an agent see and drive what is in a window, instead of only a human seeing pixels. An idea. |
