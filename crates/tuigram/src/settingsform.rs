//! The in-app settings editor's view-model: the four download-cache retention
//! knobs plus the graphics toggle (#209) the settings overlay edits (#146).
//!
//! Mirrors [`MediaDraft`](crate::mediaform::MediaDraft): one [`TextInput`] per
//! field, Tab cycling between them, and a pure [`confirm`](SettingsDraft::confirm)
//! that validates the typed values through core's [`KeepMedia`]/[`CacheCap`]
//! parsers (plus a small on/off parse for graphics). It owns no terminal state and
//! touches no `Client`, so the whole edit-and-validate flow is unit-testable
//! headlessly. The draft opens pre-filled with the policy currently in effect (via
//! each value's `Display` form), so the overlay shows what is live; confirming
//! yields the new [`StorageSettings`] (for the loop to persist and swap into the
//! running retention, [#145's `render`](tuigram_core::StorageSettings::render)) and
//! the new graphics bool, while an invalid value is rejected in place with a
//! readable reason rather than saved.

use tuigram_core::{CacheCap, KeepMedia, StorageSettings};

use crate::textinput::TextInput;

/// Which field is being edited. Tab cycles through them in this order, grouped
/// as the settings file lists them: the three per-kind TTLs, the global size
/// cap, then the graphics toggle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SettingsField {
    /// Retention for one-to-one (private, secret) chats.
    #[default]
    KeepPrivate,
    /// Retention for basic groups and supergroups.
    KeepGroups,
    /// Retention for broadcast channels.
    KeepChannels,
    /// The global cache-size ceiling.
    MaxCache,
    /// The graphics-rendering toggle (#209): avatars and inline media on or off.
    Graphics,
}

impl SettingsField {
    /// The next field in the Tab cycle (wraps from the last back to the first).
    #[must_use]
    fn next(self) -> Self {
        match self {
            Self::KeepPrivate => Self::KeepGroups,
            Self::KeepGroups => Self::KeepChannels,
            Self::KeepChannels => Self::MaxCache,
            Self::MaxCache => Self::Graphics,
            Self::Graphics => Self::KeepPrivate,
        }
    }

    /// The previous field in the Tab cycle (wraps from the first back to the
    /// last) — the mirror of [`next`](Self::next), for Shift-Tab and the
    /// overlay mouse wheel scrolling backward.
    #[must_use]
    fn prev(self) -> Self {
        match self {
            Self::KeepPrivate => Self::Graphics,
            Self::KeepGroups => Self::KeepPrivate,
            Self::KeepChannels => Self::KeepGroups,
            Self::MaxCache => Self::KeepChannels,
            Self::Graphics => Self::MaxCache,
        }
    }

    /// The label shown before this field in the overlay.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::KeepPrivate => "private",
            Self::KeepGroups => "groups",
            Self::KeepChannels => "channels",
            Self::MaxCache => "max cache",
            Self::Graphics => "graphics",
        }
    }
}

/// The settings editor's state: one input per retention knob, one for the
/// graphics toggle, which field is focused, and the last validation error (shown
/// inline, cleared as soon as editing resumes). Open it with
/// [`from_settings`](Self::from_settings) so the fields start at the values
/// currently in effect.
#[derive(Debug, Clone, Default)]
pub struct SettingsDraft {
    keep_private: TextInput,
    keep_groups: TextInput,
    keep_channels: TextInput,
    max_cache: TextInput,
    /// Typed as text (`"on"`/`"off"`) like every other field (#209) — not a
    /// dedicated toggle widget — so it reuses `input`/`active`/`value`/`cursor`
    /// unchanged rather than special-casing a boolean through every method here.
    graphics: TextInput,
    field: SettingsField,
    /// The reason the last confirm was rejected, if any — shown under the fields so
    /// the user sees *why* a value was refused. Cleared on the next edit.
    error: Option<String>,
}

impl SettingsDraft {
    /// A draft pre-filled with `settings`' current values, cursor at each field's
    /// end. Each field starts at the policy's rendered form (`"forever"`, `"3d"`,
    /// `"2GB"`, …), and `graphics` at `"on"`/`"off"`, so the overlay opens showing
    /// exactly what is in effect.
    #[must_use]
    pub fn from_settings(settings: StorageSettings, graphics: bool) -> Self {
        let mut draft = Self::default();
        draft.keep_private.set(settings.keep_private.to_string());
        draft.keep_groups.set(settings.keep_groups.to_string());
        draft.keep_channels.set(settings.keep_channels.to_string());
        draft.max_cache.set(settings.max_cache.to_string());
        draft
            .graphics
            .set(if graphics { "on" } else { "off" }.to_owned());
        draft
    }

