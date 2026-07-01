//! The application state (`App`) and the `Action` vocabulary every input is
//! reduced into. `App` is the single source of truth: each `tokio::select!` arm
//! translates its source into an [`Action`], [`App::dispatch`] applies it and
//! marks the frame dirty, and the loop repaints from the new state. Nothing here
//! touches the terminal or awaits — it stays a pure, unit-testable reducer.

use std::collections::HashSet;

use crossterm::event::Event;
use tuigram_core::model::Message;

use crate::chat_list::{ChatList, ChatListView};
use crate::composer::{Composer, Submission};
use crate::conversation::ConversationView;
use crate::event::AppEvent;
use crate::forward::{ForwardIntent, ForwardView};
use crate::keymap::{self, Focus, Overlay};
use crate::mediaform::MediaDraft;
use crate::reactions::ReactionPicker;
use crate::search::SearchView;
use crate::secret::{SecretChatPrompt, SecretLifecycle};
use crate::status::{ConnectionState, Notifications};

/// A single, already-interpreted intent. Every event source (terminal input, the
/// render tick, core updates) is funnelled through this enum before it touches
/// `App`, so all state changes share one code path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Nothing to do (e.g. an unbound key).
    Noop,
    /// Mark the frame dirty so the loop repaints, with no other state change.
    /// The catch-all for a core signal whose data the panes do not project yet
    /// (chats/messages/files/auth): repaint and let the projection read it back.
    Render,
    /// Fold the core link's connection state into the status bar — the reduction
    /// of an [`AppEvent::Connection`](crate::event::AppEvent::Connection) from the
    /// live `updateConnectionState` feed.
    SetConnection(ConnectionState),
    /// Move input focus to the next pane (chat list → history → composer → …).
    FocusNext,
    /// Move input focus to the previous pane, wrapping.
    FocusPrev,
    /// Move input focus directly to a specific pane (e.g. Enter on a chat opens
    /// the history; `i` in the history jumps to the composer).
    SetFocus(Focus),
    /// Show or hide the help overlay.
    ToggleHelp,
    /// Scroll the help overlay one line toward the end (`j` / ↓).
    HelpScrollDown,
    /// Scroll the help overlay one line toward the start (`k` / ↑).
    HelpScrollUp,
    /// Move the chat-list selection down one row.
    SelectNext,
    /// Move the chat-list selection up one row.
    SelectPrev,
    /// Switch to the next chat list (Main → Archive → folders → Main).
    NextList,
    /// Switch to the previous chat list, wrapping.
    PrevList,
    /// Scroll the conversation history one message toward the newest.
    ScrollDown,
    /// Scroll the conversation history one message toward the oldest.
    ScrollUp,
    /// Insert a typed character into the composer at the cursor — the keymap's
    /// printable-input fall-through when the composer is focused.
    ComposerInput(char),
    /// Delete the character before the composer cursor (Backspace).
    ComposerBackspace,
    /// Move the composer cursor one character left.
    ComposerLeft,
    /// Move the composer cursor one character right.
    ComposerRight,
    /// Move the composer cursor to the start of the line.
    ComposerHome,
    /// Move the composer cursor to the end of the line.
    ComposerEnd,
    /// Send the composer's buffer (a no-op when it is empty). The text is routed
    /// to core in Phase 6; for now the buffer is just consumed.
    ComposerSubmit,
    /// Drop any reply/edit context and clear the composer back to plain compose
    /// (the composer's Esc binding).
    ComposerCancel,
    /// Open the search overlay on a fresh, empty query (`/` in a nav pane).
    SearchOpen,
    /// Insert a typed character into the search query at the cursor.
    SearchInput(char),
    /// Delete the character before the search-query cursor (Backspace).
    SearchBackspace,
    /// Move the search-query cursor one character left.
    SearchLeft,
    /// Move the search-query cursor one character right.
    SearchRight,
    /// Move the search-query cursor to the start of the line.
    SearchHome,
    /// Move the search-query cursor to the end of the line.
    SearchEnd,
    /// Run the typed query and switch to the results list. Phase 6 dispatches the
    /// core search; for now the results are whatever has been injected.
    SearchSubmit,
    /// Close the search overlay (from either the query line or the results).
    SearchCancel,
    /// Move the search-results selection down one hit.
    ResultNext,
    /// Move the search-results selection up one hit.
    ResultPrev,
    /// Open the selected search hit (#117): jump to its chat and, when the message
    /// is in the loaded history, scroll to it.
    ResultOpen,
    /// Start forwarding the selected search hit: open the target picker.
    ForwardOpen,
    /// Start forwarding the selected conversation message (`f` in the history pane,
    /// as in the official client): open the target picker sourced from the open chat.
    ForwardMessage,
    /// Move the forward target-picker selection down one chat.
    ForwardNext,
    /// Move the forward target-picker selection up one chat.
    ForwardPrev,
    /// Confirm the forward to the selected target (#118): record the send and close
    /// the picker back to browsing.
    ForwardConfirm,
    /// Cancel the forward, returning to wherever it was started from (the search
    /// results, or the conversation).
    ForwardCancel,
    /// Pin or unpin the selected history message. Phase 6 also calls core; for now
    /// it flips the local pinned state behind the 📌 indicator.
    PinToggle,
    /// Open the reaction picker on the selected history message (a no-op with no
    /// selected message).
    ReactionOpen,
    /// Move the reaction-picker selection to the next emoji.
    ReactionNext,
    /// Move the reaction-picker selection to the previous emoji.
    ReactionPrev,
    /// Toggle the picked emoji on the selected message and close the picker. Phase
    /// 6 dispatches the core add/remove; for now it reflects optimistically.
    ReactionConfirm,
    /// Close the reaction picker without changing anything.
    ReactionCancel,
    /// Open the send-media prompt on a fresh, empty path/caption.
    AttachOpen,
    /// Insert a typed character into the focused send-media field.
    AttachInput(char),
    /// Delete the character before the focused send-media field's cursor.
    AttachBackspace,
    /// Move the focused send-media field's cursor one character left.
    AttachLeft,
    /// Move the focused send-media field's cursor one character right.
    AttachRight,
    /// Move the focused send-media field's cursor to the start of the line.
    AttachHome,
    /// Move the focused send-media field's cursor to the end of the line.
    AttachEnd,
    /// Move send-media editing between the path and caption fields (Tab).
    AttachToggleField,
    /// Confirm the send-media prompt (a no-op without a path). Phase 6 builds the
    /// [`OutgoingMedia`](tuigram_core::model::OutgoingMedia) and sends it; for now
    /// it just closes the prompt.
    AttachConfirm,
    /// Cancel the send-media prompt without sending.
    AttachCancel,
    /// Open the secret-chat lifecycle confirm for the selected chat (#87): start a
    /// new secret chat with a private chat's user, or close an open one. A no-op
    /// when the selection offers no lifecycle action (a group or channel).
    SecretOpen,
    /// Confirm the secret-chat lifecycle action and close the prompt. Phase 6 calls
    /// the core seam (`create_new_secret_chat` / `close_secret_chat`); for now it
    /// just closes.
    SecretConfirm,
    /// Cancel the secret-chat prompt without acting.
    SecretCancel,
    /// Dismiss the current transient toast immediately (#88), revealing the next
    /// queued one. A no-op when nothing is showing.
    NoticeDismiss,
    /// Tear down and exit the loop.
    Quit,
}

