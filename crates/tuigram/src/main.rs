//! `tuigram` — a Ratatui Telegram client.
//!
//! This is the Phase 5 spine: an RAII terminal guard, a panic hook that restores
//! the terminal, and the single `tokio::select!` loop that races terminal input,
//! a render tick, and core events into [`Action`]s applied to one [`App`]. The
//! draw call stays on the main task and is never awaited inside. Real widgets and
//! live Telegram data arrive in later Phase 5/6 issues; the loop's shape does not
//! change when they do.
//!
//! Phase 6 stands the real [`tuigram_core::Client`] up across three phases. #109
//! bootstraps an *initialized* bridge on the plain terminal ([`bootstrap`]:
//! credentials, secure storage, `setTdlibParameters`). #111 then drives **login
//! inside the TUI** ([`run_login`]): one screen per waiting auth state, answered
//! through the core `Login` seam, gating the three-pane UI behind `Ready` — only
//! then does `main` hand the bridge to [`Client::start`]. The run loop is fed by
//! the live core source (#110): [`spawn_core_source`] forwards the client's update
//! stream onto the mpsc arm the fake heartbeat used, classified into
//! [`AppEvent`](crate::event::AppEvent)s. On a chat signal the loop reads the
//! folded chat list back from the client and projects the left pane (#113),
//! paging each list to exhaustion on demand; on a message signal it reads the open
//! chat's folded history back and projects the conversation pane (#114), paging a
//! page at a time as the user opens a chat and scrolls up. While a chat is open the
//! loop acknowledges its unread messages to Telegram through the read seam (#115),
//! so the unread badge clears here and on the user's other clients. `main` closes TDLib
//! cleanly on every exit path, including a login the user quit before the facade
//! ever started.

mod app;
mod bootstrap;
mod chat_list;
mod composer;
mod conversation;
mod event;
mod forward;
mod keymap;
mod login;
mod mediaform;
mod reactions;
mod search;
mod secret;
mod status;
mod terminal;
mod textinput;
mod ui;

use std::collections::{HashMap, HashSet};
use std::io;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::EventStream;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;

use tuigram_core::model::ChatListKind;
use tuigram_core::{
    Client, HistoryRequests, NEWEST, ReadRequests, load_archive_list, load_folder_list,
    load_main_list,
};

use crate::app::{Action, App};
use crate::chat_list::project_lists;
use crate::event::{AppEvent, spawn_core_source};
use crate::keymap::Focus;
use crate::login::{LoginEnd, run_login};
use crate::terminal::{TerminalGuard, install_panic_hook};

/// Render cadence cap (~30 FPS). Bounds repaint rate independently of network
/// latency, so the UI stays smooth while core is mid-request.
const FRAME: Duration = Duration::from_millis(33);

/// How many chats to request per `loadChats` page when filling a list (#113).
/// The core pager loops a list to exhaustion at this granularity, so this only
/// sizes each batch — TDLib streams the chats back as updates the router folds.
const CHAT_PAGE: i32 = 100;

/// How many messages to request per `getChatHistory` page when filling the open
/// chat's history (#114). One page lands on open; pressing up at the very top of
/// the loaded history fetches the next older page, so memory stays bounded to what
/// the user has scrolled to rather than the whole chat.
const HISTORY_PAGE: i32 = 50;

/// Depth of the history-load → loop completion channel (#114). A history page
/// returns its messages directly (not as updates), so the spawned loader merges
/// them into the store and nudges the loop here to re-project; the loop coalesces
/// these through a full store re-read, so a shallow channel suffices.
const HISTORY_CHANNEL_DEPTH: usize = 16;

