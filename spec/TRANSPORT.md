# TRANSPORT — pluggable channels between client and server

Status: proposal (design only, nothing built).

## Problem

The client and the server need one thing from each other: a **byte stream**.
Today that is a unix socket at a path written into the session file, and it is
hardcoded: `gui` has a `Sink` enum with one variant, and `tui` binds one socket.

That is right for the common case and wrong for two others:

- **Split across machines.** The socket cannot cross a network. Riding ssh
  instead means borrowing a connection that already belongs to a shell or a
  multiplexer, which brings policy (`AllowStreamLocalForwarding`,
  `StreamLocalBindUnlink`, `ExitOnForwardFailure`), a UX cost (the user must
  invoke something new), or a coupling to one muxer's image API.
- **Same process.** When both halves run in one process there is no reason to
  involve the kernel at all.

So the channel needs to be a **choice**, and the choice needs a strategy: try
what is best, fall back cleanly, and tell the user when the answer is something
they would want to know about.

## Core decision: the abstraction is a byte stream, not a message channel

`Msg` framing already exists in `tuigui-wire` and is transport-independent. The
seam therefore goes **below** framing: a transport produces a bidirectional
stream, and `Msg::read`/`write` run over it unchanged.

This is what makes the abstraction cheap. A transport does not implement
send/recv, reconnect, or framing. It implements "give me a stream".

```
Connection = a stream + where it came from
Transport  = the factory that produces Connections
Plan       = the transport list, ranked, with fallback
Advert     = how a server publishes what it can be reached on
```

`AsyncRead`/`AsyncWrite` are poll-based and already object-safe, so they box
with no new dependency. Only `Transport`'s own `async fn`s need boxing.

## The names

### `Stream` — what boxed streams are

```rust
pub trait Stream: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> Stream for T {}
```

A blanket impl. No transport implements this by hand; it exists so
`Box<dyn Stream>` reads as one type instead of three bounds.

### `Connection` — a struct, not a trait

```rust
pub struct Connection {
    stream: Box<dyn Stream>,
    transport: &'static str,   // "unix" | "memory" | "p2p"
    peer: String,              // for logs
}

impl Connection {
    pub fn transport(&self) -> &'static str;
    pub fn peer(&self) -> &str;
    pub fn is_local(&self) -> bool;
}

impl AsyncRead  for Connection { /* delegate to stream */ }
impl AsyncWrite for Connection { /* delegate to stream */ }
```

A struct rather than a trait because an established link is *a stream plus
provenance*. As a trait, every transport would have to re-implement the same
accessors. `Connection` is what `gui`'s `enum Sink` becomes: it holds whatever
transport won, and the caller does not branch on which.

### `Transport` — the factory

```rust
pub trait Transport: Send + Sync {
    /// Stable key. Matched against `Advert`s, and selectable by name.
    fn name(&self) -> &'static str;

    /// What this transport promises. Drives ranking and warnings.
    fn capabilities(&self) -> Capabilities;

    /// Dial the peer described by `advert`.
    fn connect<'a>(&'a self, advert: &'a Advert) -> BoxFuture<'a, io::Result<Connection>>;

    /// Accept peers, if this transport can listen. `None` means dial-only.
    fn listen<'a>(&'a self) -> BoxFuture<'a, io::Result<Option<Listener>>>;
}

pub struct Listener { /* transport: &'static str, … */ }

impl Listener {
    pub fn accept<'a>(&'a self) -> BoxFuture<'a, io::Result<Connection>>;
}
```

`listen` returning `Option` covers a transport that is only dialable from one
side, without needing a second trait.

### `Capabilities` — one place that decides ranking and warnings

```rust
pub struct Capabilities {
    /// Both ends in one process. No syscalls, no serialization.
    pub in_process: bool,
    /// Traffic never leaves this machine.
    pub local: bool,
    /// Opaque to anyone without the peer's key.
    pub encrypted: bool,
    /// Establishment may require a third-party relay.
    pub may_relay: bool,
    /// Shown to the user when this transport is chosen. `None` = silent.
    pub warn: Option<&'static str>,
}

impl Capabilities {
    /// Lower is better. Derived, so adding a transport needs no ordering table.
    pub fn rank(&self) -> u8 {
        match (self.in_process, self.local, self.may_relay) {
            (true, _, _)  => 0,   // same process
            (_, true, _)  => 1,   // unix socket
            (_, _, false) => 2,   // direct, over a network
            (_, _, true)  => 3,   // relayed
        }
    }
}
```

| transport | rank | warn |
|---|---|---|
| `memory` | 0 | none |
| `unix` | 1 | none |
| `p2p` (direct) | 2 | "bypasses ssh; traffic leaves your network" |
| `p2p` (relayed) | 3 | same, plus "via relay `<host>`" |

### `Advert` and `Session` — what the server publishes

The session file stops being one socket and becomes a list of what the server
can be reached on.

```rust
pub struct Advert {
    pub transport: &'static str,
    pub params: BTreeMap<String, String>,
}

pub struct Session {
    pub adverts: Vec<Advert>,
    pub image_id: u32,
    pub cols: u32,
    pub rows: u32,
}
```

