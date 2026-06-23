//! The forward view-model: the messages being forwarded and the target-chat
//! picker.
//!
//! Forwarding is a write — core's [`Client::forward_messages`] copies one or more
//! messages into another chat — driven from the UI by a small modal: pick a target
//! chat, confirm. The picker **reuses the chat-list widget** ([`ChatListView`]) so
//! the user navigates targets exactly as they navigate their chats, rather than a
//! second, divergent list control. Phase 6 sends on confirm; Phase 5 leaves the
//! confirm a no-op that just closes the modal, so the selection behaviour is
//! exercised headlessly today.
//!
//! [`Client::forward_messages`]: tuigram_core::Client

use tuigram_core::model::Chat;

use crate::chat_list::ChatListView;

/// The forward modal's state: which messages are being forwarded and the
/// chat-list-backed target picker. Empty by default (no messages, an empty picker) —
/// the inert state held between forwards.
#[derive(Debug, Clone, Default)]
pub struct ForwardView {
    /// The messages being forwarded, by id. One or more, per the source selection.
    message_ids: Vec<i64>,
    /// The target picker — the chat-list widget reused to choose a destination.
    targets: ChatListView,
}

impl ForwardView {
    /// Begin forwarding `message_ids` into one of `targets`, with the picker's
    /// selection at the top.
    #[must_use]
    pub fn new(message_ids: Vec<i64>, targets: ChatListView) -> Self {
        Self {
            message_ids,
            targets,
        }
    }

    /// The ids of the messages being forwarded — read by the Phase 6 confirm (the
    /// `forward_messages` call) and the reducer tests; the render shows only the
    /// count, so the binary does not read it yet.
    #[allow(dead_code)]
    #[must_use]
    pub fn message_ids(&self) -> &[i64] {
        &self.message_ids
    }

    /// How many messages are being forwarded.
    #[must_use]
    pub fn count(&self) -> usize {
        self.message_ids.len()
    }

    /// The target picker, for rendering the destination list.
    #[must_use]
    pub fn targets(&self) -> &ChatListView {
        &self.targets
    }

    /// The currently selected target chat, or `None` when the picker is empty. The
    /// Phase 6 confirm reads it to pick the destination; the render uses the picker's
    /// own selection, so the binary does not read it yet.
    #[allow(dead_code)]
    #[must_use]
    pub fn selected_target(&self) -> Option<&Chat> {
        self.targets.active_chats().get(self.targets.selected())
    }

    /// Move the target selection down one row.
    pub fn select_next(&mut self) {
        self.targets.select_next();
    }

    /// Move the target selection up one row.
    pub fn select_prev(&mut self) {
        self.targets.select_prev();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat_list::{ChatList, ChatListView, sample_chat};
    use tuigram_core::model::ChatListKind;

    fn targets() -> ChatListView {
        ChatListView::from_lists(vec![ChatList {
            kind: ChatListKind::Main,
            label: "Main".to_owned(),
            chats: vec![
                sample_chat(1, "Alice", 0),
                sample_chat(2, "Bob", 0),
                sample_chat(3, "Carol", 0),
            ],
        }])
    }

    #[test]
    fn default_is_empty() {
        let view = ForwardView::default();
        assert_eq!(view.count(), 0);
        assert!(view.message_ids().is_empty());
        assert_eq!(view.selected_target(), None);
    }

    #[test]
    fn carries_the_forwarded_messages_and_a_target_picker() {
        let view = ForwardView::new(vec![10, 11], targets());
        assert_eq!(view.count(), 2);
        assert_eq!(view.message_ids(), &[10, 11]);
        // The picker starts at the top of the target list.
        assert_eq!(view.selected_target().map(|c| c.id), Some(1));
    }

    #[test]
    fn selection_moves_through_the_target_list() {
        let mut view = ForwardView::new(vec![10], targets());
        view.select_next();
        assert_eq!(
            view.selected_target().map(|c| c.title.as_str()),
            Some("Bob")
        );
        view.select_next();
        view.select_next();
        // Clamps at the last target (reusing the chat list's clamping).
        assert_eq!(
            view.selected_target().map(|c| c.title.as_str()),
            Some("Carol")
        );
        view.select_prev();
        assert_eq!(
            view.selected_target().map(|c| c.title.as_str()),
            Some("Bob")
        );
    }
}
