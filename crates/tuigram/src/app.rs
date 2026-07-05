//! The application state (`App`) and the `Action` vocabulary every input is
//! reduced into. `App` is the single source of truth: each `tokio::select!` arm
//! translates its source into an [`Action`], [`App::dispatch`] applies it and
//! marks the frame dirty, and the loop repaints from the new state. Nothing here
//! touches the terminal or awaits — it stays a pure, unit-testable reducer.

use std::collections::{HashMap, HashSet};
use std::fmt;

use crossterm::event::{Event, MouseButton, MouseEvent, MouseEventKind};
use ratatui_image::protocol::Protocol;
use tuigram_core::StorageSettings;
use tuigram_core::model::{File, Message, MessageContent, OutgoingMedia, SecretChatState, Sender};

use crate::chat_list::{ChatList, ChatListView};
use crate::composer::{Composer, Submission};
use crate::contact_picker::{ContactHit, ContactPickerView};
use crate::conversation::{ConversationView, DeleteIntent, DeletePrompt, PinIntent, SenderLabel};
use crate::event::AppEvent;
use crate::forward::{ForwardIntent, ForwardView};
use crate::keymap::{self, Focus, Overlay};
use crate::mediaform::MediaDraft;
use crate::reactions::{ReactionIntent, ReactionPicker};
use crate::search::SearchView;
use crate::secret::{SecretChatPrompt, SecretLifecycle};
use crate::settingsform::SettingsDraft;
use crate::status::{ConnectionState, Notice, Notifications};
use crate::terminal::AvatarSupport;

/// Built avatar protocols, keyed by sender user id (#201), so each user's photo
/// is decoded and handed to the `Picker` at most once per process lifetime —
/// same shape as `downloads: HashMap<i32, File>` in `conversation.rs`.
/// `ratatui_image::protocol::Protocol` has no `Debug` impl, so this wraps the
/// map in a type with a hand-written one instead of blocking `App`'s derive.
#[derive(Default)]
pub struct AvatarCache(HashMap<i64, Protocol>);

impl AvatarCache {
    /// The built protocol for a user, if their avatar has already been
    /// encoded this session.
    pub fn get(&self, user_id: i64) -> Option<&Protocol> {
        self.0.get(&user_id)
    }

    /// Record a newly built protocol for a user, replacing any previous one.
    pub fn insert(&mut self, user_id: i64, protocol: Protocol) {
        self.0.insert(user_id, protocol);
    }
}

