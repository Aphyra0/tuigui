# Terminal Inline-Graphics Protocol — Implementation Survey

How the popular terminals *actually* process the kitty graphics protocol
(and related inline-image protocols: sixel, iTerm2 inline images), focused on
three questions that matter for the `easytgp` client library:

1. **Sequential or parallel?** Is each graphics command fully processed before
   the next one is touched, or can two commands run concurrently?
2. **Are the acks exactly FIFO?** Does the terminal emit a reply
   deterministically in the same order as the corresponding command, with no
   reordering, suppression ambiguity, or asynchrony?
3. **Any pipelining optimizations?** Does the implementation batch, defer,
   parallelize, or reorder work in a way that breaks naive "next ack = my Nth
   op" correlation?

A historical note that colors everything below: **the spec itself explicitly
does not guarantee FIFO.** It says terminal emulators "must reply to query
actions immediately without processing other input. Most terminal emulators
handle input in a FIFO manner, anyway" — the MUST binds only to queries and
the FIFO sentence is descriptive, not normative. So any ordering guarantee you
rely on is per-implementation, and must be verified per-terminal.

---

## Summary table

| Terminal          | Lang   | TGP? | Processing model              | Ack delivery          | `I=`→real `i=` echo in ack | Default reply |
|-------------------|--------|------|-------------------------------|-----------------------|----------------------------|---------------|
| kitty             | C      | Full | sequential, inline            | inline `write`        | yes (`i=`+`,I=`)            | `q=0` (all)   |
| ghostty           | Zig    | Full | sequential, inline            | inline `writePty`     | yes (`i=`+`,I=`)            | `.no` (all)   |
| wezterm           | Rust   | Full | sequential, inline            | inline `writer.write` | yes                          | `Verbose` (all) |
| rio               | Rust   | Full | sequential, inline post       | via event-proxy queue | yes                          | `q=0` (all)   |
| contour           | C++    | Full | sequential, inline            | inline `terminal.reply`| yes (`i=`+`,I=`)            | `q=0` (all)   |
| iTerm2            | ObjC   | Full | sequential, inline            | side-effect queue (FIFO, async) | yes (verbosity-gated)       | `.normal` (all) |
| konsole           | C++    | Partial | sequential, inline          | inline `sendString`   | yes (`i=`+`,I=`/`,p=`)      | `q=0` (all)   |
| wayst             | C      | Full | sequential, inline            | inline output buffer  | `i=` only (never `I=`)      | query: always; ops `q<2` |
| tmux              | C      | No (sixel) | sequential              | n/a (no TGP ack)      | n/a                         | n/a           |
| foot              | C      | No (sixel, kitty kbd) | seq     | n/a                    | n/a                         | n/a           |
| Windows Terminal  | C++    | No (sixel, kitty kbd) | seq     | n/a                    | n/a                         | n/a           |
| VTE (gnome)       | C++    | No (sixel) | sequential                | n/a                    | n/a                         | n/a           |
| mlterm            | C      | No      | n/a                         | n/a                    | n/a                         | n/a           |
| alacritty         | Rust   | No (kitty kbd only) | n/a         | n/a                    | n/a                         | n/a           |

Legend: **TGP** = implements the kitty graphics protocol APC (`a=t/T/p/d/f/c/a/q`)
or a subset. "Full" = t/T/p/d/q plus (usually) animation; "Partial" = a subset.

Behaviour summarized: **every terminal that implements TGP processes commands
strictly sequentially and synchronously.** None parallelizes, batches,
reorders, or defers the *processing* of graphics commands. The only deviations
are in reply *delivery*: **rio** defers the reply write onto an event-proxy
queue and **iTerm2** delivers it through its token-executor side-effect queue —
both strictly ordered, just asynchronous. Sixel-only and kitty-keyboard-only
terminals obviously never emit TGP acks.

---

## Terminal-by-terminal detail

### 1. kitty (the reference implementation)

- **File:** `kitty/graphics.c`, dispatch core `grman_handle_command`
  (graphics.c:2634); reply builder `finish_command_response` (graphics.c:1034);
  caller that writes the reply `screen_handle_graphics_command` (screen.c:1919).
