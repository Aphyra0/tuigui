# CONNECT — dialing the client over the pty

Status: proposal (design only, nothing built).

## The idea

The server already sits in the middle of the byte stream, so it can be the one
that **dials**. A client does not need to know where the server is. It only needs
to announce itself, and the server — which controls the output buffer — will see
the announcement and use it to connect back.

The client **stdouts its endpoint id**. The server strips it off the output
stream, and now has everything it needs to dial the client over iroh.

This reverses today's direction. Today the server binds and publishes a session
file, and the client reads that file and dials. Here the client publishes and the
server dials.

## Why the server can see it at all

The server owns the last buffer before the real terminal. Everything the wrapped
command (and any multiplexer inside it) writes comes through the server's output
pump. That is the only reason the announcement reaches the server without any
side channel: it travels up the same path as normal text.

The requirement that falls out of this is that the pump must **not** forward the
announcement onward, or it would be drawn on screen. It has to recognize the
announcement, consume it, and pass everything else through byte for byte.

Today the pump is a raw copy and does no parsing at all (`tuigui-tui`
`Out::pump`). This is the one new component the idea needs.

## The announcement

One OSC sequence, using a private dispatch number:

```
ESC ] 3 0 0 0 1 ; <base64 payload> BEL
```

OSC because it has a numeric namespace and an explicit terminator, so it is
unambiguous to find and to delimit. If the stripper ever fails to recognize it,
terminals ignore an unknown OSC rather than corrupting the screen.

Payload: the client's iroh `EndpointId`, and nothing else is required.

## The handshake

1. The client starts an iroh endpoint and learns its own `EndpointId`.
2. The client writes the announcement to stdout.
3. The server strips the announcement inside its output pump.
4. The server dials that `EndpointId`.
5. The server checks `conn.remote_id()` equals the advertised id.

Step 5 is the entire authorization. iroh's connection is TLS and `remote_id()`
returns the peer key from the certificate, so a matching value proves the dialer
holds the private key for that endpoint. The advertised id is trustworthy
*because of where it arrived*: it came over the pty, which only the server and the
wrapped command can see.

No shared secret is needed. The advertisement carries the policy ("expect this
key"); iroh carries the proof.

## What iroh provides, and what it does not

Provided, for free:

- Finding the peer's address, hole punching, relay fallback, dialing by key
  alone. The client is reachable by `EndpointId` with no IP or port.
- Encryption, and the peer's verified identity via `remote_id()`.

Not provided:

- Authorization. Any key can dial an endpoint. Knowing *which* key connected is
  not the same as deciding it *may*. That decision is step 5.
- Anything about the client→server direction. `remote_id()` authenticates the
  dialer, so when the server dials, it is the *server* that gets verified. The
  client still needs its own reason to trust the connection, e.g. the nonce it
  put in the advertisement being echoed back.

## Locality

The same announcement works on one machine, where the socket path is the
transport instead of iroh. The mechanism is unchanged: the client announces,
the server strips and dials.

On one machine no secret is needed either, because a unix socket in a directory
only the user can reach is enforced by the kernel at `connect` time.

## Open problem: pane provenance

A pty has a single master fd, and a multiplexer multiplexes all its panes onto
it. So the announcement arrives with **no indication of which pane wrote it**.

With one pane this does not matter. With several it breaks twice:

- The server cannot tell which pane a stream belongs to, so it cannot draw the
  frames in the right place.
- The server cannot bind an announcement to a pane, so it cannot tell whose
  frames it is accepting.

Anything that fixes this needs the client to encode its own pane identity, plus a
way for the server to corroborate it. That is a separate design. This one should
be scoped to a single client until it is solved.