impl fmt::Debug for AvatarCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AvatarCache")
            .field("cached_avatars", &self.0.len())
            .finish()
    }
}

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
    /// Open the chat with this id and focus the history pane (a mouse click on a
    /// chat-list row) — the click-equivalent of selecting it, then pressing Enter.
    OpenChat(i64),
    /// Scroll the history so this message id is at the top and focus the history
    /// pane (a mouse click on a history row). Selection and scroll offset are the
    /// same thing here (see [`ConversationView::select_message`]), so this also
    /// brings the clicked message to the top of the pane, same as the wheel does.
    SelectMessageAt(i64),
    /// Move the composer cursor to a character index and focus the composer (a
    /// mouse click on the composer line) — the click-equivalent of tabbing into
    /// the composer and pressing Left/Right/Home/End to land the cursor there.
    ComposerClickAt(usize),
    /// Select and confirm the row at this index in the open modal list overlay —
    /// search results, forward targets, the reaction palette, or contact
    /// results (a mouse click on that overlay's row, #217). Resolved against
    /// whichever overlay is open; a no-op if it has no selectable list.
    OverlayRowClick(usize),
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
    /// Jump the conversation history to the bottom-anchored newest message (`G` /
    /// `End`), the way a chat client's "go to bottom" does (#158). Also re-arms
    /// auto-follow, since it lands the view back on the newest anchor (#159).
    JumpToNewest,
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
    /// Move the reaction-picker selection to the next emoji (palette mode).
    ReactionNext,
    /// Move the reaction-picker selection to the previous emoji (palette mode).
    ReactionPrev,
    /// A character typed in the reaction overlay. In palette mode it is a shortcut
    /// (`j`/`k` move, `c` opens the custom-emoji line); in custom mode it is appended
    /// to the custom-emoji buffer. The reducer disambiguates by the picker's mode.
    ReactionKey(char),
    /// Delete the last character of the custom-emoji buffer (Backspace); a no-op in
    /// palette mode.
    ReactionBackspace,
    /// Toggle the picked emoji — the highlighted palette one, or the typed custom one
    /// — on the selected message and close the picker, recording the core add/remove
    /// as a pending intent (#119).
    ReactionConfirm,
    /// Dismiss the reaction overlay: in custom mode this returns to the palette; in
    /// palette mode it closes the overlay.
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
    /// Confirm the secret-chat lifecycle action and close the prompt. Records the
    /// chosen action for the loop to dispatch on the core seam
    /// (`create_new_secret_chat` / `close_secret_chat`) (#121).
    SecretConfirm,
    /// Cancel the secret-chat prompt without acting.
    SecretCancel,
    /// Open the contact-search picker on a fresh, empty query (`n` in the chat
    /// list, #197) — the entry point for starting a secret chat with a contact
    /// not already in the chat list.
    ContactSearchOpen,
    /// Insert a typed character into the contact-search query at the cursor.
    ContactSearchInput(char),
    /// Delete the character before the contact-search query cursor (Backspace).
    ContactSearchBackspace,
    /// Move the contact-search query cursor one character left.
    ContactSearchLeft,
    /// Move the contact-search query cursor one character right.
    ContactSearchRight,
    /// Move the contact-search query cursor to the start of the line.
    ContactSearchHome,
    /// Move the contact-search query cursor to the end of the line.
    ContactSearchEnd,
    /// Run the typed query and switch to the results list. The loop dispatches
    /// the core `search_contacts` call; for now the results are whatever has
    /// been injected.
    ContactSearchSubmit,
    /// Close the contact-search overlay (from either the query line or the
    /// results).
    ContactSearchCancel,
    /// Move the contact-search results selection down one hit.
    ContactResultNext,
    /// Move the contact-search results selection up one hit.
    ContactResultPrev,
    /// Confirm the selected contact: open the secret-chat confirm
    /// ([`Overlay::SecretChat`]) for that user, reusing the same "are you sure"
    /// step the chat-list-scoped lifecycle uses. A no-op with no hits.
    ContactResultConfirm,
    /// Open the retention settings editor (#146), pre-filled with the policy in
    /// effect.
    SettingsOpen,
    /// Insert a typed character into the focused settings field.
    SettingsInput(char),
    /// Delete the character before the focused settings field's cursor.
    SettingsBackspace,
    /// Move the focused settings field's cursor one character left.
    SettingsLeft,
    /// Move the focused settings field's cursor one character right.
    SettingsRight,
    /// Move the focused settings field's cursor to the start of the line.
    SettingsHome,
    /// Move the focused settings field's cursor to the end of the line.
    SettingsEnd,
    /// Move settings editing to the next field (Tab).
    SettingsToggleField,
    /// Validate and confirm the settings edit. A valid edit updates the in-memory
    /// policy and records it for the loop to persist and apply live (#146); an
    /// invalid value is rejected in place, keeping the overlay open.
    SettingsConfirm,
    /// Cancel the settings editor without saving.
    SettingsCancel,
    /// Start a reply to the selected history message (`r`): put the composer into
    /// reply mode against it and focus the composer (#195). A no-op on an empty
    /// history. Reuses the already-built [`Composer::reply_to`](crate::composer::Composer::reply_to).
    ReplyMessage,
    /// Start editing the selected history message (`e`): pre-fill the composer with
    /// its text and focus the composer (#195). Only our own text messages are
    /// editable; anything else surfaces a toast and does nothing.
    EditMessage,
    /// Open the delete-confirm overlay for the selected history message (`d`), a
    /// no-op on an empty history (#195).
    DeleteMessage,
    /// Flip the delete scope between "for me" and "for everyone" in the confirm
    /// (Tab); only meaningful for our own message (#195).
    DeleteToggleScope,
    /// Confirm the delete at the chosen scope: record the intent for the loop and
    /// close the overlay (#195).
    DeleteConfirm,
    /// Cancel the delete confirm without deleting (#195).
    DeleteCancel,
    /// Save / download the selected history message's media (`S`): record its file
    /// id for the loop to reveal a local path or start the download (#195). A toast
    /// with no downloadable media.
    SaveMedia,
    /// Copy the selected history message's text to the OS clipboard (`y`, #197).
    /// Records the text for the loop to copy — `App` cannot reach the OS clipboard
    /// directly and stays pure. A toast when there is no text to copy (an empty
    /// history, or a non-text message).
    CopyMessage,
    /// Re-query the chat list after a dropped-update gap (`Ctrl-R`), recording the
    /// request for the loop to run against core (#195).
    Resync,
    /// Open the logout confirm (`Ctrl-Q`) (#195).
    LogoutOpen,
    /// Confirm the logout: record the request for the loop, which ends the session
    /// and exits (#195).
    LogoutConfirm,
    /// Cancel the logout confirm without logging out (#195).
    LogoutCancel,
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
    /// The contact-search picker's state (#197): the query line and the matching
    /// contacts it renders from.
    contacts: ContactPickerView,
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
    /// The download-cache retention policy currently in effect (#146). Seeded from
    /// `settings.toml` at startup ([`set_storage_settings`](Self::set_storage_settings)),
    /// it pre-fills the settings editor and is updated in place when an edit is
    /// confirmed, so reopening the editor shows the live values. `App` never touches
    /// the file — the loop persists and applies the confirmed change.
    storage: StorageSettings,
    /// The settings editor's state (#146): the four retention inputs being edited.
    /// Reset from [`storage`](Self::storage) each time the editor opens.
    settings: SettingsDraft,
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
    /// Set when a composer edit leaves unsent text in the buffer (#197): a pulse,
    /// not the text itself, since the loop already knows the open chat and reads
    /// the composer's buffer directly. The loop reads and clears it each tick via
    /// [`take_wants_typing_ping`](Self::take_wants_typing_ping), throttling the
    /// actual `sendChatAction(typing)` broadcast so it isn't refired on every
    /// keystroke.
    wants_typing_ping: bool,
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
    /// A submitted contact-search query awaiting dispatch to core (#197). Like
    /// `pending_search`, `App` records the intent and the loop drains it via
    /// [`take_contact_search`](Self::take_contact_search), runs
    /// `search_contacts`, and feeds the resolved hits back through
    /// [`set_contact_results`](Self::set_contact_results).
    pending_contact_search: Option<String>,
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
    /// A confirmed reaction toggle awaiting dispatch to core (#119). `App` reflects
    /// the toggle optimistically and records the intent; the loop drains it via
    /// [`take_reaction`](Self::take_reaction), routing it to
    /// [`ReactionRequests`](tuigram_core::ReactionRequests)' add/remove.
    pending_reaction: Option<ReactionIntent>,
    /// A confirmed pin toggle awaiting dispatch to core (#119). `App` flips the
    /// pinned set optimistically and records the intent; the loop drains it via
    /// [`take_pin`](Self::take_pin), routing it to
    /// [`PinRequests`](tuigram_core::PinRequests)' pin/unpin.
    pending_pin: Option<PinIntent>,
    /// A confirmed send-media attachment awaiting dispatch to core (#120). `App`
    /// stays pure and never touches the `Client`, so confirming the attach prompt
    /// builds the [`OutgoingMedia`] and lands it here; the loop drains it via
    /// [`take_media`](Self::take_media) and routes it to
    /// [`SendRequests::send_media`](tuigram_core::SendRequests::send_media), the
    /// upload streaming back through the file store exactly like the send path (#116).
    pending_media: Option<OutgoingMedia>,
    /// A confirmed secret-chat lifecycle action awaiting dispatch to core (#121).
    /// Confirming the secret-chat prompt records the chosen [`SecretLifecycle`]
    /// here (never any key material); the loop drains it via
    /// [`take_secret`](Self::take_secret) and routes it to
    /// [`SecretChatRequests`](tuigram_core::SecretChatRequests)' create/close. The
    /// resulting `updateSecretChat`/`updateNewChat` fold back and re-project.
    pending_secret: Option<SecretLifecycle>,
    /// A confirmed retention edit awaiting the loop (#146). Confirming the settings
    /// editor updates the in-memory [`storage`](Self::storage) and lands the new
    /// policy here; the loop drains it via [`take_settings`](Self::take_settings),
    /// swaps its live retention (the next sweep honours it), and persists it to
    /// `settings.toml`.
    pending_settings: Option<StorageSettings>,
    /// The delete-confirm overlay's state (#195): the message a `d` targets and the
    /// chosen scope. `Some` only while [`Overlay::DeleteConfirm`] is open.
    delete: Option<DeletePrompt>,
    /// A confirmed delete awaiting dispatch to core (#195). Confirming the delete
    /// overlay records the target and scope here; the loop drains it via
    /// [`take_delete`](Self::take_delete) and routes it to
    /// [`DeleteRequests::delete`](tuigram_core::messages::DeleteRequests). No
    /// optimistic change — the real `updateDeleteMessages` folds and re-projects.
    pending_delete: Option<DeleteIntent>,
    /// A file id whose media the user asked to save (`S`) (#195). `App` stays pure
    /// and cannot read the file store, so it records the id; the loop drains it via
    /// [`take_save`](Self::take_save), reveals the local path if the file is already
    /// present, or starts the download otherwise.
    pending_save: Option<i32>,
    /// Text the user asked to copy (`y`) (#197). `App` cannot reach the OS
    /// clipboard, so it records the text; the loop drains it via
    /// [`take_copy`](Self::take_copy) and writes it out, toasting the result.
    pending_copy: Option<String>,
    /// Set when the user asked to resync (`Ctrl-R`) (#195). The loop reads and
    /// clears it via [`take_resync`](Self::take_resync) and re-queries the chat list.
    pending_resync: bool,
    /// Set when the user confirmed a logout (`Ctrl-Q`) (#195). The loop reads and
    /// clears it via [`take_logout`](Self::take_logout), ends the session, and exits.
    pending_logout: bool,
    /// The graphics-protocol capability detected once at startup (#201), seeded
    /// via [`set_avatar_support`](Self::set_avatar_support) from
    /// `TerminalGuard::avatar_support` and read-only afterward. `None` (halfblocks
    /// or no real terminal) renders today's #194 plain header with no avatar
    /// gutter; only `Graphics` unlocks it (Stage 3).
    avatar_support: AvatarSupport,
    /// Built avatar protocols for senders seen this session (#201), populated by
    /// the render path via [`cache_avatar`](Self::cache_avatar) as `drive_avatars`
    /// finishes encoding each sender's photo.
    avatar_cache: AvatarCache,
    /// The pane rectangles the last render drew into (#161/#162), recorded by the
    /// loop after each `draw` via [`set_pane_layout`](Self::set_pane_layout) so a
    /// mouse event can be hit-tested to a pane without re-running layout. Default
    /// (all-zero) rects until the first frame, which hit-test to `None` and so map
    /// mouse events to `Noop`.
    pane_layout: crate::ui::PaneLayout,
    /// Row → chat id map the last render drew, recorded via
    /// [`set_chat_rows`](Self::set_chat_rows), so a click on an actual chat row
    /// can open that chat directly. Empty until the first frame, which hit-tests
    /// every row to `None` and so falls back to focusing the pane.
    chat_rows: crate::ui::ChatRows,
    /// Row-range → message id map the last render drew, recorded via
    /// [`set_history_rows`](Self::set_history_rows), so a click on an actual
    /// history row can select that message directly. Empty until the first
    /// frame, same fallback as `chat_rows`.
    history_rows: crate::ui::HistoryRows,
    /// Row/column → list-index map the open overlay's last render drew,
    /// recorded via [`set_overlay_rows`](Self::set_overlay_rows), so a click on
    /// an actual overlay row can select-and-confirm it directly (#217). Empty
    /// when no overlay is open, or the open one has no selectable list.
    overlay_rows: crate::ui::OverlayRows,
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

    /// The contact-search picker's state, for rendering the query line and
    /// matching contacts (#197).
    pub fn contacts(&self) -> &ContactPickerView {
        &self.contacts
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

    /// The settings editor's state, for rendering the retention fields (#146).
    pub fn settings(&self) -> &SettingsDraft {
        &self.settings
    }

    /// The delete-confirm prompt's state, for rendering the overlay (#195); `None`
    /// when the confirm is not open.
    pub fn delete(&self) -> Option<&DeletePrompt> {
        self.delete.as_ref()
    }

    /// Seed the in-effect retention policy from `settings.toml` at startup (#146), so
    /// the editor opens pre-filled with the live values. The loop calls this once
    /// after loading settings; `App` keeps it only to fill and reflect the editor,
    /// never to touch the file.
    pub fn set_storage_settings(&mut self, settings: StorageSettings) {
        self.storage = settings;
    }

    /// Seed the graphics-protocol capability detected once at startup (#201).
    /// The loop calls this immediately after `TerminalGuard::new` succeeds, with
    /// a clone of `TerminalGuard::avatar_support` (the guard keeps its own copy
    /// for the rest of the process's lifetime) — mirrors
    /// [`set_storage_settings`](Self::set_storage_settings)'s seed-once shape.
    pub fn set_avatar_support(&mut self, support: AvatarSupport) {
        self.avatar_support = support;
    }

    /// The graphics-protocol capability in effect, for the render path to
    /// decide whether to draw an avatar gutter at all.
    pub fn avatar_support(&self) -> &AvatarSupport {
        &self.avatar_support
    }

    /// The built protocol for a sender's avatar, if already encoded this
    /// session.
    pub fn cached_avatar(&self, user_id: i64) -> Option<&Protocol> {
        self.avatar_cache.get(user_id)
    }

    /// Record a newly built avatar protocol for a sender (`drive_avatars`,
    /// #201), so the next render draws it instead of a blank gutter.
    pub fn cache_avatar(&mut self, user_id: i64, protocol: Protocol) {
        self.avatar_cache.insert(user_id, protocol);
        self.dirty = true;
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

    /// Record which chats are private **bot** chats (#160), for the 🤖 row marker.
    /// The loop resolves the set from the core user store on each chat-list signal
    /// and hands it here right after [`project_chats`](Self::project_chats), so `App`
    /// stays pure — the same split as [`project_secret_states`](Self::project_secret_states).
    pub fn project_bot_chats(&mut self, ids: HashSet<i64>) {
        self.chat_list.set_bot_chats(ids);
        self.dirty = true;
    }

    /// Re-project the conversation pane from the core
    /// [`MessageStore`](tuigram_core::messages::MessageStore) (#114). The loop reads
    /// the open `chat_id`'s history and pinned ids back from the `Client` on a
    /// message signal (or a freshly-merged history page) and hands the owned
    /// snapshot here, so `App` stays pure — the same split as
    /// [`project_chats`](Self::project_chats). [`ConversationView::project`] keeps
    /// the selected message under the cursor when refreshing the same chat, and
    /// starts fresh at the top when a different chat is opened. `fresh_open` marks
    /// the one call that is a genuine open (#164) — the loop's own open/close
    /// tracking, not derivable from `chat_id` alone (see `project`'s doc).
    #[allow(clippy::too_many_arguments)]
    pub fn project_conversation(
        &mut self,
        chat_id: i64,
        messages: Vec<Message>,
        pinned: HashSet<i64>,
        senders: HashMap<Sender, SenderLabel>,
        last_read_inbox: i64,
        last_read_outbox: i64,
        fresh_open: bool,
    ) {
        self.conversation.project(
            chat_id,
            messages,
            pinned,
            senders,
            last_read_inbox,
            last_read_outbox,
            fresh_open,
        );
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

    /// Take the pending typing-ping pulse, if any (#197). The loop reads and
    /// clears this each tick: `true` means the composer holds unsent text as of
    /// the last edit, so (throttled) the loop broadcasts `sendChatAction(typing)`
    /// for the open chat; `false` means no composer edit was made since the last
    /// drain.
    pub fn take_wants_typing_ping(&mut self) -> bool {
        std::mem::take(&mut self.wants_typing_ping)
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

    /// Take the pending contact-search query, if any (#197). The loop drains
    /// this each tick, runs `search_contacts`, resolves each hit's display name,
    /// and feeds the results back through
    /// [`set_contact_results`](Self::set_contact_results); `None` means nothing
    /// was submitted since the last drain.
    pub fn take_contact_search(&mut self) -> Option<String> {
        self.pending_contact_search.take()
    }

    /// Take the pending forward, if any (#118). The loop drains this each tick and
    /// dispatches it to
    /// [`ForwardRequests::forward_messages`](tuigram_core::messages::ForwardRequests::forward_messages);
    /// `None` means no forward was confirmed since the last drain.
    pub fn take_forward(&mut self) -> Option<ForwardIntent> {
        self.pending_forward.take()
    }

    /// Take the pending reaction toggle, if any (#119). The loop drains this each
    /// tick and dispatches it to
    /// [`ReactionRequests`](tuigram_core::ReactionRequests)' add/remove on the
    /// selected message; `None` means no reaction was confirmed since the last drain.
    pub fn take_reaction(&mut self) -> Option<ReactionIntent> {
        self.pending_reaction.take()
    }

    /// Take the pending pin toggle, if any (#119). The loop drains this each tick and
    /// dispatches it to [`PinRequests`](tuigram_core::PinRequests)' pin/unpin on the
    /// selected message; `None` means no pin was toggled since the last drain.
    pub fn take_pin(&mut self) -> Option<PinIntent> {
        self.pending_pin.take()
    }

    /// Take the pending send-media attachment, if any (#120). The loop drains this
    /// each tick and dispatches it to
    /// [`SendRequests::send_media`](tuigram_core::SendRequests::send_media) for the
    /// open chat; `None` means nothing was confirmed since the last drain.
    pub fn take_media(&mut self) -> Option<OutgoingMedia> {
        self.pending_media.take()
    }

    /// Take the pending secret-chat lifecycle action, if any (#121). The loop drains
    /// this each tick and dispatches it to
    /// [`SecretChatRequests`](tuigram_core::SecretChatRequests)' create/close; `None`
    /// means no secret-chat prompt was confirmed since the last drain.
    pub fn take_secret(&mut self) -> Option<SecretLifecycle> {
        self.pending_secret.take()
    }

    /// Take the pending retention edit, if any (#146). The loop drains this each tick,
    /// swaps its live sweep policy to the new value, and writes it to `settings.toml`;
    /// `None` means no settings edit was confirmed since the last drain.
    pub fn take_settings(&mut self) -> Option<StorageSettings> {
        self.pending_settings.take()
    }

    /// Take the pending delete, if any (#195). The loop drains this each tick and
    /// dispatches it to [`DeleteRequests::delete`](tuigram_core::messages::DeleteRequests);
    /// `None` means no delete was confirmed since the last drain.
    pub fn take_delete(&mut self) -> Option<DeleteIntent> {
        self.pending_delete.take()
    }

    /// Take the pending save/download file id, if any (#195). The loop drains this
    /// each tick and either reveals the local path (already present) or starts the
    /// download; `None` means nothing was requested since the last drain.
    pub fn take_save(&mut self) -> Option<i32> {
        self.pending_save.take()
    }

    /// Take the pending copy text, if any (#197). The loop drains this each tick
    /// and writes it to the OS clipboard; `None` means nothing was requested since
    /// the last drain.
    pub fn take_copy(&mut self) -> Option<String> {
        self.pending_copy.take()
    }

    /// Take the pending resync request (#195). The loop drains this each tick and
    /// re-queries the chat list; `false` means no resync was requested since the last
    /// drain.
    pub fn take_resync(&mut self) -> bool {
        std::mem::take(&mut self.pending_resync)
    }

    /// Take the pending logout request (#195). The loop drains this each tick, ends
    /// the session, and exits; `false` means no logout was confirmed since the last
    /// drain.
    pub fn take_logout(&mut self) -> bool {
        std::mem::take(&mut self.pending_logout)
    }

    /// Re-project the secret-chat lifecycle states from the core
    /// [`SecretChatStore`](tuigram_core::SecretChatStore) (#121). The loop reads each
    /// secret chat's folded state joined to its chat id back from the `Client` and
    /// hands the owned pairs here, so `App` stays pure — the same split as
    /// [`project_chats`](Self::project_chats). Replaces the view's secret-state map
    /// wholesale, so a lifecycle advance (pending → ready → closed) is reflected.
    pub fn project_secret_states(&mut self, states: Vec<(i64, SecretChatState)>) {
        self.chat_list.project_secret_states(states);
        self.dirty = true;
    }

    /// Re-project the open chat's media download state from the core
    /// [`FileStore`](tuigram_core::files::FileStore) (#120). The loop reads the files
    /// backing the open chat's messages back from the `Client` and hands the owned
    /// snapshot here, so `App` stays pure — the same split as
    /// [`project_conversation`](Self::project_conversation). Replaces the view's
    /// download state wholesale, so a completed or advanced transfer overwrites the
    /// prior snapshot and the progress line reflects the newest `updateFile`.
    pub fn project_downloads(&mut self, files: Vec<File>) {
        self.conversation.set_downloads(files);
        self.dirty = true;
    }

    /// Record the conversation pane's inner height measured by the last render (#158),
    /// so the view can bottom-anchor an open / `G` and follow the tail against the real
    /// number of visible rows. The loop calls this after each `draw`. Marks the frame
    /// dirty only when the recorded height re-anchored the cursor (a first measurement
    /// or a resize while following), so the corrected frame repaints without the draw
    /// loop spinning when the height is unchanged.
    pub fn set_conversation_viewport(&mut self, height: usize) {
        if self.conversation.set_viewport_height(height) {
            self.dirty = true;
        }
    }

    /// Record the pane rectangles the last render drew into (#161/#162), so the
    /// next mouse event can be hit-tested to a pane. The loop calls this after each
    /// `draw` with the [`RenderOutput::panes`](crate::ui::RenderOutput) it measured.
    /// Pure bookkeeping — it never re-dirties, since geometry does not change what
    /// is drawn.
    pub fn set_pane_layout(&mut self, panes: crate::ui::PaneLayout) {
        self.pane_layout = panes;
    }

    /// Record the chat-list row map the last render drew, so the next mouse
    /// click on a chat row can open it directly. The loop calls this after each
    /// `draw` with the [`RenderOutput::chat_rows`](crate::ui::RenderOutput) it
    /// measured. Pure bookkeeping, like [`set_pane_layout`](Self::set_pane_layout).
    pub fn set_chat_rows(&mut self, rows: crate::ui::ChatRows) {
        self.chat_rows = rows;
    }

    /// Record the history row map the last render drew, so the next mouse click
    /// on a message row can select it directly. The loop calls this after each
    /// `draw` with the
    /// [`RenderOutput::history_rows`](crate::ui::RenderOutput) it measured. Pure
    /// bookkeeping, like [`set_pane_layout`](Self::set_pane_layout).
    pub fn set_history_rows(&mut self, rows: crate::ui::HistoryRows) {
        self.history_rows = rows;
    }

    /// Record the overlay row map the loop's last `draw` measured — the loop calls
    /// this each frame with the [`RenderOutput::overlay_rows`](crate::ui::RenderOutput)
    /// it measured. Pure bookkeeping, like [`set_pane_layout`](Self::set_pane_layout).
    pub fn set_overlay_rows(&mut self, rows: crate::ui::OverlayRows) {
        self.overlay_rows = rows;
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

    /// Confirm the selected search hit (#117), shared by `ResultOpen` (Enter) and
    /// a click on the hit's row (`OverlayRowClick`, #217): jump to its chat, focus
    /// the history so the loop opens and pages it, and record the target so the
    /// next projection of that chat scrolls to the message if it is loaded. A hit
    /// whose chat is not in the active list (a global hit in a folder/archive)
    /// still closes the overlay and focuses the history; the chat just stays
    /// whatever was selected. No selected hit is a no-op.
    fn confirm_search_result(&mut self) {
        if let Some(hit) = self.search.selected_hit() {
            let (chat_id, message_id) = (hit.chat_id, hit.message_id);
            self.pending_jump = Some((chat_id, message_id));
            self.chat_list.select_chat(chat_id);
            self.focus = Focus::History;
            self.overlay = Overlay::None;
            self.dirty = true;
        }
    }

    /// Confirm the forward to the selected target (#118), shared by
    /// `ForwardConfirm` (Enter) and a click on the target's row
    /// (`OverlayRowClick`, #217): record the send as a pure intent for the loop
    /// and close the picker back to browsing. An empty picker (no target chat)
    /// has nowhere to send, so it just closes without recording an intent.
    fn confirm_forward(&mut self) {
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

    /// React with the effective emoji (#119), shared by `ReactionConfirm`
    /// (Enter) and a click on a palette row (`OverlayRowClick`, #217): the typed
    /// custom one, or the highlighted palette one, then close. The optimistic
    /// flip updates the `{emoji×n*}` chips directly; the intent it records
    /// drives the core add/remove, whose real counts fold in later. Whether we
    /// already had that reaction (pre-toggle) decides add vs remove. An empty
    /// custom line has nothing to send, so it just closes.
    fn confirm_reaction(&mut self) {
        if let Some(emoji) = self.reaction.confirmed_emoji()
            && let Some((chat_id, id)) = self
                .conversation
                .selected_message()
                .map(|m| (m.chat_id, m.id))
        {
            let add = !self.conversation.has_own_reaction(id, &emoji);
            self.conversation.toggle_reaction(id, &emoji);
            self.pending_reaction = Some(ReactionIntent {
                chat_id,
                message_id: id,
                emoji,
                add,
            });
        }
        self.overlay = Overlay::None;
        self.dirty = true;
    }

    /// Confirm the selected contact (#197), shared by `ContactResultConfirm`
    /// (Enter) and a click on the contact's row (`OverlayRowClick`, #217): hand
    /// off to the same secret-chat confirm the chat-list-scoped lifecycle uses
    /// (#87, #121) — the "are you sure" step and the create seam are shared,
    /// only the target's origin differs. No selected hit (empty results) is a
    /// no-op that stays on the results overlay.
    fn confirm_contact_result(&mut self) {
        if let Some(hit) = self.contacts.selected_hit() {
            let lifecycle = SecretLifecycle::Start {
                user_id: hit.user_id,
            };
            self.secret = Some(SecretChatPrompt::new(lifecycle, hit.display_name.clone()));
            self.overlay = Overlay::SecretChat;
            self.dirty = true;
        }
    }

    /// Replace the search overlay's hits with a fresh, projected result set (#117),
    /// resetting the selection to the top. The loop calls this when a spawned search
    /// completes.
    pub fn set_search_results(&mut self, hits: Vec<crate::search::SearchHit>) {
        self.search.set_results(hits);
        self.dirty = true;
    }

    /// Replace the contact-search picker's hits with a fresh, resolved result set
    /// (#197), resetting the selection to the top. The loop calls this when a
    /// spawned `search_contacts` + name resolution completes.
    pub fn set_contact_results(&mut self, hits: Vec<ContactHit>) {
        self.contacts.set_results(hits);
        self.dirty = true;
    }

    /// Enqueue a transient toast — a failed action (#116) or a one-off core event.
    /// The notice tick ([`tick_notices`](Self::tick_notices)) ages it out and
    /// [`Action::NoticeDismiss`] drops it on demand.
    pub fn notify(&mut self, notice: crate::status::Notice) {
        self.notifications.push(notice);
        self.dirty = true;
    }

    /// Age the showing toast by one notice-clock tick (#139), dropping it when its
    /// lifetime runs out and revealing any next. Driven by the loop's ~1s notice
    /// interval, separate from the faster render tick. Marks the app dirty only when
    /// a toast actually left, since a still-counting toast looks unchanged.
    pub fn tick_notices(&mut self) {
        if self.notifications.tick() {
            self.dirty = true;
        }
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
            // A left-click focuses the pane under the pointer; the wheel scrolls the
            // pane it is over (#161/#162), hit-tested against the last render.
            Event::Mouse(mouse) => self.on_mouse(mouse),
            // A resize must repaint against the new viewport.
            Event::Resize(_, _) => Action::Render,
            _ => Action::Noop,
        }
    }

    /// Map a mouse event to an [`Action`] against the pane rectangles the last
    /// render recorded (#161/#162). While a modal overlay is open, a left-click
    /// is hit-tested against its row map instead (see
    /// [`on_overlay_click`](Self::on_overlay_click)) and every other mouse event
    /// is ignored — overlay wheel-scroll stays out of scope (#217). Otherwise a
    /// left-click focuses the pane under the pointer — or, on an actual
    /// chat/history/composer position, opens/selects/places the cursor directly
    /// (see [`on_click`](Self::on_click)) — and the wheel moves the chat-list
    /// selection or scrolls the history, the pane the pointer is *over*
    /// regardless of which pane holds focus. Clicks/wheels over the status bar
    /// or empty space, and the wheel over the composer, are ignored. Pure, like
    /// [`on_terminal_event`](Self::on_terminal_event).
    fn on_mouse(&self, mouse: MouseEvent) -> Action {
        if self.overlay != Overlay::None {
            return match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    self.on_overlay_click(mouse.column, mouse.row)
                }
                _ => Action::Noop,
            };
        }
        let pane = self.pane_layout.focus_at(mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => self.on_click(pane, mouse.column, mouse.row),
            MouseEventKind::ScrollUp => match pane {
                Some(Focus::ChatList) => Action::SelectPrev,
                Some(Focus::History) => Action::ScrollUp,
                _ => Action::Noop,
            },
            MouseEventKind::ScrollDown => match pane {
                Some(Focus::ChatList) => Action::SelectNext,
                Some(Focus::History) => Action::ScrollDown,
                _ => Action::Noop,
            },
            _ => Action::Noop,
        }
    }

    /// A left-click's action for the pane it landed in and the position it hit
    /// within that pane. A hit on an actual chat row opens that chat
    /// (`OpenChat`); a hit on an actual history row selects that message
    /// (`SelectMessageAt`) — both looked up from the row maps the last render
    /// recorded ([`ChatRows`](crate::ui::ChatRows), [`HistoryRows`](crate::ui::HistoryRows)).
    /// A hit in the composer places the cursor at the clicked column
    /// (`ComposerClickAt`) — the composer's rect always passed `focus_at`'s
    /// containment check, so the column is already known to be inside it; the
    /// text starts one column past the left border (`Block::bordered()`).
    /// A chat-list/history click with no row under it (empty list/history space
    /// below the last row) falls back to #161's plain focus-only click; outside
    /// any pane is `Noop`. Pure, like [`on_mouse`](Self::on_mouse).
    fn on_click(&self, pane: Option<Focus>, column: u16, row: u16) -> Action {
        match pane {
            Some(Focus::ChatList) => self
                .chat_rows
                .chat_at(row)
                .map_or(Action::SetFocus(Focus::ChatList), Action::OpenChat),
            Some(Focus::History) => self
                .history_rows
                .message_at(row)
                .map_or(Action::SetFocus(Focus::History), Action::SelectMessageAt),
            Some(Focus::Composer) => {
                let inner_x = self.pane_layout.composer.x + 1;
                Action::ComposerClickAt(column.saturating_sub(inner_x) as usize)
            }
            None => Action::Noop,
        }
    }

    /// A left-click's action while a modal list overlay is open: a hit on an
    /// actual row selects and confirms it (`OverlayRowClick`), looked up from the
    /// overlay row map the last render recorded ([`OverlayRows`](crate::ui::OverlayRows)),
    /// which already rejects a column outside the popup's list area. An overlay
    /// with no selectable list (help, a text-entry line, a confirm prompt) has an
    /// empty row map, so every click on it is `Noop`. Pure, like
    /// [`on_mouse`](Self::on_mouse).
    fn on_overlay_click(&self, column: u16, row: u16) -> Action {
        self.overlay_rows
            .index_at(column, row)
            .map_or(Action::Noop, Action::OverlayRowClick)
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
            | AppEvent::ChatReadOutbox
            | AppEvent::Messages
            | AppEvent::File
            | AppEvent::Secret
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
            Action::OpenChat(chat_id) => {
                // Mirrors `ResultOpen`'s "select + focus history" (#117) minus the
                // message jump: a chat-row click has no target message, just the chat.
                self.chat_list.select_chat(chat_id);
                self.focus = Focus::History;
                self.dirty = true;
            }
            Action::SelectMessageAt(message_id) => {
                self.conversation.select_message(message_id);
                self.focus = Focus::History;
                self.dirty = true;
            }
            Action::ComposerClickAt(index) => {
                self.focus = Focus::Composer;
                self.composer.set_cursor(index);
                self.dirty = true;
            }
            Action::OverlayRowClick(index) => match self.overlay {
                Overlay::SearchResults => {
                    self.search.select(index);
                    self.confirm_search_result();
                }
                Overlay::Forward => {
                    self.forward.select(index);
                    self.confirm_forward();
                }
                Overlay::Reaction => {
                    self.reaction.select(index);
                    self.confirm_reaction();
                }
                Overlay::ContactSearchResults => {
                    self.contacts.select(index);
                    self.confirm_contact_result();
                }
                _ => {}
            },
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
            Action::JumpToNewest => {
                self.conversation.jump_to_newest();
                self.dirty = true;
            }
            Action::ComposerInput(c) => {
                self.composer.insert(c);
                // A character was just inserted, so the buffer can't be empty (#197).
                self.wants_typing_ping = true;
                self.dirty = true;
            }
            Action::ComposerBackspace => {
                self.composer.backspace();
                // Only ping while there's still unsent text to indicate (#197); a
                // backspace that empties the buffer lets the action expire on its
                // own rather than sending one more ping for nothing left to type.
                if !self.composer.is_empty() {
                    self.wants_typing_ping = true;
                }
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
            Action::ResultOpen => self.confirm_search_result(),
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
            Action::ForwardConfirm => self.confirm_forward(),
            Action::ForwardCancel => {
                // Back to wherever the forward was started from — the search results
                // (forward from a hit) or the conversation (forward from history).
                self.overlay = self.forward_return;
                self.dirty = true;
            }
            Action::PinToggle => {
                // Pin/unpin the selected message: flip the 📌 indicator optimistically
                // and record the intent for the loop to send through core's pin/unpin
                // (#119). The pre-toggle pinned state decides pin vs unpin. A no-op on
                // an empty history (no selected message).
                if let Some((chat_id, id)) = self
                    .conversation
                    .selected_message()
                    .map(|m| (m.chat_id, m.id))
                {
                    let pin = !self.conversation.is_pinned(id);
                    self.conversation.toggle_pin(id);
                    self.pending_pin = Some(PinIntent {
                        chat_id,
                        message_id: id,
                        pin,
                    });
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
                // Arrow-key nav walks the palette; ignored while typing a custom emoji.
                if !self.reaction.is_custom() {
                    self.reaction.select_next();
                    self.dirty = true;
                }
            }
            Action::ReactionPrev => {
                if !self.reaction.is_custom() {
                    self.reaction.select_prev();
                    self.dirty = true;
                }
            }
            Action::ReactionKey(c) => {
                // Custom mode: every character is buffer input (a terminal has no emoji
                // key, so we take whatever the OS picker or a paste emits). Palette
                // mode: `j`/`k` move and `c` opens the custom line; any other letter is
                // ignored (reactions are emoji, not latin text).
                if self.reaction.is_custom() {
                    self.reaction.push(c);
                    self.dirty = true;
                } else {
                    let handled = match c {
                        'j' => {
                            self.reaction.select_next();
                            true
                        }
                        'k' => {
                            self.reaction.select_prev();
                            true
                        }
                        'c' => {
                            self.reaction.enter_custom();
                            true
                        }
                        _ => false,
                    };
                    self.dirty |= handled;
                }
            }
            Action::ReactionBackspace => {
                if self.reaction.is_custom() {
                    self.reaction.backspace();
                    self.dirty = true;
                }
            }
            Action::ReactionConfirm => self.confirm_reaction(),
            Action::ReactionCancel => {
                // Esc backs out of the custom line to the palette first; a second Esc
                // (now in palette mode) closes the overlay.
                if self.reaction.is_custom() {
                    self.reaction.exit_custom();
                } else {
                    self.overlay = Overlay::None;
                }
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
                // Build the `OutgoingMedia` the prompt describes and record it for the
                // loop to send (#120); the upload then streams back through the file
                // store, exactly like a text send (#116). An empty path yields no
                // media, a no-op that keeps the prompt open.
                if let Some(media) = self.media.to_outgoing() {
                    self.pending_media = Some(media);
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
                // Record the confirmed lifecycle action for the loop to dispatch on
                // the core seam (`create_new_secret_chat` / `close_secret_chat`); the
                // resulting `updateSecretChat` / `updateNewChat` fold back and
                // re-project (#121). Reads only the prompt's lifecycle, never any key
                // material. A confirm with no prompt open is an inert close.
                if let Some(prompt) = self.secret.take() {
                    self.pending_secret = Some(prompt.lifecycle());
                }
                self.overlay = Overlay::None;
                self.dirty = true;
            }
            Action::SecretCancel => {
                self.secret = None;
                self.overlay = Overlay::None;
                self.dirty = true;
            }
            Action::ContactSearchOpen => {
                // A fresh search each time, so a previous query never leaks in (#197).
                self.contacts.reset();
                self.overlay = Overlay::ContactSearchInput;
                self.dirty = true;
            }
            Action::ContactSearchInput(c) => {
                self.contacts.insert(c);
                self.dirty = true;
            }
            Action::ContactSearchBackspace => {
                self.contacts.backspace();
                self.dirty = true;
            }
            Action::ContactSearchLeft => {
                self.contacts.move_left();
                self.dirty = true;
            }
            Action::ContactSearchRight => {
                self.contacts.move_right();
                self.dirty = true;
            }
            Action::ContactSearchHome => {
                self.contacts.move_home();
                self.dirty = true;
            }
            Action::ContactSearchEnd => {
                self.contacts.move_end();
                self.dirty = true;
            }
            Action::ContactSearchSubmit => {
                // Record the query as a pure intent the loop drains and runs against
                // `search_contacts` (#197); `App` never touches the `Client`, so the
                // hits arrive later via `set_contact_results`. Clear any stale hits
                // now so the overlay is empty until they land. A blank query is a
                // no-op that stays on the input line.
                let query = self.contacts.query().trim().to_owned();
                if !query.is_empty() {
                    self.pending_contact_search = Some(query);
                    self.contacts.set_results(Vec::new());
                    self.overlay = Overlay::ContactSearchResults;
                    self.dirty = true;
                }
            }
            Action::ContactSearchCancel => {
                self.overlay = Overlay::None;
                self.dirty = true;
            }
            Action::ContactResultNext => {
                self.contacts.select_next();
                self.dirty = true;
            }
            Action::ContactResultPrev => {
                self.contacts.select_prev();
                self.dirty = true;
            }
            Action::ContactResultConfirm => self.confirm_contact_result(),
            Action::SettingsOpen => {
                // Open the editor pre-filled with the policy in effect, so the fields
                // show the live values a Tab-and-type edits (#146).
                self.settings = SettingsDraft::from_settings(self.storage);
                self.overlay = Overlay::Settings;
                self.dirty = true;
            }
            Action::SettingsInput(c) => {
                self.settings.insert(c);
                self.dirty = true;
            }
            Action::SettingsBackspace => {
                self.settings.backspace();
                self.dirty = true;
            }
            Action::SettingsLeft => {
                self.settings.move_left();
                self.dirty = true;
            }
            Action::SettingsRight => {
                self.settings.move_right();
                self.dirty = true;
            }
            Action::SettingsHome => {
                self.settings.move_home();
                self.dirty = true;
            }
            Action::SettingsEnd => {
                self.settings.move_end();
                self.dirty = true;
            }
            Action::SettingsToggleField => {
                self.settings.toggle_field();
                self.dirty = true;
            }
            Action::SettingsConfirm => {
                // Validate the four fields through core's parsers. A valid edit
                // updates the in-memory policy (so a reopen shows the new values) and
                // lands it for the loop to apply live and persist (#146); an invalid
                // value keeps the overlay open with the reason shown in place.
                if let Some(updated) = self.settings.confirm() {
                    self.storage = updated;
                    self.pending_settings = Some(updated);
                    self.overlay = Overlay::None;
                }
                self.dirty = true;
            }
            Action::SettingsCancel => {
                self.overlay = Overlay::None;
                self.dirty = true;
            }
            Action::ReplyMessage => {
                // Put the composer into reply mode against the selected message and
                // jump to it (#195). The reply body is sent through the already-wired
                // send seam on submit (#116). A no-op on an empty history.
                let target = self.conversation.selected_message().map(|m| {
                    let label = self.conversation.sender_label(m);
                    (m.id, message_preview(&label.label, m))
                });
                if let Some((id, preview)) = target {
                    self.composer.reply_to(id, preview);
                    self.focus = Focus::Composer;
                    self.dirty = true;
                }
            }
            Action::EditMessage => {
                // Pre-fill the composer with the selected message's text and jump to
                // it (#195); submitting replaces the message through the edit seam
                // (#116). Only our own text messages are editable — anything else
                // explains why with a toast and does nothing.
                let target = self
                    .conversation
                    .selected_message()
                    .map(|m| (m.id, m.is_outgoing, m.text().map(str::to_owned)));
                if let Some((id, own, text)) = target {
                    match (own, text) {
                        (true, Some(text)) => {
                            self.composer.edit(id, text);
                            self.focus = Focus::Composer;
                        }
                        (false, _) => {
                            self.notify(Notice::info("You can only edit your own messages."));
                        }
                        (true, None) => {
                            self.notify(Notice::info("Only text messages can be edited."));
                        }
                    }
                    self.dirty = true;
                }
            }
            Action::DeleteMessage => {
                // Open the delete confirm for the selected message (#195). Reads the
                // message's ownership so the confirm can offer "for everyone" only for
                // our own messages. A no-op on an empty history.
                let target = self.conversation.selected_message().map(|m| {
                    let label = self.conversation.sender_label(m);
                    (
                        m.chat_id,
                        m.id,
                        m.is_outgoing,
                        message_preview(&label.label, m),
                    )
                });
                if let Some((chat_id, id, own, preview)) = target {
                    self.delete = Some(DeletePrompt::new(chat_id, id, own, preview));
                    self.overlay = Overlay::DeleteConfirm;
                    self.dirty = true;
                }
            }
            Action::DeleteToggleScope => {
                if let Some(prompt) = self.delete.as_mut() {
                    prompt.toggle_revoke();
                    self.dirty = true;
                }
            }
            Action::DeleteConfirm => {
                // Record the delete as a pure intent for the loop (#195), then close.
                // No optimistic removal — the real `updateDeleteMessages` folds and
                // re-projects the history.
                if let Some(prompt) = self.delete.take() {
                    self.pending_delete = Some(prompt.into_intent());
                }
                self.overlay = Overlay::None;
                self.dirty = true;
            }
            Action::DeleteCancel => {
                self.delete = None;
                self.overlay = Overlay::None;
                self.dirty = true;
            }
            Action::SaveMedia => {
                // Record the selected message's file id for the loop to reveal or
                // download (#195). `App` cannot read the file store, so it only
                // resolves the id here; a message with no downloadable media (id 0 or
                // a non-media body) explains why with a toast.
                let file_id = self
                    .conversation
                    .selected_message()
                    .and_then(|m| m.content.file())
                    .map(|f| f.id)
                    .filter(|id| *id != 0);
                match file_id {
                    Some(id) => self.pending_save = Some(id),
                    None => self.notify(Notice::info("The selected message has no media to save.")),
                }
                self.dirty = true;
            }
            Action::CopyMessage => {
                // Copy the selected message's text (`y`, #197). `App` cannot reach
                // the OS clipboard, so it only resolves the text here; a non-text
                // message explains why with a toast, the same shape as
                // `EditMessage`'s restriction. A no-op on an empty history.
                match self.conversation.selected_message().map(|m| m.text()) {
                    Some(Some(text)) => {
                        self.pending_copy = Some(text.to_owned());
                        self.dirty = true;
                    }
                    Some(None) => self.notify(Notice::info("Only text messages can be copied.")),
                    None => {}
                }
            }
            Action::Resync => {
                self.pending_resync = true;
                self.dirty = true;
            }
            Action::LogoutOpen => {
                self.overlay = Overlay::LogoutConfirm;
                self.dirty = true;
            }
            Action::LogoutConfirm => {
                self.pending_logout = true;
                self.overlay = Overlay::None;
                self.dirty = true;
            }
            Action::LogoutCancel => {
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

/// A short one-line label of a message for the reply and delete prompts (#195):
/// the sender label and a trimmed snippet of the body, or a bracketed media kind
/// for a non-text message. Kept to a single line so it fits the composer's reply
/// indicator and the delete confirm.
fn message_preview(sender: &str, message: &Message) -> String {
    const MAX: usize = 40;
    let body = match message.text() {
        Some(text) if !text.trim().is_empty() => {
            let text = text.trim();
            let mut out: String = text.chars().take(MAX).collect();
            if text.chars().count() > MAX {
                out.push('…');
            }
            out
        }
        _ => format!("<{}>", media_kind(&message.content)),
    };
    format!("{sender}: {body}")
}

/// The bracketed kind label for a non-text message body (#195).
fn media_kind(content: &MessageContent) -> &'static str {
    match content {
        MessageContent::Text(_) | MessageContent::Unsupported(_) => "message",
        MessageContent::Photo(_) => "photo",
        MessageContent::Video(_) => "video",
        MessageContent::Document(_) => "document",
        MessageContent::Audio(_) => "audio",
        MessageContent::Voice(_) => "voice",
        MessageContent::Sticker(_) => "sticker",
        MessageContent::Animation(_) => "animation",
        MessageContent::Location(_) => "location",
        MessageContent::Venue(_) => "venue",
        MessageContent::Contact(_) => "contact",
        MessageContent::Poll(_) => "poll",
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

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    /// An app whose recorded pane rects match an 80×24 frame — the geometry the
    /// render tests use — so the coordinates below hit-test to real panes
    /// (#161/#162). Chat list: left column; history: right column, rows 0..19;
    /// composer: rows 20..22; status: row 23.
    fn app_with_panes() -> App {
        let mut app = App::new();
        app.set_pane_layout(crate::ui::pane_layout(ratatui::layout::Rect::new(
            0, 0, 80, 24,
        )));
        app
    }

    const LEFT_CLICK: MouseEventKind = MouseEventKind::Down(MouseButton::Left);

    #[test]
    fn left_click_focuses_the_pane_under_the_pointer() {
        let mut app = app_with_panes();
        assert_eq!(
            app.on_terminal_event(mouse(LEFT_CLICK, 1, 1)),
            Action::SetFocus(Focus::ChatList)
        );
        assert_eq!(
            app.on_terminal_event(mouse(LEFT_CLICK, 50, 1)),
            Action::SetFocus(Focus::History)
        );
        // The composer resolves to a cursor-placement action rather than a plain
        // focus one (#217) — dispatching it still focuses the pane, with the
        // cursor landed at the clicked column too.
        let composer_click = app.on_terminal_event(mouse(LEFT_CLICK, 50, 21));
        assert!(matches!(composer_click, Action::ComposerClickAt(_)));
        app.dispatch(composer_click);
        assert_eq!(app.focus(), Focus::Composer);
    }

    #[test]
    fn a_click_on_the_status_bar_or_empty_space_is_ignored() {
        let app = app_with_panes();
        // The status strip is not a focus target.
        assert_eq!(
            app.on_terminal_event(mouse(LEFT_CLICK, 0, 23)),
            Action::Noop
        );
        // Past the right edge hits nothing.
        assert_eq!(
            app.on_terminal_event(mouse(LEFT_CLICK, 100, 5)),
            Action::Noop
        );
    }

    #[test]
    fn wheel_over_the_chat_list_moves_its_selection() {
        // Regardless of focus: the app lands focused on the chat list, but the
        // pointer is what selects the target pane (#162).
        let app = app_with_panes();
        assert_eq!(
            app.on_terminal_event(mouse(MouseEventKind::ScrollDown, 1, 1)),
            Action::SelectNext
        );
        assert_eq!(
            app.on_terminal_event(mouse(MouseEventKind::ScrollUp, 1, 1)),
            Action::SelectPrev
        );
    }

    #[test]
    fn wheel_over_the_history_pane_scrolls_it() {
        let app = app_with_panes();
        assert_eq!(
            app.on_terminal_event(mouse(MouseEventKind::ScrollDown, 50, 1)),
            Action::ScrollDown
        );
        assert_eq!(
            app.on_terminal_event(mouse(MouseEventKind::ScrollUp, 50, 1)),
            Action::ScrollUp
        );
    }

    #[test]
    fn wheel_over_the_composer_is_ignored() {
        // Only the chat list and history scroll on the wheel (#162); the composer
        // and status bar do not.
        let app = app_with_panes();
        assert_eq!(
            app.on_terminal_event(mouse(MouseEventKind::ScrollDown, 50, 21)),
            Action::Noop
        );
    }

    #[test]
    fn mouse_events_are_ignored_while_a_modal_overlay_is_open() {
        // An overlay captures input, so a click/wheel falls through to nothing
        // rather than reaching the panes underneath (#161).
        let mut app = app_with_panes();
        app.dispatch(Action::ToggleHelp);
        assert_ne!(app.overlay(), Overlay::None, "help overlay should be open");
        assert_eq!(app.on_terminal_event(mouse(LEFT_CLICK, 1, 1)), Action::Noop);
        assert_eq!(
            app.on_terminal_event(mouse(MouseEventKind::ScrollDown, 50, 1)),
            Action::Noop
        );
    }

    /// Render `app` (real chat/message data, not the synthetic rects
    /// `app_with_panes` uses) into an 80×24 `TestBackend` frame and record the
    /// pane rects, chat/message row maps, and overlay row map onto it — the same
    /// calls the live loop makes after every `draw` (see `main.rs`), so a click
    /// test exercises the exact hit-testing a real session would.
    fn render_and_record(app: &mut App) {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let mut output = crate::ui::RenderOutput::default();
        terminal
            .draw(|frame| output = crate::ui::ui(frame, app))
            .unwrap();
        app.set_pane_layout(output.panes);
        app.set_chat_rows(output.chat_rows);
        app.set_history_rows(output.history_rows);
        app.set_overlay_rows(output.overlay_rows);
    }

    #[test]
    fn left_click_on_a_chat_row_opens_that_chat() {
        use crate::chat_list::{ChatList, ChatListView, sample_chat};
        use tuigram_core::model::ChatListKind;

        // Two chats so the click can target the second row specifically,
        // proving the hit follows the row clicked rather than just whatever
        // happens to already be selected.
        let view = ChatListView::from_lists(vec![ChatList {
            kind: ChatListKind::Main,
            label: "Main".to_owned(),
            chats: vec![sample_chat(1, "Alice", 0), sample_chat(2, "Bob", 0)],
        }]);
        let mut app = App::with_chat_list(view);
        render_and_record(&mut app);

        let row = (0..24)
            .find(|&r| {
                matches!(
                    app.on_terminal_event(mouse(LEFT_CLICK, 1, r)),
                    Action::OpenChat(2)
                )
            })
            .expect("Bob's row resolves to OpenChat(2)");

        app.dispatch(Action::OpenChat(2));
        assert_eq!(
            app.focus(),
            Focus::History,
            "click opens into the history pane"
        );
        assert_eq!(app.chat_list().selected_chat().map(|c| c.id), Some(2));

        // A click on a row that maps to no chat (empty list space) still just
        // focuses the pane — #161's original behavior is preserved there.
        let below_last_row = row + 1;
        assert_eq!(
            app.on_terminal_event(mouse(LEFT_CLICK, 1, below_last_row)),
            Action::SetFocus(Focus::ChatList)
        );
    }

    #[test]
    fn left_click_on_a_history_row_selects_that_message() {
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
        let mut app = App::new();
        // A tall viewport so both messages fit and the open bottom-anchors at the
        // top, same as `projecting_a_conversation_fills_the_history_and_dirties`.
        app.set_conversation_viewport(40);
        app.project_conversation(
            10,
            vec![text(1), text(2)],
            HashSet::new(),
            HashMap::new(),
            i64::MAX,
            0,
            true,
        );
        render_and_record(&mut app);

        (0..24)
            .find(|&r| {
                matches!(
                    app.on_terminal_event(mouse(LEFT_CLICK, 50, r)),
                    Action::SelectMessageAt(2)
                )
            })
            .expect("message 2's row resolves to SelectMessageAt(2)");

        app.dispatch(Action::SelectMessageAt(2));
        assert_eq!(app.focus(), Focus::History);
        assert_eq!(app.conversation().selected_message().map(|m| m.id), Some(2));

        // Message 1's header sits on the history pane's first inner row.
        assert_eq!(
            app.on_terminal_event(mouse(LEFT_CLICK, 50, 1)),
            Action::SelectMessageAt(1)
        );
    }

    #[test]
    fn left_click_in_the_composer_moves_the_cursor_there() {
        let mut composer = Composer::default();
        for c in "hello world".chars() {
            composer.insert(c);
        }
        let mut app = App::with_composer(composer);
        render_and_record(&mut app);

        // The composer's inner text starts one column past its left border
        // (`Block::bordered()`); its inner row is one row past its top border.
        let panes = crate::ui::pane_layout(ratatui::layout::Rect::new(0, 0, 80, 24));
        let inner_x = panes.composer.x + 1;
        let row = panes.composer.y + 1;

        // Click 3 columns into "hello world" — the cursor should land between
        // the "hel" and "lo world" it split, i.e. character index 3.
        let action = app.on_terminal_event(mouse(LEFT_CLICK, inner_x + 3, row));
        assert_eq!(action, Action::ComposerClickAt(3));

        app.dispatch(action);
        assert_eq!(app.focus(), Focus::Composer, "click focuses the composer");
        assert_eq!(app.composer().cursor(), 3);
    }

    #[test]
    fn left_click_on_a_search_result_row_selects_and_opens_it() {
        // #217: clicking a hit should behave exactly like navigating to it and
        // pressing Enter (`ResultOpen`, see `opening_a_hit_jumps_to_its_chat_and_focuses_the_history`).
        let mut app = app_on_results(); // chats Alice(1)/Bob(2); hits (1,10),(2,20)
        render_and_record(&mut app);

        // Column 40 sits inside the centred results popup at this width.
        let (hit_row, action) = (0..24)
            .find_map(|r| match app.on_terminal_event(mouse(LEFT_CLICK, 40, r)) {
                Action::OverlayRowClick(1) => Some((r, Action::OverlayRowClick(1))),
                _ => None,
            })
            .expect("Bob's hit (index 1) resolves to OverlayRowClick(1)");

        // A click at that same row but off to the side of the centred popup
        // (column 1, over the panes underneath) must not hit the overlay —
        // checked before dispatching, while the overlay is still open.
        assert_eq!(
            app.on_terminal_event(mouse(LEFT_CLICK, 1, hit_row)),
            Action::Noop
        );

        app.dispatch(action);
        assert_eq!(app.overlay(), Overlay::None, "overlay closed on jump");
        assert_eq!(app.focus(), Focus::History);
        assert_eq!(
            app.chat_list().selected_chat().map(|c| c.id),
            Some(2),
            "the clicked hit's chat is selected"
        );
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
        // A tall viewport (the loop records it after each render) so the whole
        // two-message history fits: the open bottom-anchors, which for a history that
        // fits is the top — message 1 at the top of the pane (#158).
        app.set_conversation_viewport(40);
        app.clear_dirty();
        app.project_conversation(
            10,
            vec![text(1), text(2)],
            HashSet::new(),
            HashMap::new(),
            i64::MAX,
            0,
            true,
        );
        assert!(app.is_dirty());
        assert_eq!(app.conversation().len(), 2);
        assert_eq!(app.conversation().selected_message().map(|m| m.id), Some(1));
    }

    #[test]
    fn jump_to_newest_bottom_anchors_the_history_and_dirties() {
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
        // Five 3-row messages in a two-message viewport: opening bottom-anchors, so
        // scrolling up and pressing G returns to the newest anchor (#158).
        let mut app = App::new();
        app.set_conversation_viewport(6);
        app.project_conversation(
            10,
            (1..=5).map(text).collect(),
            HashSet::new(),
            HashMap::new(),
            i64::MAX,
            0,
            true,
        );
        let anchor = app.conversation().offset();
        assert!(anchor > 0, "a long history opens away from the top");

        app.dispatch(Action::ScrollUp);
        app.clear_dirty();
        app.dispatch(Action::JumpToNewest);
        assert_eq!(
            app.conversation().offset(),
            anchor,
            "G returns to the newest"
        );
        assert!(app.is_dirty());
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
        app.project_conversation(
            1,
            text_history(&[1, 2]),
            HashSet::new(),
            HashMap::new(),
            i64::MAX,
            0,
            true,
        );
        assert_ne!(
            app.conversation().selected_message().map(|m| m.id),
            Some(10),
            "message not loaded, nothing to scroll to"
        );

        // The page carrying message 10 lands: the jump applies and clears.
        app.project_conversation(
            1,
            text_history(&[9, 10, 11]),
            HashSet::new(),
            HashMap::new(),
            i64::MAX,
            0,
            true,
        );
        assert_eq!(
            app.conversation().selected_message().map(|m| m.id),
            Some(10)
        );
    }

    #[test]
    fn a_jump_is_dropped_when_a_different_chat_is_opened_first() {
        let mut app = app_on_results();
        // A viewport tall enough to hold the short history, so bottom-anchoring puts
        // the oldest loaded message at the top of the pane (#158).
        app.set_conversation_viewport(40);
        app.dispatch(Action::ResultOpen); // hit for chat 1, message 10

        // The user opens chat 2 before chat 1's history arrives: the stale jump drops.
        app.project_conversation(
            2,
            text_history(&[7, 8]),
            HashSet::new(),
            HashMap::new(),
            i64::MAX,
            0,
            true,
        );
        // Chat 1's history (with message 10) arrives later, but the jump is gone, so
        // the view opens bottom-anchored (message 9 at the pane top, the whole history
        // fitting) rather than chasing message 10.
        app.project_conversation(
            1,
            text_history(&[9, 10, 11]),
            HashSet::new(),
            HashMap::new(),
            i64::MAX,
            0,
            true,
        );
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
        app.project_conversation(
            1,
            text_history(&[10, 11]),
            HashSet::new(),
            HashMap::new(),
            i64::MAX,
            0,
            true,
        );
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

    use crate::reactions::ReactionIntent;
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
    fn pin_toggles_the_selected_message_and_records_the_intent() {
        use crate::conversation::PinIntent;
        let mut app = app_with_history();
        let id = app.conversation().selected_message().unwrap().id;
        app.clear_dirty();
        app.dispatch(Action::PinToggle);
        assert!(app.conversation().is_pinned(id), "pinned optimistically");
        assert!(app.is_dirty());
        // The pin (not unpin) is recorded for the loop to send to core.
        assert_eq!(
            app.take_pin(),
            Some(PinIntent {
                chat_id: 1,
                message_id: id,
                pin: true,
            })
        );
        assert_eq!(app.take_pin(), None, "the intent is drained once");
        // Toggling again unpins, and records the matching unpin.
        app.dispatch(Action::PinToggle);
        assert!(!app.conversation().is_pinned(id), "unpinned again");
        assert_eq!(
            app.take_pin(),
            Some(PinIntent {
                chat_id: 1,
                message_id: id,
                pin: false,
            })
        );
    }

    #[test]
    fn pin_on_an_empty_history_is_a_noop() {
        let mut app = App::new();
        app.clear_dirty();
        app.dispatch(Action::PinToggle);
        assert!(!app.is_dirty(), "no selected message, nothing changes");
        assert_eq!(app.take_pin(), None, "no intent recorded");
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
        // The add (a fresh reaction) is recorded for the loop to send to core.
        assert_eq!(
            app.take_reaction(),
            Some(ReactionIntent {
                chat_id: 1,
                message_id: id,
                emoji: chosen.to_owned(),
                add: true,
            })
        );
        assert_eq!(app.take_reaction(), None, "the intent is drained once");
    }

    #[test]
    fn left_click_on_a_reaction_palette_row_selects_and_confirms_it() {
        // #217: a click on a palette emoji should behave exactly like navigating
        // to it and pressing Enter, same as the search-result click.
        let mut app = app_with_history();
        let id = app.conversation().selected_message().unwrap().id;
        app.dispatch(Action::ReactionOpen);
        render_and_record(&mut app);

        let chosen = app.reaction().palette()[2];
        let action = (0..24)
            .find_map(|r| match app.on_terminal_event(mouse(LEFT_CLICK, 40, r)) {
                Action::OverlayRowClick(2) => Some(Action::OverlayRowClick(2)),
                _ => None,
            })
            .expect("the third palette emoji resolves to OverlayRowClick(2)");

        app.dispatch(action);
        assert_eq!(app.overlay(), Overlay::None, "confirm closes the picker");
        let message = app
            .conversation()
            .messages()
            .iter()
            .find(|m| m.id == id)
            .unwrap();
        assert_eq!(message.reactions.len(), 1);
        assert_eq!(
            message.reactions[0].kind,
            ReactionKind::Emoji(chosen.to_owned())
        );
    }

    #[test]
    fn reacting_with_the_same_emoji_again_records_a_removal() {
        let mut app = app_with_history();
        let id = app.conversation().selected_message().unwrap().id;
        // React once (add), then again with the same emoji (remove).
        app.dispatch(Action::ReactionOpen);
        let chosen = app.reaction().selected_emoji();
        app.dispatch(Action::ReactionConfirm);
        assert!(matches!(app.take_reaction(), Some(i) if i.add));
        app.dispatch(Action::ReactionOpen);
        app.dispatch(Action::ReactionConfirm);
        // The optimistic bucket is gone, and the recorded intent is a removal.
        let message = app.conversation().messages().iter().find(|m| m.id == id);
        assert!(
            message.unwrap().reactions.is_empty(),
            "our reaction removed"
        );
        assert_eq!(
            app.take_reaction(),
            Some(ReactionIntent {
                chat_id: 1,
                message_id: id,
                emoji: chosen.to_owned(),
                add: false,
            })
        );
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
        assert_eq!(app.take_reaction(), None, "cancel records no intent");
    }

    #[test]
    fn typing_a_custom_emoji_reacts_with_it() {
        let mut app = app_with_history();
        let id = app.conversation().selected_message().unwrap().id;
        app.dispatch(Action::ReactionOpen);
        // `c` opens the custom line; then type a multi-scalar emoji.
        app.dispatch(Action::ReactionKey('c'));
        assert!(app.reaction().is_custom(), "custom line active");
        for c in "🥳".chars() {
            app.dispatch(Action::ReactionKey(c));
        }
        app.dispatch(Action::ReactionConfirm);
        assert_eq!(app.overlay(), Overlay::None, "confirm closes the picker");
        // The custom emoji lands on the message and is sent to core as an add.
        let reactions = &app
            .conversation()
            .messages()
            .iter()
            .find(|m| m.id == id)
            .unwrap()
            .reactions;
        assert_eq!(reactions[0].kind, ReactionKind::Emoji("🥳".to_owned()));
        assert_eq!(
            app.take_reaction(),
            Some(ReactionIntent {
                chat_id: 1,
                message_id: id,
                emoji: "🥳".to_owned(),
                add: true,
            })
        );
    }

    #[test]
    fn custom_line_keys_type_instead_of_navigating_and_esc_returns_to_the_palette() {
        let mut app = app_with_history();
        app.dispatch(Action::ReactionOpen);
        app.dispatch(Action::ReactionKey('c'));
        // In custom mode, `j`/`k` are literal input, not palette navigation.
        app.dispatch(Action::ReactionKey('j'));
        app.dispatch(Action::ReactionKey('k'));
        assert_eq!(app.reaction().custom_input(), Some("jk"));
        assert_eq!(app.reaction().selected(), 0, "palette cursor did not move");
        // Backspace edits the buffer.
        app.dispatch(Action::ReactionBackspace);
        assert_eq!(app.reaction().custom_input(), Some("j"));
        // Esc backs out to the palette (overlay stays open); a second Esc closes it.
        app.dispatch(Action::ReactionCancel);
        assert!(!app.reaction().is_custom(), "back in palette mode");
        assert_eq!(app.overlay(), Overlay::Reaction, "overlay still open");
        app.dispatch(Action::ReactionCancel);
        assert_eq!(app.overlay(), Overlay::None, "second Esc closes it");
    }

    #[test]
    fn confirming_an_empty_custom_line_reacts_with_nothing() {
        let mut app = app_with_history();
        app.dispatch(Action::ReactionOpen);
        app.dispatch(Action::ReactionKey('c'));
        // Enter with nothing typed: close the overlay, but record no reaction.
        app.dispatch(Action::ReactionConfirm);
        assert_eq!(app.overlay(), Overlay::None);
        assert!(
            app.conversation()
                .messages()
                .iter()
                .all(|m| m.reactions.is_empty()),
            "empty custom line adds no reaction"
        );
        assert_eq!(app.take_reaction(), None, "no intent recorded");
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
        assert!(
            app.take_media().is_none(),
            "an empty prompt records no send"
        );
        // A path makes it sendable; confirm then closes.
        for c in "/tmp/a.png".chars() {
            app.dispatch(Action::AttachInput(c));
        }
        app.dispatch(Action::AttachConfirm);
        assert_eq!(app.overlay(), Overlay::None);
    }

    #[test]
    fn confirming_an_attach_records_the_media_for_the_loop() {
        use tuigram_core::model::OutgoingMedia;

        let mut app = app_with_history();
        app.dispatch(Action::AttachOpen);
        for c in "/tmp/clip.mp4".chars() {
            app.dispatch(Action::AttachInput(c));
        }
        app.dispatch(Action::AttachToggleField);
        for c in "watch".chars() {
            app.dispatch(Action::AttachInput(c));
        }
        app.dispatch(Action::AttachConfirm);
        assert_eq!(app.overlay(), Overlay::None, "the prompt closed");

        // The intent carries the extension-inferred variant, path, and caption, and
        // drains exactly once (a second drain is empty).
        match app.take_media() {
            Some(OutgoingMedia::Video { path, caption }) => {
                assert_eq!(path, "/tmp/clip.mp4");
                assert_eq!(caption.text, "watch");
            }
            other => panic!("expected a video attachment, got {other:?}"),
        }
        assert!(app.take_media().is_none(), "drained once");
    }

    #[test]
    fn projecting_downloads_fills_the_conversation_progress_state() {
        use tuigram_core::model::File;

        let mut app = app_with_history();
        app.project_downloads(vec![File {
            id: 42,
            size: 100,
            downloaded_size: 40,
            is_downloading_active: true,
            ..File::default()
        }]);
        let file = app
            .conversation()
            .download(42)
            .expect("projected download state");
        assert_eq!(file.downloaded_size, 40);
        assert!(file.is_downloading_active);
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

    // --- settings editor (#146) ---

    use crate::settingsform::SettingsField;
    use tuigram_core::{CacheCap, KeepMedia, StorageSettings};

    /// Backspace over the focused field's current text, so a test can retype it.
    fn clear_field(app: &mut App) {
        for _ in 0..app.settings().value(app.settings().field()).chars().count() {
            app.dispatch(Action::SettingsBackspace);
        }
    }

    #[test]
    fn opening_settings_prefills_from_the_live_policy() {
        let mut app = App::new();
        app.set_storage_settings(StorageSettings {
            keep_private: KeepMedia::Forever,
            keep_groups: KeepMedia::Days(7),
            keep_channels: KeepMedia::Days(3),
            max_cache: CacheCap::Bytes(2 * 1024 * 1024 * 1024),
        });
        app.dispatch(Action::SettingsOpen);
        assert_eq!(app.overlay(), Overlay::Settings);
        assert_eq!(app.settings().value(SettingsField::KeepGroups), "7d");
        assert_eq!(app.settings().value(SettingsField::MaxCache), "2GB");
    }

    #[test]
    fn confirming_a_valid_edit_applies_it_live_and_hands_it_to_the_loop() {
        let mut app = App::new();
        app.dispatch(Action::SettingsOpen);
        // Edit channels forever -> 3d and the cache unbounded -> 2GB.
        app.dispatch(Action::SettingsToggleField); // groups
        app.dispatch(Action::SettingsToggleField); // channels
        clear_field(&mut app);
        for c in "3d".chars() {
            app.dispatch(Action::SettingsInput(c));
        }
        app.dispatch(Action::SettingsToggleField); // max cache
        clear_field(&mut app);
        for c in "2GB".chars() {
            app.dispatch(Action::SettingsInput(c));
        }
        app.dispatch(Action::SettingsConfirm);
        assert_eq!(
            app.overlay(),
            Overlay::None,
            "a valid edit closes the editor"
        );

        // The loop drains the new policy exactly once.
        let updated = app
            .take_settings()
            .expect("a confirmed edit is handed back");
        assert_eq!(updated.keep_channels, KeepMedia::Days(3));
        assert_eq!(updated.max_cache, CacheCap::Bytes(2 * 1024 * 1024 * 1024));
        assert!(app.take_settings().is_none(), "drained once");

        // Reopening shows the applied values, proving the in-memory policy updated.
        app.dispatch(Action::SettingsOpen);
        assert_eq!(app.settings().value(SettingsField::KeepChannels), "3d");
    }

    #[test]
    fn an_invalid_edit_keeps_the_editor_open_and_records_nothing() {
        let mut app = App::new();
        app.dispatch(Action::SettingsOpen);
        clear_field(&mut app); // private field: "forever" -> ""
        for c in "2TB".chars() {
            app.dispatch(Action::SettingsInput(c));
        }
        app.dispatch(Action::SettingsToggleField); // move off before confirming
        app.dispatch(Action::SettingsConfirm);
        assert_eq!(
            app.overlay(),
            Overlay::Settings,
            "an invalid value keeps the editor open"
        );
        assert!(
            app.take_settings().is_none(),
            "an invalid edit is never handed to the loop"
        );
        assert!(
            app.settings().error().is_some(),
            "the reason is shown in place"
        );
    }

    #[test]
    fn cancelling_settings_discards_the_edit() {
        let mut app = App::new();
        app.dispatch(Action::SettingsOpen);
        clear_field(&mut app);
        for c in "3d".chars() {
            app.dispatch(Action::SettingsInput(c));
        }
        app.dispatch(Action::SettingsCancel);
        assert_eq!(app.overlay(), Overlay::None);
        assert!(app.take_settings().is_none(), "a cancel records nothing");
        // The live policy is untouched — reopening shows the original default.
        app.dispatch(Action::SettingsOpen);
        assert_eq!(app.settings().value(SettingsField::KeepPrivate), "forever");
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
    fn confirming_the_secret_prompt_records_the_action_for_the_loop() {
        use tuigram_core::model::ChatKind;
        let mut app = app_with_one_chat(ChatKind::Private { user_id: 7 }, None);
        app.dispatch(Action::SecretOpen);
        app.dispatch(Action::SecretConfirm);
        assert_eq!(app.overlay(), Overlay::None, "confirm closes the modal");
        assert!(app.secret().is_none(), "prompt state cleared");
        // The confirmed lifecycle is queued for the loop to dispatch on the seam,
        // then drained exactly once.
        assert_eq!(
            app.take_secret(),
            Some(SecretLifecycle::Start { user_id: 7 })
        );
        assert!(app.take_secret().is_none(), "drained once");
    }

    #[test]
    fn cancelling_the_secret_prompt_acts_on_nothing() {
        use tuigram_core::model::ChatKind;
        let mut app = app_with_one_chat(ChatKind::Private { user_id: 7 }, None);
        app.dispatch(Action::SecretOpen);
        app.dispatch(Action::SecretCancel);
        assert_eq!(app.overlay(), Overlay::None);
        assert!(app.secret().is_none());
        assert!(app.take_secret().is_none(), "cancel dispatches nothing");
    }

    #[test]
    fn projecting_secret_states_lands_them_on_the_chat_list() {
        use tuigram_core::model::{ChatKind, SecretChatState};
        let mut app = app_with_one_chat(
            ChatKind::Secret {
                secret_chat_id: 9,
                user_id: 7,
            },
            None,
        );
        assert!(app.chat_list().secret_state(5).is_none());
        app.project_secret_states(vec![(5, SecretChatState::Ready)]);
        assert_eq!(
            app.chat_list().secret_state(5),
            Some(SecretChatState::Ready)
        );
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

    // --- #197: contact-search picker for new secret chats ---

    #[test]
    fn contact_search_open_resets_and_opens_the_query_overlay() {
        let mut app = App::new();
        app.dispatch(Action::ContactSearchOpen);
        assert_eq!(app.overlay(), Overlay::ContactSearchInput);
        assert_eq!(app.contacts().query(), "");
    }

    #[test]
    fn contact_search_submit_records_the_query_and_switches_to_results() {
        let mut app = App::new();
        app.dispatch(Action::ContactSearchOpen);
        for c in "ada".chars() {
            app.dispatch(Action::ContactSearchInput(c));
        }
        app.dispatch(Action::ContactSearchSubmit);
        assert_eq!(app.overlay(), Overlay::ContactSearchResults);
        assert_eq!(app.take_contact_search().as_deref(), Some("ada"));
    }

    #[test]
    fn contact_search_submit_on_a_blank_query_is_a_noop() {
        let mut app = App::new();
        app.dispatch(Action::ContactSearchOpen);
        app.dispatch(Action::ContactSearchSubmit);
        assert_eq!(
            app.overlay(),
            Overlay::ContactSearchInput,
            "stays on the input line"
        );
        assert!(app.take_contact_search().is_none());
    }

    #[test]
    fn confirming_a_contact_hit_opens_the_shared_secret_chat_confirm() {
        use crate::contact_picker::ContactHit;
        let mut app = App::new();
        app.dispatch(Action::ContactSearchOpen);
        app.set_contact_results(vec![ContactHit::new(7, "Ada Lovelace")]);
        app.dispatch(Action::ContactResultConfirm);
        assert_eq!(app.overlay(), Overlay::SecretChat);
        assert_eq!(
            app.secret().map(|p| p.lifecycle()),
            Some(SecretLifecycle::Start { user_id: 7 })
        );
        assert!(app.secret().unwrap().prompt().contains("Ada Lovelace"));
    }

    #[test]
    fn confirming_with_no_contact_hits_is_a_noop() {
        let mut app = App::new();
        app.dispatch(Action::ContactSearchOpen);
        app.dispatch(Action::ContactResultConfirm);
        assert_eq!(app.overlay(), Overlay::ContactSearchInput);
        assert!(app.secret().is_none());
    }

    #[test]
    fn contact_search_cancel_closes_the_overlay() {
        let mut app = App::new();
        app.dispatch(Action::ContactSearchOpen);
        app.dispatch(Action::ContactSearchCancel);
        assert_eq!(app.overlay(), Overlay::None);
    }

    #[test]
    fn contact_search_keys_resolve_through_the_overlay_not_the_panes() {
        let mut app = App::new();
        app.dispatch(Action::ContactSearchOpen);
        // `n` would reopen the picker in the chat list; inside the input line it types.
        assert_eq!(
            app.on_terminal_event(key(KeyCode::Char('n'), KeyModifiers::NONE)),
            Action::ContactSearchInput('n')
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
    fn tick_notices_ages_out_the_toast_and_repaints_only_when_it_leaves() {
        // The notice clock (#139) drives the toast's lifetime; the app repaints only
        // on the tick that actually drops it, since a still-counting toast is unchanged.
        let mut app = App::new();
        app.notify(Notice::info("download complete"));
        // Age it to the brink of expiry without dropping it — no repaint owed.
        let mut ticks = 0;
        loop {
            app.clear_dirty();
            app.tick_notices();
            if app.notifications().current().is_none() {
                break;
            }
            assert!(!app.is_dirty(), "a still-counting toast owes no repaint");
            ticks += 1;
            assert!(ticks < 100, "toast should have expired by now");
        }
        // The expiring tick dropped it and asked for a repaint.
        assert!(app.is_dirty(), "the expiring tick repaints");
        assert!(ticks > 0, "the toast survived at least one tick");
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

    // ---- #195: REPL→TUI command-parity actions ----

    /// Build an app whose open conversation is a single `message`, selected.
    fn app_with_message(message: Message) -> App {
        use crate::conversation::ConversationView;
        App::with_conversation(ConversationView::from_messages(
            vec![message],
            HashSet::new(),
        ))
    }

    /// A text message with `id`; `own` marks it as ours (outgoing).
    fn text_message(id: i64, body: &str, own: bool) -> Message {
        use tuigram_core::model::{FormattedText, MessageContent};
        let mut m = crate::conversation::sample_message(
            id,
            MessageContent::Text(FormattedText {
                text: body.to_owned(),
                entities: Vec::new(),
            }),
        );
        m.is_outgoing = own;
        m
    }

    #[test]
    fn reply_message_enters_reply_mode_and_focuses_the_composer() {
        use crate::composer::ComposerMode;
        let mut app = app_with_message(text_message(42, "hello", false));
        app.dispatch(Action::ReplyMessage);
        assert_eq!(app.focus(), Focus::Composer);
        assert!(matches!(
            app.composer().mode(),
            ComposerMode::Reply { message_id: 42, .. }
        ));
    }

    #[test]
    fn edit_message_prefills_only_for_our_own_text_messages() {
        use crate::composer::ComposerMode;
        // Someone else's message: not editable — composer stays in compose, focus
        // stays on the (default) chat list, and a toast explains why.
        let mut app = app_with_message(text_message(7, "hi", false));
        app.dispatch(Action::EditMessage);
        assert_eq!(app.focus(), Focus::ChatList);
        assert!(matches!(app.composer().mode(), ComposerMode::Compose));
        assert!(app.notifications.current().is_some());

        // Our own text message: editable — the composer pre-fills and focuses.
        let mut app = app_with_message(text_message(9, "mine", true));
        app.dispatch(Action::EditMessage);
        assert_eq!(app.focus(), Focus::Composer);
        assert!(matches!(
            app.composer().mode(),
            ComposerMode::Edit { message_id: 9 }
        ));
        assert_eq!(app.composer().text(), "mine");
    }

    #[test]
    fn delete_message_opens_the_confirm_and_records_the_scoped_intent() {
        let mut msg = text_message(5, "x", true);
        msg.chat_id = 77;
        let mut app = app_with_message(msg);
        app.dispatch(Action::DeleteMessage);
        assert_eq!(app.overlay(), Overlay::DeleteConfirm);
        // Defaults to the safe scope (for me); Tab flips it to for everyone since the
        // message is ours.
        assert!(!app.delete().expect("prompt open").revoke());
        app.dispatch(Action::DeleteToggleScope);
        assert!(app.delete().expect("prompt open").revoke());
        app.dispatch(Action::DeleteConfirm);
        assert_eq!(app.overlay(), Overlay::None);
        let intent = app.take_delete().expect("recorded delete");
        assert_eq!(intent.chat_id, 77);
        assert_eq!(intent.message_ids, vec![5]);
        assert!(intent.revoke);
    }

    #[test]
    fn delete_scope_stays_for_me_on_someone_elses_message() {
        let mut app = app_with_message(text_message(3, "x", false));
        app.dispatch(Action::DeleteMessage);
        // Not ours: "for everyone" is unavailable, so the toggle never revokes.
        assert!(!app.delete().expect("prompt open").can_revoke());
        app.dispatch(Action::DeleteToggleScope);
        assert!(!app.delete().expect("prompt open").revoke());
        // Cancel drops the prompt without recording an intent.
        app.dispatch(Action::DeleteCancel);
        assert_eq!(app.overlay(), Overlay::None);
        assert!(app.delete().is_none());
        assert!(app.take_delete().is_none());
    }

    #[test]
    fn save_media_records_a_file_id_for_media_and_toasts_otherwise() {
        use tuigram_core::model::{FileRef, MessageContent, Photo};
        // A media message: the file id is recorded for the loop to save.
        let mut photo = crate::conversation::sample_message(
            1,
            MessageContent::Photo(Photo {
                caption: Default::default(),
                file: FileRef::new(99),
                width: 1,
                height: 1,
            }),
        );
        photo.is_outgoing = false;
        let mut app = app_with_message(photo);
        app.dispatch(Action::SaveMedia);
        assert_eq!(app.take_save(), Some(99));

        // A text message has no media: nothing recorded, a toast instead.
        let mut app = app_with_message(text_message(2, "hi", false));
        app.dispatch(Action::SaveMedia);
        assert_eq!(app.take_save(), None);
        assert!(app.notifications.current().is_some());
    }

    #[test]
    fn copy_message_records_text_for_a_text_message_and_toasts_otherwise() {
        use tuigram_core::model::{FileRef, MessageContent, Photo};
        // A text message: its text is recorded for the loop to copy.
        let mut app = app_with_message(text_message(1, "hello there", false));
        app.dispatch(Action::CopyMessage);
        assert_eq!(app.take_copy().as_deref(), Some("hello there"));

        // A media message has no text: nothing recorded, a toast instead.
        let mut photo = crate::conversation::sample_message(
            2,
            MessageContent::Photo(Photo {
                caption: Default::default(),
                file: FileRef::new(9),
                width: 1,
                height: 1,
            }),
        );
        photo.is_outgoing = false;
        let mut app = app_with_message(photo);
        app.dispatch(Action::CopyMessage);
        assert_eq!(app.take_copy(), None);
        assert!(app.notifications.current().is_some());
    }

    #[test]
    fn copy_message_on_an_empty_history_is_a_noop() {
        let mut app = App::new();
        app.dispatch(Action::CopyMessage);
        assert_eq!(app.take_copy(), None);
        assert!(app.notifications.current().is_none());
    }

    #[test]
    fn composer_input_pings_typing_but_backspace_to_empty_does_not() {
        let mut app = App::new();
        assert!(!app.take_wants_typing_ping(), "nothing typed yet");

        app.dispatch(Action::ComposerInput('h'));
        assert!(app.take_wants_typing_ping());
        // Draining clears the pulse until the next edit.
        assert!(!app.take_wants_typing_ping());

        app.dispatch(Action::ComposerBackspace);
        // The buffer is empty again: nothing left to indicate.
        assert!(!app.take_wants_typing_ping());

        app.dispatch(Action::ComposerInput('h'));
        app.dispatch(Action::ComposerInput('i'));
        app.take_wants_typing_ping();
        app.dispatch(Action::ComposerBackspace);
        // Still "hi" -> "h" left in the buffer: still worth a ping.
        assert!(app.take_wants_typing_ping());
    }

    #[test]
    fn resync_records_a_request_drained_once() {
        let mut app = App::new();
        app.dispatch(Action::Resync);
        assert!(app.take_resync(), "the request is recorded");
        assert!(!app.take_resync(), "and cleared after one drain");
    }

    #[test]
    fn logout_confirm_flow_records_a_request_and_cancel_does_not() {
        let mut app = App::new();
        app.dispatch(Action::LogoutOpen);
        assert_eq!(app.overlay(), Overlay::LogoutConfirm);
        // Cancel closes the confirm without recording a logout.
        app.dispatch(Action::LogoutCancel);
        assert_eq!(app.overlay(), Overlay::None);
        assert!(!app.take_logout());
        // Confirm records the logout for the loop and closes.
        app.dispatch(Action::LogoutOpen);
        app.dispatch(Action::LogoutConfirm);
        assert_eq!(app.overlay(), Overlay::None);
        assert!(app.take_logout());
        assert!(!app.take_logout(), "cleared after one drain");
    }
}
