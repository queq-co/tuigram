//! The send-media prompt view-model: the local path and optional caption the
//! attach overlay edits (#85).
//!
//! Sending media is a write — core's [`SendRequests::send_media`] uploads a file
//! from a **local path** (never raw bytes) with an optional caption. The prompt
//! mirrors that: two fields, the path and the caption, each the shared
//! [`TextInput`] primitive so editing matches the composer's, with Tab moving
//! between them. Confirming builds an [`OutgoingMedia`] whose variant is inferred
//! from the path's extension; Phase 6 hands it to `send_media` and the upload
//! streams back through the file store, so Phase 5 exercises the prompt and the
//! projection headlessly.
//!
//! [`SendRequests::send_media`]: tuigram_core::SendRequests

use tuigram_core::model::{FormattedText, OutgoingMedia};

use crate::textinput::TextInput;

/// Which field of the prompt is being edited. Tab cycles between the two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MediaField {
    /// The local file path (the required field).
    #[default]
    Path,
    /// The optional caption.
    Caption,
}

/// The attach prompt's state: the path and caption inputs and which one is
/// focused. Empty by default — a fresh prompt before anything is typed.
#[derive(Debug, Clone, Default)]
pub struct MediaDraft {
    /// The local path of the file to send.
    path: TextInput,
    /// The optional caption to send with it.
    caption: TextInput,
    /// Which field the next keystroke edits.
    field: MediaField,
}

impl MediaDraft {
    /// The local path typed so far.
    #[must_use]
    pub fn path(&self) -> &str {
        self.path.text()
    }

    /// The caption typed so far.
    #[must_use]
    pub fn caption(&self) -> &str {
        self.caption.text()
    }

    /// Which field is currently focused.
    #[must_use]
    pub fn field(&self) -> MediaField {
        self.field
    }

    /// The cursor position within the focused field, for the caret.
    #[must_use]
    pub fn cursor(&self) -> usize {
        self.active().cursor()
    }

    /// The focused field's input.
    fn active(&self) -> &TextInput {
        match self.field {
            MediaField::Path => &self.path,
            MediaField::Caption => &self.caption,
        }
    }

    /// The focused field's input, mutably.
    fn active_mut(&mut self) -> &mut TextInput {
        match self.field {
            MediaField::Path => &mut self.path,
            MediaField::Caption => &mut self.caption,
        }
    }

    /// Move focus to the other field (Tab).
    pub fn toggle_field(&mut self) {
        self.field = match self.field {
            MediaField::Path => MediaField::Caption,
            MediaField::Caption => MediaField::Path,
        };
    }

    /// Insert a character into the focused field at its cursor.
    pub fn insert(&mut self, c: char) {
        self.active_mut().insert(c);
    }

