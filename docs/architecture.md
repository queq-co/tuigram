# Architecture

> Living document. The Phase 1 open decisions are now resolved (see below) and
> implemented across Phase 2; later phases extend rather than revisit them.

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
