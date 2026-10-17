# Agent Rules (tuigui)

These rules are binding for any agent/assistant working in this repository.
They override convenience and habit. Follow them exactly.

## Builds (the user gets very annoyed at needless rebuilds)

A full rebuild of `ring` / `rustls` / the TLS stack costs minutes. Avoid it.

**What invalidates fingerprints and forces the TLS stack to rebuild:**

- **Changing `PKG_CONFIG_PATH`.** Recorded in the fingerprint of every
  pkg-config unit. `turbojpeg-sys` reads it, so any change there dirties the
  whole subtree. Never set or override it by hand — do not prefix commands with
  `PKG_CONFIG_PATH=...`. Use `nix develop .` and let the shell own it.
- **Changing `CARGO_TARGET_DIR`.** Two target dirs alternating invalidates each
  other's fingerprints. The ambient value is `/cache/cargo_global_target`
  (which does not exist); always pass `CARGO_TARGET_DIR=./target` explicitly and
  use that one only.
- **Two cargo processes sharing one target dir.** Never run cargo concurrently
  with the user's cargo (including background `cargo check`s). Check with
  `pgrep -la cargo` first, and kill your own background builds when done.
- **Touching `crates/easytgp/src/lib.rs`, `Cargo.toml`, or `Cargo.lock`** in ways
  that change features or versions.

**Rules:**

- Prefer `cargo check` over `cargo build` when only verifying compilation.
- Do not run a build just to "confirm" something you can read from source,
  `git diff`, or an existing fingerprint/log file.
- If a build is unavoidable, use `-p <crate>` to build the smallest thing that
  answers the question, never `--workspace` by reflex.
- **Never leave background builds running.** The user runs cargo in the same
  tree; overlap is what corrupted things before.

## Hard prohibitions

- **NEVER use Python.** Not for scripts, not for one-liners, not for
  formatting, not for anything. No `python3`, no `python`, no `PYEOF`, no
  `.py` files. Python is banned in this project — full stop.
- **Do not use `curl`** through the shell. Use the fetch/download tools.
- **Do not commit** unless the user explicitly asks.
- **Do not add a silent fallback for a failed read.** `unwrap_or_default`,
  `unwrap_or`, and `?` on a metric, id, or parse that decides behavior hides the
  bug behind a plausible value. Return `Option`/`Result` and let the caller
  decide; when reporting, say "unmeasured" rather than `0`.

## Editing workflow

- Read the relevant context of a file before editing it. Match whitespace and
  indentation exactly.
- Prefer `edit`/`multiedit`. If the file keeps churning mtime (edit tool fails
  with "file modified since last read"), re-`view` the exact region first, then
  use `perl -0pi`/`sed` with exact anchors, and verify immediately with
  `git diff`.
- After every destructive edit, verify braces (`grep -o '{' | wc -l` vs `}`)
  and rebuild.
- Never use `apply_patch` (does not exist).

## Behavior

- Follow the user's explicit design. Do not "improve" or redesign what was
  asked without being asked.
- **Do not propose an architectural rewrite when the user reports a bug.** It
  reads as deflecting the bug onto the design. Investigate the specific failure.
- **Do not block on a long-running command.** If something takes more than a few
  seconds, run it in the background and keep working. Do not sit in `wait`.
- Test after changes (`CARGO_TARGET_DIR=./target cargo test --workspace
  --all-targets`).

## Response length (hard rule)

- Long answers are not acceptable. This agent cannot answer at length.
- Deliver the *understandable part* compactly. Do not lose important meaning
  to brevity, but do not pad to fill space either.
- Before sending: read the draft, filter it, and read it again. Strip anything
  that is not load-bearing: restated questions, preambles, postambles,
  checklists of options nobody asked for, and hedge paragraphs.
- Aim for the fewest words that still carry the full meaning. Prose is the
  default; tables and bullets only when they are genuinely shorter.
- Never answer a short question with a long essay. If the answer is one
  sentence, send one sentence.