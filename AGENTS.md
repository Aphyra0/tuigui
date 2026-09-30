# Agent Rules (tuigui)

These rules are binding for any agent/assistant working in this repository.
They override convenience and habit. Follow them exactly.

## Hard prohibitions

- **NEVER use Python.** Not for scripts, not for one-liners, not for
  formatting, not for anything. No `python3`, no `python`, no `PYEOF`, no
  `.py` files. Python is banned in this project — full stop.
- **Do not use `curl`** through the shell. Use the fetch/download tools.
- **Do not commit** unless the user explicitly asks.

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
- Test after changes (`CARGO_TARGET_DIR=./target cargo test --workspace
  --all-targets`).