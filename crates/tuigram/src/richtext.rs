//! Render a [`FormattedText`]'s entities (#211): `TDLib` carries a formatting
//! entity list end to end (bold, italic, code, spoiler, …) but until now the
//! conversation pane dropped it and rendered plain text. This module maps
//! entity ranges onto ratatui [`Span`]s.
//!
//! `TDLib` reports entity `offset`/`length` in **UTF-16 code units**, not bytes
//! or chars, so [`utf16_offset_to_byte`] converts before slicing the `&str`.
//! Entities can nest or overlap (e.g. a link inside bold text), so
//! [`styled_spans`] composes them by boundary rather than walking one entity
//! at a time — every entity covering a segment contributes to that segment's
//! style, instead of a later entity silently overwriting an earlier one.
//!
//! Spoilers depend on UI selection state, not the message itself: concealed
//! (block glyphs) while the message is not selected, revealed while it is —
//! so [`styled_spans`] takes that as a plain `bool`, not something cached
//! on the model.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use tuigram_core::model::{EntityKind, TextEntity};

/// The glyph a concealed spoiler segment renders as, one per hidden character
/// (so the concealed run keeps the original text's display width).
const SPOILER_GLYPH: char = '▒';

/// Convert a `TDLib` UTF-16 code-unit offset into `s` to a byte offset, for
/// slicing. Clamps to `s.len()` if `utf16_offset` runs past the string's
/// UTF-16 length (a defensively-tolerant read of a value `TDLib` guarantees is
/// in range, but which a mid-string clip must not panic on).
fn utf16_offset_to_byte(s: &str, utf16_offset: usize) -> usize {
    let mut utf16_count = 0usize;
    let mut byte_count = 0usize;
    for c in s.chars() {
        if utf16_count >= utf16_offset {
            break;
        }
        utf16_count += c.len_utf16();
        byte_count += c.len_utf8();
    }
    byte_count
}

/// One entity's byte range within `text`, after UTF-16 conversion — clamped
/// to `text`'s bounds and dropped if it collapses to empty.
fn byte_range(text: &str, entity: &TextEntity) -> Option<(usize, usize)> {
    let start = utf16_offset_to_byte(text, entity.offset.max(0) as usize);
    let end = utf16_offset_to_byte(text, (entity.offset.max(0) + entity.length.max(0)) as usize);
    (start < end).then_some((start, end))
}

/// The style a single entity contributes. Composed (via [`Style::patch`]) with
/// every other entity covering the same segment in [`styled_spans`], so
/// overlapping entities (e.g. bold + a link) both apply.
fn style_for(kind: &EntityKind) -> Style {
    match kind {
        EntityKind::Bold => Style::new().add_modifier(Modifier::BOLD),
        EntityKind::Italic => Style::new().add_modifier(Modifier::ITALIC),
        EntityKind::Underline => Style::new().add_modifier(Modifier::UNDERLINED),
        EntityKind::Strikethrough => Style::new().add_modifier(Modifier::CROSSED_OUT),
        EntityKind::Code | EntityKind::Pre | EntityKind::PreCode { .. } => {
            Style::new().fg(Color::Yellow)
        }
        EntityKind::TextUrl { .. } | EntityKind::Url | EntityKind::EmailAddress => {
            Style::new().add_modifier(Modifier::UNDERLINED)
        }
        // Spoiler is handled separately in `styled_spans` (it changes the
        // *text*, not just the style); every other entity kind is a plain,
        // unstyled span — auto-detected spans like mentions/hashtags render
        // as ordinary text, styling them is out of #211's scope.
        _ => Style::new(),
    }
}

/// Split `text` into styled spans per its `entities` (already the whole
/// [`FormattedText`], or a line-local slice with re-offset entities — see
/// `ui::text_lines`). `selected` gates spoiler reveal: concealed with
/// [`SPOILER_GLYPH`] blocks when `false`, shown as plain (dim-marked) text
/// when `true`.
///
/// Boundary-based composition: every entity's start/end becomes a cut point,
/// so a segment between two cuts is covered by a consistent, well-defined set
/// of entities — nested and overlapping entities (a link inside bold text)
/// both apply, rather than one silently winning.
pub fn styled_spans(text: &str, entities: &[TextEntity], selected: bool) -> Vec<Span<'static>> {
    if text.is_empty() {
        return vec![Span::raw(String::new())];
    }

    let ranges: Vec<(usize, usize, &EntityKind)> = entities
        .iter()
        .filter_map(|e| byte_range(text, e).map(|(start, end)| (start, end, &e.kind)))
        .collect();

    let mut bounds: Vec<usize> = std::iter::once(0)
        .chain(std::iter::once(text.len()))
        .chain(ranges.iter().flat_map(|(start, end, _)| [*start, *end]))
        .collect();
    bounds.sort_unstable();
    bounds.dedup();

    let mut spans = Vec::with_capacity(bounds.len().saturating_sub(1));
    for pair in bounds.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        if a >= b {
            continue;
        }
        let covering: Vec<&EntityKind> = ranges
            .iter()
            .filter(|(start, end, _)| *start <= a && *end >= b)
            .map(|(_, _, kind)| *kind)
            .collect();
        let segment = &text[a..b];
        let is_spoiler = covering.iter().any(|k| matches!(k, EntityKind::Spoiler));
        let mut style = covering
            .iter()
            .fold(Style::new(), |style, kind| style.patch(style_for(kind)));

        if is_spoiler && !selected {
            let glyphs: String =
                std::iter::repeat_n(SPOILER_GLYPH, segment.chars().count()).collect();
            spans.push(Span::styled(glyphs, style.fg(Color::DarkGray)));
        } else {
            if is_spoiler {
                // Revealed: mark it distinctly so a selected spoiler still
                // reads as "this was hidden", not indistinguishable prose.
                style = style.add_modifier(Modifier::DIM);
            }
            spans.push(Span::styled(segment.to_owned(), style));
        }
    }
    if spans.is_empty() {
        spans.push(Span::raw(String::new()));
    }
    spans
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // tests: panicking on a broken assumption is the point
mod tests {
    use super::*;
    use tuigram_core::model::TextEntity;

