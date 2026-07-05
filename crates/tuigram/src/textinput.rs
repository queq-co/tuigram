//! A single-line text-input primitive: a buffer plus a cursor, with the editing
//! operations a TUI input line needs.
//!
//! This is the shared editing core behind every text field in the UI — the
//! message [`Composer`](crate::composer::Composer) and the search box
//! ([`SearchView`](crate::search::SearchView)) both build on it, so cursor and
//! multi-byte handling live in **one** place rather than being re-derived per
//! field.
//!
//! The cursor is a **character** index into the text (`0..=chars`), so editing
//! stays correct across multi-byte input; the byte offset is derived only when the
//! `String` itself is spliced.

/// A line of editable text and the cursor within it. Empty with the cursor at the
/// start by default.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextInput {
    /// The text the user has typed.
    text: String,
    /// Cursor position as a count of characters to its left, in `0..=chars`.
    cursor: usize,
}

impl TextInput {
    /// The current text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The cursor position, as a character index in `0..=chars`.
    #[must_use]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Whether the buffer is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Number of characters in the buffer — the cursor's upper bound.
    fn char_count(&self) -> usize {
        self.text.chars().count()
    }

    /// The byte offset of character index `i`, or the buffer length when `i` is at
    /// or past the end. Used only to splice the `String` at the cursor.
    fn byte_at(&self, i: usize) -> usize {
        self.text
            .char_indices()
            .nth(i)
            .map_or(self.text.len(), |(b, _)| b)
    }

    /// Insert a character at the cursor and step the cursor past it.
    pub fn insert(&mut self, c: char) {
        let at = self.byte_at(self.cursor);
        self.text.insert(at, c);
        self.cursor += 1;
    }

    /// Delete the character before the cursor (Backspace). A no-op at the start.
    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            let at = self.byte_at(self.cursor - 1);
            self.text.remove(at);
            self.cursor -= 1;
        }
    }

    /// Move the cursor one character left, clamping at the start.
    pub fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    /// Move the cursor one character right, clamping at the end.
    pub fn move_right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.char_count());
    }

    /// Move the cursor to the start of the line (Home).
    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    /// Move the cursor to the end of the line (End).
    pub fn move_end(&mut self) {
        self.cursor = self.char_count();
    }

    /// Move the cursor directly to a character index, clamping to the end of the
    /// buffer — a click on the input line maps its column to an index and lands
    /// here, the same clamp [`move_right`](Self::move_right) uses.
    pub fn set_cursor(&mut self, index: usize) {
        self.cursor = index.min(self.char_count());
    }

    /// Replace the buffer with `text`, placing the cursor at the end — the seam an
    /// edit (prefill) or a programmatic set uses.
    pub fn set(&mut self, text: String) {
        self.text = text;
        self.cursor = self.char_count();
    }

    /// Clear the buffer and reset the cursor to the start.
    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    /// Take the buffer, leaving it empty with the cursor reset — used by a submit
    /// that consumes the typed text.
    #[must_use]
    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `TextInput` with `text` typed and the cursor at the end.
    fn typed(text: &str) -> TextInput {
        let mut input = TextInput::default();
        for c in text.chars() {
            input.insert(c);
        }
        input
    }

    #[test]
    fn default_is_empty_with_the_cursor_at_the_start() {
        let input = TextInput::default();
        assert!(input.is_empty());
        assert_eq!(input.cursor(), 0);
    }

    #[test]
    fn insert_appends_and_advances_the_cursor() {
        let input = typed("hi");
        assert_eq!(input.text(), "hi");
        assert_eq!(input.cursor(), 2);
    }

    #[test]
    fn insert_at_the_cursor_splices_mid_string() {
        let mut input = typed("ac");
        input.move_left();
        input.insert('b');
        assert_eq!(input.text(), "abc");
        assert_eq!(input.cursor(), 2);
    }

    #[test]
    fn backspace_removes_the_char_before_the_cursor_and_is_a_noop_at_the_start() {
        let mut input = typed("abc");
        input.move_left();
        input.backspace();
        assert_eq!(input.text(), "ac");
        assert_eq!(input.cursor(), 1);

        input.move_home();
        input.backspace();
        assert_eq!(input.text(), "ac");
        assert_eq!(input.cursor(), 0);
    }

    #[test]
    fn editing_is_correct_across_multibyte_characters() {
        // 'é' and '🙂' are >1 byte; cursor math is in characters, not bytes.
        let mut input = typed("é🙂");
        assert_eq!(input.cursor(), 2);
        input.backspace();
        assert_eq!(input.text(), "é");
        assert_eq!(input.cursor(), 1);
        input.move_home();
        input.insert('x');
        assert_eq!(input.text(), "xé");
    }

    #[test]
    fn cursor_movement_clamps_at_both_ends() {
        let mut input = typed("ab");
        input.move_right();
        input.move_right();
        assert_eq!(input.cursor(), 2);
        input.move_left();
        input.move_left();
        input.move_left();
        assert_eq!(input.cursor(), 0);
        input.move_end();
        assert_eq!(input.cursor(), 2);
    }

    #[test]
    fn set_replaces_the_buffer_with_the_cursor_at_the_end() {
        let mut input = typed("ab");
        input.set("longer".to_owned());
        assert_eq!(input.text(), "longer");
        assert_eq!(input.cursor(), 6);
    }

    #[test]
    fn clear_empties_the_buffer_and_resets_the_cursor() {
        let mut input = typed("text");
        input.clear();
        assert!(input.is_empty());
        assert_eq!(input.cursor(), 0);
    }

    #[test]
    fn take_returns_the_buffer_and_leaves_it_empty() {
        let mut input = typed("payload");
        assert_eq!(input.take(), "payload");
        assert!(input.is_empty());
        assert_eq!(input.cursor(), 0);
    }
}
