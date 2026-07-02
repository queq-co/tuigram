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
//! so the unread badge clears here and on the user's other clients. A composer
//! submit becomes a real send, reply, or edit into the open chat through the
//! send/edit seam (#116); the optimistic message and its delivery resolution arrive
//! back as updates the loop re-projects. A submitted search query runs against the
//! core search seam (#117) — in-chat when a chat is open, global while browsing —
//! and its hits fill the overlay; opening a hit jumps to its chat and scrolls to the
//! message when it is loaded. Forwarding a hit copies its message into the picked
//! target chat through the forward seam (#118), the copies arriving back as updates
//! like a normal send. `main` closes TDLib
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

use tuigram_core::model::{ChatKind, ChatListKind, Message};
use tuigram_core::{
    Client, DOWNLOAD_PRIORITY, EditRequests, FileRequests, FormattedText, ForwardRequests,
    HistoryRequests, NEWEST, PinRequests, ReactionRequests, ReadRequests, SecretChatRequests,
    SendRequests, StorageRequests, StorageSettings, load_archive_list, load_folder_list,
    load_main_list, search_chat, search_global,
};

use crate::app::{Action, App};
use crate::chat_list::{project_lists, project_secret_states};
use crate::composer::Submission;
use crate::event::{AppEvent, spawn_core_source};
use crate::keymap::Focus;
use crate::login::{LoginEnd, run_login};
use crate::search::SearchHit;
use crate::status::Notice;
use crate::terminal::{TerminalGuard, install_panic_hook};

/// Render cadence cap (~30 FPS). Bounds repaint rate independently of network
/// latency, so the UI stays smooth while core is mid-request.
const FRAME: Duration = Duration::from_millis(33);

/// How often a showing toast is aged (#139). A `Notice`'s lifetime is counted in
/// these ~1s heartbeats, so this is the clock that expires toasts. Independent of
/// the faster [`FRAME`] render cadence: a toast should tick down on a human-readable
/// second, not once per repaint.
const NOTICE_TICK: Duration = Duration::from_secs(1);

/// How often the download-cache retention sweep runs (#120). Retention is not
/// time-critical — expiring a file minutes late is harmless — so a slow cadence
/// keeps the maintenance out of the way; each pass re-reads the loaded chats, so
/// coverage widens as the user browses. The first tick fires at startup, when few
/// chats are loaded, so the first effective sweep is roughly one interval in.
const STORAGE_SWEEP_INTERVAL: Duration = Duration::from_secs(30 * 60);

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

/// Depth of the send → loop completion channel (#116). A composer submit spawns a
/// fire-and-forget send/edit; only a seam-level rejection reports back (as an error
/// toast), and those are rare and coalesced through the toast queue, so a shallow
/// channel suffices.
const OUTBOUND_CHANNEL_DEPTH: usize = 16;

/// How many hits to request per page when running a search (#117). The core pagers
/// (`search_chat`/`search_global`) loop to exhaustion at this granularity, so this
/// only sizes each batch.
const SEARCH_PAGE: i32 = 50;

