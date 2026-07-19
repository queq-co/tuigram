//! Pre-TUI bootstrap (Phase 6 #109, #111): stand up an *initialized* `TDLib`
//! [`Bridge`] on the plain terminal, before the login screens take over inside
//! the TUI.
//!
//! The facade pattern is "drive login to `Ready`, *then* [`Client::start`]"
//! (see `docs/architecture.md`). The login itself moved into the TUI's own
//! screens in #111 ([`crate::login::run_login`]); what stays on the **plain**
//! terminal — before [`TerminalGuard`](crate::terminal::TerminalGuard) enters
//! raw mode / the alternate screen — is the non-interactive client setup that is
//! not part of account login: resolving credentials (with first-run onboarding),
//! opening secure session storage, and sending `setTdlibParameters`.
//!
//! `setTdlibParameters` is the first request of every run and the one that
//! surfaces a bad `api_id` as `API_ID_PUBLISHED_FLOOD`; keeping it here means
//! that failure prints its actionable, multi-line guidance on the normal screen
//! rather than as a single line inside a raw-mode TUI. Once it returns, `TDLib`
//! advances to the first login state and the TUI drives the rest.
//!
//! Secrets are handled exactly as the library is: `TDLib`'s own logging is silenced
//! by [`Login::set_parameters`] before the first credential-bearing request
//! (including the `api_id`/`api_hash` in the parameters themselves). Credentials
//! and the session live only where `tuigram-core` puts them (the `600` config and
//! secure session storage) — the binary never writes them itself.

use std::error::Error;
use std::io::{self, Write};
use std::path::PathBuf;

use tuigram_core::types::Error as TdError;
use tuigram_core::{
    ApiCredentials, AuthRequests, AuthState, Bridge, ClientParameters, CredentialError,
    CredentialResolver, Login, Onboarding, SessionStorage, is_api_id_published_flood,
};

/// Any bootstrap failure, surfaced to the user before the TUI starts.
type BootResult<T> = Result<T, Box<dyn Error>>;

/// Resolve credentials, open storage, and initialize `TDLib` — returning the
/// initialized (but not-yet-logged-in) [`Bridge`] `main` holds across the login
/// screens and the run loop.
///
/// Runs entirely on the plain terminal; on any failure it returns an error for
/// `main` to print and exit on, having never entered the TUI. Login (phone, code,
/// 2FA, QR, …) happens afterwards inside the TUI via [`crate::login::run_login`].
pub async fn bootstrap() -> BootResult<Bridge> {
    print_intro();

    // Resolve credentials (env -> config -> first-run onboarding). Onboarding
    // prompts only when neither env nor config supplies them, and the captured
    // values are persisted to the 600 config so this happens once. This is
    // credential setup, not account login, so it legitimately uses stdin here.
    let resolver = CredentialResolver::from_environment()?;
    let onboarding = StdinOnboarding::new(resolver.config_path().to_path_buf());
    let creds = resolver.resolve(&onboarding)?;

    // Owner-only data dir + database encryption key (keyring, file fallback).
    let session = SessionStorage::open()?;

    let bridge = Bridge::new();
    let params = build_parameters(&creds, &session);
    initialize(&bridge, params).await?;
    Ok(bridge)
}

/// Send `setTdlibParameters` so the client is initialized before the TUI takes
/// over login. A fresh process always begins at `WaitTdlibParameters`; a
/// persisted session still needs the parameters set before `TDLib` will report
/// `Ready`. Either way one request advances the state machine to the first login
/// state the TUI then drives.
///
/// `set_parameters` first drops `TDLib`'s log verbosity — ahead of any credential,
/// including the `api_id`/`api_hash` it carries. A bad `api_id` surfaces here (or
/// on the first network request) as `API_ID_PUBLISHED_FLOOD`; it becomes the
/// actionable guidance on the plain terminal rather than a line inside the TUI.
async fn initialize(bridge: &Bridge, params: ClientParameters) -> BootResult<()> {
    let login = Login::new(bridge);
    let state = AuthState::from_tdlib(&bridge.authorization_state().await.map_err(td)?);
    if state == AuthState::WaitTdlibParameters {
        login
            .set_parameters(params)
            .await
            .map_err(|e| flood_or(e, td))?;
    }
    Ok(())
}

/// Cleanly close the `TDLib` instance before the process exits, so its database is
/// flushed and properly closed rather than left mid-write. Without this, the next
/// run fails to open a half-written database ("database disk image is malformed").
///
/// Takes the [`Bridge`] directly so it serves both exit paths: a successful run
/// (via `client.bridge()`) and a login that quit before [`Client::start`] was
/// ever called, where there is no `Client` yet — only the bridge.
pub async fn shutdown(bridge: &Bridge) {
    // The shared whole-operation (#195): close, then wait for `Closed` so the
    // database is fully flushed before the process exits — the same clean-shutdown
    // semantics the REPL harness uses. An already-closing/closed client's rejected
    // `close` is ignored inside it and the wait returns at once.
    bridge.close_and_wait().await;
}

