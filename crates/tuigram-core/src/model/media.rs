//! File-backed media content: [`FileRef`], [`File`], [`Photo`], [`Video`],
//! [`Document`], [`Audio`], [`Voice`], [`Sticker`], [`Animation`].

use tdlib_rs::enums::StickerFormat as TdStickerFormat;
use tdlib_rs::types::{
    File as TdFile, MessageAnimation as TdMessageAnimation, MessageAudio as TdMessageAudio,
    MessageDocument as TdMessageDocument, MessagePhoto as TdMessagePhoto,
    MessageSticker as TdMessageSticker, MessageVideo as TdMessageVideo,
    MessageVoiceNote as TdMessageVoiceNote,
};

use super::richtext::FormattedText;
use super::user::decode_minithumbnail;

/// A reference to a `TDLib` file, as held by media message content.
///
/// Media (a photo, video, document, …) carries only this id; the bytes and the
/// download/upload state live in the [`FileStore`](crate::files::FileStore),
/// which the single update router keeps current from `updateFile`. This is the
/// same indirection [`Sender::User`](super::user::Sender::User) uses for
/// people: content stays a cheap, `Copy` reference and the mutable file state
/// is resolved out of one store — `store.get(file_ref)` — rather than
/// duplicated into every message snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FileRef {
    /// `TDLib`'s per-session file id (the key into the [`FileStore`](crate::files::FileStore)).
    pub id: i32,
}

impl FileRef {
    /// Wrap a `TDLib` file id.
    #[must_use]
    pub fn new(id: i32) -> Self {
        Self { id }
    }
}

/// A file tuigram knows about — its size and its local/remote transfer state,
/// flattened from `TDLib`'s `File`/`LocalFile`/`RemoteFile` trio into the subset a
/// caller needs to show a thumbnail, a download/upload bar, or open the bytes.
///
/// The projection is **total** (it reads every nested field it surfaces), and
/// folding the same `updateFile` twice converges, so the [`FileStore`] can
/// re-apply `TDLib`'s repeated emissions idempotently.
///
/// [`FileStore`]: crate::files::FileStore
// Each bool mirrors one independent `TDLib` field 1:1 (active/completed are not
// mutually exclusive with their upload/download counterpart); folding them into
// an enum would break that direct correspondence for no real benefit.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct File {
    /// `TDLib`'s per-session file id.
    pub id: i32,
    /// File size in bytes; `0` if unknown (then [`expected_size`](Self::expected_size)
    /// approximates it).
    pub size: i64,
    /// Approximate size in bytes when the exact `size` is unknown; for progress.
    pub expected_size: i64,
    /// Path to the local copy; empty until a download starts writing one.
    pub local_path: String,
    /// Bytes of the file available locally so far (download progress numerator).
    pub downloaded_size: i64,
    /// Whether a download is currently in progress.
    pub is_downloading_active: bool,
    /// Whether the local copy is fully downloaded.
    pub is_downloading_completed: bool,
    /// Bytes of the file uploaded so far (upload progress numerator, for #47).
    pub uploaded_size: i64,
    /// Whether an upload is currently in progress.
    pub is_uploading_active: bool,
    /// Whether the remote copy is fully uploaded.
    pub is_uploading_completed: bool,
}

impl File {
    /// Project `TDLib`'s `File`, flattening its local and remote sub-records.
    #[must_use]
    pub fn from_tdlib(file: &TdFile) -> Self {
        Self {
            id: file.id,
            size: file.size,
            expected_size: file.expected_size,
            local_path: file.local.path.clone(),
            downloaded_size: file.local.downloaded_size,
            is_downloading_active: file.local.is_downloading_active,
            is_downloading_completed: file.local.is_downloading_completed,
            uploaded_size: file.remote.uploaded_size,
            is_uploading_active: file.remote.is_uploading_active,
            is_uploading_completed: file.remote.is_uploading_completed,
        }
    }

    /// A reference to this file, for embedding in media content.
    #[must_use]
    pub fn as_ref(&self) -> FileRef {
        FileRef::new(self.id)
    }

