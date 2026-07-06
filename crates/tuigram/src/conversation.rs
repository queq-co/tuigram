//! The conversation view-model: the projection the history pane renders from.
//!
//! The core [`MessageStore`](tuigram_core::messages::MessageStore) folds TDLib's
//! per-chat history; this is the TUI side of it — a display snapshot of the open
//! chat's [`Message`]s plus the cursor state the store has no opinion on: the set
//! of pinned message ids (carried by the chat, see
//! [`Chat::pinned_message_ids`](tuigram_core::model::Chat::pinned_message_ids))
//! and the scroll **offset** into the history. Phase 6 fills it from the store
//! over the event channel; Phase 5 leaves it empty, so the history pane shows the
//! welcome placeholder while the scrolling behaviour is still exercised headlessly
//! against whatever messages it holds.
//!
//! Messages are held oldest-first — the order they render top-to-bottom — and the
//! offset is an index into them: row `offset` is the topmost message drawn, and
//! the render windows forward from there so a long history never builds the whole
//! buffer.

use std::collections::{HashMap, HashSet};

use ratatui::style::Color;
use tuigram_core::model::{
    ChatAction, File, FormattedText, Message, MessageContent, Reaction, ReactionKind, Sender, User,
};

/// A confirmed pin toggle, recorded by `App` as a pure intent for the loop to
/// dispatch (#119) — the message, and whether the toggle **pinned** it or
/// **unpinned** it. `App` never touches the `Client`, so
/// [`PinToggle`](crate::app::Action::PinToggle) flips the pinned set optimistically
/// and records this; the loop drains it into
/// [`PinRequests::pin_chat_message`](tuigram_core::PinRequests::pin_chat_message) /
/// `unpin_chat_message`, the same intent-then-drain split forwarding (#118) uses.
/// The real `updateMessageIsPinned` then reconciles the chat's pinned set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinIntent {
    /// The chat holding the pinned message — `pin`/`unpin_chat_message`'s chat.
    pub chat_id: i64,
    /// The message being pinned or unpinned, by id.
    pub message_id: i64,
    /// Whether the toggle pinned the message (`true` → `pin_chat_message`) or
    /// unpinned it (`false` → `unpin_chat_message`), decided from its pre-toggle
    /// pinned state.
    pub pin: bool,
}

/// The delete-confirm overlay's state (#195): the message a `d` in the history
/// targets and the scope the confirm will use. Deleting is destructive, so it is
/// gated behind an explicit confirm — this holds what is being deleted and whether
/// the pending confirm revokes it **for everyone** or removes it **only for us**.
/// [`toggle_revoke`](Self::toggle_revoke) flips the scope; [`into_intent`](Self::into_intent)
/// resolves it into the pure [`DeleteIntent`] the loop dispatches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeletePrompt {
    chat_id: i64,
    message_id: i64,
    /// Whether the target is our own message — only then can Telegram delete it for
    /// everyone, so the "for everyone" scope is offered only when this holds.
    own: bool,
    /// The current scope: `true` deletes for everyone (revoke), `false` only for us.
    revoke: bool,
    /// A short label of the message, shown in the confirm so the user sees what
    /// they are about to delete.
    preview: String,
}

impl DeletePrompt {
    /// A confirm for deleting `message_id` in `chat_id`. Defaults to the safe
    /// scope — delete **only for us** — so a reflexive Enter never revokes a
    /// message for the whole chat; the user opts into "for everyone" with the
    /// scope toggle, and only when it is their own message.
    #[must_use]
    pub fn new(chat_id: i64, message_id: i64, own: bool, preview: String) -> Self {
        Self {
            chat_id,
            message_id,
            own,
            revoke: false,
            preview,
        }
    }

    /// Flip the delete scope between "for me" and "for everyone". A no-op for a
    /// message that is not ours, which can only ever be deleted for us.
    pub fn toggle_revoke(&mut self) {
        if self.own {
            self.revoke = !self.revoke;
        }
    }

    /// Whether the current scope is "for everyone" (revoke).
    #[must_use]
    pub fn revoke(&self) -> bool {
        self.revoke
    }

    /// Whether "for everyone" is an available scope (the message is our own).
    #[must_use]
    pub fn can_revoke(&self) -> bool {
        self.own
    }

    /// The message preview shown in the confirm.
    #[must_use]
    pub fn preview(&self) -> &str {
        &self.preview
    }

    /// Resolve the confirmed prompt into the pure [`DeleteIntent`] the loop drains.
    #[must_use]
    pub fn into_intent(self) -> DeleteIntent {
        DeleteIntent {
            chat_id: self.chat_id,
            message_ids: vec![self.message_id],
            revoke: self.revoke,
        }
    }
}

/// A confirmed delete, recorded by `App` as a pure intent for the loop to dispatch
/// (#195). Unlike the pin/reaction toggles there is no optimistic local change —
/// the authoritative `updateDeleteMessages` folds and re-projects the history, so
/// the loop only issues [`DeleteRequests::delete`](tuigram_core::messages::DeleteRequests)
/// and lets the removal arrive through the normal pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteIntent {
    /// The chat holding the message(s) — `delete`'s chat.
    pub chat_id: i64,
    /// The messages to delete, by id.
    pub message_ids: Vec<i64>,
    /// `true` deletes for everyone (revoke), `false` only for us.
    pub revoke: bool,
}

/// The history pane's state: the open chat's messages (oldest first), which of
/// them are pinned, the scroll offset, and the download state of any media files
/// the visible messages reference. Empty until Phase 6 projects the core message
/// and file stores into it.
///
/// The scroll **offset** doubles as the **message cursor**: the message at the
/// offset — the topmost one drawn — is the *selected* message that the
/// reaction/pin affordances (#85) act on, marked in the pane. This reuses #81's
/// `j`/`k` history navigation as the cursor rather than introducing a second,
/// independently moved selection; finer in-pane selection is a follow-up.
#[derive(Debug, Clone, Default)]
pub struct ConversationView {
    /// The chat this history belongs to, or `None` before any chat is opened.
    /// [`project`](Self::project) reads it to tell a *refresh of the same chat*
    /// (preserve the cursor) from *switching to a different chat* (fresh view).
    chat_id: Option<i64>,
    /// Messages in chronological order — index `0` is the oldest, drawn at the top.
    messages: Vec<Message>,
    /// Ids of the chat's pinned messages, for the pinned indicator.
    pinned: HashSet<i64>,
    /// Index of the topmost message to draw — also the selected-message cursor.
    /// Clamped to a valid row, or `0` when there are no messages.
    offset: usize,
    /// Rows of `messages[offset]` already scrolled past (#222): `0` means its
    /// header is the first visible row, up to one less than
    /// `message_height(messages[offset])` otherwise, so at least one of its
    /// rows (down to the trailing blank separator) stays on screen. The
    /// row-granular counterpart to `offset`, which stays a message index —
    /// `offset` alone still answers "which message is selected," `row_skip`
    /// alone answers "how far into it has the reader scrolled."
    row_skip: usize,
    /// The history pane's inner height (rows) from the last render, recorded by the
    /// loop via [`set_viewport_height`](Self::set_viewport_height) (#158). The
    /// bottom-anchoring walk sums per-message heights against it to decide which
    /// message sits at the top when the newest is pinned to the bottom. `0` until the
    /// first frame measures it; the anchor then falls back to the newest message
    /// alone and the next render re-anchors against the real height.
    viewport: usize,
    /// Download state of media files referenced by the messages, keyed by TDLib
    /// file id, for the download-progress indicator (#85). Phase 6 projects this
    /// from the core [`FileStore`](tuigram_core::files::FileStore); empty until then.
    downloads: HashMap<i32, File>,
    /// The transient chat action in the open chat (#87) — the "typing…" indicator
    /// shown in the conversation header. `None` when no one is acting. Phase 6
    /// projects this from the core [`ChatActionStore`](tuigram_core::ChatActionStore);
    /// it is never part of the message history.
    chat_action: Option<ChatAction>,
    /// Display labels for the history's message senders (#160, #194), resolved by
    /// the loop from the core user/chat stores and keyed by [`Sender`]: a user's
    /// `"Name (@handle)"` plus their accent color, or a chat's title (untinted). A
    /// sender absent here (its record not yet folded) falls back to the bare
    /// `User {id}` / `Chat {id}` in [`sender_label`](Self::sender_label), so the
    /// header is always legible.
    senders: HashMap<Sender, SenderLabel>,
    /// The chat's outbox read watermark (#163): the id of the last message of ours
    /// the peer has read. Kept live on every [`project`](Self::project) (unlike
    /// [`unread_separator`](Self::unread_separator), it is not frozen), since the
    /// read receipt glyph should advance the instant a read-outbox update repaints
    /// this view.
    last_read_outbox: i64,
    /// The unread-messages separator's target (#164), or `None` while still
    /// *pending* resolution. Outer `None` (pending) means either the chat was just
    /// (re)opened and has not yet been resolved against real data, or the history
    /// store had not warmed up yet on the call that opened it; `Some(None)` is a
    /// resolved "nothing unread"; `Some(Some(id))` is a resolved target. Once
    /// resolved it is left alone by ordinary same-chat refreshes — a later
    /// mark-read can never erase the rule the instant it appears — but a genuine
    /// re-open (see `fresh_open` on [`project`](Self::project)) resets it to
    /// pending so reopening a now-fully-read chat correctly shows no rule.
    unread_separator: Option<Option<i64>>,
    /// Whether the terminal speaks a graphics protocol (#208), seeded once via
    /// [`set_graphics_capable`](Self::set_graphics_capable) from `App`'s own
    /// one-time [`set_avatar_support`](crate::app::App::set_avatar_support)
    /// seed — kept here (rather than read from `App` at render time) so
    /// [`message_height`](Self::message_height) stays a pure function of this
    /// view's own state, computable in tests with no real `Picker`. Carried
    /// across a chat switch in [`project`](Self::project) the same way
    /// `viewport` is: it is a terminal-level fact, not per-chat state.
    graphics_capable: bool,
}