/// The whole app's state. Phase 5/6 widgets and real data hang off this; for now
/// it carries only what the spine needs to prove the loop is alive.
#[derive(Debug, Default)]
pub struct App {
    /// Set once a quit action is seen; the loop checks it and breaks.
    should_quit: bool,
    /// Set when visible state changed since the last paint; cleared after `draw`.
    dirty: bool,
    /// The left pane's chat-list view: the lists, the active one, and the
    /// selection. Empty until Phase 6 projects the core store into it.
    chat_list: ChatListView,
    /// The right pane's conversation view: the open chat's messages and the
    /// scroll offset. Empty until Phase 6 projects the core store into it.
    conversation: ConversationView,
    /// The bottom pane's composer: the input buffer, cursor, and reply/edit mode.
    composer: Composer,
    /// Which pane currently receives input; drives both key resolution and the
    /// focused-pane border highlight.
    focus: Focus,
    /// The modal overlay drawn over the panes, if any. While set it captures input
    /// (key resolution routes to it instead of `focus`).
    overlay: Overlay,
    /// The help overlay's scroll offset — the index of the topmost help line drawn,
    /// so the cheatsheet can be read on a terminal too short to show it all. Reset to
    /// the top each time help opens; clamped against [`keymap::help_line_count`].
    help_scroll: u16,
    /// The search overlay's state: the query line and the hit list it renders from.
    search: SearchView,
    /// The forward overlay's state: the messages being forwarded and the target
    /// picker. Inert until a forward is started.
    forward: ForwardView,
    /// Where a cancelled forward returns to — the overlay that was active when the
    /// forward was started, so `Esc` lands back on the search results (forward from a
    /// hit) or the conversation (forward from the history pane, `Overlay::None`).
    forward_return: Overlay,
    /// The reaction picker's state: the emoji palette and the selection. Reset each
    /// time the picker opens.
    reaction: ReactionPicker,
    /// The send-media prompt's state: the path and caption being typed. Reset each
    /// time the prompt opens.
    media: MediaDraft,
    /// The secret-chat lifecycle confirm's state (#87): the start/close action for
    /// the selected chat. `Some` only while the [`Overlay::SecretChat`] is open.
    secret: Option<SecretChatPrompt>,
    /// The core link's connection state (#88), shown in the status bar. Defaults to
    /// `Connecting`; Phase 6 folds `updateConnectionState` into it.
    connection: ConnectionState,
    /// The transient-toast queue (#88): one-off events and error codes that float
    /// over the panes without capturing input and age out on the heartbeat.
    notifications: Notifications,
    /// Set when the user scrolled up while already at the oldest loaded message, a
    /// request the loop services by paging older history (#114). The loop reads and
    /// clears it each tick via [`take_wants_older_history`](Self::take_wants_older_history).
    wants_older_history: bool,
    /// A submitted composer buffer awaiting dispatch to core (#116). `App` is pure
    /// and never touches the `Client`, so a submit lands here and the loop drains it
    /// via [`take_outbound`](Self::take_outbound), routing it to the send/edit seam.
    outbound: Option<Submission>,
    /// A submitted search query awaiting dispatch to core (#117). Like `outbound`,
    /// `App` records the intent and the loop drains it via
    /// [`take_search_query`](Self::take_search_query), runs the search (in-chat or
    /// global by context), and feeds the hits back through
    /// [`set_search_results`](Self::set_search_results).
    pending_search: Option<String>,
    /// A `(chat_id, message_id)` the user opened from a search hit and wants the
    /// conversation to land on (#117). The loop opens the chat; the next projection
    /// of that chat scrolls to the message if it is loaded, then clears this. Cleared
    /// on a switch to a different chat so a stale target never jumps a later view.
    pending_jump: Option<(i64, i64)>,
    /// A confirmed forward awaiting dispatch to core (#118). Like `outbound`, `App`
    /// records the intent and the loop drains it via
    /// [`take_forward`](Self::take_forward), routing it to
    /// [`ForwardRequests::forward_messages`](tuigram_core::messages::ForwardRequests::forward_messages).
    pending_forward: Option<ForwardIntent>,
}

impl App {
    /// A fresh app, marked dirty so the first frame paints before any event.
    pub fn new() -> Self {
        Self {
            dirty: true,
            ..Self::default()
        }
    }

    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// The chat-list view the left pane renders from.
    pub fn chat_list(&self) -> &ChatListView {
        &self.chat_list
    }

    /// The conversation view the history pane renders from.
    pub fn conversation(&self) -> &ConversationView {
        &self.conversation
    }

    /// The composer the bottom pane renders from.
    pub fn composer(&self) -> &Composer {
        &self.composer
    }

    /// Which pane currently has input focus, for the focused-border highlight.
    pub fn focus(&self) -> Focus {
        self.focus
    }

    /// The modal overlay currently drawn over the panes (or [`Overlay::None`]).
    pub fn overlay(&self) -> Overlay {
        self.overlay
    }

    /// Whether the help overlay is currently shown. A convenience predicate over
    /// [`overlay`](Self::overlay), kept for the help tests; the render path reads
    /// the full [`Overlay`] instead.
    #[allow(dead_code)]
    pub fn help_visible(&self) -> bool {
        self.overlay == Overlay::Help
    }

    /// The help overlay's scroll offset — the topmost help line to draw. Read by the
    /// render to window the cheatsheet on a short terminal.
    pub fn help_scroll(&self) -> u16 {
        self.help_scroll
    }

    /// The search overlay's state, for rendering the query line and results.
    pub fn search(&self) -> &SearchView {
        &self.search
    }

    /// The forward overlay's state, for rendering the target picker.
    pub fn forward(&self) -> &ForwardView {
        &self.forward
    }

    /// The reaction picker's state, for rendering the emoji palette.
    pub fn reaction(&self) -> &ReactionPicker {
        &self.reaction
    }

    /// The send-media prompt's state, for rendering the path/caption fields.
    pub fn media(&self) -> &MediaDraft {
        &self.media
    }

    /// The secret-chat lifecycle prompt's state, for rendering the confirm overlay
    /// (`None` when it is not open).
    pub fn secret(&self) -> Option<&SecretChatPrompt> {
        self.secret.as_ref()
    }

    /// The core link's connection state, for the status bar's left field.
    pub fn connection(&self) -> ConnectionState {
        self.connection
    }

    /// The transient-toast queue, for the status overlay and a "+N more" hint.
    pub fn notifications(&self) -> &Notifications {
        &self.notifications
    }

    /// Fold a connection-state change into the status bar — driven by
    /// [`Action::SetConnection`], the reduction of the live
    /// `updateConnectionState` feed (#110). Repaints only on an actual change.
    pub fn set_connection(&mut self, state: ConnectionState) {
        if self.connection != state {
            self.connection = state;
            self.dirty = true;
        }
    }

    /// Re-project the chat-list pane from the core [`ChatStore`](tuigram_core::ChatStore)
    /// (#113). The loop reads the folded lists back from the `Client` on a chat
    /// signal and hands the owned projection here, so `App` stays pure — it never
    /// touches the `Client`, the same split as the carried-state connection fold.
    /// The cursor is preserved across the swap (see [`ChatListView::project`]), so
    /// a live chat update repaints the list without moving the selection.
    pub fn project_chats(&mut self, lists: Vec<ChatList>) {
        self.chat_list.project(lists);
        self.dirty = true;
    }

    /// Re-project the conversation pane from the core
    /// [`MessageStore`](tuigram_core::messages::MessageStore) (#114). The loop reads
    /// the open `chat_id`'s history and pinned ids back from the `Client` on a
    /// message signal (or a freshly-merged history page) and hands the owned
    /// snapshot here, so `App` stays pure — the same split as
    /// [`project_chats`](Self::project_chats). [`ConversationView::project`] keeps
    /// the selected message under the cursor when refreshing the same chat, and
    /// starts fresh at the top when a different chat is opened.
    pub fn project_conversation(
        &mut self,
        chat_id: i64,
        messages: Vec<Message>,
        pinned: HashSet<i64>,
    ) {
        self.conversation.project(chat_id, messages, pinned);
        // Honor a pending search-hit jump (#117): once this chat's history holds the
        // target message, scroll to it and clear the jump. A jump for a different chat
        // is stale (the user opened elsewhere) — drop it so it never moves a later
        // view. A jump for this chat whose message is not loaded yet stays pending, so
        // the landing page (which projects right after open) can still land on it.
        if let Some((jump_chat, message_id)) = self.pending_jump {
            // Apply the jump only for its own chat, and only once the message is
            // loaded; clear it when applied, or when it is stale (a different chat is
            // now open). A same-chat target whose message is not loaded yet stays
            // pending for the landing page.
            let applied = jump_chat == chat_id && self.conversation.select_message(message_id);
            if applied || jump_chat != chat_id {
                self.pending_jump = None;
            }
        }
        self.dirty = true;
    }

    /// Take the pending "page older history" request (#114), clearing it. The loop
    /// calls this each tick: a `true` means the user pressed up at the very top of
    /// the loaded history, so the loop fetches the next older page for the open chat.
    pub fn take_wants_older_history(&mut self) -> bool {
        std::mem::take(&mut self.wants_older_history)
    }

    /// Take the pending composer submission, if any (#116). The loop drains this
    /// each tick and dispatches it to the send/edit seam; `None` means nothing was
    /// submitted since the last drain.
    pub fn take_outbound(&mut self) -> Option<Submission> {
        self.outbound.take()
    }

    /// Take the pending search query, if any (#117). The loop drains this each tick,
    /// runs the search (in-chat or global by context), and feeds the projected hits
    /// back through [`set_search_results`](Self::set_search_results); `None` means
    /// nothing was submitted since the last drain.
    pub fn take_search_query(&mut self) -> Option<String> {
        self.pending_search.take()
    }