```
transport=unix socket=/tmp/tuigui.sock
transport=p2p node=8f3a… relay=https://relay.example
image_id=0
cols=142
rows=45
```

Repeated keys, so the file stays line-oriented and inspectable by hand — the
property the current format was chosen for. A server that offers no transport
publishes no `transport=` line, and a client with nothing to dial is an explicit
error rather than a hang.

### `Plan` — the strategy

```rust
pub struct Plan { /* candidates: Vec<Box<dyn Transport>> */ }

impl Plan {
    pub fn new() -> Self;

    /// Opt a transport in. Anything not allowed here is never tried.
    pub fn allow(self, transport: impl Transport + 'static) -> Self;

    /// Best-first, by `Capabilities::rank`.
    pub fn ranked(&self) -> Vec<&dyn Transport>;

    /// What will be tried, in order, for the log.
    pub fn describe(&self) -> Vec<&'static str>;

    /// Rank, match against what was published, try until one works.
    pub async fn connect(&self, published: &[Advert]) -> Result<Connection, NoTransport>;
}
```

Resolution, in full:

```rust
pub async fn connect(&self, published: &[Advert]) -> Result<Connection, NoTransport> {
    let mut tried = Vec::new();
    for transport in self.ranked() {
        // A transport the server did not publish is not a failure: it is simply
        // not on offer. Only a transport that was offered and failed is reported.
        let Some(advert) = published.iter().find(|a| a.transport == transport.name()) else {
            continue;
        };
        match transport.connect(advert).await {
            Ok(conn) => {
                if let Some(warn) = transport.capabilities().warn {
                    eprintln!("tui: connected over {} — {warn}", transport.name());
                }
                return Ok(conn);
            }
            Err(e) => tried.push((transport.name(), e)),
        }
    }
    Err(NoTransport { tried })
}
```

Two properties worth stating: a transport the server did not publish is skipped
silently, so partial availability is normal; and `warn` fires on **success**, at
the moment the user wants to know, rather than at configuration time.

## Who builds the plan

The plan is a constructor argument, exactly like `stdin` and `stdout`. That is
what keeps it out of the widget and lets each binary choose.

| binary | plan | why |
|---|---|---|
| `tui` | `[unix]`, `+p2p` if asked | unix is the server's own address |
| `gui` | same as the server offers | filtered by `Advert`s |
| `tuigui` | `[memory, unix]` | both halves in-process; memory wins at rank 0 |

`memory` is the in-process case: a `tokio::io::duplex` pair, no syscalls, no
session file. Because it ranks 0, `tuigui` never reaches for a socket when both
halves are already in one process. It is also the only transport that is
meaningless outside `tuigui`, so only `tuigui` puts it in the plan.

**p2p is never in a default plan.** It appears only when the user asks for it,
and even then it ranks last, so it never wins over something quieter that already
works. Its `warn` is not decoration: it is the difference between a user choosing
a route and a user discovering one.

Both binaries must agree on which transports are allowed, or the client will try
one the server never offered. Publishing allowed transports in the `Advert`s
solves this for free: the server advertises only what it listens on, so the
client's plan is filtered by what actually exists.

## What is deliberately not here

- **The inline stdout channel.** Encoding frames as characters in a pane is
  possible, and its ceiling is real (~32 KB per screen at five combining marks
  per cell, with a full repaint per packet, and it is one-directional so a client
  cannot pace itself). It is a fallback for a muxer that permits nothing else,
  not a transport. If it is built it slots in as another `Transport` with a
  `warn`, since the abstraction does not care how bytes are carried.
- **Reusing someone else's ssh.** It is either an invasion of a connection that
  belongs to a shell or a muxer, or it is a prefix the user must type. Both are
  the user's call, not ours.
- **A muxer's image API.** Efficient and available without user action, but it
  defines *rendering*, not transport. It composes with this design rather than
  competing with it: `tui` decides how to draw with what it received, whichever
  transport carried it.

## Files

```
crates/tuigui-wire/src/
  proto.rs        ← renamed from p2p.rs: Msg only, no transport knowledge
  session.rs      ← Session { adverts, … }
  link/
    mod.rs        Connection, Stream, Transport, Listener, Capabilities, Advert, Plan
    memory.rs     rank 0, tokio::io::duplex
    unix.rs       rank 1
    p2p.rs        rank 2/3, feature-gated, off by default
```

The rename matters: `p2p.rs` today is the *message format*, not peer-to-peer.
Leaving that name in place would make `link/p2p.rs` ambiguous.

## Open questions

1. **Trait or enum for the transport set.** A trait gives `Box<dyn>` and open
   extension; an enum gives exhaustive matching and no boxing, and the compiler
   flags every site when a variant is added. With three transports known, the
   enum is simpler. Start with the trait's shape, use an enum until a plugin path
   appears.
2. **Whether a failed `connect` should be retried or skipped.** Today's sketch
   moves to the next transport, which is right for "not on offer" and arguable
   for "offered but broken".
3. **Whether `memory` should also be available to `gui` when a caller supplies
   both halves.** It is rank 0, so allowing it costs nothing when unused.
