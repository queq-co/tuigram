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

use tuigram_core::model::{ChatAction, File, Message, Reaction, ReactionKind};

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
    /// Download state of media files referenced by the messages, keyed by TDLib
    /// file id, for the download-progress indicator (#85). Phase 6 projects this
    /// from the core [`FileStore`](tuigram_core::files::FileStore); empty until then.
    downloads: HashMap<i32, File>,
    /// The transient chat action in the open chat (#87) — the "typing…" indicator
    /// shown in the conversation header. `None` when no one is acting. Phase 6
    /// projects this from the core [`ChatActionStore`](tuigram_core::ChatActionStore);
    /// it is never part of the message history.
    chat_action: Option<ChatAction>,
}

impl ConversationView {
    /// Build a view from the open chat's history (oldest first) and its set of
    /// pinned message ids, scrolled to the top.
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
            downloads: HashMap::new(),
            chat_action: None,
        }
    }

    /// Re-project the open chat's history from the core
    /// [`MessageStore`](tuigram_core::messages::MessageStore) (#114). The loop reads
    /// `chat_id`'s messages (oldest first) and pinned ids back from the `Client` and
    /// hands the owned snapshot here, so `App` stays pure — the same split as the
    /// chat-list projection (#113).
    ///
    /// **Refreshing the same chat** (a live update, or a freshly-merged history
    /// page) preserves the cursor by message *id*, not index: the selected message
    /// keeps its place even as older messages are prepended above it or a new one
    /// arrives below, so a background change never jumps the view. (A scroll-up at
    /// the very top first triggers an older page; the cursor then sits one row down
    /// from the top, so the next scroll-up reveals the newly loaded messages.)
    ///
    /// **Switching to a different chat** drops the previous chat's view entirely —
    /// messages, cursor, and the per-message download/typing state — and starts
    /// fresh at the top of the new history.
    pub fn project(&mut self, chat_id: i64, messages: Vec<Message>, pinned: HashSet<i64>) {
        if self.chat_id == Some(chat_id) {
            // Same chat: keep the selected message under the cursor across the swap.
            let anchor = self.selected_message().map(|m| m.id);
            self.messages = messages;
            self.pinned = pinned;
            self.offset = anchor
                .and_then(|id| self.messages.iter().position(|m| m.id == id))
                .unwrap_or(self.offset)
                .min(self.messages.len().saturating_sub(1));
        } else {
            // A different chat opened: a fresh view at the top, dropping the
            // previous chat's per-message state (downloads, typing indicator).
            *self = Self {
                chat_id: Some(chat_id),
                messages,
                pinned,
                ..Self::default()
            };
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

    /// Whether the message with id `id` is pinned in this chat.
    #[must_use]
    pub fn is_pinned(&self, id: i64) -> bool {
        self.pinned.contains(&id)
    }

    /// The selected message — the one at the scroll [`offset`](Self::offset),
    /// drawn at the top of the pane — or `None` on an empty history. The reaction
    /// and pin affordances act on this message.
    #[must_use]
    pub fn selected_message(&self) -> Option<&Message> {
        self.messages.get(self.offset)
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

    /// Record (or replace) the download state of a file, keyed by its id. The seam
    /// Phase 6 fills from the core [`FileStore`](tuigram_core::files::FileStore) on
    /// each `updateFile`; until then only the render tests call it.
    #[allow(dead_code)]
    pub fn set_download(&mut self, file: File) {
        self.downloads.insert(file.id, file);
    }

    /// Scroll one message toward the newest, clamping at the last message. A no-op
    /// on an empty history.
    pub fn scroll_down(&mut self) {
        self.offset = (self.offset + 1).min(self.messages.len().saturating_sub(1));
    }

    /// Scroll one message toward the oldest, clamping at the top.
    pub fn scroll_up(&mut self) {
        self.offset = self.offset.saturating_sub(1);
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
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
pub(crate) use tests::sample_message;

#[cfg(test)]
mod tests {
    use super::*;
    use tuigram_core::model::{FormattedText, Message, MessageContent, SendState, Sender};

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
        view.scroll_down();
        assert_eq!(view.offset(), 1);
        // Id 99 is not in the loaded history: the cursor stays put.
        assert!(!view.select_message(99));
        assert_eq!(view.offset(), 1);
    }

    #[test]
    fn scroll_down_advances_then_clamps_at_the_last_message() {
        let mut view = history(3);
        view.scroll_down();
        view.scroll_down();
        assert_eq!(view.offset(), 2);
        // Already on the last of three messages: clamps, does not run off the end.
        view.scroll_down();
        assert_eq!(view.offset(), 2);
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
        view.scroll_down();
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

    #[test]
    fn projecting_a_chat_populates_the_history_at_the_top() {
        let mut view = ConversationView::default();
        view.project(
            10,
            vec![text(1, "a"), text(2, "b"), text(3, "c")],
            HashSet::new(),
        );
        assert_eq!(view.len(), 3);
        assert_eq!(view.offset(), 0, "a freshly opened chat lands at the top");
        assert_eq!(view.selected_message().map(|m| m.id), Some(1));
    }

    #[test]
    fn refreshing_the_same_chat_keeps_the_selected_message_under_the_cursor() {
        let mut view = ConversationView::default();
        view.project(10, vec![text(2, "b"), text(3, "c")], HashSet::new());
        view.scroll_down(); // select message 3
        assert_eq!(view.selected_message().map(|m| m.id), Some(3));

        // An older page is merged ahead of the loaded ones: 3 stays selected, its
        // index shifts down by the two prepended messages.
        view.project(
            10,
            vec![text(0, "x"), text(1, "y"), text(2, "b"), text(3, "c")],
            HashSet::new(),
        );
        assert_eq!(
            view.selected_message().map(|m| m.id),
            Some(3),
            "cursor follows the id"
        );
        assert_eq!(view.offset(), 3);
    }

    #[test]
    fn a_live_message_on_the_same_chat_appears_without_moving_the_cursor() {
        let mut view = ConversationView::default();
        view.project(10, vec![text(1, "a"), text(2, "b")], HashSet::new());
        // A newer message arrives; the selected (top) message is unmoved.
        view.project(
            10,
            vec![text(1, "a"), text(2, "b"), text(3, "c")],
            HashSet::new(),
        );
        assert_eq!(view.len(), 3);
        assert_eq!(view.selected_message().map(|m| m.id), Some(1));
        assert_eq!(view.offset(), 0);
    }

    #[test]
    fn switching_chats_resets_to_a_fresh_view_at_the_top() {
        let mut view = ConversationView::default();
        view.project(10, vec![text(1, "a"), text(2, "b")], HashSet::from([1]));
        view.scroll_down();
        assert_eq!(view.offset(), 1);

        // A different chat replaces everything — messages, cursor, pinned set.
        view.project(20, vec![text(9, "z")], HashSet::new());
        assert_eq!(view.len(), 1);
        assert_eq!(view.offset(), 0, "new chat starts at the top");
        assert_eq!(view.selected_message().map(|m| m.id), Some(9));
        assert!(!view.is_pinned(1), "the previous chat's pins are gone");
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
    fn a_recorded_download_is_read_back_by_file_id() {
        let mut view = ConversationView::default();
        assert!(view.download(42).is_none());
        view.set_download(File {
            id: 42,
            size: 100,
            downloaded_size: 45,
            is_downloading_active: true,
            ..File::default()
        });
        let file = view.download(42).expect("recorded download");
        assert_eq!(file.downloaded_size, 45);
        assert!(file.is_downloading_active);
    }
}
