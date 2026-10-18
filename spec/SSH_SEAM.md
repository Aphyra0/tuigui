# SSH SEAM — attaching a transport to an ssh we do not own

Status: proposal (design only, nothing built).

## Problem

The frames need a byte channel between `tui` and `gui`. The fast channel is an
ssh port-forward (`ssh -R remote_sock:local_sock`), which asks sshd to create a
socket on the far side and tunnel it back. Everything else we tried is worse:

- A multiplexer's image API is a **display** path, not a byte path, and is
  request/response: measured **~95 ms per request, one request per connection**
  against herdr's bridge. Unusable for frames.
- Borrowing someone else's ssh by attaching a forward to its control socket
  requires a live `ControlMaster`, and the multiplexer in question does not
  establish one (verified: no `-S`, no `-F`, no `-R`/`-L` in its argv, and no
  `~/.ssh/config` at all).
- p2p is a new egress. On a restricted network it is either dropped or a policy
  violation, so its failure mode is worse than not working.

So we want the forward, but we cannot change how the ssh is invoked, and we
should not silently change the user's environment.

## Core decision: the ssh is found by name, and we can stand in front of it

`ssh` is spawned as a **bare command name**, resolved through `PATH`. Verified on
the live process:

```
argv[0] = ssh
exe     = /nix/store/…-openssh-10.5p1/bin/ssh
```

and the spawning process's `PATH` begins with a user-writable directory:

```
PATH=/home/sun/.local/bin:/run/wrappers/bin:…
```

So a program placed earlier in `PATH` under the name `ssh` **becomes** the ssh
that gets spawned — no root, no FUSE, no mount, no kernel feature. It is an
ordinary file in the user's own directory, and moving it away undoes it
completely.

This is the seam. Verified: a shim on `PATH` receives the full argument list.

## Shape

```
                    PATH lookup
caller ──▶ "ssh" ──▶ ┌──────────────┐
                     │ our shim     │  1. add the forward we need
                     │              │  2. exec the real ssh with everything else untouched
                     └──────┬───────┘
                            ▼
                     the real ssh, with the original argv
```

The shim is a **pass-through with an addition**, never a replacement: it appends
its own options and `exec`s the real binary with the caller's argv intact. That
keeps every other behaviour (host keys, keys, options the caller passed) exactly
as it was.

## Wiring

1. `tui` writes the shim into a private directory, e.g.
   `$XDG_RUNTIME_DIR/tuigui/bin/ssh`.
2. The shim, when run, adds and `exec`s:

   ```
   exec REAL_SSH -o ExitOnForwardFailure=yes \
                 -R /tmp/tuigui/<session>.sock:/tmp/tuigui/<session>.local.sock \
                 "$@"
   ```

   `-R` and the `-o` are *prepended*; the caller's arguments follow unchanged, so
   a caller that passes its own `-o` still wins for anything we did not set.
3. `tui` puts that directory **first** in `PATH` for the processes it wants to
   reach through the tunnel — the multiplexer, or the command it spawns.

The shim only acts when exactly one ssh is spawned and kept open for the session.
Multiple ssh processes would each add the same forward; the second would fail to
bind. `ExitOnForwardFailure=yes` turns that into a loud failure rather than a
silent no-tunnel, and the shim's `-o` is what guarantees it.

## What this does and does not do

**Does:** gets a forward onto an ssh invocation we do not control, without a
config file, without root, without a kernel feature, and without the user typing
anything different.

**Does not:** change the ssh *arguments* the caller passed, change ssh's
behaviour otherwise, or leave anything behind that a `rm -rf` of the directory
does not undo.

**The honest risk:** a `PATH` shim is invisible. Somebody debugging ssh will find
that `ssh` is not the ssh they think it is. That is why this must be announced,
not silent — see below.

## Announcing it

At startup, `tui` says what it is about to do, in the terms the user cares about:

```
tui: shimming ssh to add a tunnel for frames
     /run/user/1000/tuigui/bin/ssh → /nix/store/…/bin/ssh
     add: -o ExitOnForwardFailure=yes -R /tmp/tuigui/<session>.sock:…
```

and requires confirmation before installing it, unless the user has already said
yes (a flag, or an environment variable). Silent environment modification is the
thing to avoid; a shim the user approved is just a tool.

## Requirements, per platform

| | |
|---|---|
| Linux | nothing special. `PATH` lookup and `exec` are enough. |
| macOS | same. The shim is a shell script or a copy of our binary with an argv check. |
| Both | no root, no FUSE, no mount, no kernel extension. |

**No crate is needed for this**, which is the point: the mechanism is `PATH` and
`exec`, both of which we already have. A cross-platform FUSE overlay
(`fuser`, 0.18.0, Linux + macOS) exists but on macOS requires macFUSE, a kernel
extension installed with root — a far larger ask than a script on `PATH`.

The one thing we do need is the **path of the real ssh**, so the shim can `exec`
it. That is resolved once, before the shim is installed, by skipping our own
directory in `PATH`. No hardcoded paths.

## When this is not available

If the ssh cannot be shimmed — it is absolute rather than looked up by name, or
the directory cannot be prepended — the transport is simply not offered. The
`Plan` skips a transport the server did not advertise, so a session with no
tunnel falls back to the local socket, which is correct for a same-machine
session and honest for a remote one.

## Open questions

1. **Who installs the shim.** `tui`, before it spawns the thing that will ssh. If
   the ssh is started by something already running (a multiplexer's server, a
   long-lived process), installing it now does not affect a process that already
   has its `PATH` and has already spawned ssh.
2. **Whether to shim once or per session.** A single stable directory with a
   session argument is simpler to explain than rewriting a file per run.
3. **The name of the escape hatch.** `--shim-ssh`, `TUI_SHIM_SSH=1`, or a
   confirmation prompt — a user who runs this every day should not be asked
   every time.
