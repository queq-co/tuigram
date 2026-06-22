//! The application state (`App`) and the `Action` vocabulary every input is
//! reduced into. `App` is the single source of truth: each `tokio::select!` arm
//! translates its source into an [`Action`], [`App::dispatch`] applies it and
//! marks the frame dirty, and the loop repaints from the new state. Nothing here
//! touches the terminal or awaits — it stays a pure, unit-testable reducer.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::event::AppEvent;

/// A single, already-interpreted intent. Every event source (terminal input, the
/// render tick, core updates) is funnelled through this enum before it touches
/// `App`, so all state changes share one code path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Nothing to do (e.g. an unbound key).
    Noop,
    /// Mark the frame dirty so the loop repaints, with no other state change.
    Render,
    /// A heartbeat from core — placeholder until Phase 6 wires the real `Client`.
    Beat,
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

    /// Called by the loop after a successful `terminal.draw`.
    pub fn clear_dirty(&mut self) {
        self.dirty = false;
    }

    /// Map a raw crossterm event to an [`Action`]. Pure: no state changes here,
    /// so key bindings are trivially unit-testable.
    pub fn on_terminal_event(&self, event: Event) -> Action {
        match event {
            Event::Key(key) => Self::on_key(key),
            // A resize must repaint against the new viewport.
            Event::Resize(_, _) => Action::Render,
            _ => Action::Noop,
        }
    }

    fn on_key(key: KeyEvent) -> Action {
        // crossterm on Windows reports both Press and Release; act on Press
        // (and the kindless Unix events, which are not Release) only.
        if key.kind == KeyEventKind::Release {
            return Action::Noop;
        }
        match (key.modifiers, key.code) {
            (KeyModifiers::CONTROL, KeyCode::Char('c')) => Action::Quit,
            (_, KeyCode::Char('q') | KeyCode::Esc) => Action::Quit,
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
            Action::Quit => self.should_quit = true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }

    #[test]
    fn q_esc_and_ctrl_c_quit() {
        let app = App::new();
        assert_eq!(
            app.on_terminal_event(key(KeyCode::Char('q'), KeyModifiers::NONE)),
            Action::Quit
        );
        assert_eq!(
            app.on_terminal_event(key(KeyCode::Esc, KeyModifiers::NONE)),
            Action::Quit
        );
        assert_eq!(
            app.on_terminal_event(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Action::Quit
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
}
