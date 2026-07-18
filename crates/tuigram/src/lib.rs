//! `tuigram` — a Ratatui Telegram client.
//!
//! [`run_app`] is the thin binary's entire entry point; everything else here is
//! the library the `tuigram` bin links against. Splitting it out (#183) gives
//! `benches/` a library target to link: `chat_list`, `conversation`, `ui`, and
//! `wrap` hold the view-model projections the benchmark suite times, so their
//! benched items are re-exported at the crate root below while every other
//! module stays private — the run loop's internals are not part of this
//! crate's public surface, only the four hot paths #183 measures are.
//!
//! This is the Phase 5 spine: an RAII terminal guard, a panic hook that restores
//! the terminal, and the single `tokio::select!` loop that races terminal input,
//! a render tick, and core events into `Action`s applied to one `App`. The
//! draw call stays on the main task and is never awaited inside. Real widgets and
//! live Telegram data arrive in later Phase 5/6 issues; the loop's shape does not
//! change when they do.
//!
//! Phase 6 stands the real [`tuigram_core::Client`] up across three phases. #109
//! bootstraps an *initialized* bridge on the plain terminal (`bootstrap`:
//! credentials, secure storage, `setTdlibParameters`). #111 then drives **login
//! inside the TUI** (`run_login`): one screen per waiting auth state, answered
//! through the core `Login` seam, gating the three-pane UI behind `Ready` — only
//! then does `main` hand the bridge to [`Client::start`]. The run loop is fed by
//! the live core source (#110): `spawn_core_source` forwards the client's update
//! stream onto the mpsc arm the fake heartbeat used, classified into
//! `AppEvent`s. On a chat signal the loop reads the
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
//! like a normal send. `main` closes `TDLib`
//! cleanly on every exit path, including a login the user quit before the facade
//! ever started.

mod app;
mod avatar;
mod bootstrap;
mod chat_list;
mod cli;
mod composer;
mod contact_picker;
mod conversation;
mod event;
mod forward;
mod keymap;
mod login;
mod mediaform;
// Test-only command-surface parity guard (#197): no runtime code, so it's
// compiled only for `cargo test`, avoiding a dead-code warning on the plain bin.
#[cfg(test)]
mod parity;
mod reactions;
mod richtext;
mod search;
mod secret;
mod settingsform;
mod status;
mod terminal;
mod textinput;
mod ui;
mod wrap;

// The #183 bench surface: exactly the items each benchmark calls, re-exported
// from otherwise-private modules so `private_interfaces` stays clean without
// making the whole run-loop internals (`App`, `composer`, etc.) part of this
// crate's public API.
pub use chat_list::project_lists;
pub use conversation::{ConversationView, SenderLabel};
pub use ui::message_lines;
pub use wrap::{Row, layout_rows};

use std::collections::{HashMap, HashSet};
use std::io;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::EventStream;
use ratatui::layout::Size;
use ratatui_image::Resize;
use ratatui_image::protocol::Protocol;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;

use tuigram_core::model::{
    ChatAction, ChatKind, ChatListKind, Message, MessageContent, Sender, User, UserKind,
};
use tuigram_core::{
    AuthRequests, ChatActionRequests, ChatLifecycleRequests, Client, ContactRequests,
    DOWNLOAD_PRIORITY, DeleteRequests, FileRequests, ForwardRequests, HistoryRequests,
    InterfaceSettings, NEWEST, PinRequests, ReactionRequests, ReadRequests, SecretChatRequests,
    SendRequests, StorageRequests, StorageSettings, UserRequests, edit_formatted_text,
    load_archive_list, load_folder_list, load_main_list, search_chat, search_global,
    send_formatted_text,
};

use crate::app::{Action, App};
use crate::chat_list::project_secret_states;
use crate::composer::Submission;
use crate::contact_picker::ContactHit;
use crate::conversation::sender_label_for;
use crate::event::{AppEvent, spawn_core_source};
use crate::keymap::Focus;
use crate::login::{LoginEnd, run_login};
use crate::search::SearchHit;
use crate::status::Notice;
use crate::terminal::{AvatarSupport, TerminalGuard, install_panic_hook};

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
const STORAGE_SWEEP_INTERVAL: Duration = Duration::from_mins(30);

/// How many chats to request per `loadChats` page when filling a list (#113).
/// The core pager loops a list to exhaustion at this granularity, so this only
/// sizes each batch — `TDLib` streams the chats back as updates the router folds.
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

/// How many contacts to request per `search_contacts` call (#197). A picker list,
/// not a paged history — one page is plenty for a name search.
const CONTACT_SEARCH_LIMIT: i32 = 50;

/// Depth of the contact-search → loop completion channel (#197). A submit spawns
/// one search that delivers a single resolved result set when it finishes, so a
/// shallow channel suffices, matching [`SEARCH_CHANNEL_DEPTH`].
const CONTACT_SEARCH_CHANNEL_DEPTH: usize = 8;

/// Depth of the avatar-encode → loop completion channel (#201). Each distinct
/// sender is encoded at most once per `AvatarCache` lifetime (or once more on
/// a decode/encode failure's retry), so a shallow channel suffices.
const AVATAR_CHANNEL_DEPTH: usize = 16;

/// Depth of the inline-media-encode → loop completion channel (#208). Same
/// shape as [`AVATAR_CHANNEL_DEPTH`]: each message's media is encoded at most
/// once per `MediaCache` lifetime (or once more on a decode/encode failure's
/// retry).
const MEDIA_CHANNEL_DEPTH: usize = 16;

/// How often the outbound typing action is re-broadcast while composing (#197).
/// TDLib/Telegram's `typing` action expires on its own a few seconds after the
/// last broadcast, so a real client refreshes it periodically rather than once;
/// this bounds how often a keystroke actually reaches the network, rather than
/// firing `sendChatAction` on every character.
const TYPING_RESEND: Duration = Duration::from_secs(4);