#[tokio::main]
async fn main() -> ExitCode {
    // Phase 1 — initialize TDLib on the plain terminal (credentials, secure
    // storage, setTdlibParameters), before raw mode. A failure here prints and
    // exits without ever touching the TUI. Login happens later, in the TUI.
    let bridge = match bootstrap::bootstrap().await {
        Ok(bridge) => bridge,
        Err(err) => {
            eprintln!("tuigram: {err}");
            return ExitCode::FAILURE;
        }
    };

    install_panic_hook();
    let mut guard = match TerminalGuard::new() {
        Ok(guard) => guard,
        Err(err) => {
            eprintln!("tuigram: could not initialize the terminal: {err}");
            bootstrap::shutdown(&bridge).await;
            return ExitCode::FAILURE;
        }
    };

    // Phase 2 — drive login inside the TUI. Only on `Ready` does the bridge become
    // a live `Client` and the three-pane loop run; quitting or a closed session
    // before then skips straight to shutdown.
    let result = match run_login(&mut guard, &bridge).await {
        Ok(LoginEnd::Ready) => {
            // `Arc` so the run loop can spawn background chat-list loads that each
            // hold a clone (#113); the bridge stays reachable for shutdown below.
            let client = Arc::new(Client::start(bridge));
            let run_result = run(&mut guard, &client).await;
            // Restore explicitly before any error reaches the normal screen.
            // (`guard`'s Drop would also restore, but make the ordering obvious.)
            drop(guard);
            // Phase 3 — close TDLib cleanly so its database is flushed, not left
            // mid-write for the next run.
            bootstrap::shutdown(client.bridge()).await;
            run_result
        }
        // The user quit or the session closed before login completed: never a
        // `Client`, so close the bridge directly.
        Ok(LoginEnd::Quit | LoginEnd::Closed) => {
            drop(guard);
            bootstrap::shutdown(&bridge).await;
            Ok(())
        }
        Err(err) => {
            drop(guard);
            bootstrap::shutdown(&bridge).await;
            Err(err)
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("tuigram: {err}");
            ExitCode::FAILURE
        }
    }
}

/// The central event loop. Owns no terminal lifecycle (that is `guard`'s job) and
/// awaits only the `select!` sources — never the `draw`. The `client` feeds the
/// core arm: [`spawn_core_source`] subscribes to its live update stream, and the
/// loop reads the folded chat list back from it to project the left pane (#113).
async fn run(guard: &mut TerminalGuard, client: &Arc<Client>) -> io::Result<()> {
    let mut app = App::new();
    let mut input = EventStream::new();
    let mut tick = tokio::time::interval(FRAME);
    let mut core_rx = spawn_core_source(client);
    // The lists whose `loadChats` paging has been kicked off, so each is loaded
    // at most once per run. A handful of entries (Main, Archive, a few folders),
    // so a `Vec` lookup beats pulling `Hash` onto the core enum.
    let mut requested: Vec<ChatListKind> = Vec::new();
    // The open chat's history paging state (#114) and the channel its loaders nudge
    // when a page is merged (a history page returns data directly, not as updates,
    // so it needs an explicit completion signal).
    let mut history = HistoryState::default();
    let (history_tx, mut history_rx) = mpsc::channel::<HistoryPage>(HISTORY_CHANNEL_DEPTH);

    // Kick off the landing list (Main) before the first frame; the rest load on
    // demand as the user switches to them.
    ensure_active_list_loaded(&app, client, &mut requested);

    while !app.should_quit() {
        if app.is_dirty() {
            guard.terminal_mut().draw(|frame| ui::ui(frame, &app))?;
            app.clear_dirty();
        }

        tokio::select! {
            // Terminal input. `None` => stdin closed; treat as a quit so the
            // app never spins on a dead stream.
            maybe_event = input.next() => match maybe_event {
                Some(Ok(event)) => {
                    let action = app.on_terminal_event(event);
                    app.dispatch(action);
                }
                // Transient read error: ignore and re-enter the loop.
                Some(Err(_)) => {}
                None => app.dispatch(Action::Quit),
            },
            // Render tick: mark dirty so clocks/animations repaint on cadence.
            _ = tick.tick() => app.dispatch(Action::Render),
            // Live core events. `None` => the source ended (the bridge closed its
            // broadcast on shutdown); keep running so a late teardown can't wedge
            // the loop — the quit path drives the exit.
            maybe_app_event = core_rx.recv() => {
                if let Some(app_event) = maybe_app_event {
                    match app_event {
                        // A chat-list change: re-read the folded lists from the
                        // client and re-project the pane. The projection needs the
                        // client, so it lives here rather than in the pure `App` —
                        // which only receives the owned result.
                        AppEvent::Chats => {
                            let lists = client.read(|s| project_lists(s.chats()));
                            app.project_chats(lists);
                        }
                        // A message change in some chat: refresh the open chat's
                        // history (a no-op projection if nothing it shows changed).
                        AppEvent::Messages => project_conversation(&mut app, client, history.open),
                        // A dropped-update gap: re-project both panes to be safe.
                        AppEvent::Lagged => {
                            let lists = client.read(|s| project_lists(s.chats()));
                            app.project_chats(lists);
                            project_conversation(&mut app, client, history.open);
                        }
                        // Connection folds into the status bar; the rest repaint
                        // until their own projection lands.
                        other => {
                            let action = app.on_app_event(other);
                            app.dispatch(action);
                        }
                    }
                }
            }
            // A spawned history loader merged a page: clear its in-flight marker,
            // note an exhausted history, and re-project if it is the open chat.
            maybe_page = history_rx.recv() => {
                if let Some(page) = maybe_page {
                    history.loading.remove(&page.chat_id);
                    if page.reached_start {
                        history.exhausted.insert(page.chat_id);
                    }
                    if history.open == Some(page.chat_id) {
                        project_conversation(&mut app, client, history.open);
                    }
                }
            }
        }

        // A list switch may have moved onto a list we have not paged yet — load it.
        ensure_active_list_loaded(&app, client, &mut requested);
        // The user may have opened a different chat, or asked (scroll-up at the top)
        // for older history — drive the open chat's paging and projection.
        drive_open_chat(&mut app, client, &mut history, &history_tx);
        // With the open chat resolved and its history possibly just filled,
        // acknowledge its unread messages to Telegram (clears the unread badge).
        drive_read_state(client, &mut history);
    }

    Ok(())
}

