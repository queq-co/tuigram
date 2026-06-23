//! Pre-TUI bootstrap (Phase 6 #109): stand up a live, authenticated
//! [`Client`] before the terminal UI takes the screen.
//!
//! The facade pattern is "drive login to `Ready`, *then* [`Client::start`]"
//! (see `docs/architecture.md`). Login is interactive, so this runs on the
//! **plain** terminal — before [`TerminalGuard`](crate::terminal::TerminalGuard)
//! puts it into raw mode / the alternate screen — resolving credentials,
//! opening secure session storage, driving the auth state machine over stdin,
//! and handing the authenticated bridge to the facade.
//!
//! This stdin login is **temporary scaffolding**: Phase 6 #111 moves login into
//! the TUI's own login screens and deletes the prompting here. Until then it is
//! the smallest thing that gets the rest of Phase 6 a real `Client` to build
//! against. It mirrors the headless REPL harness's `authenticate()` (kept
//! separate because an example cannot import a binary's modules).
//!
//! Secrets are handled exactly as the library is: the login code and 2FA
//! password are read, moved straight into their TDLib request, and never logged
//! or stored; TDLib's own logging is silenced by [`Login`] before the first
//! credential-bearing request. Credentials and the session live only where
//! `tuigram-core` puts them (the `600` config and secure session storage) — the
//! binary never writes them itself.

use std::error::Error;
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Duration;

use tokio_stream::StreamExt;
use tuigram_core::enums::{AuthorizationState, Update};
use tuigram_core::types::Error as TdError;
use tuigram_core::{
    ApiCredentials, AuthRequests, AuthState, Bridge, Client, ClientParameters, CredentialError,
    CredentialResolver, Login, Onboarding, SessionStorage, TgClient, UpdateStream,
    is_api_id_published_flood,
};

/// Any bootstrap failure, surfaced to the user before the TUI starts.
type BootResult<T> = Result<T, Box<dyn Error>>;

/// Resolve credentials, open storage, drive login to `Ready`, and start the
/// update router — returning the one live [`Client`] handle `main` holds.
///
/// Runs entirely on the plain terminal; on any failure it returns an error for
/// `main` to print and exit on, having never entered the TUI.
pub async fn bootstrap() -> BootResult<Client> {
    print_intro();
    let bridge = authenticate().await?;
    println!("\nLogged in. Starting tuigram…\n");
    Ok(Client::start(bridge))
}

/// Resolve credentials, open storage, and drive login to `Ready`, returning the
/// authenticated bridge for the facade to take over.
async fn authenticate() -> BootResult<Bridge> {
    // Resolve credentials (env -> config -> first-run onboarding). Onboarding
    // prompts only when neither env nor config supplies them, and the captured
    // values are persisted to the 600 config so this happens once.
    let resolver = CredentialResolver::from_environment()?;
    let onboarding = StdinOnboarding::new(resolver.config_path().to_path_buf());
    let creds = resolver.resolve(&onboarding)?;

    // Owner-only data dir + database encryption key (keyring, file fallback).
    let session = SessionStorage::open()?;

    let bridge = Bridge::new();
    // Subscribe before driving so transitions emitted during login are captured.
    let mut updates = bridge.updates();
    let params = build_parameters(&creds, &session);

    // `Login` borrows the bridge for the duration of `drive_login`; once that
    // returns the borrow is released and the owned bridge can move to the facade.
    match drive_login(&bridge, &mut updates, params).await? {
        AuthState::Ready => Ok(bridge),
        AuthState::Closed => Err("session closed before it became ready".into()),
        other => Err(format!("login ended in a non-ready state: {other:?}").into()),
    }
}

/// Answer each waiting authorization state from stdin until login reaches a
/// terminal state, which is returned.
async fn drive_login(
    bridge: &Bridge,
    updates: &mut UpdateStream,
    params: ClientParameters,
) -> BootResult<AuthState> {
    let mut login = Login::new(bridge);
    let mut params = Some(params);
    let mut last: Option<AuthState> = None;

    // Prime with the current state in case TDLib's startup update fired before we
    // subscribed; every subsequent transition arrives on the update stream.
    let initial = bridge.authorization_state().await.map_err(td)?;
    login.on_update(&initial);
    if dispatch(&login, &mut params, &mut last).await? == Flow::Done {
        return Ok(login.state().clone());
    }

    while let Some(update) = updates.next().await {
        if let Update::AuthorizationState(u) = update {
            login.on_update(&u.authorization_state);
            if dispatch(&login, &mut params, &mut last).await? == Flow::Done {
                return Ok(login.state().clone());
            }
        }
    }

    Err("TDLib update stream ended before login completed".into())
}

/// Whether the login flow is still in progress or has reached a terminal state.
#[derive(PartialEq, Eq)]
enum Flow {
    Continue,
    Done,
}

