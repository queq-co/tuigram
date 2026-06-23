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
/// and, in Phase 6, whether a submit sends a new message, a reply, or an edit.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ComposerMode {
    /// Writing a new message in the open chat.
    #[default]
    Compose,
    /// Replying to a specific message. `preview` is a short label of the target
    /// (built by the caller from the message), shown in the pane indicator so the
    /// user sees what they are replying to.
    Reply {
        /// The message being replied to. Carried for the Phase 6 send (the reply's
        /// `reply_to`); the indicator renders only the `preview`, so the binary
        /// does not read it yet.
        #[allow(dead_code)]
        message_id: i64,
        preview: String,
    },
    /// Editing a message already sent. The buffer is pre-filled with its current
    /// text; submitting replaces it.
    Edit {
        /// The message being edited, for the Phase 6 edit call; the indicator does
        /// not render it, so it is unread in the binary for now.
        #[allow(dead_code)]
        message_id: i64,
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
}

impl Composer {
    /// The current input text.
    #[must_use]
    pub fn text(&self) -> &str {
        self.input.text()
    }

    /// The cursor position, as a character index in `0..=chars`.
    #[must_use]
    pub fn cursor(&self) -> usize {
        self.input.cursor()
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
    }

    /// Delete the character before the cursor (Backspace). A no-op at the start.
    pub fn backspace(&mut self) {
        self.input.backspace();
    }

    /// Move the cursor one character left, clamping at the start.
    pub fn move_left(&mut self) {
        self.input.move_left();
    }

    /// Move the cursor one character right, clamping at the end.
    pub fn move_right(&mut self) {
        self.input.move_right();
    }

    /// Move the cursor to the start of the line (Home).
    pub fn move_home(&mut self) {
        self.input.move_home();
    }

    /// Move the cursor to the end of the line (End).
    pub fn move_end(&mut self) {
        self.input.move_end();
    }

    /// Enter reply mode against `message_id`, showing `preview` in the indicator.
    /// The buffer is left as-is so a half-typed message survives starting a reply.
    ///
    /// The key that starts a reply lands with #83's focus model (and #84's forward
    /// flow), so this is unused in the non-test binary today; the render tests drive
    /// it through [`with_composer`](crate::app::App::with_composer).
    #[allow(dead_code)]
    pub fn reply_to(&mut self, message_id: i64, preview: String) {
        self.mode = ComposerMode::Reply {
            message_id,
            preview,
        };
    }

    /// Enter edit mode against `message_id`, pre-filling the buffer with its
    /// current `text` and placing the cursor at the end.
    ///
    /// Like [`reply_to`](Self::reply_to), the key that starts an edit arrives with
    /// #83's focus model, so this is unused in the non-test binary for now and the
    /// render tests exercise it directly.
    #[allow(dead_code)]
    pub fn edit(&mut self, message_id: i64, text: String) {
        self.input.set(text);
        self.mode = ComposerMode::Edit { message_id };
    }

    /// Cancel back to plain compose: drop any reply/edit context and clear the
    /// buffer (an edit's pre-filled text is discarded, not sent).
    pub fn cancel(&mut self) {
        self.input.clear();
        self.mode = ComposerMode::Compose;
    }

    /// Submit the buffer. Returns the text and resets to an empty compose state
    /// when there is something to send; an empty or whitespace-only buffer is a
    /// no-op that returns `None` (and leaves the mode untouched).
    ///
    /// Phase 6 routes the returned text to core — a new message, a reply, or an
    /// edit per the [mode](Self::mode) at the call site; Phase 5 simply consumes it.
    #[must_use]
    pub fn submit(&mut self) -> Option<String> {
        if self.input.text().trim().is_empty() {
            return None;
        }
        let text = self.input.take();
        self.mode = ComposerMode::Compose;
        Some(text)
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
    fn submit_returns_the_text_and_resets_to_empty_compose() {
        let mut composer = typed("hello");
        composer.reply_to(7, "User 7: hi".to_owned());
        let sent = composer.submit();
        assert_eq!(sent.as_deref(), Some("hello"));
        assert!(composer.is_empty());
        assert_eq!(composer.cursor(), 0);
        assert_eq!(composer.mode(), &ComposerMode::Compose);
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
}
