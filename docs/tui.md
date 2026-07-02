# TUI (Phase 5)

> Phase 5 builds the Ratatui front-end as a self-contained **spine**: a single
> event loop, one `App` source of truth, the three-pane layout, a focus-aware
> keymap, the modal overlays, and the status-bar/toast feedback layer — all
> rendering from **fixtures**, with no live Telegram data. Phase 6 swaps the fake
> core source for the real [`Client`](headless-client.md) update stream without
> changing the loop's shape. The Phase 1 research that set this direction is
> [research/ratatui.md](research/ratatui.md); this doc is what was actually built.

## The shape of the problem

Ratatui is synchronous (you call `terminal.draw(|frame| …)` and it paints), but
the app is asynchronous: terminal input, a render clock, and core updates all
arrive independently, and the **#1 goal is that the UI never blocks on network
I/O**. So the design question is not "how do we draw widgets" but "how do we
race three event sources into one consistent state and repaint without ever
awaiting inside the draw". The answer is the Elm-ish *central event loop +
`Action` + single `App`* pattern the Ratatui community converged on, kept
deliberately thin in Phase 5 so Phase 6 only has to feed it.

## The pieces

| Concern | Where |
|---|---|
| The `tokio::select!` loop, RAII terminal guard, panic hook | [`main.rs`](../crates/tuigram/src/main.rs) |
| The whole-app state + the `Action` reducer | [`app.rs`](../crates/tuigram/src/app.rs) |
| The pure `fn ui(frame, &App)` render | [`ui.rs`](../crates/tuigram/src/ui.rs) |
| The focus model + the one bindings table | [`keymap.rs`](../crates/tuigram/src/keymap.rs) |
| The `AppEvent` seam + the temporary fake source | [`event.rs`](../crates/tuigram/src/event.rs) |
| The terminal guard (raw mode, alt screen, restore) | [`terminal.rs`](../crates/tuigram/src/terminal.rs) |
| Per-pane / per-overlay view-models | `chat_list`, `conversation`, `composer`, `search`, `forward`, `reactions`, `mediaform`, `secret`, `settingsform`, `login`, `status`, `textinput` |

## The central event loop

[`run`](../crates/tuigram/src/main.rs) owns one `App` and a single
`tokio::select!` that races exactly three sources into [`Action`]s, each applied
through the same `App::dispatch`:

```rust
while !app.should_quit() {
    if app.is_dirty() {
        guard.terminal_mut().draw(|frame| ui::ui(frame, &app))?;
        app.clear_dirty();
    }
    tokio::select! {
        maybe_event = input.next()      => /* terminal key/resize → on_terminal_event → Action */
        _ = tick.tick()                 => app.dispatch(Action::Render),
        maybe_app_event = core_rx.recv() => /* core update → on_app_event → Action */
    }
}
```

Three properties matter, and the tests pin all three:

- **Nothing is awaited in the draw path.** `terminal.draw()` stays on the main
  task; the only `await`s are the `select!` arms. This is what decouples render
  cadence from network latency — core can be mid-request and the UI still
  repaints and still responds to keys. The render tick (`FRAME`, ~30 FPS) caps
  repaint rate independently of how fast events arrive.
- **Repaint is gated on a `dirty` flag.** `draw` runs only when visible state
  changed since the last paint, so an idle app doesn't burn frames; a fresh
  `App::new()` starts dirty so the first frame paints before any event.
- **Dead sources never spin the loop.** Closed stdin (`None`) dispatches
  `Action::Quit`; a transient input read error is ignored and the loop
  re-enters; if the core source ends, the UI stays usable without it.

The terminal lifecycle lives entirely in [`TerminalGuard`](../crates/tuigram/src/terminal.rs)
(RAII: raw mode + alt screen on construction, restore on `Drop`) plus a panic
hook that restores the terminal before the panic message prints — so a crash
never leaves the user in a wrecked terminal. `run` owns no terminal lifecycle.

## `App` is the single source of truth, `Action` is the only write path

[`App`](../crates/tuigram/src/app.rs) is one struct holding **all** mutable state:
the `dirty`/`should_quit` flags, the three panes' view-models (`chat_list`,
`conversation`, `composer`), the current `focus`, the active `overlay` and each
overlay's state (`search`, `forward`, `reaction`, `media`, `secret`), and the
feedback layer (`connection`, `notifications`). Phase 5 fills these from
fixtures; the doc-comments mark each as "empty until Phase 6 projects the core
store into it".

Every state change goes through one path:

1. A source produces an [`Action`] — a small `Copy` enum (`FocusNext`,
   `SelectNext`, `ScrollUp`, `ComposerInput(char)`, `ToggleHelp`,
   `NoticeDismiss`, `Quit`, …). `on_terminal_event` and `on_app_event` are
   **pure** translators: event → `Action`, no mutation.
2. `App::dispatch(action)` is the **only** function that writes `App`. It
   `match`es the action, mutates, and sets `dirty` when the change is visible.

Keeping translation pure and writes funnelled through one reducer is what makes
the whole UI testable without a terminal: a test builds an `App`, dispatches
`Action`s, and asserts on the resulting state — no async, no TTY.

