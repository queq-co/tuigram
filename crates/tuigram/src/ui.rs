//! The render function: a pure projection of [`App`] onto a `Frame`. It reads an
//! `App` snapshot and never awaits — all blocking lives below the UI, so the
//! draw path stays synchronous and `TestBackend`-snapshottable.
//!
//! This is the three-pane chat skeleton (issue #79): an outer horizontal split
//! of a **chat list** (left) and a **conversation** (right), with the right pane
//! split vertically into a scrolling **message history** over a fixed-height
//! **composer**. The panes are empty placeholders for now; each later `ui:` issue
//! fills one in, writing its tests against the `TestBackend` harness below.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::widgets::{Block, Paragraph};

use crate::app::App;

/// Chat-list pane width, as a percentage of the terminal; the conversation pane
/// fills the remainder. (The research doc allows fixed *or* percentage width;
/// percentage keeps the skeleton responsive across terminal sizes.)
const CHAT_LIST_PERCENT: u16 = 30;

/// Composer height in rows: one input line framed by a border.
const COMPOSER_HEIGHT: u16 = 3;

/// Render the whole UI for one frame from the current `App` state.
pub fn ui(frame: &mut Frame, app: &App) {
    // Outer split: chat list | conversation (fills the rest).
    let [list_area, convo_area] = Layout::horizontal([
        Constraint::Percentage(CHAT_LIST_PERCENT),
        Constraint::Min(0),
    ])
    .areas(frame.area());

    // Conversation split: message history over a fixed composer line.
    let [history_area, composer_area] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(COMPOSER_HEIGHT)])
            .areas(convo_area);

    render_chat_list(frame, list_area);
    render_history(frame, history_area, app);
    render_composer(frame, composer_area);
}

/// Left pane: the list of chats. Placeholder until issue #80.
fn render_chat_list(frame: &mut Frame, area: Rect) {
    let widget = Paragraph::new("(no chats yet)").block(Block::bordered().title(" Chats "));
    frame.render_widget(widget, area);
}

/// Right/top pane: the conversation history. Placeholder until issue #81; for now
/// it doubles as the liveness view, echoing the core heartbeat count.
fn render_history(frame: &mut Frame, area: Rect, app: &App) {
    let body = format!(
        "tuigram — Phase 5 TUI skeleton\n\ncore heartbeats: {}\n\npress q / Esc / Ctrl-C to quit",
        app.beats()
    );
    let widget = Paragraph::new(body).alignment(Alignment::Center).block(
        Block::bordered()
            .title(" tuigram ")
            .title_alignment(Alignment::Center),
    );
    frame.render_widget(widget, area);
}

/// Right/bottom pane: the message composer. Placeholder until issue #82.
fn render_composer(frame: &mut Frame, area: Rect) {
    let widget = Paragraph::new("type a message…").block(Block::bordered().title(" Message "));
    frame.render_widget(widget, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::{Buffer, Cell};

    /// The TTY-free snapshot harness: render a known `App` into an in-memory
    /// `Buffer` at a fixed size. Every later `ui:` issue asserts against this.
    fn render(app: &App, width: u16, height: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| ui(frame, app)).unwrap();
        terminal.backend().buffer().clone()
    }

    /// Whole-buffer text, for substring assertions on rendered content.
    fn flatten(buffer: &Buffer) -> String {
        buffer.content().iter().map(Cell::symbol).collect()
    }

    /// Text of a single buffer row, for positional (layout) assertions.
    fn row_text(buffer: &Buffer, y: u16) -> String {
        (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect()
    }

    #[test]
    fn renders_three_pane_skeleton() {
        let text = flatten(&render(&App::new(), 80, 24));
        assert!(text.contains("Chats"), "chat-list pane");
        assert!(text.contains("tuigram"), "conversation pane");
        assert!(text.contains("Message"), "composer pane");
        assert!(text.contains("quit"), "quit hint");
    }

    #[test]
    fn shows_heartbeat_count() {
        let mut app = App::new();
        app.dispatch(crate::app::Action::Beat);
        app.dispatch(crate::app::Action::Beat);
        assert!(flatten(&render(&app, 80, 24)).contains("heartbeats: 2"));
    }

    #[test]
    fn chat_list_sits_on_the_left() {
        let buffer = render(&App::new(), 80, 24);
        let top = row_text(&buffer, 0);
        let chats_col = top.find("Chats").expect("Chats title on the top row");
        // The list title must fall inside the left ~30% of an 80-col terminal.
        assert!(chats_col < (80 * CHAT_LIST_PERCENT / 100) as usize);
    }

    #[test]
    fn composer_is_pinned_to_the_bottom() {
        let buffer = render(&App::new(), 80, 24);
        // The composer is the bottom COMPOSER_HEIGHT rows; its bordered title row
        // is the first of those.
        let composer_top = row_text(&buffer, 24 - COMPOSER_HEIGHT);
        assert!(composer_top.contains("Message"), "composer at the bottom");
    }
}
