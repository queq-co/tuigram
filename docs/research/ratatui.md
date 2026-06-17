# Research: Ratatui

> **Phase 1 — findings + decision.** Researched 2026-06-17 against Ratatui 0.30.x.

## Version landscape (as of 2026-06)

- **Ratatui 0.30.x** is current (0.30.1, released June 2026). It is the first
  series to ship the multi-backend crossterm story we rely on below.
- **crossterm 0.29** is the default, natively-supported backend for 0.30.
  Ratatui 0.30 also exposes `crossterm_0_28` / `crossterm_0_29` feature flags so
  a transitive widget crate pinned to an older crossterm doesn't fracture the
  dependency graph. We pin **crossterm 0.29** and avoid mixing majors — the docs
  warn that two semver-incompatible crossterm versions keep *separate event
  queues* (lost events / races) and *track raw mode separately* (terminal not
  restored on exit). One crossterm major, full stop.

## Rendering model

Ratatui is **immediate-mode and synchronous**. Each frame you call
`terminal.draw(|frame| { ... })`; inside the closure you render widgets onto a
`Frame` backed by a `Buffer` (a grid of `Cell`s). Ratatui diffs the new buffer
against the previous one and writes only the changed cells to the backend. There
is no retained widget tree and no internal async — the library never blocks on
I/O itself, which is exactly what we want: **all blocking lives in
`tuigram-core`, never in the draw path.**

Implication for us: the UI is a pure function of application state. The draw
closure reads an `App` snapshot and renders; it must not await anything.

## Backend choice

`CrosstermBackend` via the default `crossterm` feature. Crossterm is the
portable choice (Linux/macOS/Windows, which matters for "distributable") and is
what every current Ratatui example and async template targets. Termion
(Unix-only) and Termwiz (heavier, tied to wezterm) bring no advantage for a
cross-platform chat client. The backend also owns raw mode, the alternate
screen, and mouse/keyboard/resize event capture.

## Async event loop (the core integration question)

The library is sync; the app is async (TDLib pushes updates from a background
thread/runtime). The current idiomatic pattern — from Ratatui's own `async`
example and the async-template — is:

1. Run on **tokio**.
2. Use crossterm's **`EventStream`** (the `event-stream` feature) so terminal
   input arrives as an async `Stream` instead of a blocking `poll`/`read`.
3. Drive everything from a single **`tokio::select!`** loop that races:
   - terminal input events (`EventStream`),
   - a render tick (e.g. an `interval` at ~16–60 ms / target FPS),
   - **application events from `tuigram-core`** delivered over an
     `mpsc`/`broadcast` channel (new messages, auth-state changes, etc.).

The select loop translates each source into an internal `Action`/`Message`,
mutates `App` state, and requests a redraw. `terminal.draw()` stays on the main
task and is never awaited inside. This decouples render cadence from network
latency: TDLib can be mid-request and the UI still repaints and stays
responsive. This is the standard "message/action + central event loop"
architecture (Elm-ish) the Ratatui community converged on.

### Concretely for tuigram

```
core (tokio task)  --AppEvent-->  mpsc::Receiver
                                      |
main loop: tokio::select! {
    Some(ev) = events.next()  => handle_terminal(ev)   // crossterm EventStream
    _        = tick.tick()     => /* mark dirty */
    Some(ae) = core_rx.recv()  => apply(ae)            // from tuigram-core
}
=> if dirty { terminal.draw(|f| ui(f, &app)) }
```

## Layout for a chat client

`Layout` + `Constraint` compose the three-pane view we want:

- Outer horizontal split: **chat list** (left, fixed/percentage width) |
  **conversation** (right, fills remainder).
- Right pane vertical split: **message history** (`Min`) over a **composer**
  input line (`Length(3)` or so).
- Message history is the scrolling problem child: render with a `Paragraph`
  (wrap) or a `List`, tracking scroll offset in `App` state. For long histories,
  windowing (render only visible range) beats building the full buffer.

Keep **widget state we own** (selection index, scroll offset, input buffer) in
`App`; use Ratatui's `StatefulWidget` + `ListState`/`ScrollbarState` where the
widget needs render-time state.

## Testing

`TestBackend` renders into an in-memory `Buffer` with no TTY, so UI assertions
run in normal CI. Assert against the buffer (`assert_buffer_eq!` style / snapshot
of `Buffer`). This pairs with the headless `tuigram-core`: core logic is plain
unit tests, and the `ui(frame, &app)` function is tested by feeding a known
`App` to a `TestBackend` and snapshotting the result. No terminal required for
either layer.

## Recommendation / decision

- **Ratatui 0.30.x + crossterm 0.29**, single crossterm major, `CrosstermBackend`.
- **Architecture: central `tokio::select!` event loop** racing crossterm
  `EventStream`, a render tick, and an `mpsc` channel of `AppEvent`s from
  `tuigram-core`. UI is a pure `fn ui(&mut Frame, &App)`; nothing is awaited in
  the draw path. This is what keeps the TUI responsive while the network layer
  blocks/awaits independently — our #1 "Goals" requirement.
- **State:** single `App` struct as source of truth; `StatefulWidget` only where
  Ratatui needs per-widget render state. Three-pane `Layout` (list | history /
  composer).
- **Testing:** `TestBackend` buffer snapshots for the `ui()` function; pure unit
  tests for everything in core. No TTY in CI.

## Links

- Site: https://ratatui.rs · Backends: https://ratatui.rs/concepts/backends/
- Async events tutorial: https://ratatui.rs/tutorials/counter-async-app/full-async-events/
- Releases: https://github.com/ratatui/ratatui/releases
- Async integration overview: https://deepwiki.com/ratatui/ratatui/4.5-asynchronous-operations
