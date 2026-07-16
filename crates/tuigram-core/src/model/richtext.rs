//! Formatted text: [`EntityKind`], [`TextEntity`], [`FormattedText`].

use tdlib_rs::enums::TextEntityType as TdTextEntityType;
use tdlib_rs::types::{FormattedText as TdFormattedText, TextEntity as TdTextEntity};

/// The kind of a formatting [`TextEntity`] — tuigram's projection of `TDLib`'s
/// `TextEntityType`. Data-bearing entities keep their payload; the rest are
/// pure styling or auto-detected spans.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntityKind {
    /// `@username` mention.
    Mention,
    /// `#hashtag`.
    Hashtag,
    /// `$CASHTAG`.
    Cashtag,
    /// `/botCommand`.
    BotCommand,
    /// A bare URL.
    Url,
    /// An email address.
    EmailAddress,
    /// A phone number.
    PhoneNumber,
    /// A bank card number.
    BankCardNumber,
    /// Bold text.
    Bold,
    /// Italic text.
    Italic,
    /// Underlined text.
    Underline,
    /// Strikethrough text.
    Strikethrough,
    /// Spoiler (hidden until tapped).
    Spoiler,
    /// Inline monospace code.
    Code,
    /// A preformatted block.
    Pre,
    /// A preformatted block tagged with a programming language.
    PreCode {
        /// The programming language tag.
        language: String,
    },
    /// A block quote.
    BlockQuote,
    /// A collapsible block quote.
    ExpandableBlockQuote,
    /// A text link to `url`.
    TextUrl {
        /// The link target.
        url: String,
    },
    /// A mention of a user with no username, by id.
    MentionName {
        /// `TDLib` id of the mentioned user.
        user_id: i64,
    },
    /// A custom emoji, by sticker id.
    CustomEmoji {
        /// `TDLib` id of the custom emoji sticker.
        custom_emoji_id: i64,
    },
    /// A clickable media timestamp, in seconds.
    MediaTimestamp {
        /// Offset into the media, in seconds.
        media_timestamp: i32,
    },
}

impl EntityKind {
    /// Project `TDLib`'s `TextEntityType`.
    #[must_use]
    pub fn from_tdlib(kind: &TdTextEntityType) -> Self {
        match kind {
            TdTextEntityType::Mention => Self::Mention,
            TdTextEntityType::Hashtag => Self::Hashtag,
            TdTextEntityType::Cashtag => Self::Cashtag,
            TdTextEntityType::BotCommand => Self::BotCommand,
            TdTextEntityType::Url => Self::Url,
            TdTextEntityType::EmailAddress => Self::EmailAddress,
            TdTextEntityType::PhoneNumber => Self::PhoneNumber,
            TdTextEntityType::BankCardNumber => Self::BankCardNumber,
            TdTextEntityType::Bold => Self::Bold,
            TdTextEntityType::Italic => Self::Italic,
            TdTextEntityType::Underline => Self::Underline,
            TdTextEntityType::Strikethrough => Self::Strikethrough,
            TdTextEntityType::Spoiler => Self::Spoiler,
            TdTextEntityType::Code => Self::Code,
            TdTextEntityType::Pre => Self::Pre,
            TdTextEntityType::PreCode(p) => Self::PreCode {
                language: p.language.clone(),
            },
            TdTextEntityType::BlockQuote => Self::BlockQuote,
            TdTextEntityType::ExpandableBlockQuote => Self::ExpandableBlockQuote,
            TdTextEntityType::TextUrl(u) => Self::TextUrl { url: u.url.clone() },
            TdTextEntityType::MentionName(m) => Self::MentionName { user_id: m.user_id },
            TdTextEntityType::CustomEmoji(c) => Self::CustomEmoji {
                custom_emoji_id: c.custom_emoji_id,
            },
            TdTextEntityType::MediaTimestamp(t) => Self::MediaTimestamp {
                media_timestamp: t.media_timestamp,
            },
        }
    }

