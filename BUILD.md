# Building tuigram

This covers building from source. If you just want to run tuigram, see the
[README](README.md#installing) for prebuilt install options instead.

## Toolchain

The Rust toolchain is pinned via [`rust-toolchain.toml`](rust-toolchain.toml)
(`stable`, with the `rustfmt`/`clippy` components) — `rustup` picks this up
automatically once it's installed:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

## The four `tdjson` build modes

tuigram talks to Telegram through TDLib's `tdjson` C ABI, via the
[`tdlib-rs`](https://github.com/FedericoBruzzone/tdlib-rs) crate. It offers
four Cargo features controlling where `tdjson` comes from — see
[`docs/research/tdlib.md`](docs/research/tdlib.md) for the full research and
measurements behind each:

| Mode | When | Command |
|---|---|---|
| `download-tdlib` | **Dev default.** Pulls a prebuilt `tdjson` from `tdlib-rs`'s GitHub releases at build time — no local TDLib/C++ build needed. | `cargo build` |
| `download-tdlib` + `static` | **Release/distribution.** Statically links `tdjson` into the binary (combine with `download-tdlib`, as the release pipeline does). | `cargo build --release --features tuigram-client/static` |
| `local-tdlib` | Use a TDLib you've built/installed yourself. | set `LOCAL_TDLIB_PATH`, then `cargo build --no-default-features --features tuigram-core/local-tdlib` |
| `pkg-config` | Discover TDLib via `pkg-config` (system package). | `cargo build --no-default-features --features tuigram-core/pkg-config` |

`local-tdlib`/`pkg-config` are power-user, from-source escape hatches — they
require you to have built or installed TDLib's C++ yourself (see
[TDLib's build docs](https://tdlib.github.io/td/build.html)), which needs a
C++17 toolchain and several GB of RAM. Most contributors want the default
`download-tdlib` path.

## Native runtime dependencies (OpenSSL / zlib / libc++)

The prebuilt `tdjson` dynamically links **OpenSSL 3** and **zlib** — TDLib's
own dependencies, not removed by the `static` feature (that only statically
links `tdjson` itself). On Linux it also needs LLVM's **`libc++`**, not the
system `libstdc++`. Check what your machine has and get an exact install
command for what's missing:

```sh
./scripts/check-native-deps.sh
```

This script is read-only (never installs anything, never needs `sudo`) and is
the single source of truth CI uses too. Per OS:

- **macOS**: `brew bundle` (using the repo's [`Brewfile`](Brewfile)) installs
  `openssl@3` — suggested for local dev builds, not required (release builds
  bundle OpenSSL with the app instead, so shipped binaries need no Homebrew).
- **Linux**: `libssl3`, `zlib1g` (usually preinstalled), plus `libc++1` +
  `libc++abi1` (not preinstalled — must be provisioned):
  ```sh
  sudo apt-get install -y libssl3 zlib1g libc++1 libc++abi1
  ```
  Building the `static` release variant yourself additionally needs the `-dev`
  packages (`libc++-dev libc++abi-dev`) to link, since the static build's
  final link step needs the unversioned `.so` symlinks those provide.
- **Windows**: nothing — the prebuilt bundles its native deps.

The `local-tdlib`/`pkg-config` from-source path additionally needs
`pkg-config` + `libssl-dev`/`zlib1g-dev` (Linux) or `pkg-config` + `openssl@3`
(macOS), plus a C++ toolchain to build TDLib itself.

## Building, testing, linting

```sh
cargo build
cargo test
cargo fmt --all --check
cargo clippy --workspace --all-targets
```

Install the repo's git hook so formatting is caught locally instead of in CI:

```sh
git config core.hooksPath .githooks
```

## Running the headless login/REPL harness

`crates/tuigram/examples/repl.rs` is a manual, interactive harness that logs
into a real Telegram account and drops into a stdin REPL driving the client
surface (list chats, open a chat, send/reply/edit/delete/read). It needs a
real account and a TTY, so it's excluded from default builds and CI:

```sh
cargo run -p tuigram-client --example repl --features login-harness
```

## See also

- [`docs/research/tdlib.md`](docs/research/tdlib.md) — full TDLib integration
  research, native-dependency measurements per target, and the release
  (`static`) build's per-OS linkage findings.
- [`docs/releasing.md`](docs/releasing.md) — how tagged releases are built,
  packaged, and published.
- [`CONTRIBUTING.md`](CONTRIBUTING.md) — workflow and contribution rules.
