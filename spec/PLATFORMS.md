# PLATFORMS — same client/window tree on every OS, Wayland is only the Linux backend

Status: decided (2026-10-22). Cross-platform plan, nothing built yet; the macOS
half starts after the wayland rework compiles green.

## Why this file exists

Smithay (our Linux compositor) does not run on macOS, and the first reaction to
learning that was a small panic — it sounded like the whole architecture was
non-portable. It is not. The panic came from reading the smithay crate as the
architecture. It is one backend. This file records what is portable and what
each OS supplies, so the question is settled instead of re-litigated.

## The decision

The architecture is the **client/window tree** — `RootClient`, `SubClient`,
`Window` (with children), attached caller data (`C` on clients, `W` on
windows), the capture/input plumbing, and the frame pipeline (rkyv wire →
kitty/TGP paint). That shape is platform-neutral. Each OS implements it against
that OS's native display API:

- **Linux**: the smithay compositor (current `wayland` crate). We *are* the
  Wayland compositor (plus XWayland for X11 apps). Windows are XDG toplevels,
  inputs come through the seat we own, capture is shm buffer reads. Full
  control over admission, configure, resize.
- **macOS**: a second backend implementing the same types against Quartz /
  WindowServer. No Wayland bridge, no X11 bridge — native apps never speak
  either, and bridging would reimplement the protocol as a WindowServer client
  (Cocoa-Way territory, research-grade) for no benefit, since we only need
  pixels in and events out.

The `C`/`W` generics and everything downstream of a captured `Frame` carry
over untouched. Only server lifecycle (`new`/`step`) and the capture path get
per-OS implementations.

## macOS mechanics (the Quartz backend)

Every piece has a native mechanism; nothing is missing, only different:

| Piece            | Linux (smithay)         | macOS (Quartz)                          |
|------------------|-------------------------|-----------------------------------------|
| Connect          | wayland socket          | inherited at spawn — no env var; the Mach bootstrap namespace points the child at WindowServer |
| RootClient       | first socket connection | the spawned child                       |
| Windows          | xdg_shell toplevels     | `CGWindowListCopyWindowInfo` filtered by `kCGWindowOwnerPID` |
| SubClients       | later socket connections| helper processes matched by parent pid (sysinfo subtree + `kCGWindowOwnerPID`) |
| PIDs             | `SO_PEERCRED` per socket| full process tree from sysinfo (parent pid, liveness, no permission needed) — `pid` on the client types is universal |

macOS attribution model: each client (root or sub) binds to its pid; its
windows are `kCGWindowOwnerPID == pid`. That set is flat on macOS — a
subclient's sibling windows are modeled as `Window.children` of its
root window, not as fake XDG-style nesting. Because the whole tree is
verifiable, a SubClient cannot be mistaken for the root and vice versa.
| Capture          | shm buffer on commit    | `CGWindowListCreateImage` or ScreenCaptureKit, per window; works while occluded/offscreen |
| Damage           | commit damage rects     | none — diff frames ourselves (as the h264 path already does) |
| Inputs           | seat, we own it         | `CGEventPostToPid` (mouse/keyboard at the pid) — needs the Accessibility permission |
| Resize/configure | we are the compositor   | no public Quartz API; AX API (`AXUIElementSetAttributeValue`, `kAXSizeAttribute`) or AppleScript — same Accessibility permission |
| Headless         | no display needed at all| virtual/dummy display (the macOS xvfb): app believes it is visible, keeps drawing, nothing shows |

Permissions are the macOS tax: Screen Recording (capture) and Accessibility
(inputs + resize), one TCC prompt each.

## The macOS headless mode

Native GUI apps cannot run without a WindowServer session, and "hidden" windows
freeze when the app honors `occlusionState = NotVisible` and stops drawing. So
macOS runs the program on a **virtual display** — the app thinks it is
onscreen and keeps rendering; we capture from the backing store. Headless macs
get a virtual display automatically (Apple silicon, no monitor); extra ones can
be created (BetterDisplay / CoreGraphics virtual display). This is the macOS
equivalent of "the compositor exists only in our process" on Linux.

## What this means for the current rework

Keep the shared types (`RootClient`, `SubClient`, `Window`, capture/input
plumbing) as the platform-neutral core, with smithay-vs-Quartz as a backend
module — not two crates forking the tree model twice. Build order stays:
finish the Linux rework first, macOS backend after it compiles green.
