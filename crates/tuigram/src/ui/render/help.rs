//! The help overlay: a scrollable cheatsheet of the active key bindings,
//! generated straight from the keymap.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::app::App;
use crate::keymap;

use super::common::{centered_rect, hint_line};

/// The help overlay: a centred, bordered popup listing the active key bindings,
/// generated from the keymap so it always matches what the keys actually do. On a
/// terminal too short to show every binding the body scrolls (`app.help_scroll`),
/// with a fixed hint row along the bottom; the border and hint stay put while the
/// bindings slide under them.
pub(crate) fn render_help(frame: &mut Frame, area: Rect, app: &App) {
    let lines = help_lines();
    let content_width = lines.iter().map(Line::width).max().unwrap_or(0) as u16;
    // Border (2) + the hint row (1) frame the scrollable body; `centered_rect` clamps
    // the height to the terminal, so a tall cheatsheet becomes a scroll viewport.
    let popup = centered_rect(content_width + 4, lines.len() as u16 + 3, area);
    // `Clear` wipes the panes underneath so the overlay reads as a modal.
    frame.render_widget(Clear, popup);

    let block = Block::bordered()
        .title(" Help ")
        .title_alignment(Alignment::Center);
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let [body_area, hint_area] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(inner);

    // `scroll` offsets the body; ratatui clips it to `body_area`, so the offset picks
    // the first visible binding line. The offset is already clamped to the last line
    // by the reducer.
    frame.render_widget(
        Paragraph::new(lines).scroll((app.help_scroll(), 0)),
        body_area,
    );
    frame.render_widget(
        Paragraph::new(hint_line("j / k scroll · ? / q / Esc close")),
        hint_area,
    );
}

/// The help overlay's body, one block per [`keymap::HelpSection`]: a bold heading
/// then each binding's keys and description.
fn help_lines() -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for (i, section) in keymap::help_sections().into_iter().enumerate() {
        if i > 0 {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(Span::styled(
            section.title,
            Style::new().add_modifier(Modifier::BOLD),
        )));
        for entry in section.entries {
            lines.push(Line::from(format!(
                "  {:<13}{}",
                entry.keys, entry.description
            )));
        }
    }
    lines
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // tests: panicking on a broken assumption is the point
mod tests {
    use super::super::test_support::{flatten, render};
    use super::*;

    #[test]
    fn the_help_overlay_lists_bindings_when_toggled() {
        let mut app = App::new();
        assert!(
            !flatten(&render(&app, 80, 24)).contains(" Help "),
            "no overlay until toggled"
        );
        app.dispatch(crate::app::Action::ToggleHelp);
        let text = flatten(&render(&app, 80, 24));
        assert!(text.contains("Help"), "overlay title");
        assert!(text.contains("quit"), "documents a binding");
        assert!(
            text.contains("focus next pane"),
            "documents focus switching"
        );
        assert!(text.contains("scroll"), "the footer hints the scroll keys");
    }

    #[test]
    fn help_line_count_matches_the_rendered_body() {
        // The scroll clamp keys off `keymap::help_line_count`; it must track the lines
        // `help_lines` actually produces, or a scroll could stop short or overrun.
        assert_eq!(keymap::help_line_count(), help_lines().len());
    }

    #[test]
    fn a_short_terminal_scrolls_the_help_body() {
        let mut app = App::new();
        app.dispatch(crate::app::Action::ToggleHelp);
        // A terminal too short to show the whole cheatsheet: the first section shows
        // at the top, the last does not.
        let top = flatten(&render(&app, 80, 10));
        assert!(top.contains("Global"), "the first section is at the top");
        assert!(
            !top.contains("Chat list & history"),
            "the last section is off-screen"
        );
        // Scroll down enough to move the first section out of the viewport; the
        // overlay stays open (scrolling never closes it) and the footer hint is fixed.
        for _ in 0..7 {
            app.dispatch(crate::app::Action::HelpScrollDown);
        }
        assert!(app.help_visible(), "scrolling keeps the overlay open");
        let scrolled = flatten(&render(&app, 80, 10));
        assert!(
            !scrolled.contains("Global"),
            "the first section scrolled out of view"
        );
        assert!(
            scrolled.contains("scroll"),
            "the footer hint stays put while the body scrolls"
        );
    }
}
