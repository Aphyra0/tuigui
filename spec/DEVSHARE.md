# DEVSHARE — the IDE's run button, and handing the app to a coworker

Status: idea (design only, nothing built). A use-case study for
[SHARE.md](SHARE.md); the narrow slice of it that developers live in.

## The niche

GUI developers debugging software together. Someone can't figure out where a
setting lives, or is lost navigating the app, or needs a second pair of eyes on
a state only reproducible on their machine. The coworker wants to *drive*, not
to watch a screenshot loop — and today the options are: nothing, or a
full-meeting screen-share where the helper cannot click.

This is a real and felt gap, small in market and high in value. It needs
nothing new beyond what [SHARE.md](SHARE.md) and detach mode
([DIRECTION.md](DIRECTION.md)) already describe: a URL plus multi-party input.
The coworker clicks the link, gets the app in a browser, and the two of you
take turns clicking through to the right position while on a call.

One after another, not against each other: input is serialized somewhere
(one `gui` handle, one compositor), so the *turn-taking* problem is social and
the *correctness* problem is single-threaded by construction.

## The IDE angle: the run button

Zed supports remote ssh — some people even use its run button to start the app.
The idea: prefix each ssh Zed opens with `tui` (Zed's ssh is spawned as a
command, so this is the PATH/config rung from [DIRECTION.md](DIRECTION.md), or
a documented `tui ssh` wrapper). Then, on each run-button click:

```text
   zed ──run──▶ ssh (wrapped in tui) ──▶ gui, detached
                                         │
                          window pops up on the developer's machine
```

The run button starts `gui` in **detached mode** — no pane to render into, just
the native window through the same session. Effectively GUI forwarding for Zed
and similar IDEs, with zero workflow change: the developer presses the button
they already press, and the window appears.

Note what the wrapping buys that plain ssh does not: `tui` in the path of the
pane's stream is what makes *inline* possible, but for detached it is only
needed to *know the session exists* — to own the local end that opens the
window. The run button becomes the launcher.

### Measured against Zed's own docs

- **No GUI or display forwarding.** Remote development forwards text and
  language servers, not a display. Zed's own `port_forwards` is TCP `-L`/`-R`
  only, which carries a web server, not Wayland (that needs waypipe) and not
  X11 unless the ssh args carry `-X`/`-Y` — which Zed does accept as options,
  but which does nothing for a Wayland app and expects an X server on the
  local machine. So the claim holds: a GUI started remotely by Zed does not
  appear locally today.
- **It shells out to `ssh` on PATH,** and "will inherit any configuration you
  have in `~/.ssh/config`". So both rungs apply: a PATH shim reaches it, and
  `ProxyCommand` is honored. This answers the open question below.
- **One ControlMaster per project,** multiplexing the Zed protocol, every
  terminal, and every task through it. So the wrap happens once per project and
  `tui transport` as a `ProxyCommand` is per-connection, not per pane — the
  cheaper shape for exactly this idea.
- Zed may show ssh prompts (host keys, passphrases) in its own UI rather than
  the terminal; a wrapper that also wants to prompt must not swallow that.

## The share, from there

Once the window is up, the dev clicks share, copies the link, hands it to the
coworker on the call:

- The coworker's browser joins the same `gui` — the relay fans out, or WebRTC
  does p2p.
- Both can drive; turns are taken by talking, and the compositor serializes
  input so nothing interleaves wrongly.
- The helper's clicks land on the real running app, not a screenshot, so they
  can navigate *to* the position — which was the whole point.

For the IDE case the same stream serves two ends at once: the dev's detached
window and the coworker's browser. That is multi-viewer
([SHARE.md](SHARE.md)) with mixed clients, worth noting as a requirement
there, not a separate feature.

## Why this is the right first audience for sharing

- Developers already live in the terminal and the IDE; there is no install
  story to explain — the run button *is* the onboarding.
- The failure modes are cheap: a dev can read "the session failed" and a log
  file; a general audience cannot.
- It exercises everything the general case needs — relay, tokens,
  multi-viewer, turn-taking — on users who will report bugs instead of
  churning.

A niche, deliberately: it is the wedge the general sharing feature gets built
and hardened through.

## Open questions

- Watch-only vs drive tokens for the coworker link, and how the dev grants
  drive.
- Whether IDE detection matters at all (`gui -d` from a run configuration is
  enough) or a deeper integration (a Zed extension that surfaces the share
  link in the UI) is worth it later.
- Whether the ssh prompts Zed shows in its own UI survive a wrapper in the
  path.
