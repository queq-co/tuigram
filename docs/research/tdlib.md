# Research: TDLib integration

> **Phase 1 placeholder.** Questions to answer; findings land here.

## Questions

- **`tdjson` interface**: the JSON send/receive/execute model and the auth state
  machine (`updateAuthorizationState`).
- **Binding crate**: evaluate `tdlib-rs` vs `rust-tdlib` vs a thin custom FFI —
  maintenance, async ergonomics, codegen from `td_api.tl`, license.
- **Async bridge**: how to integrate TDLib's receive loop with `tokio`.
- **Delivery**:
  - *Default (users)*: prebuilt `tdjson` shared library — where from, which
    version, how we pin/verify it, and how it links against OpenSSL 3 / zlib.
  - *Power users*: documented from-source build (`cmake`, `gperf`, `clang`,
    OpenSSL/zlib dev headers). RAM-heavy — not required for normal use.
- **Resource usage**: TDLib's local database/cache footprint and tuning.

## Links

- Repo: https://github.com/tdlib/td
- Build instructions: https://tdlib.github.io/td/build.html
