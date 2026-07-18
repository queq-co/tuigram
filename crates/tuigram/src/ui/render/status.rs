//! The status bar (#88) and the transient toast (#88/#139) that floats above
//! the content, independent of any modal overlay.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::app::App;
use crate::keymap::{Focus, Overlay};
use crate::status::NoticeLevel;

/// The persistent status bar (#88): a one-row reverse-video strip with the core
/// connection state and current chat/context on the left and the always-available
/// quit/help hint on the right. It takes over the quit hint the conversation
/// placeholder used to carry, so it is present on every screen, with or without
/// data.
pub(crate) fn render_status_bar(frame: &mut Frame, area: Rect, app: &App) {
    // A reverse-video bar sets the status row apart from the panes without relying
    // on colour, matching the rest of the UI's Modifier-only styling.
    let bar = Style::new().add_modifier(Modifier::REVERSED);
    let conn = app.connection();
    let hint = "? help · q quit ";

    // One line: connection + context on the left, the quit/help hint pushed to the
    // right by a padding span sized to fill the row.
    let mut spans = vec![
        Span::raw(" "),
        Span::styled(
            format!("{} {}", conn.symbol(), conn.label()),
            bar.add_modifier(Modifier::BOLD),
        ),
        Span::raw("  ·  "),
        Span::raw(status_context(app)),
    ];
    let used = spans
        .iter()
        .map(|s| s.content.chars().count())
        .sum::<usize>()
        + hint.chars().count();
    let pad = (area.width as usize).saturating_sub(used);
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(Span::raw(hint));

    frame.render_widget(Paragraph::new(Line::from(spans)).style(bar), area);
}

/// The status bar's context field: the selected chat's title and the focused
/// pane (or open overlay), so the bar always says where input is going.
fn status_context(app: &App) -> String {
    let chat = app
        .chat_list()
        .selected_chat()
        .map_or_else(|| "no chat selected".to_owned(), |c| c.title.clone());
    format!("{chat} — {}", mode_label(app))
}

/// The current input mode for the status bar: the open overlay's name, or the
/// focused pane when none is up.
fn mode_label(app: &App) -> &'static str {
    match app.overlay() {
        Overlay::None => match app.focus() {
            Focus::ChatList => "chats",
            Focus::History => "history",
            Focus::Composer => "compose",
        },
        Overlay::Help => "help",
        Overlay::SearchInput | Overlay::SearchResults => "search",
        Overlay::Forward => "forward",
        Overlay::Reaction => "react",
        Overlay::SendMedia => "attach",
        Overlay::SecretChat => "secret chat",
        Overlay::Settings => "settings",
        Overlay::DeleteConfirm => "delete",
        Overlay::LogoutConfirm => "logout",
        Overlay::ContactSearchInput | Overlay::ContactSearchResults => "new secret chat",
    }
}

/// Toast width as a share of the content width, clamped so a long line wraps
/// rather than spanning the screen.
const TOAST_MAX_WIDTH: u16 = 44;

