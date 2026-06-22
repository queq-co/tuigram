# Login flow (Phase 2)

> What the implemented secure login does, end to end. Code lives in
> [`tuigram-core`](../crates/tuigram-core/src); the manual harness that drives it
> is [`crates/tuigram/examples/login.rs`](../crates/tuigram/examples/login.rs).
> Background research: [research/tdlib.md](research/tdlib.md) (auth state machine)
> and [research/app-registration-security.md](research/app-registration-security.md)
> (`api_id` policy + session protection).

Scope is **every** TDLib authorization state. The **phone path** — phone number +
login code + 2FA password, the route a normal user hits — plus **QR login**
(request a code, scan the link on an already signed-in device), **new-user
registration** (accept the terms, submit a name), and **email login** (email
address + emailed code) are all driven to completion. **Premium purchase** is
modeled too, but surfaced as a dead end — completing it needs an in-store
purchase a headless client can't make (see [Out of scope](#out-of-scope)).

## The pieces

Login wires four `tuigram-core` units together, each its own module and seam:

| Piece | Module | Role |
|---|---|---|
| **Credential resolution** | [`credentials`](../crates/tuigram-core/src/credentials.rs) | Get the user's own `api_id`/`api_hash`. |
| **Session storage + key** | [`session`](../crates/tuigram-core/src/session.rs) | Owner-only data dir + DB encryption key. |
| **Async bridge** | [`bridge`](../crates/tuigram-core/src/bridge.rs) | Pump `tdjson`; expose a typed request API + update `Stream`. |
| **Auth state machine** | [`auth`](../crates/tuigram-core/src/auth.rs) | Mirror TDLib's auth states; answer each one. |

The first two produce the inputs to `setTdlibParameters`; the bridge carries the
requests; the state machine sequences them.

## State machine

TDLib *is* the authority on login: it emits `updateAuthorizationState` and waits
for the matching request, looping until `ready`. [`auth::Login`] projects that
stream onto a reduced [`auth::AuthState`] and answers each waiting state through
the [`bridge::TgClient`] seam.

```
WaitTdlibParameters         --setTdlibParameters(api_id, api_hash, dirs, key)-->
WaitPhoneNumber             --setAuthenticationPhoneNumber(phone)-------------->
  └─ or QR:                 --requestQrCodeAuthentication()------------------->
WaitOtherDeviceConfirmation{link}  scan link on a signed-in device (no input)-->
WaitEmailAddress            --setAuthenticationEmailAddress(email)------------->
WaitEmailCode{pattern}      --checkAuthenticationEmailCode(code)-------------->
WaitCode                    --checkAuthenticationCode(code)-------------------->
WaitRegistration{terms}     --registerUser(first_name, last_name)  (new user)-->
WaitPassword{hint}          --checkAuthenticationPassword(password)  (2FA only)->
WaitPremiumPurchase{product}  in-store purchase required — dead end (no answer)
Ready                         logged in; normal updates flow
Closed                        loggingOut / closing / closed — tear down
```

- At `WaitPhoneNumber`, **QR login** is the alternative answer:
  `requestQrCodeAuthentication` moves TDLib to `WaitOtherDeviceConfirmation{link}`,
  whose link is rendered as a QR code and scanned on an already signed-in device.
  No input is taken there — the flow advances on the next update (to `Ready`, or
  `WaitPassword` if 2FA is set).
- **Email login** (`WaitEmailAddress` → `WaitEmailCode{pattern}`) and **new-user
  registration** (`WaitRegistration{terms}`) are answered the same way — submit the
  email/code, or accept the terms and register a name. The states a given account
  hits depend on its setup; only those it reaches are driven.
- **Premium purchase** (`WaitPremiumPurchase{product}`) is modeled but has no answer
  here: it needs an App Store / Play in-store purchase. The flow reports it as a
  dead end (the harness errors out) rather than hanging on a silent unknown.
- `WaitPassword` is **skipped entirely** when the account has no 2FA.
- The projection ([`AuthState::from_tdlib`]) is **total** by *exhaustive* match over
  TDLib's (closed) enum — every state maps to a handled variant, with no catch-all —
  so a state added by a future TDLib version is a compile error here, never a silent
  misclassification.
- The driver does not consume the update stream itself — that stays on the bridge
  so other subsystems can observe auth transitions too. The owning loop feeds each
  update to `Login::on_update` and calls the matching handler.

The whole flow is unit-tested with a spy `TgClient`, with no network and no live
`tdjson`: see `full_login_flow_transitions_and_dispatches` and
`flow_without_2fa_goes_code_straight_to_ready` in [`auth`](../crates/tuigram-core/src/auth.rs).

## Credential resolution

Telegram ties `api_id`/`api_hash` to a developer's own app registration and
rate-limits the published *sample* id (`API_ID_PUBLISHED_FLOOD`), so this client
**ships no shared credential** — each user supplies their own. [`CredentialResolver`]
resolves them in precedence order, first hit wins:

1. **Environment** — `TUIGRAM_API_ID` / `TUIGRAM_API_HASH`. Both or neither; a
   half-set pair is a misconfiguration we surface rather than silently skip.