- **Model:** sequential, synchronous, inline. A single `switch (action)`
  dispatches every command to completion. Chunked transmits accumulate into one
  global `currently_loading` slot; a continuation chunk carrying no action is
  routed by the still-loading state (including `a=f` frame loads). There is a
  single shared `command_response[512]` buffer, which structurally forbids
  interleaving: only one reply can be under construction at a time.
- **Reply:** `finish_command_response` returns a populated buffer that
  `screen_handle_graphics_command` writes inline with
  `write_escape_code_to_child(self, ESC_APC, response)`. Exactly FIFO.
- **`I=` semantics:** a client-supplied number is stored as `client_number`;
  the terminal allocates a fresh free client id via `get_free_client_id`
  (graphics.c:974). The ack echoes **both** `i=<realid>` and `,I=<number>`.
  Image lookup for later ops is by `i=` first, else by newest `I=`.
- **`q=`:** `q=0` replies to everything (default); `q=1` replies only on
  error; `q>1` suppresses all. It respects this per command.
- **Animation:** full (`a=f` frame add, `a=c` compose, `a=a` control).
  Interesting for the last-frame-skip question: kitty **alpha-composites**
  `a=f` edits whose decoded pixels lack alpha — this is a concrete, spec-backed
  reason RGB-only frame PNGs can be silently dropped (fixed in `b79d891`).
- **Pipelining:** none. Decode (inline zlib/png) and GPU upload happen within
  the command's call stack.

### 2. ghostty

- **Files:** `src/terminal/stream_terminal.zig` (`apcEnd`, ~line 2204),
  `src/terminal/Terminal.zig` (`kittyGraphics`, 3855),
  `src/terminal/kitty/graphics_exec.zig` (`execute`, 25),
  `graphics_storage.zig`, `graphics_command.zig` (`Response.encode`, 332).
- **Model:** sequential, synchronous, inline. `apcEnd()` calls
  `terminal.kittyGraphics(...)`, which is one line to `kitty.graphics.execute(...)`;
  a single `switch (cmd.control)` handles all actions to completion.
  Chunked transmits use a single `storage.loading` slot. PNG is "decoded to
  RGBA during image loading" inline. No thread/async/spawn in the exec path.
  The only threading is `nextGeneration()` (a process-global counter), which is
  about cache-invalidation between *separate terminals*, not parallelizing one
  stream's commands.
- **Reply:** `apcEnd()` encodes the returned `Response` into a stack buffer and
  `self.writePty(...)` inline. Exactly FIFO.
- **`I=` semantics:** number stored on the image; real id allocated by a
  low-water-mark free-id counter (`nextImageId`, graphics_storage.zig:237).
  `Response.encode` prints both `i={<realid>}` and `I={<number>}`.
- **`q=`:** `Quiet`: default `.no` (reply to all); `.ok` (repo on success only
  per its docs); `.failures`. `q=0` is the default.
- **`r=` extension:** ghostty is the **only** implementation that adds `r=` =
  the actual 1-based animation frame number on frame/reply acks — a ghostty
  extension, not in the kitty spec. So it is the one place you could correlate
  a per-frame ack by content; every other terminal cannot.
- **Pipelining:** none.

### 3. wezterm

- **Files:** `term/src/terminalstate/performer.rs` (`Action::KittyImage`, 283),
  `term/src/terminalstate/kitty.rs` (`kitty_img`, `kitty_img_inner`,
  `kitty_img_transmit_inner`, `kitty_send_response`).
- **Model:** sequential, synchronous. The performer runs on the VT input
  thread; `Action::KittyImage` → `kitty_img(...)` handles the command inline.
  Chunking = a `Vec<KittyImage>` `accumulator`; the final chunk flushes it.
  PNG is decoded to RGBA inline (`image::load_from_memory → into_rgba8()`);
  raw `f=24` RGB is widened to RGBA inline too.
- **Reply:** `kitty_send_response` writes to `self.writer` synchronously on the
  same call path. Exactly FIFO.
- **`I=` semantics:** an internally allocated `max_image_id+1` id stored in a
  `number_to_id` map; the ack echoes both id and number.
