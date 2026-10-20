# DRIFT — the picture shifts under a multiplexer

Status: **open bug**, reproduced by hand, cause not found. Nothing here is built to
fix it.

## The symptom

Run `tui herdr`, then start `gui <video>` in a herdr pane. The whole screen
appears to move **down by a small amount** — often described as one row, or "a
bit". It is a **shift, not a scroll**: the content cannot be scrolled back to,
so nothing entered the scrollback. The offset is in the real terminal's view.

Three observations that narrow it a great deal:

1. **herdr redrawing fixes it.** Switching focus to another pane and back — any
   full repaint of the affected pane — puts everything back where it belongs.
2. **Ctrl+C on the `gui` process also shows it**, and leaves the shell prompt
   one row below where it should be, with a blank row between.
3. It happens **inside a herdr pane**, not on a bare terminal.

## What those three rule out

**It is not the forwarded bytes.** If `tui` were passing through something that
moved the cursor, herdr's own output would be the thing that drifted and herdr
would be equally wrong on a bare terminal. The forwarding path is byte-for-byte
a pipe (`proxy.rs`), and herdr is its first consumer — if `tui` mangled the
stream, plain `tui <anything>`, with no `gui` involved, would show it too.

**It is not herdr's layout being wrong.** Point 1 is decisive: a repaint repairs
it, so herdr's cell grid is correct and herdr positions absolutely
(`ESC[row;colH`). Whatever moved was between herdr's writes, and the next
absolute positioning overwrote it.

**It is not a scroll.** Point 2 — content cannot be scrolled back — means no
newline reached the bottom row. A missing wrap, or a stray line feed, would have
scrolled; this only moved the cursor.

So: something **moved the real terminal's cursor**, and herdr's next draw fixed
it. That leaves the only writes on that fd which are ours rather than herdr's.

## The candidates, and what is known about each

`tui` writes to the real terminal through one `Sink`, from three places:

| writer | what it emits | when |
|---|---|---|
| the proxy | the command's bytes, unchanged | always |
| `session.rs` | `MoveTo(0, 0)` — `ESC[1;1H` | only under `--dbg-no-vplacement` |
| `easytgp` | the graphics escapes: `a=T`, placement keys, `C=1` | every frame |

`MoveTo(0, 0)` is the natural suspect: it is a bare cursor move with no draw, so
it moves the cursor out from under herdr and nothing repaints it. Two things
weaken that, and they are why this is still open:

- It only runs when `--dbg-no-vplacement` is set, and the drift has been seen
  without deciding one way or the other whether that flag was on.
- `MoveTo` emits nothing when the cursor is already at the origin, so in a
  stream it fires rarely — hard to line up with a shift that appears on a
  single Ctrl+C.

The graphics escapes are the other suspect, and the more likely one: they are
what `tui` adds that a plain pipe does not, and the drift appears when a frame
is being painted. `C=1` exists to *stop* the terminal moving the cursor after a
placement, so a placement that moves it anyway is a candidate for exactly this.

## What was tried

- **Terminal modes in the tty.** `Raw::install` (`crates/tui/src/raw.rs`) puts
  fd 0 in raw mode and restores it on drop. Checked that `ISIG` is deliberately
  cleared, so `^C` reaches the command rather than signalling `tui`.
- **`?7l` (disable autowrap) at startup.** Added to `Raw::install`, built, and
  **it did not change the drift**. Reverted. The reasoning had been that a
  full-screen writer hitting the last column would wrap and push the display
  down; the reasoning was plausible and the measurement said no. It also wrote
  to `io::stdout()` directly rather than through the `Sink`, which is a race
  with the proxy that was never worth having.
- **herdr's own modes, read from source** (`/tmp/herdr-src`). It uses
  `ratatui::init()` for raw mode, plus mouse capture, bracketed paste,
  focus-change, `?7l`, and keyboard-enhancement pushes; its restore turns the
  same off. Nothing unusual, and nothing that `Make_raw` fails to cover — they
  are output modes on herdr's stdout, not tty state.

## Why it should be fixable rather than fundamental

herdr is itself a full-screen ratatui client that positions every cell, wrapping
a pane whose process is another full-screen program — structurally the same
shape as `tui` wrapping `gui`. It works. So the shape is known to work, and
`tui` does *strictly less* than herdr: it sets termios on fd 0, forwards bytes,
and adds graphics escapes. The fault must be in the last of those, because the
first two are what herdr also does.

## The next step, which is a measurement and not a guess

Two hypotheses have been wrong already, so the next move is to stop inferring
and **record every byte `tui` writes to fd 1** — an env-gated tee on the sink,
off by default. Then reproduce the shift and read off which escape lands where
the drift appears.

The specific question that would settle it fastest: does the drift happen with
`--dbg-blue-pixel` (which repaints the same image forever, so any per-frame
difference is eliminated), and does it happen with a single frame rather than a
stream? One frame and no drift would put it in the repeat placement rather than
in the first one.

## Related

- `spec/TRANSPORT.md` — the channel, and why the server never forwards graphics
  through the muxer.
- `spec/REMOTE.md` — the placeholder grid, which is what makes a muxer
  irrelevant to whether the picture appears. This bug is about where it lands,
  not whether it is drawn.
