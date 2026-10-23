---
name: revdiff
description: Review diffs, files, plans, and documents with inline annotations in the revdiff TUI overlay, then address the captured annotations. Opens revdiff in a terminal overlay (herdr, tmux, zellij, kitty, wezterm, cmux, ghostty, iTerm2, emacs vterm), captures annotations, and feeds them back to Crush. Works in git, hg, and jj repos (auto-detected). Activate on "revdiff", "review diff", "review changes", "review my changes", "annotate diff", "interactive diff review", "review this patch", "review PR", "review all files", "browse all files", "review <file>", "review this file", "review this plan", "annotate this file", "review in revdiff", "revdiff config", "revdiff themes", "revdiff keybindings", "use my latest revdiff annotations".
user-invocable: true
---

# revdiff - TUI Diff Review

Review diffs with inline annotations using the revdiff TUI in a terminal overlay,
then turn the annotations into code changes. Works in git, hg, and jj repos.

## About this skill

This is the upstream revdiff skill, vendored for Crush. See `references/` for
config, usage, and install details. The one local modification is in
`scripts/launch-revdiff.sh`: its herdr branch also triggers when Crush's shell
does not inherit `HERDR_ENV` (Crush tool shells do not), so the overlay opens in
herdr instead of failing with "no overlay terminal available".

## Paths

Resolve `$SKILL_DIR` to this skill's directory (the folder holding this file).
The launcher is `$SKILL_DIR/scripts/launch-revdiff.sh`, and the helper scripts
are in the same `scripts/` folder. `revdiff` must be on `PATH`.

Never pipe or redirect into the launcher; hand a diff over as a file with
`--stdin-file=PATH`.

## Answering questions (do not launch)

If the user asks about revdiff itself (configuration, themes, keybindings,
installation, output format), answer from `references/config.md`,
`references/usage.md`, and `references/install.md`. Do **not** launch the TUI
for an informational question.

## How it works

1. The launcher opens revdiff in a terminal overlay (herdr tab or pane split,
   tmux popup, kitty overlay, and so on) and blocks until the review ends.
2. The user annotates lines, then quits (`q`).
3. Annotations land on the launcher's stdout as structured markdown, and in a
   temp output file.
4. You read them and address each one.
5. Loop: relaunch on the same ref so the user can verify; done when a review
   comes back with no annotations.

Exit code `10` means annotations were captured, not failure. `0` means a clean
quit with no annotations. Any other nonzero exit is a real failure: relay the
launcher's stderr verbatim instead of guessing which argument was at fault.

## Step 1 - Decide what to review

- **All files**: "all files", "browse all files" -> pass `--all-files`, and each
  "exclude X" as `--exclude=X`.
- **Single file / plan**: a token that is a file on disk (`test -f`), or starts
  with `/` or `./`, or contains `/` with an extension -> pass `--only=PATH`.
  No ref. Works outside a VCS repo too.
- **External diff**: "review PR #N", "review this patch", a pasted unified diff,
  or `git format-patch` output -> write it to a private temp file and pass
  `--stdin-file=PATH` (mutually exclusive with refs/`--staged`/`--only`).
- **Explicit refs**: `HEAD~1`, `main`, `main..feature`, `HEAD~1 HEAD` -> pass
  through unchanged.
- **No ref**: run `$SKILL_DIR/scripts/detect-ref.sh`. It prints structured
  fields: `suggested_ref` (empty means uncommitted changes), `use_staged`,
  `needs_ask`, plus branch and cleanliness info.
  - `use_staged: true` -> pass `--staged`.
  - `needs_ask: true` (feature branch with uncommitted changes) -> ask the user
    whether to review uncommitted only or the full branch diff.
  - otherwise use `suggested_ref`; add `--untracked` if the agent likely created
    new files in working-tree mode.

Use `--description="..."` (or `--description-file=`) when launching on the
user's behalf after a refactor, so the `i` popup explains the change. Markdown
is supported. The two flags are mutually exclusive.

Only pass `--start-at-change`, `--filter-unreviewed`, or `--cross-file-motion`
when the user explicitly asks for them.

## Step 2 - Launch

Run the launcher and give it room: the overlay is interactive and may stay open
far longer than a normal shell call. Pass a large `auto_background_after` (e.g.
`86400`) so it is not torn down early.

```
"$SKILL_DIR/scripts/launch-revdiff.sh" [base] [against] [--staged] [--untracked] [--all-files] [--exclude=P] [--only=FILE] [--stdin-file=FILE] [--description="..."]
```

Keep the launcher call a plain command: no pipes, redirects, or command
substitution inside it. The macOS Ghostty/iTerm2 launchers drive the terminal
through `osascript`.

`REVDIFF_HERDR_PANE=1` makes herdr open a zoomed split of the agent's own pane
instead of a new tab. Only set it when the user asks to keep the agent pane
visible; it is a launcher environment variable, not a revdiff flag.

The overlay review is owned by herdr/tmux/etc, so it keeps running even if the
shell call is backgrounded. Annotations are written to disk the moment the user
quits, or earlier when they press `O` to flush.

## Step 3 - Read the annotations

Prefer the launcher's stdout. If the call was backgrounded or timed out, the
review is still live and nothing is lost; read the persisted annotations
instead, newest first:

```bash
output_file="$(find -H "${TMPDIR:-/tmp}" -maxdepth 1 -type f -name 'revdiff-output-*' -user "$(id -u)" -mmin -120 -exec ls -t {} + 2>/dev/null | head -1)"
[ -n "$output_file" ] && cat "$output_file"
```

If that is empty, run `$SKILL_DIR/scripts/read-latest-history.sh` and use its
annotations section (it matches the newest history file to this checkout).

Output format:

```
## handler.go:43 (+)
use errors.Is() instead of direct comparison

## store.go:18 (-)
don't remove this validation

## handler.go (file-level)
consider splitting this module
```

`(+)` added, `(-)` removed, `(file-level)` whole-file note; a line range
appears as `file.go:43-67`.

If a review flushes mid-session with `O` while the overlay is still open, read
the output file as above and act; do not relaunch.

## Step 3.5 - Classify

**Explanation requests**: the annotation contains `??` (two or more question
marks), or starts with `explain`, `remind`, `describe`, `what is`, `what are`,
`how does`, `how do`, `clarify`. Answer these; do not change code for them.
Write the answer as markdown, then reopen it in revdiff with `--only=<file>` so
the user can scroll it and ask follow-ups by annotating. Loop until they quit
without annotating. Pending code-change annotations are held and applied after.

**Code-change directives**: everything else.

## Step 4 - Plan, then apply

List each code-change annotation with its `file:line`, describe the fix, get
approval, then apply the changes. One annotation is one directive.

## Step 5 - Loop

Relaunch with the same arguments. The user either annotates more (back to
Step 3) or quits cleanly (review complete).

## Example

```
User: "review my changes"
-> detect-ref.sh: tree has uncommitted edits -> launch with --untracked
-> user annotates store.go:18 "don't remove this validation" and quits
-> annotations captured
-> plan: restore the validation at store.go:18
-> approve, apply
-> relaunch with the same args
-> user quits with no annotations
-> "review complete"
```
