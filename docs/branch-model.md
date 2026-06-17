# Branch model & workflow

A deliberately small model for a solo/personal project.

## Branches

- **`main`** — stable, protected. No direct pushes; changes land only via PR.
- **`develop`** — integration branch. Day-to-day work happens here.

There is intentionally **no per-feature branch**: we keep a single `develop`
line and promote to `main` through Pull Requests.

## Flow

1. Commit work to `develop`.
2. Open a PR `develop → main`.
3. Review, then merge (squash recommended to keep `main` history clean).
4. `main` stays releasable at all times.

## Conventions

- **Commits**: imperative mood, scoped where useful (e.g. `core: add auth state`).
- **PRs**: describe what + why; link the GitHub Milestone/Issue being advanced.
- **No secrets in git**: `api_id`/`api_hash`, tokens, and session data never get
  committed. Local tokens live outside the repo (e.g. `~/.config/tuigram/`).

## Tests

`cargo test` (workspace) must pass before a PR is merged. Core logic in
`tuigram-core` is unit-tested without a terminal.
