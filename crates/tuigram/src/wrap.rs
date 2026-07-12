//! Greedy, unicode-width-aware word wrap (#214, #215): both the conversation
//! pane's message bodies and the composer's draft text need to wrap at a
//! column width, so the wrapping algorithm lives in one place and each side
//! builds its row/height math and its rendering on top of the *same* break
//! points — never re-deriving them independently, which would risk drift.
//!
//! [`wrap_breaks`] operates on one logical line at a time (the caller splits
//! on literal `\n` first — a message or a composer draft can have several).
//! Width is measured in terminal columns via [`unicode_width`], per the
//! issues' request, not full grapheme-cluster segmentation.
//!
//! `width == 0` is treated the same as `width == 1`: this module always makes
//! forward progress and never loops, even for a degenerate width. Callers
//! that want a true "don't wrap at all" behavior (e.g. `width == 0` meaning
//! "not yet measured") implement that themselves, one level up.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Byte offsets of `text` split into alternating whitespace / non-whitespace
/// runs, e.g. `"a  bc"` -> `[(0,1), (1,3), (3,5)]`. Every byte of `text`
/// belongs to exactly one run, so re-joining the runs reconstructs `text`
/// exactly — [`wrap_breaks`] only ever chooses a run boundary as a break
/// point, never drops a byte.
fn runs(text: &str) -> Vec<(usize, usize)> {
    let mut runs = Vec::new();
    let mut chars = text.char_indices().peekable();
    while let Some(&(start, c)) = chars.peek() {
        let is_ws = c.is_whitespace();
        let mut end = start + c.len_utf8();
        chars.next();
        while let Some(&(idx, c2)) = chars.peek() {
            if c2.is_whitespace() != is_ws {
                break;
            }
            end = idx + c2.len_utf8();
            chars.next();
        }
        runs.push((start, end));
    }
    runs
}

/// Hard-break a single "word" run (`word`, starting at byte offset
/// `word_start` in the original text) that doesn't fit `width` even on a
/// fresh row, character by character. Pushes interior break points into
/// `breaks` and returns the display-column width consumed by the trailing
/// (possibly still over-wide) partial row, so the caller can keep accounting
/// for whatever comes after this word on the same logical line.
///
/// Always places at least one character per row before considering another
/// break, even if that character's own width exceeds `width` — this is what
/// guarantees forward progress for `width == 0`/`1` against a wide (CJK/emoji)
/// character.
fn hard_break_word(word: &str, width: usize, word_start: usize, breaks: &mut Vec<usize>) -> usize {
    let mut col = 0usize;
    for (idx, c) in word.char_indices() {
        let w = UnicodeWidthChar::width(c).unwrap_or(0);
        if col > 0 && col + w > width {
            breaks.push(word_start + idx);
            col = 0;
        }
        col += w;
    }
    col
}

/// Byte offsets where a greedy word-wrap of one logical line of `text` (no
/// embedded `\n`) breaks into rows of at most `width` display columns.
/// Always starts with `0`; `len() == 1` means the line fits on one row
/// unwrapped.
///
/// Whitespace runs are never themselves a break point — they're folded into
/// whichever row they land on (trailing whitespace past `width` is invisible
/// once rendered into a fixed-width area, so letting a row run slightly wide
/// on whitespace alone is harmless and simpler than trimming it). Only a
/// "word" (non-whitespace run) that would overflow the current row triggers
/// a break, at the word's start. A word wider than `width` all by itself is
/// hard-broken character by character via [`hard_break_word`].
pub(crate) fn wrap_breaks(text: &str, width: usize) -> Vec<usize> {
    if text.is_empty() {
        return vec![0];
    }
    let width = width.max(1);
    let mut breaks = vec![0usize];
    let mut col = 0usize;

    for (start, end) in runs(text) {
        let run = &text[start..end];
        let is_ws = run.starts_with(char::is_whitespace);
        let run_width = UnicodeWidthStr::width(run);

        if is_ws {
            col += run_width;
            continue;
        }

        if col + run_width <= width {
            col += run_width;
        } else if run_width <= width {
            // Doesn't fit on the current row, but fits on a fresh one.
            breaks.push(start);
            col = run_width;
        } else {
            // Doesn't fit even alone: start a fresh row (if we weren't
            // already on one) and hard-break it character by character.
            if col > 0 {
                breaks.push(start);
            }
            col = hard_break_word(run, width, start, &mut breaks);
        }
    }

    breaks
}

/// Number of rows [`wrap_breaks`] would wrap `text` into at `width` — used
/// for height math where the wrapped content itself isn't needed.
pub(crate) fn row_count(text: &str, width: usize) -> usize {
    wrap_breaks(text, width).len()
}

