//! Rendering primitives shared across more than one pane or overlay: the
//! bordered pane frame, the modal popup rect, the cursor-aware input line, the
//! dim hint line, and text truncation.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Block;

/// Marker drawn to the left of the selected chat row.
pub(super) const SELECTED_SYMBOL: &str = "▶ ";

/// Marker prefixed to the focused pane's border title.
pub(super) const FOCUS_MARKER: &str = "●";

/// A pane's bordered block, with the focus highlight applied when `focused`: a
/// marker prefixed to the title and a bold border, so the active pane is obvious.
pub(super) fn pane_block(title: String, focused: bool) -> Block<'static> {
    let block = Block::bordered();
    if focused {
        block
            .title(format!("{FOCUS_MARKER}{title}"))
            .border_style(Style::new().add_modifier(Modifier::BOLD))
    } else {
        block.title(title)
    }
}

/// The input line with a visible cursor: the text up to the cursor, then the
/// character under it (or a trailing space at end-of-line) drawn reverse-video so
/// the caret shows in the `TestBackend` buffer, then the remainder. Shared with the
/// login screens (#86) so every text field renders its caret identically.
pub(crate) fn input_line(text: &str, cursor: usize) -> Line<'static> {
    let chars: Vec<char> = text.chars().collect();
    let cursor = cursor.min(chars.len());
    let cursor_style = Style::new().add_modifier(Modifier::REVERSED);

    let left: String = chars[..cursor].iter().collect();
    let mut spans = vec![Span::raw(left)];
    match chars.get(cursor) {
        Some(&c) => {
            spans.push(Span::styled(c.to_string(), cursor_style));
            let right: String = chars[cursor + 1..].iter().collect();
            if !right.is_empty() {
                spans.push(Span::raw(right));
            }
        }
        None => spans.push(Span::styled(" ".to_owned(), cursor_style)),
    }
    Line::from(spans)
}

/// A `width × height` rectangle centred within `area`, clamped to fit it.
pub(super) fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height.saturating_sub(h) / 2,
        width: w,
        height: h,
    }
}

/// A dim hint line, for the key reminder along the bottom of a modal (and the
/// login screens, #86).
pub(crate) fn hint_line(hint: &'static str) -> Line<'static> {
    Line::from(Span::styled(hint, Style::new().add_modifier(Modifier::DIM)))
}

/// Shorten `s` to at most `max` characters, ending in an ellipsis when clipped, so
/// a long reply preview cannot overrun the composer's border title.
pub(super) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let head: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}
