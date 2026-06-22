//! The application state (`App`) and the `Action` vocabulary every input is
//! reduced into. `App` is the single source of truth: each `tokio::select!` arm
//! translates its source into an [`Action`], [`App::dispatch`] applies it and
//! marks the frame dirty, and the loop repaints from the new state. Nothing here
//! touches the terminal or awaits — it stays a pure, unit-testable reducer.

use crossterm::event::Event;

use crate::chat_list::ChatListView;
use crate::composer::Composer;
use crate::conversation::ConversationView;
use crate::event::AppEvent;
use crate::forward::ForwardView;
use crate::keymap::{self, Focus, Overlay};
use crate::search::SearchView;

/// A single, already-interpreted intent. Every event source (terminal input, the
/// render tick, core updates) is funnelled through this enum before it touches
/// `App`, so all state changes share one code path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Nothing to do (e.g. an unbound key).
    Noop,
    /// Mark the frame dirty so the loop repaints, with no other state change.
    Render,
    /// A heartbeat from core — placeholder until Phase 6 wires the real `Client`.
    Beat,
    /// Move input focus to the next pane (chat list → history → composer → …).
    FocusNext,
    /// Move input focus to the previous pane, wrapping.
    FocusPrev,
    /// Move input focus directly to a specific pane (e.g. Enter on a chat opens
    /// the history; `i` in the history jumps to the composer).
    SetFocus(Focus),
    /// Show or hide the help overlay.
    ToggleHelp,
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
    /// Start forwarding the selected search hit: open the target picker.
    ForwardOpen,
    /// Move the forward target-picker selection down one chat.
    ForwardNext,
    /// Move the forward target-picker selection up one chat.
    ForwardPrev,
    /// Confirm the forward to the selected target. Phase 6 sends through core; for
    /// now it just closes the picker.
    ForwardConfirm,
    /// Cancel the forward and return to the search results.
    ForwardCancel,
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
    /// Count of core heartbeats applied — proof the mpsc arm is live until the
    /// real update stream replaces the fake source in Phase 6.
    beats: u64,
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
    /// The search overlay's state: the query line and the hit list it renders from.
    search: SearchView,
    /// The forward overlay's state: the messages being forwarded and the target
    /// picker. Inert until a forward is started.
    forward: ForwardView,
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

    pub fn beats(&self) -> u64 {
        self.beats
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

    /// The search overlay's state, for rendering the query line and results.
    pub fn search(&self) -> &SearchView {
        &self.search
    }

    /// The forward overlay's state, for rendering the target picker.
    pub fn forward(&self) -> &ForwardView {
        &self.forward
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

    /// Inject a search result set, standing in for the Phase 6 core search. The
    /// seam the reducer and render tests use to drive the results/forward overlays.
    #[cfg(test)]
    pub fn inject_search_results(&mut self, results: Vec<crate::search::SearchHit>) {
        self.search.set_results(results);
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

    /// Map a core [`AppEvent`] to an [`Action`].
    pub fn on_app_event(&self, event: AppEvent) -> Action {
        match event {
            AppEvent::Beat => Action::Beat,
        }
    }

    /// Apply an [`Action`], mutating state and marking the frame dirty when the
    /// visible state changed. The single write path into `App`.
    pub fn dispatch(&mut self, action: Action) {
        match action {
            Action::Noop => {}
            Action::Render => self.dirty = true,
            Action::Beat => {
                self.beats += 1;
                self.dirty = true;
            }
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
                // only emits this from browsing or while help is already open.
                self.overlay = if self.overlay == Overlay::Help {
                    Overlay::None
                } else {
                    Overlay::Help
                };
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
                // Phase 6 routes the submitted text to core (a new message, reply,
                // or edit per the composer's mode); for now it is consumed. An empty
                // buffer returns `None` — the send is a no-op that does not repaint.
                if self.composer.submit().is_some() {
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
                // Phase 6 runs the query against core and folds the hits in through
                // `set_results`; for now we just switch to the (injected-or-empty)
                // results list so the overlay flow is exercised headlessly.
                self.overlay = Overlay::SearchResults;
                self.dirty = true;
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
            Action::ForwardOpen => {
                // Forward the selected hit. The picker reuses a snapshot of the
                // chat list as its target list. No selected hit (empty results) is
                // a no-op that stays on the results overlay.
                if let Some(message_id) = self.search.selected_hit().map(|h| h.message_id) {
                    self.forward = ForwardView::new(vec![message_id], self.chat_list.clone());
                    self.overlay = Overlay::Forward;
                    self.dirty = true;
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
                // Phase 6 calls `Client::forward_messages` to the selected target;
                // for now confirming just closes the picker back to browsing.
                self.overlay = Overlay::None;
                self.dirty = true;
            }
            Action::ForwardCancel => {
                // Back to the results the forward was started from.
                self.overlay = Overlay::SearchResults;
                self.dirty = true;
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
    fn beat_counts_and_dirties() {
        let mut app = App::new();
        app.clear_dirty();
        let action = app.on_app_event(AppEvent::Beat);
        app.dispatch(action);
        assert_eq!(app.beats(), 1);
        assert!(app.is_dirty());
    }

    #[test]
    fn noop_leaves_state_untouched() {
        let mut app = App::new();
        app.clear_dirty();
        app.dispatch(Action::Noop);
        assert!(!app.is_dirty());
        assert!(!app.should_quit());
        assert_eq!(app.beats(), 0);
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
    fn toggle_help_shows_then_hides_the_overlay_and_a_key_dismisses_it() {
        let mut app = App::new();
        assert!(!app.help_visible());
        app.dispatch(Action::ToggleHelp);
        assert!(app.help_visible());
        // While open the overlay is modal: any key resolves to a dismiss.
        assert_eq!(
            app.on_terminal_event(key(KeyCode::Char('x'), KeyModifiers::NONE)),
            Action::ToggleHelp
        );
        app.dispatch(Action::ToggleHelp);
        assert!(!app.help_visible());
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
    fn typing_then_submitting_routes_through_the_composer_and_dirties() {
        let mut app = App::new();
        app.dispatch(Action::ComposerInput('h'));
        app.dispatch(Action::ComposerInput('i'));
        assert_eq!(app.composer().text(), "hi");

        app.clear_dirty();
        app.dispatch(Action::ComposerSubmit);
        assert!(app.composer().is_empty(), "buffer consumed on send");
        assert!(app.is_dirty());
    }

    #[test]
    fn empty_submit_is_a_noop_and_does_not_repaint() {
        let mut app = App::new();
        app.clear_dirty();
        app.dispatch(Action::ComposerSubmit);
        assert!(app.composer().is_empty());
        assert!(!app.is_dirty(), "an empty send changes nothing");
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
        // Hits arrive (Phase 6: from the core search) before we land on results.
        app.inject_search_results(vec![
            SearchHit::new(1, 10, "Alice: hello"),
            SearchHit::new(2, 20, "Bob: kenobi"),
        ]);
        app.dispatch(Action::SearchSubmit);
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
    fn forwarding_a_hit_opens_the_target_picker_with_that_message() {
        let mut app = app_on_results();
        app.dispatch(Action::ResultNext); // select Bob's hit (message 20)
        app.dispatch(Action::ForwardOpen);
        assert_eq!(app.overlay(), Overlay::Forward);
        assert_eq!(app.forward().message_ids(), &[20]);
        // The picker reuses the chat list as its target list.
        assert_eq!(
            app.forward().selected_target().map(|c| c.title.as_str()),
            Some("Alice")
        );
    }

    #[test]
    fn forward_picks_a_target_then_confirms_back_to_browsing() {
        let mut app = app_on_results();
        app.dispatch(Action::ForwardOpen);
        app.dispatch(Action::ForwardNext);
        assert_eq!(
            app.forward().selected_target().map(|c| c.title.as_str()),
            Some("Bob")
        );
        app.dispatch(Action::ForwardConfirm);
        assert_eq!(app.overlay(), Overlay::None, "confirm closes the modal");
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
        app.dispatch(Action::SearchSubmit); // empty results
        app.dispatch(Action::ForwardOpen);
        assert_eq!(
            app.overlay(),
            Overlay::SearchResults,
            "no hit to forward, stays put"
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
}