    /// The text typed in each field, for rendering.
    #[must_use]
    pub fn value(&self, field: SettingsField) -> &str {
        self.input(field).text()
    }

    /// Which field the next keystroke edits.
    #[must_use]
    pub fn field(&self) -> SettingsField {
        self.field
    }

    /// The cursor position within the focused field, for the caret.
    #[must_use]
    pub fn cursor(&self) -> usize {
        self.active().cursor()
    }

    /// The last validation error, if the previous confirm was rejected.
    #[must_use]
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// The input backing `field`.
    fn input(&self, field: SettingsField) -> &TextInput {
        match field {
            SettingsField::KeepPrivate => &self.keep_private,
            SettingsField::KeepGroups => &self.keep_groups,
            SettingsField::KeepChannels => &self.keep_channels,
            SettingsField::MaxCache => &self.max_cache,
            SettingsField::Graphics => &self.graphics,
        }
    }

    /// The focused field's input.
    fn active(&self) -> &TextInput {
        self.input(self.field)
    }

    /// The focused field's input, mutably.
    fn active_mut(&mut self) -> &mut TextInput {
        match self.field {
            SettingsField::KeepPrivate => &mut self.keep_private,
            SettingsField::KeepGroups => &mut self.keep_groups,
            SettingsField::KeepChannels => &mut self.keep_channels,
            SettingsField::MaxCache => &mut self.max_cache,
            SettingsField::Graphics => &mut self.graphics,
        }
    }

    /// Move focus to the next field (Tab). Clears any stale error, since the user is
    /// editing again.
    pub fn toggle_field(&mut self) {
        self.field = self.field.next();
        self.error = None;
    }

    /// Move focus to the previous field (Shift-Tab, and the overlay mouse
    /// wheel scrolling up) — the mirror of [`toggle_field`](Self::toggle_field).
    pub fn toggle_field_prev(&mut self) {
        self.field = self.field.prev();
        self.error = None;
    }

    /// Insert a character into the focused field at its cursor.
    pub fn insert(&mut self, c: char) {
        self.active_mut().insert(c);
        self.error = None;
    }

    /// Delete the character before the focused field's cursor (Backspace).
    pub fn backspace(&mut self) {
        self.active_mut().backspace();
        self.error = None;
    }

    /// Move the focused field's cursor one character left.
    pub fn move_left(&mut self) {
        self.active_mut().move_left();
    }

    /// Move the focused field's cursor one character right.
    pub fn move_right(&mut self) {
        self.active_mut().move_right();
    }

    /// Move the focused field's cursor to the start of the line.
    pub fn move_home(&mut self) {
        self.active_mut().move_home();
    }

    /// Move the focused field's cursor to the end of the line.
    pub fn move_end(&mut self) {
        self.active_mut().move_end();
    }

    /// Validate every field — the four retention knobs through the same
    /// [`KeepMedia`]/[`CacheCap`] parsers the config file uses, and `graphics`
    /// through its own on/off parse — returning the assembled [`StorageSettings`]
    /// and the graphics bool on success. On the first invalid value it moves focus
    /// to the offending field, records a readable reason (retrievable via
    /// [`error`](Self::error)), and returns `None` — the reducer keeps the overlay
    /// open so the user can fix it in place. The error text is derived only from
    /// the field name and the parser's own message, never leaking anything beyond
    /// what the user typed back at them.
    pub fn confirm(&mut self) -> Option<(StorageSettings, bool)> {
        let keep_private = self.parse_keep(SettingsField::KeepPrivate)?;
        let keep_groups = self.parse_keep(SettingsField::KeepGroups)?;
        let keep_channels = self.parse_keep(SettingsField::KeepChannels)?;
        let max_cache = self.parse_cap(SettingsField::MaxCache)?;
        let graphics = self.parse_graphics(SettingsField::Graphics)?;
        self.error = None;
        Some((
            StorageSettings {
                keep_private,
                keep_groups,
                keep_channels,
                max_cache,
            },
            graphics,
        ))
    }

    /// Parse a per-kind TTL field, recording a field-scoped error and focusing it on
    /// failure.
    fn parse_keep(&mut self, field: SettingsField) -> Option<KeepMedia> {
        match self.value(field).parse::<KeepMedia>() {
            Ok(keep) => Some(keep),
            Err(reason) => {
                self.reject(field, &reason);
                None
            }
        }
    }

    /// Parse the cache-cap field, recording a field-scoped error and focusing it on
    /// failure.
    fn parse_cap(&mut self, field: SettingsField) -> Option<CacheCap> {
        match self.value(field).parse::<CacheCap>() {
            Ok(cap) => Some(cap),
            Err(reason) => {
                self.reject(field, &reason);
                None
            }
        }
    }