## The three-pane layout (+ status bar)

[`ui()`](../crates/tuigram/src/ui.rs) is a pure `fn(&mut Frame, &App)` that
composes the view with nested `Layout`s:

```
┌──────────────┬───────────────────────────────┐
│              │  history (Min 0)               │
│  chat list   │                                │   ← content_area
│  (30%)       ├───────────────────────────────┤
│              │  composer (Length 3)           │
├──────────────┴───────────────────────────────┤
│ ● online · #chat — chats        ? help · q quit│  ← status bar (Length 1)
└────────────────────────────────────────────────┘
```

- Outer **vertical** split: the panes (`Min 0`) over a one-row status bar.
- Content **horizontal** split: chat list (`Percentage`) | conversation (`Min 0`).
- Conversation **vertical** split: history (`Min 0`) over a fixed-height composer.

The focused pane is drawn with a highlighted, bold border so the active target
is always obvious. The **status bar** (added in #88) shows the connection state
and current chat/mode on the left and the always-available `? help · q quit`
hint on the right.

A **modal overlay** (help, search, forward, reaction, send-media, secret-chat,
settings) floats above the panes and **captures input** while open. A **transient toast**
also floats above the content but deliberately does **not** capture input, so a
notification never blocks the loop. Both are drawn after the panes; the toast
sits outside the overlay match for exactly that reason.

## The focus-aware keymap

Every binding lives in **one** table, [`BINDINGS`](../crates/tuigram/src/keymap.rs),
not scattered across widgets. [`resolve`] turns a `KeyEvent` into an `Action` by
walking that table in the current [`Focus`] (`ChatList` → `History` →
`Composer`, cycled by Tab) and [`Overlay`]; [`help_sections`] renders the *same*
table into the help overlay, so the cheatsheet can never drift from the bindings
it documents.

Resolution is **focus-aware**: a binding declares the `Context` it applies in
(`Global`, `Nav`, `ChatList`, `History`, `Composer`), so the same physical key
means different things per pane — `j` selects a chat in the list, scrolls in the
history, and types a letter in the composer. A key in the composer that matches
no binding **falls through** to "insert this character", which is why printable
input needs no per-letter entry. Ctrl-chord globals (quit, focus switch,
dismiss-notification) don't fall through to composer text insertion.

## The fake-source boundary

This is the Phase 5 ↔ Phase 6 seam, and the whole point of the spine.
[`event.rs`](../crates/tuigram/src/event.rs) defines `AppEvent` — today only
`AppEvent::Beat`, a liveness heartbeat — and `spawn_fake_source`, a placeholder
task that emits a `Beat` every `HEARTBEAT` (~1s). The loop's `mpsc` arm consumes
it end-to-end, so the core-event path is exercised before a real `Client`
exists; the heartbeat count is rendered as proof the arm is live, and it also
drives the toast TTL clock.

In Phase 6 the real [`tuigram_core::Client`](headless-client.md) update stream
replaces `spawn_fake_source`, and `AppEvent` grows the variants the core already
folds (new messages, auth-state and connection changes, …). **The loop's shape
does not change**: same `select!`, same `on_app_event → Action → dispatch` path.
The view-models are likewise pre-wired — `App` already holds the panes' state and
the Phase-6 ingest seams (e.g. `set_connection`, `notify`) — they're just fed
from fixtures for now, marked `#[allow(dead_code)]` where only the Phase-6 path
reaches them.

## Testing: `TestBackend` snapshots, no TTY

`TestBackend` renders `ui()` into an in-memory `Buffer` with no terminal, so UI
assertions run in normal CI alongside the headless core's plain unit tests. The
harness in [`ui.rs`](../crates/tuigram/src/ui.rs) is one helper —

```rust
fn render(app: &App, width: u16, height: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| ui(frame, app)).unwrap();
    terminal.backend().buffer().clone()
}
```

— and tests assert against the buffer two ways: `flatten` (whole-buffer text,
for "is this content present") and `row_text(y)` (one row, for positional/layout
assertions like "the status bar is on the bottom row" or "the composer sits
above it"). Because the draw path is synchronous and `App` is the single source
of truth, every layer is testable without a TTY: pure reducer tests on
`dispatch`, `resolve` tests on the keymap, and buffer snapshots on `ui()`. This
mirrors the headless-core discipline — logic verified without the I/O it
eventually drives.

## Out of scope (Phase 5)

- **Live Telegram data.** Every pane renders from fixtures; the real `Client`
  feed is Phase 6 (the fake-source boundary above).
- **Real lifecycle exercise.** The spine is proven headlessly; the connected
  paths (a real login, a real send, real `updateConnectionState`) are exercised
  via the REPL against live TDLib in Phase 6, not asserted in CI here — see
  [phase6-verification.md](phase6-verification.md) for the checklist and recorded
  outcomes.

## Trying it

`cargo run -p tuigram` launches the skeleton: Tab cycles focus, `?` opens the
help overlay (generated from the keymap), `q` quits, and the heartbeat counter
ticks in the conversation pane as proof the core-event arm is live.
