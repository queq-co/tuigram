# Architecture

> Living document. Decisions here are revisited as Phase 1 research lands.

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

## Open decisions (tracked in Phase 1)

- **Async runtime**: TDLib's `tdjson` is callback/poll based; we need an async
  bridge. `tokio` is the likely choice — confirm against the binding crate.
- **TDLib binding**: `tdlib-rs` vs `rust-tdlib` vs thin custom FFI over `tdjson`.
- **TDLib delivery**: prebuilt `tdjson` for users; documented from-source build
  for power users. RAM-heavy source builds are *not* required for normal use.
- **`api_id`/`api_hash` strategy**: the core distribution/security question.