    fn entity(offset: i32, length: i32, kind: EntityKind) -> TextEntity {
        TextEntity {
            offset,
            length,
            kind,
        }
    }

    #[test]
    fn utf16_offset_to_byte_handles_non_bmp_emoji() {
        // 😀 is one `char` but two UTF-16 code units and four UTF-8 bytes; a
        // char-indexed mapper would stop one code unit short, a byte-indexed
        // one would overshoot.
        let s = "😀x";
        assert_eq!(utf16_offset_to_byte(s, 0), 0);
        assert_eq!(utf16_offset_to_byte(s, 2), 4); // past the emoji, at 'x'
        assert_eq!(utf16_offset_to_byte(s, 3), 5); // past 'x', at the end
    }

    #[test]
    fn utf16_offset_to_byte_handles_cjk() {
        // 中 is one UTF-16 code unit but three UTF-8 bytes; a byte-indexed
        // mapper would overshoot immediately.
        let s = "中x";
        assert_eq!(utf16_offset_to_byte(s, 0), 0);
        assert_eq!(utf16_offset_to_byte(s, 1), 3); // past 中, at 'x'
        assert_eq!(utf16_offset_to_byte(s, 2), 4);
    }

    #[test]
    fn utf16_offset_to_byte_clamps_past_the_end() {
        assert_eq!(utf16_offset_to_byte("hi", 99), 2);
    }

    #[test]
    fn plain_text_with_no_entities_is_one_unstyled_span() {
        let spans = styled_spans("hello", &[], false);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "hello");
        assert_eq!(spans[0].style, Style::default());
    }

    #[test]
    fn a_single_entity_styles_only_its_range() {
        let entities = [entity(0, 4, EntityKind::Bold)];
        let spans = styled_spans("bold rest", &entities, false);
        assert_eq!(spans[0].content, "bold");
        assert!(spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(spans[1].content, " rest");
        assert_eq!(spans[1].style, Style::default());
    }

    #[test]
    fn overlapping_entities_both_apply_to_their_shared_segment() {
        // "bold link" — bold covers the whole string, a link covers "link".
        let entities = [
            entity(0, 9, EntityKind::Bold),
            entity(
                5,
                4,
                EntityKind::TextUrl {
                    url: "https://example.com".to_owned(),
                },
            ),
        ];
        let spans = styled_spans("bold link", &entities, false);
        // "bold " is bold only; "link" is bold *and* underlined.
        let bold_only = spans.iter().find(|s| s.content == "bold ").unwrap();
        assert!(bold_only.style.add_modifier.contains(Modifier::BOLD));
        assert!(!bold_only.style.add_modifier.contains(Modifier::UNDERLINED));

        let bold_and_link = spans.iter().find(|s| s.content == "link").unwrap();
        assert!(bold_and_link.style.add_modifier.contains(Modifier::BOLD));
        assert!(
            bold_and_link
                .style
                .add_modifier
                .contains(Modifier::UNDERLINED)
        );
    }

    #[test]
    fn spoiler_conceals_when_not_selected_and_reveals_when_selected() {
        let entities = [entity(0, 6, EntityKind::Spoiler)];
        let concealed = styled_spans("secret", &entities, false);
        assert_eq!(concealed[0].content, SPOILER_GLYPH.to_string().repeat(6));

        let revealed = styled_spans("secret", &entities, true);
        assert_eq!(revealed[0].content, "secret");
    }

    #[test]
    fn emoji_heavy_text_styles_the_correct_utf16_range() {
        // "😀😀bold" — bold entity offset 4 (past both emoji, 2 UTF-16 units
        // each), length 4.
        let entities = [entity(4, 4, EntityKind::Bold)];
        let spans = styled_spans("😀😀bold", &entities, false);
        let bold = spans.iter().find(|s| s.content == "bold").unwrap();
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
    }
}