impl ConversationView {
    /// Build a view from the open chat's history (oldest first) and its set of
    /// pinned message ids, scrolled to the top. The viewport is unmeasured (`0`),
    /// so the bottom-anchoring of a real open ([`project`](Self::project)) is not in
    /// play here; this is the raw seam the render tests place content with.
    ///
    /// The Phase 6 update path (and the render tests) build the view this way; the
    /// running binary still shows the empty [`default`](Self::default) until that
    /// path is wired, so this is unused in the non-test binary for now.
    #[allow(dead_code)]
    #[must_use]
    pub fn from_messages(messages: Vec<Message>, pinned: HashSet<i64>) -> Self {
        Self {
            chat_id: None,
            messages,
            pinned,
            offset: 0,
            row_skip: 0,
            viewport: 0,
            downloads: HashMap::new(),
            chat_action: None,
            senders: HashMap::new(),
            last_read_outbox: 0,
            unread_separator: None,
            graphics_capable: false,
        }
    }

    /// Re-project the open chat's history from the core
    /// [`MessageStore`](tuigram_core::messages::MessageStore) (#114). The loop reads
    /// `chat_id`'s messages (oldest first) and pinned ids back from the `Client` and
    /// hands the owned snapshot here, so `App` stays pure — the same split as the
    /// chat-list projection (#113).
    ///
    /// **Refreshing the same chat** (a live update, or a freshly-merged history
    /// page) keeps the reader where they are. If the view was pinned to the newest
    /// message — sitting at the bottom-anchored position — it *follows* the tail onto
    /// the new newest (#159). Otherwise the cursor is preserved by message *id*, not
    /// index: the selected message keeps its place even as older messages are
    /// prepended above it or a new one arrives below, so reading history is never
    /// interrupted. (A scroll-up at the very top first triggers an older page; the
    /// cursor then sits one row down from the top, so the next scroll-up reveals the
    /// newly loaded messages.)
    ///
    /// **Switching to a different chat** drops the previous chat's view entirely —
    /// messages, cursor, and the per-message download/typing state — and opens
    /// bottom-anchored at the newest message (#158), the way a chat client does. The
    /// last-rendered viewport height carries over (the pane geometry is unchanged),
    /// so the anchor is right immediately; the next render re-confirms it.
    ///
    /// `fresh_open` marks the one moment that actually counts as "the user opened
    /// this chat" — driven by the loop's own open/close tracking, not derived from
    /// whether `chat_id` differs from the last projection. That distinction matters
    /// because a chat_id-only check conflates two different things: focus merely
    /// leaving and returning to the *same* chat (a continuation — #158's cursor
    /// stays put in that case, deliberately) versus a genuine re-open, which must
    /// re-resolve the unread separator against the *current* inbox watermark (#164)
    /// — otherwise reopening a chat that has since been fully read would still show
    /// a stale rule forever, since a same-chat refresh alone never recomputes it.
    #[allow(clippy::too_many_arguments)]
    pub fn project(
        &mut self,
        chat_id: i64,
        messages: Vec<Message>,
        pinned: HashSet<i64>,
        senders: HashMap<Sender, SenderLabel>,
        last_read_inbox: i64,
        last_read_outbox: i64,
        fresh_open: bool,
    ) {
        self.last_read_outbox = last_read_outbox;
        if self.chat_id == Some(chat_id) {
            // Derive follow-ness from the *old* view before the swap: were we pinned
            // to the newest message? If so, advance onto the new newest; if not, hold
            // the selected message under the cursor by id.
            let following = self.is_at_newest();
            let anchor = self.selected_message().map(|m| m.id);
            self.messages = messages;
            self.pinned = pinned;
            self.senders = senders;
            if following {
                (self.offset, self.row_skip) = self.newest_anchor();
            } else {
                self.offset = anchor
                    .and_then(|id| self.messages.iter().position(|m| m.id == id))
                    .unwrap_or(self.offset)
                    .min(self.messages.len().saturating_sub(1));
                // The target message's own shape may have changed (a reaction
                // added, media finishing a download); land on its header rather
                // than trying to preserve an exact row position across that.
                self.row_skip = 0;
            }
        } else {
            // A different chat opened: a fresh view, dropping the previous chat's
            // per-message state (downloads, typing indicator), bottom-anchored at the
            // newest message. Carry the measured viewport and the terminal's graphics
            // capability (#208) — neither is per-chat state — so neither the anchor
            // nor the media-row math falls back to its startup default on every
            // chat switch.
            let viewport = self.viewport;
            let graphics_capable = self.graphics_capable;
            *self = Self {
                chat_id: Some(chat_id),
                messages,
                pinned,
                viewport,
                senders,
                last_read_outbox,
                graphics_capable,
                ..Self::default()
            };
            (self.offset, self.row_skip) = self.newest_anchor();
        }

        // #164: a genuine re-open resets the separator to *pending*, regardless of
        // which branch above ran, so reopening a chat that's since been fully read
        // resolves against the fresh watermark rather than keeping a stale rule.
        if fresh_open {
            self.unread_separator = None;
        }
        // While pending, resolve as soon as real history is present. Gating on a
        // non-empty history defers resolution past the landing-page race: `open`'s
        // very first projection can fire before the async history page merges, and
        // resolving "nothing unread" against that empty snapshot would freeze the
        // wrong answer before the real messages ever arrive as a same-chat refresh.
        // Once resolved, it is left alone — a later live update (mark-read, a new
        // message) must not erase or move the rule out from under the reader.
        //
        // This correctly waits for that landing page only because `last_read_inbox`
        // itself cannot have advanced yet on an empty open: `drive_read_state`
        // (main.rs) early-returns when the store holds no loaded messages for the
        // chat, so mark-read never races ahead of the history it would need to mark.
        // If that early-return were ever removed, this resolution would need to gate
        // on more than "non-empty" to still catch the true first-unread message.
        if self.unread_separator.is_none() && !self.messages.is_empty() {
            self.unread_separator = Some(
                self.messages
                    .iter()
                    .find(|m| !m.is_outgoing && m.id > last_read_inbox)
                    .map(|m| m.id),
            );
        }
    }

    /// The messages to render, oldest first.
    #[must_use]
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Whether the open chat has no messages (the Phase 5 placeholder state).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// Number of messages in the history — the scrollbar's content length.
    #[must_use]
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    /// The scroll offset: the index of the topmost message drawn.
    #[must_use]
    pub fn offset(&self) -> usize {
        self.offset
    }

    /// Rows of the message at [`offset`](Self::offset) already scrolled past
    /// (#222) — `0` means its header is the first visible row. The render
    /// loop drops this many lines from that message's own block before
    /// drawing it.
    #[must_use]
    pub(crate) fn row_skip(&self) -> usize {
        self.row_skip
    }

    /// Whether the message with id `id` is pinned in this chat.
    #[must_use]
    pub fn is_pinned(&self, id: i64) -> bool {
        self.pinned.contains(&id)
    }

    /// The chat's outbox read watermark (#163): a message with this id or lower, if
    /// ours, has been read by the peer.
    #[must_use]
    pub fn last_read_outbox(&self) -> i64 {
        self.last_read_outbox
    }

    /// Whether the unread-messages rule (#164) belongs immediately above message
    /// `id` — the first incoming message unread as of this chat's open.
    #[must_use]
    pub fn unread_separator_before(&self, id: i64) -> bool {
        self.unread_separator.flatten() == Some(id)
    }

    /// The selected message — the one at the scroll [`offset`](Self::offset),
    /// drawn at the top of the pane — or `None` on an empty history. The reaction
    /// and pin affordances act on this message.
    #[must_use]
    pub fn selected_message(&self) -> Option<&Message> {
        self.messages.get(self.offset)
    }

