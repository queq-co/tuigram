//! Headless REPL harness (#9, #22) — drives the Phase 3 client end-to-end over
//! stdin against a real account, before the TUI (Phase 4) exists.
//!
//! This is a **manual verification tool**, not the product: it is feature-gated
//! (`login-harness`) and off by default, so it is excluded from the product
//! binary and from default CI. Run it against a real account with:
//!
//! ```text
//! cargo run -p tuigram --example repl --features login-harness
//! ```
//!
//! It first wires the four Phase 2 pieces together — credential resolution
//! ([`tuigram_core::CredentialResolver`]), secure session storage
//! ([`tuigram_core::SessionStorage`]), the async TDLib bridge
//! ([`tuigram_core::Bridge`]), and the auth state machine
//! ([`tuigram_core::Login`]) — to log in, then hands the authenticated bridge to
//! the [`tuigram_core::Client`] facade and drops into a stdin REPL. The REPL
//! exercises the Phase 3 surface: list chats, open a chat (load + view history),
//! send, reply, edit, delete, mark read, and log out. Reads come from the
//! facade's folded snapshot (kept current by its single update router); writes go
//! over the bridge's per-domain request traits.
//!
//! `logout` invalidates the account session and wipes TDLib's local database, so
//! the next run starts at a fresh login rather than resuming the persisted
//! session — the inverse of the login the harness opens with.
//!
//! Secrets are handled the same way the library does: the login code and the 2FA
//! password are read, moved straight into their TDLib request, and never logged
//! or stored. TDLib's own logging is silenced by the auth driver before the first
//! credential-bearing request. (The 2FA password is echoed to the local TTY as
//! typed — acceptable for a developer harness; the future TUI will suppress it.)
//! The REPL never logs message content on its own: it prints a chat's messages
//! only when the operator explicitly asks (`open` / `history`), and never echoes
//! the unsolicited live stream.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio_stream::StreamExt;
use tuigram_core::enums::Update;
use tuigram_core::types::Error as TdError;
use tuigram_core::{
    ApiCredentials, AuthRequests, AuthState, Bridge, Client, ClientParameters, CredentialError,
    CredentialResolver, FormattedText, Login, Message, MessageContent, MessageRequests, NEWEST,
    Onboarding, SendState, Sender, SessionStorage, TgClient, UpdateStream, load_main_list,
};

type Fallible = Result<(), Box<dyn std::error::Error>>;

/// How many of a chat's most recent messages a single `open` pulls.
const HISTORY_PAGE: i32 = 50;
/// How many chats to ask the Main list for on startup.
const CHATS_PAGE: i32 = 100;
/// A brief pause after an async load so the router has folded the resulting
/// updates before the REPL reads the snapshot back. The folded state is
/// eventually consistent regardless; this only makes the first read look settled.
const SETTLE: Duration = Duration::from_millis(300);

#[tokio::main]
async fn main() -> Fallible {
    print_intro();
    let bridge = authenticate().await?;
    println!("\nLogged in. Entering the headless REPL — type `help` for commands.\n");
    let client = Client::start(bridge);
    run_repl(&client).await
}

// ----------------------------------------------------------------------------
// Login (Phase 2)
// ----------------------------------------------------------------------------

