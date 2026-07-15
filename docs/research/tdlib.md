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
- **Prebuilt targets provided:** Linux x86_64, Linux arm64, macOS arm64, Windows
  x86_64, Windows arm64 — covers everything we'd distribute. (The crate also ships
  a macOS x86_64 prebuilt, but we do not support Intel Macs.)
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

## Native dependencies (OpenSSL / zlib) across targets

The prebuilt `tdjson` is not self-contained: it dynamically links **OpenSSL 3**
and **zlib**, which are *TDLib's own* native deps, not ours — and on **Linux** it
is built against **LLVM's `libc++`** (not the system `libstdc++`), adding
`libc++.so.1` / `libc++abi.so.1` to the runtime set there. Crucially, the
`tdlib-rs` **`static` feature statically links `tdjson` only** — it does **not**
bundle OpenSSL/zlib/libc++. So these remain a runtime requirement on every
target, even for a "static" build. We therefore treat them as a **per-target
contract** that each platform must satisfy and that CI **verifies empirically**
(a linkage audit with `otool -L` / `ldd` / `dumpbin`), rather than assuming a
single host.

Build deps split into two layers, handled uniformly everywhere:

| Layer | What | Controlled by | Agnostic? |
|---|---|---|---|
| `tdjson` | TDLib C ABI lib | `tdlib-rs` features: `download-tdlib` (+ `static`) | Yes — same on all targets |
| OpenSSL 3 + zlib (+ libc++ on Linux) | TDLib's transitive native deps | **not** removed by `static`; satisfied at runtime per OS | Needs the contract below |

### Runtime contract per target

| Target | OpenSSL | zlib | C++ runtime | Satisfied by |
|---|---|---|---|---|
| linux x86_64 / arm64 | `libssl.so.3`, `libcrypto.so.3` | `libz.so.1` | **`libc++.so.1`, `libc++abi.so.1`** *(measured in CI)* — LLVM libc++, **not** libstdc++ | distro pkgs: `libssl3`, `zlib1g` (usually preinstalled) + **`libc++1`, `libc++abi1`** (**not** preinstalled — must be provisioned) |
| macOS arm64 | **`/opt/homebrew/opt/openssl@3/lib/libssl.3.dylib` + `libcrypto.3.dylib`** *(measured)* | `/usr/lib/libz.1.dylib` (system) *(measured)* | system `libc++` (always present on macOS) | dev: `brew install openssl@3` (suggested); release: bundled — see Distribution strategy |
| windows x86_64 / arm64 | bundled in prebuilt *(confirm in CI)* | bundled | bundled (MSVC runtime) | nothing |

> **Measured on this M4 (aarch64-apple-darwin), tdlib-rs 1.4.0 / TDLib 1.8.61.**
> `otool -L libtdjson.1.8.61.dylib` →
> `/opt/homebrew/opt/openssl@3/lib/libssl.3.dylib`,
> `/opt/homebrew/opt/openssl@3/lib/libcrypto.3.dylib`, `/usr/lib/libz.1.dylib`.
> The OpenSSL paths are **absolute Homebrew paths, not `@rpath`** — so a *plain*
> `download-tdlib` build loads `openssl@3` from the Homebrew prefix at runtime,
> and `static` linking `tdjson` does **not** remove it. zlib is satisfied by the
> system, so no Homebrew zlib is needed. Our distribution strategy (below)
> removes the Homebrew-at-runtime requirement for shipped builds.

