//! The composer view-model: the editable input line the bottom pane renders from.
//!
//! Unlike the chat-list and conversation views (projections of a core store), the
//! composer is local UI state the user owns directly: the text being typed, the
//! cursor within it, and which *mode* the input is in — composing a new message,
//! replying to one, or editing one. Phase 6 routes a submitted buffer to core
//! ([`Client::send_message`]/edit); Phase 5 leaves the send a no-op that just
//! consumes the buffer, so the editing and mode behaviour is exercised headlessly
//! today.
//!
//! The editable buffer and cursor live in a shared [`TextInput`] primitive (cursor
//! math is character-indexed there, so editing stays correct across multi-byte
//! input); this module adds the messaging *mode* on top — compose, reply, or edit —
//! and the submit/cancel semantics those imply.
//!
//! [`Client::send_message`]: tuigram_core::Client

use crate::textinput::TextInput;

/// What the composer is currently acting on. The mode drives the pane's indicator
/// and, on submit, whether the buffer becomes a new message, a reply, or an edit
/// (#116).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ComposerMode {
    /// Writing a new message in the open chat.
    #[default]
    Compose,
    /// Replying to a specific message. `preview` is a short label of the target
    /// (built by the caller from the message), shown in the pane indicator so the
    /// user sees what they are replying to.
    Reply {
        /// The message being replied to — the reply's `reply_to` on submit (#116).
        message_id: i64,
        preview: String,
    },
    /// Editing a message already sent. The buffer is pre-filled with its current
    /// text; submitting replaces it.
    Edit {
        /// The message being edited — the edit target on submit (#116).
        message_id: i64,
    },
}

/// A submitted composer buffer and what core should do with it (#116). [`submit`]
/// resolves the [mode](ComposerMode) into one of these so the loop can route it to
/// the matching seam without re-reading the composer's state; the text is always
/// non-empty (an empty submit is a no-op).
///
/// [`submit`]: Composer::submit
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Submission {
    /// Send `text` as a new message in the open chat.
    Send {
        /// The message body.
        text: String,
    },
    /// Send `text` as a reply to `reply_to` in the open chat.
    Reply {
        /// The message being replied to.
        reply_to: i64,
        /// The reply body.
        text: String,
    },
    /// Replace message `message_id`'s text with `text`.
    Edit {
        /// The message being edited.
        message_id: i64,
        /// The replacement text.
        text: String,
    },
}

/// The composer pane's state: the input buffer (a shared [`TextInput`]) and the
/// current [mode](ComposerMode). Empty and in [`Compose`](ComposerMode::Compose)
/// by default — the pre-data Phase 5 state.
#[derive(Debug, Clone, Default)]
pub struct Composer {
    /// The editable buffer and cursor.
    input: TextInput,
    /// What a submit will do, and what the indicator shows.
    mode: ComposerMode,
    /// The display column [`move_up`](Self::move_up)/[`move_down`](Self::move_down)
    /// are steering toward, remembered across a run of consecutive vertical
    /// moves so crossing a shorter row and back doesn't drift the cursor's
    /// horizontal position (#215) — standard editor behavior. `None` means no
    /// vertical move is in progress; reset by every other mutator below so a
    /// stale goal never survives an edit or a horizontal/click move.
    goal_col: Option<usize>,
}

impl Composer {
    /// The current input text.
    #[must_use]
    pub fn text(&self) -> &str {
        self.input.text()
    }

    /// The cursor position, as a character index in `0..=chars`. Rendering and
    /// row-aware movement read [`cursor_byte`](Self::cursor_byte) instead
    /// (#215); this plain char-index form is test-only, for asserting cursor
    /// state directly.
    #[cfg(test)]
    #[must_use]
    pub fn cursor(&self) -> usize {
        self.input.cursor()
    }

    /// The cursor position, as a byte offset into [`text`](Self::text) — the
    /// unit [`crate::wrap`]'s row geometry operates in, needed by the
    /// composer's multi-row renderer (#215).
    #[must_use]
    pub fn cursor_byte(&self) -> usize {
        self.input.byte_at(self.input.cursor())
    }

