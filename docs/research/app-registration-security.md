# Research: distributable & secure app registration

> **Phase 1 — findings + decision.** The central distribution/security question.
> Researched 2026-06-17 against Telegram's current API ToS.

## The problem

Telegram requires an `api_id` + `api_hash` (from https://my.telegram.org) for
*every* login. An open-source, distributable client must get those credentials
into users' hands somehow — but Telegram's terms and rate-limiting constrain how.
This decision shapes the entire Phase 2 login flow.

## What Telegram's terms actually say (the hard constraints)

From the official **API Terms of Service** and the **obtaining api_id** page
(both read 2026-06-17):

1. **ToS 2.1 — "You must obtain your own `api_id` for your application."** This is
   the legal crux. Credentials are tied to the developer/app, not freely shared.
2. **Each phone number maps to exactly one `api_id`.** So we cannot mint
   per-user ids on the fly; a user either reuses one or registers at my.telegram.org.
3. **The published "sample" `api_id` is server-limited** → using it in a released
   app yields `API_ID_PUBLISHED_FLOOD` for users. Telegram is explicit: *"It is
   necessary that you obtain your own API id before you publish your app."*
4. **Accounts using unofficial clients are auto-placed "under observation."**
   Abuse (flooding/spam/fake counts) = permanent ban. So our client must behave
   like a well-mannered citizen.
5. **Transparency (2.2–2.4):** users must be told the app uses the Telegram API
   and is part of the ecosystem; the app title must **not** contain "Telegram"
   (unless prefixed "Unofficial"); must **not** use the official Telegram logo.
6. **No feature interference (1.4):** no ghost mode, no suppressing read/typing/
   online statuses, no blocking self-destruct. Standard-client behavior required.
7. **No AI scraping (1.5):** data from Telegram may not be used to train/fine-tune
   ML models.

The decisive consequence: **a single project-owned `api_id` committed into a
public repo and shared by every user is effectively the "published api_id"
case.** It will trip `API_ID_PUBLISHED_FLOOD` at any real scale and sits awkwardly
against 2.1. Bundling-a-shared-secret is therefore *not* a viable default for a
FOSS client, regardless of obfuscation — obfuscation in a distributed binary is
not secrecy (anyone can extract it), and extraction isn't even the failure mode;
*shared usage hitting the flood limit* is.

## How the options actually stack up

- **User-supplied credentials (each user registers their own).** ToS-clean
  (satisfies 2.1 per user), no shared-rate-limit blast radius, no secret to leak.
  Cost: one-time onboarding friction (visit my.telegram.org, paste two values).
  This is what serious third-party clients (e.g. the Telethon/Pyrogram ecosystem,
  CLI clients) tell users to do.
- **Bundled obfuscated project credential.** Best first-run UX, but: extractable,
  shared across all users → `API_ID_PUBLISHED_FLOOD`, and weakest ToS footing.
  Telegram tolerates baked-in ids for *official* whitelisted apps (Telegram
  Desktop ships one); a third-party FOSS client does not get that whitelist.
- **Hybrid.** Some combination — the question is *which* combination is honest.

The naive "bundle a default + allow override" hybrid still ships a shared secret
to everyone and inherits the flood problem. So we refine it.

## Recommendation / decision

**User-supplied credentials as the required default, with a clean onboarding
flow and an override mechanism — and a clearly-labelled, opt-in build-time
injection path for maintainers' official binaries. The public source tree never
contains credentials.**

Concretely:

1. **Source repo: zero credentials, ever.** No `api_id`/`api_hash` in git, in any
   form, obfuscated or not. CI builds from source contain none.
2. **Runtime resolution order** (first hit wins):
   1. `TUIGRAM_API_ID` / `TUIGRAM_API_HASH` environment variables;
   2. config file `~/.config/tuigram/config.toml` (perms `600`);
   3. **first-run interactive onboarding**: explain *why* (link to
      my.telegram.org, 4 steps), capture the two values, write them to the
      config with `600`. One time only.
3. **Optional maintainer build** (`--features bundled-credentials` or a build env
   var read in `build.rs`): an *official* release binary MAY have a maintainer's
   own registered `api_id` injected **at build time from a CI secret** — never
   committed. This is opt-in, documented as "official builds only," and monitored
   for flood errors; if it ever trips `API_ID_PUBLISHED_FLOOD` we fall back to
   prompting the user. Source builds and `cargo install` get the user-supplied
   path by default. This is the *honest* hybrid: convenience for official
   binaries without a secret in the repo and without forcing the shared-credential
   risk on every downstream build.
4. **Detect and explain `API_ID_PUBLISHED_FLOOD`** at runtime: if it occurs,
   surface a clear message telling the user to register their own credentials,
   rather than failing opaquely. (Verbose, actionable errors — matches how we
   want failures reported.)

### Protecting the real sensitive asset: the user's session

The `api_id`/`api_hash` are low-value compared to the **TDLib session/database**,
which is live account access. So:

- TDLib database/session dir under `~/.local/share/tuigram/` (or
  `$XDG_DATA_HOME`), created **`700`**, files **`600`**.
- TDLib already encrypts its local DB; we **enable the optional database
  encryption key** and store/derive it locally (e.g. OS keyring where available,
  otherwise a key file `600`). Document the threat model plainly: this protects
  against casual disk access, **not** against a root-level attacker on the same
  machine.
- Never log credentials, codes, or the encryption key. Never commit session data
  (already covered by `.gitignore`).
- Honor 2FA (`waitPassword`) — never store the 2FA password; pass it straight to
  TDLib.

### ToS compliance checklist (carry into Phase 2/4)

Phase 2 closed the items expressible in the login path; the rest are behavioral
guarantees of the **TUI** and carry into **Phase 4**, where the surfaces they
constrain (rendering, presence, the app's branding) first exist.

- [x] **(2.2)** First-run intro states the app uses the **Telegram API** / is part
      of the ecosystem. *Done — `print_intro` in
      [`examples/login.rs`](../../crates/tuigram/examples/login.rs); the future
      TUI carries the same disclosure (Phase 4).*
- [x] **(2.4)** App name has no "Telegram" (we're "tuigram" — clear). *Name done.*
      → **Phase 4:** no official Telegram logo in any TUI branding/splash.
- [ ] **(1.4) — Phase 4.** No ghost mode / no tampering with
      read·typing·online·self-destruct: the TUI must send read receipts, typing,
      and online status like a standard client and honor self-destruct.
- [ ] **(1.5) — ongoing policy.** No AI scraping of Telegram data (no using
      fetched data to train/fine-tune models).
- [ ] **(abuse radar) — ongoing.** Behave well: no flooding / no
      automation-as-user without consent. Relevant once the client can send
      (Phase 3+) and act on the user's behalf (Phase 4+).

## Why this over the memo's lean ("bundled default + user override")

The Phase 0 lean was a reasonable starting hypothesis, but the ToS + flood-error
research shows a *shared bundled credential in a public FOSS repo* is the one
thing Telegram explicitly designs against (`API_ID_PUBLISHED_FLOOD`, 2.1). The
recommendation keeps the spirit of "hybrid" (override + optional convenience) but
moves the bundled path to **opt-in, build-time, official-binary-only, secret-not-
in-git** — which is both ToS-defensible and operationally robust.

## Links

- Obtaining api_id: https://core.telegram.org/api/obtaining_api_id
- API Terms of Service: https://core.telegram.org/api/terms
- TDLib: https://core.telegram.org/tdlib
- Security guidelines: https://core.telegram.org/mtproto/security_guidelines
