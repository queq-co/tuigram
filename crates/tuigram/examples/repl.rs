//! Headless REPL harness (#9, #22, #57) — drives the Phase 3 + Phase 4 client
//! end-to-end over stdin against a real account, before the TUI (Phase 5) exists.
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
//! exercises the Phase 3 surface — list chats, open a chat (load + view history),
//! send, reply, edit, delete, mark read, search, forward, log out — and the
//! Phase 4 surface on top: download/inspect a message's media and send media
//! (#1–#2, #4), list the archive and folders (#5–#6), react and pin (#8), send a
//! typing action (#9), and create/list/close secret chats (#10–#11; open and send
//! reuse the ordinary chat-id commands). Reads come from the facade's folded
//! snapshot (kept current by its single update router); writes go over the
//! bridge's per-domain request traits. `search` is the exception that returns its
//! hits directly: they print from the facade's transient `SearchResults` and never
//! fold into the snapshot, so a search leaves loaded history untouched.
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
//! the unsolicited live stream. Media is handled by **path only** — `download`
//! reports the local path TDLib writes to and `sendmedia` takes a local path; the
//! file's bytes are never opened, read, or logged by the harness.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rustyline::completion::Completer;
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::history::DefaultHistory;
use rustyline::validate::Validator;
use rustyline::{CompletionType, Config, Context, Editor, Helper};
use tokio_stream::StreamExt;
use tuigram_core::enums::Update;
use tuigram_core::types::Error as TdError;
use tuigram_core::{
    ApiCredentials, AuthRequests, AuthState, Bridge, Chat, ChatAction, ChatActionRequests,
    ChatKind, Client, ClientParameters, ConnectionState, CredentialError, CredentialResolver,
    DOWNLOAD_PRIORITY, DeleteRequests, EditRequests, FileRequests, FormattedText, HistoryRequests,
    Login, Message, MessageContent, NEWEST, Onboarding, OutgoingMedia, PinRequests, Reaction,
    ReactionKind, ReactionRequests, ReadRequests, SecretChatRequests, SecretChatState,
    SendRequests, SendState, Sender, SessionStorage, TgClient, UpdateStream, load_archive_list,
    load_folder_list, load_main_list, scrub_line, scrub_prose,
};

type Fallible = Result<(), Box<dyn std::error::Error>>;

