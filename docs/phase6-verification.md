# Phase 6 — real-TDLib lifecycle verification

> **The milestone gate.** Per our standing rule, Phase 6 is not "done" until the
> connected paths are run **for real** against live TDLib via the REPL — not just
> asserted in CI. This note is the record of that run: each path, the command
> that exercised it, what was observed, and a PASS/FAIL/blocked verdict. Gaps are
> filed as honest follow-up issues, not papered over.
>
> Tracking issue: [#123](https://github.com/queq-co/tuigram/issues/123). The
> Phase 6 docs pass ([#124](https://github.com/queq-co/tuigram/issues/124))
> consumes this note.

## How to run

The harness is [`crates/tuigram/examples/repl.rs`](../crates/tuigram/examples/repl.rs) —
a feature-gated manual tool, off in the product binary and default CI:

```text
cargo run -p tuigram --example repl --features login-harness
```

On first run it captures your Telegram API credentials (`api_id` / `api_hash`,
persisted to `~/.config/tuigram/config.toml`, mode 600) and drives the real login
(phone → code → optional 2FA). After "Logged in." it drops into a stdin REPL;
`help` lists every command. The `<chat>` / `<msg>` arguments are the numeric ids
printed by `chats` / `open` / `history`.

Run this against a **real, non-test account** on a residential connection. Do not
run it in CI.

## Verification checklist

Fill the **Result** column as each path is exercised: `PASS`, `FAIL (#nnn)`, or
`BLOCKED (reason)`. Leave a one-line observation in **Observed**.

| # | Path | REPL command(s) | What to observe | Result | Observed |
|---|------|-----------------|-----------------|--------|----------|
| 1 | Real login | *(startup flow)* | Phone → code → 2FA completes; reaches `Ready`; session persists so the next run resumes without re-auth | `PASS` | All tests successful; session is persisted. |
| 2 | Connection state transitions | `status`, then pull the network | `updateConnectionState` moves Connecting → Updating → Ready; `status` reflects each; recovery after a drop | `PASS` | Status is correctly reflected |
| 3 | Chat list load | `chats`, `archive`, `folders`, `folder <id>` | Main list, Archive, and folders populate with real chats | `PASS` | Commands work as expected. |
| 4 | Open + history | `open <chat>`, `history <chat>` | Recent messages load and render; `open` also marks read (see #6) | `PASS` | Commands work as expected. |
| 5 | Send / reply / edit / delete | `send <chat> <text>`, `reply <chat> <msg> <text>`, `edit <chat> <msg> <text>`, `delete <chat> <msg> [all]` | Message appears on the real account; reply threads; edit updates in place; delete removes it | `PASS` | Commands work as expected. |
| 6 | Mark-as-read | `read <chat>` (and via `open`) | Unread count clears on the real account; read state folds back | `PASS` | Commands work as expected. |
| 7 | Search | `search <query>` (account-wide), `search <chat> <query>` (scoped) | Real hits return; scoped vs. global differ; loaded history untouched afterward | `PASS` | Commands work as expected. |
| 8 | React / unreact | `react <chat> <msg> <emoji>`, `unreact <chat> <msg> <emoji>` | Reaction appears/disappears on the real message | `PASS` | Commands work as expected. |
| 9 | Pin / unpin | `pin <chat> <msg>`, `unpin <chat> <msg>` | Message pins/unpins chat-wide | `PASS` | Commands work as expected. |
| 10 | Forward | `forward <from> <ids> <to>` | Comma-separated ids arrive in the target chat | `PASS` | Commands work as expected. |
| 11 | Media download | `download <chat> <msg>`, `file <file_id>` | Media downloads; local path reported; `file` shows transfer state | `PASS` | Commands work as expected. |
| 12 | Media send | `sendmedia <chat> photo\|video\|document <path> [cap]` | File arrives on the real account with the caption | `PASS` | Commands work as expected. |
| 13 | Typing action | `typing <chat>` | Recipient sees the typing indicator | `PASS` | Commands work as expected. |
| 14 | Secret chats | `secret-new <user_id>`, `secrets`, `secret-close <secret_id>` (open/send reuse chat-id commands) | Secret chat negotiates to Ready; send/receive works; close tears it down | `PASS` | Commands work as expected. |
| 15 | Dropped-update recovery | provoke a gap, `status`, `resync` | `status` reports STALE with a dropped count; `resync` re-queries and clears it | `PASS` | Commands work as expected. |
| 16 | Logout | `logout` | Session invalidated + local DB wiped; the next run starts at a fresh login | `PASS` | Commands work as expected. |

## Suggested run order

A mechanical sequence that touches every row with the least account churn:

1. Launch → complete login (**#1**).
2. `status` (**#2** baseline) → `chats` / `archive` / `folders` (**#3**).
3. `open <chat>` a busy chat → `history <chat>` (**#4**); confirm unread cleared (**#6**).
4. `send` a marker message; `reply` to it; `edit` it; `react`/`unreact`; `pin`/`unpin`; `typing`; `forward` it elsewhere; `delete` it (**#5, #8, #9, #10, #13**).
5. `sendmedia` a small local file; `download` an incoming media message; `file <id>` (**#11, #12**).
6. `search` global then scoped (**#7**).
7. `secret-new` with a second account; exchange a message; `secret-close` (**#14**).
8. Kill Wi-Fi briefly mid-session, restore, `status`, `resync` (**#2, #15**).
9. `logout`; relaunch to confirm the fresh-login path (**#16**).

## Run log

Record each real run here (append, don't overwrite):

- **Date:** 2026-07-02
- **Account / environment:** personal account, residential network, macOS
- **TDLib / crate version:** TDLib 1.8.61 (via `tdlib-rs =1.4.0`); crate `tuigram_core` 0.0.0 — the build under test; month-based CalVer (`2026.7.0`) was adopted immediately after this run
- **Summary:** Full lifecycle exercised end-to-end against a live account; all 16 paths PASS, no gaps attributed to the tested functionality.

## Gaps found

File each gap as its own GitHub issue and link it here — don't paper over it.

- **No verification gaps.** All 16 paths PASS; nothing attributable to the tested
  functionality itself.
- _Harness quality-of-life (not a verification gap):_ REPL command-history
  navigation (↑/↓) and command autocompletion surfaced as future improvements to
  the manual REPL — tracked in
  [#152](https://github.com/queq-co/tuigram/issues/152).
