//! The composer pane (#82): the message input, its reply/edit mode title, and
//! multi-row (#215) cursor + scroll handling for a draft longer than one line.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::App;
use crate::composer::ComposerMode;
use crate::keymap::Focus;

use super::common::{input_line, pane_block, truncate};

/// Hint shown in the composer while its buffer is empty.
const COMPOSER_PLACEHOLDER: &str = "type a message…";

/// Right/bottom pane: the message composer (#82). The border title is the mode
/// indicator — " Message " when composing, the reply target when replying, an edit
/// marker when editing — and the inner line is the input: a dim placeholder while
/// empty, otherwise the text with a reverse-video block marking the cursor.
pub(crate) fn render_composer(frame: &mut Frame, area: Rect, app: &App) {
    let composer = app.composer();
    let block = pane_block(
        composer_title(composer.mode()),
        app.focus() == Focus::Composer,
    );

    if composer.is_empty() {
        let line = Line::from(Span::styled(
            COMPOSER_PLACEHOLDER,
            Style::new().add_modifier(Modifier::DIM),
        ));
        frame.render_widget(Paragraph::new(line).block(block), area);
        return;
    }

    let inner = block.inner(area);
    let width = inner.width as usize;
    let text = composer.text();
    let cursor_byte = composer.cursor_byte();
    let rows = crate::wrap::layout_rows(text, width);
    let cursor_row = crate::wrap::row_of(&rows, cursor_byte);
    let lines = composer_lines(text, &rows, cursor_byte, cursor_row);
    let scroll = composer_scroll(rows.len(), inner.height as usize, cursor_row);
    frame.render_widget(Paragraph::new(lines).block(block).scroll((scroll, 0)), area);
}

/// Multi-row counterpart of [`input_line`] for the composer (#215): one
/// [`Line`] per row in `rows` (already laid out by
/// [`crate::wrap::layout_rows`]), with the reverse-video cursor cell placed
/// in `cursor_row`. Never used by the single-line fields `input_line` still
/// serves (`search/login/mediaform/settingsform/contact_picker`) — those are
/// unaffected by this addition.
fn composer_lines(
    text: &str,
    rows: &[crate::wrap::Row],
    cursor_byte: usize,
    cursor_row: usize,
) -> Vec<Line<'static>> {
    rows.iter()
        .enumerate()
        .map(|(i, row)| {
            let row_text = &text[row.start..row.end];
            if i == cursor_row {
                let row_cursor = row_text[..cursor_byte - row.start].chars().count();
                input_line(row_text, row_cursor)
            } else {
                Line::from(row_text.to_owned())
            }
        })
        .collect()
}

/// The first visible row that keeps `cursor_row` in view (#215): `0` while
/// everything fits (`total_rows <= visible_rows`), otherwise just enough
/// forward scroll that `cursor_row` becomes the last visible row. A pure
/// function of the three counts — no persisted scroll state, so it's always
/// exactly in sync with wherever the cursor moved to this render.
fn composer_scroll(total_rows: usize, visible_rows: usize, cursor_row: usize) -> u16 {
    if visible_rows == 0 || total_rows <= visible_rows {
        return 0;
    }
    let max_scroll = total_rows - visible_rows;
    cursor_row.saturating_sub(visible_rows - 1).min(max_scroll) as u16
}

