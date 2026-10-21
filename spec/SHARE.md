# SHARE — a transport that reaches through our servers

Status: idea (design only, nothing built).

## The idea

A transport, `shared`: `gui` dials out to a relay we run and receives a URL. It
draws that URL in the pane, next to the address it always draws. Whoever opens
the URL gets the app in a browser — canvas, keyboard, mouse.

One outbound connection is the whole requirement. It works behind NAT, in a
browser ssh session, from a phone client, in an environment that forbids local
code and ignores `~/.ssh/config`: everywhere the ladder in
[DIRECTION.md](DIRECTION.md) runs out, because every other rung needs
something local or a path back. This needs only that the app's machine can
reach the internet.

So it is two things at once: the escape hatch for when nothing works at all,
and a standalone feature — the same URL is a way to hand someone else the app.

## Two ways to carry it

- **Via our relay.** `gui` holds one outbound TLS connection; the browser
  connects to the relay; the relay pairs them by token. Always works, costs
  us bandwidth.
- **Peer to peer.** Browsers cannot dial a raw QUIC peer — WebTransport is
  browser-to-server over HTTP/3 only. Browser p2p is WebRTC: DataChannel for
  frames and input, ICE/STUN for the path, DTLS for the channel. With
  hole-punching it goes browser-to-`gui` directly; when it cannot, the same
  relay carries it, TURN-style.

The same shape as the native plan (p2p QUIC, relay fallback) with the protocol
the browser forces. The relay never needs to understand frames: pair, forward,
account.

## Sharing

The URL is a bearer capability, deliberately: "open this link, see my app" is
the point. On top of that:

- watch and drive as separate tokens, minted per audience
- multi-viewer: one `gui`, many browsers — the relay is already the fan-out
  point
- expiry and revocation, held by `gui`, the one side that can cut the stream

Monetization lives here rather than in the escape hatch: relay bandwidth and
persistent links are our cost, sharing with an audience is what someone pays
for. The plain escape hatch stays free and short-lived.

## What the strict environment sees

The pane prints the URL the way it prints the socket path today. Nothing is
required of the terminal, the ssh, or the muxer; the browser already in the
loop (the web ssh client) can open it in another tab.

## Open questions

- WebRTC or relay-first to start; WebRTC is more moving parts for less relay
  cost.
- End-to-end encryption on the relay leg: DTLS covers the p2p leg; a relay
  forwards plaintext unless we add our own layer above the pairing.
- Fan-out limits and per-viewer pacing when more than one browser is watching.
- Which of watch-only tokens, multi-viewer and expiry are worth building
  before anyone asks for them.
- Mixed clients on one session: a detached native window and a browser page
  at the same time, which the IDE case in [DEVSHARE.md](DEVSHARE.md) needs.
