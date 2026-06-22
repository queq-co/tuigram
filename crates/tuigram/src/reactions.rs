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

/// The reaction overlay's state: the emoji palette and the selected one. The
/// palette is fixed, so this is just a clamped cursor over [`PALETTE`].
#[derive(Debug, Clone, Default)]
pub struct ReactionPicker {
    /// Selection index into [`PALETTE`]; `0` (the first emoji) by default.
    selected: usize,
}

impl ReactionPicker {
    /// A fresh picker, selection on the first emoji. Opened each time the overlay
    /// is shown so the cursor never carries over from a previous reaction.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The emoji to offer, in display order.
    #[must_use]
    pub fn palette(&self) -> &'static [&'static str] {
        PALETTE
    }

    /// The selected emoji's index in the palette.
    #[must_use]
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// The currently selected emoji.
    #[must_use]
    pub fn selected_emoji(&self) -> &'static str {
        PALETTE[self.selected]
    }

    /// Move the selection to the next emoji, clamping at the last.
    pub fn select_next(&mut self) {
        self.selected = (self.selected + 1).min(PALETTE.len() - 1);
    }

    /// Move the selection to the previous emoji, clamping at the first.
    pub fn select_prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
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
}