    /// Project back to `TDLib`'s `TextEntityType`, for entities on outgoing text.
    /// Total, mirroring [`EntityKind::from_tdlib`]: a new variant added here must
    /// be sendable too, or it fails to compile.
    #[must_use]
    pub fn to_tdlib(&self) -> TdTextEntityType {
        use tdlib_rs::types::{
            TextEntityTypeCustomEmoji, TextEntityTypeMediaTimestamp, TextEntityTypeMentionName,
            TextEntityTypePreCode, TextEntityTypeTextUrl,
        };
        match self {
            Self::Mention => TdTextEntityType::Mention,
            Self::Hashtag => TdTextEntityType::Hashtag,
            Self::Cashtag => TdTextEntityType::Cashtag,
            Self::BotCommand => TdTextEntityType::BotCommand,
            Self::Url => TdTextEntityType::Url,
            Self::EmailAddress => TdTextEntityType::EmailAddress,
            Self::PhoneNumber => TdTextEntityType::PhoneNumber,
            Self::BankCardNumber => TdTextEntityType::BankCardNumber,
            Self::Bold => TdTextEntityType::Bold,
            Self::Italic => TdTextEntityType::Italic,
            Self::Underline => TdTextEntityType::Underline,
            Self::Strikethrough => TdTextEntityType::Strikethrough,
            Self::Spoiler => TdTextEntityType::Spoiler,
            Self::Code => TdTextEntityType::Code,
            Self::Pre => TdTextEntityType::Pre,
            Self::PreCode { language } => TdTextEntityType::PreCode(TextEntityTypePreCode {
                language: language.clone(),
            }),
            Self::BlockQuote => TdTextEntityType::BlockQuote,
            Self::ExpandableBlockQuote => TdTextEntityType::ExpandableBlockQuote,
            Self::TextUrl { url } => {
                TdTextEntityType::TextUrl(TextEntityTypeTextUrl { url: url.clone() })
            }
            Self::MentionName { user_id } => {
                TdTextEntityType::MentionName(TextEntityTypeMentionName { user_id: *user_id })
            }
            Self::CustomEmoji { custom_emoji_id } => {
                TdTextEntityType::CustomEmoji(TextEntityTypeCustomEmoji {
                    custom_emoji_id: *custom_emoji_id,
                })
            }
            Self::MediaTimestamp { media_timestamp } => {
                TdTextEntityType::MediaTimestamp(TextEntityTypeMediaTimestamp {
                    media_timestamp: *media_timestamp,
                })
            }
        }
    }
}

/// One formatting span within a [`FormattedText`]. Offsets and lengths are in
/// UTF-16 code units, as `TDLib` reports them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextEntity {
    /// Start of the span, in UTF-16 code units.
    pub offset: i32,
    /// Length of the span, in UTF-16 code units.
    pub length: i32,
    /// What kind of formatting the span carries.
    pub kind: EntityKind,
}

impl TextEntity {
    /// Project `TDLib`'s `TextEntity`.
    #[must_use]
    pub fn from_tdlib(entity: &TdTextEntity) -> Self {
        Self {
            offset: entity.offset,
            length: entity.length,
            kind: EntityKind::from_tdlib(&entity.r#type),
        }
    }

    /// Project back to `TDLib`'s `TextEntity`, for an outgoing formatted message.
    #[must_use]
    pub fn to_tdlib(&self) -> TdTextEntity {
        TdTextEntity {
            offset: self.offset,
            length: self.length,
            r#type: self.kind.to_tdlib(),
        }
    }
}

/// Text with its formatting entities — tuigram's projection of `TDLib`'s
/// `FormattedText`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FormattedText {
    /// The raw text.
    pub text: String,
    /// Formatting spans over `text`.
    pub entities: Vec<TextEntity>,
}

impl FormattedText {
    /// Project `TDLib`'s `FormattedText`.
    #[must_use]
    pub fn from_tdlib(text: &TdFormattedText) -> Self {
        // Trust boundary: message bodies and captions are attacker-controlled and
        // end up in terminal cells, so neutralize control sequences here — once,
        // where every text/caption/poll projection funnels through. Replacing
        // controls one-for-one keeps the entities' UTF-16 offsets aligned.
        Self {
            text: crate::sanitize::scrub_prose(&text.text),
            entities: text.entities.iter().map(TextEntity::from_tdlib).collect(),
        }
    }

    /// Project back to `TDLib`'s `FormattedText`, for sending. A plain string with
    /// no entities round-trips as bare text.
    #[must_use]
    pub fn to_tdlib(&self) -> TdFormattedText {
        TdFormattedText {
            text: self.text.clone(),
            entities: self.entities.iter().map(TextEntity::to_tdlib).collect(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // tests: panicking on a broken assumption is the point
mod tests {
    use super::*;

    #[test]
    fn formatted_text_round_trips_through_tdlib_for_sending() {
        // Representative entities: bare, payload-bearing (data + styling), so the
        // reverse projection is exercised across the variant shapes.
        let ft = FormattedText {
            text: "bold link code".to_owned(),
            entities: vec![
                TextEntity {
                    offset: 0,
                    length: 4,
                    kind: EntityKind::Bold,
                },
                TextEntity {
                    offset: 5,
                    length: 4,
                    kind: EntityKind::TextUrl {
                        url: "https://t.me".to_owned(),
                    },
                },
                TextEntity {
                    offset: 10,
                    length: 4,
                    kind: EntityKind::PreCode {
                        language: "rust".to_owned(),
                    },
                },
            ],
        };
        // to_tdlib then back is the identity — the projections mirror each other.
        assert_eq!(FormattedText::from_tdlib(&ft.to_tdlib()), ft);
    }
}
