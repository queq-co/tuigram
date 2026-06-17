# Research: TDLib integration

> **Phase 1 — findings + decision.** Researched 2026-06-17.

## The `tdjson` interface

TDLib's stable, language-agnostic surface is **`tdjson`**: a tiny C ABI where
every request/response is a JSON object. The four calls that matter:

- `td_json_client_create()` → an opaque client id.
- `td_send(client, request_json)` — fire a request (non-blocking).
- `td_receive(client, timeout)` — poll for the next incoming update/response
  (blocking up to `timeout`). This is the loop we must drive.
- `td_execute(request_json)` — synchronous, for a small set of methods that
  don't need the network (e.g. setting log verbosity).

Responses correlate to requests via an `@extra` field you attach to each
request and get echoed back. Everything else arrives as unsolicited **updates**.

### Auth state machine

Login is driven entirely by the `updateAuthorizationState` update. TDLib emits a
state and waits for you to answer it:

```
waitTdlibParameters   -> setTdlibParameters (api_id, api_hash, db dir, etc.)
waitPhoneNumber       -> setAuthenticationPhoneNumber
waitCode              -> checkAuthenticationCode
waitPassword          -> checkAuthenticationPassword   (2FA, if enabled)
ready                 -> logged in; normal updates flow
loggingOut / closed   -> teardown
```

Phase 2's login UI is literally a state machine that mirrors these states. Note
`waitTdlibParameters` is where `api_id`/`api_hash` enter — see
[app-registration-security.md](app-registration-security.md).

## Binding crate — evaluation

Three real options on crates.io / GitHub today:

| Option | State (2026-06) | tdjson supply | Async | Verdict |
|---|---|---|---|---|
| **`tdlib-rs`** (FedericoBruzzone fork) | Actively maintained; powers the `tgt` TUI client | **Downloads prebuilt tdjson** *or* local/pkg-config/static | tokio-friendly client code, codegen from `td_api.tl` | **Pick this** |
| `tdlib-rs` (paper-plane-developers, original) | Largely stale; the fork above supersedes it | Expects system/pkg-config tdlib | yes | Superseded |
| `rust-tdlib` (aCLr) | Older, less active | Requires tdlib built+installed on the system | yes | Heaviest setup |
| thin custom FFI over `tdjson` | n/a | our problem | our problem | Only if we outgrow the above |

**`tdlib-rs` (FedericoBruzzone)** is the clear winner and aligns exactly with our
Phase 0 decision to use prebuilt binaries:

- **Generator** turns TDLib's `td_api.tl` Type Language file into typed Rust
  request/response structs — no hand-maintaining hundreds of types.
- **Four build modes via Cargo features:**
  - `download-tdlib` — pulls a precompiled tdjson from the crate's GitHub
    releases at build time. **This is our default dev/user path.**
  - `local-tdlib` — use a tdlib you built/installed (`LOCAL_TDLIB_PATH`).
  - `pkg-config` — discover via pkg-config.
  - `static` — statically link tdjson into our binary (combine with
    `download-tdlib` or `local-tdlib`) so the shipped binary needs no tdjson at
    runtime. **This is our release/distribution path.**
- **Prebuilt targets provided:** Linux x86_64, Linux arm64, macOS x86_64, macOS
  arm64, Windows x86_64, Windows arm64 — covers everything we'd distribute.
- **License: MIT OR Apache-2.0** — compatible with our MIT project.
- Pins a known **TDLib version: 1.8.61** (a specific `tdlib/td` commit).

The crate's own `download-tdlib` removes the need for the user (or our CI) to
build TDLib's C++ from source — which directly solves the ~4 GiB-RAM / OOM risk
flagged in Phase 0. `pkg-config`/`libssl-dev`/`zlib1g-dev` are then only needed
for the *from-source* power-user path, not the default.

## Async bridge to tokio

`td_receive` is a blocking poll, so we **don't** call it on an async task
directly. Pattern:

1. Spawn a dedicated **blocking thread** (or `tokio::task::spawn_blocking`) that
   loops on `td_receive(timeout)` and forwards each parsed update into a tokio
   **`mpsc`/`broadcast`** channel.
2. Outbound requests go through `td_send` from anywhere; correlate replies by
   `@extra` (a `oneshot` map: `@extra` → `oneshot::Sender`).
3. `tuigram-core` exposes an async API (`async fn send_message(...)`, an update
   `Stream`) over that bridge; the UI's `select!` loop (see
   [ratatui.md](ratatui.md)) consumes the update stream as `AppEvent`s.

`tdlib-rs` already provides client/async glue in this shape, so we lean on it
rather than reimplementing the receive loop.

## Version pinning & verification

- Pin the **exact `tdlib-rs` version** in `Cargo.lock`; it transitively fixes the
  TDLib version (1.8.61) and the prebuilt artifact. TDLib has **no stable ABI**
  across versions and the server may deprecate old layers, so pinning is
  mandatory and upgrades are deliberate, tested events.
- For `download-tdlib`, record the artifact's checksum in our build notes and
  re-verify on bump. For release binaries prefer **`static`** linking so users
  get one self-contained executable with no `LD_LIBRARY_PATH` dance.

## OpenSSL 3 / zlib on Ubuntu 26.04

Host is **Ubuntu 26.04, OpenSSL 3.5.5, zlib present**. The prebuilt tdjson links
against OpenSSL 3 / zlib, which the system already provides — so the
`download-tdlib` path "just works" without dev headers. The dev packages
(`pkg-config libssl-dev zlib1g-dev`) and a C++ toolchain are only required for
the documented **from-source** build, which we keep as the power-user escape
hatch, not the default.

## Resource usage

TDLib keeps a local **encrypted database + file cache** on disk (configured via
`setTdlibParameters`: `database_directory`, `files_directory`,
`use_message_database`, `use_file_database`, `use_chat_info_database`). For a
lean TUI we can disable the file database / limit cached media and cap automatic
downloads to keep the footprint small. This is tuning for Phase 3, noted here so
the parameters are on the radar at `setTdlibParameters` time.

## Recommendation / decision

- **Binding: `tdlib-rs` (FedericoBruzzone fork).** Actively maintained, codegen
  from `td_api.tl`, MIT/Apache, and — decisively — it ships prebuilt tdjson and
  supports static linking, matching our "prebuilt, no from-source build for
  normal users" constraint.
- **Build modes:** `download-tdlib` for dev; **`download-tdlib` + `static`** for
  release binaries (one self-contained executable). `local-tdlib`/`pkg-config`
  documented for the from-source power-user path.
- **Async:** dedicated blocking `td_receive` thread → tokio channel; `@extra` →
  `oneshot` correlation; core exposes an async API + update `Stream`.
- **Pin** `tdlib-rs` exactly (→ TDLib 1.8.61); treat version bumps as deliberate,
  tested events. Prefer static linking to dodge runtime lib-path issues.
- **No system dev headers needed for the default path** on Ubuntu 26.04; OpenSSL
  3 / zlib are already present. `libssl-dev`/`zlib1g-dev`/`pkg-config` remain
  pending only for the optional from-source build.

## Links

- TDLib: https://core.telegram.org/tdlib · Build: https://tdlib.github.io/td/build.html
- `tdlib-rs` (chosen): https://github.com/FedericoBruzzone/tdlib-rs · docs https://docs.rs/tdlib-rs
- `rust-tdlib`: https://github.com/aCLr/rust-tdlib
- TL language: https://core.telegram.org/mtproto/TL
