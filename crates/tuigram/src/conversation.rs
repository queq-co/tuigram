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

use tuigram_core::model::{File, Message, Reaction, ReactionKind};

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
            messages,
            pinned,
            offset: 0,
            downloads: HashMap::new(),
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