/// A merged history page reported back to the loop by a spawned loader (#114):
/// which chat it was for, and whether it came back empty (the start of history was
/// reached, so no older page exists).
struct HistoryPage {
    chat_id: i64,
    reached_start: bool,
}

/// The open chat's history paging state, threaded through the loop (#114).
#[derive(Default)]
struct HistoryState {
    /// The chat currently shown in the conversation pane — the chat-list selection
    /// while the history pane is focused — or `None` while browsing the list.
    open: Option<i64>,
    /// Chats whose first (newest) page has been requested this run, so opening a
    /// chat fetches its landing page once.
    first_paged: HashSet<i64>,
    /// Chats with a page request in flight, so scroll-up never stacks overlapping
    /// loads onto one chat.
    loading: HashSet<i64>,
    /// Chats whose start-of-history was reached, so we stop paging older.
    exhausted: HashSet<i64>,
    /// Per-chat high-water mark of the newest message id already acknowledged as
    /// read (#115). A re-projection or render tick fires many times before the
    /// resulting `updateChatReadInbox` lands, so this de-dupes the `view_messages`
    /// call to one per new horizon — without it the loop would re-send the same
    /// view on every frame until the fold caught up.
    read_through: HashMap<i64, i64>,
}

/// The chat to show in the conversation pane: the chat-list selection while the
/// history pane is focused (Enter moves focus there to open it). `None` while the
/// user is browsing the list, so paging through chats does not eagerly load every
/// one — only the chat the user actually opens.
fn open_chat_id(app: &App) -> Option<i64> {
    if app.focus() == Focus::History {
        app.chat_list().selected_chat().map(|chat| chat.id)
    } else {
        None
    }
}

/// Drive the open chat's history (#114): project it when the user opens a different
/// chat (kicking off its landing page once), and service a scroll-up-at-the-top
/// request by fetching the next older page. Both loads run off `Arc<Client>` clones
/// so the network round-trips never block the loop.
fn drive_open_chat(
    app: &mut App,
    client: &Arc<Client>,
    history: &mut HistoryState,
    history_tx: &mpsc::Sender<HistoryPage>,
) {
    let open = open_chat_id(app);
    if open != history.open {
        history.open = open;
        if let Some(chat_id) = open {
            // Project whatever the store already holds (possibly empty, then filled
            // as the landing page lands), and fetch that page once per chat per run.
            project_conversation(app, client, Some(chat_id));
            if history.first_paged.insert(chat_id) {
                history.loading.insert(chat_id);
                spawn_history_page(client, chat_id, NEWEST, history_tx.clone());
            }
        }
    }

    // A scroll-up at the very top asks for older history: page from the oldest
    // loaded message, unless this chat is already loading or fully paged.
    if app.take_wants_older_history()
        && let Some(chat_id) = history.open
        && !history.loading.contains(&chat_id)
        && !history.exhausted.contains(&chat_id)
    {
        let anchor = client
            .read(|s| s.messages().history(chat_id).first().map(|m| m.id))
            .unwrap_or(NEWEST);
        history.loading.insert(chat_id);
        spawn_history_page(client, chat_id, anchor, history_tx.clone());
    }
}