/// Answer the current [`AuthState`], prompting the user as needed.
///
/// Skips a state identical to the one just handled — TDLib re-emits the startup
/// `WaitTdlibParameters` after we prime from a direct query, and a wrong entry is
/// re-prompted in-handler rather than via a fresh update, so the only same-state
/// repeats reaching here are duplicates safe to ignore.
async fn dispatch(
    login: &Login<'_, Bridge>,
    params: &mut Option<ClientParameters>,
    last: &mut Option<AuthState>,
) -> BootResult<Flow> {
    let state = login.state().clone();
    if last.as_ref() == Some(&state) {
        return Ok(Flow::Continue);
    }
    *last = Some(state.clone());

    match state {
        AuthState::WaitTdlibParameters => {
            let params = params
                .take()
                .ok_or("TDLib requested setTdlibParameters more than once")?;
            // A bad api_id surfaces here (or on the first network request) as
            // API_ID_PUBLISHED_FLOOD; turn it into the actionable guidance.
            login
                .set_parameters(params)
                .await
                .map_err(|e| flood_or(e, td))?;
        }
        AuthState::WaitPhoneNumber => loop {
            let phone = prompt("Phone number (international format, e.g. +15551234567): ")?;
            match login.submit_phone_number(phone).await {
                Ok(()) => break,
                Err(e) => retry_or_flood(&e)?,
            }
        },
        AuthState::WaitCode => loop {
            let code = prompt("Login code (sent to you by Telegram): ")?;
            match login.submit_code(code).await {
                Ok(()) => break,
                Err(e) => retry_or_flood(&e)?,
            }
        },
        AuthState::WaitPassword { hint } => {
            if !hint.is_empty() {
                println!("  Two-step verification hint: {hint}");
            }
            loop {
                let password = prompt("Two-step verification password (input is visible): ")?;
                match login.submit_password(password).await {
                    Ok(()) => break,
                    Err(e) => retry_or_flood(&e)?,
                }
            }
        }
        AuthState::WaitOtherDeviceConfirmation { link } => {
            // QR login: nothing to read here — TDLib advances on its own once the
            // link is scanned on an already signed-in device. Show it and wait.
            println!("\nScan this link on a signed-in Telegram device to confirm login:");
            println!("  {link}");
            println!("Waiting for confirmation…");
        }
        AuthState::WaitEmailAddress => loop {
            let email = prompt("Email address for login: ")?;
            match login.submit_email_address(email).await {
                Ok(()) => break,
                Err(e) => retry_or_flood(&e)?,
            }
        },
        AuthState::WaitEmailCode { email_pattern } => {
            println!("  A login code was sent to {email_pattern}.");
            loop {
                let code = prompt("Email login code: ")?;
                match login.submit_email_code(code).await {
                    Ok(()) => break,
                    Err(e) => retry_or_flood(&e)?,
                }
            }
        }
        AuthState::WaitRegistration { terms_of_service } => {
            // Unregistered number: create the account. Show the terms first so
            // accepting (by registering) is informed.
            if !terms_of_service.is_empty() {
                println!("\nTerms of service:\n{terms_of_service}\n");
            }
            println!("This phone number isn't registered yet — create the account.");
            loop {
                let first = prompt("First name: ")?;
                let last = prompt("Last name (optional): ")?;
                match login.register(first, last).await {
                    Ok(()) => break,
                    Err(e) => retry_or_flood(&e)?,
                }
            }
        }
        AuthState::WaitPremiumPurchase { store_product_id } => {
            // No headless answer exists: completing this needs an App Store / Play
            // in-store purchase. Report the dead end rather than hang.
            return Err(format!(
                "login requires buying Telegram Premium (store product \
                 {store_product_id}) as an in-store purchase, which this client \
                 can't perform — log in on a mobile app first"
            )
            .into());
        }
        AuthState::Ready => return Ok(Flow::Done),
        AuthState::Closed => {
            println!("\nSession closed (logged out or shutting down).");
            return Ok(Flow::Done);
        }
    }
    Ok(Flow::Continue)
}

/// Cleanly close the TDLib instance before the process exits, so its database is
/// flushed and properly closed rather than left mid-write. Without this, the next
/// run fails to open a half-written database ("database disk image is malformed").
pub async fn shutdown(client: &Client) {
    // Ignore the result: an already-closing/closed client rejects `close`, which
    // is exactly the state we want.
    let _ = client.bridge().close().await;
    wait_until_closed(client.bridge()).await;
}

/// Wait for TDLib to reach `Closed` — the signal that `close` has finished
/// flushing and closing the local database. Bounded (~5s) so a stuck teardown
/// cannot hang exit; a query that errors (the client is already gone) counts as
/// closed.
async fn wait_until_closed(bridge: &Bridge) {
    for _ in 0..50 {
        match bridge.authorization_state().await {
            Ok(AuthorizationState::Closed) | Err(_) => return,
            Ok(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
}

/// Build the `setTdlibParameters` bundle from the resolved credentials and the
/// secure session storage. The api_hash and database encryption key move
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

/// Report a rejected entry (bad phone/code/password) and signal a re-prompt,
/// unless the failure is the published-api_id flood — which no retry can fix, so
/// it is surfaced as a fatal, actionable error instead. The TDLib message is an
/// error code (e.g. `PHONE_CODE_INVALID`), never the input.
fn retry_or_flood(e: &TdError) -> BootResult<()> {
    if is_api_id_published_flood(e) {
        return Err(Box::new(CredentialError::PublishedApiIdFlood));
    }
    eprintln!("  Rejected ({}): {}. Try again.", e.code, e.message);
    Ok(())
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
        .map(|c| c.to_ascii_lowercase())
        .unwrap_or_else(|| "en".to_owned())
}

/// Convert a TDLib error into a boxed error without leaking any input.
fn td(e: TdError) -> Box<dyn Error> {
    format!("Telegram error {}: {}", e.code, e.message).into()
}

/// First-run disclosure. Satisfies Telegram ToS 2.2 (state that the app uses the
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