    /// Delete the character before the focused field's cursor (Backspace).
    pub fn backspace(&mut self) {
        self.active_mut().backspace();
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

    /// Whether the prompt has a path to send — the confirm is a no-op without one.
    #[must_use]
    pub fn is_sendable(&self) -> bool {
        !self.path.text().trim().is_empty()
    }

    /// Build the [`OutgoingMedia`] the prompt describes, its variant inferred from
    /// the path's extension (image ⇒ photo, video ⇒ video, audio ⇒ audio, else
    /// document), or `None` when no path is set. Phase 6 hands the result to
    /// [`send_media`](tuigram_core::SendRequests::send_media); only the reducer
    /// tests build it today, so it is unused in the non-test binary for now.
    ///
    /// [`send_media`]: tuigram_core::SendRequests::send_media
    #[allow(dead_code)]
    #[must_use]
    pub fn to_outgoing(&self) -> Option<OutgoingMedia> {
        if !self.is_sendable() {
            return None;
        }
        let path = self.path.text().trim().to_owned();
        let caption = FormattedText {
            text: self.caption.text().to_owned(),
            entities: Vec::new(),
        };
        Some(match media_kind(&path) {
            MediaKind::Photo => OutgoingMedia::Photo { path, caption },
            MediaKind::Video => OutgoingMedia::Video { path, caption },
            MediaKind::Audio => OutgoingMedia::Audio { path, caption },
            MediaKind::Document => OutgoingMedia::Document { path, caption },
        })
    }
}

/// The media variant a path's extension implies. A coarse mapping — the common
/// cases a keyboard client sends — defaulting anything unrecognised to a document.
enum MediaKind {
    Photo,
    Video,
    Audio,
    Document,
}

/// Infer the [`MediaKind`] from a path's lowercased extension.
fn media_kind(path: &str) -> MediaKind {
    let ext = path
        .rsplit('.')
        .next()
        .filter(|e| !e.contains('/'))
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    match ext.as_str() {
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" => MediaKind::Photo,
        "mp4" | "mov" | "mkv" | "webm" | "avi" => MediaKind::Video,
        "mp3" | "ogg" | "oga" | "wav" | "flac" | "m4a" => MediaKind::Audio,
        _ => MediaKind::Document,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(draft: &mut MediaDraft, s: &str) {
        for c in s.chars() {
            draft.insert(c);
        }
    }

    #[test]
    fn default_is_an_empty_unsendable_path_prompt() {
        let draft = MediaDraft::default();
        assert_eq!(draft.path(), "");
        assert_eq!(draft.caption(), "");
        assert_eq!(draft.field(), MediaField::Path);
        assert!(!draft.is_sendable());
        assert_eq!(draft.to_outgoing(), None);
    }

    #[test]
    fn tab_moves_editing_between_the_path_and_the_caption() {
        let mut draft = MediaDraft::default();
        typed(&mut draft, "/tmp/a.png");
        draft.toggle_field();
        assert_eq!(draft.field(), MediaField::Caption);
        typed(&mut draft, "hi");
        assert_eq!(draft.path(), "/tmp/a.png", "path field untouched");
        assert_eq!(draft.caption(), "hi");
        // The caret reports the focused (caption) field's cursor.
        assert_eq!(draft.cursor(), 2);
    }

    #[test]
    fn to_outgoing_infers_the_variant_from_the_extension() {
        // The label each path's inferred variant should report, so the assertion
        // reads as a plain mapping from extension to media kind.
        fn variant_of(media: &OutgoingMedia) -> &'static str {
            match media {
                OutgoingMedia::Photo { .. } => "photo",
                OutgoingMedia::Video { .. } => "video",
                OutgoingMedia::Audio { .. } => "audio",
                OutgoingMedia::Document { .. } => "document",
                OutgoingMedia::Voice { .. } => "voice",
                OutgoingMedia::Animation { .. } => "animation",
            }
        }
        let cases = [
            ("/tmp/pic.JPG", "photo"), // case-insensitive extension
            ("/tmp/clip.mp4", "video"),
            ("/tmp/song.mp3", "audio"),
            ("/tmp/notes.pdf", "document"),
            ("/tmp/no_ext", "document"), // unrecognised ⇒ document
        ];
        for (path, expected) in cases {
            let mut draft = MediaDraft::default();
            typed(&mut draft, path);
            let media = draft.to_outgoing().expect("sendable with a path");
            assert_eq!(variant_of(&media), expected, "wrong variant for {path}");
        }
    }

    #[test]
    fn to_outgoing_carries_the_path_and_caption() {
        let mut draft = MediaDraft::default();
        typed(&mut draft, "/tmp/a.png");
        draft.toggle_field();
        typed(&mut draft, "a caption");
        match draft.to_outgoing().expect("sendable") {
            OutgoingMedia::Photo { path, caption } => {
                assert_eq!(path, "/tmp/a.png");
                assert_eq!(caption.text, "a caption");
            }
            other => panic!("expected a photo, got {other:?}"),
        }
    }

    #[test]
    fn a_blank_path_is_not_sendable() {
        let mut draft = MediaDraft::default();
        typed(&mut draft, "   ");
        assert!(!draft.is_sendable());
        assert_eq!(draft.to_outgoing(), None);
    }
}
