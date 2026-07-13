# Releasing tuigram

CalVer (`YYYY.M.P`, unpadded — see root `Cargo.toml`'s `[workspace.package]`
comment). This is the checklist; `.github/workflows/release.yml` does the
actual build/package/publish once a matching tag lands.

1. Confirm `develop` is green (CI passing).
2. Bump `[workspace.package] version` in the root `Cargo.toml` to the new
   `YYYY.M.P` (current year, release month, patch within that month — no
   leading zeros; Cargo rejects those as invalid SemVer).
3. Open a PR with just the version bump, merge to `develop`.
4. Open the periodic `develop` → `main` release PR (existing practice — see
   `docs/branch-model.md`), merge.
5. On `main`:
   ```sh
   git tag v<version>   # e.g. v2026.7.1 for Cargo.toml's 2026.7.1
   git push origin v<version>
   ```
   The tag carries the `v`; the package version does not. `release.yml`'s
   `preflight` job asserts they match and fails fast if they've drifted.
6. The tag push triggers `.github/workflows/release.yml`:
   `build` (release, `--features tuigram/static`, per target) → `package`
   (assembles `tuigram-<version>-<target>.tar.gz`/`.zip` with `LICENSE` +
   `README.md`) → `sums` (`SHA256SUMS` over every archive) → the clean-machine
   smoke tests (#169) → a **draft** Release with everything attached → `publish`
   flips it to published + latest, but **only if every smoke test passed**.
7. Post-release verification (human):
   - The release page shows three artifacts + `SHA256SUMS`, and is marked
     "Latest".
   - Download the Linux tarball, `sha256sum -c SHA256SUMS` against it, unpack,
     run `tuigram --version` — confirm it prints the tagged version.
8. **If a smoke test fails**: the Release stays a draft and is never marked
   latest — the artifacts are still there to inspect. Investigate the failing
   job's log on that run, fix forward on `develop`, bump to a new patch version
   and re-tag. Never force-move an existing tag.
9. **Dry run without affecting real users**: push a prerelease-style tag
   matching `v20*` with a `-` suffix, e.g. `v2026.99.0-test1` — `release`
   passes `--prerelease` automatically when the version string contains `-`.
   Delete the tag and the (draft or published) release afterward:
   ```sh
   git push --delete origin v2026.99.0-test1
   gh release delete v2026.99.0-test1 --yes
   ```

## macOS distribution note

Direct macOS binary download is **unsupported** — no code-signing identity,
so an unsigned browser-downloaded binary hits Gatekeeper quarantine. macOS
users should install via `brew` (once #170 lands) or `cargo install`. Linux
users may take the released tarball directly, per the runtime deps
`docs/research/tdlib.md` documents for that target.