    /// Whether the full file is readable from [`local_path`](Self::local_path)
    /// now — downloaded to completion with a path set. The single bool a caller
    /// checks before opening the bytes, rather than re-deriving it each time.
    #[must_use]
    pub fn is_present(&self) -> bool {
        self.is_downloading_completed && !self.local_path.is_empty()
    }

    /// The best known total size in bytes: the exact `size` when `TDLib` has it,
    /// else the `expected_size` estimate. The denominator for a progress bar.
    #[must_use]
    pub fn total_size(&self) -> i64 {
        if self.size > 0 {
            self.size
        } else {
            self.expected_size
        }
    }
}

/// A photo message: its caption and the single best (largest) size to show.
///
/// `TDLib` sends a photo as several pre-scaled [`sizes`](TdMessagePhoto); a
/// keyboard-driven client renders one, so this keeps the largest and drops the
/// thumbnails. The bytes live in the [`FileStore`](crate::files::FileStore) under
/// [`file`](Self::file) — content stays a cheap reference, same as elsewhere.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Photo {
    /// Caption shown with the photo (empty when there is none).
    pub caption: FormattedText,
    /// The largest available size's file (id `0` if the photo has no sizes).
    pub file: FileRef,
    /// Width of [`file`](Self::file), in pixels.
    pub width: i32,
    /// Height of [`file`](Self::file), in pixels.
    pub height: i32,
}

impl Photo {
    /// Project `TDLib`'s `messagePhoto`, keeping its largest size.
    #[must_use]
    pub fn from_tdlib(m: &TdMessagePhoto) -> Self {
        // TDLib doesn't guarantee `sizes` order, so pick by pixel area rather
        // than trusting the last element; absent sizes degrade to a 0 ref.
        let largest = m
            .photo
            .sizes
            .iter()
            .max_by_key(|s| i64::from(s.width) * i64::from(s.height));
        let (file, width, height) = match largest {
            Some(s) => (FileRef::new(s.photo.id), s.width, s.height),
            None => (FileRef::new(0), 0, 0),
        };
        Self {
            caption: FormattedText::from_tdlib(&m.caption),
            file,
            width,
            height,
        }
    }
}

/// A video message: caption, dimensions, duration, and the file to play.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Video {
    /// Caption shown with the video (empty when there is none).
    pub caption: FormattedText,
    /// The video file.
    pub file: FileRef,
    /// Video width, in pixels.
    pub width: i32,
    /// Video height, in pixels.
    pub height: i32,
    /// Duration, in seconds.
    pub duration: i32,
    /// Original file name, as given by the sender (may be empty).
    pub file_name: String,
    /// MIME type, as given by the sender (may be empty).
    pub mime_type: String,
    /// The video's minithumbnail, decoded to raw JPEG bytes (#208): a small
    /// inline preview `TDLib` delivers with the video itself, needing no
    /// `downloadFile` round trip — used as the video's static still. `None`
    /// when `TDLib` attached none.
    pub minithumbnail: Option<Vec<u8>>,
}

impl Video {
    /// Project `TDLib`'s `messageVideo`.
    #[must_use]
    pub fn from_tdlib(m: &TdMessageVideo) -> Self {
        Self {
            caption: FormattedText::from_tdlib(&m.caption),
            file: FileRef::new(m.video.video.id),
            width: m.video.width,
            height: m.video.height,
            duration: m.video.duration,
            file_name: crate::sanitize::scrub_line(&m.video.file_name),
            mime_type: crate::sanitize::scrub_line(&m.video.mime_type),
            minithumbnail: decode_minithumbnail(m.video.minithumbnail.as_ref()),
        }
    }
}

/// A document (arbitrary file) message: caption, name, MIME type, and file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Document {
    /// Caption shown with the document (empty when there is none).
    pub caption: FormattedText,
    /// The document file.
    pub file: FileRef,
    /// Original file name, as given by the sender (may be empty).
    pub file_name: String,
    /// MIME type, as given by the sender (may be empty).
    pub mime_type: String,
}

