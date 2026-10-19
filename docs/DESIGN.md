# easytgp — Settled Architecture (session decisions, 2026-10-08)

This is the design contract for the crate. Everything here was argued to
settlement; implement it, don't redesign it. When in doubt, this file wins
over code comments.

## Public model (one sentence per component)

- **Router = pure demux.** Parses replies (`Message::parse_next`), matches
  them to outstanding ops, fills `ready`, wakes. It never sends a byte, never
  runs user code, never recovers. Spawned by `Controller::init` (NOT commented
  out like today).
- **Ops are sync.** `send()`/`place()`/`new_frame()` = validate client-side,
  encode, emit bytes under the chunk LOCK, return handles. No async in the
  wire path. Client-side errors via `Result` (`?`) — these are the *user-fault*
  class and must be caught before the wire (see error taxonomy).
- **`ready()` is the single settlement + recovery gate.** Awaited by async
  hosts, `.wait()`/`wait_timeout()` (park) by sync hosts, `try_ready()` (poll)
  by render loops. Semantics: "everything I sent on this image is settled."
  Implementation: settled = own ack seen OR any later reply seen (FIFO).
  For images addressed by `I=` (pre-ack), settlement rides the ops' own acks.
- **`restore()` is the only recovery verb.** Called BY THE USER at the point
  where `ready()` (or a handle's await) returned `Err(Evicted(restorable))`.
  Library replays what it stored (request + `Arc<PlacementRequest>`s);
  animated images hand the job to the app (it owns frame content).

## Ack attribution (replaces marker-per-op)

- **Op's own ack is its marker.** `i=<id>` echoes for id-addressed ops,
  `I=N` echo for `a=t,I=N` transmits (reply: `i=<assigned>,I=N;OK`),
  `i=<img>,p=<pid>` for placed-with-pid. Pending map keys = identities the
  op already had. No separate marker counter for these.
- **Trailing barrier query** (`a=q,i=M,f=24,s=1,v=1;AAAA`) only where own-ack
  is impossible: ambiguous same-image acks (`a=f`/`a=c`/transmit all echo bare
  `i=` — identical replies), silent ops (`a=d`, `a=a` reply nothing), and
  silent-on-success terminals (contour acks failures only).
  Marker ids must be `i=`, never `I=` (TERMS.md implication 9).
- **Reply shapes** (see TERMS.md): `i=M;OK` / `errno:msg`; place acks
  `i=<id>,p=<pid>;OK`; konsole strips extra keys; contour silent on success.
  Disambiguation needs: disjoint value namespaces (markers vs image ids) +
  router membership test + FIFO `last` attachment for data messages.
- **`q=` rules:** never suppress an op whose outcome `ready()` vouches for.
  `q=2` on an op destroys its only verdict (FIFO "settled" ≠ "succeeded").
  `q=2` only for deliberate fire-and-forget, never in `send_raw` globally
  (today's lib.rs:1020 hardcode is a bug).

## Futures / wait mechanics (all hand-rolled, std-only)

- `ReplyFuture: impl Future` stays — executor-agnostic, works under tokio
  AND smol. No tokio/async-std dependency, ever. `impl<R: Unpin> Future for
  PendingOperation<R>` via `Pin::get_mut` + `Pin::new` (both structs Unpin).
- **`wait()`**: loop { poll-now → park() } with the thread's own waker
  installed into `pending` before parking (waker must match whoever waits;
  `wait(self)` consumes to make misuse a compile error). `wake()` = `unpark()`
  via the same `Waker` vtable — router unchanged for both consumer types.
  `wait_timeout(d)` = `park_timeout` → "skip the frame if not ready".
  Spurious wakeups: always check-then-park, loop. Don't hold locks across park.
- Re-insert waker every poll (don't `contains_key`-guard); `impl Drop` on
  pending removes from `pending`/`ready` (dead-waker hygiene).
- Async ≠ park: same suspend-register-wake-recheck mechanism, but futures are
  bytes and threads are stacks. Few shallow waits → sync wins; 10k concurrent
  waits → tokio path (free via the same wakers). Per-op async tax ~100s ns —
  why the wire path stays sync.

## Transmits, ids, creation

- `transmit()` uses **`I=N`** and returns `PendingOperation<DynImage>`;
  awaiting it yields the `DynImage` built from the ack's assigned `i=`
  (closure parses `i=` from `i=<assigned>,I=N;OK`). Blocking init is
  EXPLICIT by design — one round-trip at creation, then everything sync.
  Rejected alternative: `DynId::Waiting` deferred id resolved lazily on next
  op — hidden, breaks `ready()` settlement, raises eyebrows.
- `I=N` number resolution = newest image with that number (spec + kitty
  `img_by_client_number`). Client must not reuse `N` while ops referencing the
  old image are outstanding. `I=` also powers migration (below).
- Frame numbers: client-counted (`frames.len()+2`), matches kitty's
  `extra_framecnt + 2` assignment (graphics.c:1870-72). Frame acks are NOT
  uniquely correlatable (bare `i=`) except ghostty's `r=` extension.

## Error taxonomy (the load-bearing contract)

**User-fault (deterministic on retry; catch client-side or make
unrepresentable; loud failure, no recovery):**
malformed cds, unknown format/tt/compression, `i=`+`I=`, zero dims,
>7680px, frame>base, rect OOB, overlap, `ECYCLE`/`ETOODEEP`/`ENOPARENT`,
self-parent, bookkeeping ENOENT (never-created id/frame), own `EILSEQ`
(chunk interleave), own `ENODATA` (payload < declared), `EFBIG`.

**Not user-fault (recovery meaningful; surfaced via `ready()`):**
- `Evicted` — ENOENT on an **ever-OK'd** id: terminal quota/scrollback
  eviction OR another client's retransmit over our id (spec :520 replace
  semantics). Recovery = `restore()` (stills: replay stored request +
  placements; animation: app re-decodes/restarts). `I=`-retransmit on heal
  gets fresh id, no stomp-war with a buggy co-client.
- `ResourcePressure` — ENOSPC/ENOMEM (kitty cache ×5 storage_limit,
  contour quota/MaxAnimationFrames=128/32MB chunked, iTerm2 id-space).
  Recovery = shed + retry (client-owned frames rotation policy).
- `TransportLost` — no reply ever (terminal died, session reset,
  unsupported terminal). Router timeout, not errno.
- `Unknown` — decode skew (wayst/contour re-decode our PNG), foreign EILSEQ
  from a co-client's in-flight chunks through a muxer. Retry-once, else crash
  by policy (TUIs may panic; fine).

Same errno can land in different classes — classify by router history
(was this id ever OK'd?) not by errno string alone. Parse errno symbol,
keep message opaque. Note: wezterm/rio/konsole silently ignore malformed ops
(no error arrives) — client-side validation is mandatory, error arrival
cannot be relied on.

## Runtime invariants (verified, load-bearing)

- FIFO ack ordering per-terminal: verified across kitty/ghostty/wezterm/rio/
  contour/iTerm2/konsole/wayst (TERMS.md 2026-10 pass). FIFO is per-wire;
  cross-task ordering needs a happens-before edge (channels suffice) OR the
  app serializes ops through one task.
- Router thread: blocking read on pty = kernel-descheduled until bytes
  (park/free). Canonical mode HOLDS replies until newline — termios raw-ish
  mode (ICANON/ECHO off) MUST be set before spawning router (the commented
  block at lib.rs:38-46 is the fix; put it in `ReplyRouter::start`, not in
  the lazy slab init).
- `map_while(Result::ok)` on stdin bytes kills the router on any transient
  read error — replace with explicit error handling.
- Chunked load = critical section (LOCK) + kitty's single `currently_loading`
  slot; a frozen slot self-clears on the next complete command
  (`free_load_data` first). Only first chunk carries full controls.
- Chunks: 4096 limit is on BASE64 chars (multiple of 4), not raw bytes.
  `b as char`/`from_utf8().unwrap()` in send path: safe for base64 payloads
  only.
- Stdin single-reader invariant: router owns stdin exclusively (two blocking
  readers = nondeterministic byte split).

## API surface decisions

- Sync-first: public methods sync; async exists only as `impl Future` on
  handles + `PendingOperation<DynImage>` at creation.
- ratatui/iocraft integration = separate thin crates (`easytgp-ratatui`),
  core stays framework-free. Widget: `impl Widget for &mut ImageView`,
  blank cells for image area, re-place on geometry diff, transmit at
  construction (never in render), `checkpoint()` after `terminal.draw()`.
  Images don't scroll with content (screen-space) — re-place per scroll tick
  (or later: kitty Unicode placeholders U=, cell-anchored).
- ratatui is executor-agnostic/sync-render; iocraft async at handlers only,
  sync render. Both: never await/block inside render.
- Client-side rasterize at destination size (cols×cell_w, rows×cell_h) —
  never rely on terminal scaling (no filter key in protocol; kitty linear,
  ghostty nearest). Pixel-perfect cross-terminal = 1:1 transmit.
- Scale/limits assumptions encoded as constants: ≤128 frames/image,
  ≤32MB chunked total, 4096 base64 chunks. Frames rotation (delete oldest)
  is USER policy via exposed frame handles.
- Keep raw `Message` public under a low-level feature (widget authors need
  the escape hatch).

## Explicitly settled (don't reopen)

- `yield_now`/detach-everything/`(R, verifier)` tuples — GONE. Creation is
  the only awaited thing; settlement is `ready()`.
- Router-side auto-bootstrap/callback-on-evict — GONE. `restore()` user-side
  only. (Callback variant discussed and parked; user may revisit.)
- `I=` for everything "to dodge bad clients" — NO. `I=` at transmit +
  fresh-`I=` migration only. Hostile id-targeting = out of scope (muxer/user
  responsibility).
- Wayst ENODATA-on-continuation as environmental — NO (transport can't drop
  middle bytes; it's user-fault or silence).
- Server-side "parks thread on empty stream" — free (kernel wait queue),
  no condvar/self-pipe needed unless multi-source wake appears.

## Next steps (vertical slice)

1. Commit session artifacts (TERMS.md, Cargo.toml).
2. Branch; implement: termios setup + router spawn + `I=` transmit await
   path + `place` (own-ack) + `ready()`/`wait()` + `try_ready()`.
3. Test in kitty: one PNG transmit+place+display; then video loop with
   `wait_timeout` frame-skip.
4. Then: `restore()` replay, eviction test (scrollback/quota), ratatui
   widget skeleton.
