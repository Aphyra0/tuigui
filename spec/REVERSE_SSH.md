# REVERSE SSH — dialing back to the side that started the session

Status: idea (design only, nothing built).

## The idea

A pane reached over `ssh` knows where the user is sitting: `sshd` puts it in
`SSH_CONNECTION`. So the pane side can turn the session around and **ssh back**
to the terminal side.

```text
   tui  <── ssh back ──  gui
```

No forward to arrange, no shim in front of someone else's ssh, nothing
installed. The one fact needed is already in the pane's environment. That is
what [SSH_SEAM.md](SSH_SEAM.md) cannot avoid and this can.

## What `SSH_CONNECTION` gives

```text
   "203.0.113.5 51234 198.51.100.7 22"
    ▲           ▲     ▲             ▲
    │           │     │             └─ the pane's machine
    │           │     └─────────────── the pane's address
    │           └───────────────────── the ssh client's *source* port — do not dial it
    └───────────────────────────────── the user's address — dial this
```

`client_port` is an ephemeral source port of an established connection: nothing
listens there. The port to reach the terminal side on is ours to agree, by
default or by argument on both sides.

`SSH_CONNECTION` proves nothing — anything in the session can set it. Identity
comes from the handshake, as with every other transport.

## The password

The dial back is an `ssh`, so its authorization is ssh's. If key auth works
there is nothing to do and the session is automatic. If that sshd requires a
password, `gui` asks for it — **in the pane**, because the pane is the only
screen it owns and there may be no terminal in its process tree to prompt on.
That means `gui` reads input, which it does not do today.

No password of our own, no separate scheme: the remote sshd decides, and we
relay the question.

## What the pane side offers

```text
   gui --transport unix           default: listen locally, be dialed
   gui --transport reverse-ssh    ssh back to SSH_CONNECTION
   gui                            auto: reverse-ssh when SSH_CONNECTION is set
```

and draws which it chose, plus the address it is trying when a dial fails. A
pane that says nothing is a pane nobody can debug.

## When it cannot work

- **No `SSH_CONNECTION`** — not an ssh session, so it is not offered. The common
  case, not an error.
- **NAT** — the recorded address is the router's, and the dial goes nowhere.
- **A jump host** — the address is the bastion's.
- **Inbound blocked** — the dial times out.

None of these is detectable before trying, which is why this is a fallback and
not a default.

## Consequences

- It is a second `Transport`, so **ranking comes back** — the machinery
  [TRANSPORT.md](TRANSPORT.md) left out until there were two.
- The direction is already allowed: either end may dial.

## Open questions

1. **The port**, and whether a fixed default is acceptable as a fixed target.
2. **Listening on both** the socket and the port, so a failed dial back is not a
   dead session.
3. **Whether to corroborate the address** against anything, which locally we
   cannot.
