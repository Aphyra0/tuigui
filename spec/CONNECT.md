# CONNECT — how the two ends find each other

Status: **built** for one machine. The remote case is unbuilt and needs a second
transport; see [TRANSPORT.md](TRANSPORT.md) for the channel itself.

## The idea

The server sits in the middle of the byte stream, so it can be the one that
**dials**. A client does not need to know where the server is. It only needs to
announce itself, and the server — which is already reading every byte the pane
produces — sees the announcement and connects back.

This reverses the original direction. The server used to bind a socket and
publish a session file, and the client read that file and dialed. Now the client
publishes and the server dials.

## Why the server can see it at all

The server owns the last buffer before the real terminal. Everything the wrapped
command — and any multiplexer inside it — writes comes through one pty, and the
server reads it. That is the only reason the announcement reaches the server
without a side channel: it travels up the same path as normal text.

The clean form of this is a **subscription, not a callback**: the pty hands out a
handle to what it produces, and the server reads it. The pty does not know who is
reading, so adding a reader is not a change to the pty. Two readers exist — the
scan and the terminal — and each gets every chunk, so neither can consume bytes
the other never saw.

## The announcement

```text
   unix : /run/user/1000/tuigui.sock
```

One line of text, written into the pane's output as *drawing*. It began as a
private OSC sequence, which is invisible and unambiguously ours, and that is
wrong for the case this exists for: a multiplexer parses a pane's output into a
grid of cells and re-draws it, so an OSC it does not model is dropped and never
read. Text is not dropped. The client makes its announcement by drawing it, so
the text the server reads is the same text the user sees.

The announcement carries no secret and proves nothing. What it gives is a
**channel to dial**, and the identity that matters comes from the handshake on
that channel.

## The handshake

Both ends are peers; either may dial and either may accept. The exchange is the
same with the order reversed:

```text
   dialer                              acceptor
   ──────                              ────────
   connect to the announced address    wait for a connection
   send Ping(label)                    recv → Ping
   recv → Pong                         send Ping's answer
   ⇒ an established link               ⇒ an established link
```

Only an established link is returned to the caller. A peer that never answers is
a peer that is not there, and there is no such thing as a link that is up but
unproven — that state was removed rather than left for every caller to check.

On one machine this is the whole of the authorization: a unix socket in a
directory only the user can reach is enforced by the kernel at `connect` time.

## Locality

One machine needs nothing beyond the socket: the client picks a path under
`XDG_RUNTIME_DIR` — private to the user, cleared on logout — binds it, and says so
in the pane. Nothing has to be provisioned and nothing survives a crash.

## Across machines

Unbuilt. The announcement travels fine, since a pane's output crosses ssh like
any other text; what is missing is a channel on the far side to dial. The
candidate is ssh: it is already there, it is authenticated, and its port
forwarding gives a socket on whichever end needs one.

Two things make it different from the local case, and both are open:

- **The forward must exist before the announcement is read**, and the ssh is
  owned by whoever started the session rather than by us. `SSH_SEAM.md` is the
  proposal for that.
- **A second transport would bring ranking back.** Today there is exactly one
  channel, so nothing has to choose.
- **The address would be stable.** The scan remembers addresses so a redraw is
  not a second client, and never forgets them, which is right for a per-pid
  socket path and wrong for a stable remote one.

## Open problem: pane provenance

A pty has a single master fd, and a multiplexer multiplexes all its panes onto
it. So an announcement arrives with **no indication of which pane wrote it**.

With one client this does not matter. With several it breaks twice:

- The server cannot tell which pane a stream belongs to, so it cannot draw the
  frames in the right place.
- The server cannot bind an announcement to a pane, so it cannot tell whose
  frames it is accepting.

Anything that fixes this needs the client to encode its own pane identity, plus a
way for the server to corroborate it. That is a separate design, and this one is
scoped to a single client until it is solved.
