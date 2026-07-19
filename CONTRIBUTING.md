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

# Enforce formatting locally, so CI's Format step never surprises a PR
git config core.hooksPath .githooks
```

### Coverage

CI's `coverage` job (see `.github/workflows/ci.yml`) runs
[`cargo-llvm-cov`](https://github.com/taiki-e/cargo-llvm-cov) over the
workspace on every push/PR and uploads the HTML report + an `lcov.info` as
build artifacts — informational only, not a merge gate (a percentage floor
would reward padding tests over fixing real gaps; see #181). To run the same
report locally:

```sh
rustup component add llvm-tools-preview
cargo install cargo-llvm-cov --locked
cargo llvm-cov --workspace --html   # report at target/llvm-cov/html/index.html
```

TDLib system dependencies and `tdjson` setup are documented in
[docs/research/tdlib.md](docs/research/tdlib.md) (prebuilt by default; from-source
for power users).

## Rules

- Never commit secrets: `api_id`/`api_hash`, tokens, or session data.
- Keep `tuigram-core` free of terminal/UI dependencies so it stays unit-testable.
- `cargo test` must pass before a PR is merged.
- `cargo fmt --all --check` must pass before a PR is merged — install the
  `.githooks/pre-commit` hook (see Setup) so this is caught locally, not in CI.
