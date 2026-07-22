//! The reaction-picker view-model: the small emoji palette the reaction overlay
//! renders from (#85).
//!
//! Adding or removing a reaction is a write — core's
//! [`ReactionRequests`](tuigram_core::ReactionRequests) only sends standard emoji
//! reactions — so the picker offers a fixed palette of common emoji and a clamped
//! cursor over them, exactly like the chat list's selection. Confirming toggles
//! the chosen emoji on the selected message: Phase 6 dispatches the core
//! add/remove and lets the resulting counts fold in; Phase 5 reflects it
//! optimistically through [`ConversationView::toggle_reaction`], so the picker and
//! the `{emoji×n*}` chips are exercised headlessly today.
//!
//! [`ConversationView::toggle_reaction`]: crate::conversation::ConversationView::toggle_reaction

/// The common emoji offered by the picker, in display order. A small, fixed set —
/// the reactions a keyboard client reaches for — rather than the full per-chat
/// available-reactions list, which is a Phase 6 refinement.
const PALETTE: &[&str] = &["👍", "👎", "❤️", "🔥", "🎉", "😁", "😢", "🙏"];

/// A confirmed reaction toggle, recorded by `App` as a pure intent for the loop to
/// dispatch (#119) — the message and emoji, and whether the toggle **added** our
/// reaction or **removed** it. `App` never touches the `Client`, so
/// [`ReactionConfirm`](crate::app::Action::ReactionConfirm) reflects the toggle
/// optimistically and records this; the loop drains it into
/// [`ReactionRequests::add_message_reaction`] / `remove_message_reaction`, the same
/// intent-then-drain split forwarding (#118) uses. The real
/// `updateMessageInteractionInfo` then folds the authoritative counts back over the
/// optimistic ones.
///
/// [`ReactionRequests::add_message_reaction`]: tuigram_core::ReactionRequests::add_message_reaction
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReactionIntent {
    /// The chat holding the reacted-to message — `add`/`remove_message_reaction`'s
    /// chat.
    pub chat_id: i64,
    /// The message being reacted to, by id.
    pub message_id: i64,
    /// The standard emoji reaction being toggled (e.g. `"👍"`).
    pub emoji: String,
    /// Whether the toggle added our reaction (`true` → `add_message_reaction`) or
    /// removed it (`false` → `remove_message_reaction`), decided from the message's
    /// pre-toggle state.
    pub add: bool,
}

/// The reaction overlay's state: the emoji palette with a clamped cursor, plus an
/// optional custom-emoji entry line for reacting with an emoji outside the palette
/// (#119). Two modes: *palette* (`custom` is `None`) navigates [`PALETTE`]; *custom*
/// (`custom` is `Some(buffer)`) accumulates typed/pasted characters — a terminal has
/// no emoji key, so the buffer takes whatever the OS emoji picker or a paste emits
/// (one or many scalars: 👍, ❤️, ZWJ sequences) and sends it verbatim. The core seam
/// ([`ReactionRequests`](tuigram_core::ReactionRequests)) already accepts an
/// arbitrary emoji string, so a custom emoji flows through the same path as a palette
/// one; the server validates it against the chat's available reactions.
#[derive(Debug, Clone, Default)]
pub struct ReactionPicker {
    /// Selection index into [`PALETTE`]; `0` (the first emoji) by default.
    selected: usize,
    /// The custom-emoji entry buffer while that line is active, or `None` in palette
    /// mode. `Some("")` is the empty custom line (just entered, nothing typed yet).
    custom: Option<String>,
}