/// Resolve credentials, open storage, and drive login to `Ready`, returning the
/// authenticated bridge for the facade to take over.
async fn authenticate() -> Result<Bridge, Box<dyn std::error::Error>> {
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
) -> Result<AuthState, Box<dyn std::error::Error>> {
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
        AuthState::Ready => return Ok(Flow::Done),
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

// ----------------------------------------------------------------------------
// REPL (Phase 3)
// ----------------------------------------------------------------------------

/// Read commands from stdin and drive the Phase 3 client until EOF or `quit`.
async fn run_repl(client: &Client) -> Fallible {
    // Populate the Main list once; the chats arrive as updates the router folds,
    // so the first `chats` may run before they settle — re-run it if so.
    if let Err(e) = load_main_list(client.bridge(), CHATS_PAGE).await {
        eprintln!("Could not load the chat list: {} {}", e.code, e.message);
    }
    tokio::time::sleep(SETTLE).await;
    print_help();

    loop {
        // EOF (Ctrl-D) ends the REPL cleanly, the same exit a `quit` gives.
        let line = match prompt("\ntuigram> ") {
            Ok(line) => line,
            Err(_) => {
                println!("Bye.");
                return Ok(());
            }
        };
        if line.is_empty() {
            continue;
        }

        let (cmd, rest) = split_first(&line);
        match cmd {
            "help" | "?" => print_help(),
            "quit" | "exit" => {
                println!("Bye.");
                return Ok(());
            }
            "chats" => list_chats(client),
            "open" => match parse_chat(rest) {
                Ok(chat_id) => open_chat(client, chat_id).await,
                Err(e) => println!("{e}"),
            },
            "history" => match parse_chat(rest) {
                Ok(chat_id) => show_history(client, chat_id),
                Err(e) => println!("{e}"),
            },
            "send" => match parse_chat_and_text(rest) {
                Ok((chat_id, text)) => send_text(client, chat_id, None, text).await,
                Err(e) => println!("{e}"),
            },
            "reply" => match parse_chat_msg_and_text(rest) {
                Ok((chat_id, message_id, text)) => {
                    send_text(client, chat_id, Some(message_id), text).await;
                }
                Err(e) => println!("{e}"),
            },
            "edit" => match parse_chat_msg_and_text(rest) {
                Ok((chat_id, message_id, text)) => {
                    edit_text(client, chat_id, message_id, text).await;
                }
                Err(e) => println!("{e}"),
            },
            "delete" => match parse_delete(rest) {
                Ok((chat_id, message_id, revoke)) => {
                    delete_message(client, chat_id, message_id, revoke).await;
                }
                Err(e) => println!("{e}"),
            },
            "read" => match parse_chat(rest) {
                Ok(chat_id) => mark_read(client, chat_id).await,
                Err(e) => println!("{e}"),
            },
            "logout" => {
                if logout(client).await == Flow::Done {
                    return Ok(());
                }
            }
            other => println!("Unknown command: {other:?}. Type `help`."),
        }
    }
}

/// Print the Main chat list from the folded snapshot: id, unread count, title.
fn list_chats(client: &Client) {
    let rows = client.read(|state| {
        state
            .chats()
            .main_list()
            .iter()
            .map(|c| format!("  {:>14}  unread {:<5} {}", c.id, c.unread_count, c.title))
            .collect::<Vec<_>>()
    });
    if rows.is_empty() {
        println!("(no chats loaded yet — they fold in asynchronously; try `chats` again)");
    } else {
        println!("Chats (most recent first):");
        for row in rows {
            println!("{row}");
        }
    }
}

/// Load a chat's most recent page of history, mark those messages read, then
/// print them.
async fn open_chat(client: &Client, chat_id: i64) {
    match client
        .bridge()
        .get_chat_history(chat_id, NEWEST, HISTORY_PAGE)
        .await
    {
        Ok(page) => client.merge_history(page),
        Err(e) => {
            println!("Could not load history: {} {}", e.code, e.message);
            return;
        }
    }
    tokio::time::sleep(SETTLE).await;
    mark_read(client, chat_id).await;
    show_history(client, chat_id);
}

/// Print a chat's known messages from the folded snapshot, oldest first.
fn show_history(client: &Client, chat_id: i64) {
    let lines = client.read(|state| {
        state
            .messages()
            .history(chat_id)
            .iter()
            .map(|m| format_message(m))
            .collect::<Vec<_>>()
    });
    if lines.is_empty() {
        println!("(no messages known for {chat_id} — try `open {chat_id}` first)");
    } else {
        for line in lines {
            println!("{line}");
        }
    }
}

/// Send `text` to a chat, optionally as a reply. The optimistic message folds in
/// via the router; `history` shows it (and its send state) reconcile.
async fn send_text(client: &Client, chat_id: i64, reply_to: Option<i64>, text: String) {
    let content = FormattedText {
        text,
        entities: vec![],
    };
    match client.bridge().send_text(chat_id, reply_to, content).await {
        Ok(msg) => println!(
            "Sent (optimistic id {}). `history {chat_id}` to follow it.",
            msg.id
        ),
        Err(e) => println!("Send failed: {} {}", e.code, e.message),
    }
}

/// Replace the text of one of our own messages.
async fn edit_text(client: &Client, chat_id: i64, message_id: i64, text: String) {
    let content = FormattedText {
        text,
        entities: vec![],
    };
    match client
        .bridge()
        .edit_text(chat_id, message_id, content)
        .await
    {
        Ok(_) => println!("Edited {message_id}."),
        Err(e) => println!("Edit failed: {} {}", e.code, e.message),
    }
}

/// Delete a message — for everyone with `revoke`, otherwise only for us.
async fn delete_message(client: &Client, chat_id: i64, message_id: i64, revoke: bool) {
    match client
        .bridge()
        .delete(chat_id, vec![message_id], revoke)
        .await
    {
        Ok(()) => {
            let scope = if revoke { "everyone" } else { "you" };
            println!("Deleted {message_id} for {scope}.");
        }
        Err(e) => println!("Delete failed: {} {}", e.code, e.message),
    }
}

/// Mark every known message in a chat read. Advisory — the unread count settles
/// asynchronously via `updateChatReadInbox`.
async fn mark_read(client: &Client, chat_id: i64) {
    let ids = client.read(|state| {
        state
            .messages()
            .history(chat_id)
            .iter()
            .map(|m| m.id)
            .collect::<Vec<_>>()
    });
    if ids.is_empty() {
        return;
    }
    if let Err(e) = client.bridge().view_messages(chat_id, ids).await {
        println!("Mark-read failed: {} {}", e.code, e.message);
    }
}

/// Log out: invalidate the session, wait for TDLib to clear it, then end the
/// REPL so the next run starts at a fresh login. A failed request stays in the
/// REPL ([`Flow::Continue`]); a successful one exits ([`Flow::Done`]).
async fn logout(client: &Client) -> Flow {
    println!("Logging out…");
    if let Err(e) = client.bridge().log_out().await {
        println!("Logout failed: {} {}", e.code, e.message);
        return Flow::Continue;
    }
    wait_until_logged_out(client.bridge()).await;
    println!("Logged out. The local session has been cleared — re-run to sign in again.");
    Flow::Done
}

/// After `log_out`, wait for TDLib to leave `Ready`. `logOut` clears the session
/// asynchronously (`Ready` -> `LoggingOut` -> `WaitPhoneNumber`), and we want
/// that to have taken effect before the process exits. Bounded (~5s) so a stuck
/// logout cannot hang the harness.
async fn wait_until_logged_out(bridge: &Bridge) {
    for _ in 0..50 {
        match bridge.authorization_state().await {
            Ok(state) if AuthState::from_tdlib(&state) == AuthState::Ready => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            _ => return,
        }
    }
}

/// Render one message for display: id, sender, send state, and its body. A
/// non-text message shows its TDLib content type in angle brackets rather than
/// any payload.
fn format_message(m: &Message) -> String {
    let who = if m.is_outgoing {
        "You".to_owned()
    } else {
        match &m.sender {
            Sender::User(id) => format!("user {id}"),
            Sender::Chat(id) => format!("chat {id}"),
        }
    };
    let state = match &m.send_state {
        SendState::Sent => "",
        SendState::Pending => " [sending…]",
        SendState::Failed { .. } => " [failed]",
    };
    // Append a media caption inline when there is one, so a photo/video with a
    // caption reads as `<photo 1280x720> nice view` rather than dropping the text.
    let caption = |c: &FormattedText| {
        if c.text.is_empty() {
            String::new()
        } else {
            format!(" {}", c.text)
        }
    };
    let body = match &m.content {
        MessageContent::Text(t) => t.text.clone(),
        MessageContent::Photo(p) => {
            format!("<photo {}x{}>{}", p.width, p.height, caption(&p.caption))
        }
        MessageContent::Video(v) => format!(
            "<video {}x{} {}s>{}",
            v.width,
            v.height,
            v.duration,
            caption(&v.caption)
        ),
        MessageContent::Document(d) => {
            format!("<document {}>{}", d.file_name, caption(&d.caption))
        }
        MessageContent::Audio(a) => {
            format!(
                "<audio {} — {}>{}",
                a.performer,
                a.title,
                caption(&a.caption)
            )
        }
        MessageContent::Voice(v) => format!("<voice {}s>{}", v.duration, caption(&v.caption)),
        MessageContent::Sticker(s) => format!("<sticker {} {}x{}>", s.emoji, s.width, s.height),
        MessageContent::Animation(a) => format!(
            "<animation {}x{} {}s>{}",
            a.width,
            a.height,
            a.duration,
            caption(&a.caption)
        ),
        MessageContent::Location(l) => {
            format!("<location {:.5},{:.5}>", l.latitude, l.longitude)
        }
        MessageContent::Venue(v) => format!("<venue {} — {}>", v.title, v.address),
        MessageContent::Contact(c) => {
            format!(
                "<contact {} {} {}>",
                c.first_name, c.last_name, c.phone_number
            )
        }
        MessageContent::Poll(p) => {
            format!("<poll {} ({} options)>", p.question.text, p.options.len())
        }
        MessageContent::Unsupported(name) => format!("<{name}>"),
    };
    format!("  [{}] {who}{state}: {body}", m.id)
}

// ----------------------------------------------------------------------------
// Command parsing
// ----------------------------------------------------------------------------

/// Split a line into its first whitespace-delimited token and the trimmed rest.
fn split_first(line: &str) -> (&str, &str) {
    match line.split_once(char::is_whitespace) {
        Some((head, tail)) => (head, tail.trim()),
        None => (line, ""),
    }
}

/// Parse a single chat id argument.
fn parse_chat(rest: &str) -> Result<i64, String> {
    rest.trim()
        .parse()
        .map_err(|_| "usage: <command> <chat_id>".to_owned())
}

/// Parse `<chat_id> <text...>`, where the text is everything after the id.
fn parse_chat_and_text(rest: &str) -> Result<(i64, String), String> {
    let (id, text) = split_first(rest.trim());
    let chat_id = id
        .parse()
        .map_err(|_| "usage: send <chat_id> <text>".to_owned())?;
    if text.is_empty() {
        return Err("usage: send <chat_id> <text>".to_owned());
    }
    Ok((chat_id, text.to_owned()))
}

/// Parse `<chat_id> <message_id> <text...>`.
fn parse_chat_msg_and_text(rest: &str) -> Result<(i64, i64, String), String> {
    let usage = "usage: <command> <chat_id> <message_id> <text>";
    let (id, after) = split_first(rest.trim());
    let (msg, text) = split_first(after);
    let chat_id = id.parse().map_err(|_| usage.to_owned())?;
    let message_id = msg.parse().map_err(|_| usage.to_owned())?;
    if text.is_empty() {
        return Err(usage.to_owned());
    }
    Ok((chat_id, message_id, text.to_owned()))
}

/// Parse `<chat_id> <message_id> [all]`, where a trailing `all` revokes for
/// everyone (otherwise the delete is for us only).
fn parse_delete(rest: &str) -> Result<(i64, i64, bool), String> {
    let usage = "usage: delete <chat_id> <message_id> [all]";
    let mut parts = rest.split_whitespace();
    let chat_id = parts
        .next()
        .and_then(|p| p.parse().ok())
        .ok_or_else(|| usage.to_owned())?;
    let message_id = parts
        .next()
        .and_then(|p| p.parse().ok())
        .ok_or_else(|| usage.to_owned())?;
    let revoke = match parts.next() {
        None => false,
        Some("all") => true,
        Some(other) => return Err(format!("expected `all` or nothing, got {other:?}")),
    };
    Ok((chat_id, message_id, revoke))
}

/// The command reference, shown on entry and on `help`.
fn print_help() {
    println!(
        "Commands:\n\
         \x20 chats                              list the Main chat list\n\
         \x20 open <chat>                        load + mark read + show recent history\n\
         \x20 history <chat>                     show known messages for a chat\n\
         \x20 send <chat> <text>                 send a text message\n\
         \x20 reply <chat> <msg> <text>          reply to a message\n\
         \x20 edit <chat> <msg> <text>           edit one of your messages\n\
         \x20 delete <chat> <msg> [all]          delete a message (all = for everyone)\n\
         \x20 read <chat>                        mark a chat's known messages read\n\
         \x20 logout                             end the session and exit (next run logs in fresh)\n\
         \x20 help                               show this help\n\
         \x20 quit                               exit (Ctrl-D also works)"
    );
}

// ----------------------------------------------------------------------------
// Shared helpers (carried from the Phase 2 login harness)
// ----------------------------------------------------------------------------

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
        return Err("input closed (EOF)".into());
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
        "tuigram headless harness {}\n\
         \n\
         tuigram is an independent, open-source terminal client built on the\n\
         Telegram API (via TDLib). It is part of the Telegram ecosystem but is\n\
         not affiliated with, endorsed by, or operated by Telegram, and carries\n\
         no official Telegram branding or logo.\n\
         \n\
         This harness logs in to a real Telegram account over stdin and then\n\
         drives the client (list/open chats, send, reply, edit, delete, read) to\n\
         verify the core before the TUI exists. Your credentials, login code, and\n\
         2FA password are never logged.\n",
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
