//! The chat-list view-model: the projection the left pane renders from.
//!
//! The core [`ChatStore`](tuigram_core::ChatStore) folds TDLib's update stream
//! into the authoritative chat state and reads back each list already ordered
//! ([`main_list`](tuigram_core::ChatStore::main_list),
//! [`archive_list`](tuigram_core::ChatStore::archive_list),
//! [`folder_list`](tuigram_core::ChatStore::folder_list)). This view is the TUI
//! side of that: a display snapshot of those ordered lists plus the cursor state
//! the store has no opinion on — which list is **active** and which row is
//! **selected**. Phase 6 fills it from the store over the event channel; Phase 5
//! leaves it empty (real keys still drive selection and list switching against
//! whatever it holds, so the behaviour is exercised headlessly today).
//!
//! The lists are held in switch order — Main, then Archive, then each
//! user-defined folder — so [`next_list`](ChatListView::next_list) /
//! [`prev_list`](ChatListView::prev_list) cycle them and the chat store's
//! ordering is preserved verbatim (this never re-sorts; it only points at rows).

use std::collections::HashMap;

use tuigram_core::model::{Chat, ChatAction, ChatListKind, SecretChatState};

/// One switchable list: its [kind](ChatListKind), the label shown in the pane
/// title, and its chats in the order the store handed them back.
#[derive(Debug, Clone)]
pub struct ChatList {
    /// Which TDLib list this is (Main, Archive, or a folder by id). Carried for
    /// the Phase 6 projection (mapping a list back to its store query); the
    /// render reads only the label and chats, so the binary does not read it yet.
    #[allow(dead_code)]
    pub kind: ChatListKind,
    /// Display label for the pane title — "Main", "Archive", or a folder name.
    pub label: String,
    /// The list's chats, already ordered by the core store.
    pub chats: Vec<Chat>,
}

/// The chat-list pane's state: the lists the user can cycle through, which one is
/// active, and the selection within it. Always holds at least the Main list, so
/// "the active list" and "switch to the next list" are always well-defined.
#[derive(Debug, Clone)]
pub struct ChatListView {
    /// Lists in switch order; never empty (Main is always present).
    lists: Vec<ChatList>,
    /// Index into `lists` of the active list.
    active: usize,
    /// Selection index within the active list's chats. Clamped to a valid row,
    /// or `0` when the active list is empty.
    selected: usize,
    /// Secret-chat lifecycle state, keyed by chat id (#87). Only secret chats
    /// appear; a chat id is globally unique, so one map spans every list. Phase 6
    /// projects this from the core
    /// [`SecretChatStore`](tuigram_core::SecretChatStore); never any key material,
    /// only the [`SecretChatState`].
    secret_states: HashMap<i64, SecretChatState>,
    /// The transient chat action currently shown per chat id (#87) — the "typing…"
    /// indicator. Phase 6 projects this from the core
    /// [`ChatActionStore`](tuigram_core::ChatActionStore); empty until then.
    actions: HashMap<i64, ChatAction>,
}

impl Default for ChatListView {
    /// An empty Main list, nothing selected. The pre-data Phase 5 state.
    fn default() -> Self {
        Self {
            lists: vec![ChatList {
                kind: ChatListKind::Main,
                label: "Main".to_owned(),
                chats: Vec::new(),
            }],
            active: 0,
            selected: 0,
            secret_states: HashMap::new(),
            actions: HashMap::new(),
        }
    }
}

impl ChatListView {
    /// Build a view from the lists the core store projected, in switch order.
    /// Empty input falls back to the default empty Main list, preserving the
    /// "always at least one list" invariant. The first list is active.
    ///
    /// The Phase 6 update path (and the render tests) build the view this way;
    /// the running binary still only shows [`default`](Self::default) until that
    /// path is wired, so this is unused in the non-test binary for now.
    #[allow(dead_code)]
    #[must_use]
    pub fn from_lists(lists: Vec<ChatList>) -> Self {
        if lists.is_empty() {
            return Self::default();
        }
        Self {
            lists,
            active: 0,
            selected: 0,
            secret_states: HashMap::new(),
            actions: HashMap::new(),
        }
    }

