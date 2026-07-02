# Architecture

> Living document. The Phase 1 open decisions are now resolved (see below) and
> implemented across Phase 2; Phase 3 builds the headless core client on top of
> them, and Phase 4 extends that client (media, archive/folders, search/forward,
> reactions/pins, chat actions, secret chats, full login) without revisiting it.
> Phase 5 adds the Ratatui front-end as a spine over fixtures (see below;
> [tui.md](tui.md) holds the detail), and Phase 6 feeds that spine the real
> `Client` — live updates in, actions out — without changing its shape
> ([wiring.md](wiring.md) holds the detail). Later phases extend rather than
> revisit these decisions.

## Goals

- **Responsive** — a TUI must never block on network I/O.
- **Secure** — protect user credentials/session and the app's `api_id`/`api_hash`.
- **Distributable** — buildable and shippable without leaking secrets or
  requiring each user to compile TDLib from scratch.
- **Tested** — core logic unit-tested without a terminal.

## Workspace layout

A Cargo **workspace** with two crates, separating Telegram logic from UI:

```
tuigram/
├── Cargo.toml              # workspace manifest
├── rust-toolchain.toml     # pinned stable toolchain + rustfmt/clippy
├── crates/
│   ├── tuigram-core/       # library: TDLib client, auth, chats, messages (headless)
│   └── tuigram/            # binary: Ratatui TUI, depends on tuigram-core
└── docs/
```

- **`tuigram-core`** — all Telegram/TDLib logic. No terminal dependencies, so it
  is unit-testable in CI without a TTY. This is where Phases 2–3 are built.
- **`tuigram`** — the Ratatui front-end (Phases 4–5), depending on `-core`.

This split directly serves the testing requirement and keeps UI churn from
touching protocol logic.

## Resolved decisions (Phase 1 research → Phase 2 implementation)

The Phase 1 research closed every open decision below; each is now implemented in
`tuigram-core`. The research docs hold the full reasoning — this is the summary.

