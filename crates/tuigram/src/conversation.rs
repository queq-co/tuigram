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

use std::collections::HashSet;

use tuigram_core::model::Message;

/// The history pane's state: the open chat's messages (oldest first), which of
/// them are pinned, and the scroll offset. Empty until Phase 6 projects the core
/// message store into it.
#[derive(Debug, Clone, Default)]
pub struct ConversationView {
    /// Messages in chronological order — index `0` is the oldest, drawn at the top.
    messages: Vec<Message>,
    /// Ids of the chat's pinned messages, for the pinned indicator.
    pinned: HashSet<i64>,
    /// Index of the topmost message to draw. Clamped to a valid row, or `0` when
    /// there are no messages.
    offset: usize,
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
}