/// Build the `setTdlibParameters` bundle from the resolved credentials and the
/// secure session storage. The `api_hash` and database encryption key move
/// straight into the request and are never logged.
fn build_parameters(creds: &ApiCredentials, session: &SessionStorage) -> ClientParameters {
    ClientParameters {
        api_id: creds.api_id,
        api_hash: creds.api_hash.clone(),
        database_directory: session.database_directory(),
        files_directory: session.files_directory(),
        database_encryption_key: session.encryption_key().expose().to_owned(),
        system_language_code: system_language_code(),
        device_model: "tuigram".to_owned(),
        application_version: tuigram_core::version().to_owned(),
        use_test_dc: false,
    }
}

/// First-run interactive capture of the user's own Telegram API credentials.
struct StdinOnboarding {
    config_path: PathBuf,
}

impl StdinOnboarding {
    fn new(config_path: PathBuf) -> Self {
        Self { config_path }
    }
}

impl Onboarding for StdinOnboarding {
    fn capture(&self) -> Result<ApiCredentials, CredentialError> {
        print_registration_help(&self.config_path);

        let raw_id = read_field("api_id (a number): ")?;
        let api_id = raw_id.parse::<i32>().map_err(|_| {
            CredentialError::Onboarding(format!(
                "api_id must be a positive integer, got {raw_id:?}"
            ))
        })?;
        if api_id <= 0 {
            return Err(CredentialError::Onboarding(
                "api_id must be a positive integer".to_owned(),
            ));
        }

        let api_hash = read_field("api_hash (a hex string): ")?;
        if api_hash.is_empty() {
            return Err(CredentialError::Onboarding(
                "api_hash must not be empty".to_owned(),
            ));
        }

        Ok(ApiCredentials { api_id, api_hash })
    }
}

/// Read one onboarding field, mapping I/O failure to a credential error.
fn read_field(label: &str) -> Result<String, CredentialError> {
    prompt(label).map_err(|e| CredentialError::Onboarding(e.to_string()))
}

/// Print a prompt and read one trimmed line from stdin; EOF is an error.
fn prompt(label: &str) -> BootResult<String> {
    print!("{label}");
    io::stdout().flush()?;
    let mut line = String::new();
    if io::stdin().read_line(&mut line)? == 0 {
        return Err("input closed (EOF)".into());
    }
    Ok(line.trim().to_owned())
}

/// Map a fatal login error: the published-api_id flood becomes its actionable
/// guidance, anything else falls through to `fallback`.
fn flood_or(e: TdError, fallback: impl FnOnce(TdError) -> Box<dyn Error>) -> Box<dyn Error> {
    if is_api_id_published_flood(&e) {
        Box::new(CredentialError::PublishedApiIdFlood)
    } else {
        fallback(e)
    }
}

/// Best-effort IETF language tag from `$LANG` (e.g. `en_US.UTF-8` -> `en`),
/// defaulting to `en` when it is unset or not a two-letter code.
fn system_language_code() -> String {
    std::env::var("LANG")
        .ok()
        .and_then(|l| l.split(['_', '.', '@']).next().map(str::to_owned))
        .filter(|c| c.len() == 2 && c.bytes().all(|b| b.is_ascii_alphabetic()))
        .map_or_else(|| "en".to_owned(), |c| c.to_ascii_lowercase())
}

/// Convert a `TDLib` error into a boxed error without leaking any input.
// Must accept by value: used as a bare fn pointer where `map_err`/`flood_or`
// expect `FnOnce(TdError) -> _`.
#[allow(clippy::needless_pass_by_value)]
fn td(e: TdError) -> Box<dyn Error> {
    format!("Telegram error {}: {}", e.code, e.message).into()
}

/// First-run disclosure. Satisfies Telegram `ToS` 2.2 (state that the app uses the
/// Telegram API / is part of the ecosystem) and 2.4 (no "Telegram" in the name,
/// no official logo).
fn print_intro() {
    println!(
        "tuigram {}\n\
         \n\
         tuigram is an independent, open-source terminal client built on the\n\
         Telegram API (via TDLib). It is part of the Telegram ecosystem but is\n\
         not affiliated with, endorsed by, or operated by Telegram.\n",
        tuigram_core::version()
    );
}

/// Explain how to register an app and where the captured values are stored,
/// shown only on the first run that needs onboarding.
fn print_registration_help(config_path: &std::path::Path) {
    println!(
        "\nFirst run: tuigram needs your own Telegram API credentials.\n\
         Each user supplies their own — a FOSS client must never ship a shared\n\
         api_id (Telegram rate-limits the public sample as API_ID_PUBLISHED_FLOOD).\n\
         \n\
         1. Sign in at https://my.telegram.org and open \"API development tools\".\n\
         2. Create an application; copy its api_id and api_hash.\n\
         \n\
         They will be saved (owner-only) to {} so this is asked once. You can also\n\
         set TUIGRAM_API_ID / TUIGRAM_API_HASH in the environment instead.\n",
        config_path.display()
    );
}