    /// The current mode (compose, reply, or edit).
    #[must_use]
    pub fn mode(&self) -> &ComposerMode {
        &self.mode
    }

    /// Whether the input buffer is empty (drives the placeholder vs. the cursor).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.input.is_empty()
    }

    /// Insert a character at the cursor and step the cursor past it.
    pub fn insert(&mut self, c: char) {
        self.input.insert(c);
        self.goal_col = None;
    }

    /// Delete the character before the cursor (Backspace). A no-op at the start.
    pub fn backspace(&mut self) {
        self.input.backspace();
        self.goal_col = None;
    }

    /// Move the cursor one character left, clamping at the start.
    pub fn move_left(&mut self) {
        self.input.move_left();
        self.goal_col = None;
    }

    /// Move the cursor one character right, clamping at the end.
    pub fn move_right(&mut self) {
        self.input.move_right();
        self.goal_col = None;
    }

    /// Move the cursor to the start of the current logical line (Home) — the
    /// nearest embedded newline at or before the cursor, or the buffer start
    /// (#215).
    pub fn move_home(&mut self) {
        self.input.move_line_home();
        self.goal_col = None;
    }

    /// Move the cursor to the end of the current logical line (End) — the
    /// nearest embedded newline at or after the cursor, or the buffer end
    /// (#215).
    pub fn move_end(&mut self) {
        self.input.move_line_end();
        self.goal_col = None;
    }

    /// Move the cursor up one visual row at `width` display columns,
    /// preserving the horizontal position (the "goal column") across a run
    /// of consecutive vertical moves (#215). A no-op on the first visual row.
    pub fn move_up(&mut self, width: usize) {
        self.move_vertical(width, -1);
    }

    /// Move the cursor down one visual row at `width` display columns,
    /// preserving the horizontal position (the "goal column") across a run
    /// of consecutive vertical moves (#215). A no-op on the last visual row.
    pub fn move_down(&mut self, width: usize) {
        self.move_vertical(width, 1);
    }

    /// Shared implementation of [`move_up`](Self::move_up)/[`move_down`](Self::move_down):
    /// steps the cursor to the row at `row_of(current) + direction` (`-1` or
    /// `1`), landing at the remembered [`goal_col`](Self::goal_col) — or the
    /// current display column, if no vertical move is already in progress.
    /// A no-op if the target row would fall outside `0..rows.len()`.
    fn move_vertical(&mut self, width: usize, direction: isize) {
        let text = self.input.text();
        let rows = crate::wrap::layout_rows(text, width);
        let cursor_byte = self.input.byte_at(self.input.cursor());
        let row = crate::wrap::row_of(&rows, cursor_byte);
        let Some(target) = row.checked_add_signed(direction) else {
            return;
        };
        if target >= rows.len() {
            return;
        }

        let goal = self.goal_col.unwrap_or_else(|| {
            let current_row = rows[row];
            crate::wrap::display_col(
                &text[current_row.start..current_row.end],
                cursor_byte - current_row.start,
            )
        });

        let new_byte = crate::wrap::resolve_in_row(text, &rows, target, goal);
        self.input.set_cursor(self.input.char_at(new_byte));
        self.goal_col = Some(goal);
    }

    /// Move the cursor directly to a character index, clamping to the end of
    /// the buffer. Mouse clicks resolve through the row-aware
    /// [`set_cursor_at_row_col`](Self::set_cursor_at_row_col) instead (#215);
    /// this plain char-index form is test-only, for positioning the cursor
    /// before exercising a movement.
    #[cfg(test)]
    pub fn set_cursor(&mut self, index: usize) {
        self.input.set_cursor(index);
        self.goal_col = None;
    }

    /// Move the cursor to the char index at visual `row`/terminal cell `col`
    /// at `width` display columns (a click on a multi-row composer) (#215).
    /// `row` clamps to the last visual row; `col` resolves the same way
    /// [`move_up`](Self::move_up)/[`move_down`](Self::move_down) does, so a
    /// click and a vertical move land identically on a wide (CJK/emoji)
    /// character.
    pub fn set_cursor_at_row_col(&mut self, row: usize, col: usize, width: usize) {
        let text = self.input.text();
        let rows = crate::wrap::layout_rows(text, width);
        let row = row.min(rows.len() - 1);
        let new_byte = crate::wrap::resolve_in_row(text, &rows, row, col);
        self.input.set_cursor(self.input.char_at(new_byte));
        self.goal_col = None;
    }

    /// Enter reply mode against `message_id`, showing `preview` in the indicator.
    /// The buffer is left as-is so a half-typed message survives starting a reply.
    ///
    /// Driven by [`Action::ReplyMessage`](crate::app::Action::ReplyMessage) — `r` in
    /// the history pane (#195) — which focuses the composer so the reply can be typed
    /// straight away; submitting routes through the send seam as a reply (#116).
    pub fn reply_to(&mut self, message_id: i64, preview: String) {
        self.mode = ComposerMode::Reply {
            message_id,
            preview,
        };
    }

    /// Enter edit mode against `message_id`, pre-filling the buffer with its
    /// current `text` and placing the cursor at the end.
    ///
    /// Driven by [`Action::EditMessage`](crate::app::Action::EditMessage) — `e` in
    /// the history pane (#195), for our own text messages — which focuses the
    /// composer; submitting routes through the edit seam (#116).
    pub fn edit(&mut self, message_id: i64, text: String) {
        self.input.set(text);
        self.mode = ComposerMode::Edit { message_id };
        self.goal_col = None;
    }

    /// Cancel back to plain compose: drop any reply/edit context and clear the
    /// buffer (an edit's pre-filled text is discarded, not sent).
    pub fn cancel(&mut self) {
        self.input.clear();
        self.mode = ComposerMode::Compose;
        self.goal_col = None;
    }

    /// Submit the buffer. Resolves the current [mode](Self::mode) into a
    /// [`Submission`] (a new message, a reply, or an edit), clears the buffer, and
    /// resets to plain compose. An empty or whitespace-only buffer is a no-op that
    /// returns `None` and leaves the mode untouched.
    ///
    /// The loop routes the returned [`Submission`] to the send/edit seam (#116).
    #[must_use]
    pub fn submit(&mut self) -> Option<Submission> {
        if self.input.text().trim().is_empty() {
            return None;
        }
        let text = self.input.take();
        self.goal_col = None;
        // `take` resets the mode to its `Compose` default, leaving the composer
        // ready for the next message regardless of what it was just acting on.
        let submission = match std::mem::take(&mut self.mode) {
            ComposerMode::Compose => Submission::Send { text },
            ComposerMode::Reply { message_id, .. } => Submission::Reply {
                reply_to: message_id,
                text,
            },
            ComposerMode::Edit { message_id } => Submission::Edit { message_id, text },
        };
        Some(submission)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A composer with `text` typed and the cursor at the end.
    fn typed(text: &str) -> Composer {
        let mut composer = Composer::default();
        for c in text.chars() {
            composer.insert(c);
        }
        composer
    }

    #[test]
    fn default_is_empty_and_composing() {
        let composer = Composer::default();
        assert!(composer.is_empty());
        assert_eq!(composer.cursor(), 0);
        assert_eq!(composer.mode(), &ComposerMode::Compose);
    }

    #[test]
    fn insert_appends_and_advances_the_cursor() {
        let composer = typed("hi");
        assert_eq!(composer.text(), "hi");
        assert_eq!(composer.cursor(), 2);
    }

    #[test]
    fn insert_at_the_cursor_splices_mid_string() {
        let mut composer = typed("ac");
        composer.move_left();
        composer.insert('b');
        assert_eq!(composer.text(), "abc");
        // Cursor sits just after the inserted character.
        assert_eq!(composer.cursor(), 2);
    }

    #[test]
    fn backspace_removes_the_char_before_the_cursor() {
        let mut composer = typed("abc");
        composer.move_left();
        composer.backspace();
        assert_eq!(composer.text(), "ac");
        assert_eq!(composer.cursor(), 1);
    }

    #[test]
    fn backspace_at_the_start_is_a_noop() {
        let mut composer = typed("ab");
        composer.move_home();
        composer.backspace();
        assert_eq!(composer.text(), "ab");
        assert_eq!(composer.cursor(), 0);
    }

    #[test]
    fn editing_is_correct_across_multibyte_characters() {
        // 'é' and '🙂' are >1 byte; cursor math is in characters, not bytes.
        let mut composer = typed("é🙂");
        assert_eq!(composer.cursor(), 2);
        composer.backspace();
        assert_eq!(composer.text(), "é");
        assert_eq!(composer.cursor(), 1);
        composer.move_home();
        composer.insert('x');
        assert_eq!(composer.text(), "xé");
    }

    #[test]
    fn cursor_movement_clamps_at_both_ends() {
        let mut composer = typed("ab");
        composer.move_right();
        composer.move_right();
        assert_eq!(composer.cursor(), 2);
        composer.move_left();
        composer.move_left();
        composer.move_left();
        assert_eq!(composer.cursor(), 0);
        composer.move_end();
        assert_eq!(composer.cursor(), 2);
    }

    #[test]
    fn submit_resolves_the_mode_and_resets_to_empty_compose() {
        // A plain compose submits as a new message.
        let mut composer = typed("hello");
        assert_eq!(
            composer.submit(),
            Some(Submission::Send {
                text: "hello".to_owned()
            })
        );
        assert!(composer.is_empty());
        assert_eq!(composer.cursor(), 0);
        assert_eq!(composer.mode(), &ComposerMode::Compose);

        // Reply mode carries its target through as the reply's `reply_to`.
        let mut replying = typed("sure");
        replying.reply_to(7, "User 7: hi".to_owned());
        assert_eq!(
            replying.submit(),
            Some(Submission::Reply {
                reply_to: 7,
                text: "sure".to_owned()
            })
        );
        assert_eq!(replying.mode(), &ComposerMode::Compose);

        // Edit mode carries the message being edited.
        let mut editing = Composer::default();
        editing.edit(99, "old".to_owned());
        editing.insert('!');
        assert_eq!(
            editing.submit(),
            Some(Submission::Edit {
                message_id: 99,
                text: "old!".to_owned()
            })
        );
        assert_eq!(editing.mode(), &ComposerMode::Compose);
    }

    #[test]
    fn empty_submit_is_a_noop() {
        let mut composer = Composer::default();
        assert_eq!(composer.submit(), None);
        // Whitespace-only is treated the same — nothing to send.
        let mut spaces = typed("   ");
        assert_eq!(spaces.submit(), None);
        assert_eq!(spaces.text(), "   ");
    }

    #[test]
    fn reply_mode_keeps_the_buffer_and_records_the_target() {
        let mut composer = typed("draft");
        composer.reply_to(42, "User 42: yo".to_owned());
        assert_eq!(composer.text(), "draft");
        assert_eq!(
            composer.mode(),
            &ComposerMode::Reply {
                message_id: 42,
                preview: "User 42: yo".to_owned(),
            }
        );
    }

    #[test]
    fn edit_mode_prefills_the_buffer_with_the_cursor_at_the_end() {
        let mut composer = Composer::default();
        composer.edit(99, "old text".to_owned());
        assert_eq!(composer.text(), "old text");
        assert_eq!(composer.cursor(), 8);
        assert_eq!(composer.mode(), &ComposerMode::Edit { message_id: 99 });
    }

    #[test]
    fn cancel_clears_the_buffer_and_returns_to_compose() {
        let mut composer = Composer::default();
        composer.edit(99, "old text".to_owned());
        composer.cancel();
        assert!(composer.is_empty());
        assert_eq!(composer.cursor(), 0);
        assert_eq!(composer.mode(), &ComposerMode::Compose);
    }

    #[test]
    fn move_up_and_down_cross_a_wrap_seam_preserving_the_goal_column() {
        // "abcde" @ width 3 wraps into "abc" | "de".
        let mut composer = typed("abcde"); // cursor at the end (char 5, row 1 col 2)
        composer.move_up(3);
        // Row 1's cursor was at its own end (col 2); row 0 is wider, so the
        // goal column (2) lands one char short of row 0's end.
        assert_eq!(composer.cursor(), 2);
        composer.move_down(3);
        // The remembered goal column (2) returns the cursor to row 1's end.
        assert_eq!(composer.cursor(), 5);
    }

    #[test]
    fn move_up_is_a_noop_on_the_first_row_and_move_down_on_the_last() {
        let mut composer = typed("abcde"); // "abc" | "de" @ width 3
        composer.set_cursor(1);
        composer.move_up(3);
        assert_eq!(composer.cursor(), 1);

        composer.set_cursor(5);
        composer.move_down(3);
        assert_eq!(composer.cursor(), 5);
    }

    #[test]
    fn move_up_reaches_the_top_row_across_a_full_width_hard_break_seam() {
        // "abcdef" @ width 3 hard-breaks into "abc" | "def" with no '\n' —
        // the last row is exactly as wide as the first, so the goal column
        // (3, "def"'s own width) lands right on the ambiguous wrap seam
        // between them. A naive landing there would resolve back onto row 1
        // (see `wrap::resolve_in_row`), leaving Up looking like a no-op and
        // a second Up permanently stuck instead of reaching row 0.
        let mut composer = typed("abcdef");
        composer.move_up(3);
        assert_eq!(composer.cursor(), 2, "landed on row 0, not back on row 1");
        // Pressing Up again is a genuine no-op at the first row, not a
        // symptom of being stuck mid-buffer.
        composer.move_up(3);
        assert_eq!(composer.cursor(), 2);
    }

    #[test]
    fn any_other_movement_resets_the_goal_column() {
        // "abcde" @ width 3 wraps into "abc" | "de".
        let mut composer = typed("abcde");
        composer.move_up(3); // cursor -> 2 (row 0, col 2), goal_col = Some(2)
        composer.move_left(); // cursor -> 1 (row 0, col 1); resets goal_col
        composer.move_down(3);
        // A stale goal (2) would land at col 2 ("de"'s end, cursor 5); the
        // freshly recomputed goal (1) lands one column short instead.
        assert_eq!(composer.cursor(), 4);
    }

    #[test]
    fn move_up_and_down_measure_the_goal_column_by_display_width_not_char_count() {
        // "ab😀" (a real line, ended by '\n') over "cd" — 😀 is width 2.
        let mut composer = typed("ab😀\ncd");
        composer.set_cursor(3); // right after 😀, before the '\n'
        composer.move_down(10);
        // Row 1 ("cd") is narrower than the goal column (4: 1+1+2), so the
        // cursor clamps to its end.
        assert_eq!(composer.cursor(), 6);
        composer.move_up(10);
        // The remembered goal column (4) returns the cursor to right after
        // 😀 — proof the column math is display-width-aware, not char-count.
        assert_eq!(composer.cursor(), 3);
    }

    #[test]
    fn set_cursor_at_row_col_resolves_a_click_and_clamps_past_the_row_and_past_the_last_row() {
        // "abcde" @ width 3 wraps into "abc" | "de".
        let mut composer = typed("abcde");
        composer.set_cursor_at_row_col(0, 1, 3);
        assert_eq!(composer.cursor(), 1); // between 'a' and 'b'
        composer.set_cursor_at_row_col(1, 5, 3);
        assert_eq!(composer.cursor(), 5); // clamped to row 1's end
        composer.set_cursor_at_row_col(9, 0, 3);
        assert_eq!(composer.cursor(), 3); // row clamped to the last row (1)
    }

    #[test]
    fn set_cursor_at_row_col_at_a_full_width_seam_stays_on_the_clicked_row() {
        // "abcdef" @ width 3 hard-breaks into "abc" | "def", contiguous — a
        // click past row 0's right edge must still land on row 0, not
        // silently resolve onto row 1 (the same ambiguous-seam hazard
        // `move_up`/`move_down` have).
        let mut composer = typed("abcdef");
        composer.set_cursor_at_row_col(0, 3, 3);
        assert_eq!(composer.cursor(), 2);
    }
}