/// Depth of the search → loop completion channel (#117). A submit spawns one search
/// that delivers a single projected result set when it finishes, so a shallow
/// channel suffices.
const SEARCH_CHANNEL_DEPTH: usize = 8;

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
    // Ages a showing toast once a second (#139), independent of the render tick, so
    // notices actually time out; a still-counting toast is repainted by the render
    // tick meanwhile.
    let mut notice_tick = tokio::time::interval(NOTICE_TICK);
    let mut core_rx = spawn_core_source(client);
    // Download-cache retention policy (#120), read once from settings.toml; the
    // periodic sweep applies it. Absent/malformed settings default to keep-forever,
    // so retention is off unless the user opts in. On first run write a default file so
    // there is something to edit (#145) — best-effort, and never over an existing file.
    StorageSettings::ensure_default_file();
    let storage_settings = StorageSettings::load();
    let mut sweep_tick = tokio::time::interval(STORAGE_SWEEP_INTERVAL);
    // The lists whose `loadChats` paging has been kicked off, so each is loaded
    // at most once per run. A handful of entries (Main, Archive, a few folders),
    // so a `Vec` lookup beats pulling `Hash` onto the core enum.
    let mut requested: Vec<ChatListKind> = Vec::new();
    // The open chat's history paging state (#114) and the channel its loaders nudge
    // when a page is merged (a history page returns data directly, not as updates,
    // so it needs an explicit completion signal).
    let mut history = HistoryState::default();
    // The media file ids whose download has been kicked off this run (#120), so each
    // incoming attachment is requested at most once — `updateFile` then streams its
    // progress and the projection reflects it, without the loop re-requesting on
    // every re-projection.
    let mut downloading: HashSet<i32> = HashSet::new();
    let (history_tx, mut history_rx) = mpsc::channel::<HistoryPage>(HISTORY_CHANNEL_DEPTH);
    // A spawned send/edit (#116) reports a seam-level rejection back here as a toast;
    // the loop surfaces it through the notification queue.
    let (outbound_tx, mut outbound_rx) = mpsc::channel::<Notice>(OUTBOUND_CHANNEL_DEPTH);
    // A spawned search (#117) reports its projected hits back here; the loop feeds
    // them into the search overlay. (A failed search reuses `outbound_tx`'s toast.)
    let (search_tx, mut search_rx) = mpsc::channel::<Vec<SearchHit>>(SEARCH_CHANNEL_DEPTH);

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
            // Notice tick (#139): age the showing toast so it eventually times out.
            _ = notice_tick.tick() => app.tick_notices(),
            // Retention sweep (#120): expire old downloaded media per the settings.
            // Purely a background side effect — no UI state changes here.
            _ = sweep_tick.tick() => drive_storage_sweep(client, &storage_settings),
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
                            // A new secret chat arrives as updateNewChat; re-project
                            // its lifecycle state so the fresh row shows it (#121).
                            reproject_secret_states(&mut app, client);
                        }
                        // A message change in some chat: refresh the open chat's
                        // history (a no-op projection if nothing it shows changed).
                        AppEvent::Messages => project_conversation(&mut app, client, history.open),
                        // A file transfer advanced (#120): re-project so the open
                        // chat's download-progress lines reflect the newest `updateFile`.
                        AppEvent::File => project_conversation(&mut app, client, history.open),
                        // A secret chat's lifecycle advanced (#121): re-project the
                        // secret-state map so the row reflects pending → ready → closed.
                        AppEvent::Secret => reproject_secret_states(&mut app, client),
                        // A dropped-update gap: re-project both panes to be safe.
                        AppEvent::Lagged => {
                            let lists = client.read(|s| project_lists(s.chats()));
                            app.project_chats(lists);
                            reproject_secret_states(&mut app, client);
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
            // A spawned send/edit was rejected at the seam (#116): float the toast.
            // (A send that reached `Pending` resolves in the conversation instead,
            // through the optimistic message's `Sent`/`Failed` fold.)
            maybe_notice = outbound_rx.recv() => {
                if let Some(notice) = maybe_notice {
                    app.notify(notice);
                }
            }
            // A spawned search finished (#117): fill the overlay with its hits.
            maybe_hits = search_rx.recv() => {
                if let Some(hits) = maybe_hits {
                    app.set_search_results(hits);
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
        // A composer submit becomes a real send/reply/edit into the open chat (#116).
        drive_outbound(&mut app, client, &history, &outbound_tx);
        // A submitted search query runs against core, in-chat or global by context (#117).
        drive_search(&mut app, client, &history, &search_tx, &outbound_tx);
        // A confirmed forward copies its messages into the picked target chat (#118).
        drive_forward(&mut app, client, &outbound_tx);
        // A confirmed reaction/pin toggle hits Telegram; the real update reconciles
        // the optimistic state the reducer already reflected (#119).
        drive_reaction(&mut app, client, &outbound_tx);
        drive_pin(&mut app, client, &outbound_tx);
        // A confirmed attachment uploads into the open chat; its file streams back
        // through the store like a text send (#120).
        drive_media(&mut app, client, &history, &outbound_tx);
        // A confirmed secret-chat lifecycle action hits Telegram; the resulting
        // `updateSecretChat`/`updateNewChat` fold back and re-project (#121).
        drive_secret(&mut app, client, &outbound_tx);
        // Pull down the open chat's incoming media, each file once, so the progress
        // lines and saved markers resolve as `updateFile` folds (#120).
        drive_downloads(client, &history, &mut downloading);
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
/// history pane (or its composer) is focused. Enter moves focus to the history to
/// open a chat; tabbing on into the composer keeps that chat open, since the
/// composer belongs to the open conversation — so typing or sending (#116) never
/// "closes" the chat, and its history keeps re-projecting and its unread messages
/// keep settling (#115) while the user composes. `None` while browsing the list, so
/// paging through chats does not eagerly load every one — only the chat the user
/// actually opens.
fn open_chat_id(app: &App) -> Option<i64> {
    if matches!(app.focus(), Focus::History | Focus::Composer) {
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

/// Dispatch a submitted composer buffer to Telegram (#116). `App` records the
/// submission as a pure intent; here the loop pairs it with the open chat and routes
/// it to the matching seam — a new message or reply through
/// [`SendRequests::send_text`], an edit through [`EditRequests::edit_text`].
///
/// The send is fire-and-forget, like the read path (#115): TDLib streams the
/// optimistic `Pending` message (and later its `Sent`/`Failed` resolution) as
/// updates the router folds and the loop re-projects, so the composer's feedback
/// arrives through the normal pipeline rather than this call's return. Only a
/// seam-level rejection — the request never reaching `Pending` — reports back, as an
/// error toast on `outbound_tx`. With no chat open (an empty conversation) there is
/// nowhere to send, so the submission is dropped.
fn drive_outbound(
    app: &mut App,
    client: &Arc<Client>,
    history: &HistoryState,
    outbound_tx: &mpsc::Sender<Notice>,
) {
    let Some(submission) = app.take_outbound() else {
        return;
    };
    let Some(chat_id) = history.open else { return };
    // The toast names the failed action; an edit is reported as "edit", the rest
    // (new message, reply) as "send".
    let action = match submission {
        Submission::Edit { .. } => "edit",
        _ => "send",
    };
    let client = Arc::clone(client);
    let outbound_tx = outbound_tx.clone();
    tokio::spawn(async move {
        let result = match submission {
            Submission::Send { text } => client
                .bridge()
                .send_text(chat_id, None, plain_text(text))
                .await
                .map(|_| ()),
            Submission::Reply { reply_to, text } => client
                .bridge()
                .send_text(chat_id, Some(reply_to), plain_text(text))
                .await
                .map(|_| ()),
            Submission::Edit { message_id, text } => client
                .bridge()
                .edit_text(chat_id, message_id, plain_text(text))
                .await
                .map(|_| ()),
        };
        if let Err(err) = result {
            // The TDLib message is a fixed error code (e.g. CHAT_WRITE_FORBIDDEN),
            // never the user's typed text — safe to show; `from_core_error`
            // normalizes it to a readable phrase (#122).
            let _ = outbound_tx
                .send(Notice::from_core_error(action, &err.message))
                .await;
        }
    });
}

/// A plain [`FormattedText`] (no formatting entities) for a composer send or edit
/// (#116). The composer is a single-line plain-text input today; rich entities
/// arrive with a later formatting pass.
fn plain_text(text: String) -> FormattedText {
    FormattedText {
        text,
        entities: Vec::new(),
    }
}

/// Run a submitted search query against core (#117). `App` records the query as a
/// pure intent; here the loop drains it, picks the scope from the open chat —
/// [`search_chat`] in the chat the user has open, [`search_global`] while browsing
/// the list — and spawns the search off an `Arc<Client>` clone so the round-trips
/// never block the loop.
///
/// On success the spawned task projects each hit into a [`SearchHit`] (reading the
/// chat title back from the folded store for the preview) and sends the result set
/// on `search_tx`, which the loop drains into the overlay. A failed search reuses
/// the `outbound_tx` toast path (#116) to surface an error naming the action. Both
/// pagers run to exhaustion, the search counterpart to a full history load.
fn drive_search(
    app: &mut App,
    client: &Arc<Client>,
    history: &HistoryState,
    search_tx: &mpsc::Sender<Vec<SearchHit>>,
    outbound_tx: &mpsc::Sender<Notice>,
) {
    let Some(query) = app.take_search_query() else {
        return;
    };
    let scope = history.open;
    let client = Arc::clone(client);
    let search_tx = search_tx.clone();
    let outbound_tx = outbound_tx.clone();
    tokio::spawn(async move {
        let results = match scope {
            Some(chat_id) => search_chat(client.bridge(), chat_id, query, None, SEARCH_PAGE).await,
            None => search_global(client.bridge(), query, SEARCH_PAGE).await,
        };
        match results {
            Ok(results) => {
                // Project each hit with its chat title, read back from the folded
                // store; an unknown chat (not folded yet) drops the title prefix.
                let hits = client.read(|state| {
                    results
                        .messages()
                        .iter()
                        .map(|message| {
                            let title = state
                                .chats()
                                .get(message.chat_id)
                                .map_or("", |chat| chat.title.as_str());
                            SearchHit::from_message(message, title)
                        })
                        .collect()
                });
                let _ = search_tx.send(hits).await;
            }
            // The TDLib message is a fixed error code, never the user's query — safe
            // to show; `from_core_error` normalizes it to a readable phrase (#122).
            Err(err) => {
                let _ = outbound_tx
                    .send(Notice::from_core_error("search", &err.message))
                    .await;
            }
        }
    });
}

/// Dispatch a confirmed forward to Telegram (#118). `App` records the picked source,
/// messages, and target as a pure [`ForwardIntent`](crate::forward::ForwardIntent);
/// here the loop drains it and calls
/// [`ForwardRequests::forward_messages`], copying the messages into the target chat.
///
/// The forward keeps the usual "forwarded from" attribution (`send_copy` false, so no
/// caption to strip either) — an MVP forward, not a copy-as-own. Like the send path
/// (#116) it is fire-and-forget: TDLib streams the optimistic `Pending` copies (and
/// their `Sent`/`Failed` resolution) into the target chat as updates the router folds,
/// so the forward surfaces through the normal projection pipeline rather than this
/// call's return. Only a seam-level rejection reports back, as an error toast on
/// `outbound_tx`.
fn drive_forward(app: &mut App, client: &Arc<Client>, outbound_tx: &mpsc::Sender<Notice>) {
    let Some(intent) = app.take_forward() else {
        return;
    };
    let client = Arc::clone(client);
    let outbound_tx = outbound_tx.clone();
    tokio::spawn(async move {
        // A normal forward: keep attribution (`send_copy` false), so `remove_caption`
        // is moot and also false.
        let result = client
            .bridge()
            .forward_messages(
                intent.from_chat_id,
                intent.message_ids,
                intent.to_chat_id,
                false,
                false,
            )
            .await;
        if let Err(err) = result {
            // The TDLib message is a fixed error code (e.g. CHAT_FORWARDS_RESTRICTED),
            // never user content — safe to show; `from_core_error` normalizes it to a
            // readable phrase (#122).
            let _ = outbound_tx
                .send(Notice::from_core_error("forward", &err.message))
                .await;
        }
    });
}

/// Dispatch a confirmed reaction toggle to Telegram (#119). `App` reflects the
/// toggle optimistically and records a pure [`ReactionIntent`](crate::reactions::ReactionIntent);
/// here the loop drains it and calls [`ReactionRequests`]' add or remove per the
/// intent's `add` flag.
///
/// The call is advisory and fire-and-forget: the reaction is acknowledged to the
/// server and the authoritative counts arrive as `updateMessageInteractionInfo`,
/// which the router folds and the next projection reconciles over the optimistic
/// chips. Only a seam-level rejection reports back, as an error toast on
/// `outbound_tx`.
fn drive_reaction(app: &mut App, client: &Arc<Client>, outbound_tx: &mpsc::Sender<Notice>) {
    let Some(intent) = app.take_reaction() else {
        return;
    };
    let client = Arc::clone(client);
    let outbound_tx = outbound_tx.clone();
    tokio::spawn(async move {
        let bridge = client.bridge();
        let result = if intent.add {
            bridge
                .add_message_reaction(intent.chat_id, intent.message_id, intent.emoji)
                .await
        } else {
            bridge
                .remove_message_reaction(intent.chat_id, intent.message_id, intent.emoji)
                .await
        };
        if let Err(err) = result {
            // A fixed TDLib error code, normalized to a readable phrase (#122).
            let _ = outbound_tx
                .send(Notice::from_core_error("reaction", &err.message))
                .await;
        }
    });
}

/// Dispatch a confirmed pin toggle to Telegram (#119). `App` flips the chat's pinned
/// set optimistically and records a pure [`PinIntent`](crate::conversation::PinIntent);
/// here the loop drains it and calls [`PinRequests`]' pin or unpin per the intent's
/// `pin` flag.
///
/// A plain pin: not silent and shared with the chat (`disable_notification` and
/// `only_for_self` both false). Fire-and-forget like the reaction path — the real
/// `updateMessageIsPinned` folds the chat's pinned set, which the next projection
/// reconciles over the optimistic 📌. Only a seam-level rejection reports back, as an
/// error toast on `outbound_tx`.
fn drive_pin(app: &mut App, client: &Arc<Client>, outbound_tx: &mpsc::Sender<Notice>) {
    let Some(intent) = app.take_pin() else {
        return;
    };
    let client = Arc::clone(client);
    let outbound_tx = outbound_tx.clone();
    tokio::spawn(async move {
        let bridge = client.bridge();
        let result = if intent.pin {
            bridge
                .pin_chat_message(intent.chat_id, intent.message_id, false, false)
                .await
        } else {
            bridge
                .unpin_chat_message(intent.chat_id, intent.message_id)
                .await
        };
        if let Err(err) = result {
            // A fixed TDLib error code, normalized to a readable phrase (#122).
            let _ = outbound_tx
                .send(Notice::from_core_error("pin", &err.message))
                .await;
        }
    });
}

/// Dispatch a confirmed attachment to Telegram (#120). `App` builds the
/// [`OutgoingMedia`] from the attach prompt and records it; here the loop drains it
/// and calls [`SendRequests::send_media`], uploading the local file into the open
/// chat.
///
/// Fire-and-forget like the text send (#116): TDLib returns an optimistic `Pending`
/// message immediately, streams the upload as `updateFile` (folded by the file
/// store, so the progress line moves), and settles the send via
/// `updateMessageSendSucceeded`/`Failed` — all arriving back as updates the loop
/// re-projects. The media is sent to the open chat (`history.open`), the same chat
/// the composer targets; with no chat open there is nothing to attach to, so the
/// drained intent is dropped. Only a seam-level rejection reports back, as an error
/// toast on `outbound_tx`.
fn drive_media(
    app: &mut App,
    client: &Arc<Client>,
    history: &HistoryState,
    outbound_tx: &mpsc::Sender<Notice>,
) {
    let Some(media) = app.take_media() else {
        return;
    };
    let Some(chat_id) = history.open else { return };
    let client = Arc::clone(client);
    let outbound_tx = outbound_tx.clone();
    tokio::spawn(async move {
        // A plain attachment, not a reply (`reply_to` None) — the attach prompt
        // carries no reply target.
        if let Err(err) = client.bridge().send_media(chat_id, None, media).await {
            // The TDLib message is a fixed error code (e.g. CHAT_WRITE_FORBIDDEN),
            // never the local path or caption — safe to show; `from_core_error`
            // normalizes it to a readable phrase (#122).
            let _ = outbound_tx
                .send(Notice::from_core_error("send", &err.message))
                .await;
        }
    });
}

/// Dispatch a confirmed secret-chat lifecycle action to Telegram (#121). Confirming
/// the secret-chat prompt records a pure [`SecretLifecycle`](crate::secret::SecretLifecycle)
/// on `App`; here the loop drains it and calls [`SecretChatRequests`]' create or
/// close per the action.
///
/// Fire-and-forget like the reaction/pin toggles: the authoritative state arrives as
/// `updateSecretChat` (the lifecycle advance) and, for a create, `updateNewChat` (the
/// new chat) — both folded by the router and reflected on the next projection, so this
/// only issues the request. `create_new_secret_chat` also returns the new [`Chat`],
/// but the fold is the source of truth, so the returned copy is dropped. Only a
/// seam-level rejection reports back, as an error toast on `outbound_tx`.
fn drive_secret(app: &mut App, client: &Arc<Client>, outbound_tx: &mpsc::Sender<Notice>) {
    let Some(lifecycle) = app.take_secret() else {
        return;
    };
    let client = Arc::clone(client);
    let outbound_tx = outbound_tx.clone();
    tokio::spawn(async move {
        use crate::secret::SecretLifecycle;
        let bridge = client.bridge();
        let result = match lifecycle {
            SecretLifecycle::Start { user_id } => {
                bridge.create_new_secret_chat(user_id).await.map(|_| ())
            }
            SecretLifecycle::Close { secret_chat_id } => {
                bridge.close_secret_chat(secret_chat_id).await
            }
        };
        if let Err(err) = result {
            // A fixed TDLib error code (e.g. USER_NOT_FOUND), never key material or
            // user input — safe to show; `from_core_error` normalizes it to a
            // readable phrase (#122).
            let _ = outbound_tx
                .send(Notice::from_core_error("secret chat", &err.message))
                .await;
        }
    });
}

/// Re-read the folded secret-chat states from the client and re-project them onto
/// the chat list (#121). The projection needs the client, so it lives here rather
/// than in the pure `App` — which only receives the owned pairs — the same split as
/// [`project_lists`]. Joins each [`ChatKind::Secret`] chat to its
/// [`SecretChatStore`](tuigram_core::SecretChatStore) record by `secret_chat_id`.
fn reproject_secret_states(app: &mut App, client: &Arc<Client>) {
    let states = client.read(|s| project_secret_states(s.chats(), s.secret_chats()));
    app.project_secret_states(states);
}

/// Start downloading the open chat's incoming media (#120), each file at most once
/// per run. Reads the file every message in the open chat's history references back
/// from the store; a file that is neither present nor already transferring, and not
/// requested yet this run, is downloaded at [`DOWNLOAD_PRIORITY`] and its id recorded
/// in `downloading` so a later re-projection never re-requests it.
///
/// The download runs asynchronously: TDLib streams progress as `updateFile`, folded
/// by the store and re-projected onto the conversation's progress line (via
/// [`AppEvent::File`]), so this only starts the transfer and never awaits it. A file
/// the store has not folded yet is skipped this pass and picked up once its first
/// `updateFile` lands. The dedup is per-run, the download counterpart to the
/// once-per-run list paging (`ensure_active_list_loaded`); a start rejected at the
/// seam is not retried until the next run. With no chat open there is nothing to
/// fetch.
fn drive_downloads(client: &Arc<Client>, history: &HistoryState, downloading: &mut HashSet<i32>) {
    let Some(chat_id) = history.open else { return };
    // The ids to start: files the history references that the store knows, are not
    // yet present or active, and have not been requested this run. Photos with no
    // sizes carry a 0 ref, which is not downloadable — skip it.
    let to_start: Vec<i32> = client.read(|s| {
        s.messages()
            .history(chat_id)
            .into_iter()
            .filter_map(|m| m.content.file())
            .filter(|file| file.id != 0 && !downloading.contains(&file.id))
            .filter_map(|file| s.files().get(file.id))
            .filter(|file| !file.is_present() && !file.is_downloading_active)
            .map(|file| file.id)
            .collect()
    });
    for file_id in to_start {
        downloading.insert(file_id);
        let client = Arc::clone(client);
        tokio::spawn(async move {
            let _ = client
                .bridge()
                .download_file(file_id, DOWNLOAD_PRIORITY)
                .await;
        });
    }
}

/// Loaded chat ids split by retention category (#120): one-to-one/secret chats,
/// groups (basic + super), and broadcast channels — the grouping the official apps'
/// per-kind "Keep Media" TTLs use.
#[derive(Default)]
struct RetentionGroups {
    private: Vec<i64>,
    groups: Vec<i64>,
    channels: Vec<i64>,
}

/// Split chats into [`RetentionGroups`] by [`ChatKind`]. Pure over the chats it is
/// given (the driver feeds it the store's loaded chats), so the mapping is unit-
/// testable without a `Client`: private and secret chats group together, basic groups
/// and supergroups as "groups", and channels on their own.
fn categorize_chats<'a>(
    chats: impl Iterator<Item = &'a tuigram_core::model::Chat>,
) -> RetentionGroups {
    let mut out = RetentionGroups::default();
    for chat in chats {
        match chat.kind {
            ChatKind::Private { .. } | ChatKind::Secret { .. } => out.private.push(chat.id),
            ChatKind::BasicGroup { .. } | ChatKind::Supergroup { .. } => out.groups.push(chat.id),
            ChatKind::Channel { .. } => out.channels.push(chat.id),
        }
    }
    out
}

/// Run the download-cache retention sweep (#120): expire downloaded media not
/// accessed within each chat kind's configured TTL. `App` is uninvolved — this is
/// background maintenance with no UI state.
///
/// Two complementary policies run here. The **per-kind TTL** sweeps group the loaded
/// chats by retention category — one-to-one/secret chats, groups (basic + super), and
/// channels — from whatever the chat store currently holds, and each category with a
/// finite TTL and at least one loaded chat gets one `optimizeStorage` scoped to its
/// chat ids. An **empty** category is skipped, never swept with an empty chat list:
/// `optimizeStorage` treats that as *all* chats, which would misapply one kind's TTL
/// globally. Kinds kept forever are skipped outright.
///
/// The TTL sweeps' coverage tracks what is loaded: files from chats the user has not
/// opened this session are not reached until those chats page in. So a **global size
/// backstop** (#138) runs alongside them when `max_cache` is set — one *unscoped*
/// `optimizeStorage` with a byte ceiling over every chat, bounding the total cache
/// regardless of which chats have loaded. Each spawned sweep is fire-and-forget — a
/// rejection is dropped and the next interval retries.
fn drive_storage_sweep(client: &Arc<Client>, settings: &StorageSettings) {
    // Nothing configured: no sweep, no chat read.
    if !settings.sweeps_anything() {
        return;
    }
    // Per-kind TTL sweeps: only touch the store when at least one kind has a finite
    // TTL — a cache-cap-only config sweeps globally below without reading any chats.
    let per_kind = [
        settings.keep_private,
        settings.keep_groups,
        settings.keep_channels,
    ];
    if per_kind.iter().any(|k| k.to_ttl_seconds().is_some()) {
        // Partition the loaded chats' ids by retention category in a single store read.
        let RetentionGroups {
            private,
            groups,
            channels,
        } = client.read(|s| categorize_chats(s.chats().iter()));

        for (keep, chat_ids) in [
            (settings.keep_private, private),
            (settings.keep_groups, groups),
            (settings.keep_channels, channels),
        ] {
            // Only sweep a kind with a finite TTL that actually has loaded chats — an
            // empty list would sweep everything, so it is skipped, not passed through.
            if let Some(ttl) = keep.to_ttl_seconds()
                && !chat_ids.is_empty()
            {
                let client = Arc::clone(client);
                tokio::spawn(async move {
                    let _ = client.bridge().sweep_chat_media(ttl, chat_ids).await;
                });
            }
        }
    }

    // Global size backstop: an unscoped byte ceiling over every chat, so media from
    // chats never opened this session is still bounded (#138). Independent of the
    // per-kind TTLs and needs no chat read — TDLib evicts least-recently-used first.
    if let Some(max_bytes) = settings.max_cache.to_size_bytes() {
        let client = Arc::clone(client);
        tokio::spawn(async move {
            let _ = client.bridge().sweep_cache_to_size(max_bytes).await;
        });
    }
}

/// Read the open chat's folded history and pinned ids back from the `Client` and
/// project them onto the conversation pane (#114). The projection needs the client,
/// so it lives here rather than in the pure `App`, which only receives the owned
/// snapshot. A `None` open chat (the user is browsing the list) is a no-op.
fn project_conversation(app: &mut App, client: &Arc<Client>, open: Option<i64>) {
    let Some(chat_id) = open else { return };
    let (messages, pinned, files) = client.read(|s| {
        let messages: Vec<Message> = s.messages().history(chat_id).into_iter().cloned().collect();
        let pinned = s
            .chats()
            .get(chat_id)
            .map(|chat| chat.pinned_message_ids.iter().copied().collect())
            .unwrap_or_default();
        // The download state of every file the history's media references, read back
        // from the file store so the progress lines project alongside the messages
        // (#120). A file the store has not folded yet is simply absent until it does.
        let files = messages
            .iter()
            .filter_map(|m| m.content.file())
            .filter_map(|file| s.files().get(file.id).cloned())
            .collect();
        (messages, pinned, files)
    });
    app.project_conversation(chat_id, messages, pinned);
    app.project_downloads(files);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat_list::sample_chat;
    use tuigram_core::model::{Chat, ChatKind};

    fn chat_of_kind(id: i64, kind: ChatKind) -> Chat {
        let mut chat = sample_chat(id, "c", 0);
        chat.kind = kind;
        chat
    }

    #[test]
    fn categorize_chats_groups_each_kind_and_pairs_private_with_secret() {
        let chats = [
            chat_of_kind(1, ChatKind::Private { user_id: 1 }),
            chat_of_kind(
                2,
                ChatKind::Secret {
                    secret_chat_id: 9,
                    user_id: 2,
                },
            ),
            chat_of_kind(3, ChatKind::BasicGroup { basic_group_id: 3 }),
            chat_of_kind(4, ChatKind::Supergroup { supergroup_id: 4 }),
            chat_of_kind(5, ChatKind::Channel { supergroup_id: 5 }),
        ];
        let mut groups = categorize_chats(chats.iter());
        // Order within a category follows iteration; sort for a stable assertion.
        groups.private.sort_unstable();
        groups.groups.sort_unstable();
        groups.channels.sort_unstable();
        assert_eq!(groups.private, vec![1, 2], "private + secret together");
        assert_eq!(groups.groups, vec![3, 4], "basic + super as groups");
        assert_eq!(groups.channels, vec![5]);
    }

    #[test]
    fn categorize_chats_on_no_chats_yields_empty_categories() {
        let groups = categorize_chats(std::iter::empty());
        assert!(groups.private.is_empty());
        assert!(groups.groups.is_empty());
        assert!(groups.channels.is_empty());
    }
}