    /// Parse the graphics toggle field: `"on"`/`"true"`/`"yes"` or
    /// `"off"`/`"false"`/`"no"`, case-insensitively, recording a field-scoped error
    /// and focusing it on failure — same rejection shape as the other fields.
    fn parse_graphics(&mut self, field: SettingsField) -> Option<bool> {
        match self.value(field).trim().to_ascii_lowercase().as_str() {
            "on" | "true" | "yes" => Some(true),
            "off" | "false" | "no" => Some(false),
            _ => {
                self.reject(field, "expected \"on\" or \"off\"");
                None
            }
        }
    }

    /// Focus the offending field and record a readable reason for the rejection.
    fn reject(&mut self, field: SettingsField, reason: &str) {
        self.field = field;
        self.error = Some(format!("{}: {reason}", field.label()));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // tests: panicking on a broken assumption is the point
mod tests {
    use super::*;

    fn typed(draft: &mut SettingsDraft, s: &str) {
        for c in s.chars() {
            draft.insert(c);
        }
    }

    #[test]
    fn opens_prefilled_with_the_current_policy() {
        let settings = StorageSettings {
            keep_private: KeepMedia::Forever,
            keep_groups: KeepMedia::Days(7),
            keep_channels: KeepMedia::Days(3),
            max_cache: CacheCap::Bytes(2 * 1024 * 1024 * 1024),
        };
        let draft = SettingsDraft::from_settings(settings, false);
        assert_eq!(draft.value(SettingsField::KeepPrivate), "forever");
        assert_eq!(draft.value(SettingsField::KeepGroups), "7d");
        assert_eq!(draft.value(SettingsField::KeepChannels), "3d");
        assert_eq!(draft.value(SettingsField::MaxCache), "2GB");
        assert_eq!(draft.value(SettingsField::Graphics), "off");
        // Landing focus is the first field, no error yet.
        assert_eq!(draft.field(), SettingsField::KeepPrivate);
        assert_eq!(draft.error(), None);
    }

    #[test]
    fn tab_cycles_through_all_five_fields_and_wraps() {
        let mut draft = SettingsDraft::default();
        assert_eq!(draft.field(), SettingsField::KeepPrivate);
        draft.toggle_field();
        assert_eq!(draft.field(), SettingsField::KeepGroups);
        draft.toggle_field();
        assert_eq!(draft.field(), SettingsField::KeepChannels);
        draft.toggle_field();
        assert_eq!(draft.field(), SettingsField::MaxCache);
        draft.toggle_field();
        assert_eq!(draft.field(), SettingsField::Graphics);
        draft.toggle_field();
        assert_eq!(
            draft.field(),
            SettingsField::KeepPrivate,
            "wraps to the first"
        );
    }

    #[test]
    fn shift_tab_cycles_backward_and_wraps() {
        let mut draft = SettingsDraft::default();
        assert_eq!(draft.field(), SettingsField::KeepPrivate);
        draft.toggle_field_prev();
        assert_eq!(draft.field(), SettingsField::Graphics, "wraps to the last");
        draft.toggle_field_prev();
        assert_eq!(draft.field(), SettingsField::MaxCache);
        draft.toggle_field_prev();
        assert_eq!(draft.field(), SettingsField::KeepChannels);
        draft.toggle_field_prev();
        assert_eq!(draft.field(), SettingsField::KeepGroups);
        draft.toggle_field_prev();
        assert_eq!(draft.field(), SettingsField::KeepPrivate);
    }

    #[test]
    fn forward_then_backward_through_every_field_returns_to_the_start() {
        let mut draft = SettingsDraft::default();
        let start = draft.field();
        for _ in 0..5 {
            draft.toggle_field();
        }
        assert_eq!(draft.field(), start, "next() is a full 5-cycle");
        for _ in 0..5 {
            draft.toggle_field_prev();
        }
        assert_eq!(
            draft.field(),
            start,
            "prev() is a full 5-cycle, the other way"
        );
    }

    #[test]
    fn editing_touches_only_the_focused_field() {
        let mut draft = SettingsDraft::from_settings(StorageSettings::default(), true);
        // Clear the private field and retype it, then move on and edit channels.
        for _ in 0.."forever".len() {
            draft.backspace();
        }
        typed(&mut draft, "1w");
        draft.toggle_field(); // groups
        draft.toggle_field(); // channels
        for _ in 0.."forever".len() {
            draft.backspace();
        }
        typed(&mut draft, "3d");
        assert_eq!(draft.value(SettingsField::KeepPrivate), "1w");
        assert_eq!(
            draft.value(SettingsField::KeepGroups),
            "forever",
            "untouched"
        );
        assert_eq!(draft.value(SettingsField::KeepChannels), "3d");
        assert_eq!(draft.value(SettingsField::Graphics), "on", "untouched");
    }

    #[test]
    fn confirm_builds_the_edited_policy() {
        let mut draft = SettingsDraft::from_settings(StorageSettings::default(), true);
        // Set channels to 3d and max cache to 2GB, leaving the rest at forever.
        draft.toggle_field(); // groups
        draft.toggle_field(); // channels
        for _ in 0.."forever".len() {
            draft.backspace();
        }
        typed(&mut draft, "3d");
        draft.toggle_field(); // max cache
        for _ in 0.."unbounded".len() {
            draft.backspace();
        }
        typed(&mut draft, "2GB");
        draft.toggle_field(); // graphics
        for _ in 0.."on".len() {
            draft.backspace();
        }
        typed(&mut draft, "off");
        let (settings, graphics) = draft.confirm().expect("all fields valid");
        assert_eq!(settings.keep_private, KeepMedia::Forever);
        assert_eq!(settings.keep_channels, KeepMedia::Days(3));
        assert_eq!(settings.max_cache, CacheCap::Bytes(2 * 1024 * 1024 * 1024));
        assert!(!graphics);
        assert_eq!(draft.error(), None);
    }

    #[test]
    fn an_invalid_value_is_rejected_in_place_with_a_reason() {
        let mut draft = SettingsDraft::from_settings(StorageSettings::default(), true);
        // Type an unsupported unit into the max-cache field.
        draft.toggle_field();
        draft.toggle_field();
        draft.toggle_field(); // max cache
        for _ in 0.."unbounded".len() {
            draft.backspace();
        }
        typed(&mut draft, "2TB");
        assert!(draft.confirm().is_none(), "invalid value must not save");
        // Focus jumps to the offending field and the reason names it.
        assert_eq!(draft.field(), SettingsField::MaxCache);
        let error = draft.error().expect("a rejection reason");
        assert!(error.starts_with("max cache:"), "names the field: {error}");
    }

    #[test]
    fn a_zero_ttl_is_rejected() {
        // "0d" would wipe media the instant it is fetched — the parser rejects it, and
        // the editor surfaces that rather than saving.
        let mut draft = SettingsDraft::from_settings(StorageSettings::default(), true);
        for _ in 0.."forever".len() {
            draft.backspace();
        }
        typed(&mut draft, "0d");
        assert!(draft.confirm().is_none());
        assert_eq!(draft.field(), SettingsField::KeepPrivate);
        assert!(draft.error().unwrap().starts_with("private:"));
    }

    #[test]
    fn editing_after_a_rejection_clears_the_error() {
        let mut draft = SettingsDraft::from_settings(StorageSettings::default(), true);
        for _ in 0.."forever".len() {
            draft.backspace();
        }
        typed(&mut draft, "nope");
        assert!(draft.confirm().is_none());
        assert!(draft.error().is_some());
        draft.backspace();
        assert_eq!(
            draft.error(),
            None,
            "resuming an edit clears the stale error"
        );
    }

    #[test]
    fn graphics_accepts_on_off_synonyms_case_insensitively() {
        for (typed_value, expected) in [
            ("on", true),
            ("ON", true),
            ("true", true),
            ("yes", true),
            ("off", false),
            ("OFF", false),
            ("false", false),
            ("no", false),
        ] {
            let mut draft = SettingsDraft::from_settings(StorageSettings::default(), true);
            draft.toggle_field();
            draft.toggle_field();
            draft.toggle_field();
            draft.toggle_field(); // graphics
            for _ in 0.."on".len() {
                draft.backspace();
            }
            typed(&mut draft, typed_value);
            let (_, graphics) = draft.confirm().expect("a recognised on/off synonym");
            assert_eq!(graphics, expected, "input {typed_value:?}");
        }
    }

    #[test]
    fn an_unrecognised_graphics_value_is_rejected_in_place() {
        let mut draft = SettingsDraft::from_settings(StorageSettings::default(), true);
        draft.toggle_field();
        draft.toggle_field();
        draft.toggle_field();
        draft.toggle_field(); // graphics
        for _ in 0.."on".len() {
            draft.backspace();
        }
        typed(&mut draft, "maybe");
        assert!(draft.confirm().is_none());
        assert_eq!(draft.field(), SettingsField::Graphics);
        let error = draft.error().expect("a rejection reason");
        assert!(error.starts_with("graphics:"), "names the field: {error}");
    }
}