impl ReactionPicker {
    /// A fresh picker in palette mode, selection on the first emoji. Opened each time
    /// the overlay is shown so neither the cursor nor a custom buffer carries over.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The emoji to offer, in display order.
    // `&self` unused; kept for symmetry with the other getters on this type.
    #[allow(clippy::unused_self)]
    #[must_use]
    pub fn palette(&self) -> &'static [&'static str] {
        PALETTE
    }

    /// The selected emoji's index in the palette.
    #[must_use]
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// The currently selected palette emoji.
    #[must_use]
    pub fn selected_emoji(&self) -> &'static str {
        PALETTE[self.selected]
    }

    /// Whether the custom-emoji entry line is active (as opposed to palette mode).
    #[must_use]
    pub fn is_custom(&self) -> bool {
        self.custom.is_some()
    }

    /// The custom-emoji entry buffer while that line is active, or `None` in palette
    /// mode — read by the render to draw the entry line and its cursor.
    #[must_use]
    pub fn custom_input(&self) -> Option<&str> {
        self.custom.as_deref()
    }

    /// Switch to the custom-emoji entry line, starting from an empty buffer. A no-op
    /// if it is already active, so re-entering never discards what was typed.
    pub fn enter_custom(&mut self) {
        if self.custom.is_none() {
            self.custom = Some(String::new());
        }
    }

    /// Leave the custom-emoji entry line, back to palette mode, discarding the buffer.
    pub fn exit_custom(&mut self) {
        self.custom = None;
    }

    /// Append a typed/pasted character to the custom buffer. A no-op in palette mode.
    pub fn push(&mut self, c: char) {
        if let Some(buffer) = self.custom.as_mut() {
            buffer.push(c);
        }
    }

    /// Delete the last character of the custom buffer. A no-op in palette mode or on
    /// an empty buffer.
    pub fn backspace(&mut self) {
        if let Some(buffer) = self.custom.as_mut() {
            buffer.pop();
        }
    }

    /// The emoji a confirm would react with: the trimmed custom buffer in custom mode
    /// (`None` when it is empty — nothing to send), or the selected palette emoji in
    /// palette mode. Normalized via
    /// [`normalize_reaction_emoji`](tuigram_core::normalize_reaction_emoji) — dropping
    /// a VS16 (e.g. on ❤️) — so this, the sole producer of the value that reaches the
    /// optimistic local reaction bucket and the outbound `add`/`remove_message_reaction`
    /// call, hands both a form matching Telegram's canonical reaction string. The
    /// reducer reads this to decide whether the confirm acts.
    #[must_use]
    pub fn confirmed_emoji(&self) -> Option<String> {
        let raw = match &self.custom {
            Some(buffer) => {
                let trimmed = buffer.trim();
                if trimmed.is_empty() {
                    return None;
                }
                trimmed
            }
            None => self.selected_emoji(),
        };
        Some(tuigram_core::normalize_reaction_emoji(raw))
    }

    /// Move the selection to the next emoji, clamping at the last.
    pub fn select_next(&mut self) {
        self.selected = (self.selected + 1).min(PALETTE.len() - 1);
    }

    /// Move the selection to the previous emoji, clamping at the first.
    pub fn select_prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Move the selection directly to a palette index (a click on an emoji),
    /// clamping at the last.
    pub fn select(&mut self, index: usize) {
        self.selected = index.min(PALETTE.len() - 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_picker_starts_on_the_first_emoji() {
        let picker = ReactionPicker::new();
        assert_eq!(picker.selected(), 0);
        assert_eq!(picker.selected_emoji(), "👍");
        assert!(!picker.palette().is_empty());
    }

    #[test]
    fn selection_advances_then_clamps_at_the_last_emoji() {
        let mut picker = ReactionPicker::new();
        picker.select_next();
        assert_eq!(picker.selected_emoji(), "👎");
        for _ in 0..100 {
            picker.select_next();
        }
        assert_eq!(
            picker.selected(),
            picker.palette().len() - 1,
            "clamps, does not run off the end"
        );
    }

    #[test]
    fn selection_retreats_then_clamps_at_the_first_emoji() {
        let mut picker = ReactionPicker::new();
        picker.select_next();
        picker.select_next();
        picker.select_prev();
        assert_eq!(picker.selected(), 1);
        picker.select_prev();
        picker.select_prev();
        assert_eq!(picker.selected(), 0, "clamps at the top");
    }

    #[test]
    fn confirms_the_palette_emoji_in_palette_mode() {
        let mut picker = ReactionPicker::new();
        assert!(!picker.is_custom());
        assert_eq!(picker.confirmed_emoji().as_deref(), Some("👍"));
        picker.select_next();
        assert_eq!(picker.confirmed_emoji().as_deref(), Some("👎"));
        // Typing does nothing while palette mode is active.
        picker.push('x');
        assert_eq!(picker.confirmed_emoji().as_deref(), Some("👎"));
    }

    #[test]
    fn palette_heart_confirms_as_telegrams_bare_codepoint() {
        // The palette displays "❤️" (heart + VS16) so the picker renders the emoji
        // glyph, but confirming it must send Telegram's canonical single-codepoint
        // "❤" — the form the server expects and echoes back.
        let mut picker = ReactionPicker::new();
        let heart_index = picker
            .palette()
            .iter()
            .position(|&e| e == "❤️")
            .expect("palette has a heart entry");
        picker.select(heart_index);
        assert_eq!(
            picker.selected_emoji(),
            "❤️",
            "display glyph keeps its VS16"
        );
        assert_eq!(picker.confirmed_emoji().as_deref(), Some("❤"));
    }

    #[test]
    fn custom_mode_accumulates_and_confirms_the_typed_emoji() {
        let mut picker = ReactionPicker::new();
        picker.enter_custom();
        assert!(picker.is_custom());
        assert_eq!(picker.custom_input(), Some(""));
        // An empty buffer has nothing to send.
        assert_eq!(picker.confirmed_emoji(), None);
        // A multi-scalar emoji (heart + VS16) normalizes to Telegram's bare form.
        for c in "❤️".chars() {
            picker.push(c);
        }
        assert_eq!(picker.confirmed_emoji().as_deref(), Some("❤"));
        picker.backspace(); // drop the VS16
        picker.backspace(); // drop the heart
        assert_eq!(picker.custom_input(), Some(""));
        assert_eq!(picker.confirmed_emoji(), None, "empty again");
    }

    #[test]
    fn custom_confirm_ignores_surrounding_whitespace() {
        let mut picker = ReactionPicker::new();
        picker.enter_custom();
        for c in "  🔥 ".chars() {
            picker.push(c);
        }
        assert_eq!(picker.confirmed_emoji().as_deref(), Some("🔥"));
    }

    #[test]
    fn leaving_custom_mode_returns_to_the_palette() {
        let mut picker = ReactionPicker::new();
        picker.select_next(); // land on 👎
        picker.enter_custom();
        picker.push('🥳');
        picker.exit_custom();
        assert!(!picker.is_custom());
        assert_eq!(picker.custom_input(), None);
        // The palette selection is intact, and confirm falls back to it.
        assert_eq!(picker.confirmed_emoji().as_deref(), Some("👎"));
        // Re-entering starts fresh (the discarded buffer does not linger).
        picker.enter_custom();
        assert_eq!(picker.custom_input(), Some(""));
    }
}