    /// Take the pending forward, if any (#118). The loop drains this each tick and
    /// dispatches it to
    /// [`ForwardRequests::forward_messages`](tuigram_core::messages::ForwardRequests::forward_messages);
    /// `None` means no forward was confirmed since the last drain.
    pub fn take_forward(&mut self) -> Option<ForwardIntent> {
        self.pending_forward.take()
    }

    /// Open the forward target picker for `message_id` from `source_chat_id`, shared
    /// by the two entry points (a search hit, `ForwardOpen`; the selected history
    /// message, `ForwardMessage`). The picker reuses a snapshot of the chat list as
    /// its targets, and the current overlay is remembered as the cancel-return target.
    fn open_forward(&mut self, source_chat_id: i64, message_id: i64) {
        self.forward_return = self.overlay;
        self.forward = ForwardView::new(source_chat_id, vec![message_id], self.chat_list.clone());
        self.overlay = Overlay::Forward;
        self.dirty = true;
    }

    /// Replace the search overlay's hits with a fresh, projected result set (#117),
    /// resetting the selection to the top. The loop calls this when a spawned search
    /// completes.
    pub fn set_search_results(&mut self, hits: Vec<crate::search::SearchHit>) {
        self.search.set_results(hits);
        self.dirty = true;
    }

    /// Enqueue a transient toast — a failed action (#116) or a one-off core event.
    /// The heartbeat tick ages it out and [`Action::NoticeDismiss`] drops it on
    /// demand.
    pub fn notify(&mut self, notice: crate::status::Notice) {
        self.notifications.push(notice);
        self.dirty = true;
    }

    /// A fresh app showing `chat_list`, marked dirty so the first frame paints.
    /// The seam Phase 6 (and the render tests) use to inject a populated view.
    #[cfg(test)]
    pub fn with_chat_list(chat_list: ChatListView) -> Self {
        Self {
            chat_list,
            ..Self::new()
        }
    }

    /// A fresh app showing `conversation`, marked dirty so the first frame paints.
    /// The seam Phase 6 (and the render tests) use to inject a populated history.
    #[cfg(test)]
    pub fn with_conversation(conversation: ConversationView) -> Self {
        Self {
            conversation,
            ..Self::new()
        }
    }

    /// A fresh app whose composer is `composer`, marked dirty so the first frame
    /// paints. The seam the render tests use to inject a typed/reply/edit state.
    #[cfg(test)]
    pub fn with_composer(composer: Composer) -> Self {
        Self {
            composer,
            ..Self::new()
        }
    }

    /// Inject a search result set, the test seam standing in for a completed core
    /// search (#117). Delegates to [`set_search_results`](Self::set_search_results),
    /// the same path the loop uses, so the reducer and render tests drive the
    /// results/forward overlays exactly as the live search does.
    #[cfg(test)]
    pub fn inject_search_results(&mut self, results: Vec<crate::search::SearchHit>) {
        self.set_search_results(results);
    }

    /// Called by the loop after a successful `terminal.draw`.
    pub fn clear_dirty(&mut self) {
        self.dirty = false;
    }

    /// Map a raw crossterm event to an [`Action`]. Pure: no state changes here,
    /// so key bindings are trivially unit-testable.
    ///
    /// Key events are resolved through the central [`keymap`] against the current
    /// focus and help-overlay state, so this stays a thin adapter and the bindings
    /// live in one place.
    pub fn on_terminal_event(&self, event: Event) -> Action {
        match event {
            Event::Key(key) => keymap::resolve(self.focus, self.overlay, &key),
            // A resize must repaint against the new viewport.
            Event::Resize(_, _) => Action::Render,
            _ => Action::Noop,
        }
    }

    /// Map a core [`AppEvent`] to an [`Action`]. Pure: the live source already
    /// classified the update, so this only chooses the reduction.
    ///
    /// Connection changes fold into the status bar. Chat and message signals don't
    /// pass through here — the loop reads the folded list/history back from the
    /// `Client` and calls [`project_chats`](Self::project_chats) (#113) /
    /// [`project_conversation`](Self::project_conversation) (#114) directly, since
    /// the projection needs the `Client` and `App` stays pure. Every remaining
    /// signal (files/auth) is a repaint nudge until its own projection lands.
    pub fn on_app_event(&self, event: AppEvent) -> Action {
        match event {
            AppEvent::Connection(state) => Action::SetConnection(state),
            AppEvent::Auth
            | AppEvent::Chats
            | AppEvent::Messages
            | AppEvent::File
            | AppEvent::Lagged => Action::Render,
        }
    }

