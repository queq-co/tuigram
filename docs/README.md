# tuigram docs

Project documentation index. Milestones and sub-step tracking live on GitHub
(**Issues + Milestones**), not in this folder — see
[the milestones](https://github.com/queq-co/tuigram/milestones).

## Contents

- [architecture.md](architecture.md) — high-level design and the workspace layout.
- [login-flow.md](login-flow.md) — Phase 2 secure login: state machine, credential
  resolution, session storage + key, threat model.
- [headless-client.md](headless-client.md) — the headless client (Phases 3–4): the
  `Client` facade + single update router, the headless model with total content
  mapping, per-domain folding (chats incl. archive/folders, messages incl. media/
  reactions/pins/search/forward, files, chat actions, secret chats, users), the
  send/download lifecycle, read state, and drafts.
- [tui.md](tui.md) — the Phase 5 Ratatui front-end: the central `tokio::select!`
  event loop (nothing awaited in the draw path), `App`-as-single-source-of-truth
  + the `Action` reducer, the three-pane layout + status bar, the focus-aware
  keymap, the fake-source ↔ Phase 6 boundary, and `TestBackend` snapshot testing.
- [wiring.md](wiring.md) — Phase 6, as built: standing up the real `Client` (bootstrap
  → in-TUI login → `Client::start`), the update stream → `AppEvent` → project-folded-
  state → pane path, and how each keypress routes to a per-domain request seam
  fire-and-forget — all without changing the Phase 5 loop's shape.
- [phase6-verification.md](phase6-verification.md) — the Phase 6 milestone gate: the
  real-TDLib lifecycle verification checklist run via the REPL (login, connection,
  send/read/react/pin/forward/search/media/secret, resync, logout), its recorded
  outcomes, and any gaps filed.
- [../settings.example.toml](../settings.example.toml) — annotated template for
  `~/.config/tuigram/settings.toml`: download-cache retention (per-kind TTLs and the
  global size backstop). Optional; copy it into place to opt in.
- [branch-model.md](branch-model.md) — how we use `develop` → PR → `main`.
- Research (Phase 1):
  - [research/ratatui.md](research/ratatui.md) — how Ratatui works and how we'll use it.
  - [research/tdlib.md](research/tdlib.md) — TDLib integration (prebuilt `tdjson` + binding crate).
  - [research/app-registration-security.md](research/app-registration-security.md) —
    distributable + secure Telegram app registration (`api_id`/`api_hash`).

## Roadmap (phases)

| Phase | Milestone | Focus |
|------:|-----------|-------|
| 0 | Bootstrap & GitHub integration | Repo access, toolchain, branch model — **done** |
| 1 | State of the art & security research | Ratatui, TDLib, app registration |
| 2 | Secure login | Authenticate as a user via TDLib |
| 3 | Core client features (headless) | List chats/messages, send, reply — **done** |
| 4 | Extended client features (headless) | Media, archive/folders, search/forward, reactions/pins, chat actions, secret chats, full login — **done** |
| 5 | TUI | Ratatui interface — event loop, panes, keymap, overlays, status/toasts (fixtures) — **done** |
| 6 | Wire Telegram ↔ TUI (MVP) | Render & interact over real data — **done** |