- **Async runtime — `tokio`.** TDLib's `tdjson` is poll-based (`td_receive`
  blocks), so a dedicated blocking thread pumps it and fans updates out over a
  tokio `broadcast` channel; requests correlate by `@extra`. The
  [`Bridge`](../crates/tuigram-core/src/bridge.rs) owns that loop and exposes an
  async request API + update `Stream` behind the `TgClient` seam, so logic above
  it is unit-testable without a live `tdjson`. See
  [research/tdlib.md](research/tdlib.md#async-bridge-to-tokio).
- **TDLib binding — `tdlib-rs` (FedericoBruzzone fork).** Codegen from
  `td_api.tl`, MIT/Apache, and decisively it ships prebuilt `tdjson`. Pinned
  exactly (`=1.4.0`), which transitively fixes **TDLib 1.8.61**; bumps are
  deliberate, tested events. See [research/tdlib.md](research/tdlib.md#binding-crate--evaluation).
- **TDLib delivery — prebuilt `tdjson`, no from-source build for normal users.**
  `download-tdlib` for dev/users (CI exercises it across all three hosted
  targets); `download-tdlib` + `static` for release binaries. OpenSSL 3 / zlib
  (+ libc++ on Linux) stay a **per-target runtime contract**, provisioned per OS
  and **audited in CI** with `ldd`/`otool`/`dumpbin`, not assumed from one host.
  See [research/tdlib.md](research/tdlib.md#native-dependencies-openssl--zlib-across-targets).
- **`api_id`/`api_hash` strategy — user-supplied, zero credentials in the repo.**
  Resolved per user in order env → config → first-run onboarding
  ([`credentials`](../crates/tuigram-core/src/credentials.rs)); a bundled FOSS
  credential would trip `API_ID_PUBLISHED_FLOOD` and sit awkwardly against ToS
  2.1. An opt-in, build-time, official-binary-only injection path stays available
  to maintainers (secret never committed). See
  [research/app-registration-security.md](research/app-registration-security.md).

The Phase 2 login that ties these together is documented in
[login-flow.md](login-flow.md), including the session-protection threat model.

## Resolved decisions (Phase 3 — headless core client)

Phase 3 builds the headless client surface (list chats/messages, send, reply,
edit, delete, read state, drafts) on the Phase 2 bridge. Two decisions shape it;
[headless-client.md](headless-client.md) holds the full reasoning.

- **One `Client` facade + a single update router.** TDLib pushes account content
  as a `broadcast` firehose of updates. Rather than have each subsystem subscribe
  and clone the whole stream, a single long-lived router task — the **only**
  always-on subscriber — drains it once, classifies each update, and dispatches it
  O(1) to the owning domain's reducer behind an `UpdateSink` seam. One update
  clone, one auditable data path for account content, and O(1) growth as domains
  are added. The router holds no business logic (classification only), so each
  reducer stays independently unit-testable with synthetic updates, and a
  broadcast `Lagged` is resynced rather than silently dropped. The
  [`Client`](../crates/tuigram-core/src/client.rs) facade owns that router and the
  folded state so the app holds one handle.
- **A headless model + per-domain request seams.** The crate depends on its own
  [`model`](../crates/tuigram-core/src/model.rs) types (`Chat`, `Message`,
  `Draft`, …), projected from `tdlib_rs` shapes — the same insulation `AuthState`
  gave Phase 2, with content mapping kept **total** (`Unsupported(name)` for
  anything unmodeled). Each domain owns its slice of the request surface as its
  own trait (`ChatRequests`/`MessageRequests`/`UserRequests`), segregated exactly
  as `AuthRequests` was, so the [bridge](../crates/tuigram-core/src/bridge.rs)
  stays pure transport.

## Resolved decisions (Phase 4 — extended client features)

Phase 4 widens the headless client (structured media content + a download/upload
lifecycle, the Archive list and folders, message search and forward, reactions and
pins, chat actions, secret chats) and finishes the login machine. Its one
architectural decision is **not to make a new one**;
[headless-client.md](headless-client.md) (which documents the core and extended
surface as one client) holds the full reasoning.

- **Extend the Phase 3 pattern, don't revisit it.** Every new capability is the
  same shape as before: a new domain is a write-seam + a read-store the single
  router folds into (files, chat actions, secret chats), and a widened domain just
  adds requests and folds to an existing one (messages gain media/reactions/pins/
  search/forward; chats gain archive/folders). The router stays logic-free, the
  bridge stays pure transport, and request traits stay segregated per domain — so
  the surface grows O(1) without a central file accreting knowledge. Two existing
  invariants are deliberately **kept, not relaxed**: `MessageContent::from_tdlib`
  stays **total** (real variants now, the rest still `Unsupported(name)`), and with
  every login state handled, `AuthState::from_tdlib` becomes total by *exhaustive*
  match — a future TDLib state is a compile error, not a silent miss. Search is the
  one read that stays *beside* the fold rather than in it: it returns a transient
  result view so a query never mutates the owned history.

## Resolved decisions (Phase 5 — TUI)

Phase 5 builds the Ratatui front-end in the `tuigram` binary as a self-contained
spine over **fixtures** — no live Telegram data yet. It makes no new core
decisions; it realises the Phase 1 Ratatui research ([research/ratatui.md](research/ratatui.md))
and is structured so Phase 6 only has to feed it. [tui.md](tui.md) holds the full
walkthrough; the shaping decisions are:

- **One central `tokio::select!` loop; nothing awaited in the draw path.** A
  single loop in [`main.rs`](../crates/tuigram/src/main.rs) races terminal input,
  a render tick, and a core-event channel into [`Action`]s. `terminal.draw()`
  stays on the main task and is never awaited, so render cadence is decoupled
  from network latency — the #1 "responsive, never block on I/O" goal. Repaint is
  gated on a `dirty` flag; the terminal lifecycle is RAII ([`TerminalGuard`](../crates/tuigram/src/terminal.rs))
  plus a panic hook that restores the terminal before the message prints.
- **`App` is the single source of truth; `Action` is the only write path.** All
  mutable state lives in one [`App`](../crates/tuigram/src/app.rs) struct; event
  translation (`on_terminal_event`/`on_app_event`) is **pure** (event → `Action`,
  no mutation) and `App::dispatch` is the **only** function that writes. This
  Elm-ish split is what makes the whole UI testable without a terminal.
- **One focus-aware keymap table.** Every binding lives in `BINDINGS`
  ([`keymap.rs`](../crates/tuigram/src/keymap.rs)); `resolve` reads it in the
  current `Focus`/`Overlay` and the help overlay renders the *same* table, so the
  cheatsheet can't drift. Unbound keys in the composer fall through to text
  insertion, so printable input needs no per-letter entry.
- **The fake-source boundary is the only Phase 6 wiring point.**
  [`event.rs`](../crates/tuigram/src/event.rs) defines the `AppEvent` seam and a
  temporary heartbeat source; Phase 6 replaces the source with the real
  [`Client`](headless-client.md) update stream and grows `AppEvent` **without the
  loop's shape changing**. The panes already hold their view-model state and the
  Phase-6 ingest seams (e.g. `set_connection`, `notify`), fed from fixtures for
  now.
- **`TestBackend` snapshots keep the UI testable with no TTY.** `ui()` is a pure
  `fn(&mut Frame, &App)` rendered into an in-memory `Buffer`; tests assert on
  whole-buffer text and per-row layout, alongside the headless core's plain unit
  tests — no terminal in CI, the same discipline as the core.

## Resolved decisions (Phase 6 — wire Telegram ↔ TUI)

Phase 6 feeds the Phase 5 spine the real [`Client`](headless-client.md): login,
live updates, and every action routed to a real request seam. It makes no new core
decisions — it realises the seam Phase 5 left open, and the loop's shape is
unchanged. [wiring.md](wiring.md) holds the full walkthrough; the shaping decisions
are:

- **The fake source becomes the real one; the loop is untouched.** Phase 5 fed the
  loop's mpsc arm from a heartbeat; Phase 6 feeds it from
  [`spawn_core_source`](../crates/tuigram/src/event.rs), which subscribes to the
  client's update feed, classifies each event, and forwards it onto the *same*
  channel. The three-phase client standup (bootstrap on the plain terminal →
  in-TUI login → `Client::start` only on `Ready`) lives in
  [`main.rs`](../crates/tuigram/src/main.rs); TDLib is closed cleanly on every exit
  path so its database is never left mid-write.
- **`AppEvent` is a redraw signal, not the data.** Each variant means "this domain
  may have changed, repaint"; the projection reads the current folded state back
  from the `Client` (`Connection` is the one exception, carrying its already-
  projected state). `classify` mirrors the core router's own routing and drops
  unmodelled updates at the source, so the loop only wakes for redraw-worthy
  signals. A second, independent lagged-aware subscription means a broadcast gap
  here is harmless — the router folded the authoritative state regardless, so
  `Lagged` just re-projects.
- **Projections read the store; `App` stays pure.** Reading folded state needs the
  `Client`, so the `project_*` calls live in the loop and hand `App` an **owned**
  snapshot (`Vec<Message>`, projected lists) — `App` never holds a `Client`, so the
  reducer and every pane stay unit-testable without a live core.
- **Actions are pure intents, drained to fire-and-forget seams.** A keypress
  records an intent on `App`; the loop drains it and routes it to the matching
  per-domain request seam off an `Arc<Client>` clone, never awaiting in the loop.
  The request's return value never feeds the UI — the authoritative result arrives
  as an update the router folds and the loop re-projects, the same path an
  unsolicited change takes. Only a seam-level rejection reports back, as an error
  toast carrying a fixed TDLib error code, never user content. Reaction/pin toggles
  apply optimistically and reconcile on the real update.