- **`q=`:** `KittyImageVerbosity`: default `Verbose` (reply to everything);
  `OnlyErrors`; `Quiet`. Default replies to all.
- **`r=` :** none for frames.
- **Pipelining:** none.

### 4. rio

- **Files:** `rio-vt/src/performer/handler.rs` (`dispatch_apc`, ~844),
  `rio-vt/src/crosswords/mod.rs` (`kitty_graphics_response`, 5283),
  `rio-backend/src/graphics/kitty/mod.rs` (tests),
  `rio-vt/src/ansi/kitty_graphics_protocol.rs` (`parse`).
- **Model:** sequential, synchronous processing. `kitty_graphics_protocol::parse`
  blocks and returns a `KittyGraphicsResponse`; chunk accumulation returns
  `incomplete`. State mutation (store/place/delete) is inline on the VT thread.
- **Reply — the one deviation:** `kitty_graphics_response` does
  `self.event_proxy.send_event(RioEvent::PtyWrite(route_id, response))` — the
  reply is **deferred** onto an async event-proxy queue rather than written
  byte-synchronously. It is still strictly ordered (FIFO), just delivered some
  microseconds later. No worker threads for decode/upload; no reordering.
- **`I=` semantics:** allocated/echoed per spec.
- **`q=`:** defaults to reply-on-all.
- **Pipelining:** the deferred reply write is the closest thing to pipelining
  in this whole survey, and it does not affect ordering.

### 5. contour

- **Files:** `src/vtbackend/screen/Screen.cpp` (`processAPC`:
  `body.front() != 'G'` guard, then `processKittyGraphics`, 5591;
  `replyKittyGraphics`, 5414), `src/vtbackend/screen/Terminal.cpp` (dispatch
  via `Terminal::decoder` on the VT input feed, 5666),
  `src/vtbackend/graphics/KittyGraphics.hpp` (Action enum).
- **Model:** sequential, synchronous, inline. `processAPC` runs on the VT
  input thread and calls into `processKittyGraphics`, which parses
  (`parseCommand`) and executes a `switch(command.action)` to completion.
  Chunked transmissions are stitched into the first chunk's command via a
  single `_kittyChunkedCommand` / `_kittyChunkedPayload` pair, with a hard
  `MaxChunkedPayloadSize` cap.
- **Actions:** full set minus `a=f` frame *transmit* — it implements `q`, `t`,
  `T`, `p`, `d`, and `c` (compose). `a=f` load-frame is not implemented
  (it returns ENOTSUP for frame composition only).
- **Reply:** `replyKittyGraphics` → `reply(...)` → `_terminal->reply(...)`,
  synchronous. Co-inline FIFO. It echoes `i=` and `i=,I=` when a number is
  present, and honours `q=` (skips success replies on quietOnSuccess, all on
  quietAlways). Id-less success commands get no reply (to avoid
  desynchronizing a client that never identified itself).
- **Pipelining:** none.

### 6. iTerm2

- **Files:** `sources/VT100/VT100Terminal.m` (`executeAPC`), `sources/InlineImages/KittyImageController.swift`
  (full parser: `KittyImageCommand.initWithAPCString`, `execute`, `executeTransmit`,
  `executeTransmitDirect`).
- **Model:** sequential, synchronous. `executeAPC` parses `G` into an
  `iTermKittyImageCommand` and calls `terminalDidReceiveKittyImageCommand` →
  `kittyImageController.execute(command:)` synchronously on the mutation/VT
  thread. Chunking accumulates into an in-flight transmission. It implements
  t/T/p/d and animation (`executeLoadAnimationFrame`, `executeComposeAnimationFrame`,
  `executeControlAnimation`), plus file/shared-memory media.
- **Reply:** built inline by the controller, but *delivered* through the token
  executor's side-effect queue: `kittyImageControllerReportWithMessage` calls
  `addSideEffect`, which appends to a single FIFO task queue drained on the
  main thread (`VT100ScreenMutableState.m:8595-8604`,
  `TokenExecutor.swift:580-585`). Strictly ordered, but asynchronous relative
  to command processing — a visible session flushes quickly; a background
  session batches side effects at a longer period (all tiers share one FIFO
  that `executeSideEffects` drains whole). Verbosity-gated (`.normal` replies
  to everything, `.query` only errors, `.quiet` nothing). Echoes `i=`, `I=`,
  `p=` as present.
