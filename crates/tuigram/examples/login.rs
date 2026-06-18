//! Thin stdin-driven login harness (#9) — drives a real login end-to-end before
//! the TUI exists.
//!
//! This is a **manual verification tool**, not the product: it is feature-gated
//! (`login-harness`) and off by default, so it is excluded from the product
//! binary and from default CI. Run it against a real account with:
//!
//! ```text
//! cargo run -p tuigram --example login --features login-harness
//! ```
//!
//! It wires the four Phase 2 pieces together — credential resolution
//! ([`tuigram_core::CredentialResolver`]), secure session storage
//! ([`tuigram_core::SessionStorage`]), the async TDLib bridge
//! ([`tuigram_core::Bridge`]), and the auth state machine
//! ([`tuigram_core::Login`]) — and answers each waiting authorization state from
//! stdin until the session reaches `ready`.
//!
//! Secrets are handled the same way the library does: the login code and the 2FA
//! password are read, moved straight into their TDLib request, and never logged
//! or stored. TDLib's own logging is silenced by the auth driver before the first
//! credential-bearing request. (The 2FA password is echoed to the local TTY as
//! typed — acceptable for a developer harness; the future TUI will suppress it.)

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use tokio_stream::StreamExt;
use tuigram_core::enums::Update;
use tuigram_core::types::Error as TdError;
use tuigram_core::{
    ApiCredentials, AuthRequests, AuthState, Bridge, ClientParameters, CredentialError,
    CredentialResolver, Login, Onboarding, SessionStorage, TgClient,
};

type Fallible = Result<(), Box<dyn std::error::Error>>;

#[tokio::main]
async fn main() -> Fallible {
    print_intro();

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
    let mut login = Login::new(&bridge);

    let mut params = Some(build_parameters(&creds, &session));
    let mut last: Option<AuthState> = None;

    // Prime with the current state in case TDLib's startup update fired before we
    // subscribed; every subsequent transition arrives on the update stream.
    let initial = bridge.authorization_state().await.map_err(td)?;
    login.on_update(&initial);
    if dispatch(&login, &mut params, &mut last).await? == Flow::Done {
        return Ok(());
    }

    while let Some(update) = updates.next().await {
        if let Update::AuthorizationState(u) = update {
            login.on_update(&u.authorization_state);
            if dispatch(&login, &mut params, &mut last).await? == Flow::Done {
                return Ok(());
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
) -> Result<Flow, Box<dyn std::error::Error>> {
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
            login.set_parameters(params).await.map_err(td)?;
        }
        AuthState::WaitPhoneNumber => loop {
            let phone = prompt("Phone number (international format, e.g. +15551234567): ")?;
            match login.submit_phone_number(phone).await {
                Ok(()) => break,
                Err(e) => report_retry(&e),
            }
        },
        AuthState::WaitCode => loop {
            let code = prompt("Login code (sent to you by Telegram): ")?;
            match login.submit_code(code).await {
                Ok(()) => break,
                Err(e) => report_retry(&e),
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
                    Err(e) => report_retry(&e),
                }
            }
        }
        AuthState::Ready => {
            println!("\nLogged in — the session is ready.");
            return Ok(Flow::Done);
        }
        AuthState::Closed => {
            println!("\nSession closed (logged out or shutting down).");
            return Ok(Flow::Done);
        }
        AuthState::Unsupported(name) => {
            return Err(format!(
                "login reached an unsupported state ({name}); this harness handles \
                 phone number + login code + 2FA password only"
            )
            .into());
        }
    }
    Ok(Flow::Continue)
}

/// Assemble `setTdlibParameters` input from resolved credentials and storage.
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
fn prompt(label: &str) -> Result<String, Box<dyn std::error::Error>> {
    print!("{label}");
    io::stdout().flush()?;
    let mut line = String::new();
    if io::stdin().read_line(&mut line)? == 0 {
        return Err("input closed (EOF) before login completed".into());
    }
    Ok(line.trim().to_owned())
}

/// Report a rejected entry (bad phone/code/password) and signal a re-prompt. The
/// TDLib message is an error code (e.g. `PHONE_CODE_INVALID`), never the input.
fn report_retry(e: &TdError) {
    eprintln!("  Rejected ({}): {}. Try again.", e.code, e.message);
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
fn td(e: TdError) -> Box<dyn std::error::Error> {
    format!("Telegram error {}: {}", e.code, e.message).into()
}

/// First-run disclosure. Satisfies Telegram ToS 2.2 (state that the app uses the
/// Telegram API / is part of the ecosystem) and 2.4 (no "Telegram" in the name,
/// no official logo).
fn print_intro() {
    println!(
        "tuigram login harness {}\n\
         \n\
         tuigram is an independent, open-source terminal client built on the\n\
         Telegram API (via TDLib). It is part of the Telegram ecosystem but is\n\
         not affiliated with, endorsed by, or operated by Telegram, and carries\n\
         no official Telegram branding or logo.\n\
         \n\
         This harness logs in to a real Telegram account over stdin to verify the\n\
         login flow before the TUI exists. Your credentials, login code, and 2FA\n\
         password are never logged.\n",
        tuigram_core::version()
    );
}

/// The my.telegram.org walkthrough shown only on first run, when no credentials
/// are configured yet. (The "why" the credential module deferred to the harness.)
fn print_registration_help(config_path: &Path) {
    println!(
        "\nYou need your own Telegram API credentials. This client ships none:\n\
         Telegram rate-limits the public sample id, so each user registers their\n\
         own app once.\n\
         \n\
         1. Open https://my.telegram.org and log in with your phone number.\n\
         2. Choose \"API development tools\".\n\
         3. Create an app (any title; platform \"Other\" is fine).\n\
         4. Copy the api_id and api_hash shown, and enter them below.\n\
         \n\
         They will be saved to {} (readable only by you).\n",
        config_path.display()
    );
}