/// How many of a chat's most recent messages a single `open` pulls.
const HISTORY_PAGE: i32 = 50;
/// How many search hits to request per page; the facade pages to exhaustion.
const SEARCH_PAGE: i32 = 50;
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
    let result = run_repl(&client).await;
    // Flush and cleanly close TDLib's database before exit — on every path
    // (`quit`, EOF, or `logout`). Dropping the bridge only stops the receive
    // loop; without an explicit close TDLib's SQLite database is left mid-write
    // and the next run fails to open it ("database disk image is malformed").
    shutdown(&client).await;
    result
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
                Err(e) => report_retry(&e),
            }
        },
        AuthState::WaitEmailCode { email_pattern } => {
            println!("  A login code was sent to {email_pattern}.");
            loop {
                let code = prompt("Email login code: ")?;
                match login.submit_email_code(code).await {
                    Ok(()) => break,
                    Err(e) => report_retry(&e),
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
                    Err(e) => report_retry(&e),
                }
            }
        }
        AuthState::WaitPremiumPurchase { store_product_id } => {
            // No headless answer exists: completing this needs an App Store / Play
            // in-store purchase. Report the dead end rather than hang.
            return Err(format!(
                "login requires buying Telegram Premium (store product \
                 {store_product_id}) as an in-store purchase, which this headless \
                 client can't perform — log in on a mobile app first"
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

    // The interactive command prompt gets readline-style editing: in-session
    // up/down recall and command-name tab completion (#156–#157). History is
    // in-memory only (see `build_editor`); the login and onboarding prompts above
    // deliberately keep the plain `prompt` reads so secrets never reach it.
    let mut editor = match build_editor() {
        Ok(editor) => editor,
        Err(e) => {
            eprintln!("Could not start the line editor: {e}");
            return Ok(());
        }
    };

    loop {
        let line = match editor.readline("\ntuigram> ") {
            Ok(line) => {
                let line = line.trim().to_owned();
                if !line.is_empty() {
                    // Record the command for up/down recall (in-memory only).
                    let _ = editor.add_history_entry(line.as_str());
                }
                line
            }
            // Ctrl-C abandons the current line and re-prompts, like a shell.
            Err(ReadlineError::Interrupted) => continue,
            // Ctrl-D (EOF) ends the REPL cleanly — the same exit `quit` gives, so
            // `shutdown` still flushes and closes the TDLib database on the way out.
            Err(ReadlineError::Eof) => {
                println!("Bye.");
                return Ok(());
            }
            Err(e) => {
                eprintln!("Input error: {e}");
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
            "search" => run_search(client, rest).await,
            "forward" => match parse_forward(rest) {
                Ok((from, ids, to)) => forward_messages(client, from, ids, to).await,
                Err(e) => println!("{e}"),
            },
            "download" => match parse_chat_msg(rest) {
                Ok((chat_id, message_id)) => download_media(client, chat_id, message_id).await,
                Err(e) => println!("{e}"),
            },
            "file" => match rest.trim().parse::<i32>() {
                Ok(file_id) => show_file(client, file_id),
                Err(_) => println!("usage: file <file_id>"),
            },
            "sendmedia" => match parse_sendmedia(rest) {
                Ok((chat_id, media)) => send_media(client, chat_id, media).await,
                Err(e) => println!("{e}"),
            },
            "archive" => list_archive(client).await,
            "folders" => list_folders(client),
            "folder" => match parse_chat(rest) {
                Ok(folder_id) => open_folder(client, folder_id as i32).await,
                Err(_) => println!("usage: folder <folder_id>"),
            },
            "react" => match parse_chat_msg_emoji(rest) {
                Ok((chat_id, message_id, emoji)) => {
                    set_reaction(client, chat_id, message_id, emoji, true).await;
                }
                Err(e) => println!("{e}"),
            },
            "unreact" => match parse_chat_msg_emoji(rest) {
                Ok((chat_id, message_id, emoji)) => {
                    set_reaction(client, chat_id, message_id, emoji, false).await;
                }
                Err(e) => println!("{e}"),
            },
            "pin" => match parse_chat_msg(rest) {
                Ok((chat_id, message_id)) => set_pin(client, chat_id, message_id, true).await,
                Err(e) => println!("{e}"),
            },
            "unpin" => match parse_chat_msg(rest) {
                Ok((chat_id, message_id)) => set_pin(client, chat_id, message_id, false).await,
                Err(e) => println!("{e}"),
            },
            "typing" => match parse_chat(rest) {
                Ok(chat_id) => send_typing(client, chat_id).await,
                Err(e) => println!("{e}"),
            },
            "secret-new" => match rest.trim().parse::<i64>() {
                Ok(user_id) => new_secret_chat(client, user_id).await,
                Err(_) => println!("usage: secret-new <user_id>"),
            },
            "secrets" => list_secret_chats(client),
            "secret-close" => match rest.trim().parse::<i32>() {
                Ok(secret_chat_id) => close_secret_chat(client, secret_chat_id).await,
                Err(_) => println!("usage: secret-close <secret_chat_id>"),
            },
            "status" => show_status(client),
            "probe" => run_probe(rest),
            "resync" => resync(client).await,
            "logout" => {
                if logout(client).await == Flow::Done {
                    return Ok(());
                }
            }
            other => println!("Unknown command: {other:?}. Type `help`."),
        }
    }
}

/// Print the transport's connection/sync status and whether a dropped-update gap
/// is outstanding — the harness window onto what a TUI status bar would show.
fn show_status(client: &Client) {
    let (state, needs_resync, dropped) = client.read(|s| {
        (
            s.connection().state(),
            s.needs_resync(),
            s.dropped_updates(),
        )
    });
    println!("Connection: {}", connection_label(state));
    if needs_resync {
        println!("Sync: STALE — {dropped} update(s) dropped since the last resync; run `resync`.");
    } else if dropped > 0 {
        println!("Sync: in sync ({dropped} update(s) dropped earlier, since recovered).");
    } else {
        println!("Sync: in sync.");
    }
}

/// Re-query the chat list after a dropped-update gap, then report the new status.
async fn resync(client: &Client) {
    match client.resync().await {
        Ok(()) => {
            tokio::time::sleep(SETTLE).await;
            println!("Resynced the Main chat list.");
            show_status(client);
        }
        Err(e) => println!("Resync failed: {} {}", e.code, e.message),
    }
}

/// A human-readable label for a [`ConnectionState`], for `status`.
fn connection_label(state: ConnectionState) -> &'static str {
    match state {
        ConnectionState::WaitingForNetwork => "waiting for network",
        ConnectionState::Connecting => "connecting",
        ConnectionState::ConnectingToProxy => "connecting (via proxy)",
        ConnectionState::Updating => "updating (catching up)",
        ConnectionState::Ready => "ready (in sync)",
    }
}

/// Print the Main chat list from the folded snapshot: id, unread count, title.
fn list_chats(client: &Client) {
    let rows = client.read(|state| {
        state
            .chats()
            .main_list()
            .iter()
            .map(format_chat_row)
            .collect()
    });
    print_chat_rows(
        rows,
        "Chats (most recent first):",
        "(no chats loaded yet — they fold in asynchronously; try `chats` again)",
    );
}

/// Format one chat-list row: id, unread count, title.
fn format_chat_row(c: &&Chat) -> String {
    format!("  {:>14}  unread {:<5} {}", c.id, c.unread_count, c.title)
}

/// Print a list of chat rows under a header, or an `empty` notice if there are none.
fn print_chat_rows(rows: Vec<String>, header: &str, empty: &str) {
    if rows.is_empty() {
        println!("{empty}");
    } else {
        println!("{header}");
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

/// Run a message search and print the hits straight from the returned transient
/// view. With a leading chat id the search is scoped to that chat; otherwise it
/// is account-wide. The results never fold into the folded snapshot, so the live
/// history store is untouched — this prints from the [`SearchResults`] directly,
/// not from `client.read`.
async fn run_search(client: &Client, rest: &str) {
    let (scope, query) = parse_search(rest);
    if query.is_empty() {
        println!("usage: search [chat] <query>");
        return;
    }
    let results = match scope {
        // No sender filter from the REPL; the facade still threads one through.
        Some(chat_id) => client.search_chat(chat_id, query, None, SEARCH_PAGE).await,
        None => client.search_messages(query, SEARCH_PAGE).await,
    };
    match results {
        Ok(hits) if hits.is_empty() => println!("(no matches)"),
        Ok(hits) => {
            println!("{} match(es):", hits.len());
            for m in hits.messages() {
                // Prefix the chat so global hits across chats are distinguishable.
                println!(
                    "  chat {:>14} {}",
                    m.chat_id,
                    format_message(m).trim_start()
                );
            }
        }
        Err(e) => println!("Search failed: {} {}", e.code, e.message),
    }
}

/// Forward messages from one chat into another, carrying the usual "forwarded
/// from" attribution. The forwarded copies fold into the target chat via the
/// router on the optimistic-send lifecycle, so `history <to>` shows them settle.
async fn forward_messages(client: &Client, from: i64, ids: Vec<i64>, to: i64) {
    let count = ids.len();
    match client.forward_messages(from, ids, to, false, false).await {
        Ok(msgs) => {
            let temp_ids = msgs
                .iter()
                .map(|m| m.id.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            println!(
                "Forwarded {count} message(s) to {to} (optimistic ids {temp_ids}). \
                 `history {to}` to follow them."
            );
        }
        Err(e) => println!("Forward failed: {} {}", e.code, e.message),
    }
}

// ----------------------------------------------------------------------------
// Media (Phase 4: #1–#2 download/inspect, #4 send)
// ----------------------------------------------------------------------------

/// Download the media attached to a known message and report its local path.
/// Looks the file id up from the folded snapshot, asks TDLib to download it, then
/// — once `updateFile` has folded the progress — prints the path. The bytes are
/// never read or logged, only the path TDLib writes them to.
async fn download_media(client: &Client, chat_id: i64, message_id: i64) {
    let file_id = client.read(|state| {
        state
            .messages()
            .history(chat_id)
            .iter()
            .find(|m| m.id == message_id)
            .and_then(|m| media_file_id(&m.content))
    });
    let Some(file_id) = file_id else {
        println!("(no downloadable media on {message_id} in {chat_id} — `open {chat_id}` first?)");
        return;
    };
    if let Err(e) = client
        .bridge()
        .download_file(file_id, DOWNLOAD_PRIORITY)
        .await
    {
        println!("Download failed: {} {}", e.code, e.message);
        return;
    }
    tokio::time::sleep(SETTLE).await;
    show_file(client, file_id);
}

/// Print a file's transfer state from the folded `FileStore`: progress and the
/// local path once it exists. Never opens or reads the file's bytes.
fn show_file(client: &Client, file_id: i32) {
    let line = client.read(|state| {
        state.files().get(file_id).map(|f| {
            let dest = if f.local_path.is_empty() {
                "(not on disk yet)".to_owned()
            } else {
                f.local_path.clone()
            };
            let status = if f.is_downloading_completed {
                "complete"
            } else if f.is_downloading_active {
                "downloading"
            } else {
                "idle"
            };
            format!(
                "file {file_id}: {status}, {}/{} bytes -> {dest}",
                f.downloaded_size, f.size
            )
        })
    });
    match line {
        Some(line) => println!("{line}"),
        None => println!("(file {file_id} unknown yet — it folds in as it transfers)"),
    }
}

/// The downloadable file id of a media message, if it carries one.
fn media_file_id(content: &MessageContent) -> Option<i32> {
    let id = match content {
        MessageContent::Photo(p) => p.file.id,
        MessageContent::Video(v) => v.file.id,
        MessageContent::Document(d) => d.file.id,
        MessageContent::Audio(a) => a.file.id,
        MessageContent::Voice(v) => v.file.id,
        MessageContent::Animation(a) => a.file.id,
        MessageContent::Sticker(s) => s.file.id,
        _ => return None,
    };
    Some(id)
}

/// Send a local media file to a chat. The optimistic message folds in like a text
/// send; the upload streams as `updateFile` (watch it with `file`) and the send
/// reconciles — `history <chat>` follows both. Only the path is handled here,
/// never the file's bytes.
async fn send_media(client: &Client, chat_id: i64, media: OutgoingMedia) {
    match client.bridge().send_media(chat_id, None, media).await {
        Ok(msg) => println!(
            "Uploading (optimistic id {}). `history {chat_id}` to follow it.",
            msg.id
        ),
        Err(e) => println!("Send failed: {} {}", e.code, e.message),
    }
}

// ----------------------------------------------------------------------------
// Archive + folders (Phase 4: #5–#6)
// ----------------------------------------------------------------------------

/// Load and print the Archive chat list, the same shape as `chats`.
async fn list_archive(client: &Client) {
    if let Err(e) = load_archive_list(client.bridge(), CHATS_PAGE).await {
        println!("Could not load the archive: {} {}", e.code, e.message);
        return;
    }
    tokio::time::sleep(SETTLE).await;
    let rows = client.read(|state| {
        state
            .chats()
            .archive_list()
            .iter()
            .map(format_chat_row)
            .collect()
    });
    print_chat_rows(rows, "Archived chats:", "(no archived chats)");
}

/// List the user's chat folders (id + title) from the folded snapshot. Folders
/// arrive as `updateChatFolders` and fold on their own; `folder <id>` lists a
/// folder's chats.
fn list_folders(client: &Client) {
    let rows = client.read(|state| {
        state
            .chats()
            .folders()
            .iter()
            .map(|f| format!("  {:>6}  {}", f.id, f.title))
            .collect::<Vec<_>>()
    });
    print_chat_rows(rows, "Folders:", "(no folders defined)");
}

/// Load and print a folder's chats by folder id.
async fn open_folder(client: &Client, folder_id: i32) {
    if let Err(e) = load_folder_list(client.bridge(), folder_id, CHATS_PAGE).await {
        println!(
            "Could not load folder {folder_id}: {} {}",
            e.code, e.message
        );
        return;
    }
    tokio::time::sleep(SETTLE).await;
    let rows = client.read(|state| {
        state
            .chats()
            .folder_list(folder_id)
            .iter()
            .map(format_chat_row)
            .collect()
    });
    print_chat_rows(
        rows,
        &format!("Folder {folder_id} chats:"),
        &format!("(folder {folder_id} has no loaded chats — is the id right?)"),
    );
}

// ----------------------------------------------------------------------------
// Reactions + pins (Phase 4: #8)
// ----------------------------------------------------------------------------

/// Add or remove our emoji reaction on a message. The new counts fold via
/// `updateMessageInteractionInfo`; `history <chat>` shows them (reactions print
/// inline after the body).
async fn set_reaction(client: &Client, chat_id: i64, message_id: i64, emoji: String, add: bool) {
    let bridge = client.bridge();
    let result = if add {
        bridge
            .add_message_reaction(chat_id, message_id, emoji)
            .await
    } else {
        bridge
            .remove_message_reaction(chat_id, message_id, emoji)
            .await
    };
    match result {
        Ok(()) => {
            let verb = if add { "Reacted to" } else { "Un-reacted from" };
            println!("{verb} {message_id}. `history {chat_id}` to see the counts.");
        }
        Err(e) => println!("Reaction failed: {} {}", e.code, e.message),
    }
}

/// Pin or unpin a message in a chat. Pins fold into the chat's pinned set
/// (`updateChatPinnedMessage`/`updateMessageIsPinned`). Pins silently (no member
/// notification) and chat-wide, not only for us.
async fn set_pin(client: &Client, chat_id: i64, message_id: i64, pin: bool) {
    let bridge = client.bridge();
    let result = if pin {
        bridge
            .pin_chat_message(chat_id, message_id, true, false)
            .await
    } else {
        bridge.unpin_chat_message(chat_id, message_id).await
    };
    match result {
        Ok(()) => {
            let verb = if pin { "Pinned" } else { "Unpinned" };
            println!("{verb} {message_id} in {chat_id}.");
        }
        Err(e) => println!("Pin failed: {} {}", e.code, e.message),
    }
}

// ----------------------------------------------------------------------------
// Chat actions (Phase 4: #9)
// ----------------------------------------------------------------------------

/// Broadcast a one-shot "typing…" action to a chat. Advisory and best-effort: the
/// server expires it after a few seconds and never echoes our own action back, so
/// there is nothing to fold — this just fires it.
async fn send_typing(client: &Client, chat_id: i64) {
    match client
        .bridge()
        .send_chat_action(chat_id, Some(ChatAction::Typing))
        .await
    {
        Ok(()) => println!("Sent a typing action to {chat_id} (expires on its own)."),
        Err(e) => println!("Chat action failed: {} {}", e.code, e.message),
    }
}

// ----------------------------------------------------------------------------
// Secret chats (Phase 4: #10–#11)
// ----------------------------------------------------------------------------

/// Create a new secret chat with a user. Returns the new chat synchronously; its
/// encryption record arrives as `updateSecretChat` (see `secrets`). It starts
/// pending until the partner's device completes the key exchange — once ready,
/// send/open it by its chat id with the ordinary `send`/`open`.
async fn new_secret_chat(client: &Client, user_id: i64) {
    match client.bridge().create_new_secret_chat(user_id).await {
        Ok(chat) => println!(
            "Secret chat created (chat id {}). `secrets` for its state; \
             `open {}` / `send {} <text>` once it's ready.",
            chat.id, chat.id, chat.id
        ),
        Err(e) => println!("Could not create secret chat: {} {}", e.code, e.message),
    }
}

/// List the secret chats among the loaded Main list, joining each
/// [`ChatKind::Secret`] to its encryption record: lifecycle state, who opened it,
/// and whether the key-verification hash has arrived (size only — never the bytes).
fn list_secret_chats(client: &Client) {
    let rows =
        client.read(|state| {
            state
            .chats()
            .main_list()
            .iter()
            .filter_map(|c| match &c.kind {
                ChatKind::Secret { secret_chat_id, .. } => {
                    let sc = state.secret_chats().get(*secret_chat_id)?;
                    let state_str = match sc.state {
                        SecretChatState::Pending => "pending",
                        SecretChatState::Ready => "ready",
                        SecretChatState::Closed => "closed",
                    };
                    let who = if sc.is_outbound { "outbound" } else { "inbound" };
                    let key = if sc.key_hash.is_empty() {
                        String::new()
                    } else {
                        format!(" key:{}B", sc.key_hash.len())
                    };
                    Some(format!(
                        "  chat {:>14}  secret {secret_chat_id:<4} {state_str} ({who}){key}  {}",
                        c.id, c.title
                    ))
                }
                _ => None,
            })
            .collect::<Vec<_>>()
        });
    print_chat_rows(
        rows,
        "Secret chats:",
        "(no secret chats in the loaded Main list — `secret-new <user_id>` to start one)",
    );
}

/// Close a secret chat by its secret-chat id. The state advances to closed via
/// `updateSecretChat`; `secrets` reflects it.
async fn close_secret_chat(client: &Client, secret_chat_id: i32) {
    match client.bridge().close_secret_chat(secret_chat_id).await {
        Ok(()) => println!("Closed secret chat {secret_chat_id}."),
        Err(e) => println!("Close failed: {} {}", e.code, e.message),
    }
}

/// Log out: invalidate the session, wait for TDLib to *fully* clear it, then end
/// the REPL so the next run starts at a fresh login. A failed request stays in
/// the REPL ([`Flow::Continue`]); a successful one exits ([`Flow::Done`]).
///
/// `logOut` is asynchronous — TDLib invalidates the session server-side and
/// destroys all local data, driving authorization through `Closing` to `Closed`.
/// Waiting for `Closed` here is what makes the next run start with no session on
/// disk and behave exactly like a first-time login; returning early would strand
/// a half-cleared session the next run can neither resume nor cleanly replace.
async fn logout(client: &Client) -> Flow {
    println!("Logging out…");
    // `log_out_and_wait` bundles the logOut with the wait for `Closed` that makes
    // the teardown complete before we return (#195) — the same whole-operation the
    // TUI uses, so the two clients cannot drift on what "logout" requires.
    if let Err(e) = client.bridge().log_out_and_wait().await {
        println!("Logout failed: {} {}", e.code, e.message);
        return Flow::Continue;
    }
    println!("Logged out. The local session has been cleared — re-run to sign in again.");
    Flow::Done
}

/// Cleanly close the TDLib instance before the process exits, so its database is
/// flushed and properly closed rather than left mid-write. Called on every exit
/// path; harmless when the session is already gone (e.g. straight after
/// `logout`) — `close_and_wait` ignores the rejected `close` and returns at once.
///
/// Delegates to the shared [`AuthRequests::close_and_wait`] whole-operation (#195),
/// the same one the TUI's shutdown uses, so "clean shutdown" means one thing.
async fn shutdown(client: &Client) {
    client.bridge().close_and_wait().await;
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
    format!(
        "  [{}] {who}{state}: {body}{}",
        m.id,
        format_reactions(&m.reactions)
    )
}

/// Render a message's reaction buckets inline, e.g. ` {👍×3* ❤×1}`, where `*`
/// marks a reaction our own account chose. Empty when there are none.
fn format_reactions(reactions: &[Reaction]) -> String {
    if reactions.is_empty() {
        return String::new();
    }
    let parts = reactions
        .iter()
        .map(|r| {
            let label = match &r.kind {
                ReactionKind::Emoji(e) => e.clone(),
                ReactionKind::CustomEmoji(id) => format!("custom:{id}"),
                ReactionKind::Paid => "⭐paid".to_owned(),
            };
            let chosen = if r.is_chosen { "*" } else { "" };
            format!("{label}×{}{chosen}", r.count)
        })
        .collect::<Vec<_>>()
        .join(" ");
    format!(" {{{parts}}}")
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

/// Parse `search`'s argument: an optional leading chat id followed by the query.
/// A first token that parses as an integer (with a non-empty remainder) scopes
/// the search to that chat; otherwise the whole string is a global query. A
/// global query that begins with a bare number is therefore read as a chat id —
/// an accepted ambiguity for this harness.
fn parse_search(rest: &str) -> (Option<i64>, String) {
    let rest = rest.trim();
    let (head, tail) = split_first(rest);
    match head.parse::<i64>() {
        Ok(chat_id) if !tail.is_empty() => (Some(chat_id), tail.to_owned()),
        _ => (None, rest.to_owned()),
    }
}

/// Parse `forward <from_chat> <msg_ids> <to_chat>`, where `msg_ids` is one id or
/// a comma-separated list with no spaces (e.g. `101,102,103`).
fn parse_forward(rest: &str) -> Result<(i64, Vec<i64>, i64), String> {
    let usage = "usage: forward <from_chat> <msg_id[,msg_id...]> <to_chat>";
    let mut parts = rest.split_whitespace();
    let from = parts
        .next()
        .and_then(|p| p.parse().ok())
        .ok_or_else(|| usage.to_owned())?;
    let ids_raw = parts.next().ok_or_else(|| usage.to_owned())?;
    let to = parts
        .next()
        .and_then(|p| p.parse().ok())
        .ok_or_else(|| usage.to_owned())?;
    if parts.next().is_some() {
        return Err(usage.to_owned());
    }
    let message_ids = ids_raw
        .split(',')
        .map(str::parse)
        .collect::<Result<Vec<i64>, _>>()
        .map_err(|_| usage.to_owned())?;
    if message_ids.is_empty() {
        return Err(usage.to_owned());
    }
    Ok((from, message_ids, to))
}

/// Parse `<chat_id> <message_id>`.
fn parse_chat_msg(rest: &str) -> Result<(i64, i64), String> {
    let usage = "usage: <command> <chat_id> <message_id>";
    let mut parts = rest.split_whitespace();
    let chat_id = parts
        .next()
        .and_then(|p| p.parse().ok())
        .ok_or_else(|| usage.to_owned())?;
    let message_id = parts
        .next()
        .and_then(|p| p.parse().ok())
        .ok_or_else(|| usage.to_owned())?;
    Ok((chat_id, message_id))
}

/// Parse `<chat_id> <message_id> <emoji>`.
fn parse_chat_msg_emoji(rest: &str) -> Result<(i64, i64, String), String> {
    let usage = "usage: <command> <chat_id> <message_id> <emoji>";
    let (chat, after) = split_first(rest.trim());
    let (msg, emoji) = split_first(after);
    let chat_id = chat.parse().map_err(|_| usage.to_owned())?;
    let message_id = msg.parse().map_err(|_| usage.to_owned())?;
    if emoji.is_empty() {
        return Err(usage.to_owned());
    }
    Ok((chat_id, message_id, emoji.to_owned()))
}

/// Parse `<chat_id> <photo|video|document> <path> [caption...]` into the chat and
/// an [`OutgoingMedia`]. The path is a single whitespace-free token; everything
/// after it is an optional caption.
fn parse_sendmedia(rest: &str) -> Result<(i64, OutgoingMedia), String> {
    let usage = "usage: sendmedia <chat_id> <photo|video|document> <path> [caption]";
    let (chat, after) = split_first(rest.trim());
    let (kind, after) = split_first(after);
    let (path, caption_text) = split_first(after);
    let chat_id = chat.parse().map_err(|_| usage.to_owned())?;
    if path.is_empty() {
        return Err(usage.to_owned());
    }
    let path = path.to_owned();
    let caption = FormattedText {
        text: caption_text.to_owned(),
        entities: vec![],
    };
    let media = match kind {
        "photo" => OutgoingMedia::Photo { path, caption },
        "video" => OutgoingMedia::Video { path, caption },
        "document" | "doc" => OutgoingMedia::Document { path, caption },
        other => {
            return Err(format!(
                "unknown media kind {other:?}; use photo, video, or document"
            ));
        }
    };
    Ok((chat_id, media))
}

/// A built-in terminal-injection payload for the `probe` command: a human label
/// and the raw hostile bytes exactly as a contact could embed them in a message.
struct Payload {
    /// What the payload attempts, for the operator's benefit.
    label: &'static str,
    /// The raw attacker bytes — real ESC/BEL/bidi, not their printable spellings.
    raw: &'static str,
    /// Whether this stands in for a single-line identifier (file name, title),
    /// which the boundary scrubs with [`scrub_line`], versus a prose body
    /// ([`scrub_prose`]). Mirrors the field-shape split in `from_tdlib`.
    line: bool,
}

/// The payloads `probe` exercises: the escape-injection vectors #174 defends
/// against, carried as literals so the operator never has to type a raw ESC
/// (which most terminals and editors swallow) to reproduce the attack.
const PROBE_PAYLOADS: &[Payload] = &[
    Payload {
        label: "OSC set-window-title",
        raw: "before \x1b]0;PWNED\x07 after",
        line: false,
    },
    Payload {
        label: "OSC 8 hyperlink cloak",
        raw: "\x1b]8;;https://evil.example\x1b\\click me\x1b]8;;\x1b\\",
        line: false,
    },
    Payload {
        label: "DECRQSS status query",
        raw: "\x1bP$qm\x1b\\",
        line: false,
    },
    Payload {
        label: "SGR colour bleed",
        raw: "\x1b[31mred-forever",
        line: false,
    },
    Payload {
        label: "Trojan-Source file name",
        raw: "report_e\u{202e}xe.txt",
        line: true,
    },
];

/// Demonstrate the terminal-injection defence (#174) with no network round-trip
/// and no second account. The default run pushes each built-in payload through
/// the same public sanitizer the `from_tdlib` trust boundary uses and prints the
/// neutralized result, so you can confirm on a real terminal that nothing reacts.
/// `probe raw` instead emits the *unscrubbed* bytes so you can watch the attack
/// fire — proof the threat is real — then restores the window title.
fn run_probe(rest: &str) {
    if rest.trim() == "raw" {
        probe_raw();
    } else {
        probe_safe();
    }
}

/// Scrub each payload at the boundary and report whether any control byte
/// survived — the property #174 guarantees.
fn probe_safe() {
    println!(
        "Terminal-injection probe — each payload scrubbed at the from_tdlib boundary (#174):\n"
    );
    let mut all_inert = true;
    for p in PROBE_PAYLOADS {
        let scrubbed = if p.line {
            scrub_line(p.raw)
        } else {
            scrub_prose(p.raw)
        };
        // `\n` is legitimate in scrubbed prose; any other control byte is a leak.
        let inert = !scrubbed.chars().any(|c| c.is_control() && c != '\n');
        all_inert &= inert;
        let verdict = if inert { "inert" } else { "LEAK" };
        println!("  {:<24} [{verdict}] {scrubbed}", p.label);
    }
    if all_inert {
        println!(
            "\nAll payloads inert. Your terminal title is unchanged and every escape shows as `\u{fffd}`."
        );
    } else {
        println!("\nA payload LEAKED a control byte — #174 has regressed.");
    }
    println!("Run `probe raw` to see the same payloads fire on an unprotected terminal.");
}

/// Emit the payloads unscrubbed so the operator can watch a real terminal react —
/// then undo the window title so the REPL does not leave `PWNED` behind.
fn probe_raw() {
    println!(
        "DANGER: writing raw payloads to your terminal — expect the window title to change.\n"
    );
    for p in PROBE_PAYLOADS {
        // The hostile bytes verbatim, exactly as an unsanitized render would emit.
        println!("  {}:\n    {}", p.label, p.raw);
    }
    // Courtesy reset: put the window title back so the demonstration is reversible.
    print!("\x1b]0;tuigram repl\x07");
    let _ = io::stdout().flush();
    println!("\n(window title restored). This is what #174 neutralizes on the real message path.");
}

/// One entry in the REPL's command table: the word that invokes it, its argument
/// summary, and a one-line description. This table is the single source of truth
/// for both the help listing ([`print_help`]) and tab completion ([`ReplHelper`]),
/// so the two cannot drift (#157).
struct Command {
    /// The word typed to run it — the tab-completion target.
    name: &'static str,
    /// Argument summary shown after the name in help, e.g. `"<chat> <text>"`.
    args: &'static str,
    /// One-line description of what the command does.
    help: &'static str,
}

/// The full command set, in help-display order. `help`/`quit` list their canonical
/// spellings; the `?`/`exit` aliases still work in the dispatcher but are not
/// completion targets. Keep this in sync with the `match` in [`run_repl`].
const COMMANDS: &[Command] = &[
    Command {
        name: "chats",
        args: "",
        help: "list the Main chat list",
    },
    Command {
        name: "open",
        args: "<chat>",
        help: "load + mark read + show recent history",
    },
    Command {
        name: "history",
        args: "<chat>",
        help: "show known messages for a chat",
    },
    Command {
        name: "send",
        args: "<chat> <text>",
        help: "send a text message",
    },
    Command {
        name: "reply",
        args: "<chat> <msg> <text>",
        help: "reply to a message",
    },
    Command {
        name: "edit",
        args: "<chat> <msg> <text>",
        help: "edit one of your messages",
    },
    Command {
        name: "delete",
        args: "<chat> <msg> [all]",
        help: "delete a message (all = for everyone)",
    },
    Command {
        name: "read",
        args: "<chat>",
        help: "mark a chat's known messages read",
    },
    Command {
        name: "search",
        args: "[chat] <query>",
        help: "search one chat (with id) or the whole account",
    },
    Command {
        name: "forward",
        args: "<from> <ids> <to>",
        help: "forward msg ids (comma-separated) between chats",
    },
    Command {
        name: "download",
        args: "<chat> <msg>",
        help: "download a message's media; show the local path",
    },
    Command {
        name: "file",
        args: "<file_id>",
        help: "show a file's transfer state + local path",
    },
    Command {
        name: "sendmedia",
        args: "<chat> <kind> <path> [cap]",
        help: "send photo|video|document from a local path",
    },
    Command {
        name: "archive",
        args: "",
        help: "list the Archive chat list",
    },
    Command {
        name: "folders",
        args: "",
        help: "list chat folders",
    },
    Command {
        name: "folder",
        args: "<id>",
        help: "list a folder's chats",
    },
    Command {
        name: "react",
        args: "<chat> <msg> <emoji>",
        help: "add an emoji reaction",
    },
    Command {
        name: "unreact",
        args: "<chat> <msg> <emoji>",
        help: "remove your emoji reaction",
    },
    Command {
        name: "pin",
        args: "<chat> <msg>",
        help: "pin a message (silently, chat-wide)",
    },
    Command {
        name: "unpin",
        args: "<chat> <msg>",
        help: "unpin a message",
    },
    Command {
        name: "typing",
        args: "<chat>",
        help: "send a one-shot typing action",
    },
    Command {
        name: "secret-new",
        args: "<user_id>",
        help: "start a secret chat with a user",
    },
    Command {
        name: "secrets",
        args: "",
        help: "list known secret chats + state",
    },
    Command {
        name: "secret-close",
        args: "<secret_id>",
        help: "close a secret chat",
    },
    Command {
        name: "status",
        args: "",
        help: "show connection/sync status + any dropped-update gap",
    },
    Command {
        name: "probe",
        args: "[raw]",
        help: "terminal-injection self-test (#174); `raw` fires the unscrubbed attack",
    },
    Command {
        name: "resync",
        args: "",
        help: "re-query the chat list after a dropped-update gap",
    },
    Command {
        name: "logout",
        args: "",
        help: "end the session and exit (next run logs in fresh)",
    },
    Command {
        name: "help",
        args: "",
        help: "show this help",
    },
    Command {
        name: "quit",
        args: "",
        help: "exit (Ctrl-D also works)",
    },
];

/// The command reference, shown on entry and on `help`. Rendered from the shared
/// [`COMMANDS`] table so it always lists exactly what tab completion offers.
fn print_help() {
    // Align the descriptions: pad each "name args" signature to the widest one.
    let width = COMMANDS
        .iter()
        .map(|c| {
            c.name.len()
                + if c.args.is_empty() {
                    0
                } else {
                    1 + c.args.len()
                }
        })
        .max()
        .unwrap_or(0);
    println!("Commands:");
    for c in COMMANDS {
        let signature = if c.args.is_empty() {
            c.name.to_owned()
        } else {
            format!("{} {}", c.name, c.args)
        };
        println!("  {signature:<width$}  {}", c.help);
    }
}

/// The command-name candidates for a completion `head` — the whitespace-free
/// command word before the cursor. Shared by the [`Completer`] impl and its tests,
/// and read straight from [`COMMANDS`] so completion always tracks the table (#157).
fn complete_command(head: &str) -> Vec<String> {
    COMMANDS
        .iter()
        .map(|c| c.name)
        .filter(|name| name.starts_with(head))
        .map(str::to_owned)
        .collect()
}

/// Build the REPL's line editor: command-name tab completion (#157) plus in-memory
/// command history for up/down recall (#156). The history is `MemHistory` — session
/// only, never written to disk — because command lines carry real message text and
/// file paths; persisting them would drop private content into a plaintext file, at
/// odds with the encrypted-at-rest posture (#8). `CompletionType::List` prints the
/// candidates on an ambiguous prefix rather than cycling through them silently.
fn build_editor() -> Result<Editor<ReplHelper, DefaultHistory>, ReadlineError> {
    let config = Config::builder()
        .completion_type(CompletionType::List)
        .build();
    let mut editor: Editor<ReplHelper, DefaultHistory> = Editor::with_config(config)?;
    editor.set_helper(Some(ReplHelper));
    Ok(editor)
}

/// rustyline glue for the REPL prompt. It completes the command word from the
/// shared [`COMMANDS`] table (#157); editing, hinting, and highlighting stay at
/// their trait defaults. Completion only fires in command position — once the
/// cursor is past the first token it returns nothing, so argument typing is left
/// untouched (the stretch of completing chat ids in argument position is not done).
struct ReplHelper;

impl Completer for ReplHelper {
    type Candidate = String;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> Result<(usize, Vec<String>), ReadlineError> {
        // Only the first word is a command. If anything before the cursor is
        // whitespace we are in argument position — offer nothing there.
        let head = &line[..pos];
        if head.contains(char::is_whitespace) {
            return Ok((pos, Vec::new()));
        }
        // Replace from index 0: `head` is the whole (whitespace-free) command word.
        Ok((0, complete_command(head)))
    }
}

// The REPL needs only completion; hinting, highlighting, and validation stay at
// their trait defaults so `ReplHelper` can satisfy `Helper`.
impl Hinter for ReplHelper {
    type Hint = String;
}
impl Highlighter for ReplHelper {}
impl Validator for ReplHelper {}
impl Helper for ReplHelper {}

#[cfg(test)]
mod tests {
    use super::{COMMANDS, complete_command};

    #[test]
    fn unique_prefix_completes_to_the_one_command() {
        assert_eq!(complete_command("op"), ["open"]);
    }

    #[test]
    fn ambiguous_prefix_lists_every_candidate() {
        let mut hits = complete_command("secret");
        hits.sort();
        assert_eq!(hits, ["secret-close", "secret-new", "secrets"]);
    }

    #[test]
    fn empty_head_offers_the_whole_command_table() {
        assert_eq!(complete_command("").len(), COMMANDS.len());
    }

    #[test]
    fn unknown_prefix_completes_to_nothing() {
        assert!(complete_command("zzz").is_empty());
    }

    #[test]
    fn every_dispatched_command_is_in_the_table() {
        // Guards the table against drifting from the `run_repl` match arms.
        for name in [
            "chats",
            "open",
            "history",
            "send",
            "reply",
            "edit",
            "delete",
            "read",
            "search",
            "forward",
            "download",
            "file",
            "sendmedia",
            "archive",
            "folders",
            "folder",
            "react",
            "unreact",
            "pin",
            "unpin",
            "typing",
            "secret-new",
            "secrets",
            "secret-close",
            "status",
            "probe",
            "resync",
            "logout",
            "help",
            "quit",
        ] {
            assert!(
                COMMANDS.iter().any(|c| c.name == name),
                "`{name}` is dispatched but missing from COMMANDS (help + completion)",
            );
        }
    }
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
         drives the client (chats, messages, media, archive/folders, reactions,\n\
         pins, typing, and secret chats) to verify the core before the TUI exists.\n\
         Your credentials, login code, and 2FA password are never logged, and\n\
         media is handled by local path only — file bytes are never read or logged.\n",
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
