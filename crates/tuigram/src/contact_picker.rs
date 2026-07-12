//! The contact-search picker (#197): search this account's contacts by name and
//! pick one to start a new secret chat with — the gap `secret.rs`'s lifecycle
//! decision can't reach, since it only offers a secret chat for a chat already
//! in the chat list ([`SecretLifecycle::for_chat`](crate::secret::SecretLifecycle::for_chat)),
//! never an arbitrary contact. The REPL's `secret-new <user_id>` has no such
//! restriction (it takes any id typed in), which is why this was deferred out of
//! #195/#196 rather than bound to a key outright.
//!
//! Mirrors [`search.rs`](crate::search)'s shape: a [`TextInput`] query line plus
//! a flat, clamped-cursor result list. The loop runs
//! [`ContactRequests::search_contacts`](tuigram_core::ContactRequests::search_contacts)
//! and resolves each returned id to a display name through the folded user
//! store (backfilling via `get_user` exactly like any other id-only result),
//! then fills [`set_results`](ContactPickerView::set_results). Confirming a hit
//! does not itself create the secret chat — it hands off to the existing
//! [`SecretChatPrompt`](crate::secret::SecretChatPrompt) confirm overlay, so the
//! "are you sure" step stays shared with the chat-list-scoped lifecycle path.

use crate::textinput::TextInput;

/// One contact hit: a user id and the display name resolved for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContactHit {
    /// The user id — what [`SecretLifecycle::Start`](crate::secret::SecretLifecycle::Start)
    /// needs to open the secret chat.
    pub user_id: i64,
    /// The name shown in the results list and the confirm prompt.
    pub display_name: String,
}

impl ContactHit {
    /// A hit for `user_id` named `display_name`.
    #[must_use]
    pub fn new(user_id: i64, display_name: impl Into<String>) -> Self {
        Self {
            user_id,
            display_name: display_name.into(),
        }
    }
}

/// The contact-picker overlay's state: the editable query and a snapshot of the
/// matching contacts, with the selected row. Empty by default — a fresh search
/// before anything is typed.
#[derive(Debug, Clone, Default)]
pub struct ContactPickerView {
    /// The query the user is typing.
    input: TextInput,
    /// The hits from the last run, in result order; empty before a run (or when
    /// a run found nothing).
    results: Vec<ContactHit>,
    /// Selection index into `results`, clamped to a valid row or `0` when empty.
    selected: usize,
}

impl ContactPickerView {
    /// The current query text.
    #[must_use]
    pub fn query(&self) -> &str {
        self.input.text()
    }

    /// The query cursor position, as a character index.
    #[must_use]
    pub fn cursor(&self) -> usize {
        self.input.cursor()
    }

    /// Insert a character into the query at the cursor.
    pub fn insert(&mut self, c: char) {
        self.input.insert(c);
    }

    /// Delete the character before the query cursor (Backspace).
    pub fn backspace(&mut self) {
        self.input.backspace();
    }

    /// Move the query cursor one character left.
    pub fn move_left(&mut self) {
        self.input.move_left();
    }

    /// Move the query cursor one character right.
    pub fn move_right(&mut self) {
        self.input.move_right();
    }

    /// Move the query cursor to the start of the line.
    pub fn move_home(&mut self) {
        self.input.move_home();
    }

    /// Move the query cursor to the end of the line.
    pub fn move_end(&mut self) {
        self.input.move_end();
    }

    /// Replace the hits with a fresh result set, resetting the selection to the
    /// top. Filled from the projected core result set when a search completes.
    pub fn set_results(&mut self, results: Vec<ContactHit>) {
        self.results = results;
        self.selected = 0;
    }

    /// The collected hits, in result order.
    #[must_use]
    pub fn results(&self) -> &[ContactHit] {
        &self.results
    }

    /// The selected row index (`0` when there are no hits).
    #[must_use]
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// The currently selected hit, or `None` when there are no hits.
    #[must_use]
    pub fn selected_hit(&self) -> Option<&ContactHit> {
        self.results.get(self.selected)
    }

    /// Move the selection down one row, clamping at the last hit. A no-op with
    /// no hits.
    pub fn select_next(&mut self) {
        if !self.results.is_empty() {
            self.selected = (self.selected + 1).min(self.results.len() - 1);
        }
    }

    /// Move the selection up one row, clamping at the first hit.
    pub fn select_prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Move the selection directly to a row index (a click on a result), clamping
    /// at the last hit. A no-op with no hits.
    pub fn select(&mut self, index: usize) {
        if !self.results.is_empty() {
            self.selected = index.min(self.results.len() - 1);
        }
    }

    /// Clear the query and results back to a fresh search — used when the
    /// overlay (re)opens so a previous search never leaks into the next.
    pub fn reset(&mut self) {
        self.input.clear();
        self.results.clear();
        self.selected = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hits(n: usize) -> Vec<ContactHit> {
        (0..n)
            .map(|i| ContactHit::new(i as i64, format!("Contact {i}")))
            .collect()
    }

    #[test]
    fn default_is_an_empty_query_with_no_hits() {
        let view = ContactPickerView::default();
        assert_eq!(view.query(), "");
        assert!(view.results().is_empty());
        assert_eq!(view.selected(), 0);
        assert_eq!(view.selected_hit(), None);
    }

    #[test]
    fn typing_builds_the_query_through_the_shared_input() {
        let mut view = ContactPickerView::default();
        for c in "ada".chars() {
            view.insert(c);
        }
        assert_eq!(view.query(), "ada");
        assert_eq!(view.cursor(), 3);
        view.backspace();
        assert_eq!(view.query(), "ad");
    }

    #[test]
    fn set_results_replaces_the_hits_and_resets_the_selection() {
        let mut view = ContactPickerView::default();
        view.set_results(hits(3));
        view.select_next();
        view.select_next();
        assert_eq!(view.selected(), 2);
        view.set_results(hits(2));
        assert_eq!(view.selected(), 0);
        assert_eq!(view.results().len(), 2);
    }

    #[test]
    fn selection_advances_then_clamps_at_the_last_hit() {
        let mut view = ContactPickerView::default();
        view.set_results(hits(2));
        view.select_next();
        assert_eq!(view.selected(), 1);
        view.select_next();
        assert_eq!(view.selected(), 1, "clamps, does not wrap");
        assert_eq!(view.selected_hit(), Some(&ContactHit::new(1, "Contact 1")));
    }

    #[test]
    fn selection_on_no_hits_is_a_noop() {
        let mut view = ContactPickerView::default();
        view.select_next();
        view.select_prev();
        assert_eq!(view.selected(), 0);
        assert_eq!(view.selected_hit(), None);
    }

    #[test]
    fn reset_clears_the_query_and_the_hits() {
        let mut view = ContactPickerView::default();
        view.insert('x');
        view.set_results(hits(3));
        view.select_next();
        view.reset();
        assert_eq!(view.query(), "");
        assert!(view.results().is_empty());
        assert_eq!(view.selected(), 0);
    }
}