impl Document {
    /// Project `TDLib`'s `messageDocument`.
    #[must_use]
    pub fn from_tdlib(m: &TdMessageDocument) -> Self {
        Self {
            caption: FormattedText::from_tdlib(&m.caption),
            file: FileRef::new(m.document.document.id),
            file_name: crate::sanitize::scrub_line(&m.document.file_name),
            mime_type: crate::sanitize::scrub_line(&m.document.mime_type),
        }
    }
}

/// A music/audio message: caption, track metadata, and the file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Audio {
    /// Caption shown with the audio (empty when there is none).
    pub caption: FormattedText,
    /// The audio file.
    pub file: FileRef,
    /// Duration, in seconds.
    pub duration: i32,
    /// Track title, as given by the sender (may be empty).
    pub title: String,
    /// Performer, as given by the sender (may be empty).
    pub performer: String,
    /// Original file name, as given by the sender (may be empty).
    pub file_name: String,
    /// MIME type, as given by the sender (may be empty).
    pub mime_type: String,
}

impl Audio {
    /// Project `TDLib`'s `messageAudio`.
    #[must_use]
    pub fn from_tdlib(m: &TdMessageAudio) -> Self {
        Self {
            caption: FormattedText::from_tdlib(&m.caption),
            file: FileRef::new(m.audio.audio.id),
            duration: m.audio.duration,
            title: crate::sanitize::scrub_line(&m.audio.title),
            performer: crate::sanitize::scrub_line(&m.audio.performer),
            file_name: crate::sanitize::scrub_line(&m.audio.file_name),
            mime_type: crate::sanitize::scrub_line(&m.audio.mime_type),
        }
    }
}

/// A voice-note message: caption, duration, MIME type, and the file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Voice {
    /// Caption shown with the voice note (empty when there is none).
    pub caption: FormattedText,
    /// The voice-note file.
    pub file: FileRef,
    /// Duration, in seconds.
    pub duration: i32,
    /// MIME type, as given by the sender (e.g. `audio/ogg`; may be empty).
    pub mime_type: String,
}

impl Voice {
    /// Project `TDLib`'s `messageVoiceNote`.
    #[must_use]
    pub fn from_tdlib(m: &TdMessageVoiceNote) -> Self {
        Self {
            caption: FormattedText::from_tdlib(&m.caption),
            file: FileRef::new(m.voice_note.voice.id),
            duration: m.voice_note.duration,
            mime_type: crate::sanitize::scrub_line(&m.voice_note.mime_type),
        }
    }
}

/// A sticker message: its emoji, dimensions, and the file. Stickers carry no
/// caption in `TDLib`, so there is none here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sticker {
    /// The sticker file.
    pub file: FileRef,
    /// Sticker width, in pixels.
    pub width: i32,
    /// Sticker height, in pixels.
    pub height: i32,
    /// The emoji the sticker corresponds to (may be empty if unknown).
    pub emoji: String,
    /// Whether the sticker is a static WEBP image (#208) — `false` for the
    /// animated TGS (Lottie vector) and WEBM (video) formats, neither of
    /// which the `image` crate can raster-decode. Kept as a plain bool rather
    /// than exposing `TDLib`'s own `StickerFormat` enum, consistent with this
    /// module's insulation from `tdlib_rs` shapes.
    pub is_static: bool,
}

impl Sticker {
    /// Project `TDLib`'s `messageSticker`.
    #[must_use]
    pub fn from_tdlib(m: &TdMessageSticker) -> Self {
        Self {
            file: FileRef::new(m.sticker.sticker.id),
            width: m.sticker.width,
            height: m.sticker.height,
            emoji: crate::sanitize::scrub_line(&m.sticker.emoji),
            is_static: matches!(m.sticker.format, TdStickerFormat::Webp),
        }
    }
}

