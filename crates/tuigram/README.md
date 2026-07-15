# tuigram

A terminal UI ([Ratatui](https://ratatui.rs)) Telegram client, built on
[TDLib](https://core.telegram.org/tdlib) via [`tuigram-core`](https://crates.io/crates/tuigram-core).

Published as `tuigram-client` — the name `tuigram` was already taken on
crates.io by an unrelated crate. The installed binary is still named
`tuigram`.

```
cargo install tuigram-client --features static
```

The default (dynamic) build isn't reliable for `cargo install` — see the
[repository README](https://github.com/queq-co/tuigram#readme) and
[`docs/releasing.md`](https://github.com/queq-co/tuigram/blob/main/docs/releasing.md)
for why, plus prebuilt binaries and other install channels: macOS users can
`brew install queq-co/tuigram/tuigram` instead, and Linux/Windows users can
grab a tarball/zip from the [Releases page](https://github.com/queq-co/tuigram/releases).

License: MIT
