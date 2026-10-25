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
- **Never invent a value a read did not produce.** No `unwrap_or`,
  `unwrap_or_default`, `unwrap_or_else` (except on a poisoned lock, where the
  guarded value is still valid), `.unwrap_or(<literal>)`, `ok()`, `Default` as a
  stand-in for a failure, or `?` on a metric, id, or parse that decides behavior.
  Each of these hides the failure behind a plausible value and turns a bug into
  silently wrong behavior. Return `Option`/`Result` and let the caller decide.
  When reporting, say "unmeasured" rather than `0`. This is enforced in review:
  a default value for a failed read is a rejected change, not a style nit.

## Editing workflow

- **Use `view`, `grep` and `edit`/`multiedit`. Never edit with another language.**
  No `python`, no `python3`, no `perl -0pi`, no `sed -i`, no `awk` splices, no
  heredoc-fed scripts. Python is banned outright (see Hard prohibitions); the
  others are banned for edits because a script cannot be inspected before it
  runs, and every time one has been used in this repo it has corrupted a file
  (duplicated blocks, cut functions in half, missing braces).
- Read the relevant context of a file before editing it. Match whitespace and
  indentation exactly.
- For a whole function, method or type, prefer `lsp_replace_symbol`: it finds the
  boundaries itself and cannot fail on whitespace.
- If `edit` fails with "file modified since last read", re-`view` the exact
  region and try `edit` again with the text the view shows. Do not switch to a
  script.
- After every destructive edit, verify braces (`grep -o '{' | wc -l` vs `}`)
  and rebuild.
- Never use `apply_patch` (does not exist).

## Builds that actually build the thing

- `cargo build --examples` builds **only** examples: it replaces the default
  target selection. To get binaries and examples use `--bins --examples`.
- Cargo will not recreate a deleted binary if its fingerprint is satisfied, so a
  build that reports `Finished` in under a second may not have produced anything.
  Verify the file exists and is newer than the source.
- Never leave a background build running.

## Behavior

- **All communication is in English.** Replies, explanations, commit messages,
  code comments, and any text in a file: English only. No other language, ever.
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