    /// The active list's display label, for the pane title.
    #[must_use]
    pub fn active_label(&self) -> &str {
        &self.lists[self.active].label
    }

    /// The active list's chats, in store order.
    #[must_use]
    pub fn active_chats(&self) -> &[Chat] {
        &self.lists[self.active].chats
    }

    /// The selected row index within the active list (`0` when empty).
    #[must_use]
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// The chat under the selection in the active list, or `None` when the list is
    /// empty. The chat the secret-chat lifecycle action (#87) operates on.
    #[must_use]
    pub fn selected_chat(&self) -> Option<&Chat> {
        self.active_chats().get(self.selected)
    }

    /// The folded secret-chat lifecycle state for chat `chat_id`, if it is a known
    /// secret chat (#87). `None` for an ordinary chat, or a secret chat whose
    /// `updateSecretChat` has not arrived yet.
    #[must_use]
    pub fn secret_state(&self, chat_id: i64) -> Option<SecretChatState> {
        self.secret_states.get(&chat_id).copied()
    }

    /// The chat action currently being performed in chat `chat_id`, if any (#87) —
    /// the source of the "typing…" indicator on that row.
    #[must_use]
    pub fn action(&self, chat_id: i64) -> Option<&ChatAction> {
        self.actions.get(&chat_id)
    }

    /// Record (or replace) the secret-chat lifecycle state for chat `chat_id`. The
    /// seam Phase 6 fills from the core
    /// [`SecretChatStore`](tuigram_core::SecretChatStore) on each `updateSecretChat`;
    /// until then only tests call it.
    #[allow(dead_code)]
    pub fn set_secret_state(&mut self, chat_id: i64, state: SecretChatState) {
        self.secret_states.insert(chat_id, state);
    }

    /// Record (or clear) the chat action for chat `chat_id`: `Some` shows the
    /// indicator, `None` removes it (a cancel). The seam Phase 6 fills from the core
    /// [`ChatActionStore`](tuigram_core::ChatActionStore) on each `updateChatAction`;
    /// until then only tests call it.
    #[allow(dead_code)]
    pub fn set_action(&mut self, chat_id: i64, action: Option<ChatAction>) {
        match action {
            Some(action) => {
                self.actions.insert(chat_id, action);
            }
            None => {
                self.actions.remove(&chat_id);
            }
        }
    }

    /// Move the selection down one row, clamping at the last row. A no-op on an
    /// empty list.
    pub fn select_next(&mut self) {
        let len = self.active_chats().len();
        if len > 0 {
            self.selected = (self.selected + 1).min(len - 1);
        }
    }

    /// Move the selection up one row, clamping at the first row.
    pub fn select_prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Switch to the next list in cycle order (Main → Archive → folders → Main),
    /// resetting the selection to the top of the newly active list.
    pub fn next_list(&mut self) {
        self.active = (self.active + 1) % self.lists.len();
        self.selected = 0;
    }

    /// Switch to the previous list in cycle order, wrapping, and reset the
    /// selection to the top.
    pub fn prev_list(&mut self) {
        self.active = (self.active + self.lists.len() - 1) % self.lists.len();
        self.selected = 0;
    }
}

#[cfg(test)]
pub(crate) use tests::sample_chat;

#[cfg(test)]
mod tests {
    use super::*;
    use tuigram_core::model::ChatKind;

    /// A minimal projected [`Chat`] for view tests: id, title, and unread count;
    /// every other field defaulted. The view holds chats already ordered, so no
    /// position is needed here.
    pub(crate) fn sample_chat(id: i64, title: &str, unread: i32) -> Chat {
        Chat {
            id,
            title: title.to_owned(),
            kind: ChatKind::Private { user_id: id },
            last_message: None,
            unread_count: unread,
            unread_mention_count: 0,
            last_read_inbox_message_id: 0,
            last_read_outbox_message_id: 0,
            positions: Vec::new(),
            draft: None,
            pinned_message_ids: Vec::new(),
        }
    }

    fn list(kind: ChatListKind, label: &str, titles: &[&str]) -> ChatList {
        ChatList {
            kind,
            label: label.to_owned(),
            chats: titles
                .iter()
                .enumerate()
                .map(|(i, t)| sample_chat(i as i64, t, 0))
                .collect(),
        }
    }

