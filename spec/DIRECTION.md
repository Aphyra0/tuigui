# DIRECTION — wrapping the terminal instead of the muxer, evaluated

Status: rejected (design only, nothing built). The alternative project direction
was explored and rejected; what survives of it is recorded here with the reasons.

## The proposal

Same architecture, but `tui` wraps not the muxer but the **terminal itself**: a
GUI app that wraps other GUI apps. The wrapper creates the window and proxies
all Wayland events to the underlying GUI — it does **not** create a new
compositor; the GUIs inside the terminal do, and they talk to the wrapper.
Claimed consequences: bypasses kitty graphics, works on any terminal, very
efficient, usable in anything that runs a terminal — a browser ssh session to a
VPS could wrap the browser and display video.

Taken as given, per the proposer: the keyboard/input path is solvable, and GPU
rendering is happening either way.

## Weaknesses named in evaluation

- **"Bypassing kitty" ≠ bypassing the pty.** Unless pixels travel a real side
  channel, the terminal's pipe is still the pipe; "efficient" and "works in any
  terminal" pull in opposite directions, and each deployment gets one.
- **The flagship remote scenario is remote-framebuffer**, where waypipe ships
  protocol and renders on the viewer's GPU — until the frames plan (below)
  changes the comparison.
- **"Any terminal" ≠ "any machine".** The wrapper needs a display server on the
  viewing end; macOS/Windows mean per-platform backends, and the browser-ssh
  machine has no window to open at all.
- **A fork it must pick:** two windows (focus split, the pane-grid identity
  gone) or become a terminal (a GPU terminal emulator; a multi-year surface).
- Quiet gaps: multi-window apps, audio, and a trust boundary — any process that
  can write to the pane can get a pixel-perfect window on the host desktop.

The proposer's own points against, which decided it:

1. **Friction** — it asks users to wrap their GUI with our GUI.
2. **Cost** — proxying a tui (text) is far cheaper than proxying pixels; too
   much machinery for the simple pane case.
3. **Thin benefits** — every target use-case assumes a terminal somewhere; this
   is not VNC for apps.
4. **Coverage was already imperfect** — terminals that are multiplexers lose
   "any terminal" anyway, and tuigui still runs inside their panes.
5. **No graphics, no demand** — if a terminal supports neither kitty nor
   sixel, its user does not want images; drawing them there is not a feature.

## The remote claim, resolved

Remote is the **market**, not a side case. The plan is h.264 with p2p QUIC
(relay fallback for blocked UDP), which on a bad link beats protocol proxying:
rate control degrades to softness instead of freezing, P-frames beat waypipe's
pixel-delta fallback (which is what it does when dmabuf sharing fails), and
there is no TCP-over-TCP. Video inside a browser — the flagship — is exactly
where inter-frame coding wins. Consequences for the encode side: fast preset,
intra-refresh instead of big keyframes, a bitrate floor for text sharpness.

## Why the wrapper dissolved instead of being refuted

With frames + GPU + h.264 decided, "wrapper streams pixels to a native window"
is the *same design* as a presentation choice on the existing `tui`: identical
server stack (compositor, encoder, input API), identical wire, only the
destination differs — cells or a window. Its only distinct variants are
separate products: Wayland-protocol proxying is rebuilding waypipe, and being
the terminal is [EMULATOR.md](EMULATOR.md).

**Decision: do not build the wrapper.** The pane path stays the identity.

## What survives: detach mode

The wrapper's UX, at the cost of a renderer swap inside `tui`:

- Same stream, two presentations: inline (cell anchor) or detached native
  window. Chosen by a detach key/button in `tui` or `gui -d`; reattach is
  symmetric (close the window, pixels return to cells).
- A terminal that cannot render inline just opens detached by default.
- Detached gets real window input events for free — no terminal mouse modes,
  no cell-to-pixel math.
- The window lives inside the running `tui` process, not a separate viewer:
  the pty interception and anchor rewrite are already there.

## Covering every terminal without wrapping anything

Rule: **inline needs `tui` in the path of the pane's stream; detached needs
only reachability.** Ordered outcomes, never a failure to show anything:

1. **Inline** — graphics-capable terminal, `tui` wraps the command, muxer, or
   ssh.
2. **Detached window** — no graphics in the terminal, or someone else owns the
   pane's ssh (from there we only ever see ciphertext).
3. **Web viewer** — nothing local can run (browser ssh, phone app): a
   browser page joins the stream; inline if that web terminal does kitty,
   detached otherwise. Escape hatch for users who want graphics but whose
   terminal supply is limited.
4. **`shared` transport** — even the last resort fails, or there is no `tui`
   and no path back at all: `gui` dials out to our relay and gets a URL to
   draw in the pane; any browser can open it. One outbound connection, works
   behind NAT and in the strictest environments. It doubles as a standalone
   feature — sharing an app with others — and a monetization surface. See
   [SHARE.md](SHARE.md).
5. **Direct transfer** — no `tui` on the other end at all: `gui` transfers
   straight to the terminal without the anchor rewriting. Slower, but works.

### Best-effort ssh pathing (the one open problem)

When the terminal owns the tiling and ssh-es each pane itself, escalate:

1. `tui ssh` — own the command.
2. **PATH shim** — own the lookup; covers terminals spawning `ssh` by name.
3. **`~/.ssh/config`** — own the config; read whatever spawns ssh:
   `ProxyCommand tui transport %h %p` runs our code locally per connect (tui is
   the encrypted transport pipe), `RemoteForward` gives `gui` the path back
   ([REVERSE_SSH.md](REVERSE_SSH.md)). Detached only; scope the stanza, since
   `Host *` clobbers a user's `ProxyJump`.
4. The `shared` transport — own nothing: `gui` dials out, the URL in the
   pane is the whole interface. See [SHARE.md](SHARE.md).

Measured against two such programs: herdr spawns `ssh` by name and includes the
user's ssh config, so rungs 2–3 reach it, and it has per-pane command options
besides; Supacode embeds libghostty (it *is* the terminal) and pins
`/usr/bin/ssh`, so only rung 3 applies, which its config-file handling permits
(no `-F`). Rung 3's ceiling — detached — is the honest maximum there, not a
defect.

## Open questions

- Does `tui` enable mouse reporting itself when wrapping a command that
  cannot ask for it?
- Which crate parses pane input; nothing found supplies pixel coordinates,
  only cells.
- Detach mode's position in the build order relative to the h.264/QUIC wire.
- Where `shared` sits in the ladder: below the web viewer or replacing it,
  and relay-first or WebRTC-p2p-first to build.
