//! The render function: a pure projection of [`App`] onto a `Frame`. It reads an
//! `App` snapshot and never awaits — all blocking lives below the UI, so the
//! draw path stays synchronous and `TestBackend`-snapshottable. The real
//! three-pane chat layout lands in a later Phase 5 issue; this is the minimal
//! "the spine boots and repaints" view.

use ratatui::Frame;
use ratatui::layout::Alignment;
use ratatui::widgets::{Block, Paragraph};

use crate::app::App;

/// Render the whole UI for one frame from the current `App` state.
pub fn ui(frame: &mut Frame, app: &App) {
    let body = format!(
        "tuigram — Phase 5 TUI scaffold\n\ncore heartbeats: {}\n\npress q / Esc / Ctrl-C to quit",
        app.beats()
    );
    let widget = Paragraph::new(body).alignment(Alignment::Center).block(
        Block::bordered()
            .title(" tuigram ")
            .title_alignment(Alignment::Center),
    );
    frame.render_widget(widget, frame.area());
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn rendered_text(app: &App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(48, 12)).unwrap();
        terminal.draw(|frame| ui(frame, app)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect()
    }

    #[test]
    fn renders_scaffold_and_quit_hint() {
        let text = rendered_text(&App::new());
        assert!(text.contains("tuigram"));
        assert!(text.contains("quit"));
    }

    #[test]
    fn shows_heartbeat_count() {
        let mut app = App::new();
        app.dispatch(crate::app::Action::Beat);
        app.dispatch(crate::app::Action::Beat);
        let text = rendered_text(&app);
        assert!(text.contains("heartbeats: 2"));
    }
}
