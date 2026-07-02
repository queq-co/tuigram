# Wiring Telegram ↔ TUI (Phase 6)

> Phase 5 built the [TUI spine](tui.md) over **fixtures**: one `tokio::select!`
> loop, one `App` source of truth, the panes and overlays, all rendering from fake
> data. Phase 6 swaps the fake source for the real
> [`tuigram_core::Client`](headless-client.md) — login, live updates, and every
> action routed to a real request seam — **without changing the loop's shape**.
> This doc is that swap, as built. The verification that it actually works against
> live TDLib is [phase6-verification.md](phase6-verification.md).

## The shape of the problem

The spine was deliberately designed so Phase 6 would be a *feed*, not a rewrite:
the loop already raced three sources into `Action`s, `App` already held every
pane's view-model, and the ingest seams (`set_connection`, `notify`, the
`project_*` methods) already existed — fed from fixtures. So Phase 6 is three
questions, each answered without touching the loop:

1. **How does a real session start?** (login on the plain terminal, then in the
   TUI, then `Client::start`.)
2. **How does a live update become a repaint?** (the update stream → `AppEvent`
   → read the folded state back → project it onto a pane.)
3. **How does a keypress become a real Telegram action?** (a pure intent on `App`
   → the loop drains it → a per-domain request seam, fire-and-forget.)

The through-line is that **the request's return value never feeds the UI**. Every
action is fire-and-forget; the authoritative result comes back the *same* way an
unsolicited change does — as an update the core router folds and the loop
re-projects. Input and output share one pipeline, so there is no second,
divergent "did my send land" path to keep consistent.

## Standing up the session ([`main.rs`](../crates/tuigram/src/main.rs))

The real `Client` comes up in three stages, each on the right surface:

1. **Bootstrap on the plain terminal (#109).**
   [`bootstrap`](../crates/tuigram/src/bootstrap.rs) resolves credentials
   (env → config → first-run onboarding), opens secure session storage, and sends
   `setTdlibParameters` — all **before** [`TerminalGuard`](../crates/tuigram/src/terminal.rs)
   enters raw mode. `setTdlibParameters` is the request that surfaces a bad
   `api_id` as `API_ID_PUBLISHED_FLOOD`, so keeping it here means that failure
   prints its actionable, multi-line guidance on a normal screen instead of a
   single line buried in a raw-mode TUI. It returns an *initialized but not
   logged-in* [`Bridge`](../crates/tuigram-core/src/bridge.rs).
2. **Login inside the TUI (#111).** [`run_login`](../crates/tuigram/src/login.rs)
   drives one screen per waiting auth state (phone → code → 2FA/QR) through the
   core `Login` seam, gating the three-pane UI behind `Ready`. Only on
   `LoginEnd::Ready` does the bridge become a live client:
   `Arc::new(Client::start(bridge))` — the `Arc` so background loaders can each
   hold a clone while the bridge stays reachable for shutdown.
3. **Clean shutdown on every exit path.** `bootstrap::shutdown` closes TDLib and
   waits for `Closed` so the local database is flushed, never left mid-write — for
   a full run *and* for a login the user quit before `Client::start` ever ran
   (there is no `Client` then, only the bridge).

## From update stream to repaint ([`event.rs`](../crates/tuigram/src/event.rs))

This is the Phase 5 ↔ 6 seam. Phase 5 fed the loop's mpsc arm from
`spawn_fake_source` (a heartbeat); Phase 6 (#110) feeds it from
[`spawn_core_source`], which subscribes to the client's live update feed,
classifies each event, and forwards it onto **the same mpsc channel**. The loop's
`select!` arm is byte-for-byte the one the heartbeat used.

Two design choices make this cheap and safe:

- **`AppEvent` is a signal, not the data.** Each variant — `Chats`, `Messages`,
  `File`, `Secret`, `Auth`, `Lagged` — means only "this domain may have changed,
  repaint". The projection reads the *current* folded state back from the
  `Client`, so the event carries no payload to keep in sync. The lone exception is
  `AppEvent::Connection(ConnectionState)`, which carries the already-projected
  state so the status bar folds it without a second core read.
  [`classify_update`] mirrors the core router's own routing (chats / messages /
  files / secret / connection), plus the post-login `updateAuthorizationState` the
  router ignores but the UI cares about; a new, unmodelled update defaults to
  `None` — **dropped at the source**, so the loop only wakes for redraw-worthy
  signals and idle connectivity/metadata churn never spins it.
- **A second, independent subscription.** `spawn_core_source` takes its *own*
  lagged-aware subscription, separate from the router's. The router keeps folding
  the authoritative account state on its subscription; this one only nudges the UI
  to repaint. So a broadcast gap here is harmless: it surfaces as `AppEvent::Lagged`
  and the loop simply re-projects every pane — the state it re-reads is still
  correct because the *router's* subscription folded it regardless.

## Projecting the folded store onto the panes

When a signal arrives, the loop reads the folded state back through the client's
`read` snapshot seam and hands the **owned** result to a pure `App::project_*`
method. The projections live in [`main.rs`](../crates/tuigram/src/main.rs) rather
than inside `App` precisely because they need the `Client` to read — `App` only
ever receives an owned snapshot, so it stays testable without a live core:

| Signal | Read back | Projected onto |
|--------|-----------|----------------|
| `Chats` | `project_lists(s.chats())` | `app.project_chats` — the left pane's lists (Main/Archive/folders) (#113); also re-projects secret-chat rows |
| `Messages` | `s.messages().history(chat_id)` + pinned ids + file states | `app.project_conversation` + `app.project_downloads` — the open chat (#114) |
| `File` | the same conversation read | download-progress lines on the open chat (#120) |
| `Secret` | `project_secret_states(s.chats(), s.secret_chats())` | `app.project_secret_states` — pending → ready → closed on the row (#121) |
| `Connection` | *(carried on the event)* | `app.on_app_event` → the status bar |
| `Lagged` | all of the above | re-project every pane, to be safe |

The conversation projection also drives **history paging** and **read state**: on
opening a chat the loop fetches its landing page once (`get_chat_history` →
`merge_history`), a scroll-up at the very top pages older, and while a chat is open
its unread incoming messages are acknowledged through `view_messages` — the read
marker then folds back as `updateChatReadInbox` and re-projects, clearing the
unread badge here and on the user's other clients.

## Routing actions back to the request seams

Input never calls the network directly. The keymap and reducer record a **pure
intent** on `App` (`Submission`, `ForwardIntent`, `ReactionIntent`, …); after each
`select!` iteration the loop drains those intents and routes each to its matching
per-domain request seam, spawned off an `Arc<Client>` clone so the round-trip
never blocks the loop:

| Intent (drained from `App`) | Seam called | Issue |
|-----------------------------|-------------|-------|
| composer submit → `drive_outbound` | `SendRequests::send_text` / `EditRequests::edit_text` | #116 |
| `drive_read_state` | `ReadRequests::view_messages` | #115 |
| search query → `drive_search` | `search_chat` / `search_global` | #117 |
| forward → `drive_forward` | `ForwardRequests::forward_messages` | #118 |
| reaction → `drive_reaction` | `ReactionRequests` add/remove | #119 |
| pin → `drive_pin` | `PinRequests` pin/unpin | #119 |
| attachment → `drive_media` | `SendRequests::send_media` | #120 |
| secret lifecycle → `drive_secret` | `SecretChatRequests` create/close | #121 |
| open-chat media → `drive_downloads` | `FileRequests::download_file` | #120 |
| list paging → `ensure_active_list_loaded` | `load_main`/`archive`/`folder_list` | #113 |

Every one of these is **fire-and-forget**, and two properties fall out of that:

- **The result arrives as an update, not a return value.** A send streams its
  optimistic `Pending` message, then its `Sent`/`Failed` resolution, as updates the
  router folds and the loop re-projects — so the composer's feedback comes through
  the normal projection pipeline. Only a *seam-level rejection* (the request never
  reaching `Pending` — e.g. `CHAT_WRITE_FORBIDDEN`) reports back, as an error toast
  on the `outbound_tx` channel, its message a fixed TDLib error code (never the
  user's typed content).
- **Optimistic where it helps.** Reaction and pin toggles flip `App` state
  immediately so the chip/📌 responds instantly; the authoritative
  `updateMessageInteractionInfo` / `updateMessageIsPinned` then folds and the next
  projection reconciles over the optimistic state.

## The loop's shape did not change

That is the whole claim of the phase, and it holds. The Phase 5 loop —

```rust
while !app.should_quit() {
    if app.is_dirty() { guard.terminal_mut().draw(|frame| ui::ui(frame, &app))?; app.clear_dirty(); }
    tokio::select! {
        maybe_event    = input.next()   => /* key/resize → Action */,
        _              = tick.tick()    => app.dispatch(Action::Render),
        maybe_app_event = core_rx.recv() => /* AppEvent → project/dispatch */,
    }
}
```

— is still exactly this. Phase 6 changed only what plugs into it: `core_rx` now
comes from `spawn_core_source(client)` instead of the heartbeat, the core arm
projects real folded state instead of counting beats, and a handful of extra arms
(completion channels for spawned history/search/send loaders, a notice-ageing
tick, a retention-sweep tick) were added **beside** the originals. Nothing is
awaited in the draw path, repaint is still gated on the `dirty` flag, and event
translation is still pure (`on_terminal_event` / `on_app_event` → `Action` →
`dispatch`). The seam Phase 5 promised was the only wiring point turned out to be
exactly that.

## See also

- [tui.md](tui.md) — the Phase 5 spine this feeds.
- [headless-client.md](headless-client.md) — the `Client`, its router, the folded
  stores, and the per-domain request seams this wires to.
- [phase6-verification.md](phase6-verification.md) — the real-TDLib lifecycle
  verification pass that confirms the wiring end-to-end.
