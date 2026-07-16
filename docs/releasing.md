# Releasing tuigram

CalVer (`YYYY.M.P`, unpadded — see root `Cargo.toml`'s `[workspace.package]`
comment). This is the checklist; `.github/workflows/release.yml` does the
actual build/package/publish once a matching tag lands.

1. Confirm `develop` is green (CI passing).
2. Bump `[workspace.package] version` in the root `Cargo.toml` to the new
   `YYYY.M.P` (current year, release month, patch within that month — no
   leading zeros; Cargo rejects those as invalid SemVer). **Also** bump the
   `tuigram-core = { version = "=X.Y.Z", ... }` dependency string in
   `crates/tuigram/Cargo.toml` to match, in the same commit — this does
   **not** update automatically (`version.workspace = true` only equalizes
   the two crates' own package versions, not a hardcoded dependency
   requirement like this one), and a stale one fails `tuigram`'s crates.io
   publish (see "Publishing to crates.io" below).
3. Open a PR with just the version bump, merge to `develop`.
4. Open the periodic `develop` → `main` release PR (existing practice — see
   `docs/branch-model.md`), merge.
5. On `main`:
   ```sh
   git tag v<version>   # e.g. v2026.7.1 for Cargo.toml's 2026.7.1
   git push origin v<version>
   ```
   The tag carries the `v`; the package version does not. `release.yml`'s
   `preflight` job asserts the tag's base version (before any `-suffix`)
   matches `Cargo.toml` and fails fast if they've drifted.
6. The tag push triggers `.github/workflows/release.yml`:
   `build` (release, `--features tuigram-client/static`, per target) → `package`
   (assembles `tuigram-<version>-<target>.tar.gz`/`.zip` with `LICENSE` +
   `README.md`) → `sums` (`SHA256SUMS` over every archive) → the clean-machine
   smoke tests (#169) → a **draft** Release with everything attached → `publish`
   flips it to published + latest, but **only if every smoke test passed**.
7. Post-release verification (human):
   - The release page shows four artifacts + `SHA256SUMS`, and is marked
     "Latest".
   - Download the Linux tarball, `sha256sum -c SHA256SUMS` against it, unpack,
     run `tuigram --version` — confirm it prints `Cargo.toml`'s version (which
     for a real release equals the tag, minus its `v`).
8. **If a smoke test fails**: the Release stays a draft and is never marked
   latest — the artifacts are still there to inspect. Investigate the failing
   job's log on that run, fix forward on `develop`, bump to a new patch version
   and re-tag. Never force-move an existing tag.
9. **Dry run without affecting real users**: push a prerelease-style tag
   matching `v20*` with a `-` suffix appended to the **current**
   `Cargo.toml` version, e.g. `v2026.7.1-test1` if `Cargo.toml` says
   `2026.7.1` — `preflight` only checks the tag's base version (everything
   before the first `-`) against `Cargo.toml`, so the suffix is free-form,
   but the base still has to match what's actually checked out. `release`
   passes `--prerelease` automatically when the tag contains `-`. Delete the
   tag and the (draft or published) release afterward:
   ```sh
   git push --delete origin v2026.7.1-test1
   gh release delete v2026.7.1-test1 --yes
   ```

## Vulnerability response (out-of-band patch releases)

A RustSec advisory (or an OpenSSL/zlib/tdjson CVE — see below) can require a
release outside the normal CalVer cadence. Triggers, in order of how they're
usually noticed:

- **The weekly advisory sweep** (`.github/workflows/deny-weekly.yml`, #176):
  runs `cargo deny check advisories` every Monday against whatever's in
  `Cargo.lock` at the time, independent of pushes. A new high/critical
  advisory affecting a dependency actually compiled into a shipped binary is
  the primary trigger for an out-of-band release.
- **A GitHub Security Advisory / Dependabot alert** on this repo directly.
- **An upstream OpenSSL, zlib, or tdjson advisory** — see the frozen-OpenSSL
  note below; these don't necessarily show up in `cargo deny`'s Rust-crate
  advisory database at all, since the vulnerable code isn't a Cargo
  dependency.

**Frozen-OpenSSL problem:** release artifacts do not link OpenSSL dynamically
against whatever the end user's system provides at run time — they freeze a
specific build of it:

- **macOS** artifacts bundle the build machine's Homebrew `openssl@3` dylibs
  directly into the archive (`scripts/bundle-native-deps.sh`), so an
  OpenSSL patch on the *build* runner's Homebrew is invisible to already-shipped
  users — that dylib only updates on the next build.
- The **Windows** prebuilt and **release-mode static Linux** builds link
  OpenSSL statically into `tdjson`/the binary at compile time (see
  `docs/research/tdlib.md`), so a patched OpenSSL on the *runtime* host has no
  effect either — the vulnerable code is baked into the artifact itself.

Consequence: an OpenSSL (or zlib) advisory serious enough to act on requires a
**rebuild and re-release**, even with zero application code changes — bumping
the patch version and re-running the checklist above (step 2 onward) picks up
whatever OpenSSL/zlib the build runners provide at that later date. There is
no way to patch a shipped archive in place; the fix is always a new tagged
release.

`cargo-auditable` (#177): release builds run through `cargo auditable build`,
which embeds the exact resolved Cargo dependency tree in a linker section of
the binary. Given a shipped `tuigram` binary from any past release, `cargo
audit bin tuigram` (from the [`cargo-audit`] tool, not `cargo-deny`) answers
"is *this specific artifact* affected?" against RustSec's database without
having to reconstruct which `Cargo.lock` it was built from — useful for
triaging a report against an old release still in the wild.

[`cargo-audit`]: https://github.com/rustsec/rustsec/tree/main/cargo-audit

## macOS distribution note

Direct macOS binary download is **unsupported** — no code-signing identity,
so an unsigned browser-downloaded binary hits Gatekeeper quarantine. macOS
users should install via `brew install queq-co/tuigram/tuigram` (#170;
tap: [queq-co/homebrew-tuigram](https://github.com/queq-co/homebrew-tuigram))
or `cargo install`. Linux users may take the released tarball directly, per
the runtime deps `docs/research/tdlib.md` documents for that target.

The tap's `Formula/tuigram.rb` is bumped automatically by this workflow's
`bump-homebrew-tap` job on every real (non-dry-run) release: it opens a PR
against the tap with the new `download-url`/`download-sha256`, then
auto-merges once the tap's own `tests.yml` (`brew audit`/`style`/`install`/
`test`) passes. Dry-run tags (step 9 below) are skipped by this job on
purpose — only a real release should ever touch the tap.

## Publishing to crates.io

The binary crate (`crates/tuigram/`) publishes under the package name
**`tuigram-client`**, not `tuigram` — the name `tuigram` was already
registered on crates.io by an unrelated crate (a TUI sequence diagram
editor, unaffiliated with this project) by the time #171 went to publish
it, so this repo's binary crate had to pick a different registry name.
Only the *package* name changed: the installed executable is still named
`tuigram` (the `[[bin]]` name in `crates/tuigram/Cargo.toml` is unchanged),
and the repository/directory (`crates/tuigram/`) keeps its existing name too.

`tuigram-core` must publish first, `tuigram-client` second —
`tuigram-client`'s manifest depends on it via `tuigram-core = { version =
"=X.Y.Z", path = "../tuigram-core" }`, which crates.io resolves against the
registry (the `path` component only applies to local/workspace builds), so
`tuigram-client`'s publish fails until that exact version is live on the
index.

1. Do this **after** a real GitHub Release tag has been pushed and verified
   (steps 1–7 above), not before — crates.io renders whatever rustdoc exists
   at publish time, and a stale/broken build there is far more visible than
   a GitHub Release artifact problem.
2. `cargo publish --dry-run -p tuigram-core`, then `cargo publish -p tuigram-core`.
3. Wait for it to appear at <https://crates.io/crates/tuigram-core> (usually
   under a minute) — `tuigram-client`'s publish will fail to resolve the
   dependency until it does.
4. `cargo publish --dry-run -p tuigram-client`, then `cargo publish -p tuigram-client`.
5. **This is irrevocable.** crates.io has no delete, only `cargo yank` (hides
   a version from new dependents' resolution; existing lockfiles still work).
   If something's wrong post-publish, yank the bad version and publish a new
   patch — never assume a publish can be undone.

**`cargo install tuigram-client` recommendation**: the default (dynamic,
`download-tdlib`) build is **not** reliable for `cargo install`. tdlib-rs's
build script bakes an rpath pointing at the build-time `OUT_DIR`
(`target/.../build/tdlib-rs-<hash>/out/tdlib/lib`) into the compiled binary,
but `cargo install` discards everything except the final binary from its temp
build directory — so the installed binary's rpath points at a directory that
no longer exists, and it fails to find `libtdjson` at runtime. Always
recommend:
```sh
cargo install tuigram-client --features static
```
which statically links tdjson into the binary instead (see
`docs/research/tdlib.md`'s "Release (static) build — measured" section for
what that build needs per platform). This installs a binary still named
`tuigram` — only the package you pass to `cargo install` is `tuigram-client`.
`cargo binstall tuigram-client` sidesteps this entirely by fetching the
prebuilt release artifact instead of compiling.
