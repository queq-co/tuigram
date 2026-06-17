# Contributing to tuigram

## Workflow

Work happens on **`develop`** and reaches **`main`** only via Pull Request.
See [docs/branch-model.md](docs/branch-model.md) for the details.

## Setup

```sh
# Rust toolchain (pinned via rust-toolchain.toml)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Build & test the workspace
cargo build
cargo test
cargo fmt --check
cargo clippy --workspace --all-targets
```

TDLib system dependencies and `tdjson` setup are documented in
[docs/research/tdlib.md](docs/research/tdlib.md) (prebuilt by default; from-source
for power users).

## Rules

- Never commit secrets: `api_id`/`api_hash`, tokens, or session data.
- Keep `tuigram-core` free of terminal/UI dependencies so it stays unit-testable.
- `cargo test` must pass before a PR is merged.