    fn three_lists() -> ChatListView {
        ChatListView::from_lists(vec![
            list(ChatListKind::Main, "Main", &["Alice", "Bob", "Carol"]),
            list(ChatListKind::Archive, "Archive", &["Old"]),
            list(ChatListKind::Folder(7), "Work", &["Team", "Boss"]),
        ])
    }

    #[test]
    fn default_is_an_empty_main_list() {
        let view = ChatListView::default();
        assert_eq!(view.active_label(), "Main");
        assert!(view.active_chats().is_empty());
        assert_eq!(view.selected(), 0);
    }

    #[test]
    fn empty_input_falls_back_to_default() {
        let view = ChatListView::from_lists(Vec::new());
        assert_eq!(view.active_label(), "Main");
        assert!(view.active_chats().is_empty());
    }

    #[test]
    fn select_next_advances_then_clamps_at_the_last_row() {
        let mut view = three_lists();
        assert_eq!(view.selected(), 0);
        view.select_next();
        view.select_next();
        assert_eq!(view.selected(), 2);
        // Already on the last of three rows: clamps, does not wrap.
        view.select_next();
        assert_eq!(view.selected(), 2);
    }

    #[test]
    fn select_prev_clamps_at_the_first_row() {
        let mut view = three_lists();
        view.select_next();
        view.select_prev();
        view.select_prev();
        assert_eq!(view.selected(), 0);
    }

    #[test]
    fn select_next_on_an_empty_list_is_a_noop() {
        let mut view = ChatListView::default();
        view.select_next();
        assert_eq!(view.selected(), 0);
    }

    #[test]
    fn next_list_cycles_main_archive_folder_and_back() {
        let mut view = three_lists();
        assert_eq!(view.active_label(), "Main");
        view.next_list();
        assert_eq!(view.active_label(), "Archive");
        view.next_list();
        assert_eq!(view.active_label(), "Work");
        view.next_list();
        assert_eq!(view.active_label(), "Main");
    }

    #[test]
    fn prev_list_cycles_backwards_with_wrap() {
        let mut view = three_lists();
        view.prev_list();
        assert_eq!(view.active_label(), "Work");
        view.prev_list();
        assert_eq!(view.active_label(), "Archive");
    }

    #[test]
    fn the_selected_chat_is_the_one_under_the_cursor() {
        let mut view = three_lists();
        assert_eq!(
            view.selected_chat().map(|c| c.title.as_str()),
            Some("Alice")
        );
        view.select_next();
        assert_eq!(view.selected_chat().map(|c| c.title.as_str()), Some("Bob"));
        // An empty list resolves to no selected chat rather than panicking.
        assert!(ChatListView::default().selected_chat().is_none());
    }

    #[test]
    fn a_secret_state_is_recorded_and_read_back_by_chat_id() {
        let mut view = three_lists();
        assert!(view.secret_state(0).is_none(), "no state until projected");
        view.set_secret_state(0, SecretChatState::Pending);
        assert_eq!(view.secret_state(0), Some(SecretChatState::Pending));
        // A lifecycle advance overwrites in place.
        view.set_secret_state(0, SecretChatState::Ready);
        assert_eq!(view.secret_state(0), Some(SecretChatState::Ready));
    }

    #[test]
    fn a_chat_action_is_recorded_then_cleared_by_chat_id() {
        let mut view = three_lists();
        assert!(view.action(1).is_none());
        view.set_action(1, Some(ChatAction::Typing));
        assert_eq!(view.action(1), Some(&ChatAction::Typing));
        // A cancel (None) clears it.
        view.set_action(1, None);
        assert!(view.action(1).is_none(), "cancel removes the indicator");
    }

    #[test]
    fn switching_lists_resets_the_selection() {
        let mut view = three_lists();
        view.select_next();
        view.select_next();
        assert_eq!(view.selected(), 2);
        view.next_list();
        // New list, cursor back at the top — not a stale index into the old list.
        assert_eq!(view.selected(), 0);
        assert_eq!(view.active_chats().len(), 1);
    }
}