    /// The header name for a message (#160, #194): `"You"` for our own messages,
    /// else the sender's resolved [`SenderLabel`] — a user's `"Name (@handle)"`
    /// tinted with their accent color, or a chat's untinted title, folded in by the
    /// loop — falling back to the bare, untinted `User {id}` / `Chat {id}` when the
    /// record has not arrived yet, so the header is never blank or ambiguous.
    #[must_use]
    pub(crate) fn sender_label(&self, message: &Message) -> SenderLabel {
        if message.is_outgoing {
            return SenderLabel {
                label: "You".to_owned(),
                color: None,
            };
        }
        if let Some(label) = self.senders.get(&message.sender) {
            return label.clone();
        }
        let label = match message.sender {
            Sender::User(id) => format!("User {id}"),
            Sender::Chat(id) => format!("Chat {id}"),
        };
        SenderLabel { label, color: None }
    }

    /// Toggle the pinned state of message `id`: pin it if it is not pinned, unpin
    /// it if it is. The optimistic local flip behind the pin action; Phase 6 also
    /// calls [`PinRequests`](tuigram_core::PinRequests) and lets the resulting
    /// `updateMessageIsPinned` reconcile this set.
    pub fn toggle_pin(&mut self, id: i64) {
        if !self.pinned.remove(&id) {
            self.pinned.insert(id);
        }
    }

    /// Whether tuigram's own account has already reacted to message `id` with
    /// `emoji`. Read *before* the optimistic [`toggle_reaction`](Self::toggle_reaction)
    /// so the confirm can tell whether it is adding a reaction or removing one, and
    /// record the matching core call (#119). `false` for an unknown id.
    #[must_use]
    pub fn has_own_reaction(&self, id: i64, emoji: &str) -> bool {
        let kind = ReactionKind::Emoji(emoji.to_owned());
        self.messages
            .iter()
            .find(|m| m.id == id)
            .is_some_and(|m| m.reactions.iter().any(|r| r.is_chosen && r.kind == kind))
    }

    /// Toggle tuigram's own `emoji` reaction on message `id`, updating the
    /// message's reaction buckets the existing `{emoji×n*}` rendering reads:
    /// adding our choice creates or increments the bucket and marks it chosen;
    /// removing it decrements (dropping a bucket that reaches zero). A no-op if no
    /// message has that id.
    ///
    /// This is the optimistic local reflection of the reaction picker; Phase 6
    /// also calls [`ReactionRequests`](tuigram_core::ReactionRequests) and lets the
    /// resulting `updateMessageInteractionInfo` fold the authoritative counts.
    pub fn toggle_reaction(&mut self, id: i64, emoji: &str) {
        let Some(message) = self.messages.iter_mut().find(|m| m.id == id) else {
            return;
        };
        let kind = ReactionKind::Emoji(emoji.to_owned());
        match message.reactions.iter().position(|r| r.kind == kind) {
            Some(i) if message.reactions[i].is_chosen => {
                // Remove our own reaction; drop the bucket if we were the last.
                message.reactions[i].count -= 1;
                message.reactions[i].is_chosen = false;
                if message.reactions[i].count <= 0 {
                    message.reactions.remove(i);
                }
            }
            Some(i) => {
                // Others have it; add our choice to the existing bucket.
                message.reactions[i].count += 1;
                message.reactions[i].is_chosen = true;
            }
            None => message.reactions.push(Reaction {
                kind,
                count: 1,
                is_chosen: true,
            }),
        }
    }

    /// The download state of the file with TDLib id `file_id`, if known — the
    /// source of the media download-progress indicator.
    #[must_use]
    pub fn download(&self, file_id: i32) -> Option<&File> {
        self.downloads.get(&file_id)
    }

    /// The chat action currently being performed in the open chat (#87), if any —
    /// the "typing…" indicator drawn in the conversation header.
    #[must_use]
    pub fn chat_action(&self) -> Option<&ChatAction> {
        self.chat_action.as_ref()
    }

    /// Set (or clear) the open chat's chat action: `Some` shows the header
    /// indicator, `None` removes it (a cancel). The seam Phase 6 fills from the core
    /// [`ChatActionStore`](tuigram_core::ChatActionStore) on each `updateChatAction`;
    /// until then only tests call it.
    #[allow(dead_code)]
    pub fn set_chat_action(&mut self, action: Option<ChatAction>) {
        self.chat_action = action;
    }

    /// Replace the download state of the open chat's media files, keyed by id (#120).
    /// The loop projects this from the core
    /// [`FileStore`](tuigram_core::files::FileStore) on each `updateFile` (and on a
    /// history refresh), reading back the files the visible messages reference. A
    /// wholesale replace is correct because the store carries the newest full record
    /// for every file, so an advanced or completed transfer overwrites the last
    /// snapshot and the progress line reflects it; a switch to a chat whose media has
    /// not been folded yet clears to empty until its files arrive.
    pub fn set_downloads(&mut self, files: Vec<File>) {
        self.downloads = files.into_iter().map(|file| (file.id, file)).collect();
    }

    /// Scroll one row toward the newest (#222): advances within the current
    /// message's own rows, rolling onto the next message once they are
    /// exhausted. Clamps at the bottom-anchored position
    /// ([`newest_anchor`](Self::newest_anchor)) rather than allowing an
    /// overscroll past it — a real pager's "you're at the end." A no-op on an
    /// empty history.
    pub fn scroll_down(&mut self) {
        if self.messages.is_empty() {
            return;
        }
        let (anchor_offset, anchor_skip) = self.newest_anchor();
        let at_or_past_anchor = self.offset > anchor_offset
            || (self.offset == anchor_offset && self.row_skip >= anchor_skip);
        if at_or_past_anchor {
            return;
        }
        let height = self.message_height(&self.messages[self.offset]);
        if self.row_skip + 1 < height {
            self.row_skip += 1;
        } else if self.offset + 1 < self.messages.len() {
            self.offset += 1;
            self.row_skip = 0;
        }
    }

    /// Scroll one row toward the oldest (#222): retreats within the current
    /// message's own rows, rolling onto the previous message's trailing row
    /// (its blank separator) once exhausted. Clamps at the very top.
    pub fn scroll_up(&mut self) {
        if self.row_skip > 0 {
            self.row_skip -= 1;
        } else if self.offset > 0 {
            self.offset -= 1;
            self.row_skip = self
                .message_height(&self.messages[self.offset])
                .saturating_sub(1);
        }
    }

    /// Scroll a full viewport toward the newest (#222) — the `PageDown`
    /// action, meaningfully bigger than [`scroll_down`](Self::scroll_down)'s
    /// single-row step now that the distinction is possible. Reuses the same
    /// row-step (and its anchor clamp) rather than duplicating the stepping
    /// logic; falls back to a single row before the first render measures a
    /// viewport.
    pub fn page_down(&mut self) {
        for _ in 0..self.viewport.max(1) {
            self.scroll_down();
        }
    }

    /// Scroll a full viewport toward the oldest (#222) — the `PageUp` action.
    pub fn page_up(&mut self) {
        for _ in 0..self.viewport.max(1) {
            self.scroll_up();
        }
    }

    /// Move the cursor to message `message_id` if it is in the loaded history,
    /// returning whether it was found. Used to land on a search hit the user opened
    /// (#117): a hit already on the loaded (newest) page scrolls to it; one that has
    /// not been paged in is a no-op (`false`), leaving the view where it was — the
    /// caller stays at the newest page rather than chasing an unloaded message.
    pub fn select_message(&mut self, message_id: i64) -> bool {
        match self.messages.iter().position(|m| m.id == message_id) {
            Some(index) => {
                self.offset = index;
                self.row_skip = 0;
                true
            }
            None => false,
        }
    }

    /// Jump to the bottom-anchored newest position (#158) — the `G` / `End` action.
    /// The cursor lands on the topmost message of the last screenful (consistent with
    /// the "message at offset" cursor); repeated `k` then walks upward from there.
    pub fn jump_to_newest(&mut self) {
        (self.offset, self.row_skip) = self.newest_anchor();
    }

    /// Whether the view is pinned to the newest message — sitting exactly at the
    /// bottom-anchored position (#159). This is *derived*, not a toggled mode: opening
    /// a chat and `G` land here, and any scroll away leaves it. A same-chat refresh
    /// reads it to decide whether to follow the tail or hold the reader's place.
    #[must_use]
    pub fn is_at_newest(&self) -> bool {
        (self.offset, self.row_skip) == self.newest_anchor()
    }

    /// Record the history pane's inner height (rows) measured by the last render
    /// (#158). When the height changes while the view is pinned to the newest
    /// message, re-anchor so a resize keeps the newest on screen. Returns whether the
    /// offset moved, so the caller can repaint the corrected frame.
    pub fn set_viewport_height(&mut self, height: usize) -> bool {
        if height == self.viewport {
            return false;
        }
        let following = self.is_at_newest();
        self.viewport = height;
        if following {
            let previous = (self.offset, self.row_skip);
            (self.offset, self.row_skip) = self.newest_anchor();
            return (self.offset, self.row_skip) != previous;
        }
        false
    }