/// The bin's entire body (#183): `main.rs` is a one-line `ExitCode` forward to
/// this, so a `benches/` file can link the crate as a library without pulling
/// in a second, redundant compile of every module through a `mod` in the bin.
///
/// # Panics
///
/// Panics if `#[tokio::main]`'s generated runtime fails to build (this is the
/// former `fn main`'s body verbatim, aside from the rename; unexported `main`
/// carried the same runtime-construction panic without needing a doc section).
#[tokio::main]
#[must_use]
pub async fn run_app() -> ExitCode {
    // tokio-console (#185, `profile-console` feature): registers the runtime
    // with the console-subscriber aggregator before anything else spawns, so
    // task instrumentation covers the whole run, not just the TUI loop.
    #[cfg(feature = "profile-console")]
    console_subscriber::init();

    // argv check (#166): before any terminal-mode or TDLib work, so
    // `--version`/`--help`/an unknown argument exit cleanly with no TTY and no
    // `~/.config/tuigram/` access — required for packaging smoke tests and the
    // Homebrew formula's `test do` block.
    if let cli::Action::Exit(code) = cli::parse(std::env::args().skip(1)) {
        return code;
    }

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
    // Read the mouse toggle before entering the terminal so capture is enabled (or
    // not) from the first frame (#161). A missing/malformed settings file defaults
    // to mouse on; `StorageSettings::load` (in `run`) surfaces a parse warning, so
    // this stays quiet.
    let interface = InterfaceSettings::load();
    let mut guard = match TerminalGuard::new(interface.mouse) {
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
    let result = match Box::pin(run_login(&mut guard, &bridge)).await {
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
    // Seed once from what `TerminalGuard::new` already detected (#201); `guard`
    // keeps its own copy (needed nowhere else yet), so this is a clone, not a move.
    app.set_avatar_support(guard.avatar_support().clone());
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
    // Mutable so the in-app editor (#146) can swap the live policy without a restart;
    // the next sweep tick honours whatever it holds. Seed `App` with it too, so the
    // editor opens pre-filled with the values in effect.
    let mut storage_settings = StorageSettings::load();
    app.set_storage_settings(storage_settings);
    // The graphics toggle (#209): same "mutable local, live-swappable via the
    // in-app editor" shape as `storage_settings` above, read fresh here (rather
    // than reusing `main`'s pre-login `interface` read) since a user could edit
    // the file by hand between that read and here.
    let mut interface_settings = InterfaceSettings::load();
    app.set_graphics_enabled(interface_settings.graphics);
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
    // The user ids whose avatar photo is being decoded+encoded off the render
    // thread this run (#201), so a re-scan of the open chat never spawns a
    // second encode for the same sender while one is already in flight.
    let mut avatar_encoding: HashSet<i64> = HashSet::new();
    // The message ids whose inline media is being decoded+encoded off the
    // render thread this run (#208), same dedup shape as `avatar_encoding`.
    let mut media_encoding: HashSet<i64> = HashSet::new();
    let (history_tx, mut history_rx) = mpsc::channel::<HistoryPage>(HISTORY_CHANNEL_DEPTH);
    // A spawned send/edit (#116) reports a seam-level rejection back here as a toast;
    // the loop surfaces it through the notification queue.
    let (outbound_tx, mut outbound_rx) = mpsc::channel::<Notice>(OUTBOUND_CHANNEL_DEPTH);
    // A spawned search (#117) reports its projected hits back here; the loop feeds
    // them into the search overlay. (A failed search reuses `outbound_tx`'s toast.)
    let (search_tx, mut search_rx) = mpsc::channel::<Vec<SearchHit>>(SEARCH_CHANNEL_DEPTH);
    // A spawned contact search (#197) reports its resolved hits back here; the loop
    // feeds them into the contact-search overlay. (A failed search reuses
    // `outbound_tx`'s toast.)
    let (contact_tx, mut contact_rx) =
        mpsc::channel::<Vec<ContactHit>>(CONTACT_SEARCH_CHANNEL_DEPTH);
    // A spawned avatar encode (#201) reports the built protocol back here (or
    // `None` on a decode/encode failure); the loop caches it and clears the
    // sender's in-flight marker either way.
    let (avatar_tx, mut avatar_rx) = mpsc::channel::<(i64, Option<Protocol>)>(AVATAR_CHANNEL_DEPTH);
    // A spawned inline-media encode (#208) reports the built protocol back here
    // (or `None` on a decode/encode failure); the loop caches it and clears the
    // message's in-flight marker either way. No reproject needed on receipt —
    // see `drive_inline_media`'s doc for why.
    let (media_tx, mut media_rx) = mpsc::channel::<(i64, Option<Protocol>)>(MEDIA_CHANNEL_DEPTH);

    // Kick off the landing list (Main) before the first frame; the rest load on
    // demand as the user switches to them.
    ensure_active_list_loaded(&app, client, &mut requested);

    // The open chat and overlay as of the last drawn frame (#229), so a change in
    // either can be detected before the next `draw` — see `should_clear_for_graphics`.
    let mut last_open_chat: Option<i64> = None;
    let mut last_overlay = app.overlay();

    while !app.should_quit() {
        if app.is_dirty() {
            // UNVERIFIED mitigation (#229): reported as leftover/garbled
            // characters on a chat switch or overlay open. `Kitty::render`'s
            // cell-level `Skip` marking turned out NOT to be the mechanism — a
            // `TestBackend` regression test proved ratatui's own cell-diffing
            // (`Cell::eq` compares `diff_option`, so a cell reverting from
            // `Skip` to ordinary content is always detected as changed) already
            // repaints correctly for Kitty. What's actually documented upstream
            // is real-terminal-side: some Sixel and iTerm2 protocol
            // implementations don't reliably clear previously-painted pixels
            // even when ratatui rewrites the underlying cell, since those
            // protocols paint raw pixels with no cell-level linkage (unlike
            // Kitty's unicode-placeholder trick). A real clear (`ClearType::All`
            // + resetting ratatui's own back-buffer) is the standard workaround
            // for that class of issue, but isn't guaranteed on every terminal
            // (e.g. Contour+Sixel is reported not to clear even then) — this
            // is a best-effort mitigation pending real-terminal confirmation,
            // not a verified fix.
            //
            // Forced via `resize()` to the current size, not `Terminal::clear()`
            // (#234 hotfix): `clear()` snapshots the cursor position first via a
            // blocking DSR query (`ESC[6n`, crossterm's `cursor::position()`),
            // which reads the terminal's reply from stdin — the same stdin our
            // `EventStream` is concurrently draining in the background. Under
            // active input (scrolling generates a steady stream of key/mouse
            // events) that reader reliably wins the race and steals the DSR
            // reply, so `position()` times out and `clear()` returns a fatal
            // `io::Error` ("cursor position could not be read"), crashing the
            // whole app via `?`. `resize()` to an unchanged size performs the
            // exact same clear + back-buffer-reset ratatui's own resize path
            // does internally (see `clear_viewport`) but never touches cursor
            // position, so it can't race the event reader.
            if should_clear_for_graphics(
                app.graphics_active(),
                history.open != last_open_chat,
                app.overlay() != last_overlay,
            ) {
                let area = guard.terminal_mut().size()?.into();
                guard.terminal_mut().resize(area)?;
            }
            last_open_chat = history.open;
            last_overlay = app.overlay();

            // The draw reports the history pane's inner height; record it on the view
            // so an open/`G`/tail-follow can bottom-anchor against the real number of
            // visible rows (#158). A first measurement or a resize while following
            // re-anchors and re-dirties, so the corrected frame paints next iteration.
            let mut render_out = ui::RenderOutput::default();
            guard
                .terminal_mut()
                .draw(|frame| render_out = ui::ui(frame, &app))?;
            app.clear_dirty();
            app.set_conversation_viewport(render_out.convo_viewport);
            // Record the history pane's measured body width too (#214), so message
            // bodies wrap against the real column budget and a resize re-anchors.
            app.set_conversation_width(render_out.convo_width);
            // Record the pane rectangles this frame drew into, so a mouse event can
            // be hit-tested to a pane without re-running layout (#161/#162).
            app.set_pane_layout(render_out.panes);
            // Record the chat/message row maps this frame drew, so a mouse click on
            // an actual row can open the chat or select the message directly.
            app.set_chat_rows(render_out.chat_rows);
            app.set_history_rows(render_out.history_rows);
            // Record the open overlay's row map this frame drew, so a mouse click
            // on an actual overlay row can select-and-confirm it directly (#217).
            app.set_overlay_rows(render_out.overlay_rows);
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
                            reproject_chats(&mut app, client);
                            // A new secret chat arrives as updateNewChat; re-project
                            // its lifecycle state so the fresh row shows it (#121).
                            reproject_secret_states(&mut app, client);
                        }
                        // The peer read one of our messages (#163): everything
                        // `Chats` does, plus a conversation reproject so the open
                        // pane's read-receipt glyph (✓ → ✓✓) advances live — the one
                        // chat-list update the open pane itself depends on.
                        AppEvent::ChatReadOutbox => {
                            reproject_chats(&mut app, client);
                            reproject_secret_states(&mut app, client);
                            project_conversation(&mut app, client, history.open, false);
                        }
                        // A message change in some chat: refresh the open chat's
                        // history (a no-op projection if nothing it shows changed).
                        AppEvent::Messages => {
                            project_conversation(&mut app, client, history.open, false);
                        }
                        // A file transfer advanced (#120): re-project so the open
                        // chat's download-progress lines reflect the newest `updateFile` —
                        // but only when it could actually affect it (#276): `updateFile`
                        // fires for every in-flight transfer on the whole account, and
                        // most emissions in a busy media chat are for files elsewhere.
                        AppEvent::File(file_id) => {
                            drive_file_update(&mut app, client, history.open, file_id);
                        }
                        // A secret chat's lifecycle advanced (#121): re-project the
                        // secret-state map so the row reflects pending → ready → closed.
                        AppEvent::Secret => reproject_secret_states(&mut app, client),
                        // A dropped-update gap: re-project both panes to be safe. Same
                        // recovery as `ChatReadOutbox` today by coincidence, not by
                        // shared meaning — kept as a separate arm so the two can diverge
                        // independently.
                        #[allow(clippy::match_same_arms)]
                        AppEvent::Lagged => {
                            reproject_chats(&mut app, client);
                            reproject_secret_states(&mut app, client);
                            project_conversation(&mut app, client, history.open, false);
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
                        project_conversation(&mut app, client, history.open, false);
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
            // A spawned contact search finished (#197): fill the overlay with its hits.
            maybe_contacts = contact_rx.recv() => {
                if let Some(hits) = maybe_contacts {
                    app.set_contact_results(hits);
                }
            }
            // A spawned search finished (#117): fill the overlay with its hits.
            maybe_hits = search_rx.recv() => {
                if let Some(hits) = maybe_hits {
                    app.set_search_results(hits);
                }
            }
            // A spawned avatar encode finished (#201): clear its in-flight marker
            // and cache the protocol so the next frame draws it instead of a
            // blank gutter. `None` (a decode/encode failure) just clears the
            // marker — that sender keeps its blank gutter this run.
            maybe_avatar = avatar_rx.recv() => {
                if let Some((user_id, protocol)) = maybe_avatar {
                    avatar_encoding.remove(&user_id);
                    if let Some(protocol) = protocol {
                        app.cache_avatar(user_id, protocol);
                    }
                }
            }
            // A spawned inline-media encode finished (#208): clear its in-flight
            // marker and cache the protocol so the next frame draws it in the
            // space `message_height` already reserved. `None` (a decode/encode
            // failure) just clears the marker — that message keeps its
            // placeholder this run.
            maybe_media = media_rx.recv() => {
                if let Some((message_id, protocol)) = maybe_media {
                    media_encoding.remove(&message_id);
                    if let Some(protocol) = protocol {
                        app.cache_media(message_id, protocol);
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
        // A composer submit becomes a real send/reply/edit into the open chat (#116).
        drive_outbound(&mut app, client, &history, &outbound_tx);
        // A submitted search query runs against core, in-chat or global by context (#117).
        drive_search(&mut app, client, &history, &search_tx, &outbound_tx);
        // A submitted contact-search query resolves matching contacts by name (#197).
        drive_contact_search(&mut app, client, &contact_tx, &outbound_tx);
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
        // A confirmed retention edit swaps the live sweep policy and writes it back to
        // settings.toml, taking effect on the next sweep with no restart (#146).
        drive_settings(&mut app, &mut storage_settings);
        // A confirmed graphics toggle already took effect in-memory (the reducer
        // applies it on confirm); this only persists it to settings.toml (#209).
        drive_graphics_setting(&mut app, &mut interface_settings);
        // Pull down the open chat's incoming media, each file once, so the progress
        // lines and saved markers resolve as `updateFile` folds (#120).
        drive_downloads(client, &history, &mut downloading);
        // Encode the open chat's sender avatars, each user once, so the gutter
        // fills in as `Picker::new_protocol` finishes off the render thread (#201).
        drive_avatars(&app, client, &history, &mut avatar_encoding, &avatar_tx);
        // Encode the open chat's ready inline media, each message once, so the
        // media box fills in as `Picker::new_protocol` finishes off the render
        // thread (#208).
        drive_inline_media(&app, client, &history, &mut media_encoding, &media_tx);
        // A confirmed delete removes the message for us or everyone; the real
        // `updateDeleteMessages` folds and re-projects the history (#195).
        drive_delete(&mut app, client, &outbound_tx);
        // A save request reveals the media's local path (already downloaded) or
        // starts its download (#195).
        drive_save(&mut app, client);
        // A copy request writes the selected message's text to the OS clipboard (#197).
        drive_copy(&mut app);
        // Unsent composer text broadcasts a typing action for the open chat,
        // throttled so it isn't refired on every keystroke (#197).
        drive_typing(&mut app, client, &mut history);
        // A resync re-queries the chat list after a dropped-update gap (#195).
        drive_resync(&mut app, client, &outbound_tx);
        // A confirmed logout ends the session and quits; awaited (not spawned)
        // since the whole session is going away and the exit waits on it (#195).
        drive_logout(&mut app, client).await;
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
    /// Per-chat timestamp of the last outbound typing broadcast (#197), throttling
    /// [`drive_typing`] to [`TYPING_RESEND`] instead of firing on every keystroke.
    typing_sent: HashMap<i64, std::time::Instant>,
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
        let previous = history.open;
        history.open = open;
        // Tell TDLib the previously open chat no longer is (#207) — the `openChat`
        // lifecycle's close half, fired on every transition away, including onto a
        // different chat becoming open.
        if let Some(chat_id) = previous {
            spawn_close_chat(client, chat_id);
        }
        if let Some(chat_id) = open {
            // Mark this chat open (#207) before anything else: several update
            // families TDLib streams (message reactions, edits) are only
            // guaranteed for a chat it considers open, so this must precede the
            // projection and paging below that depend on those updates arriving.
            spawn_open_chat(client, chat_id);
            // Project whatever the store already holds (possibly empty, then filled
            // as the landing page lands), and fetch that page once per chat per run.
            // This is the one genuine "opened this chat" moment (#164) — including a
            // re-open of the same chat after focus left and came back, which #158's
            // own chat_id check alone cannot distinguish from a mere continuation.
            project_conversation(app, client, Some(chat_id), true);
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
/// (`force_read`, the `ChatHistory` source): `TDLib` advances the read marker and
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
/// [`send_formatted_text`], an edit through [`edit_formatted_text`] — each parsing
/// the buffer as markdown before it goes out (#212).
///
/// The send is fire-and-forget, like the read path (#115): `TDLib` streams the
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
        // #212: the composer's text is parsed as MarkdownV2 before it goes out
        // — `send_formatted_text`/`edit_formatted_text` fall back to plain text
        // themselves on a parse error, so this never blocks on malformed markup.
        let result = match submission {
            Submission::Send { text } => send_formatted_text(client.bridge(), chat_id, None, text)
                .await
                .map(|_| ()),
            Submission::Reply { reply_to, text } => {
                send_formatted_text(client.bridge(), chat_id, Some(reply_to), text)
                    .await
                    .map(|_| ())
            }
            Submission::Edit { message_id, text } => {
                edit_formatted_text(client.bridge(), chat_id, message_id, text)
                    .await
                    .map(|_| ())
            }
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

/// Run a submitted contact-search query against core (#197). `App` records the
/// query as a pure intent; here the loop drains it, searches this account's
/// contacts, and resolves each returned id to a display name — reading the
/// folded user store back, backfilling via `get_user` for any id the update
/// stream hasn't announced yet, the same backfill any other id-only result
/// (a message sender, a private chat's peer) goes through. Spawned off an
/// `Arc<Client>` clone so the round-trips never block the loop.
///
/// On success the hits land on `contact_tx`, which the loop drains into the
/// overlay. A failed search reuses the `outbound_tx` toast path (#116) to
/// surface an error naming the action.
fn drive_contact_search(
    app: &mut App,
    client: &Arc<Client>,
    contact_tx: &mpsc::Sender<Vec<ContactHit>>,
    outbound_tx: &mpsc::Sender<Notice>,
) {
    let Some(query) = app.take_contact_search() else {
        return;
    };
    let client = Arc::clone(client);
    let contact_tx = contact_tx.clone();
    let outbound_tx = outbound_tx.clone();
    tokio::spawn(async move {
        match client
            .bridge()
            .search_contacts(query, CONTACT_SEARCH_LIMIT)
            .await
        {
            Ok(ids) => {
                let mut hits = Vec::with_capacity(ids.len());
                for user_id in ids {
                    let known = client.read(|state| state.users().get(user_id).cloned());
                    let name = match known {
                        Some(user) => user.display_name(),
                        None => match client.bridge().get_user(user_id).await {
                            Ok(user) => user.display_name(),
                            // A lookup failure for one contact shouldn't drop the
                            // whole result set — fall back to the bare id, the same
                            // graceful degradation `UserStore::display_name` uses
                            // for an unresolved sender.
                            Err(_) => format!("User {user_id}"),
                        },
                    };
                    hits.push(ContactHit::new(user_id, name));
                }
                let _ = contact_tx.send(hits).await;
            }
            // The TDLib message is a fixed error code, never the user's query —
            // safe to show; `from_core_error` normalizes it to a readable phrase
            // (#122).
            Err(err) => {
                let _ = outbound_tx
                    .send(Notice::from_core_error("contact search", &err.message))
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
/// (#116) it is fire-and-forget: `TDLib` streams the optimistic `Pending` copies (and
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
/// Fire-and-forget like the text send (#116): `TDLib` returns an optimistic `Pending`
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

/// Whether the loop should force a full terminal repaint before the next
/// `draw` (#229): only when graphics are actually in play (a graphics-capable
/// terminal *and* the user's setting on, [`App::graphics_active`]) and the
/// open chat or overlay just changed. Pure and independent of the tokio loop
/// so the decision itself is unit-testable without a real terminal — see
/// `run`'s call site for the full (and still not fully confirmed) rationale.
fn should_clear_for_graphics(
    graphics_active: bool,
    chat_changed: bool,
    overlay_changed: bool,
) -> bool {
    graphics_active && (chat_changed || overlay_changed)
}

/// Apply a confirmed retention edit from the in-app editor (#146). The editor
/// validates the four knobs on `App` and lands the resulting
/// [`StorageSettings`](tuigram_core::StorageSettings); here the loop drains it,
/// **swaps the live sweep policy first** so the session reflects the change on the
/// next sweep tick regardless of the disk write, then persists it to
/// `settings.toml`.
///
/// The write is deliberately synchronous — it is a single small file, negligible
/// beside the loop's other per-tick work — so a completed edit is durable before the
/// loop moves on. A save failure (an unwritable config dir) surfaces as an error
/// toast but never blocks the in-memory apply, matching the acceptance criteria: the
/// running session honours the edit even when it could not be written back.
fn drive_settings(app: &mut App, storage_settings: &mut StorageSettings) {
    let Some(updated) = app.take_settings() else {
        return;
    };
    *storage_settings = updated;
    if updated.save().is_err() {
        // The error is a local I/O failure (no config dir, permissions) — a fixed
        // phrase, never the user's typed values, the same rule the send paths follow.
        app.notify(Notice::error("settings save", None));
    }
}

/// Persist a confirmed graphics-toggle edit from the in-app editor (#209). Unlike
/// [`drive_settings`], the in-memory swap already happened at confirm time
/// (`App::set_graphics_enabled`, so the very next frame reflects it with no
/// restart) — this only writes the local mirror through to `settings.toml`,
/// mirroring `drive_settings`'s error handling.
fn drive_graphics_setting(app: &mut App, interface_settings: &mut InterfaceSettings) {
    let Some(enabled) = app.take_graphics() else {
        return;
    };
    interface_settings.graphics = enabled;
    if interface_settings.save().is_err() {
        app.notify(Notice::error("settings save", None));
    }
}

/// Re-read the folded chat lists from the client and re-project the pane (#113),
/// resolving in the same read which private chats have a **bot** peer (#160) so
/// their rows can carry the 🤖 marker — the chat's [`ChatKind`] says "private", only
/// the [`UserStore`](tuigram_core::UserStore) says "bot". The projection needs the
/// client, so it lives here rather than in the pure `App`, which only receives the
/// owned results (lists, then the bot-id set).
fn reproject_chats(app: &mut App, client: &Arc<Client>) {
    let (lists, bots) = client.read(|s| {
        let lists = project_lists(s.chats());
        let bots: HashSet<i64> = lists
            .iter()
            .flat_map(|list| &list.chats)
            .filter_map(|chat| match chat.kind {
                ChatKind::Private { user_id }
                    if s.users()
                        .get(user_id)
                        .is_some_and(|user| matches!(user.kind, UserKind::Bot)) =>
                {
                    Some(chat.id)
                }
                _ => None,
            })
            .collect();
        (lists, bots)
    });
    app.project_chats(lists);
    app.project_bot_chats(bots);
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
/// The download runs asynchronously: `TDLib` streams progress as `updateFile`, folded
/// by the store and re-projected onto the conversation's progress line (via
/// [`AppEvent::File`]), so this only starts the transfer and never awaits it. A file
/// the store has not folded yet is skipped this pass and picked up once its first
/// `updateFile` lands. The dedup is per-run, the download counterpart to the
/// once-per-run list paging (`ensure_active_list_loaded`); a start rejected at the
/// seam is not retried until the next run. With no chat open there is nothing to
/// fetch.
fn drive_downloads(client: &Arc<Client>, history: &HistoryState, downloading: &mut HashSet<i32>) {
    let Some(chat_id) = history.open else { return };
    // The ids to start: files the history references that are not already present
    // or actively transferring, and have not been requested this run. Photos with
    // no sizes carry a 0 ref, which is not downloadable — skip it.
    //
    // A file the store has not folded an `updateFile` for yet reads as "unknown",
    // not "definitely fine" — TDLib does not proactively announce every file a
    // loaded history references, only ones a client has shown interest in, so an
    // unknown file must still be attempted or it would never be requested at all
    // (a chicken-and-egg gap: the first `updateFile` only arrives *because* a
    // download started). `is_none_or` treats "unknown" the same as "not present,
    // not active" — attempt it — while a *known* file still skips correctly once
    // it settles into present or actively downloading.
    let to_start: Vec<i32> = client.read(|s| {
        s.messages()
            .history(chat_id)
            .into_iter()
            .filter_map(|m| m.content.file())
            .filter(|file| file.id != 0 && !downloading.contains(&file.id))
            .filter(|file| {
                s.files()
                    .get(file.id)
                    .is_none_or(|f| !f.is_present() && !f.is_downloading_active)
            })
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

/// The source image for one sender's avatar bubble (#201): a real photo's
/// minithumbnail JPEG bytes, or — Stage 4 — the sender's own `User` record to
/// build a generated fallback bubble from when they have no photo.
enum AvatarSource {
    Photo(Vec<u8>),
    Fallback(User),
}

/// Kick off avatar encoding for the open chat's user senders (#201): for each
/// distinct [`Sender::User`] the history references, not yet cached on `App`
/// and not already in flight this run, build a [`Protocol`] — from a decoded
/// minithumbnail if the sender has one, else a generated fallback bubble
/// (Stage 4) — off the render thread, reporting it back on `avatar_tx`. A
/// no-op with graphics support off (#201's scope decision — nowhere to draw
/// the result), the user's `graphics` setting off (#209 — same reasoning,
/// nothing will render even though the terminal could), or no chat open.
///
/// Like [`drive_downloads`], this dedups per-run via `encoding`; unlike it, a
/// decode/encode failure still reports back (as `None`) so its in-flight
/// marker clears rather than wedging that sender permanently skipped.
fn drive_avatars(
    app: &App,
    client: &Arc<Client>,
    history: &HistoryState,
    encoding: &mut HashSet<i64>,
    avatar_tx: &mpsc::Sender<(i64, Option<Protocol>)>,
) {
    if !app.graphics_enabled() {
        return;
    }
    let AvatarSupport::Graphics(picker) = app.avatar_support() else {
        return;
    };
    let font_size = picker.font_size();
    let gutter_cols = app.avatar_gutter_cols();
    // The exact cell area the render path reserves for the bubble (#201) — the
    // same `gutter_cols()` the header's leading span is sized to — so the
    // encoded image fills the gutter rather than a fixed, possibly-mismatched
    // guess.
    let size = Size::new(gutter_cols as u16, 2);
    let Some(chat_id) = history.open else { return };
    // Distinct user senders in the loaded history, not already cached or in
    // flight — read once as a batch rather than spawning a lookup per message.
    let to_start: Vec<(i64, AvatarSource)> = client.read(|s| {
        let senders: HashSet<i64> = s
            .messages()
            .history(chat_id)
            .into_iter()
            .filter_map(|m| match m.sender {
                Sender::User(id) => Some(id),
                Sender::Chat(_) => None,
            })
            .filter(|id| app.cached_avatar(*id).is_none() && !encoding.contains(id))
            .collect();
        senders
            .into_iter()
            .filter_map(|id| {
                let user = s.users().get(id)?;
                let source = match &user.avatar_minithumbnail {
                    Some(bytes) => AvatarSource::Photo(bytes.clone()),
                    None => AvatarSource::Fallback(user.clone()),
                };
                Some((id, source))
            })
            .collect()
    });

    for (user_id, source) in to_start {
        encoding.insert(user_id);
        let picker = picker.clone();
        let avatar_tx = avatar_tx.clone();
        tokio::spawn(async move {
            let protocol = tokio::task::spawn_blocking(move || {
                let image = match source {
                    AvatarSource::Photo(bytes) => image::load_from_memory(&bytes).ok()?,
                    AvatarSource::Fallback(user) => {
                        avatar::fallback_bubble(font_size, gutter_cols, &user)
                    }
                };
                // `Fit` only ever shrinks (`min(target, image_size)` in its
                // pixel math) — a minithumbnail is typically far smaller in
                // pixels than one terminal cell, so `Fit` would render it at
                // its tiny native size instead of filling the gutter. `Scale`
                // resizes in both directions to hit `size`, upscaling a small
                // source image the way this always-tiny minithumbnail needs.
                picker.new_protocol(image, size, Resize::Scale(None)).ok()
            })
            .await
            .unwrap_or(None);
            let _ = avatar_tx.send((user_id, protocol)).await;
        });
    }
}

/// The source bytes for one message's inline-media still (#208): a downloaded
/// file's local path (`Photo`, a static `Sticker` — read once decoding starts,
/// off the render thread), or an embedded minithumbnail needing no download at
/// all (`Video`, `Animation`).
enum MediaSource {
    File(String),
    Bytes(Vec<u8>),
}

/// Kick off inline-media encoding for the open chat's visible messages
/// (#208): for each message not yet cached on `App` and not already in flight
/// this run, whose content is [`media_ready`](crate::conversation::media_ready),
/// decode its bytes and build a [`Protocol`] off the render thread, reporting
/// it back on `media_tx`. A no-op with graphics support off, the user's
/// `graphics` setting off (#209), or no chat open — same shape as
/// [`drive_avatars`], deduping per-run via `encoding` and reporting a decode
/// failure back as `None` so its in-flight marker still clears.
///
/// Unlike `drive_avatars`, this never triggers a new download itself: a
/// `Photo`/static `Sticker`'s file is already fetched by [`drive_downloads`]
/// (the same file `message_height`'s readiness check already watches), and a
/// `Video`/`Animation` still needs none. So there is no reproject call in the
/// receiving loop arm either — the row space was already reserved the moment
/// the file became present (which itself already triggers a reproject via
/// `AppEvent::File`); this only fills in the pixels.
fn drive_inline_media(
    app: &App,
    client: &Arc<Client>,
    history: &HistoryState,
    encoding: &mut HashSet<i64>,
    media_tx: &mpsc::Sender<(i64, Option<Protocol>)>,
) {
    if !app.graphics_enabled() {
        return;
    }
    let AvatarSupport::Graphics(picker) = app.avatar_support() else {
        return;
    };
    let picker = picker.clone();
    // Match `render_conversation`'s own clamp (`ui.rs`) rather than always
    // encoding at the fixed `MEDIA_COLS` — otherwise a terminal narrower than
    // that (a common width, not an edge case) gets its media's right edge
    // silently, permanently cropped by `allow_clipping` (#222) at render time
    // regardless of scroll position (#226).
    let gutter_cols = app.avatar_gutter_cols();
    let media_cols = crate::ui::media_cols(app.pane_layout().history.width, gutter_cols);
    let size = Size::new(media_cols as u16, crate::conversation::MEDIA_ROWS as u16);
    let Some(chat_id) = history.open else { return };

    let to_start: Vec<(i64, MediaSource)> = client.read(|s| {
        s.messages()
            .history(chat_id)
            .into_iter()
            .filter(|m| app.cached_media(m.id).is_none() && !encoding.contains(&m.id))
            .filter_map(|m| {
                let source = match &m.content {
                    MessageContent::Photo(p) => {
                        let file = s.files().get(p.file.id)?;
                        file.is_present()
                            .then(|| MediaSource::File(file.local_path.clone()))?
                    }
                    MessageContent::Sticker(sticker) if sticker.is_static => {
                        let file = s.files().get(sticker.file.id)?;
                        file.is_present()
                            .then(|| MediaSource::File(file.local_path.clone()))?
                    }
                    MessageContent::Video(v) => MediaSource::Bytes(v.minithumbnail.clone()?),
                    MessageContent::Animation(a) => MediaSource::Bytes(a.minithumbnail.clone()?),
                    _ => return None,
                };
                Some((m.id, source))
            })
            .collect()
    });

    for (message_id, source) in to_start {
        encoding.insert(message_id);
        let picker = picker.clone();
        let media_tx = media_tx.clone();
        tokio::spawn(async move {
            let protocol = tokio::task::spawn_blocking(move || {
                // `File` is a downloaded `Photo`/static `Sticker` — normal-or-larger
                // than the box, so `Fit` (shrink-only) is correct. `Bytes` is always
                // a `Video`/`Animation` minithumbnail — TDLib caps these at a few
                // dozen pixels, far smaller than the reserved cell area, so (like
                // the avatar path above, for the same reason) it needs `Scale` to
                // upscale into the box; `Fit` would leave it at its tiny native
                // size, rendering as a mini-thumbnail inside the empty reservation.
                let (bytes, resize) = match source {
                    MediaSource::File(path) => (std::fs::read(path).ok()?, Resize::Fit(None)),
                    MediaSource::Bytes(bytes) => (bytes, Resize::Scale(None)),
                };
                let image = image::load_from_memory(&bytes).ok()?;
                picker.new_protocol(image, size, resize).ok()
            })
            .await
            .unwrap_or(None);
            let _ = media_tx.send((message_id, protocol)).await;
        });
    }
}

/// Dispatch a confirmed delete to Telegram (#195). `App` records the target and
/// scope as a pure [`DeleteIntent`](crate::conversation::DeleteIntent) from the
/// delete confirm; here the loop drains it and calls
/// [`DeleteRequests::delete`](tuigram_core::DeleteRequests).
///
/// Fire-and-forget like the send/forward paths: there is no optimistic local
/// removal — `TDLib` streams `updateDeleteMessages`, folded by the message store and
/// re-projected onto the open chat's history, so the message vanishes through the
/// normal pipeline. Only a seam-level rejection reports back, as an error toast on
/// `outbound_tx`.
fn drive_delete(app: &mut App, client: &Arc<Client>, outbound_tx: &mpsc::Sender<Notice>) {
    let Some(intent) = app.take_delete() else {
        return;
    };
    let client = Arc::clone(client);
    let outbound_tx = outbound_tx.clone();
    tokio::spawn(async move {
        if let Err(err) = client
            .bridge()
            .delete(intent.chat_id, intent.message_ids, intent.revoke)
            .await
        {
            // A fixed TDLib error code (e.g. MESSAGE_DELETE_FORBIDDEN), never user
            // content — safe to show; `from_core_error` normalizes it (#122).
            let _ = outbound_tx
                .send(Notice::from_core_error("delete", &err.message))
                .await;
        }
    });
}

/// Service a save/download request for a message's media (#195). `App` records the
/// file id from the selected message; here the loop reads the file store back:
/// a file already on disk (the auto-download of incoming media, #120, usually has
/// it) is **revealed** by toasting its local path, so the user knows where to open
/// it; a file not yet present starts the download and says so, its progress then
/// tracked by the conversation's download line — a second `S` once complete reveals
/// the path.
fn drive_save(app: &mut App, client: &Arc<Client>) {
    let Some(file_id) = app.take_save() else {
        return;
    };
    let present_path = client.read(|state| {
        state
            .files()
            .get(file_id)
            .filter(|file| file.is_present())
            .map(|file| file.local_path.clone())
    });
    match present_path {
        Some(path) if !path.is_empty() => app.notify(Notice::success(format!("Saved to {path}"))),
        _ => {
            let client = Arc::clone(client);
            tokio::spawn(async move {
                let _ = client
                    .bridge()
                    .download_file(file_id, DOWNLOAD_PRIORITY)
                    .await;
            });
            app.notify(Notice::info(
                "Downloading… progress shows in the conversation; press S again when it completes.",
            ));
        }
    }
}

/// Write the selected message's text to the OS clipboard (`y`, #197). `App`
/// records the text (it cannot reach the OS clipboard and stays pure); the loop
/// drains it and writes it out here, toasting either result. Best-effort: no
/// clipboard on this host (e.g. a headless Linux session with no X11/Wayland
/// server) surfaces as a failure toast rather than a panic.
fn drive_copy(app: &mut App) {
    let Some(text) = app.take_copy() else {
        return;
    };
    match arboard::Clipboard::new().and_then(|mut clipboard| clipboard.set_text(text)) {
        Ok(()) => app.notify(Notice::success("Copied to clipboard")),
        Err(_) => app.notify(Notice::error("copy", None)),
    }
}

/// Broadcast a typing action for the open chat while the composer holds unsent
/// text (#197), mirroring a normal client. `App` only pulses that an edit left
/// text in the buffer — it never touches the `Client` and doesn't know the open
/// chat id, which lives in `history` — so this pairs the pulse with the open
/// chat and throttles the actual broadcast to once per [`TYPING_RESEND`] per
/// chat, so a burst of keystrokes sends one `sendChatAction`, not one per
/// character. Fire-and-forget, like the read path (#115) — advisory, never
/// blocks composing.
fn drive_typing(app: &mut App, client: &Arc<Client>, history: &mut HistoryState) {
    if !app.take_wants_typing_ping() {
        return;
    }
    let Some(chat_id) = history.open else { return };
    let now = std::time::Instant::now();
    let due = history
        .typing_sent
        .get(&chat_id)
        .is_none_or(|&last| now.duration_since(last) >= TYPING_RESEND);
    if !due {
        return;
    }
    history.typing_sent.insert(chat_id, now);
    let client = Arc::clone(client);
    tokio::spawn(async move {
        let _ = client
            .bridge()
            .send_chat_action(chat_id, Some(ChatAction::Typing))
            .await;
    });
}

/// Re-query the chat list after a dropped-update gap (#195). `App` records the
/// request (`Ctrl-R`); here the loop drains it and calls [`Client::resync`], the
/// same recovery the status bar's "run resync" hint points at. Spawned off an
/// `Arc<Client>` clone so the round-trip never blocks the loop; the recovered list
/// folds back and re-projects on its own. A seam-level failure reports as a toast.
fn drive_resync(app: &mut App, client: &Arc<Client>, outbound_tx: &mpsc::Sender<Notice>) {
    if !app.take_resync() {
        return;
    }
    app.notify(Notice::info("Resyncing the chat list…"));
    let client = Arc::clone(client);
    let outbound_tx = outbound_tx.clone();
    tokio::spawn(async move {
        if let Err(err) = client.resync().await {
            let _ = outbound_tx
                .send(Notice::from_core_error("resync", &err.message))
                .await;
        }
    });
}

/// Log out and exit on a confirmed logout (#195). Unlike the fire-and-forget seams
/// this is **awaited** in the loop: logout is terminal — the whole session is going
/// away — so on success the app quits (the outer teardown in `main` then waits for
/// `TDLib` to reach `Closed` and flushes the database, exactly as on any exit),
/// wiping the local session so the next launch starts at a fresh login. A rejected
/// logout stays in the app and surfaces why, rather than stranding a half-torn-down
/// session.
async fn drive_logout(app: &mut App, client: &Arc<Client>) {
    if !app.take_logout() {
        return;
    }
    // `log_out_and_wait` is the shared whole-operation (#195): it issues the logOut
    // and then waits for `Closed`, so TDLib's asynchronous local-data destruction
    // fully completes before we quit. Quitting early would let the outer teardown's
    // `close` race the in-flight logout and strand a half-cleared session, which the
    // next run opens straight into Closed (no login UI, silent exit). The wait is
    // bounded (~5s), so a stuck teardown never wedges the exit. The REPL harness
    // drives the identical operation, so the two clients cannot drift on it.
    match client.bridge().log_out_and_wait().await {
        Ok(()) => app.dispatch(Action::Quit),
        // A fixed TDLib error code, never user content — safe to show (#122).
        Err(err) => app.notify(Notice::from_core_error("logout", &err.message)),
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

/// Re-project the open chat from an `updateFile` tick, but only when the
/// touched file could actually affect what it shows (#276). `updateFile` fires
/// for every in-flight transfer on the whole account, not just the open chat —
/// most emissions in a busy media chat are for files elsewhere entirely — so
/// this drops the ones [`should_reproject_for_file`] says the open pane can't
/// be affected by, before paying for a full history re-read, sender
/// resolution, and reprojection.
fn drive_file_update(app: &mut App, client: &Arc<Client>, open: Option<i64>, file_id: i32) {
    if should_reproject_for_file(app, open, file_id) {
        project_conversation(app, client, open, false);
    }
}

/// The pure gate [`drive_file_update`] applies — split out so it is testable
/// without an `Arc<Client>` (#276): no chat open, or the open chat's loaded
/// messages don't reference `file_id`, and the tick is a no-op.
fn should_reproject_for_file(app: &App, open: Option<i64>, file_id: i32) -> bool {
    open.is_some() && app.conversation().references_file(file_id)
}

/// Read the open chat's folded history and pinned ids back from the `Client` and
/// project them onto the conversation pane (#114). The projection needs the client,
/// so it lives here rather than in the pure `App`, which only receives the owned
/// snapshot. A `None` open chat (the user is browsing the list) is a no-op.
///
/// `fresh_open` is `true` only from [`drive_open_chat`]'s own open-transition —
/// the one call that is a genuine "the user opened this chat" (#164), as opposed
/// to a live-update or history-page reproject of an already-open chat. It is *not*
/// derivable from `open`/`chat_id` alone: focus leaving and returning to the same
/// chat is not a fresh open (#158 deliberately preserves the cursor for that), but
/// it must still reset the unread-separator watermark so a chat that has since
/// been fully read no longer shows a stale rule — see [`ConversationView::project`].
fn project_conversation(app: &mut App, client: &Arc<Client>, open: Option<i64>, fresh_open: bool) {
    let Some(chat_id) = open else { return };
    let (messages, pinned, files, senders, last_read_inbox, last_read_outbox) = client.read(|s| {
        let messages: Vec<Message> = s.messages().history(chat_id).into_iter().cloned().collect();
        let chat = s.chats().get(chat_id);
        let pinned = chat
            .map(|chat| chat.pinned_message_ids.iter().copied().collect())
            .unwrap_or_default();
        // The chat's read watermarks (#163, #164), fetched from the same record —
        // last_read_inbox for the unread separator, last_read_outbox for outgoing
        // messages' read-receipt glyph.
        let last_read_inbox = chat.map_or(0, |chat| chat.last_read_inbox_message_id);
        let last_read_outbox = chat.map_or(0, |chat| chat.last_read_outbox_message_id);
        // The download state of every file the history's media references, read back
        // from the file store so the progress lines project alongside the messages
        // (#120). A file the store has not folded yet is simply absent until it does.
        let files = messages
            .iter()
            .filter_map(|m| m.content.file())
            .filter_map(|file| s.files().get(file.id).cloned())
            .collect();
        // Resolve each distinct sender to its display label (#160, #194): a user's
        // "Name (@handle)" tinted with their accent color via the user store, or a
        // chat's untinted title. A sender whose record has not been folded yet is
        // left out — the view then falls back to a bare `User {id}` / `Chat {id}`
        // and a later `updateUser` repaints the header.
        let mut senders: HashMap<Sender, SenderLabel> = HashMap::new();
        for sender in messages.iter().map(|m| &m.sender) {
            if senders.contains_key(sender) {
                continue;
            }
            let label = match *sender {
                Sender::User(id) => s.users().get(id).map(sender_label_for),
                Sender::Chat(id) => s.chats().get(id).map(|chat| SenderLabel {
                    label: chat.title.clone(),
                    color: None,
                }),
            };
            if let Some(label) = label {
                senders.insert(sender.clone(), label);
            }
        }
        (
            messages,
            pinned,
            files,
            senders,
            last_read_inbox,
            last_read_outbox,
        )
    });
    app.project_conversation(
        chat_id,
        messages,
        pinned,
        senders,
        last_read_inbox,
        last_read_outbox,
        fresh_open,
    );
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

/// Tell `TDLib` a chat is now open (#207), fire-and-forget like the reaction and
/// pin drivers — the loop never blocks a chat switch on this acknowledging.
/// `TDLib` only guarantees delivery of some live updates (message reactions,
/// edits) for a chat it considers open, so this must run for the open chat's
/// live updates to be trusted; see [`drive_open_chat`].
fn spawn_open_chat(client: &Arc<Client>, chat_id: i64) {
    let client = Arc::clone(client);
    tokio::spawn(async move {
        let _ = client.bridge().open_chat(chat_id).await;
    });
}

/// The close counterpart to [`spawn_open_chat`] (#207), fired when a chat stops
/// being the open one. Best-effort, like the open call: a failure just means
/// `TDLib` keeps treating it as open a little longer.
fn spawn_close_chat(client: &Arc<Client>, chat_id: i64) {
    let client = Arc::clone(client);
    tokio::spawn(async move {
        let _ = client.bridge().close_chat(chat_id).await;
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

    // --- file-reproject relevance gate (#276) ---

    #[test]
    fn should_reproject_for_file_ignores_ids_the_open_chat_does_not_reference() {
        let mut app = App::new();
        let messages = tuigram_fixtures::fake_media_messages(5, 1, 100); // file ids 100..104
        app.project_conversation(1, messages, HashSet::new(), HashMap::new(), 0, 0, true);

        assert!(
            !should_reproject_for_file(&app, Some(1), 999),
            "no loaded message references this id"
        );
        assert!(
            should_reproject_for_file(&app, Some(1), 100),
            "referenced by a loaded message"
        );
        assert!(
            !should_reproject_for_file(&app, None, 100),
            "no chat is open"
        );
    }

    // --- ghosting-fix clear decision (#229) ---

    #[test]
    fn never_clears_when_graphics_are_not_active() {
        // No images were ever drawn, so there is nothing to ghost — regardless of
        // whether the chat or overlay changed.
        assert!(!should_clear_for_graphics(false, false, false));
        assert!(!should_clear_for_graphics(false, true, false));
        assert!(!should_clear_for_graphics(false, false, true));
        assert!(!should_clear_for_graphics(false, true, true));
    }

    #[test]
    fn never_clears_when_graphics_are_active_but_nothing_changed() {
        // No structural transition happened, so nothing could have been left
        // behind since the last frame.
        assert!(!should_clear_for_graphics(true, false, false));
    }

    #[test]
    fn clears_when_graphics_are_active_and_the_chat_or_overlay_changed() {
        assert!(should_clear_for_graphics(true, true, false), "chat changed");
        assert!(
            should_clear_for_graphics(true, false, true),
            "overlay changed"
        );
        assert!(
            should_clear_for_graphics(true, true, true),
            "both changed at once"
        );
    }
}