/// Acknowledge the open chat's unread messages to Telegram (#115). When a chat is
/// open and its newest loaded incoming message is newer than the chat's read
/// horizon, send the unread ids through [`ReadRequests::view_messages`]
/// (`force_read`, the `ChatHistory` source): TDLib advances the read marker and
/// replies with `updateChatReadInbox`, which the chat store folds and the loop
/// re-projects, clearing the unread badge here and on the user's other clients.
///
/// Two things bound the traffic: `read_through` de-dupes so one `view_messages`
/// fires per new horizon (not once per frame), and the open gate is the focused
/// history pane — browsing the list never marks a chat read. The send is advisory
/// and fire-and-forget, matching the seam's contract: the read path never waits on
/// it, and a failed view simply leaves the chat unread until a newer message (or a
/// later open) re-triggers.
fn drive_read_state(client: &Arc<Client>, history: &mut HistoryState) {
    let Some(chat_id) = history.open else { return };
    // The chat's server read horizon, and the loaded incoming messages past it
    // (oldest first, since the store is keyed by ascending id).
    let unread: Vec<i64> = client.read(|s| {
        let last_read = s
            .chats()
            .get(chat_id)
            .map_or(0, |chat| chat.last_read_inbox_message_id);
        s.messages()
            .history(chat_id)
            .into_iter()
            .filter(|message| !message.is_outgoing && message.id > last_read)
            .map(|message| message.id)
            .collect()
    });
    // Nothing unread loaded, or already acknowledged up to the newest: no view.
    let Some(&newest) = unread.last() else { return };
    if newest <= history.read_through.get(&chat_id).copied().unwrap_or(0) {
        return;
    }
    history.read_through.insert(chat_id, newest);
    let client = Arc::clone(client);
    tokio::spawn(async move {
        let _ = client.bridge().view_messages(chat_id, unread).await;
    });
}

/// Read the open chat's folded history and pinned ids back from the `Client` and
/// project them onto the conversation pane (#114). The projection needs the client,
/// so it lives here rather than in the pure `App`, which only receives the owned
/// snapshot. A `None` open chat (the user is browsing the list) is a no-op.
fn project_conversation(app: &mut App, client: &Arc<Client>, open: Option<i64>) {
    let Some(chat_id) = open else { return };
    let (messages, pinned) = client.read(|s| {
        let messages = s.messages().history(chat_id).into_iter().cloned().collect();
        let pinned = s
            .chats()
            .get(chat_id)
            .map(|chat| chat.pinned_message_ids.iter().copied().collect())
            .unwrap_or_default();
        (messages, pinned)
    });
    app.project_conversation(chat_id, messages, pinned);
}

/// Fetch one history page for `chat_id` older than `anchor` ([`NEWEST`] for the
/// landing page), merge it into the store, and report completion to the loop
/// (#114). Runs on an `Arc<Client>` clone so it never blocks the loop; on error it
/// still reports back (not exhausted) so a later scroll-up can retry.
fn spawn_history_page(
    client: &Arc<Client>,
    chat_id: i64,
    anchor: i64,
    history_tx: mpsc::Sender<HistoryPage>,
) {
    let client = Arc::clone(client);
    tokio::spawn(async move {
        let reached_start = match client
            .bridge()
            .get_chat_history(chat_id, anchor, HISTORY_PAGE)
            .await
        {
            Ok(page) => {
                let empty = page.is_empty();
                if !empty {
                    client.merge_history(page);
                }
                empty
            }
            // Treat a failed page as "more may exist": clear in-flight, allow retry.
            Err(_) => false,
        };
        let _ = history_tx
            .send(HistoryPage {
                chat_id,
                reached_start,
            })
            .await;
    });
}

/// Page the active chat list to exhaustion if it has not been requested yet this
/// run (#113), spawning the load so the network round-trips never block the loop.
///
/// "On demand": Main is loaded at startup (the landing list), each other list the
/// first time the user switches onto it. The chats arrive as updates the router
/// folds and the loop re-projects; a failed load (network, or the session closing
/// mid-page) only leaves that list unfilled until a later run.
fn ensure_active_list_loaded(app: &App, client: &Arc<Client>, requested: &mut Vec<ChatListKind>) {
    let kind = app.chat_list().active_kind().clone();
    if requested.contains(&kind) {
        return;
    }
    requested.push(kind.clone());
    let client = Arc::clone(client);
    tokio::spawn(async move {
        let bridge = client.bridge();
        let _ = match kind {
            ChatListKind::Main => load_main_list(bridge, CHAT_PAGE).await,
            ChatListKind::Archive => load_archive_list(bridge, CHAT_PAGE).await,
            ChatListKind::Folder(id) => load_folder_list(bridge, id, CHAT_PAGE).await,
        };
    });
}