> **Measured in CI (ubuntu x86_64, Phase 2 #4).** A plain `download-tdlib` build
> linked and compiled, but `cargo test` failed at *runtime* loading the test
> binary: `libc++.so.1: cannot open shared object file`. The Linux prebuilt
> `tdjson` is built against LLVM `libc++`, which ubuntu runners don't preinstall
> (they ship `libstdc++`). Fix: provision `libc++1` + `libc++abi1` on Linux
> (CI step + [`check-native-deps.sh`](../../scripts/check-native-deps.sh) hint).
> macOS/Windows were unaffected — `libc++` is macOS's system C++ runtime, and the
> Windows prebuilt bundles its runtime. This is exactly the kind of host-specific
> gap the "verify empirically per target" stance is meant to catch.

> **Measured in CI (ubuntu-24.04-arm, linux-arm64, #173).** A plain
> `download-tdlib` build linked, compiled, and passed the full linkage audit on
> the first run — `ldd libtdjson.so.1.8.61` resolves the identical dependency
> set as x86_64 (`libssl.so.3`, `libcrypto.so.3`, `libz.so.1`, `libc++.so.1`,
> `libc++abi.so.1`), just from `/lib/aarch64-linux-gnu/` instead of
> `/lib/x86_64-linux-gnu/`. tdlib-rs 1.4.0's aarch64 prebuilt tdjson needs no
> extra provisioning beyond what x86_64 already required, confirming the
> combined "linux x86_64 / arm64" table row above.

### Distribution strategy: native, one place per OS/arch

Principle: resolve deps **as natively as possible** (stay dynamically linked the
platform-natural way — no from-source static OpenSSL), and keep each platform's
resolution in **one place**. Homebrew is **suggested for local dev, never
required at end-user runtime**.

| OS | Native resolution | Single place |
|---|---|---|
| Linux | use the system `libssl.so.3` / `libz.so.1` (usually preinstalled) **plus `libc++1` / `libc++abi1`** (not preinstalled); declare all as package deps | package manifest + [`check-native-deps.sh`](../../scripts/check-native-deps.sh) |
| macOS (arm64) | **bundle** `libssl.3.dylib` + `libcrypto.3.dylib` beside the binary and rewrite the Mach-O load commands to `@loader_path` via `install_name_tool` (then re-`codesign`); zlib stays the system `/usr/lib/libz` | [`scripts/bundle-native-deps.sh`](../../scripts/bundle-native-deps.sh) |
| Windows (x86_64 + arm64) | OpenSSL/zlib are already bundled in the prebuilt | the prebuilt (no step) |

For macOS this is the most native option that doesn't force a package manager on
users: the same dynamically-linked OpenSSL 3, shipped *with* the app and loaded
relatively, so the binary is self-contained. The OpenSSL dylibs are copied at
**release time** from the build host's `openssl@3` (present on GitHub macOS
runners and dev machines) — so brew is a build-time convenience, not a runtime
dependency. `bundle-native-deps.sh` is a no-op on Linux/Windows by design, and
finishes by re-running the linkage audit to assert no absolute Homebrew paths
remain.

### Release (static) build — measured (#167)

All of the above was measured on the **dynamic** `download-tdlib` build. The
`download-tdlib + static` build releases actually ship was designed but never
built or measured until #167. First data point, macOS arm64:

> **Measured on this M4 (aarch64-apple-darwin), `cargo build --release
> --features tuigram/static` (→ `tuigram-core/static` → `tdlib-rs/static`,
> confirmed via `cargo tree -e features -i tdlib-rs`), tdlib-rs 1.4.0 / TDLib
> 1.8.61.** `otool -L target/release/tuigram` shows **no** `libssl`,
> `libcrypto`, `libz`, or `libtdjson` reference at all — only system
> frameworks (AppKit, Foundation, CoreGraphics, Security, libc++, libiconv,
> libSystem). This **contradicts the assumption above** (carried over from the
> dynamic build) that a static macOS build still references Homebrew OpenSSL
> by absolute path: it does not. `scripts/bundle-native-deps.sh` run against
> this binary confirms it — "No absolute openssl@3 references ... already
> bundled or statically resolved" — and exits 0 as a no-op. The macOS
> `static` prebuilt appears to statically link OpenSSL and zlib alongside
> tdjson itself, not just tdjson. **Practical effect:** the macOS release
> artifact needs no `lib/` bundling step and no Homebrew at build *or* run
> time; `bundle-native-deps.sh` stays in the release pipeline as a no-op
> safety net (harmless if a future TDLib/tdlib-rs bump changes this) rather
> than as a required step. Windows static-build linkage is still pending CI
> measurement — see `.github/workflows/release.yml`'s `build` job.

> **Measured in CI (ubuntu-latest x86_64), same build command/pin as above.**
> Two distinct findings, one at link time and one at run time:
>
> - **Link time**: the plain `libc++1`/`libc++abi1` runtime packages this repo
>   already provisions for the *dynamic* build are **not sufficient to link**
>   the static build. First CI attempt failed: `rust-lld: error: unable to
>   find library -lc++` / `-lc++abi`. A statically-linked `tdjson.a` carries no
>   `DT_NEEDED` tags the way a `.so` does, so the final Rust link step must
>   resolve `-lc++`/`-lc++abi` itself — which needs the *unversioned* `.so`
>   symlinks that only `libc++-dev`/`libc++abi-dev` provide, not the versioned
>   `.so.1` files `libc++1`/`libc++abi1` ship. Fixed by installing the `-dev`
>   packages in the release build job.
> - **Run time**: once linked, `ldd target/release/tuigram` shows
>   `libc++.so.1` and `libc++abi.so.1` as dynamic dependencies of the binary
>   itself (`libtdjson` is absent, confirming it *is* statically linked in, as
>   intended). So unlike macOS, **Linux's static build is not fully
>   self-contained w.r.t. libc++** — the runtime contract from the dynamic
>   build (needs `libc++1`/`libc++abi1` provisioned on the host, e.g. via
>   `apt-get install libc++1 libc++abi1`) carries over unchanged to the static
>   build. `.github/workflows/release.yml`'s `smoke-linux` job installs
>   exactly this pair (plus `libssl3`/`zlib1g`, per the table above) on a bare
>   `debian:stable-slim` container and confirms `tuigram --version` runs.
>
> **Practical effect:** the Linux release build job needs `libc++-dev` +
> `libc++abi-dev` (not just the runtime `-1` packages) to link; the
> **artifact** still needs `libc++1`/`libc++abi1` at the end user's runtime,
> same as the dynamic build — Linux release tarball users are not fully
> dependency-free the way macOS's are.

> **Measured in CI (windows-latest x86_64), same build command/pin as above.**
> `dumpbin /DEPENDENTS target/release/tuigram.exe` shows no import matching
> `libssl`/`libcrypto`/`zlib1.dll`/`tdjson`, confirming the expectation from
> the dynamic-build table above: Windows's prebuilt already bundles OpenSSL,
> zlib, and the MSVC runtime, and `static` additionally links tdjson itself
> in. **Practical effect:** the Windows release artifact needs nothing at
> build or run time beyond what the prebuilt already carries — no bundling
> step, matching macOS's self-contained result (for a different reason: macOS
> because its prebuilt's OpenSSL/zlib turned out statically linked too,
> Windows because it always bundled them).