/// A visual row's byte span within (possibly multi-line) text, half-open
/// (`start..end`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Row {
    pub start: usize,
    pub end: usize,
}

/// Visual rows for `text` (which may contain `'\n'`) wrapped at `width`
/// display columns — the composer's row/cursor geometry (#215), built on the
/// same [`wrap_breaks`] the conversation pane uses so both stay in lockstep.
///
/// Splits on `'\n'` first, then windows each logical line's [`wrap_breaks`]
/// output into spans the same way `ui.rs`'s `content_lines`/`text_lines`
/// slice `line[row_start..row_end]` — the break list always starts at `0`
/// and the last break's row extends to the line's end.
///
/// `width == 0` is the existing "not yet measured" sentinel (see
/// `conversation::content_rows`): one unwrapped `Row` per logical line.
///
/// Always returns at least one `Row` — empty text yields a single empty row.
pub(crate) fn layout_rows(text: &str, width: usize) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut line_start = 0usize;
    for line in text.split('\n') {
        if width == 0 {
            rows.push(Row {
                start: line_start,
                end: line_start + line.len(),
            });
        } else {
            let breaks = wrap_breaks(line, width);
            for (i, &row_start) in breaks.iter().enumerate() {
                let row_end = breaks.get(i + 1).copied().unwrap_or(line.len());
                rows.push(Row {
                    start: line_start + row_start,
                    end: line_start + row_end,
                });
            }
        }
        line_start += line.len() + 1; // +1 for the '\n' consumed by split
    }
    rows
}

/// The index into `rows` containing byte offset `pos`.
///
/// Seam rule, shared by rendering and vertical navigation so they can never
/// disagree: a position exactly at a row's `end` advances to the next row
/// only when the two rows are contiguous (`rows[i + 1].start == rows[i].end`
/// — a plain *wrap* seam with no literal `'\n'` consumed between them, so the
/// text continues immediately into the next row). When a literal `'\n'` was
/// consumed between them (`rows[i + 1].start > rows[i].end`), the position
/// stays on row `i` — the cursor visually sits at the end of that logical
/// line, not wrapped onto the next one. The true end of the text always
/// belongs to the last row.
pub(crate) fn row_of(rows: &[Row], pos: usize) -> usize {
    let last = rows.len() - 1;
    let mut i = 0;
    while i < last {
        let seamless = rows[i + 1].start == rows[i].end;
        if pos < rows[i].end || (pos == rows[i].end && !seamless) {
            return i;
        }
        i += 1;
    }
    last
}

/// Display-column width of `row_text[..byte_offset]`. `byte_offset` must
/// land on a char boundary within `row_text`.
pub(crate) fn display_col(row_text: &str, byte_offset: usize) -> usize {
    UnicodeWidthStr::width(&row_text[..byte_offset])
}

/// The inverse of [`display_col`]: the byte offset in `row_text` whose
/// display column is closest to `target_col` without exceeding it — snapping
/// to the left edge of a wide character that straddles the target column.
/// Clamped to `row_text.len()`.
///
/// Shared by vertical navigation (`target_col` = a remembered goal column)
/// and mouse click (`target_col` = the clicked terminal cell column) so wide
/// (CJK/emoji) characters resolve identically for both.
pub(crate) fn byte_for_col(row_text: &str, target_col: usize) -> usize {
    let mut col = 0usize;
    for (idx, c) in row_text.char_indices() {
        let w = UnicodeWidthChar::width(c).unwrap_or(0);
        if col + w > target_col {
            return idx;
        }
        col += w;
    }
    row_text.len()
}