    /// Apply an [`Action`], mutating state and marking the frame dirty when the
    /// visible state changed. The single write path into `App`.
    pub fn dispatch(&mut self, action: Action) {
        match action {
            Action::Noop => {}
            Action::Render => self.dirty = true,
            Action::SetConnection(state) => self.set_connection(state),
            Action::FocusNext => {
                self.focus = self.focus.next();
                self.dirty = true;
            }
            Action::FocusPrev => {
                self.focus = self.focus.prev();
                self.dirty = true;
            }
            Action::SetFocus(focus) => {
                self.focus = focus;
                self.dirty = true;
            }
            Action::ToggleHelp => {
                // Toggles between no overlay and the help cheatsheet; the keymap
                // only emits this from browsing or while help is already open. A
                // fresh open starts at the top of the cheatsheet.
                self.overlay = if self.overlay == Overlay::Help {
                    Overlay::None
                } else {
                    self.help_scroll = 0;
                    Overlay::Help
                };
                self.dirty = true;
            }
            Action::HelpScrollDown => {
                // Clamp at the last help line so a scroll never runs off the end; the
                // render further clips to the popup's height.
                let max = keymap::help_line_count().saturating_sub(1) as u16;
                self.help_scroll = (self.help_scroll + 1).min(max);
                self.dirty = true;
            }
            Action::HelpScrollUp => {
                self.help_scroll = self.help_scroll.saturating_sub(1);
                self.dirty = true;
            }
            Action::SelectNext => {
                self.chat_list.select_next();
                self.dirty = true;
            }
            Action::SelectPrev => {
                self.chat_list.select_prev();
                self.dirty = true;
            }
            Action::NextList => {
                self.chat_list.next_list();
                self.dirty = true;
            }
            Action::PrevList => {
                self.chat_list.prev_list();
                self.dirty = true;
            }
            Action::ScrollDown => {
                self.conversation.scroll_down();
                self.dirty = true;
            }
            Action::ScrollUp => {
                // A scroll-up that can't move (already at the oldest loaded message)
                // is the request to page older history; the loop services it (#114).
                if self.conversation.offset() == 0 && !self.conversation.is_empty() {
                    self.wants_older_history = true;
                }
                self.conversation.scroll_up();
                self.dirty = true;
            }
            Action::ComposerInput(c) => {
                self.composer.insert(c);
                self.dirty = true;
            }
            Action::ComposerBackspace => {
                self.composer.backspace();
                self.dirty = true;
            }
            Action::ComposerLeft => {
                self.composer.move_left();
                self.dirty = true;
            }
            Action::ComposerRight => {
                self.composer.move_right();
                self.dirty = true;
            }
            Action::ComposerHome => {
                self.composer.move_home();
                self.dirty = true;
            }
            Action::ComposerEnd => {
                self.composer.move_end();
                self.dirty = true;
            }
            Action::ComposerSubmit => {
                // Route the submitted buffer to core (#116): a new message, a reply,
                // or an edit per the composer's mode. `App` is pure, so it records the
                // resolved submission as an outbound intent the loop drains and
                // dispatches to the send/edit seam; an empty buffer returns `None`, a
                // no-op that does not repaint.
                if let Some(submission) = self.composer.submit() {
                    self.outbound = Some(submission);
                    self.dirty = true;
                }
            }
            Action::ComposerCancel => {
                self.composer.cancel();
                self.dirty = true;
            }
            Action::SearchOpen => {
                // A fresh search each time, so a previous query never leaks in.
                self.search.reset();
                self.overlay = Overlay::SearchInput;
                self.dirty = true;
            }
            Action::SearchInput(c) => {
                self.search.insert(c);
                self.dirty = true;
            }
            Action::SearchBackspace => {
                self.search.backspace();
                self.dirty = true;
            }
            Action::SearchLeft => {
                self.search.move_left();
                self.dirty = true;
            }
            Action::SearchRight => {
                self.search.move_right();
                self.dirty = true;
            }
            Action::SearchHome => {
                self.search.move_home();
                self.dirty = true;
            }
            Action::SearchEnd => {
                self.search.move_end();
                self.dirty = true;
            }
            Action::SearchSubmit => {
                // Record the query as a pure intent the loop drains and runs against
                // core (#117), then switch to the results overlay. `App` is pure, so
                // the hits arrive later via `set_search_results`; clear any stale hits
                // now so the overlay is empty until they land. A blank query is a
                // no-op that stays on the input line.
                let query = self.search.query().trim().to_owned();
                if !query.is_empty() {
                    self.pending_search = Some(query);
                    self.search.set_results(Vec::new());
                    self.overlay = Overlay::SearchResults;
                    self.dirty = true;
                }
            }
            Action::SearchCancel => {
                self.overlay = Overlay::None;
                self.dirty = true;
            }
            Action::ResultNext => {
                self.search.select_next();
                self.dirty = true;
            }
            Action::ResultPrev => {
                self.search.select_prev();
                self.dirty = true;
            }
            Action::ResultOpen => {
                // Jump to the selected hit (#117): select its chat in the list, focus
                // the history so the loop opens and pages it, and record the target so
                // the next projection of that chat scrolls to the message if it is
                // loaded. A hit whose chat is not in the active list (a global hit in a
                // folder/archive) still closes the overlay and focuses the history; the
                // chat just stays whatever was selected. No selected hit is a no-op.
                if let Some(hit) = self.search.selected_hit() {
                    let (chat_id, message_id) = (hit.chat_id, hit.message_id);
                    self.pending_jump = Some((chat_id, message_id));
                    self.chat_list.select_chat(chat_id);
                    self.focus = Focus::History;
                    self.overlay = Overlay::None;
                    self.dirty = true;
                }
            }
            Action::ForwardOpen => {
                // Forward the selected hit. The picker reuses a snapshot of the
                // chat list as its target list; the hit's chat is the source the
                // forward carries. No selected hit (empty results) is a no-op that
                // stays on the results overlay.
                if let Some(hit) = self.search.selected_hit() {
                    let (source, message_id) = (hit.chat_id, hit.message_id);
                    self.open_forward(source, message_id);
                }
            }
            Action::ForwardMessage => {
                // Forward the selected conversation message (`f` in the history pane).
                // The message carries its own chat, which is the open chat and the
                // forward's source. No selected message (empty history) is a no-op.
                if let Some((source, message_id)) = self
                    .conversation
                    .selected_message()
                    .map(|m| (m.chat_id, m.id))
                {
                    self.open_forward(source, message_id);
                }
            }
            Action::ForwardNext => {
                self.forward.select_next();
                self.dirty = true;
            }
            Action::ForwardPrev => {
                self.forward.select_prev();
                self.dirty = true;
            }
            Action::ForwardConfirm => {
                // Record the forward as a pure intent for the loop to send through
                // `forward_messages` (#118), then close the picker back to browsing.
                // An empty picker (no target chat) has nowhere to send, so it just
                // closes without recording an intent.
                if let Some(to_chat_id) = self.forward.selected_target().map(|c| c.id) {
                    self.pending_forward = Some(ForwardIntent {
                        from_chat_id: self.forward.source_chat_id(),
                        message_ids: self.forward.message_ids().to_vec(),
                        to_chat_id,
                    });
                }
                self.overlay = Overlay::None;
                self.dirty = true;
            }
            Action::ForwardCancel => {
                // Back to wherever the forward was started from — the search results
                // (forward from a hit) or the conversation (forward from history).
                self.overlay = self.forward_return;
                self.dirty = true;
            }
            Action::PinToggle => {
                // Pin/unpin the selected message. Phase 6 also calls core's
                // pin/unpin; for now the local flip drives the 📌 indicator. A no-op
                // on an empty history (no selected message).
                if let Some(id) = self.conversation.selected_message().map(|m| m.id) {
                    self.conversation.toggle_pin(id);
                    self.dirty = true;
                }
            }
            Action::ReactionOpen => {
                // Open the picker only when there is a message to react to.
                if self.conversation.selected_message().is_some() {
                    self.reaction = ReactionPicker::new();
                    self.overlay = Overlay::Reaction;
                    self.dirty = true;
                }
            }
            Action::ReactionNext => {
                self.reaction.select_next();
                self.dirty = true;
            }
            Action::ReactionPrev => {
                self.reaction.select_prev();
                self.dirty = true;
            }
            Action::ReactionConfirm => {
                // Toggle the picked emoji on the selected message and close. Phase 6
                // dispatches the core add/remove and folds the real counts; here the
                // optimistic flip updates the `{emoji×n*}` chips directly.
                if let Some(id) = self.conversation.selected_message().map(|m| m.id) {
                    self.conversation
                        .toggle_reaction(id, self.reaction.selected_emoji());
                }
                self.overlay = Overlay::None;
                self.dirty = true;
            }
            Action::ReactionCancel => {
                self.overlay = Overlay::None;
                self.dirty = true;
            }
            Action::AttachOpen => {
                // A fresh prompt each time, so a previous path never leaks in.
                self.media = MediaDraft::default();
                self.overlay = Overlay::SendMedia;
                self.dirty = true;
            }
            Action::AttachInput(c) => {
                self.media.insert(c);
                self.dirty = true;
            }
            Action::AttachBackspace => {
                self.media.backspace();
                self.dirty = true;
            }
            Action::AttachLeft => {
                self.media.move_left();
                self.dirty = true;
            }
            Action::AttachRight => {
                self.media.move_right();
                self.dirty = true;
            }
            Action::AttachHome => {
                self.media.move_home();
                self.dirty = true;
            }
            Action::AttachEnd => {
                self.media.move_end();
                self.dirty = true;
            }
            Action::AttachToggleField => {
                self.media.toggle_field();
                self.dirty = true;
            }
            Action::AttachConfirm => {
                // Phase 6 builds the `OutgoingMedia` from the prompt and calls
                // `send_media`; for now confirming with a path just closes the
                // prompt, and an empty path is a no-op that keeps it open.
                if self.media.is_sendable() {
                    self.overlay = Overlay::None;
                    self.dirty = true;
                }
            }
            Action::AttachCancel => {
                self.overlay = Overlay::None;
                self.dirty = true;
            }
            Action::SecretOpen => {
                // Offer the lifecycle action for the selected chat — start a secret
                // chat with a private chat's user, or close an open one. The
                // decision reads only the chat's kind and folded state (never key
                // material). No selected chat, or one with no action (a group), is a
                // no-op that opens nothing.
                if let Some(chat) = self.chat_list.selected_chat() {
                    let state = self.chat_list.secret_state(chat.id);
                    if let Some(lifecycle) = SecretLifecycle::for_chat(chat, state) {
                        self.secret = Some(SecretChatPrompt::new(lifecycle, chat.title.clone()));
                        self.overlay = Overlay::SecretChat;
                        self.dirty = true;
                    }
                }
            }
            Action::SecretConfirm => {
                // Phase 6 dispatches the core seam for `self.secret`'s lifecycle
                // (`create_new_secret_chat` / `close_secret_chat`) and lets the
                // resulting `updateSecretChat` / `updateNewChat` fold in; for now
                // confirming just closes the prompt back to browsing.
                self.secret = None;
                self.overlay = Overlay::None;
                self.dirty = true;
            }
            Action::SecretCancel => {
                self.secret = None;
                self.overlay = Overlay::None;
                self.dirty = true;
            }
            Action::NoticeDismiss => {
                // Drop the showing toast (revealing any next); a no-op that does
                // not repaint when none is up.
                if self.notifications.current().is_some() {
                    self.notifications.dismiss();
                    self.dirty = true;
                }
            }
            Action::Quit => self.should_quit = true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }

    #[test]
    fn q_and_ctrl_c_quit_from_a_nav_pane() {
        let app = App::new(); // lands focused on the chat list
        assert_eq!(
            app.on_terminal_event(key(KeyCode::Char('q'), KeyModifiers::NONE)),
            Action::Quit
        );
        assert_eq!(
            app.on_terminal_event(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Action::Quit
        );
        // Esc is the composer's cancel key now, not a global quit — unbound here.
        assert_eq!(
            app.on_terminal_event(key(KeyCode::Esc, KeyModifiers::NONE)),
            Action::Noop
        );
    }

    #[test]
    fn unbound_key_is_noop() {
        let app = App::new();
        assert_eq!(
            app.on_terminal_event(key(KeyCode::Char('x'), KeyModifiers::NONE)),
            Action::Noop
        );
    }

    #[test]
    fn key_release_is_ignored() {
        let app = App::new();
        let mut release = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        assert_eq!(app.on_terminal_event(Event::Key(release)), Action::Noop);
    }

    #[test]
    fn resize_requests_render() {
        let app = App::new();
        assert_eq!(app.on_terminal_event(Event::Resize(80, 24)), Action::Render);
    }

    #[test]
    fn quit_sets_flag() {
        let mut app = App::new();
        assert!(!app.should_quit());
        app.dispatch(Action::Quit);
        assert!(app.should_quit());
    }

    #[test]
    fn a_connection_event_folds_into_the_status_bar() {
        let mut app = App::new();
        assert_eq!(app.connection(), ConnectionState::Connecting);
        // The live source classifies updateConnectionState into Connection(state);
        // on_app_event reduces it to SetConnection, dispatch folds it.
        let action = app.on_app_event(AppEvent::Connection(ConnectionState::Ready));
        assert_eq!(action, Action::SetConnection(ConnectionState::Ready));
        app.clear_dirty();
        app.dispatch(action);
        assert_eq!(app.connection(), ConnectionState::Ready);
        assert!(app.is_dirty());
    }

    #[test]
    fn data_signals_map_to_a_repaint() {
        // Until the panes project the folded state, every non-connection signal is
        // a repaint nudge — each variant maps, so the mpsc arm is exercised whole.
        let app = App::new();
        for event in [
            AppEvent::Auth,
            AppEvent::Chats,
            AppEvent::Messages,
            AppEvent::File,
            AppEvent::Lagged,
        ] {
            assert_eq!(app.on_app_event(event), Action::Render);
        }
    }

    #[test]
    fn noop_leaves_state_untouched() {
        let mut app = App::new();
        app.clear_dirty();
        app.dispatch(Action::Noop);
        assert!(!app.is_dirty());
        assert!(!app.should_quit());
    }

    #[test]
    fn chat_list_focus_maps_navigation_keys() {
        let app = App::new(); // default focus: the chat list
        let mapped = |code| app.on_terminal_event(key(code, KeyModifiers::NONE));
        assert_eq!(mapped(KeyCode::Down), Action::SelectNext);
        assert_eq!(mapped(KeyCode::Char('j')), Action::SelectNext);
        assert_eq!(mapped(KeyCode::Up), Action::SelectPrev);
        assert_eq!(mapped(KeyCode::Char('k')), Action::SelectPrev);
        assert_eq!(mapped(KeyCode::Char(']')), Action::NextList);
        assert_eq!(mapped(KeyCode::Char('[')), Action::PrevList);
        // Tab now switches panes; Enter opens the selected chat in the history.
        assert_eq!(mapped(KeyCode::Tab), Action::FocusNext);
        assert_eq!(mapped(KeyCode::Enter), Action::SetFocus(Focus::History));
    }

    #[test]
    fn key_resolution_follows_the_focused_pane() {
        let mut app = App::new();
        app.dispatch(Action::SetFocus(Focus::History));
        // `j` scrolls in the history, where the same key selected in the list.
        assert_eq!(
            app.on_terminal_event(key(KeyCode::Char('j'), KeyModifiers::NONE)),
            Action::ScrollDown
        );
    }

    #[test]
    fn focus_cycles_through_the_panes_and_dirties() {
        let mut app = App::new();
        assert_eq!(app.focus(), Focus::ChatList);
        app.clear_dirty();
        app.dispatch(Action::FocusNext);
        assert_eq!(app.focus(), Focus::History);
        assert!(app.is_dirty());
        app.dispatch(Action::FocusNext);
        assert_eq!(app.focus(), Focus::Composer);
        app.dispatch(Action::FocusNext);
        assert_eq!(app.focus(), Focus::ChatList, "wraps back to the start");
        app.dispatch(Action::FocusPrev);
        assert_eq!(app.focus(), Focus::Composer, "and back the other way");
    }

    #[test]
    fn toggle_help_shows_then_hides_the_overlay_and_a_stray_key_is_ignored() {
        let mut app = App::new();
        assert!(!app.help_visible());
        app.dispatch(Action::ToggleHelp);
        assert!(app.help_visible());
        // While open the overlay is modal and explicitly closed: a stray key no
        // longer dismisses it, so a half-read page survives an accidental press.
        assert_eq!(
            app.on_terminal_event(key(KeyCode::Char('x'), KeyModifiers::NONE)),
            Action::Noop
        );
        assert!(app.help_visible(), "a stray key does not close help");
        app.dispatch(Action::ToggleHelp);
        assert!(!app.help_visible());
    }

    #[test]
    fn help_scrolls_within_bounds_and_resets_on_reopen() {
        let mut app = App::new();
        app.dispatch(Action::ToggleHelp);
        assert_eq!(app.help_scroll(), 0);
        // Scrolling up at the top is a clamped no-op.
        app.dispatch(Action::HelpScrollUp);
        assert_eq!(app.help_scroll(), 0);
        app.dispatch(Action::HelpScrollDown);
        assert_eq!(app.help_scroll(), 1);
        // It never runs past the last help line, however many downs arrive.
        let max = (keymap::help_line_count() - 1) as u16;
        for _ in 0..keymap::help_line_count() + 5 {
            app.dispatch(Action::HelpScrollDown);
        }
        assert_eq!(app.help_scroll(), max, "clamped at the last line");
        // Closing and reopening starts back at the top.
        app.dispatch(Action::ToggleHelp);
        app.dispatch(Action::ToggleHelp);
        assert_eq!(app.help_scroll(), 0, "reopen resets to the top");
    }

    #[test]
    fn scroll_down_advances_the_history_offset_and_dirties() {
        use crate::conversation::{ConversationView, sample_message};
        use std::collections::HashSet;
        use tuigram_core::model::{FormattedText, MessageContent};

        let messages = (0..3)
            .map(|i| {
                sample_message(
                    i,
                    MessageContent::Text(FormattedText {
                        text: format!("m{i}"),
                        entities: Vec::new(),
                    }),
                )
            })
            .collect();
        let mut app =
            App::with_conversation(ConversationView::from_messages(messages, HashSet::new()));
        app.clear_dirty();

        app.dispatch(Action::ScrollDown);
        assert_eq!(app.conversation().offset(), 1);
        assert!(app.is_dirty());
    }

    #[test]
    fn composer_focus_maps_editing_keys_and_inserts_text() {
        let mut app = App::new();
        app.dispatch(Action::SetFocus(Focus::Composer));
        let mapped = |code| app.on_terminal_event(key(code, KeyModifiers::NONE));
        assert_eq!(mapped(KeyCode::Enter), Action::ComposerSubmit);
        assert_eq!(mapped(KeyCode::Backspace), Action::ComposerBackspace);
        assert_eq!(mapped(KeyCode::Left), Action::ComposerLeft);
        assert_eq!(mapped(KeyCode::Right), Action::ComposerRight);
        assert_eq!(mapped(KeyCode::Home), Action::ComposerHome);
        assert_eq!(mapped(KeyCode::End), Action::ComposerEnd);
        assert_eq!(mapped(KeyCode::Esc), Action::ComposerCancel);
        // An unbound printable key inserts rather than running a command.
        assert_eq!(mapped(KeyCode::Char('q')), Action::ComposerInput('q'));
    }

    #[test]
    fn typing_then_submitting_records_an_outbound_intent_and_dirties() {
        let mut app = App::new();
        app.dispatch(Action::ComposerInput('h'));
        app.dispatch(Action::ComposerInput('i'));
        assert_eq!(app.composer().text(), "hi");

        app.clear_dirty();
        app.dispatch(Action::ComposerSubmit);
        assert!(app.composer().is_empty(), "buffer consumed on send");
        assert!(app.is_dirty());
        // The submit becomes an intent the loop drains and routes to the seam (#116).
        assert_eq!(
            app.take_outbound(),
            Some(Submission::Send {
                text: "hi".to_owned()
            })
        );
        assert_eq!(app.take_outbound(), None, "drained once");
    }

    #[test]
    fn empty_submit_is_a_noop_and_does_not_repaint() {
        let mut app = App::new();
        app.clear_dirty();
        app.dispatch(Action::ComposerSubmit);
        assert!(app.composer().is_empty());
        assert!(!app.is_dirty(), "an empty send changes nothing");
        assert_eq!(app.take_outbound(), None, "nothing to dispatch");
    }

    #[test]
    fn select_next_advances_the_chat_selection_and_dirties() {
        use crate::chat_list::{ChatList, ChatListView, sample_chat};
        use tuigram_core::model::ChatListKind;

        let view = ChatListView::from_lists(vec![ChatList {
            kind: ChatListKind::Main,
            label: "Main".to_owned(),
            chats: vec![sample_chat(1, "A", 0), sample_chat(2, "B", 0)],
        }]);
        let mut app = App::with_chat_list(view);
        app.clear_dirty();

        app.dispatch(Action::SelectNext);
        assert_eq!(app.chat_list().selected(), 1);
        assert!(app.is_dirty());
    }

    #[test]
    fn projecting_chats_refreshes_the_pane_and_dirties() {
        use crate::chat_list::{ChatList, sample_chat};
        use tuigram_core::model::ChatListKind;

        // Stands in for the loop's read-back from the core ChatStore on a chat
        // signal: an owned projection handed to the pure App.
        let mut app = App::new();
        app.clear_dirty();
        app.project_chats(vec![ChatList {
            kind: ChatListKind::Main,
            label: "Main".to_owned(),
            chats: vec![sample_chat(1, "Alice", 3), sample_chat(2, "Bob", 0)],
        }]);
        assert!(app.is_dirty());
        assert_eq!(app.chat_list().active_chats().len(), 2);
        assert_eq!(
            app.chat_list().selected_chat().map(|c| c.title.as_str()),
            Some("Alice")
        );
    }

    #[test]
    fn projecting_a_conversation_fills_the_history_and_dirties() {
        use crate::conversation::sample_message;
        use tuigram_core::model::{FormattedText, MessageContent};

        let text = |id: i64| {
            sample_message(
                id,
                MessageContent::Text(FormattedText {
                    text: format!("m{id}"),
                    entities: Vec::new(),
                }),
            )
        };
        // Stands in for the loop's read-back of the open chat's MessageStore.
        let mut app = App::new();
        app.clear_dirty();
        app.project_conversation(10, vec![text(1), text(2)], HashSet::new());
        assert!(app.is_dirty());
        assert_eq!(app.conversation().len(), 2);
        assert_eq!(app.conversation().selected_message().map(|m| m.id), Some(1));
    }

    #[test]
    fn scrolling_up_at_the_top_requests_older_history_once() {
        use crate::conversation::sample_message;
        use tuigram_core::model::{FormattedText, MessageContent};

        let messages = (1..=2)
            .map(|i| {
                sample_message(
                    i,
                    MessageContent::Text(FormattedText {
                        text: format!("m{i}"),
                        entities: Vec::new(),
                    }),
                )
            })
            .collect();
        let mut app =
            App::with_conversation(ConversationView::from_messages(messages, HashSet::new()));
        // Move down so we are no longer at the top: a scroll-up there just moves.
        app.dispatch(Action::ScrollDown);
        app.dispatch(Action::ScrollUp);
        assert!(
            !app.take_wants_older_history(),
            "a normal scroll-up pages nothing"
        );

        // Now at the top (offset 0): a further scroll-up is the paging request.
        app.dispatch(Action::ScrollUp);
        assert!(
            app.take_wants_older_history(),
            "up at the top requests older history"
        );
        assert!(
            !app.take_wants_older_history(),
            "the request is cleared once taken"
        );
    }

    #[test]
    fn scrolling_up_on_an_empty_history_requests_nothing() {
        let mut app = App::new();
        app.dispatch(Action::ScrollUp);
        assert!(
            !app.take_wants_older_history(),
            "no history, nothing to page"
        );
    }

    // --- search & forward overlays (#84) ---

    use crate::search::SearchHit;

    /// An app with two chats and a two-hit search result set, sitting on the
    /// results overlay — the state a forward is started from.
    fn app_on_results() -> App {
        use crate::chat_list::{ChatList, ChatListView, sample_chat};
        use tuigram_core::model::ChatListKind;

        let view = ChatListView::from_lists(vec![ChatList {
            kind: ChatListKind::Main,
            label: "Main".to_owned(),
            chats: vec![sample_chat(1, "Alice", 0), sample_chat(2, "Bob", 0)],
        }]);
        let mut app = App::with_chat_list(view);
        app.dispatch(Action::SearchOpen);
        for c in "kenobi".chars() {
            app.dispatch(Action::SearchInput(c));
        }
        app.dispatch(Action::SearchSubmit);
        // The hits arrive from the core search (the loop's `set_search_results`) once
        // it completes; inject them to stand in for that delivery.
        app.inject_search_results(vec![
            SearchHit::new(1, 10, "Alice: hello"),
            SearchHit::new(2, 20, "Bob: kenobi"),
        ]);
        app
    }

    #[test]
    fn opening_search_enters_the_input_overlay_on_a_fresh_query() {
        let mut app = App::new();
        // Type into a search, cancel, reopen — the old query must not leak in.
        app.dispatch(Action::SearchOpen);
        app.dispatch(Action::SearchInput('x'));
        app.dispatch(Action::SearchCancel);
        app.dispatch(Action::SearchOpen);
        assert_eq!(app.overlay(), Overlay::SearchInput);
        assert_eq!(app.search().query(), "", "reopened search starts empty");
    }

    #[test]
    fn typing_a_query_then_submitting_moves_to_the_results_overlay() {
        let mut app = App::new();
        app.dispatch(Action::SearchOpen);
        for c in "hi".chars() {
            app.dispatch(Action::SearchInput(c));
        }
        assert_eq!(app.search().query(), "hi");
        app.dispatch(Action::SearchSubmit);
        assert_eq!(app.overlay(), Overlay::SearchResults);
    }

    #[test]
    fn navigating_results_moves_the_selection() {
        let mut app = app_on_results();
        assert_eq!(app.search().selected(), 0);
        app.dispatch(Action::ResultNext);
        assert_eq!(app.search().selected(), 1);
        app.dispatch(Action::ResultPrev);
        assert_eq!(app.search().selected(), 0);
    }

    #[test]
    fn submitting_an_empty_query_stays_on_the_input_with_no_intent() {
        let mut app = App::new();
        app.dispatch(Action::SearchOpen);
        app.dispatch(Action::SearchSubmit);
        assert_eq!(
            app.overlay(),
            Overlay::SearchInput,
            "blank query does not search"
        );
        assert_eq!(app.take_search_query(), None, "nothing to dispatch");
    }

    #[test]
    fn submitting_a_query_records_the_intent_clears_stale_hits_and_drains_once() {
        // The helper already submitted "kenobi" and (via inject) holds two hits.
        let mut app = app_on_results();
        assert_eq!(app.search().results().len(), 2);

        // A fresh search: the query becomes a pending intent and the stale hits clear
        // until the loop feeds the new ones back.
        app.dispatch(Action::SearchOpen);
        for c in "vader".chars() {
            app.dispatch(Action::SearchInput(c));
        }
        app.dispatch(Action::SearchSubmit);
        assert_eq!(app.overlay(), Overlay::SearchResults);
        assert!(
            app.search().results().is_empty(),
            "stale hits cleared until new ones land"
        );
        assert_eq!(app.take_search_query().as_deref(), Some("vader"));
        assert_eq!(app.take_search_query(), None, "the intent is drained once");
    }

    #[test]
    fn set_search_results_fills_the_overlay() {
        let mut app = App::new();
        app.set_search_results(vec![SearchHit::new(1, 5, "a hit")]);
        assert_eq!(app.search().results().len(), 1);
        assert_eq!(app.search().selected(), 0);
    }

    #[test]
    fn opening_a_hit_jumps_to_its_chat_and_focuses_the_history() {
        let mut app = app_on_results(); // chats Alice(1)/Bob(2); hits (1,10),(2,20)
        app.dispatch(Action::ResultNext); // select Bob's hit (chat 2, message 20)
        app.dispatch(Action::ResultOpen);
        assert_eq!(app.overlay(), Overlay::None, "overlay closed on jump");
        assert_eq!(
            app.focus(),
            Focus::History,
            "history focused so the loop opens it"
        );
        assert_eq!(
            app.chat_list().selected_chat().map(|c| c.id),
            Some(2),
            "the hit's chat is selected"
        );
    }

    /// A run of text messages with the given ids, for the jump projection tests.
    fn text_history(ids: &[i64]) -> Vec<Message> {
        use crate::conversation::sample_message;
        use tuigram_core::model::{FormattedText, MessageContent};
        ids.iter()
            .map(|&id| {
                sample_message(
                    id,
                    MessageContent::Text(FormattedText {
                        text: format!("m{id}"),
                        entities: Vec::new(),
                    }),
                )
            })
            .collect()
    }

    #[test]
    fn a_jump_waits_for_the_message_then_scrolls_to_it_when_it_loads() {
        let mut app = app_on_results();
        app.dispatch(Action::ResultOpen); // first hit: chat 1, message 10

        // The first projection of chat 1 does not carry message 10 yet: no jump.
        app.project_conversation(1, text_history(&[1, 2]), HashSet::new());
        assert_ne!(
            app.conversation().selected_message().map(|m| m.id),
            Some(10),
            "message not loaded, nothing to scroll to"
        );

        // The page carrying message 10 lands: the jump applies and clears.
        app.project_conversation(1, text_history(&[9, 10, 11]), HashSet::new());
        assert_eq!(
            app.conversation().selected_message().map(|m| m.id),
            Some(10)
        );
    }

    #[test]
    fn a_jump_is_dropped_when_a_different_chat_is_opened_first() {
        let mut app = app_on_results();
        app.dispatch(Action::ResultOpen); // hit for chat 1, message 10

        // The user opens chat 2 before chat 1's history arrives: the stale jump drops.
        app.project_conversation(2, text_history(&[7, 8]), HashSet::new());
        // Chat 1's history (with message 10) arrives later, but the jump is gone, so
        // the view lands fresh at the top (message 9) rather than chasing message 10.
        app.project_conversation(1, text_history(&[9, 10, 11]), HashSet::new());
        assert_eq!(app.conversation().selected_message().map(|m| m.id), Some(9));
    }

    #[test]
    fn forwarding_a_hit_opens_the_target_picker_with_that_message() {
        let mut app = app_on_results();
        app.dispatch(Action::ResultNext); // select Bob's hit (message 20)
        app.dispatch(Action::ForwardOpen);
        assert_eq!(app.overlay(), Overlay::Forward);
        assert_eq!(app.forward().message_ids(), &[20]);
        // The hit's chat (Bob, id 2) is the source the forward carries.
        assert_eq!(app.forward().source_chat_id(), 2);
        // The picker reuses the chat list as its target list.
        assert_eq!(
            app.forward().selected_target().map(|c| c.title.as_str()),
            Some("Alice")
        );
    }

    #[test]
    fn forward_picks_a_target_then_confirms_records_the_intent() {
        let mut app = app_on_results(); // selected hit: Alice's (chat 1, message 10)
        app.dispatch(Action::ForwardOpen);
        app.dispatch(Action::ForwardNext);
        assert_eq!(
            app.forward().selected_target().map(|c| c.title.as_str()),
            Some("Bob")
        );
        app.dispatch(Action::ForwardConfirm);
        assert_eq!(app.overlay(), Overlay::None, "confirm closes the modal");
        // The confirmed forward is recorded for the loop: message 10 from its source
        // chat (1) into the picked target (Bob, 2).
        assert_eq!(
            app.take_forward(),
            Some(ForwardIntent {
                from_chat_id: 1,
                message_ids: vec![10],
                to_chat_id: 2,
            })
        );
        assert_eq!(app.take_forward(), None, "the intent is drained once");
    }

    #[test]
    fn cancelling_a_forward_returns_to_the_results() {
        let mut app = app_on_results();
        app.dispatch(Action::ForwardOpen);
        app.dispatch(Action::ForwardCancel);
        assert_eq!(app.overlay(), Overlay::SearchResults);
    }

    #[test]
    fn forwarding_with_no_hits_is_a_noop() {
        let mut app = App::new();
        app.dispatch(Action::SearchOpen);
        app.dispatch(Action::SearchInput('q')); // a query, but the search returns nothing
        app.dispatch(Action::SearchSubmit); // empty results
        app.dispatch(Action::ForwardOpen);
        assert_eq!(
            app.overlay(),
            Overlay::SearchResults,
            "no hit to forward, stays put"
        );
    }

    /// An app with a two-chat list and an open conversation (chat 1) — the state a
    /// forward is started from in the history pane (`f`).
    fn app_on_conversation() -> App {
        use crate::chat_list::{ChatList, ChatListView, sample_chat};
        use std::collections::HashSet;
        use tuigram_core::model::ChatListKind;

        let view = ChatListView::from_lists(vec![ChatList {
            kind: ChatListKind::Main,
            label: "Main".to_owned(),
            chats: vec![sample_chat(1, "Alice", 0), sample_chat(2, "Bob", 0)],
        }]);
        let mut app = App::with_chat_list(view);
        app.project_conversation(1, text_history(&[10, 11]), HashSet::new());
        app
    }

    #[test]
    fn forwarding_the_selected_history_message_opens_the_picker_from_the_open_chat() {
        let mut app = app_on_conversation();
        // The message the history pane has selected (sample_message pins chat_id to 1).
        let selected = app.conversation().selected_message().map(|m| m.id).unwrap();
        app.dispatch(Action::ForwardMessage);
        assert_eq!(app.overlay(), Overlay::Forward);
        assert_eq!(app.forward().message_ids(), &[selected]);
        assert_eq!(
            app.forward().source_chat_id(),
            1,
            "sourced from the open chat"
        );
        // The picker reuses the chat list as its target list.
        assert_eq!(
            app.forward().selected_target().map(|c| c.title.as_str()),
            Some("Alice")
        );
    }

    #[test]
    fn cancelling_a_history_forward_returns_to_the_conversation() {
        let mut app = app_on_conversation();
        app.dispatch(Action::ForwardMessage);
        assert_eq!(app.overlay(), Overlay::Forward);
        app.dispatch(Action::ForwardCancel);
        assert_eq!(
            app.overlay(),
            Overlay::None,
            "cancel lands back on the conversation, not the search results"
        );
    }

    #[test]
    fn forwarding_from_an_empty_history_is_a_noop() {
        let mut app = App::new();
        app.dispatch(Action::ForwardMessage);
        assert_eq!(
            app.overlay(),
            Overlay::None,
            "no selected message, nothing to forward"
        );
    }

    #[test]
    fn search_keys_resolve_through_the_overlay_not_the_panes() {
        let mut app = App::new();
        app.dispatch(Action::SearchOpen);
        // `j` would select a chat in browse mode; in the search input it types.
        assert_eq!(
            app.on_terminal_event(key(KeyCode::Char('j'), KeyModifiers::NONE)),
            Action::SearchInput('j')
        );
    }

    // --- media, reactions & pins (#85) ---

    use tuigram_core::model::ReactionKind;

    /// An app whose history holds two text messages, the first (oldest, at the top)
    /// the selected one.
    fn app_with_history() -> App {
        use crate::conversation::{ConversationView, sample_message};
        use std::collections::HashSet;
        use tuigram_core::model::{FormattedText, MessageContent};

        let messages = (1..=2)
            .map(|i| {
                sample_message(
                    i,
                    MessageContent::Text(FormattedText {
                        text: format!("m{i}"),
                        entities: Vec::new(),
                    }),
                )
            })
            .collect();
        App::with_conversation(ConversationView::from_messages(messages, HashSet::new()))
    }

    #[test]
    fn pin_toggles_the_selected_message_and_dirties() {
        let mut app = app_with_history();
        let id = app.conversation().selected_message().unwrap().id;
        app.clear_dirty();
        app.dispatch(Action::PinToggle);
        assert!(app.conversation().is_pinned(id), "pinned");
        assert!(app.is_dirty());
        app.dispatch(Action::PinToggle);
        assert!(!app.conversation().is_pinned(id), "unpinned again");
    }

    #[test]
    fn pin_on_an_empty_history_is_a_noop() {
        let mut app = App::new();
        app.clear_dirty();
        app.dispatch(Action::PinToggle);
        assert!(!app.is_dirty(), "no selected message, nothing changes");
    }

    #[test]
    fn reacting_opens_the_picker_then_confirm_toggles_the_reaction() {
        let mut app = app_with_history();
        let id = app.conversation().selected_message().unwrap().id;
        app.dispatch(Action::ReactionOpen);
        assert_eq!(app.overlay(), Overlay::Reaction);
        // Pick the second emoji, confirm — it lands on the selected message.
        app.dispatch(Action::ReactionNext);
        let chosen = app.reaction().selected_emoji();
        app.dispatch(Action::ReactionConfirm);
        assert_eq!(app.overlay(), Overlay::None, "confirm closes the picker");
        let message = app.conversation().messages().iter().find(|m| m.id == id);
        let reactions = &message.unwrap().reactions;
        assert_eq!(reactions.len(), 1);
        assert!(reactions[0].is_chosen, "our reaction is recorded");
        assert_eq!(reactions[0].kind, ReactionKind::Emoji(chosen.to_owned()));
    }

    #[test]
    fn reacting_with_no_selected_message_does_not_open() {
        let mut app = App::new();
        app.dispatch(Action::ReactionOpen);
        assert_eq!(
            app.overlay(),
            Overlay::None,
            "nothing to react to, stays closed"
        );
    }

    #[test]
    fn cancelling_the_reaction_picker_changes_nothing() {
        let mut app = app_with_history();
        app.dispatch(Action::ReactionOpen);
        app.dispatch(Action::ReactionCancel);
        assert_eq!(app.overlay(), Overlay::None);
        assert!(
            app.conversation()
                .messages()
                .iter()
                .all(|m| m.reactions.is_empty()),
            "cancel adds no reaction"
        );
    }

    #[test]
    fn attaching_opens_a_fresh_prompt_and_edits_the_fields() {
        let mut app = app_with_history();
        app.dispatch(Action::AttachOpen);
        assert_eq!(app.overlay(), Overlay::SendMedia);
        for c in "/tmp/a.png".chars() {
            app.dispatch(Action::AttachInput(c));
        }
        app.dispatch(Action::AttachToggleField);
        for c in "hi".chars() {
            app.dispatch(Action::AttachInput(c));
        }
        assert_eq!(app.media().path(), "/tmp/a.png");
        assert_eq!(app.media().caption(), "hi");
    }

    #[test]
    fn confirming_an_empty_attach_keeps_the_prompt_open() {
        let mut app = app_with_history();
        app.dispatch(Action::AttachOpen);
        app.dispatch(Action::AttachConfirm);
        assert_eq!(
            app.overlay(),
            Overlay::SendMedia,
            "no path, nothing to send"
        );
        // A path makes it sendable; confirm then closes.
        for c in "/tmp/a.png".chars() {
            app.dispatch(Action::AttachInput(c));
        }
        app.dispatch(Action::AttachConfirm);
        assert_eq!(app.overlay(), Overlay::None);
    }

    #[test]
    fn reopening_attach_starts_from_an_empty_prompt() {
        let mut app = app_with_history();
        app.dispatch(Action::AttachOpen);
        app.dispatch(Action::AttachInput('x'));
        app.dispatch(Action::AttachCancel);
        app.dispatch(Action::AttachOpen);
        assert_eq!(app.media().path(), "", "reopened prompt starts empty");
    }

    // --- secret chats & chat-action indicators (#87) ---

    use crate::secret::SecretLifecycle;

    /// An app whose chat list holds one chat, of `kind`, optionally carrying the
    /// secret-chat `state`. The selection lands on it.
    fn app_with_one_chat(
        kind: tuigram_core::model::ChatKind,
        state: Option<tuigram_core::model::SecretChatState>,
    ) -> App {
        use crate::chat_list::{ChatList, ChatListView, sample_chat};
        use tuigram_core::model::ChatListKind;

        let mut chat = sample_chat(5, "Mallory", 0);
        chat.kind = kind;
        let mut view = ChatListView::from_lists(vec![ChatList {
            kind: ChatListKind::Main,
            label: "Main".to_owned(),
            chats: vec![chat],
        }]);
        if let Some(state) = state {
            view.set_secret_state(5, state);
        }
        App::with_chat_list(view)
    }

    #[test]
    fn secret_open_offers_to_start_from_a_private_chat() {
        use tuigram_core::model::ChatKind;
        let mut app = app_with_one_chat(ChatKind::Private { user_id: 7 }, None);
        app.dispatch(Action::SecretOpen);
        assert_eq!(app.overlay(), Overlay::SecretChat);
        assert_eq!(
            app.secret().map(|p| p.lifecycle()),
            Some(SecretLifecycle::Start { user_id: 7 })
        );
    }

    #[test]
    fn secret_open_offers_to_close_an_open_secret_chat() {
        use tuigram_core::model::{ChatKind, SecretChatState};
        let mut app = app_with_one_chat(
            ChatKind::Secret {
                secret_chat_id: 9,
                user_id: 7,
            },
            Some(SecretChatState::Ready),
        );
        app.dispatch(Action::SecretOpen);
        assert_eq!(app.overlay(), Overlay::SecretChat);
        assert_eq!(
            app.secret().map(|p| p.lifecycle()),
            Some(SecretLifecycle::Close { secret_chat_id: 9 })
        );
    }

    #[test]
    fn secret_open_on_a_group_opens_nothing() {
        use tuigram_core::model::ChatKind;
        let mut app = app_with_one_chat(ChatKind::BasicGroup { basic_group_id: 1 }, None);
        app.clear_dirty();
        app.dispatch(Action::SecretOpen);
        assert_eq!(
            app.overlay(),
            Overlay::None,
            "no lifecycle action, no modal"
        );
        assert!(app.secret().is_none());
        assert!(!app.is_dirty(), "nothing changed");
    }

    #[test]
    fn confirming_the_secret_prompt_closes_it() {
        use tuigram_core::model::ChatKind;
        let mut app = app_with_one_chat(ChatKind::Private { user_id: 7 }, None);
        app.dispatch(Action::SecretOpen);
        app.dispatch(Action::SecretConfirm);
        assert_eq!(app.overlay(), Overlay::None, "confirm closes the modal");
        assert!(app.secret().is_none(), "prompt state cleared");
    }

    #[test]
    fn cancelling_the_secret_prompt_acts_on_nothing() {
        use tuigram_core::model::ChatKind;
        let mut app = app_with_one_chat(ChatKind::Private { user_id: 7 }, None);
        app.dispatch(Action::SecretOpen);
        app.dispatch(Action::SecretCancel);
        assert_eq!(app.overlay(), Overlay::None);
        assert!(app.secret().is_none());
    }

    #[test]
    fn secret_keys_resolve_through_the_overlay_not_the_panes() {
        use tuigram_core::model::ChatKind;
        let mut app = app_with_one_chat(ChatKind::Private { user_id: 7 }, None);
        app.dispatch(Action::SecretOpen);
        // `s` would reopen the lifecycle in the chat list; inside the modal Enter
        // confirms and other keys are inert — the overlay owns input.
        assert_eq!(
            app.on_terminal_event(key(KeyCode::Enter, KeyModifiers::NONE)),
            Action::SecretConfirm
        );
    }

    #[test]
    fn history_keys_resolve_through_the_send_media_overlay() {
        let mut app = app_with_history();
        app.dispatch(Action::SetFocus(Focus::History));
        app.dispatch(Action::AttachOpen);
        // `a` would open attach in the history; inside the prompt it types.
        assert_eq!(
            app.on_terminal_event(key(KeyCode::Char('a'), KeyModifiers::NONE)),
            Action::AttachInput('a')
        );
    }

    // --- status bar & notifications (#88) ---

    use crate::status::{ConnectionState, Notice};

    #[test]
    fn the_app_starts_connecting_and_folds_connection_updates() {
        let mut app = App::new();
        assert_eq!(app.connection(), ConnectionState::Connecting);
        app.clear_dirty();

        app.set_connection(ConnectionState::Ready);
        assert_eq!(app.connection(), ConnectionState::Ready);
        assert!(app.is_dirty(), "a state change repaints");

        // Setting the same state again is a no-op that does not repaint.
        app.clear_dirty();
        app.set_connection(ConnectionState::Ready);
        assert!(!app.is_dirty());
    }

    #[test]
    fn notify_enqueues_a_toast_and_repaints() {
        let mut app = App::new();
        assert!(app.notifications().current().is_none());
        app.clear_dirty();

        app.notify(Notice::error("send", Some("FLOOD_WAIT")));
        assert!(app.is_dirty());
        assert_eq!(
            app.notifications().current().unwrap().line(),
            "✗ send failed (FLOOD_WAIT)"
        );
    }

    #[test]
    fn dismiss_drops_the_current_toast_and_is_a_noop_when_empty() {
        let mut app = App::new();
        app.notify(Notice::info("one"));
        app.notify(Notice::info("two"));

        app.dispatch(Action::NoticeDismiss);
        assert_eq!(
            app.notifications().current().unwrap().line(),
            "ℹ two",
            "the next toast shows"
        );

        app.dispatch(Action::NoticeDismiss);
        assert!(app.notifications().current().is_none());

        // Dismissing with nothing showing changes nothing.
        app.clear_dirty();
        app.dispatch(Action::NoticeDismiss);
        assert!(!app.is_dirty());
    }

    #[test]
    fn ctrl_g_dismisses_a_notification_from_a_pane() {
        let app = App::new(); // lands on the chat list
        assert_eq!(
            app.on_terminal_event(key(KeyCode::Char('g'), KeyModifiers::CONTROL)),
            Action::NoticeDismiss
        );
    }
}