- **`I=` semantics:** per spec; the controller allocates identifiers and uses
  numbers. (It returns `ENOSPC:Out of identifiers` when exhausted.)
- **Pipelining:** none (only coalescing of placement-damage notifications,
  which is a repaint optimization, not a protocol reorder).

### 7. konsole

- **Files:** `src/Vt102Emulation.cpp` (`apc_end` → `processGraphicsToken`,
  2415; `sendGraphicsReply`, 2631), `src/Screen.*` (kitty placements).
- **Model:** sequential, synchronous, inline on the VT-thread
  `receiveData`/`receiveChars` path. `apc_end` fires only when the APC payload
  starts with `G`, then `processGraphicsToken` runs the whole command.
- **Actions:** `q`, `t`, `T`, `p`, `d` — **no animation** (`a=f`, `a=c`, `a=a`
  are not implemented). It also refuses `t`/`q` with an explicit
  `ENOTSUPPORTED:` reply (`t=d` excepted) when data medium is not direct.
- **Reply:** `sendGraphicsReply` builds `\e_G<i=..>[,I=..][,p=..];{OK|err}\e\`
  and pushes it through `sendString` inline. Queries always reply; transmits
  reply at `q==0` and on error; `I=` is echoed back alongside the allocated id.
- **Pipelining:** none.

### 8. wayst

- **Files:** `src/vt_core.c` (`Vt_handle_APC`, `Vt_img_proto_transmit`,
  `Vt_img_proto_validate`), and
  its own `vt_img_proto` / `vt_sixel.h` image protocol layer.
- **Model:** sequential, synchronous, inline in the VT parser (`PARSER_STATE_APC`).
- **Actions:** full t/T/p/d (self-declared "incomplete and unstable" in a code
  comment).
- **Reply:** (re-verified against current master, 2026-08 tree) the query
  action replies **unconditionally** — `case VT_IMAGE_PROTO_ACTION_QUERY:`
  runs `Vt_img_proto_validate` and writes `\e_Gi=%u;{error|OK}\e\` with no
  quiet check at all (`vt_core.c:4169-4173`), so even `q=2` queries are
  answered (a kitty deviation in the opposite direction). Identified
  transmits and displays reply OK at `q<1` and errors at `q<2`
  (`vt_core.c:4040-4046`, `4050-4056`). It echoes only the `i=` it parsed —
  never `I=` — which is harmless for marker correlation since markers carry
  their own unique `i=`. The query dispatch runs in the same synchronous
  parser path as every other action (`Vt_handle_APC`, reached from
  `PARSER_STATE_APC` in the feed loop; replies append to a single
  `self->output` vector drained front-first by `Vt_peek_output`/
  `Vt_consumed_output`, so FIFO is structural).
  Note: `a=q` with `t=` anything other than `d` still validates and answers —
  but markers should keep the default `t=d` anyway.
- **Pipelining:** none.

### 9–14. Sixel-only / no-TGP terminals

These are included for completeness: they are popular, support *some* inline
graphics (sixel, or kitty keyboard only), but **do not implement the kitty
graphics protocol**, so they emit no TGP acks and are irrelevant to ack
routing. The important consequence: if you rely on acks, you must first detect
TGP support (via `a=q`), because these will answer the query with silence
(or behave as pure sixel).

- **tmux** — SIXEL images only (`input.c`: `sixel_parse` inline; no `\x1b_G`
  handling). Not a TGP terminal.
- **foot** — SIXEL images (`dcs.c`: `sixel_init`, inline) + kitty *keyboard*;
  no TGP.
- **Windows Terminal** — SIXEL (`terminal/adapter/SixelParser`) + kitty
  *keyboard* protocol; its APC state machine collects Strings but does not
  dispatch `<G` as TGP.
- **VTE (gnome-terminal)** — SIXEL + inline images (`image.cc`); no TGP APC.
- **mlterm** — no kitty graphics, no TGP.
- **alacritty** — kitty *keyboard* protocol only; explicitly **no** graphics
  protocol support as of this survey.

---

## Answers to the three questions

### 1. Sequential or parallel?

**Sequential, everywhere.** Every TGP implementation processes each graphics
command to completion before the next one begins. There is no terminal in this
survey that runs two graphics commands concurrently. The reasons are partly
architectural (a single VT input thread or mutation queue parses and executes
synchronously) and partly enforced by shared per-terminal mutable state (a
single `currently_loading` slot in kitty/ghostty; a chunk accumulator in
wezterm/rio/contour/iTerm2) that would make true parallelism unsafe.
iTerm2's token executor executes tokens and drains side effects on a single
FIFO; iTerm2's controller even marks *no two* chunked transmits as concurrent
(a single accumulator slot).

### 2. Are the acks exactly FIFO?

**For the terminals that reply at all, yes — in practice, deterministically so,
with two caveats.**

- Six of eight (kitty, ghostty, wezterm, contour, konsole, wayst) emit the
  reply within the same synchronous call stack that processed the command —
  byte-for-byte FIFO.
- **rio** defers the reply write onto an event-proxy queue and **iTerm2**
  delivers it through the token executor's main-thread side-effect queue:
  both still strictly ordered, just asynchronous relative to the command.
- **wayst**'s deviation is reply *policy*, not order: queries reply
  unconditionally (even `q=2`), identified transmits/displays reply OK at
  `q<1`, everything else stays silent.

But FIFO cannot mean "the Nth reply belongs to my Nth op" in general, because
**replies are not 1:1 with commands even when FIFO holds**: id-less commands
get no reply, most terminals suppress success replies under `q` settings, and
animation `a=a` and delete often reply nothing. One such command desyncs pure
positional matching.

### 3. Any pipelining optimizations?

**No terminal reorders or batches graphics commands.** The closest to
pipelining is rio's deferred reply write (ordered) and iTerm2/others' coalesced
repaint notifications (render-only, not protocol). Decoding (PNG inflate,
raw RGB→RGBA) happens inline in every implementation; none offload it to
a worker thread in a way that reorders acks.

---

## Implications for `easytgp`

These findings **confirm** the already-settled design and narrow the remaining
choice:

1. **Do not route acks by position.** Even with universal practical FIFO, the
   non-1:1 correspondence (id-less commands, `q` suppression, `a=a`/`d` no-reply,
   wayst's error-only replies, rio's deferred write) makes positional routing
   fragile. Match by id where content uniquely identifies the op.

2. **The ack channel can only reliably attribute two things by content:**
   `I=`-allocation replies (unique number → real `i=`) and `a=q` queries.
   Everything else — every `a=p`, `a=f`, `a=c` on an already-known image —
   carries **no distinguishing token** (only kitty's `i=`, and only ghostty
   adds a real `r=` frame number). Correlation therefore reduces to
   (a) per-entity serialization + order, or (b) not needing the reply at all.

3. **Serialization is the routing mechanism.** Keep **one in-flight op per
   image (per `i=`)**. Because at most one op for a given image is pending at
   once, any ack bearing that `i=` is by definition that op's reply — ordering
   + mutual exclusion is the correlation, no frame token required. This matches
   the "serialize same-entity concurrent ops" decision.

4. **Frame pipelines must not await per-frame acks.** Since repeated `a=f`
   acks are indistinguishable (same `i=`, same `I=`), do not build a future per
   frame. Transmit frames eagerly and in-order; if batching truly parallel
   waits, coalesce them (the future resolves for the aggregate, not per frame).
   The only ack worth awaiting for a new image is the unique `I=`-allocation
   reply that yields the real `i=`.

5. **`q=` default should stay "reply to everything"** (`q=0`), because the
   `I=`-allocation success reply is exactly what you need to learn the real
   `i=`; a suppressing default (`q=1`/`q=2`) would hide it. Do not pre-suppress:
   the cost is a handful of small replies, and the value is a reliable id.

6. **Detect TGP support before relying on acks.** Sixel-only terminals (tmux,
   foot, Windows Terminal, VTE, mlterm, alacritty) will silently ignore every
   `\x1b_G` and never ack. Probe with `a=q` first; treat a missing reply to a
   *query* as an unsupported/absent terminal (this is the one case where the
   spec mandates an immediate reply, so silence is meaningful).

7. **RGBA-forced frames are still the right call for the last-frame-skip bug.**
   Kitty (and wezterm, rio, iTerm2) all normalize decoded pixels to RGBA and
   alpha-composite `a=f` edits; an RGB-only PNG frame can be dropped. Widening
   animation frame payloads to alpha-255 RGBA before PNG encode remains the
   correct, spec-aligned fix, independent of the concurrency questions above.

8. **konsole answers `a=q` with non-direct media by immediate error.** konsole
   replies `ENOTSUPPORTED` for any `a=q` whose `t=` is not `d`
   (`Vt102Emulation.cpp:2470-2476`), before looking at the payload. The marker
   must keep the default `t=d` (or omit `t=` entirely) so queries validate as
   direct transmissions. konsole also echoes only `i=` on the query ack (no
   `I=`) and sends it even at `q=2` — both kitty deviations, both harmless for
   markers, but worth knowing.

9. **The marker's unique token must be `i=`, never `I=`.** For `a=q` the
   terminals that gate on an id gate on `i=` *only*: kitty requires `q_iid =
   g->id` and drops a query without it (graphics.c:2711-2718; `REPORT_ERROR`
   is log-only, nothing goes on the wire), ghostty requires `t.image_id != 0`
   (graphics_exec.zig:154-156) and even its EINVAL reply is then suppressed
   by `Response.encode`'s id-less no-write rule (graphics_command.zig:342-344),
   and rio reports "image ID required" for `a=q,I=7` (its protocol tests pin
   this). `I=` exists to create images and learn their id; it is never
   accepted as a query address. Id-less queries (`a=q` with neither key) get
   a key-less ack on wezterm (`\e_G;OK\e\`), `i=0;OK` on konsole/wayst/iTerm2,
   silence on contour success / `i=0;EINVAL` on contour failure. All of these
   are unattributable — which is fine, because the marker always carries its
   own unique `i=`.

---

## Methodology & verification

Sources were downloaded as full source trees and dispatched by reading the
actual command-processing code paths, not header docs:

- **kitty** `graphics.c`/`screen.c`, **ghostty** `stream_terminal.zig`,
  `Terminal.zig`, `graphics_exec.zig`, `graphics_storage.zig`,
  `graphics_command.zig`, **rio** `handler.rs`/`crosswords/mod.rs`,
  `kitty_graphics_protocol.rs`, **wezterm** `performer.rs`/`kitty.rs`,
  **contour** `Screen.cpp`/`graphics/KittyGraphics.hpp`,
  **iTerm2** `VT100Terminal.m`/`KittyImageCommand.swift`,
  **konsole** `Vt102Emulation.cpp`, **wayst** `vt_core.c`,
  **tmux** `input.c`, **foot** `dcs.c`/`vt.c`/`sixel.c`,
  **Windows Terminal** `OutputStateMachineEngine.cpp`/`adaptDispatch.cpp`,
  **VTE** `vte.cc`/`vteseq.cc`, **mlterm** `vtemu/`, **alacritty** whole tree.

Each was checked for: the presence of a TGP APC handler, the synchronous vs
asynchronous nature of the dispatch, the reply-write mechanism, `I=`→real-id
handling, `q=` defaults, animation support, and any thread/queue/pipeline.

**2026-10 re-verification pass** (this update): every claim above was
re-checked against current upstream master via fetched source. Corrections
found: wayst's repo is `91861/wayst` and its reply policy changed since the
first survey (it now answers `a=q` unconditionally and replies OK for
identified ops at `q<1`); iTerm2's reply goes through the token executor's
side-effect queue (ordered FIFO, async) rather than inline. contour was
re-verified against the current `src/vtbackend/screen/` layout. The iTerm2
token executor maintains one FIFO across token execution and side-effect
drain, so background-session batching delays but never reorders replies
(`VT100ScreenMutableState+Private.h` issue-13013 comment).