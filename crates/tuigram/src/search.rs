//! The search view-model: the query line and the transient results the search
//! overlay renders from.
//!
//! Search in core returns its hits in a transient
//! [`SearchResults`](tuigram_core::SearchResults) that is **never** folded into the
//! message store — a search must not mutate loaded history. This view mirrors that
//! separation on the UI side: it owns the query the user types and a snapshot of the
//! hits, rendered as their own overlay over the conversation rather than by
//! rewriting the history pane. Phase 6 runs the query and fills [`set_results`] from
//! the core result set over the event channel; Phase 5 drives it headlessly (the
//! render tests inject hits), so the input and navigation behaviour is exercised
//! today.
//!
//! The query line reuses the shared [`TextInput`] primitive, so its editing matches
//! the composer's exactly; the results are a flat, selectable list with a clamped
//! cursor, like the chat list.

use crate::textinput::TextInput;

/// One search hit, projected for display. Carries the `(chat_id, message_id)` a
/// Phase 6 jump-to or forward needs, plus the `preview` line shown in the overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    /// The chat the hit belongs to (for a Phase 6 jump-to / forward source).
    pub chat_id: i64,
    /// The matched message's id.
    pub message_id: i64,
    /// The display line: a short label of the chat/sender and the matched text.
    pub preview: String,
}

impl SearchHit {
    /// A hit with the given identity and display preview. Phase 6 builds these from
    /// the core result set; until then only the render tests construct them, so this
    /// is unused in the non-test binary.
    #[allow(dead_code)]
    #[must_use]
    pub fn new(chat_id: i64, message_id: i64, preview: impl Into<String>) -> Self {
        Self {
            chat_id,
            message_id,
            preview: preview.into(),
        }
    }
}

/// The search overlay's state: the editable query and a snapshot of the hits with
/// the selected row. Empty by default — a fresh search before anything is typed.
#[derive(Debug, Clone, Default)]
pub struct SearchView {
    /// The query the user is typing.
    input: TextInput,
    /// The hits from the last run, in result order; empty before a run (or when a
    /// run found nothing).
    results: Vec<SearchHit>,
    /// Selection index into `results`, clamped to a valid row or `0` when empty.
    selected: usize,
}

impl SearchView {
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

    /// Replace the hits with a fresh result set, resetting the selection to the top.
    /// The seam Phase 6 (and the render tests) fill from the core result set; until
    /// the search query is wired to core, only the tests call it.
    #[allow(dead_code)]
    pub fn set_results(&mut self, results: Vec<SearchHit>) {
        self.results = results;
        self.selected = 0;
    }

    /// The collected hits, in result order.
    #[must_use]
    pub fn results(&self) -> &[SearchHit] {
        &self.results
    }

    /// The selected row index (`0` when there are no hits).
    #[must_use]
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// The currently selected hit, or `None` when there are no hits.
    #[must_use]
    pub fn selected_hit(&self) -> Option<&SearchHit> {
        self.results.get(self.selected)
    }

    /// Move the selection down one row, clamping at the last hit. A no-op with no
    /// hits.
    pub fn select_next(&mut self) {
        if !self.results.is_empty() {
            self.selected = (self.selected + 1).min(self.results.len() - 1);
        }
    }

    /// Move the selection up one row, clamping at the first hit.
    pub fn select_prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Clear the query and results back to a fresh search — used when the overlay
    /// is (re)opened so a previous search never leaks into the next.
    pub fn reset(&mut self) {
        self.input.clear();
        self.results.clear();
        self.selected = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hits(n: usize) -> Vec<SearchHit> {
        (0..n)
            .map(|i| SearchHit::new(1, i as i64, format!("hit {i}")))
            .collect()
    }

    #[test]
    fn default_is_an_empty_query_with_no_hits() {
        let view = SearchView::default();
        assert_eq!(view.query(), "");
        assert!(view.results().is_empty());
        assert_eq!(view.selected(), 0);
        assert_eq!(view.selected_hit(), None);
    }

    #[test]
    fn typing_builds_the_query_through_the_shared_input() {
        let mut view = SearchView::default();
        for c in "kenobi".chars() {
            view.insert(c);
        }
        assert_eq!(view.query(), "kenobi");
        assert_eq!(view.cursor(), 6);
        view.backspace();
        assert_eq!(view.query(), "kenob");
    }

    #[test]
    fn set_results_replaces_the_hits_and_resets_the_selection() {
        let mut view = SearchView::default();
        view.set_results(hits(3));
        view.select_next();
        view.select_next();
        assert_eq!(view.selected(), 2);
        // A new result set drops the stale cursor back to the top.
        view.set_results(hits(2));
        assert_eq!(view.selected(), 0);
        assert_eq!(view.results().len(), 2);
    }

    #[test]
    fn selection_advances_then_clamps_at_the_last_hit() {
        let mut view = SearchView::default();
        view.set_results(hits(2));
        view.select_next();
        assert_eq!(view.selected(), 1);
        view.select_next();
        assert_eq!(view.selected(), 1, "clamps, does not wrap");
        assert_eq!(view.selected_hit(), Some(&SearchHit::new(1, 1, "hit 1")));
    }

    #[test]
    fn selection_on_no_hits_is_a_noop() {
        let mut view = SearchView::default();
        view.select_next();
        view.select_prev();
        assert_eq!(view.selected(), 0);
        assert_eq!(view.selected_hit(), None);
    }

    #[test]
    fn reset_clears_the_query_and_the_hits() {
        let mut view = SearchView::default();
        view.insert('x');
        view.set_results(hits(3));
        view.select_next();
        view.reset();
        assert_eq!(view.query(), "");
        assert!(view.results().is_empty());
        assert_eq!(view.selected(), 0);
    }
}
