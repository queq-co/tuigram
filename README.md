# tuigram
TUI client for Telegram, written in Rust

## Installing

- **Linux**: download the released `tuigram-<version>-linux-x86_64.tar.gz`
  from the [Releases page](https://github.com/queq-co/tuigram/releases) and
  run the `tuigram` binary directly.
- **macOS**: direct binary download is **not supported** — the release
  artifact ships unsigned, so a browser download hits Gatekeeper quarantine.
  Install via Homebrew or `cargo install tuigram-client --features static`
  instead (the crates.io package is `tuigram-client` — `tuigram` was already
  taken by an unrelated crate — but it installs a binary still named
  `tuigram`).
- **Windows**: download the released `.zip` from the
  [Releases page](https://github.com/queq-co/tuigram/releases) and unpack it.

See [docs/releasing.md](docs/releasing.md) for how releases are built.