**Summary across all three targets (#167):** macOS and Windows release
artifacts are fully self-contained (no OpenSSL/zlib/tdjson runtime deps);
Linux is the one target where the static build still needs
`libc++1`/`libc++abi1` provisioned on the end user's machine — `docs/releasing.md`
and the README's installing section reflect this asymmetry.

### Provisioning & verification

- **One place per OS, not scattered assumptions:** [`check-native-deps.sh`](../../scripts/check-native-deps.sh)
  (verify/provision, all OSes) and [`bundle-native-deps.sh`](../../scripts/bundle-native-deps.sh)
  (relocate deps into release artifacts, macOS-only work). The check script is
  read-only — it detects a compatible OpenSSL 3 + zlib and prints the exact
  install command for the current OS if anything is missing; CI and devs run the
  same script. On macOS it points at the [`Brewfile`](../../Brewfile) but treats
  `openssl@3` as a *suggested dev convenience*, since release builds bundle it.
- **From-source escape hatch only:** `pkg-config` + `libssl-dev`/`zlib1g-dev`
  (Linux) or `pkg-config` + `openssl@3` (macOS) plus a C++ toolchain are needed
  *only* for the `pkg-config`/`local-tdlib` power-user build, never the default.
- **Historical note (Phase 1):** the original host was Ubuntu 26.04 with OpenSSL
  3 / zlib already present, so `download-tdlib` "just worked" there. That was a
  property of that one host, not a portable guarantee — hence this contract.

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
- **Native deps (OpenSSL 3 / zlib) are a per-target contract, not a host
  assumption** — see [Native dependencies across targets](#native-dependencies-openssl--zlib-across-targets).
  `static` covers `tdjson` only; OpenSSL/zlib stay dynamic and are provisioned
  per OS (`Brewfile` / `scripts/check-native-deps.sh`) and audited in CI.
  `libssl-dev`/`zlib1g-dev`/`pkg-config` remain only for the from-source path.

## Links

- TDLib: https://core.telegram.org/tdlib · Build: https://tdlib.github.io/td/build.html
- `tdlib-rs` (chosen): https://github.com/FedericoBruzzone/tdlib-rs · docs https://docs.rs/tdlib-rs
- `rust-tdlib`: https://github.com/aCLr/rust-tdlib
- TL language: https://core.telegram.org/mtproto/TL
