//! The render function: a pure projection of [`App`] onto a `Frame`. It reads an
//! `App` snapshot and never awaits — all blocking lives below the UI, so the
//! draw path stays synchronous and `TestBackend`-snapshottable.
//!
//! This is the three-pane chat skeleton (issue #79): an outer horizontal split
//! of a **chat list** (left) and a **conversation** (right), with the right pane
//! split vertically into a scrolling **message history** over a fixed-height
//! **composer**. The chat-list pane is live (issue #80); the conversation and
//! composer are still placeholders each later `ui:` issue fills in, writing its
//! tests against the `TestBackend` harness below.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph};

use tuigram_core::model::Chat;

use crate::app::App;

/// Chat-list pane width, as a percentage of the terminal; the conversation pane
/// fills the remainder. (The research doc allows fixed *or* percentage width;
/// percentage keeps the skeleton responsive across terminal sizes.)
const CHAT_LIST_PERCENT: u16 = 30;

/// Composer height in rows: one input line framed by a border.
const COMPOSER_HEIGHT: u16 = 3;

/// Marker drawn to the left of the selected chat row.
const SELECTED_SYMBOL: &str = "▶ ";

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

    render_chat_list(frame, list_area, app);
    render_history(frame, history_area, app);
    render_composer(frame, composer_area);
}

/// Left pane: the chat list (#80). Renders the active list's chats — each a title
/// with an unread badge — under a title naming the active list, with the selected
/// row highlighted. An empty list shows a placeholder. List switching and moving
/// the selection are driven through [`App`]'s reducer by the keymap.
fn render_chat_list(frame: &mut Frame, area: Rect, app: &App) {
    let view = app.chat_list();
    let block = Block::bordered().title(format!(" Chats — {} ", view.active_label()));

    let chats = view.active_chats();
    if chats.is_empty() {
        frame.render_widget(Paragraph::new("(no chats yet)").block(block), area);
        return;
    }

    let items: Vec<ListItem> = chats.iter().map(chat_item).collect();
    let list = List::new(items)
        .block(block)
        .highlight_symbol(SELECTED_SYMBOL)
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED));

    // A fresh `ListState` each frame: the selection comes from `App`, and ratatui
    // scrolls the offset to keep it visible — so long lists window themselves
    // without the (immutable) render path holding mutable scroll state.
    let mut state = ListState::default().with_selected(Some(view.selected()));
    frame.render_stateful_widget(list, area, &mut state);
}

/// One chat row: the title, plus a bold unread badge when the chat has unread
/// incoming messages.
fn chat_item(chat: &Chat) -> ListItem<'static> {
    let mut spans = vec![Span::raw(chat.title.clone())];
    if chat.unread_count > 0 {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("({})", chat.unread_count),
            Style::new().add_modifier(Modifier::BOLD),
        ));
    }
    ListItem::new(Line::from(spans))
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

    // --- chat-list pane (#80) ---

    use crate::app::Action;
    use crate::chat_list::{ChatList, ChatListView, sample_chat};
    use tuigram_core::model::ChatListKind;

    fn chat_list(kind: ChatListKind, label: &str, titles: &[(&str, i32)]) -> ChatList {
        ChatList {
            kind,
            label: label.to_owned(),
            chats: titles
                .iter()
                .enumerate()
                .map(|(i, (t, unread))| sample_chat(i as i64, t, *unread))
                .collect(),
        }
    }

    /// An app whose chat-list pane holds a Main list and an Archive list.
    fn app_with_lists() -> App {
        let view = ChatListView::from_lists(vec![
            chat_list(
                ChatListKind::Main,
                "Main",
                &[("Alice", 0), ("Bob", 3), ("Carol", 0)],
            ),
            chat_list(ChatListKind::Archive, "Archive", &[("Old Friend", 0)]),
        ]);
        App::with_chat_list(view)
    }

    /// Find the first buffer row whose text contains `needle`.
    fn row_containing(buffer: &Buffer, needle: &str) -> String {
        (0..buffer.area.height)
            .map(|y| row_text(buffer, y))
            .find(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("no row contains {needle:?}"))
    }

    #[test]
    fn empty_chat_list_shows_the_placeholder_and_active_label() {
        let text = flatten(&render(&App::new(), 80, 24));
        assert!(text.contains("Chats — Main"), "active list label");
        assert!(text.contains("(no chats yet)"), "empty placeholder");
    }

    #[test]
    fn chat_titles_render_in_the_list() {
        let text = flatten(&render(&app_with_lists(), 80, 24));
        assert!(text.contains("Alice"));
        assert!(text.contains("Bob"));
        assert!(text.contains("Carol"));
    }

    #[test]
    fn unread_chats_show_a_badge_count() {
        let buffer = render(&app_with_lists(), 80, 24);
        // Bob has 3 unread; the badge sits on Bob's row, read chats carry none.
        assert!(
            row_containing(&buffer, "Bob").contains("(3)"),
            "unread badge"
        );
        assert!(
            !row_containing(&buffer, "Alice").contains('('),
            "no badge if read"
        );
    }

    #[test]
    fn the_selected_row_carries_the_highlight_marker() {
        let mut app = app_with_lists();
        // Move the selection onto the second row (Bob).
        app.dispatch(Action::SelectNext);
        let buffer = render(&app, 80, 24);
        assert!(
            row_containing(&buffer, "Bob").contains('▶'),
            "marker on the selected row"
        );
        assert!(
            !row_containing(&buffer, "Alice").contains('▶'),
            "no marker on unselected rows"
        );
    }

    #[test]
    fn switching_lists_repaints_the_other_list() {
        let mut app = app_with_lists();
        app.dispatch(Action::NextList);
        let text = flatten(&render(&app, 80, 24));
        // The pane now shows Archive and its chat, not the Main list's chats.
        assert!(text.contains("Chats — Archive"), "archive label");
        assert!(text.contains("Old Friend"), "archive chat");
        assert!(!text.contains("Alice"), "main chats gone");
    }
}