/// An animation (GIF/silent video) message: caption, dimensions, duration, file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Animation {
    /// Caption shown with the animation (empty when there is none).
    pub caption: FormattedText,
    /// The animation file.
    pub file: FileRef,
    /// Animation width, in pixels.
    pub width: i32,
    /// Animation height, in pixels.
    pub height: i32,
    /// Duration, in seconds.
    pub duration: i32,
    /// Original file name, as given by the sender (may be empty).
    pub file_name: String,
    /// MIME type, as given by the sender (e.g. `video/mp4`; may be empty).
    pub mime_type: String,
    /// The animation's minithumbnail, decoded to raw JPEG bytes (#208): a
    /// small inline preview `TDLib` delivers with the animation itself, needing
    /// no `downloadFile` round trip — used as the animation's static still.
    /// `None` when `TDLib` attached none.
    pub minithumbnail: Option<Vec<u8>>,
}

impl Animation {
    /// Project `TDLib`'s `messageAnimation`.
    #[must_use]
    pub fn from_tdlib(m: &TdMessageAnimation) -> Self {
        Self {
            caption: FormattedText::from_tdlib(&m.caption),
            file: FileRef::new(m.animation.animation.id),
            width: m.animation.width,
            height: m.animation.height,
            duration: m.animation.duration,
            file_name: crate::sanitize::scrub_line(&m.animation.file_name),
            mime_type: crate::sanitize::scrub_line(&m.animation.mime_type),
            minithumbnail: decode_minithumbnail(m.animation.minithumbnail.as_ref()),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // tests: panicking on a broken assumption is the point
mod tests {
    use super::*;
    use crate::model::message::MessageContent;
    use crate::model::test_support::td_file;
    use tdlib_rs::enums::MessageContent as TdMessageContent;
    use tdlib_rs::types::FormattedText as TdFormattedTextT;

    #[test]
    fn photo_content_keeps_the_largest_size_and_caption() {
        // Two sizes out of natural order; the projection must pick by area, not
        // position, so the 1280x720 size wins over the 90x90 thumbnail.
        let content = TdMessageContent::MessagePhoto(TdMessagePhoto {
            photo: tdlib_rs::types::Photo {
                sizes: vec![
                    tdlib_rs::types::PhotoSize {
                        photo: td_file(11),
                        width: 1280,
                        height: 720,
                        ..Default::default()
                    },
                    tdlib_rs::types::PhotoSize {
                        photo: td_file(10),
                        width: 90,
                        height: 90,
                        ..Default::default()
                    },
                ],
                ..Default::default()
            },
            caption: TdFormattedTextT {
                text: "sunset".to_owned(),
                entities: vec![],
            },
            ..Default::default()
        });
        assert_eq!(
            MessageContent::from_tdlib(&content),
            MessageContent::Photo(Photo {
                caption: FormattedText {
                    text: "sunset".to_owned(),
                    entities: vec![],
                },
                file: FileRef::new(11),
                width: 1280,
                height: 720,
            })
        );
    }

    #[test]
    fn photo_with_no_sizes_degrades_to_a_zero_ref() {
        let content = TdMessageContent::MessagePhoto(TdMessagePhoto::default());
        assert_eq!(
            MessageContent::from_tdlib(&content),
            MessageContent::Photo(Photo {
                caption: FormattedText::default(),
                file: FileRef::new(0),
                width: 0,
                height: 0,
            })
        );
    }

    #[test]
    fn video_content_projects_metadata_and_file() {
        let content = TdMessageContent::MessageVideo(TdMessageVideo {
            video: tdlib_rs::types::Video {
                duration: 12,
                width: 640,
                height: 480,
                file_name: "clip.mp4".to_owned(),
                mime_type: "video/mp4".to_owned(),
                has_stickers: false,
                supports_streaming: true,
                minithumbnail: None,
                thumbnail: None,
                video: td_file(7),
            },
            alternative_videos: vec![],
            storyboards: vec![],
            cover: None,
            start_timestamp: 0,
            caption: TdFormattedTextT {
                text: "watch".to_owned(),
                entities: vec![],
            },
            show_caption_above_media: false,
            has_spoiler: false,
            is_secret: false,
        });
        assert_eq!(
            MessageContent::from_tdlib(&content),
            MessageContent::Video(Video {
                caption: FormattedText {
                    text: "watch".to_owned(),
                    entities: vec![],
                },
                file: FileRef::new(7),
                width: 640,
                height: 480,
                duration: 12,
                file_name: "clip.mp4".to_owned(),
                mime_type: "video/mp4".to_owned(),
                minithumbnail: None,
            })
        );
    }

    #[test]
    fn document_content_projects_name_mime_and_file() {
        let content = TdMessageContent::MessageDocument(TdMessageDocument {
            document: tdlib_rs::types::Document {
                file_name: "report.pdf".to_owned(),
                mime_type: "application/pdf".to_owned(),
                minithumbnail: None,
                thumbnail: None,
                document: td_file(3),
            },
            caption: TdFormattedTextT::default(),
        });
        assert_eq!(
            MessageContent::from_tdlib(&content),
            MessageContent::Document(Document {
                caption: FormattedText::default(),
                file: FileRef::new(3),
                file_name: "report.pdf".to_owned(),
                mime_type: "application/pdf".to_owned(),
            })
        );
    }

    #[test]
    fn document_projection_scrubs_bidi_spoofed_file_name() {
        // A Trojan-Source file name (an override that flips `exe`/`txt`) is
        // neutralized on projection so the stored name reads honestly.
        let content = TdMessageContent::MessageDocument(TdMessageDocument {
            document: tdlib_rs::types::Document {
                file_name: "report_e\u{202e}xe.txt".to_owned(),
                mime_type: "application/pdf".to_owned(),
                minithumbnail: None,
                thumbnail: None,
                document: td_file(3),
            },
            caption: TdFormattedTextT::default(),
        });
        let MessageContent::Document(doc) = MessageContent::from_tdlib(&content) else {
            panic!("document content");
        };
        assert!(!doc.file_name.contains('\u{202e}'), "override removed");
        assert_eq!(doc.file_name, "report_e\u{fffd}xe.txt");
    }

    #[test]
    fn audio_content_projects_track_metadata_and_file() {
        let content = TdMessageContent::MessageAudio(TdMessageAudio {
            audio: tdlib_rs::types::Audio {
                duration: 200,
                title: "Song".to_owned(),
                performer: "Artist".to_owned(),
                file_name: "song.mp3".to_owned(),
                mime_type: "audio/mpeg".to_owned(),
                album_cover_minithumbnail: None,
                album_cover_thumbnail: None,
                external_album_covers: vec![],
                audio: td_file(5),
            },
            caption: TdFormattedTextT::default(),
        });
        assert_eq!(
            MessageContent::from_tdlib(&content),
            MessageContent::Audio(Audio {
                caption: FormattedText::default(),
                file: FileRef::new(5),
                duration: 200,
                title: "Song".to_owned(),
                performer: "Artist".to_owned(),
                file_name: "song.mp3".to_owned(),
                mime_type: "audio/mpeg".to_owned(),
            })
        );
    }

    #[test]
    fn voice_content_projects_duration_mime_and_file() {
        let content = TdMessageContent::MessageVoiceNote(TdMessageVoiceNote {
            voice_note: tdlib_rs::types::VoiceNote {
                duration: 8,
                mime_type: "audio/ogg".to_owned(),
                voice: td_file(9),
                ..Default::default()
            },
            caption: TdFormattedTextT::default(),
            ..Default::default()
        });
        assert_eq!(
            MessageContent::from_tdlib(&content),
            MessageContent::Voice(Voice {
                caption: FormattedText::default(),
                file: FileRef::new(9),
                duration: 8,
                mime_type: "audio/ogg".to_owned(),
            })
        );
    }

    #[test]
    fn sticker_content_projects_emoji_dimensions_and_file() {
        let content = TdMessageContent::MessageSticker(TdMessageSticker {
            sticker: tdlib_rs::types::Sticker {
                id: 0,
                set_id: 0,
                width: 512,
                height: 512,
                emoji: "😀".to_owned(),
                format: tdlib_rs::enums::StickerFormat::Webp,
                full_type: tdlib_rs::enums::StickerFullType::Regular(
                    tdlib_rs::types::StickerFullTypeRegular::default(),
                ),
                thumbnail: None,
                sticker: td_file(4),
            },
            is_premium: false,
        });
        assert_eq!(
            MessageContent::from_tdlib(&content),
            MessageContent::Sticker(Sticker {
                file: FileRef::new(4),
                width: 512,
                height: 512,
                emoji: "😀".to_owned(),
                is_static: true,
            })
        );
    }

    #[test]
    fn animated_sticker_projects_as_not_static() {
        let content = TdMessageContent::MessageSticker(TdMessageSticker {
            sticker: tdlib_rs::types::Sticker {
                id: 0,
                set_id: 0,
                width: 512,
                height: 512,
                emoji: "😀".to_owned(),
                format: tdlib_rs::enums::StickerFormat::Tgs,
                full_type: tdlib_rs::enums::StickerFullType::Regular(
                    tdlib_rs::types::StickerFullTypeRegular::default(),
                ),
                thumbnail: None,
                sticker: td_file(4),
            },
            is_premium: false,
        });
        let MessageContent::Sticker(sticker) = MessageContent::from_tdlib(&content) else {
            panic!("expected Sticker content");
        };
        assert!(!sticker.is_static);
    }

    #[test]
    fn animation_content_projects_metadata_and_file() {
        let content = TdMessageContent::MessageAnimation(TdMessageAnimation {
            animation: tdlib_rs::types::Animation {
                duration: 3,
                width: 320,
                height: 240,
                file_name: "loop.gif".to_owned(),
                mime_type: "video/mp4".to_owned(),
                has_stickers: false,
                minithumbnail: None,
                thumbnail: None,
                animation: td_file(6),
            },
            caption: TdFormattedTextT::default(),
            show_caption_above_media: false,
            has_spoiler: false,
            is_secret: false,
        });
        assert_eq!(
            MessageContent::from_tdlib(&content),
            MessageContent::Animation(Animation {
                caption: FormattedText::default(),
                file: FileRef::new(6),
                width: 320,
                height: 240,
                duration: 3,
                file_name: "loop.gif".to_owned(),
                mime_type: "video/mp4".to_owned(),
                minithumbnail: None,
            })
        );
    }

    #[test]
    fn video_and_animation_decode_their_minithumbnail_when_present() {
        use base64::Engine as _;
        let raw = b"not really a jpeg, just test bytes".to_vec();
        let encoded = base64::engine::general_purpose::STANDARD.encode(&raw);
        let minithumbnail = Some(tdlib_rs::types::Minithumbnail {
            width: 8,
            height: 8,
            data: encoded,
        });

        let video_content = TdMessageContent::MessageVideo(TdMessageVideo {
            video: tdlib_rs::types::Video {
                duration: 12,
                width: 640,
                height: 480,
                file_name: "clip.mp4".to_owned(),
                mime_type: "video/mp4".to_owned(),
                has_stickers: false,
                supports_streaming: true,
                minithumbnail: minithumbnail.clone(),
                thumbnail: None,
                video: td_file(7),
            },
            alternative_videos: vec![],
            storyboards: vec![],
            cover: None,
            start_timestamp: 0,
            caption: TdFormattedTextT::default(),
            show_caption_above_media: false,
            has_spoiler: false,
            is_secret: false,
        });
        let MessageContent::Video(video) = MessageContent::from_tdlib(&video_content) else {
            panic!("expected Video content");
        };
        assert_eq!(video.minithumbnail, Some(raw.clone()));

        let animation_content = TdMessageContent::MessageAnimation(TdMessageAnimation {
            animation: tdlib_rs::types::Animation {
                duration: 3,
                width: 320,
                height: 240,
                file_name: "loop.gif".to_owned(),
                mime_type: "video/mp4".to_owned(),
                has_stickers: false,
                minithumbnail,
                thumbnail: None,
                animation: td_file(6),
            },
            caption: TdFormattedTextT::default(),
            show_caption_above_media: false,
            has_spoiler: false,
            is_secret: false,
        });
        let MessageContent::Animation(animation) = MessageContent::from_tdlib(&animation_content)
        else {
            panic!("expected Animation content");
        };
        assert_eq!(animation.minithumbnail, Some(raw));
    }
}