    /// Seed the terminal's graphics-protocol capability (#208), so
    /// [`message_height`](Self::message_height) knows whether to reserve rows
    /// for inline media. `App::set_avatar_support` calls this once, the same
    /// moment it seeds `AvatarSupport` itself — this never changes again
    /// within a run today (pre-#209's live toggle), so unlike
    /// [`set_viewport_height`](Self::set_viewport_height) there is no
    /// re-anchoring to do here.
    pub fn set_graphics_capable(&mut self, capable: bool) {
        self.graphics_capable = capable;
    }

    /// The `(message index, row skip)` that pins the newest message to the
    /// bottom of the viewport (#158, row-granular since #222): walk back from
    /// the last message summing [`message_height`](Self::message_height); the
    /// first message that would overflow the remaining space is included
    /// *partially* — skip exactly its excess rows from the top — so the
    /// newest message's last row lands exactly on the viewport's bottom row,
    /// rather than the pre-#222 whole-message-only anchor (which excluded an
    /// overflowing message entirely, leaving a gap above it). Returns `(0, 0)`
    /// on an empty history, or one that fits the viewport with room to spare;
    /// `(last, 0)` before the first render measures a viewport; and
    /// `(last, height - viewport)` when even the newest message alone
    /// overflows — best effort, showing its tail rather than its head.
    fn newest_anchor(&self) -> (usize, usize) {
        let Some(last) = self.messages.len().checked_sub(1) else {
            return (0, 0);
        };
        if self.viewport == 0 {
            return (last, 0);
        }
        let mut used = 0;
        for index in (0..=last).rev() {
            let height = self.message_height(&self.messages[index]);
            if used + height >= self.viewport {
                return (index, used + height - self.viewport);
            }
            used += height;
        }
        (0, 0)
    }

    /// The number of terminal rows one message occupies in the history pane — the
    /// same count [`crate::ui::message_lines`] renders, since that pane does not wrap
    /// (each `Line` is one row, so the height is width-independent). A drift-guard
    /// test in `ui.rs` keeps this in lockstep with the renderer.
    pub(crate) fn message_height(&self, message: &Message) -> usize {
        // A bold header, the body, an optional inline-media box, an optional
        // download-progress line, an optional reaction line, and a blank separator
        // below.
        1 + content_rows(&message.content)
            + self.media_rows(&message.content)
            + usize::from(self.has_download_line(&message.content))
            + usize::from(!message.reactions.is_empty())
            + 1
    }

    /// Whether a message's content draws a download-progress line — mirroring
    /// [`crate::ui::download_line`]: the file is known and either actively
    /// downloading or already present.
    fn has_download_line(&self, content: &MessageContent) -> bool {
        content
            .file()
            .and_then(|file| self.downloads.get(&file.id))
            .is_some_and(|file| file.is_downloading_active || file.is_present())
    }

    /// The rows an inline-media box adds below a message's placeholder/caption
    /// lines (#208): [`MEDIA_ROWS`] when the terminal is graphics-capable and the
    /// content is media whose bytes are already available, `0` otherwise — the
    /// same fixed-height reservation regardless of the source image's real aspect
    /// ratio, mirroring the avatar gutter's fixed 2 rows. Additive to (never a
    /// replacement for) `content_rows`'s placeholder/caption lines, so a pending,
    /// failed, or non-graphics render is byte-identical to before #208.
    fn media_rows(&self, content: &MessageContent) -> usize {
        if self.graphics_capable && media_ready(content, &self.downloads) {
            MEDIA_ROWS
        } else {
            0
        }
    }
}

/// The fixed row height of an inline-media box (#208) — photos, static
/// stickers, and video/animation stills are all scaled to fit this box
/// regardless of their real aspect ratio, so height math never depends on an
/// async decode's result, only on whether it has started.
///
/// Sized (with [`MEDIA_COLS`]) to read as an actual photo rather than an
/// icon: at a typical terminal cell aspect (~1:2 width:height in pixels), 16
/// rows × 48 cols works out to roughly a 3:2 landscape photo — double each
/// dimension of this box's original 8×24 (four times the pixel area), which
/// in practice looked avatar-bubble-sized rather than photo-sized.
pub(crate) const MEDIA_ROWS: usize = 16;

/// The inline-media box's column width (#208), a fixed target `drive_media`
/// (`main.rs`) encodes into and the render path draws into (clamped further
/// to the pane's actual inner width there) — the same "fixed box, not the
/// image's real aspect" reasoning as [`MEDIA_ROWS`], which also documents
/// this size's derivation.
pub(crate) const MEDIA_COLS: usize = 48;

/// Whether a message's content has raster bytes ready to render inline
/// (#208), mirrored independently by [`crate::ui::media_ready`] (kept as two
/// separate implementations, guarded by a drift-guard test, the same
/// convention `content_rows`/`content_lines` already follow):
/// - `Photo` and a static `Sticker` are ready once the backing file the
///   existing download driver already fetches is present.
/// - `Video` and `Animation` are ready as soon as they carry a minithumbnail —
///   embedded with the message, no download needed.
/// - Everything else (animated stickers included — TDLib gives those no
///   minithumbnail, and rendering their `thumbnail` would need a new
///   download-trigger path out of scope for #208) is never ready.
pub(crate) fn media_ready(content: &MessageContent, downloads: &HashMap<i32, File>) -> bool {
    let file_present = |file_id: i32| downloads.get(&file_id).is_some_and(File::is_present);
    match content {
        MessageContent::Photo(p) => file_present(p.file.id),
        MessageContent::Sticker(s) => s.is_static && file_present(s.file.id),
        MessageContent::Video(v) => v.minithumbnail.is_some(),
        MessageContent::Animation(a) => a.minithumbnail.is_some(),
        _ => false,
    }
}

/// The row count of a message body, mirroring [`crate::ui::content_lines`]: text
/// bodies and captions keep their own line breaks (an empty text still takes one
/// line), and media placeholders add a single label line above any caption.
fn content_rows(content: &MessageContent) -> usize {
    fn text_rows(text: &FormattedText) -> usize {
        text.text.split('\n').count()
    }
    fn caption_rows(caption: &FormattedText) -> usize {
        if caption.text.is_empty() {
            0
        } else {
            text_rows(caption)
        }
    }
    match content {
        MessageContent::Text(text) => text_rows(text),
        MessageContent::Photo(p) => 1 + caption_rows(&p.caption),
        MessageContent::Video(v) => 1 + caption_rows(&v.caption),
        MessageContent::Document(d) => 1 + caption_rows(&d.caption),
        MessageContent::Audio(a) => 1 + caption_rows(&a.caption),
        MessageContent::Voice(v) => 1 + caption_rows(&v.caption),
        MessageContent::Animation(a) => 1 + caption_rows(&a.caption),
        MessageContent::Sticker(_)
        | MessageContent::Location(_)
        | MessageContent::Venue(_)
        | MessageContent::Contact(_)
        | MessageContent::Poll(_)
        | MessageContent::Unsupported(_) => 1,
    }
}

/// A sender's resolved header text plus the accent color to tint it with
/// (#194). `color` is `None` for senders that get no accent tint — "You", a
/// chat (channel/anonymous-admin post), or an unresolved fallback id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SenderLabel {
    pub(crate) label: String,
    pub(crate) color: Option<Color>,
}

/// The display label for a user sender (#160, #194): `"Name (@handle)"` when the
/// user has both a name and a primary username, `"Name"` when only a name,
/// `"@handle"` when only a username, and otherwise core's [`User::display_name`]
/// fallback ("Deleted Account" or the bare `User {id}`), tinted with the user's
/// accent color. The loop resolves each history sender through this before
/// handing the labels to [`ConversationView::project`].
#[must_use]
pub(crate) fn sender_label_for(user: &User) -> SenderLabel {
    let name = format!("{} {}", user.first_name, user.last_name);
    let name = name.trim();
    let label = match (name.is_empty(), user.username()) {
        (false, Some(handle)) => format!("{name} (@{handle})"),
        (false, None) => name.to_owned(),
        (true, Some(handle)) => format!("@{handle}"),
        (true, None) => user.display_name(),
    };
    SenderLabel {
        label,
        color: Some(accent_color(user.accent_color_id, user.id)),
    }
}

/// The 7 fixed Telegram peer colors (red/orange/violet/green/cyan/blue/pink),
/// approximated onto ratatui's named ANSI colors — there is no exact
/// Orange/Violet/Pink variant — so the header tint follows whatever palette the
/// user's terminal theme maps these names to, rather than a fixed hex value.
const ACCENT_PALETTE: [Color; 7] = [
    Color::Red,
    Color::Yellow,
    Color::Magenta,
    Color::Green,
    Color::Cyan,
    Color::Blue,
    Color::LightMagenta,
];