/// A transient toast (#88), anchored top-right over the content: the current
/// notice's marker and message in a small bordered box, with a "+N" title when
/// more are queued and a dim dismiss hint. Errors are bolded. It draws nothing
/// when the queue is empty (the caller guards that) and never captures input.
pub(crate) fn render_toast(frame: &mut Frame, area: Rect, app: &App) {
    let notes = app.notifications();
    let Some(notice) = notes.current() else {
        return;
    };

    let line = notice.line();
    let line_cols = line.chars().count();
    let pending = notes.pending();
    let title = if pending > 0 {
        format!(" Notice (+{pending}) ")
    } else {
        " Notice ".to_owned()
    };
    // The dismiss affordance (#139), on the bottom border: a toast also ages out on
    // its own, but this tells the user how to clear one immediately.
    let hint = " Ctrl-G to dismiss ";

    // Width fits the message, the title, or the hint — whichever is widest — plus
    // borders, clamped to a readable maximum and to the content area.
    let content_cols = (line_cols
        .max(title.chars().count())
        .max(hint.chars().count())
        + 2) as u16;
    let width = content_cols
        .clamp(1, TOAST_MAX_WIDTH)
        .min(area.width.saturating_sub(2));
    // Height: the message wrapped to the inner width, plus borders, capped to the
    // content area.
    let inner = width.saturating_sub(2).max(1) as usize;
    let rows = line_cols.div_ceil(inner).max(1) as u16;
    let height = (rows + 2).min(area.height);

    // Top-right, one cell in from the border so it does not sit on the corner.
    let x = area.x + area.width.saturating_sub(width + 1);
    let y = area.y + 1;
    let rect = Rect {
        x,
        y: y.min(area.y + area.height.saturating_sub(height)),
        width,
        height,
    };

    // Errors bold the message; the hint stays dim regardless, so each carries its own
    // style rather than styling the whole widget.
    let emphasis = if notice.level() == NoticeLevel::Error {
        Style::new().add_modifier(Modifier::BOLD)
    } else {
        Style::new()
    };
    let dim = Style::new().add_modifier(Modifier::DIM);
    let block = Block::bordered()
        .title(title)
        .title_bottom(Line::styled(hint, dim).right_aligned());
    let body = Paragraph::new(Line::styled(line, emphasis))
        .wrap(ratatui::widgets::Wrap { trim: false })
        .block(block);
    frame.render_widget(Clear, rect);
    frame.render_widget(body, rect);
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // tests: panicking on a broken assumption is the point
mod tests {
    use ratatui::buffer::Buffer;
    use tuigram_core::model::ChatKind;

    use crate::status::{ConnectionState, Notice};

    use super::super::test_support::{flatten, render, row_text, view_with_one_chat};
    use super::*;

    /// The bottom status row of a rendered buffer.
    fn status_row(buffer: &Buffer) -> String {
        row_text(buffer, buffer.area.height - 1)
    }

    #[test]
    fn the_status_bar_sits_on_the_bottom_row_with_connection_and_hint() {
        let buffer = render(&App::new(), 80, 24);
        let bar = status_row(&buffer);
        // Default connection is "connecting…", and the quit/help hint the
        // placeholder used to carry now lives here.
        assert!(bar.contains("connecting"), "connection state: {bar:?}");
        assert!(bar.contains("quit"), "quit hint on the bar");
        assert!(bar.contains("help"), "help hint on the bar");
    }

    #[test]
    fn the_status_bar_reflects_the_connection_state() {
        let mut app = App::new();
        app.set_connection(ConnectionState::Ready);
        assert!(status_row(&render(&app, 80, 24)).contains("online"));
    }

    #[test]
    fn the_status_bar_names_the_current_chat_and_mode() {
        let view = view_with_one_chat("Alice", ChatKind::Private { user_id: 7 }, None);
        let app = App::with_chat_list(view);
        let bar = status_row(&render(&app, 80, 24));
        assert!(bar.contains("Alice"), "current chat: {bar:?}");
        assert!(bar.contains("chats"), "focused-pane mode");
    }

    #[test]
    fn no_toast_renders_with_an_empty_queue() {
        // The placeholder banner is the only thing in the conversation pane.
        let text = flatten(&render(&App::new(), 80, 24));
        assert!(!text.contains("Notice"), "no toast box without a notice");
    }

    #[test]
    fn a_toast_floats_over_the_panes_with_its_message() {
        let mut app = App::new();
        app.notify(Notice::info("download complete"));
        let text = flatten(&render(&app, 80, 24));
        assert!(text.contains("Notice"), "toast box title");
        assert!(text.contains("download complete"), "toast message");
    }

    #[test]
    fn a_toast_shows_how_to_dismiss_it() {
        // The box carries the dismiss affordance (#139) so the user is never left
        // wondering how to clear a notice.
        let mut app = App::new();
        app.notify(Notice::info("download complete"));
        let text = flatten(&render(&app, 80, 24));
        assert!(text.contains("Ctrl-G"), "dismiss hint on the toast");
    }

    #[test]
    fn an_error_toast_surfaces_its_code() {
        let mut app = App::new();
        app.notify(Notice::error("send", Some("FLOOD_WAIT")));
        let text = flatten(&render(&app, 80, 24));
        assert!(text.contains("send failed"), "error category");
        assert!(text.contains("FLOOD_WAIT"), "error code");
    }

    #[test]
    fn a_queued_toast_shows_a_pending_count() {
        let mut app = App::new();
        app.notify(Notice::info("first"));
        app.notify(Notice::info("second"));
        let text = flatten(&render(&app, 80, 24));
        // The front shows; the title hints at the one waiting behind it.
        assert!(text.contains("first"), "front toast");
        assert!(text.contains("+1"), "pending count");
    }
}
