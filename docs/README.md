# tuigram docs

Project documentation index. Milestones and sub-step tracking live on GitHub
(**Issues + Milestones**), not in this folder — see
[the milestones](https://github.com/queq-co/tuigram/milestones).

## Contents

- [architecture.md](architecture.md) — high-level design and the workspace layout.
- [login-flow.md](login-flow.md) — Phase 2 secure login: state machine, credential
  resolution, session storage + key, threat model.
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
| 3 | Core client features (headless) | List chats/messages, send, reply |
| 4 | TUI | Ratatui interface |
| 5 | Wire Telegram ↔ TUI (MVP) | Render & interact over real data |
