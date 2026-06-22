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
use crate::keymap::{self, Focus};

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
    /// Whether the help overlay is shown over the panes.
    help_visible: bool,
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

    /// Whether the help overlay is currently shown.
    pub fn help_visible(&self) -> bool {
        self.help_visible
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
            Event::Key(key) => keymap::resolve(self.focus, self.help_visible, &key),
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
                self.help_visible = !self.help_visible;
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
}