2. **Config file** — `$XDG_CONFIG_HOME/tuigram/config.toml` (else `~/.config/...`),
   under a `[telegram]` section, written `600`.
3. **First-run onboarding** — capture the two values interactively, explain *why*
   (link to my.telegram.org, 4 steps), and persist them to the `600` config so
   this happens exactly once.

Onboarding is the [`Onboarding`] seam: the prompt copy lives in the harness, while
precedence and on-disk handling stay unit-testable. If TDLib later rejects the id
with `API_ID_PUBLISHED_FLOOD` (someone configured the public sample), the resolver
surfaces an actionable error telling the user to register their own — never a
silent failure. See [research/app-registration-security.md](research/app-registration-security.md)
for why a bundled FOSS credential is not viable.

## Session storage + encryption key

The `api_id`/`api_hash` are low-value; the **TDLib session/database is live
account access**, so it is the asset worth protecting. [`SessionStorage::open`]:

- roots state at `$XDG_DATA_HOME/tuigram` (else `~/.local/share/tuigram`), created
  **`700`**, with `database/` and `files/` subdirs for TDLib;
- enables TDLib's optional **database encryption key** — 32 bytes of OS-CSPRNG
  entropy, hex-encoded ([`EncryptionKey`]), generated once on first use and reused
  thereafter;
- stores that key in the **OS keyring** (macOS Keychain / Windows Credential
  Manager / Linux Secret Service), **falling back to a `600` key file** in the data
  dir where no keyring is reachable (headless Linux, CI, minimal hosts).

A corrupt or wrong-length stored key is surfaced, never silently used. The key is
never logged: `EncryptionKey` redacts itself in `Debug` and is exposed only via
`expose()`, at the point it is moved into `setTdlibParameters`.

## Threat model

**What this protects against — casual disk access.** A stolen laptop, a synced
backup, another local user reading `~/.local/share`. The on-disk database is
encrypted, and the key sits in the OS credential store (or a file only the owner
can read). The config (`600`) and data dir (`700`) keep `api_id`/`api_hash` and
session files off other local accounts.

**What it does *not* protect against — a root-level attacker on the same
machine.** Root can read the keyring, the key file, and this process's memory.
Defending against that needs a hardware token or a passphrase typed every launch,
neither of which this client asks for. This boundary is stated plainly rather than
papered over.

**Secrets are never logged or retained beyond their request:**

- TDLib logs request/response payloads to stderr at its default verbosity — which
  during login would include the phone number, code, and 2FA password. The driver
  drops to [`auth::SECURE_LOG_VERBOSITY`] (errors only) inside `set_parameters`,
  **before the first credential-bearing request**, including the `api_id`/`api_hash`
  in `setTdlibParameters` itself.
- The login code and 2FA password are taken **by value** and moved straight into
  their TDLib request; nothing stores them. The 2FA password is never persisted —
  it is the account password, not ours to keep.
- Rejected entries (bad phone/code/password) are re-prompted by reporting TDLib's
  *error code* (e.g. `PHONE_CODE_INVALID`), never the input the user typed.

(The developer harness echoes the typed 2FA password to the local TTY as a
deliberate, documented exception; the future TUI will suppress it.)

## Out of scope

Every TDLib authorization state is now modeled and mapped — there is no
`AuthState::Unsupported` catch-all anymore. What remains out of scope is narrower:
specific *answers* a headless client can't give, not whole states.

- **Premium purchase** (`waitPremiumPurchase`) is modeled and surfaced, but can't
  be *completed* here: it needs an App Store / Play in-store purchase. The state is
  reported as a dead end rather than hung on.
- At `waitEmailCode`, only the **emailed code** answer is wired; the **Apple ID /
  Google ID** token answers TDLib would also accept there are mobile-only and not
  implemented.

A user who hits the premium dead end logs in on a mobile app first; everything
else completes headlessly.

## Trying it

The login path has no UI yet (that is Phase 5). A feature-gated harness drives it
end to end against a real account over stdin:

```text
cargo run -p tuigram --example login --features login-harness
```

It is off by default — excluded from the product binary and from default CI — and
opens with a ToS-required disclosure that tuigram is an independent client built on
the Telegram API. See [`crates/tuigram/examples/login.rs`](../crates/tuigram/examples/login.rs).

[`auth::Login`]: ../crates/tuigram-core/src/auth.rs
[`auth::AuthState`]: ../crates/tuigram-core/src/auth.rs
[`AuthState::from_tdlib`]: ../crates/tuigram-core/src/auth.rs
[`auth::SECURE_LOG_VERBOSITY`]: ../crates/tuigram-core/src/auth.rs
[`bridge::TgClient`]: ../crates/tuigram-core/src/bridge.rs
[`CredentialResolver`]: ../crates/tuigram-core/src/credentials.rs
[`Onboarding`]: ../crates/tuigram-core/src/credentials.rs
[`SessionStorage::open`]: ../crates/tuigram-core/src/session.rs
[`EncryptionKey`]: ../crates/tuigram-core/src/session.rs
