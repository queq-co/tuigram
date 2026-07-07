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
}
