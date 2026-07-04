# REPL ↔ TUI divergences

The REPL (`crates/tuigram/examples/repl.rs`) and the TUI (`crates/tuigram/src`)
drive the same `tuigram-core` seams, but their surfaces are not (and are not
meant to be) identical. #195 shipped because the TUI silently fell behind the
REPL's command set with nothing to flag it; this doc, plus the parity guard
test in `crates/tuigram/src/parity.rs`, are the fix: every REPL command either
maps to a real TUI action, or is listed below with a reason. If it's in
neither place, the guard test fails CI.

The two lists mirror `crates/tuigram/src/parity.rs`'s `DIVERGENT` constant and
its `bound_action` match — update code and doc together.

## REPL-only commands (no TUI keybinding)

| Command | Why it has no TUI equivalent |
|---|---|
| `chats` | the chat-list pane is always visible; no toggle needed |
| `history` | the history pane always shows the open chat; no separate on-demand fetch |
| `read` | the TUI marks the open chat's messages read automatically while it's open; the REPL has no equivalent live loop, so it needs an explicit command |
| `file` | the conversation pane always renders a downloadable message's transfer state inline; the REPL has no persistent view, so it needs an explicit command |
| `folder` | the TUI cycles chat lists in a fixed order (`NextList`/`PrevList`, `]`/`[`); a REPL-style jump to an arbitrary folder id has no keybinding |
| `secrets` | each chat row already shows its secret-chat lifecycle state inline; the REPL has no persistent view, so it needs an explicit listing command |
| `status` | the status bar always shows connection state; no on-demand command needed |
| `probe` | a terminal-injection self-test (#174); a developer/security diagnostic, not a user action |
| `typing` | the TUI sends the typing action automatically while the composer has unsent input (#197); there is no manual one-shot command |

## Other intentional behavioral divergences

Not REPL commands, but places the two front-ends deliberately behave
differently:

- **Auto-download of incoming media** — the TUI does not automatically
  download incoming photos/videos/documents; `S` (save) reveals a local path
  once downloaded, or starts the download. This avoids surprising bandwidth
  use just from opening a chat.
- **Auto-mark-read** — the TUI marks a chat's messages read as soon as it is
  open (mirroring a normal client); the REPL requires the explicit `read`
  command, since it has no persistent "open chat" view to hook the read
  acknowledgement to.
- **Automatic typing indicator** — the TUI broadcasts a typing action
  automatically while the composer has unsent text in the open chat (#197,
  throttled to avoid re-sending on every keystroke); the REPL's `typing`
  command remains a manual, explicit one-shot for testing/diagnostics.
- **New secret chat targeting** — the REPL's `secret-new <user_id>` takes a raw
  id typed in directly; the TUI's `n` contact-search picker (#197) reaches the
  same arbitrary-user capability by searching contacts by name instead, since
  there's no free-text id entry in a modal UI.

## Adding a new REPL command

When adding a new REPL command, either give it a TUI binding (and add an entry
to `bound_action` in `crates/tuigram/src/parity.rs`), or add it to `DIVERGENT`
there with a reason, and mirror the entry here. The parity test in that module
fails until one of the two is done — that's the guard.