/// A sender's accent color (#194): a built-in `accent_color_id` (`0..=6`) maps
/// directly onto [`ACCENT_PALETTE`]; a custom Premium id (`>=7`) or an
/// out-of-range/negative id falls back to a deterministic hash of the user id,
/// so a user without a chosen accent still always gets one stable color.
///
/// `pub(crate)` (not module-private) so the generated fallback-avatar bubble
/// (#201, Stage 4) tints itself with the same palette/hash mapping as the
/// header, rather than reimplementing it.
pub(crate) fn accent_color(accent_color_id: i32, user_id: i64) -> Color {
    let index = usize::try_from(accent_color_id)
        .ok()
        .filter(|&id| id < ACCENT_PALETTE.len())
        .unwrap_or_else(|| (hash_user_id(user_id) % ACCENT_PALETTE.len() as u64) as usize);
    ACCENT_PALETTE[index]
}

/// A splitmix64 finalizer mix on the user id — deterministic across runs
/// (unlike `HashMap`'s randomized `RandomState`), so the same user always
/// falls back to the same accent color.
fn hash_user_id(id: i64) -> u64 {
    let mut x = id as u64;
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

#[cfg(test)]
pub(crate) use tests::sample_message;

#[cfg(test)]
mod tests {
    use super::*;
    use tuigram_core::model::{
        Animation, FileRef, FormattedText, Message, MessageContent, Photo, Presence, SendState,
        Sender, Sticker, UserKind, Video,
    };

    /// A minimal incoming [`Message`] for view tests: an id and content, every
    /// other field inert. Tests that need a timestamp, reactions, an outgoing
    /// flag, or a pinned state set them on the returned value.
    pub(crate) fn sample_message(id: i64, content: MessageContent) -> Message {
        Message {
            id,
            chat_id: 1,
            sender: Sender::User(id),
            date: 0,
            edit_date: 0,
            is_outgoing: false,
            content,
            send_state: SendState::Sent,
            reactions: Vec::new(),
        }
    }

    fn text(id: i64, body: &str) -> Message {
        sample_message(
            id,
            MessageContent::Text(FormattedText {
                text: body.to_owned(),
                entities: Vec::new(),
            }),
        )
    }

    fn history(n: i64) -> ConversationView {
        let messages = (0..n).map(|i| text(i, &format!("m{i}"))).collect();
        ConversationView::from_messages(messages, HashSet::new())
    }

    #[test]
    fn default_is_empty() {
        let view = ConversationView::default();
        assert!(view.is_empty());
        assert_eq!(view.len(), 0);
        assert_eq!(view.offset(), 0);
    }

    #[test]
    fn from_messages_keeps_order_and_starts_at_the_top() {
        let view = history(3);
        assert_eq!(view.len(), 3);
        assert_eq!(view.offset(), 0);
        assert_eq!(view.messages()[0].id, 0);
        assert_eq!(view.messages()[2].id, 2);
    }

    #[test]
    fn select_message_moves_the_cursor_to_a_loaded_id() {
        let mut view = history(4);
        assert!(view.select_message(2));
        assert_eq!(view.offset(), 2);
        assert_eq!(view.selected_message().map(|m| m.id), Some(2));
    }

    #[test]
    fn select_message_for_an_unloaded_id_is_a_noop() {
        let mut view = history(4);
        view.select_message(1);
        assert_eq!(view.offset(), 1);
        // Id 99 is not in the loaded history: the cursor stays put.
        assert!(!view.select_message(99));
        assert_eq!(view.offset(), 1);
    }

    #[test]
    fn scroll_down_steps_one_row_rolling_onto_the_next_message_once_exhausted() {
        // #222: scroll_down is now a row step, not a message step.
        // `history`'s plain-text messages are 3 rows each (header, body, blank
        // separator) with no viewport measured, so 2 steps stay within
        // message 0's own rows and the 3rd rolls onto message 1.
        let mut view = history(3);
        view.scroll_down();
        view.scroll_down();
        assert_eq!(view.offset(), 0, "still within message 0's own 3 rows");
        view.scroll_down();
        assert_eq!(
            view.offset(),
            1,
            "message 0's rows exhausted, rolls onto message 1"
        );
    }

    #[test]
    fn scroll_down_clamps_at_the_bottom_anchored_position() {
        // #222: the row-granular clamp is the bottom anchor itself (a real
        // pager's "you're at the end"), not the old raw `min(len - 1)`.
        // Viewport fits two 3-row messages; the anchor is offset 3, row 0
        // (verified in `opening_a_chat_lands_bottom_anchored_at_the_newest_message`).
        let mut view = view_fitting(2);
        view.project(
            10,
            (1..=5).map(|i| text(i, "m")).collect(),
            HashSet::new(),
            HashMap::new(),
            i64::MAX,
            0,
            true,
        );
        // More steps than rows in the whole history: scrolling down this much
        // must still land exactly on the anchor, not past it.
        for _ in 0..100 {
            view.scroll_down();
        }
        assert!(
            view.is_at_newest(),
            "repeated scrolling down stops exactly at the bottom anchor"
        );
    }

    #[test]
    fn scroll_down_moves_one_row_while_page_down_moves_a_full_viewport() {
        // #222: PageDown must be meaningfully bigger than a single j/k step.
        let mut single = view_fitting(2); // 6-row viewport
        single.project(
            10,
            (1..=5).map(|i| text(i, "m")).collect(),
            HashSet::new(),
            HashMap::new(),
            i64::MAX,
            0,
            true,
        );
        for _ in 0..100 {
            single.scroll_up();
        }
        single.scroll_down();
        assert_eq!(
            single.offset(),
            0,
            "one row-step barely moves within message 0's own 3 rows"
        );

        let mut paged = view_fitting(2);
        paged.project(
            10,
            (1..=5).map(|i| text(i, "m")).collect(),
            HashSet::new(),
            HashMap::new(),
            i64::MAX,
            0,
            true,
        );
        for _ in 0..100 {
            paged.scroll_up();
        }
        paged.page_down();
        assert_eq!(
            paged.offset(),
            2,
            "PageDown moves a full 6-row viewport (two 3-row messages) at once"
        );
    }

    #[test]
    fn page_up_moves_a_full_viewport_back_toward_the_oldest() {
        let mut view = view_fitting(2);
        view.project(
            10,
            (1..=5).map(|i| text(i, "m")).collect(),
            HashSet::new(),
            HashMap::new(),
            i64::MAX,
            0,
            true,
        );
        assert!(view.is_at_newest());
        view.page_up();
        assert_eq!(
            view.offset(),
            1,
            "a full 6-row viewport back from the anchor at offset 3"
        );
    }

    #[test]
    fn scrolling_through_a_media_message_advances_one_row_at_a_time() {
        // #222: this is the bug the issue fixes — before, crossing a media
        // message jumped the whole way in a single scroll step; now it takes
        // one row-step per row, same as any other message.
        let mut view =
            ConversationView::from_messages(vec![photo(1, 42), text(2, "after")], HashSet::new());
        view.set_graphics_capable(true);
        view.set_downloads(vec![present_file(42)]);
        let height = view.message_height(&view.messages()[0].clone());
        assert!(
            height > 10,
            "a ready photo is much taller than a plain-text message"
        );

        for step in 1..height {
            view.scroll_down();
            assert_eq!(
                view.offset(),
                0,
                "still within the photo message's own {height} rows at step {step}"
            );
        }
        view.scroll_down();
        assert_eq!(
            view.offset(),
            1,
            "the photo's rows are exhausted, rolls onto the next message"
        );
    }

    #[test]
    fn scroll_up_clamps_at_the_top() {
        let mut view = history(3);
        view.scroll_down();
        view.scroll_up();
        view.scroll_up();
        assert_eq!(view.offset(), 0);
    }

    #[test]
    fn scrolling_an_empty_history_is_a_noop() {
        let mut view = ConversationView::default();
        view.scroll_down();
        assert_eq!(view.offset(), 0);
    }

    #[test]
    fn pinned_ids_are_reported() {
        let view = ConversationView::from_messages(vec![text(7, "hi")], HashSet::from([7]));
        assert!(view.is_pinned(7));
        assert!(!view.is_pinned(8));
    }

    #[test]
    fn the_selected_message_is_the_one_at_the_offset() {
        let mut view = history(3);
        assert_eq!(view.selected_message().map(|m| m.id), Some(0), "top first");
        // #222: 3 row-steps to exhaust message 0's own 3 rows and roll onto message 1.
        for _ in 0..3 {
            view.scroll_down();
        }
        assert_eq!(view.selected_message().map(|m| m.id), Some(1));
        assert_eq!(ConversationView::default().selected_message(), None);
    }

    #[test]
    fn toggling_a_pin_flips_membership_both_ways() {
        let mut view = ConversationView::from_messages(vec![text(7, "hi")], HashSet::new());
        assert!(!view.is_pinned(7));
        view.toggle_pin(7);
        assert!(view.is_pinned(7), "pinned");
        view.toggle_pin(7);
        assert!(!view.is_pinned(7), "unpinned again");
    }

    #[test]
    fn toggling_a_reaction_adds_then_removes_our_choice() {
        let mut view = ConversationView::from_messages(vec![text(1, "nice")], HashSet::new());
        view.toggle_reaction(1, "👍");
        let reactions = &view.messages()[0].reactions;
        assert_eq!(reactions.len(), 1);
        assert_eq!(reactions[0].kind, ReactionKind::Emoji("👍".to_owned()));
        assert_eq!(reactions[0].count, 1);
        assert!(reactions[0].is_chosen);
        // Toggling the same emoji off drops the bucket (we were the only reactor).
        view.toggle_reaction(1, "👍");
        assert!(view.messages()[0].reactions.is_empty());
    }

    #[test]
    fn has_own_reaction_tracks_only_our_chosen_emoji() {
        let mut message = text(1, "nice");
        // Others reacted with 🔥, but not us.
        message.reactions = vec![Reaction {
            kind: ReactionKind::Emoji("🔥".to_owned()),
            count: 2,
            is_chosen: false,
        }];
        let mut view = ConversationView::from_messages(vec![message], HashSet::new());
        assert!(
            !view.has_own_reaction(1, "🔥"),
            "others' reaction is not ours"
        );
        assert!(!view.has_own_reaction(1, "👍"), "unreacted emoji");
        assert!(!view.has_own_reaction(99, "👍"), "unknown message");
        // After we add 👍 it reads as ours; 🔥 (still only others') does not.
        view.toggle_reaction(1, "👍");
        assert!(view.has_own_reaction(1, "👍"));
        assert!(!view.has_own_reaction(1, "🔥"));
    }

    #[test]
    fn toggling_a_reaction_others_already_have_keeps_the_bucket() {
        let mut message = text(1, "nice");
        message.reactions = vec![Reaction {
            kind: ReactionKind::Emoji("🔥".to_owned()),
            count: 2,
            is_chosen: false,
        }];
        let mut view = ConversationView::from_messages(vec![message], HashSet::new());
        view.toggle_reaction(1, "🔥");
        let bucket = &view.messages()[0].reactions[0];
        assert_eq!(bucket.count, 3, "our choice adds to the existing count");
        assert!(bucket.is_chosen);
        // Removing ours leaves the others' reaction behind.
        view.toggle_reaction(1, "🔥");
        let bucket = &view.messages()[0].reactions[0];
        assert_eq!(bucket.count, 2);
        assert!(!bucket.is_chosen);
    }

    /// A view with the viewport measured to hold exactly `messages` single-line
    /// text messages (each 3 rows: header, body, blank), for deterministic anchoring.
    fn view_fitting(messages: usize) -> ConversationView {
        let mut view = ConversationView::default();
        view.set_viewport_height(messages * 3);
        view
    }

    #[test]
    fn opening_a_chat_lands_bottom_anchored_at_the_newest_message() {
        // Viewport fits two 3-row messages; a five-message history opens with the
        // newest (5) at the bottom and message 4 at the top of the last screenful.
        let mut view = view_fitting(2);
        view.project(
            10,
            (1..=5).map(|i| text(i, "m")).collect(),
            HashSet::new(),
            HashMap::new(),
            i64::MAX,
            0,
            true,
        );
        assert_eq!(view.len(), 5);
        assert_eq!(view.offset(), 3, "top of the last screenful");
        assert_eq!(view.selected_message().map(|m| m.id), Some(4));
        assert!(view.is_at_newest(), "an open is pinned to the newest");
    }

    #[test]
    fn opening_a_chat_with_unread_messages_marks_the_first_one() {
        // #164: last_read_inbox = 2, so message 3 is the first unread incoming
        // message — the separator belongs immediately above it.
        let mut view = ConversationView::default();
        view.project(
            10,
            (1..=5).map(|i| text(i, "m")).collect(),
            HashSet::new(),
            HashMap::new(),
            2,
            0,
            true,
        );
        assert!(view.unread_separator_before(3));
        assert!(!view.unread_separator_before(1));
        assert!(
            !view.unread_separator_before(4),
            "only the first unread one"
        );
    }

    #[test]
    fn opening_a_fully_read_chat_sets_no_separator() {
        let mut view = ConversationView::default();
        view.project(
            10,
            (1..=5).map(|i| text(i, "m")).collect(),
            HashSet::new(),
            HashMap::new(),
            5,
            0,
            true,
        );
        for id in 1..=5 {
            assert!(!view.unread_separator_before(id));
        }
    }

    #[test]
    fn a_same_chat_refresh_never_recomputes_the_frozen_separator() {
        // The separator is computed once, on the different-chat branch of
        // `project`, from the inbox watermark *as of that open* (#164) — a
        // same-chat refresh must not recompute it even though the live watermark
        // moves on moments after open (the open-triggered mark-read).
        let mut view = ConversationView::default();
        view.project(
            10,
            (1..=5).map(|i| text(i, "m")).collect(),
            HashSet::new(),
            HashMap::new(),
            2,
            0,
            true,
        );
        assert!(view.unread_separator_before(3));
        // A same-chat refresh (fresh_open = false) arrives with the watermark
        // already advanced past everything (as mark-read-on-open would report) —
        // the separator must hold at message 3, not vanish or move.
        view.project(
            10,
            (1..=5).map(|i| text(i, "m")).collect(),
            HashSet::new(),
            HashMap::new(),
            5,
            0,
            false,
        );
        assert!(
            view.unread_separator_before(3),
            "frozen at open, not recomputed on refresh"
        );
    }

    #[test]
    fn reopening_the_same_chat_after_it_is_fully_read_clears_the_separator() {
        // A genuine re-open (fresh_open = true) of the *same* chat_id must still
        // re-resolve against the current watermark — unlike a live-update refresh,
        // it is not a mere continuation. Without this, closing and reopening a
        // chat that has since been fully read would show a stale rule forever,
        // since the same-chat branch alone never recomputes it.
        let mut view = ConversationView::default();
        view.project(
            10,
            (1..=5).map(|i| text(i, "m")).collect(),
            HashSet::new(),
            HashMap::new(),
            2,
            0,
            true,
        );
        assert!(view.unread_separator_before(3), "unread on first open");
        // Re-open the same chat; by now everything has been read.
        view.project(
            10,
            (1..=5).map(|i| text(i, "m")).collect(),
            HashSet::new(),
            HashMap::new(),
            5,
            0,
            true,
        );
        for id in 1..=5 {
            assert!(
                !view.unread_separator_before(id),
                "re-open re-resolved against the now-fully-read watermark"
            );
        }
    }

    #[test]
    fn a_landing_page_resolves_the_separator_left_pending_by_an_empty_open() {
        // The very first projection of a chat never before cached can fire with an
        // empty history — the async landing page merges moments later as a
        // same-chat refresh (fresh_open = false). The separator must stay pending
        // through that empty open and resolve once the real messages land, rather
        // than freezing "nothing unread" against the empty snapshot.
        let mut view = ConversationView::default();
        view.project(10, vec![], HashSet::new(), HashMap::new(), 2, 0, true);
        assert!(
            !view.unread_separator_before(3),
            "nothing to resolve against yet"
        );
        view.project(
            10,
            (1..=5).map(|i| text(i, "m")).collect(),
            HashSet::new(),
            HashMap::new(),
            2,
            0,
            false,
        );
        assert!(
            view.unread_separator_before(3),
            "the landing page resolves it once real messages are loaded"
        );
    }

    #[test]
    fn jump_to_newest_from_any_offset_lands_at_the_same_bottom_anchor() {
        let mut view = view_fitting(2);
        view.project(
            10,
            (1..=5).map(|i| text(i, "m")).collect(),
            HashSet::new(),
            HashMap::new(),
            i64::MAX,
            0,
            true,
        );
        let anchor = view.offset();
        // Scroll to the very top, then jump: it lands back on the identical anchor.
        for _ in 0..10 {
            view.scroll_up();
        }
        assert_eq!(view.offset(), 0);
        view.jump_to_newest();
        assert_eq!(view.offset(), anchor);
        assert!(view.is_at_newest());
    }

    #[test]
    fn a_history_shorter_than_the_viewport_anchors_at_the_top() {
        // Three 3-row messages (9 rows) in a 30-row viewport: everything fits, so the
        // anchor is the top with no blank gap to scroll past.
        let mut view = view_fitting(10);
        view.project(
            10,
            (1..=3).map(|i| text(i, "m")).collect(),
            HashSet::new(),
            HashMap::new(),
            i64::MAX,
            0,
            true,
        );
        assert_eq!(view.offset(), 0);
        assert!(view.is_at_newest());
    }

    #[test]
    fn a_message_taller_than_the_viewport_still_anchors_on_the_newest() {
        // The newest message alone overflows the viewport; anchoring keeps it on
        // screen (offset on it) rather than falling off the bottom.
        let mut view = view_fitting(1); // 3 rows
        let tall = text(2, "l1\nl2\nl3\nl4\nl5"); // 1 + 5 + 1 = 7 rows > 3
        view.project(
            10,
            vec![text(1, "m"), tall],
            HashSet::new(),
            HashMap::new(),
            i64::MAX,
            0,
            true,
        );
        assert_eq!(
            view.offset(),
            1,
            "newest stays anchored though it overflows"
        );
    }

    #[test]
    fn refreshing_the_same_chat_while_scrolled_up_keeps_the_selected_message() {
        // Four 3-row messages in a two-message viewport, so scrolling up genuinely
        // leaves the newest anchor (offset 2) onto message 2.
        let mut view = view_fitting(2);
        view.project(
            10,
            (1..=4).map(|i| text(i, "m")).collect(),
            HashSet::new(),
            HashMap::new(),
            i64::MAX,
            0,
            true,
        );
        view.scroll_up(); // off the newest anchor, onto message 2
        assert_eq!(view.selected_message().map(|m| m.id), Some(2));
        assert!(!view.is_at_newest(), "scrolled up: not following");

        // An older page is merged ahead of the loaded ones: 2 stays selected, its
        // index shifts down by the two prepended messages — reading is not interrupted.
        view.project(
            10,
            vec![
                text(90, "x"),
                text(91, "y"),
                text(1, "m"),
                text(2, "m"),
                text(3, "m"),
                text(4, "m"),
            ],
            HashSet::new(),
            HashMap::new(),
            i64::MAX,
            0,
            true,
        );
        assert_eq!(
            view.selected_message().map(|m| m.id),
            Some(2),
            "cursor follows the id"
        );
        assert_eq!(
            view.offset(),
            3,
            "index shifted by the two prepended messages"
        );
    }

    #[test]
    fn a_live_message_while_pinned_to_the_newest_follows_the_tail() {
        // Sitting at the bottom-anchored newest, a new message arrives (#159): the
        // view advances onto the new newest rather than holding still.
        let mut view = view_fitting(2);
        view.project(
            10,
            (1..=4).map(|i| text(i, "m")).collect(),
            HashSet::new(),
            HashMap::new(),
            i64::MAX,
            0,
            true,
        );
        assert!(view.is_at_newest());
        let before = view.offset();

        view.project(
            10,
            (1..=5).map(|i| text(i, "m")).collect(),
            HashSet::new(),
            HashMap::new(),
            i64::MAX,
            0,
            true,
        );
        assert!(view.offset() > before, "the anchor advanced with the tail");
        assert!(view.is_at_newest(), "still pinned to the (new) newest");
        // The newest message is now the last one loaded.
        assert_eq!(view.messages().last().map(|m| m.id), Some(5));
    }

    #[test]
    fn a_live_message_while_scrolled_up_does_not_move_the_cursor() {
        let mut view = view_fitting(2);
        view.project(
            10,
            (1..=4).map(|i| text(i, "m")).collect(),
            HashSet::new(),
            HashMap::new(),
            i64::MAX,
            0,
            true,
        );
        view.scroll_up();
        view.scroll_up();
        let (offset, selected) = (view.offset(), view.selected_message().map(|m| m.id));
        assert!(!view.is_at_newest(), "reading history: not following");

        // A newer message arrives; the reader is undisturbed.
        view.project(
            10,
            (1..=5).map(|i| text(i, "m")).collect(),
            HashSet::new(),
            HashMap::new(),
            i64::MAX,
            0,
            true,
        );
        assert_eq!(view.offset(), offset);
        assert_eq!(view.selected_message().map(|m| m.id), selected);
    }

    #[test]
    fn a_resize_while_following_re_anchors_to_the_newest() {
        let mut view = view_fitting(2);
        view.project(
            10,
            (1..=5).map(|i| text(i, "m")).collect(),
            HashSet::new(),
            HashMap::new(),
            i64::MAX,
            0,
            true,
        );
        let before = view.offset();
        // Growing the pane to fit three messages re-anchors upward; the offset moves.
        assert!(view.set_viewport_height(9), "re-anchored while following");
        assert!(
            view.offset() < before,
            "more messages now fit above the newest"
        );
        assert!(view.is_at_newest());
        // An unchanged height is a no-op that reports no move.
        assert!(!view.set_viewport_height(9));
    }

    #[test]
    fn a_resize_while_scrolled_up_leaves_the_cursor_put() {
        let mut view = view_fitting(2);
        view.project(
            10,
            (1..=5).map(|i| text(i, "m")).collect(),
            HashSet::new(),
            HashMap::new(),
            i64::MAX,
            0,
            true,
        );
        view.scroll_up();
        let offset = view.offset();
        // Not following: a resize records the height but does not move the cursor.
        assert!(!view.set_viewport_height(9));
        assert_eq!(view.offset(), offset);
    }

    #[test]
    fn switching_chats_resets_to_a_fresh_bottom_anchored_view() {
        let mut view = view_fitting(2);
        view.project(
            10,
            (1..=4).map(|i| text(i, "m")).collect(),
            HashSet::from([1]),
            HashMap::new(),
            i64::MAX,
            0,
            true,
        );
        view.scroll_up();
        assert!(!view.is_at_newest());

        // A different chat replaces everything — messages, cursor, pinned set — and
        // opens bottom-anchored at its newest message.
        view.project(
            20,
            (7..=9).map(|i| text(i, "z")).collect(),
            HashSet::new(),
            HashMap::new(),
            i64::MAX,
            0,
            true,
        );
        assert_eq!(view.len(), 3);
        assert!(view.is_at_newest(), "new chat opens pinned to the newest");
        assert_eq!(view.messages().last().map(|m| m.id), Some(9));
        assert!(!view.is_pinned(1), "the previous chat's pins are gone");
    }

    #[test]
    fn graphics_capability_survives_a_chat_switch() {
        // Like the measured viewport, this is a terminal-level fact, not per-chat
        // state — a chat switch must not silently fall back to its startup default.
        let mut view = view_fitting(2);
        view.set_graphics_capable(true);
        view.project(
            20,
            (7..=9).map(|i| text(i, "z")).collect(),
            HashSet::new(),
            HashMap::new(),
            i64::MAX,
            0,
            true,
        );
        view.set_downloads(vec![present_file(42)]);
        let with_media = photo(1, 42);
        assert_eq!(view.media_rows(&with_media.content), MEDIA_ROWS);
    }

    #[test]
    fn a_chat_action_is_recorded_then_cleared() {
        let mut view = ConversationView::default();
        assert!(view.chat_action().is_none(), "no one acting by default");
        view.set_chat_action(Some(ChatAction::Typing));
        assert_eq!(view.chat_action(), Some(&ChatAction::Typing));
        // A cancel clears the indicator.
        view.set_chat_action(None);
        assert!(view.chat_action().is_none());
    }

    #[test]
    fn projected_downloads_are_read_back_by_file_id_and_replace_wholesale() {
        let mut view = ConversationView::default();
        assert!(view.download(42).is_none());
        view.set_downloads(vec![
            File {
                id: 42,
                size: 100,
                downloaded_size: 45,
                is_downloading_active: true,
                ..File::default()
            },
            File {
                id: 7,
                size: 100,
                downloaded_size: 100,
                is_downloading_completed: true,
                local_path: "/tmp/7".to_owned(),
                ..File::default()
            },
        ]);
        let file = view.download(42).expect("recorded download");
        assert_eq!(file.downloaded_size, 45);
        assert!(file.is_downloading_active);
        assert!(view.download(7).expect("second file").is_present());

        // A fresh projection replaces the map wholesale: the old ids are gone.
        view.set_downloads(vec![File {
            id: 99,
            ..File::default()
        }]);
        assert!(view.download(42).is_none(), "prior snapshot cleared");
        assert!(view.download(99).is_some());
    }

    fn photo(id: i64, file_id: i32) -> Message {
        sample_message(
            id,
            MessageContent::Photo(Photo {
                caption: FormattedText::default(),
                file: FileRef::new(file_id),
                width: 100,
                height: 100,
            }),
        )
    }

    fn static_sticker(id: i64, file_id: i32) -> Message {
        sample_message(
            id,
            MessageContent::Sticker(Sticker {
                file: FileRef::new(file_id),
                width: 100,
                height: 100,
                emoji: "😀".to_owned(),
                is_static: true,
            }),
        )
    }

    fn animated_sticker(id: i64, file_id: i32) -> Message {
        sample_message(
            id,
            MessageContent::Sticker(Sticker {
                file: FileRef::new(file_id),
                width: 100,
                height: 100,
                emoji: "😀".to_owned(),
                is_static: false,
            }),
        )
    }

    fn video_with_minithumbnail(id: i64, file_id: i32, minithumbnail: Option<Vec<u8>>) -> Message {
        sample_message(
            id,
            MessageContent::Video(Video {
                caption: FormattedText::default(),
                file: FileRef::new(file_id),
                width: 100,
                height: 100,
                duration: 5,
                file_name: String::new(),
                mime_type: "video/mp4".to_owned(),
                minithumbnail,
            }),
        )
    }

    fn animation_with_minithumbnail(
        id: i64,
        file_id: i32,
        minithumbnail: Option<Vec<u8>>,
    ) -> Message {
        sample_message(
            id,
            MessageContent::Animation(Animation {
                caption: FormattedText::default(),
                file: FileRef::new(file_id),
                width: 100,
                height: 100,
                duration: 5,
                file_name: String::new(),
                mime_type: "video/mp4".to_owned(),
                minithumbnail,
            }),
        )
    }

    fn present_file(id: i32) -> File {
        File {
            id,
            size: 10,
            downloaded_size: 10,
            is_downloading_completed: true,
            local_path: format!("/tmp/{id}"),
            ..File::default()
        }
    }

    #[test]
    fn media_rows_are_reserved_only_when_graphics_capable_and_ready() {
        let mut view = ConversationView::from_messages(vec![photo(1, 42)], HashSet::new());
        let message = &view.messages()[0].clone();

        // Not graphics-capable, file present: no media rows.
        view.set_downloads(vec![present_file(42)]);
        assert_eq!(view.media_rows(&message.content), 0);

        // Graphics-capable, file not yet present: still no media rows.
        view.set_graphics_capable(true);
        view.set_downloads(vec![]);
        assert_eq!(view.media_rows(&message.content), 0);

        // Graphics-capable and the file is present: the fixed media box.
        view.set_downloads(vec![present_file(42)]);
        assert_eq!(view.media_rows(&message.content), MEDIA_ROWS);
    }

    #[test]
    fn static_stickers_are_ready_once_present_animated_ones_never_are() {
        let mut view = ConversationView::from_messages(
            vec![static_sticker(1, 1), animated_sticker(2, 2)],
            HashSet::new(),
        );
        view.set_graphics_capable(true);
        view.set_downloads(vec![present_file(1), present_file(2)]);
        assert_eq!(
            view.media_rows(&view.messages()[0].content.clone()),
            MEDIA_ROWS
        );
        assert_eq!(
            view.media_rows(&view.messages()[1].content.clone()),
            0,
            "an animated sticker has no minithumbnail and is out of #208's scope"
        );
    }

    #[test]
    fn video_and_animation_stills_need_no_download_just_a_minithumbnail() {
        let raw = Some(b"jpeg bytes".to_vec());
        let mut view = ConversationView::from_messages(
            vec![
                video_with_minithumbnail(1, 1, raw.clone()),
                animation_with_minithumbnail(2, 2, None),
            ],
            HashSet::new(),
        );
        view.set_graphics_capable(true);
        // No downloads projected at all — these never need one.
        assert_eq!(
            view.media_rows(&view.messages()[0].content.clone()),
            MEDIA_ROWS,
            "a video with a minithumbnail is ready with no download"
        );
        assert_eq!(
            view.media_rows(&view.messages()[1].content.clone()),
            0,
            "an animation with no minithumbnail stays a placeholder"
        );
    }

    #[test]
    fn message_height_grows_by_media_rows_only_while_ready() {
        // The file is present from the start, so the pre-existing "✓ saved"
        // download line's own contribution to the height stays constant across
        // the toggle below — isolating the height delta to the media box alone.
        let mut view = ConversationView::from_messages(vec![photo(1, 42)], HashSet::new());
        view.set_downloads(vec![present_file(42)]);
        let message = view.messages()[0].clone();
        let before = view.message_height(&message);

        view.set_graphics_capable(true);
        let after = view.message_height(&message);

        assert_eq!(after, before + MEDIA_ROWS);
    }

    /// A [`User`] with the given name and usernames; every other field inert. `kind`
    /// stays `Regular` so the empty-name fallback is `User {id}`, not "Deleted".
    /// `accent_color_id` is `-1` (out of the built-in `0..=6` range) so callers
    /// that don't care about color land on the deterministic hash fallback.
    fn user(id: i64, first: &str, last: &str, handles: &[&str]) -> User {
        User {
            id,
            first_name: first.to_owned(),
            last_name: last.to_owned(),
            usernames: handles.iter().map(|h| (*h).to_owned()).collect(),
            phone_number: None,
            is_contact: false,
            kind: UserKind::Regular,
            status: Presence::Never,
            accent_color_id: -1,
            avatar_minithumbnail: None,
        }
    }

    #[test]
    fn sender_label_for_joins_the_name_and_handle() {
        assert_eq!(
            sender_label_for(&user(7, "Ada", "Lovelace", &["ada"])).label,
            "Ada Lovelace (@ada)"
        );
    }

    #[test]
    fn sender_label_for_uses_the_name_alone_without_a_handle() {
        assert_eq!(
            sender_label_for(&user(7, "Ada", "Lovelace", &[])).label,
            "Ada Lovelace"
        );
    }

    #[test]
    fn sender_label_for_falls_back_to_the_handle_when_the_name_is_empty() {
        assert_eq!(sender_label_for(&user(7, "", "", &["ada"])).label, "@ada");
    }

    #[test]
    fn sender_label_for_falls_back_to_the_bare_id_when_nothing_is_known() {
        // No name, no handle, and a regular (non-deleted) account: core's
        // `display_name` bottoms out at `User {id}`.
        assert_eq!(sender_label_for(&user(7, "", "", &[])).label, "User 7");
    }

    #[test]
    fn sender_label_for_carries_the_users_accent_color() {
        // A resolved user is always tinted, matching `accent_color`'s own mapping.
        let with_id = user(7, "Ada", "Lovelace", &["ada"]);
        assert_eq!(
            sender_label_for(&with_id).color,
            Some(accent_color(with_id.accent_color_id, with_id.id))
        );
    }

    #[test]
    fn accent_color_maps_builtin_ids_onto_the_palette() {
        for id in 0..7 {
            assert_eq!(accent_color(id, 0), ACCENT_PALETTE[id as usize]);
        }
    }

    #[test]
    fn accent_color_falls_back_deterministically_for_ids_outside_the_palette() {
        // Any out-of-range id (negative, or >= the palette length) hashes the same
        // way for a given user — the exact id past the built-in range is inert.
        assert_eq!(accent_color(-1, 42), accent_color(999, 42));
        assert_eq!(accent_color(7, 42), accent_color(-1, 42));
        // Different users generally land on different fallback colors (a sanity
        // check, not a collision proof).
        assert_ne!(accent_color(-1, 1), accent_color(-1, 2));
    }

    #[test]
    fn sender_label_resolves_a_known_user_from_the_projected_map() {
        let mut view = ConversationView::default();
        let message = text(1, "hi"); // sample_message sets sender = User(1)
        view.project(
            10,
            vec![message.clone()],
            HashSet::new(),
            HashMap::from([(
                Sender::User(1),
                SenderLabel {
                    label: "Ada Lovelace (@ada)".to_owned(),
                    color: Some(Color::Red),
                },
            )]),
            i64::MAX,
            0,
            true,
        );
        assert_eq!(view.sender_label(&message).label, "Ada Lovelace (@ada)");
    }

    #[test]
    fn sender_label_falls_back_to_the_id_for_an_unresolved_user() {
        let mut view = ConversationView::default();
        let message = text(1, "hi");
        view.project(
            10,
            vec![message.clone()],
            HashSet::new(),
            HashMap::new(),
            i64::MAX,
            0,
            true,
        );
        assert_eq!(view.sender_label(&message).label, "User 1");
    }

    #[test]
    fn sender_label_resolves_a_chat_sender_to_its_title() {
        let mut view = ConversationView::default();
        let mut message = text(1, "post");
        message.sender = Sender::Chat(-100);
        view.project(
            10,
            vec![message.clone()],
            HashSet::new(),
            HashMap::from([(
                Sender::Chat(-100),
                SenderLabel {
                    label: "Rust News".to_owned(),
                    color: None,
                },
            )]),
            i64::MAX,
            0,
            true,
        );
        assert_eq!(view.sender_label(&message).label, "Rust News");
    }

    #[test]
    fn sender_label_reads_you_for_an_outgoing_message() {
        let mut view = ConversationView::default();
        let mut message = text(1, "mine");
        message.is_outgoing = true;
        // Even with a name in the map, our own messages read "You".
        view.project(
            10,
            vec![message.clone()],
            HashSet::new(),
            HashMap::from([(
                Sender::User(1),
                SenderLabel {
                    label: "Ada Lovelace (@ada)".to_owned(),
                    color: Some(Color::Red),
                },
            )]),
            i64::MAX,
            0,
            true,
        );
        assert_eq!(view.sender_label(&message).label, "You");
    }
}
