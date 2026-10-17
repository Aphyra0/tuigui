# EMULATOR — what a terminal could do instead of `tui`

Status: note (idea only, nothing built).

## Observation

`tui` (see `spec/REMOTE.md`) is a local server that sits between a remote frame
producer and the terminal. Nearly everything it does is work the terminal is
better placed to do:

| `tui` does today | Why it is really the terminal's job |
|---|---|
| decode the wire codec (JPEG today) | the terminal already decodes PNG for TGP |
| stage pixels in a `/dev/shm` object | it does that anyway, per transmit |
| emit the TGP escapes | it is the only side that can |
| own the image id and the placeholder grid | it owns the id namespace |
| pace frames and wait for acks | it is the one applying them |

That is a whole process, plus a socket or tunnel hop, in front of something the
emulator is already doing. The remote side could speak *directly* to the
terminal if the terminal offered a way to receive a stream instead of a series
of finished images.

## The idea

Define a protocol by which a terminal emulator accepts a **compressed frame
stream** and takes responsibility for the rest — decode, stage, place, pace.

The producer then sends only encoded bytes. Both halves of `tui` disappear:
no local decode server, no second placement path, and the network sees one
stream instead of re-encoded images.

Reuse the TGP chassis rather than inventing a new transport: the APC framing,
the chunking rules, the reply/ack scheme, and the medium keys (`t=`) all still
apply. What is new is only that a payload may be a *stream* with its own
decoder state, rather than a self-contained image.

## Address the image with Unicode placeholders

Do not invent an addressing scheme. Re-use kitty's Unicode placeholders
(`U+10EEEE` plus row/column diacritics, `docs/kitty-graphics-protocol.rst`).

They already solve the hard part: the image follows text the *application*
moves, so a multiplexer needs no graphics awareness, and position is decoupled
from content. A streamed frame then only ever updates the image; the cells that
reference it never change, so nothing on screen flickers and no repaint is
needed when a frame arrives.

The id would come from the terminal (it owns the namespace) and be reported back
to the producer before any cells are drawn, which is the same handshake `tui`
does now with `Hello`/`Image`.

## Worth having because

- One process and one hop fewer on the local machine.
- The terminal can decode on the GPU where it already has a decoder, instead of
  the producer re-encoding into a container it can parse.
- Backpressure becomes real: the terminal knows when it is behind and can ask
  the producer to drop or skip, rather than being flooded through a socket.
- Any client that can write a compressed stream gets video in any pane, with no
  `tui`-shaped sidecar.

## Open questions

- Which codec(s) to require. Intra-only keeps it simple; inter-frame is where
  the bandwidth is, but needs the terminal to own decoder state and keyframes.
- Whether the terminal or the producer chooses the grid and the id.
- What the failure reply is when a frame cannot be decoded, so the producer can
  resync rather than wedge (`tui`'s `Msg::Error` is the shape of that today).
- Whether this belongs in TGP as an extension or as a separate side-channel.
