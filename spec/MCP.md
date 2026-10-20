# MCP — letting an agent look inside the windows

Status: idea (design only, nothing built).

## The observation

Everything this project does is for a human eye. `gui` draws a pane, `tui`
transmits an image, and the terminal shows pixels. That is a fine interface for a
person sitting at the keyboard and a useless one for an agent, which has no
terminal to look at.

An agent running on the same machine can already *start* a session. What it cannot
do is see the result. So the interesting question is not "can an agent drive the
GUI" — it is **what should an agent be given, if pixels are the wrong answer?**

The answer is not a screenshot. A screenshot is what a human needs and an agent
only tolerates: it costs tokens proportional to area, it needs a vision model to
interpret, and it tells the agent nothing about what is *interactive*. An agent
wants to know what is on the screen in a form it can reason about and act on.

## The shape of the problem

The content of a window is already in this process. `gui` runs the app inside a
compositor of our own (`crates/compositor`), so we own the buffer it paints into
and the seat it reads input from. Nothing is a screenshot; the picture is a
`Vec<u8>` we are holding.

That means there are three different things to hand out, and they are not the same
interface:

| what | who wants it | cost |
|---|---|---|
| the pixels | a human, or a vision model | high, and grows with area |
| the *structure* (what is on screen, and where) | an agent | low, and grows with content |
| input (click, type, scroll) | an agent that has decided to act | — |

The middle row is the whole idea, and it is also the part that does not exist yet.
Everything below is a plan for the other two around it.

## An MCP server, because that is what agents already speak

Say it as an MCP server rather than a bespoke API, so no agent has to be taught a
protocol for this: tools and resources are what the client already knows how to
call. Three tools are enough to start.

```text
  window.list()                  -> [ { id, title, app_id, size } ]
  window.capture(id)             -> a PNG (the escape hatch, not the main path)
  window.input(id, action)       -> click / key / scroll
```

`window.list` is where a session becomes legible. Today `session.rs` holds exactly
one image id because the design serves one pane; with a handful of windows this
becomes the agent's model of what is running.

Two design decisions are load-bearing here.

**It is per-session, not per-machine.** The MCP server belongs to a `tui` session,
because that is where the content is. A machine-wide MCP server could only see
processes, never pixels.

**`capture` is a fallback, not the interface.** It is what a vision model wants and
what a human inspecting an odd frame wants. It should not be the thing an agent
reaches for by default, or the tokens go to pixels rather than to meaning.

## What the pixels are not enough for, and the missing middle

`window.capture` exists. The structural channel does not, and it is the hard part.

The cheap versions of it are wrong. A grid of the terminal's own cells is the
presentation an agent should not have to reason about. An accessibility tree is the
right shape but has nothing to build on: the compositor does not link atspi, and an
app renders *pixels* into a compositor — it publishes no widget tree to us. Getting
one means asking the app (AT-SPI, which needs a bus) or inferring it (from the
pixels we already have).

So the honest position is:

- **shm-apps: not yet.** The compositor records no window title and no `app_id`
  today (`Window` in `crates/compositor/src/surface.rs` tracks geometry, pixels,
  damage and the configure serial, and nothing else), so `window.list` cannot be
  built as written. Titles are one `xdg_toplevel.title` handler away, and that is
  the first piece of work.
- **What is already there: a text channel.** Many of the apps this is for are
  terminals themselves — the pane can be running a shell. Text goes over that
  channel at almost no cost, with no model needed to read it, and the app in the
  pane controls what it says. An app that *knows* it is being watched by an agent
  can be told so and can describe itself.

That last point is the design direction worth taking seriously: rather than the
agent guessing what a GUI means, let the agent-visible surface be something the app
cooperates with. Aiding a blind user and giving an agent a model are the same
problem, which is why the accessibility route is the one that stays relevant as the
idea grows.

## Where it sits in the stream

The wire already carries opaque frames (`Message::Frame(Vec<u8>)`,
`crates/wire/src/message.rs`), so a second message type is a small change rather
than a new protocol. The channel is the same one a client announces.

```text
   app  ──▶  compositor (gui side)  ──▶  wire  ──▶  tui  ──▶  terminal (pixels)
                                          │
                                          └──▶ MCP server ──▶ agent
```

The input half already exists on the far side: `Client::type_key`, `click`,
`scroll`, `move_mouse` and `pointer_state` are all in the compositor's API. Nothing
in `gui` calls the pointer ones yet — the client has no mouse reporting — so
"click" is the part of this that needs new work in the terminal path, not here.

## Input is the half that needs a policy first

`window.capture` is a read. `window.input` is not, and it is asymmetric in a way
that deserves saying out loud.

An agent typing into a real application is a security boundary, not a feature. The
app may be a browser mid-authentication, a shell, or an agent's own terminal. The
same problem MCP has anywhere it touches a live system, and the answer here is
containment rather than trust:

- the compositor is already the boundary — it holds **one window**, so "the app
  this agent may touch" is a fact about the process rather than a filter, and
  `spawn` already scrubs the child's environment so it cannot reach the host
  compositor,
- one MCP server per session means one agent per pane, not one agent for the
  machine,
- `--dbg-`-style flags belong on the *server*, not on the agent: which tools are
  enabled, and whether input is offered at all, is the user's decision. A session
  started for a person should not have an input tool exposed by accident.

Credentials reach that app too — an environment variable for it, or written to its
filesystem — and the window is a pixel-accurate view of everything the app decides
to draw. That is worth stating rather than discovering.

## What this is not

Not a screenshot tool. There are enough of those, and a compositor that owns a
window is not needed to take one.

Not a remote-control protocol. It is local by construction: the compositor, the
window and the seat are in the process running the app.

Not a replacement for the terminal pane. The pane stays what it is — the way a
human watches, and the way text gets in and out.

## What to build first

1. **Window identity.** Handle `xdg_toplevel.title` and `app_id` in
   `crates/compositor` and surface them on `WindowShared`. Without it there is no
   `window.list`.
2. **The MCP server skeleton.** `window.list` (which needs 1) and
   `window.capture`. Both are reads and neither needs a policy.
3. **A `capture` that is honest about its channels.** The compositor's capture is
   `wl_shm` only today, so an EGL client (`kitty`, `weston-flower`) captures black.
   An MCP tool that returns a black PNG for a working window is worse than one that
   says it cannot see it.
4. **Input last**, behind a flag, once there is a session to test it in — and after
   the terminal path has mouse reporting at all.

## Related

| | |
|---|---|
| [COMP.md](COMP.md) | the compositor whose API this is built on. Built. |
| [DESIGN.md](DESIGN.md) | the session and the pixel path. Built. |
| [REMOTE.md](REMOTE.md) | the same session across machines, which is what makes a *remote* agent possible. Planned. |
| [EMULATOR.md](EMULATOR.md) | a terminal that takes a compressed stream. An idea. |