/// The composer's border title, doubling as the mode indicator: a plain label when
/// composing, the reply target when replying (so the user sees which message), and
/// an edit marker when editing the prefilled buffer.
fn composer_title(mode: &ComposerMode) -> String {
    match mode {
        ComposerMode::Compose => " Message ".to_owned(),
        ComposerMode::Reply { preview, .. } => format!(" Reply ↩ {} ", truncate(preview, 40)),
        ComposerMode::Edit { .. } => " Edit ✎ ".to_owned(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // tests: panicking on a broken assumption is the point
mod tests {
    use ratatui::buffer::Buffer;

    use crate::composer::Composer;
    use crate::ui::{COMPOSER_HEIGHT, STATUS_HEIGHT, pane_layout};

    use super::super::test_support::{flatten, render, render_output};
    use super::*;

    /// A composer with `body` typed in and the cursor left at the end.
    fn typed_composer(body: &str) -> Composer {
        let mut composer = Composer::default();
        for c in body.chars() {
            composer.insert(c);
        }
        composer
    }

    /// The symbol of the reverse-video cursor cell on the composer's input row, if
    /// one is drawn. The input row sits just above the composer's bottom border.
    fn cursor_symbol(buffer: &Buffer) -> Option<String> {
        // The composer's input row is the middle of its three rows, which now sit
        // just above the one-row status bar.
        let y = buffer.area.height - STATUS_HEIGHT - COMPOSER_HEIGHT + 1;
        (0..buffer.area.width).find_map(|x| {
            let cell = &buffer[(x, y)];
            cell.modifier
                .contains(Modifier::REVERSED)
                .then(|| cell.symbol().to_owned())
        })
    }

    #[test]
    fn empty_composer_shows_the_placeholder_under_the_message_title() {
        let buffer = render(&App::with_composer(Composer::default()), 80, 24);
        let text = flatten(&buffer);
        assert!(text.contains("Message"), "compose-mode title");
        assert!(text.contains("type a message"), "empty placeholder");
        // No caret while empty — the placeholder owns the line.
        assert_eq!(cursor_symbol(&buffer), None);
    }

    #[test]
    fn typing_shows_the_text_with_the_cursor_on_the_character() {
        let mut composer = typed_composer("hi");
        // Put the caret on the 'i' so the cursor cell carries a stable symbol.
        composer.move_left();
        let buffer = render(&App::with_composer(composer), 80, 24);
        assert!(flatten(&buffer).contains("hi"), "typed text rendered");
        assert_eq!(
            cursor_symbol(&buffer).as_deref(),
            Some("i"),
            "cursor highlights the character it sits on"
        );
    }

    #[test]
    fn reply_mode_names_the_target_in_the_indicator() {
        let mut composer = Composer::default();
        composer.reply_to(7, "User 7: general kenobi".to_owned());
        let text = flatten(&render(&App::with_composer(composer), 80, 24));
        assert!(text.contains("Reply"), "reply indicator");
        assert!(
            text.contains("User 7: general kenobi"),
            "the replied-to message"
        );
    }

    #[test]
    fn edit_mode_prefills_the_buffer_and_marks_the_indicator() {
        let mut composer = Composer::default();
        composer.edit(9, "old message".to_owned());
        let buffer = render(&App::with_composer(composer), 80, 24);
        let text = flatten(&buffer);
        assert!(text.contains("Edit"), "edit indicator");
        assert!(text.contains("old message"), "prefilled buffer");
        // The prefilled buffer is non-empty, so the caret is drawn.
        assert!(
            cursor_symbol(&buffer).is_some(),
            "cursor on the prefilled text"
        );
    }

    #[test]
    fn a_long_draft_wraps_and_grows_the_composer_up_to_the_cap() {
        // A draft wider than the pane wraps onto more than one row; the
        // composer pane grows to fit it (#215).
        let width = pane_layout(Rect::new(0, 0, 80, 24), "", false)
            .composer
            .width as usize
            - 2;
        let composer = typed_composer(&"x".repeat(width * 3));
        let output = render_output(&App::with_composer(composer), 80, 24);
        assert_eq!(output.panes.composer.height, 5); // 3 rows + 2 border
    }

    #[test]
    fn a_draft_past_the_row_cap_scrolls_internally_keeping_the_cursor_in_view() {
        // 8 short logical lines, well under the row cap's width but over its
        // row count — the cursor (typed to the end) should stay in view by
        // scrolling the earlier lines out, not by growing past the cap.
        let lines: Vec<String> = (0..8).map(|i| format!("line{i}")).collect();
        let composer = typed_composer(&lines.join("\n"));
        let output = render_output(&App::with_composer(composer.clone()), 80, 24);
        assert_eq!(output.panes.composer.height, 7, "capped at 5 rows + border");

        let text = flatten(&render(&App::with_composer(composer), 80, 24));
        for i in 0..3 {
            let line = format!("line{i}");
            assert!(
                !text.contains(&line),
                "{line} should have scrolled out of view"
            );
        }
        for i in 3..8 {
            let line = format!("line{i}");
            assert!(text.contains(&line), "{line} should still be visible");
        }
    }
}