/// The absolute byte offset in `text` for `target_col` on `rows[row]`,
/// guaranteed to resolve back to `row` under [`row_of`] (#215).
///
/// [`byte_for_col`] alone can return a row's raw length — landing exactly at
/// its end — which, for a row that continues *seamlessly* (no literal
/// `'\n'`) into `rows[row + 1]`, is the same byte value [`row_of`] assigns to
/// that next row: [`move_up`](crate::composer::Composer::move_up)/
/// [`move_down`](crate::composer::Composer::move_down) or a click landing
/// there would silently resolve onto the row below instead, discarding the
/// move (up could get permanently stuck a full-width row short of the top).
/// This backs off to the last char boundary strictly inside the row in that
/// one case — trading a column of precision at that exact edge for
/// navigation that never gets stuck. Every other case (a real newline, or
/// the true last row) is never ambiguous and is returned unchanged.
pub(crate) fn resolve_in_row(text: &str, rows: &[Row], row: usize, target_col: usize) -> usize {
    let span = rows[row];
    let row_text = &text[span.start..span.end];
    let offset = byte_for_col(row_text, target_col);
    let seamless_next = rows.get(row + 1).is_some_and(|next| next.start == span.end);
    let offset = if seamless_next && offset == row_text.len() {
        row_text
            .char_indices()
            .last()
            .map_or(offset, |(prev, _)| prev)
    } else {
        offset
    };
    span.start + offset
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_text_is_a_single_empty_row() {
        assert_eq!(wrap_breaks("", 10), vec![0]);
        assert_eq!(row_count("", 10), 1);
    }

    #[test]
    fn text_that_fits_is_a_single_row() {
        assert_eq!(wrap_breaks("hello world", 20), vec![0]);
        assert_eq!(row_count("hello world", 20), 1);
    }

    #[test]
    fn wraps_at_word_boundaries() {
        let breaks = wrap_breaks("hello world foo", 5);
        // "hello" (5) | "world" (5, leading space folded onto row 1) | "foo"
        assert_eq!(breaks.len(), 3);
        for w in breaks.windows(2) {
            assert!(w[0] < w[1]);
        }
    }

    #[test]
    fn a_word_longer_than_width_hard_breaks_by_character() {
        let text = "abcdefgh";
        let breaks = wrap_breaks(text, 3);
        assert_eq!(breaks, vec![0, 3, 6]);
        assert_eq!(row_count(text, 3), 3);
    }

    #[test]
    fn width_zero_and_one_terminate_and_make_progress() {
        // Must not loop; every row must contain at least one character.
        for width in [0usize, 1] {
            let breaks = wrap_breaks("hello world", width);
            assert_eq!(breaks.len(), row_count("hello world", width));
            assert!(breaks.windows(2).all(|w| w[0] < w[1]));
        }
    }

    #[test]
    fn a_single_wide_char_wider_than_width_still_makes_progress() {
        // 中 is width 2; at width 1 each char must still land on its own row
        // rather than looping forever.
        let breaks = wrap_breaks("中中中", 1);
        assert_eq!(breaks.len(), 3);
        assert_eq!(row_count("中中中", 1), 3);
    }

    #[test]
    fn wide_cjk_characters_wrap_by_display_column_not_char_count() {
        // Each 中 is 2 columns wide; width 4 fits exactly two per row.
        let breaks = wrap_breaks("中中中中", 4);
        assert_eq!(breaks.len(), 2);
    }

    #[test]
    fn emoji_width_is_accounted_for() {
        // 😀 is width 2 (per unicode-width); three of them need width >= 6
        // to fit on one row.
        assert_eq!(row_count("😀😀😀", 6), 1);
        assert_eq!(row_count("😀😀😀", 5), 2);
    }

    #[test]
    fn layout_rows_of_empty_text_is_a_single_empty_row() {
        assert_eq!(layout_rows("", 10), vec![Row { start: 0, end: 0 }]);
    }

    #[test]
    fn layout_rows_splits_on_newlines_before_wrapping_each_line() {
        let rows = layout_rows("ab\ncd", 10);
        assert_eq!(
            rows,
            vec![Row { start: 0, end: 2 }, Row { start: 3, end: 5 }]
        );
    }

    #[test]
    fn layout_rows_wraps_a_long_line_into_contiguous_rows() {
        // "abcde" at width 3 wraps into "abc" | "de" — contiguous, no gap.
        let rows = layout_rows("abcde", 3);
        assert_eq!(
            rows,
            vec![Row { start: 0, end: 3 }, Row { start: 3, end: 5 }]
        );
    }

    #[test]
    fn layout_rows_width_zero_is_one_unwrapped_row_per_logical_line() {
        let rows = layout_rows("hello world\nfoo", 0);
        assert_eq!(
            rows,
            vec![Row { start: 0, end: 11 }, Row { start: 12, end: 15 },]
        );
    }

    #[test]
    fn row_of_a_wrap_seam_belongs_to_the_next_row() {
        // "abcde" @ width 3 -> rows [0,3) "abc", [3,5) "de", contiguous.
        let rows = layout_rows("abcde", 3);
        assert_eq!(row_of(&rows, 2), 0); // mid-row
        assert_eq!(row_of(&rows, 3), 1); // exactly at the seam
        assert_eq!(row_of(&rows, 5), 1); // true end of text
    }

    #[test]
    fn row_of_a_real_newline_seam_stays_on_the_current_row() {
        // "ab\ncd" -> rows [0,2) "ab", [3,5) "cd" — a gap for the '\n'.
        let rows = layout_rows("ab\ncd", 10);
        assert_eq!(row_of(&rows, 2), 0); // right before the '\n': end of row 0
        assert_eq!(row_of(&rows, 3), 1); // right after the '\n': start of row 1
        assert_eq!(row_of(&rows, 5), 1); // true end of text
    }

    #[test]
    fn row_of_a_single_row_is_always_zero() {
        let rows = layout_rows("hi", 10);
        assert_eq!(row_of(&rows, 0), 0);
        assert_eq!(row_of(&rows, 2), 0);
    }

    #[test]
    fn display_col_and_byte_for_col_round_trip_ascii() {
        assert_eq!(display_col("hello", 3), 3);
        assert_eq!(byte_for_col("hello", 3), 3);
    }

    #[test]
    fn display_col_and_byte_for_col_round_trip_through_wide_emoji() {
        // 😀 is width 2 in unicode-width and 4 bytes in UTF-8.
        let text = "a😀b";
        assert_eq!(display_col(text, 0), 0); // before 'a'
        assert_eq!(display_col(text, 1), 1); // before 😀
        assert_eq!(display_col(text, 5), 3); // before 'b' (1 + 2)
        assert_eq!(display_col(text, 6), 4); // end of text

        assert_eq!(byte_for_col(text, 0), 0);
        assert_eq!(byte_for_col(text, 1), 1);
        assert_eq!(byte_for_col(text, 3), 5); // exactly at 'b'
        assert_eq!(byte_for_col(text, 4), 6); // past the end -> clamped
    }

    #[test]
    fn byte_for_col_snaps_to_the_left_edge_of_a_straddled_wide_char() {
        // 😀 spans columns 1-2; a target column of 2 lands inside it, so the
        // byte offset snaps back to its start rather than splitting it.
        let text = "a😀b";
        assert_eq!(byte_for_col(text, 2), 1);
    }

    #[test]
    fn display_col_and_byte_for_col_round_trip_through_wide_cjk() {
        // 中 is width 2 and 3 bytes in UTF-8.
        let text = "中中";
        assert_eq!(display_col(text, 3), 2);
        assert_eq!(byte_for_col(text, 2), 3);
        assert_eq!(byte_for_col(text, 4), 6); // past the end -> clamped
    }

    #[test]
    fn resolve_in_row_backs_off_a_full_width_seamless_seam_so_it_stays_on_that_row() {
        // "abcdef" @ width 3 hard-breaks into "abc" | "def", contiguous (no
        // '\n'). Landing at column 3 of row 0 would be byte 3 — but that's
        // also row 1's start, so plain `byte_for_col` would put the cursor
        // right back where `row_of` reports it as row 1, exactly the bug a
        // repeated Up should never hit (it would look stuck).
        let text = "abcdef";
        let rows = layout_rows(text, 3);
        assert_eq!(
            rows,
            vec![Row { start: 0, end: 3 }, Row { start: 3, end: 6 }]
        );

        let landed = resolve_in_row(text, &rows, 0, 3);
        assert_eq!(landed, 2, "backs off one char from the ambiguous seam");
        assert_eq!(row_of(&rows, landed), 0, "stays resolvable to row 0");
    }

    #[test]
    fn resolve_in_row_does_not_back_off_the_true_last_row() {
        // Row 1 here has no row after it, so landing at its full end (the
        // true end of the text) is never ambiguous — must not be touched.
        let text = "abcdef";
        let rows = layout_rows(text, 3);
        let landed = resolve_in_row(text, &rows, 1, 10); // clamps well past "def"
        assert_eq!(landed, 6);
        assert_eq!(row_of(&rows, landed), 1);
    }

    #[test]
    fn resolve_in_row_does_not_back_off_a_real_newline_seam() {
        // "ab" | "cd" separated by a literal '\n' — row 0's end is never
        // ambiguous with row 1's start (there's a byte gap for the '\n'), so
        // landing at its raw end is already unambiguous and untouched.
        let text = "ab\ncd";
        let rows = layout_rows(text, 10);
        let landed = resolve_in_row(text, &rows, 0, 10); // clamps past "ab"
        assert_eq!(landed, 2);
        assert_eq!(row_of(&rows, landed), 0);
    }

    #[test]
    fn resolve_in_row_leaves_a_short_landing_within_the_row_untouched() {
        // A target column that lands strictly inside the row needs no
        // adjustment at all.
        let text = "abcdef";
        let rows = layout_rows(text, 3);
        assert_eq!(resolve_in_row(text, &rows, 0, 1), 1);
    }
}
