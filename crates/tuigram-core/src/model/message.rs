//! A message's content and lifecycle: [`SendState`], [`MessageContent`],
//! [`OutgoingMedia`], [`ReactionKind`], [`Reaction`], [`ReplyTo`], [`Message`],
//! [`Draft`].

use tdlib_rs::enums::{
    InputFile as TdInputFile, InputMessageContent as TdInputMessageContent,
    InputMessageReplyTo as TdInputMessageReplyTo, MessageContent as TdMessageContent,
    MessageReplyTo as TdMessageReplyTo, MessageSendingState as TdMessageSendingState,
    ReactionType as TdReactionType,
};
use tdlib_rs::types::{
    DraftMessage as TdDraftMessage, FormattedText as TdFormattedText, InputFileLocal,
    InputMessageAnimation, InputMessageAudio, InputMessageDocument, InputMessagePhoto,
    InputMessageReplyToMessage, InputMessageText, InputMessageVideo, InputMessageVoiceNote,
    Message as TdMessage, MessageInteractionInfo as TdMessageInteractionInfo,
    MessageReaction as TdMessageReaction, ReactionTypeCustomEmoji, ReactionTypeEmoji,
};

use super::content::{Contact, Location, Poll, Venue};
use super::media::{Animation, Audio, Document, FileRef, Photo, Sticker, Video, Voice};
use super::richtext::FormattedText;
use super::user::Sender;

/// The delivery state of a message tuigram is sending.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SendState {
    /// Delivered to the server — `TDLib` carries no sending state.
    Sent,
    /// Optimistically created locally, awaiting the server's acknowledgement.
    Pending,
    /// The server rejected the send; carries the error for display and retry.
    Failed {
        /// `TDLib` error code.
        code: i32,
        /// Human-readable error message.
        message: String,
    },
}

impl SendState {
    /// Project `TDLib`'s optional `MessageSendingState` (`None` ⇒ delivered).
    #[must_use]
    pub fn from_tdlib(state: Option<&TdMessageSendingState>) -> Self {
        match state {
            None => Self::Sent,
            Some(TdMessageSendingState::Pending(_)) => Self::Pending,
            Some(TdMessageSendingState::Failed(f)) => Self::Failed {
                code: f.error.code,
                message: f.error.message.clone(),
            },
        }
    }
}

/// The content of a message. tuigram models text, the common file-backed media
/// types ([`Photo`], [`Video`], [`Document`], [`Audio`], [`Voice`], [`Sticker`],
/// [`Animation`]), and the structured types ([`Location`], [`Venue`],
/// [`Contact`], [`Poll`]); everything else is [`MessageContent::Unsupported`]
/// carrying `TDLib`'s content type name.
#[derive(Clone, Debug, PartialEq)]
pub enum MessageContent {
    /// A text message, with its formatting entities.
    Text(FormattedText),
    /// A photo message.
    Photo(Photo),
    /// A video message.
    Video(Video),
    /// A document (arbitrary file) message.
    Document(Document),
    /// A music/audio message.
    Audio(Audio),
    /// A voice-note message.
    Voice(Voice),
    /// A sticker message.
    Sticker(Sticker),
    /// An animation (GIF/silent video) message.
    Animation(Animation),
    /// A location (a geographic point), live or static.
    Location(Location),
    /// A venue — a named place at a location.
    Venue(Venue),
    /// A shared contact card.
    Contact(Contact),
    /// A poll or quiz.
    Poll(Poll),
    /// A content type tuigram does not model yet. Carries `TDLib`'s type name
    /// (e.g. `"messageVideoNote"`) so callers can report it precisely.
    Unsupported(&'static str),
}

impl MessageContent {
    /// Project `TDLib`'s `MessageContent`. Total over the enum: a new `TDLib`
    /// content variant will fail to compile here until it is classified.
    #[must_use]
    pub fn from_tdlib(content: &TdMessageContent) -> Self {
        match content {
            TdMessageContent::MessageText(t) => Self::Text(FormattedText::from_tdlib(&t.text)),
            TdMessageContent::MessageAnimation(m) => Self::Animation(Animation::from_tdlib(m)),
            TdMessageContent::MessageAudio(m) => Self::Audio(Audio::from_tdlib(m)),
            TdMessageContent::MessageDocument(m) => Self::Document(Document::from_tdlib(m)),
            TdMessageContent::MessagePaidMedia(_) => Self::Unsupported("messagePaidMedia"),
            TdMessageContent::MessagePhoto(m) => Self::Photo(Photo::from_tdlib(m)),
            TdMessageContent::MessageSticker(m) => Self::Sticker(Sticker::from_tdlib(m)),
            TdMessageContent::MessageVideo(m) => Self::Video(Video::from_tdlib(m)),
            TdMessageContent::MessageVideoNote(_) => Self::Unsupported("messageVideoNote"),
            TdMessageContent::MessageVoiceNote(m) => Self::Voice(Voice::from_tdlib(m)),
            TdMessageContent::MessageExpiredPhoto => Self::Unsupported("messageExpiredPhoto"),
            TdMessageContent::MessageExpiredVideo => Self::Unsupported("messageExpiredVideo"),
            TdMessageContent::MessageExpiredVideoNote => {
                Self::Unsupported("messageExpiredVideoNote")
            }
            TdMessageContent::MessageExpiredVoiceNote => {
                Self::Unsupported("messageExpiredVoiceNote")
            }
            TdMessageContent::MessageLocation(m) => {
                Self::Location(Location::from_tdlib(&m.location))
            }
            TdMessageContent::MessageVenue(m) => Self::Venue(Venue::from_tdlib(&m.venue)),
            TdMessageContent::MessageContact(m) => Self::Contact(Contact::from_tdlib(&m.contact)),
            TdMessageContent::MessageAnimatedEmoji(_) => Self::Unsupported("messageAnimatedEmoji"),
            TdMessageContent::MessageDice(_) => Self::Unsupported("messageDice"),
            TdMessageContent::MessageGame(_) => Self::Unsupported("messageGame"),
            TdMessageContent::MessagePoll(m) => Self::Poll(Poll::from_tdlib(&m.poll)),
            TdMessageContent::MessageStakeDice(_) => Self::Unsupported("messageStakeDice"),
            TdMessageContent::MessageStory(_) => Self::Unsupported("messageStory"),
            TdMessageContent::MessageChecklist(_) => Self::Unsupported("messageChecklist"),
            TdMessageContent::MessageInvoice(_) => Self::Unsupported("messageInvoice"),
            TdMessageContent::MessageCall(_) => Self::Unsupported("messageCall"),
            TdMessageContent::MessageGroupCall(_) => Self::Unsupported("messageGroupCall"),
            TdMessageContent::MessageVideoChatScheduled(_) => {
                Self::Unsupported("messageVideoChatScheduled")
            }
            TdMessageContent::MessageVideoChatStarted(_) => {
                Self::Unsupported("messageVideoChatStarted")
            }
            TdMessageContent::MessageVideoChatEnded(_) => {
                Self::Unsupported("messageVideoChatEnded")
            }
            TdMessageContent::MessageInviteVideoChatParticipants(_) => {
                Self::Unsupported("messageInviteVideoChatParticipants")
            }
            TdMessageContent::MessageBasicGroupChatCreate(_) => {
                Self::Unsupported("messageBasicGroupChatCreate")
            }
            TdMessageContent::MessageSupergroupChatCreate(_) => {
                Self::Unsupported("messageSupergroupChatCreate")
            }
            TdMessageContent::MessageChatChangeTitle(_) => {
                Self::Unsupported("messageChatChangeTitle")
            }
            TdMessageContent::MessageChatChangePhoto(_) => {
                Self::Unsupported("messageChatChangePhoto")
            }
            TdMessageContent::MessageChatDeletePhoto => Self::Unsupported("messageChatDeletePhoto"),
            TdMessageContent::MessageChatOwnerLeft(_) => Self::Unsupported("messageChatOwnerLeft"),
            TdMessageContent::MessageChatOwnerChanged(_) => {
                Self::Unsupported("messageChatOwnerChanged")
            }
            TdMessageContent::MessageChatAddMembers(_) => {
                Self::Unsupported("messageChatAddMembers")
            }
            TdMessageContent::MessageChatJoinByLink => Self::Unsupported("messageChatJoinByLink"),
            TdMessageContent::MessageChatJoinByRequest => {
                Self::Unsupported("messageChatJoinByRequest")
            }
            TdMessageContent::MessageChatDeleteMember(_) => {
                Self::Unsupported("messageChatDeleteMember")
            }
            TdMessageContent::MessageChatUpgradeTo(_) => Self::Unsupported("messageChatUpgradeTo"),
            TdMessageContent::MessageChatUpgradeFrom(_) => {
                Self::Unsupported("messageChatUpgradeFrom")
            }
            TdMessageContent::MessagePinMessage(_) => Self::Unsupported("messagePinMessage"),
            TdMessageContent::MessageScreenshotTaken => Self::Unsupported("messageScreenshotTaken"),
            TdMessageContent::MessageChatSetBackground(_) => {
                Self::Unsupported("messageChatSetBackground")
            }
            TdMessageContent::MessageChatSetTheme(_) => Self::Unsupported("messageChatSetTheme"),
            TdMessageContent::MessageChatSetMessageAutoDeleteTime(_) => {
                Self::Unsupported("messageChatSetMessageAutoDeleteTime")
            }
            TdMessageContent::MessageChatBoost(_) => Self::Unsupported("messageChatBoost"),
            TdMessageContent::MessageForumTopicCreated(_) => {
                Self::Unsupported("messageForumTopicCreated")
            }
            TdMessageContent::MessageForumTopicEdited(_) => {
                Self::Unsupported("messageForumTopicEdited")
            }
            TdMessageContent::MessageForumTopicIsClosedToggled(_) => {
                Self::Unsupported("messageForumTopicIsClosedToggled")
            }
            TdMessageContent::MessageForumTopicIsHiddenToggled(_) => {
                Self::Unsupported("messageForumTopicIsHiddenToggled")
            }
            TdMessageContent::MessageSuggestProfilePhoto(_) => {
                Self::Unsupported("messageSuggestProfilePhoto")
            }
            TdMessageContent::MessageSuggestBirthdate(_) => {
                Self::Unsupported("messageSuggestBirthdate")
            }
            TdMessageContent::MessageCustomServiceAction(_) => {
                Self::Unsupported("messageCustomServiceAction")
            }
            TdMessageContent::MessageGameScore(_) => Self::Unsupported("messageGameScore"),
            TdMessageContent::MessagePaymentSuccessful(_) => {
                Self::Unsupported("messagePaymentSuccessful")
            }
            TdMessageContent::MessagePaymentSuccessfulBot(_) => {
                Self::Unsupported("messagePaymentSuccessfulBot")
            }
            TdMessageContent::MessagePaymentRefunded(_) => {
                Self::Unsupported("messagePaymentRefunded")
            }
            TdMessageContent::MessageGiftedPremium(_) => Self::Unsupported("messageGiftedPremium"),
            TdMessageContent::MessagePremiumGiftCode(_) => {
                Self::Unsupported("messagePremiumGiftCode")
            }
            TdMessageContent::MessageGiveawayCreated(_) => {
                Self::Unsupported("messageGiveawayCreated")
            }
            TdMessageContent::MessageGiveaway(_) => Self::Unsupported("messageGiveaway"),
            TdMessageContent::MessageGiveawayCompleted(_) => {
                Self::Unsupported("messageGiveawayCompleted")
            }
            TdMessageContent::MessageGiveawayWinners(_) => {
                Self::Unsupported("messageGiveawayWinners")
            }
            TdMessageContent::MessageGiftedStars(_) => Self::Unsupported("messageGiftedStars"),
            TdMessageContent::MessageGiftedTon(_) => Self::Unsupported("messageGiftedTon"),
            TdMessageContent::MessageGiveawayPrizeStars(_) => {
                Self::Unsupported("messageGiveawayPrizeStars")
            }
            TdMessageContent::MessageGift(_) => Self::Unsupported("messageGift"),
            TdMessageContent::MessageUpgradedGift(_) => Self::Unsupported("messageUpgradedGift"),
            TdMessageContent::MessageRefundedUpgradedGift(_) => {
                Self::Unsupported("messageRefundedUpgradedGift")
            }
            TdMessageContent::MessageUpgradedGiftPurchaseOffer(_) => {
                Self::Unsupported("messageUpgradedGiftPurchaseOffer")
            }
            TdMessageContent::MessageUpgradedGiftPurchaseOfferRejected(_) => {
                Self::Unsupported("messageUpgradedGiftPurchaseOfferRejected")
            }
            TdMessageContent::MessagePaidMessagesRefunded(_) => {
                Self::Unsupported("messagePaidMessagesRefunded")
            }
            TdMessageContent::MessagePaidMessagePriceChanged(_) => {
                Self::Unsupported("messagePaidMessagePriceChanged")
            }
            TdMessageContent::MessageDirectMessagePriceChanged(_) => {
                Self::Unsupported("messageDirectMessagePriceChanged")
            }
            TdMessageContent::MessageChecklistTasksDone(_) => {
                Self::Unsupported("messageChecklistTasksDone")
            }
            TdMessageContent::MessageChecklistTasksAdded(_) => {
                Self::Unsupported("messageChecklistTasksAdded")
            }
            TdMessageContent::MessageSuggestedPostApprovalFailed(_) => {
                Self::Unsupported("messageSuggestedPostApprovalFailed")
            }
            TdMessageContent::MessageSuggestedPostApproved(_) => {
                Self::Unsupported("messageSuggestedPostApproved")
            }
            TdMessageContent::MessageSuggestedPostDeclined(_) => {
                Self::Unsupported("messageSuggestedPostDeclined")
            }
            TdMessageContent::MessageSuggestedPostPaid(_) => {
                Self::Unsupported("messageSuggestedPostPaid")
            }
            TdMessageContent::MessageSuggestedPostRefunded(_) => {
                Self::Unsupported("messageSuggestedPostRefunded")
            }
            TdMessageContent::MessageContactRegistered => {
                Self::Unsupported("messageContactRegistered")
            }
            TdMessageContent::MessageUsersShared(_) => Self::Unsupported("messageUsersShared"),
            TdMessageContent::MessageChatShared(_) => Self::Unsupported("messageChatShared"),
            TdMessageContent::MessageBotWriteAccessAllowed(_) => {
                Self::Unsupported("messageBotWriteAccessAllowed")
            }
            TdMessageContent::MessageWebAppDataSent(_) => {
                Self::Unsupported("messageWebAppDataSent")
            }
            TdMessageContent::MessagePassportDataSent(_) => {
                Self::Unsupported("messagePassportDataSent")
            }
            TdMessageContent::MessageProximityAlertTriggered(_) => {
                Self::Unsupported("messageProximityAlertTriggered")
            }
            TdMessageContent::MessageUnsupported => Self::Unsupported("messageUnsupported"),
        }
    }

    /// The downloadable file this content references, if any — the key into the
    /// [`FileStore`](crate::files::FileStore) for a download or progress bar. The
    /// media variants (photo, video, document, audio, voice, sticker, animation)
    /// each carry one file; every other variant (text, location, poll, …) has none.
    ///
    /// The single source of truth for "which file backs this message", shared by the
    /// download driver (#120) and the UI's progress line so the two never drift.
    #[must_use]
    pub fn file(&self) -> Option<FileRef> {
        match self {
            Self::Photo(p) => Some(p.file),
            Self::Video(v) => Some(v.file),
            Self::Document(d) => Some(d.file),
            Self::Audio(a) => Some(a.file),
            Self::Voice(v) => Some(v.file),
            Self::Sticker(s) => Some(s.file),
            Self::Animation(a) => Some(a.file),
            Self::Text(_)
            | Self::Location(_)
            | Self::Venue(_)
            | Self::Contact(_)
            | Self::Poll(_)
            | Self::Unsupported(_) => None,
        }
    }
}

/// A file-backed message to send, from a local path — the write-side counterpart
/// to the read-side media [`MessageContent`] variants ([`Photo`], [`Video`], …).
///
/// Each variant carries the **local path** to upload and a caption (an empty
/// [`FormattedText`] when there is none). The remaining `TDLib` metadata a
/// `inputMessage*` accepts — dimensions, duration, thumbnails — is left for `TDLib`
/// to detect from the file itself: a headless client sends what it has on disk, so
/// it does not pre-measure media. [`to_tdlib`](Self::to_tdlib) projects each
/// variant into the matching `inputMessage*` content with that metadata defaulted.
///
/// Sticker is intentionally absent: a sticker is sent by file id from an installed
/// set, not by a local path, so it does not belong on this local-file seam.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutgoingMedia {
    /// A photo, from a local image file.
    Photo {
        /// Local path to the image to upload.
        path: String,
        /// Caption to send with it (empty for none).
        caption: FormattedText,
    },
    /// A video, from a local video file.
    Video {
        /// Local path to the video to upload.
        path: String,
        /// Caption to send with it (empty for none).
        caption: FormattedText,
    },
    /// A document (arbitrary file), from a local path.
    Document {
        /// Local path to the file to upload.
        path: String,
        /// Caption to send with it (empty for none).
        caption: FormattedText,
    },
    /// A music/audio track, from a local audio file.
    Audio {
        /// Local path to the audio to upload.
        path: String,
        /// Caption to send with it (empty for none).
        caption: FormattedText,
    },
    /// A voice note, from a local audio file.
    Voice {
        /// Local path to the audio to upload.
        path: String,
        /// Caption to send with it (empty for none).
        caption: FormattedText,
    },
    /// An animation (GIF/silent video), from a local file.
    Animation {
        /// Local path to the file to upload.
        path: String,
        /// Caption to send with it (empty for none).
        caption: FormattedText,
    },
}

impl OutgoingMedia {
    /// Project into the matching `TDLib` `inputMessage*` content, wrapping the local
    /// path as an [`InputFile::Local`](TdInputFile::Local) and carrying the caption
    /// (omitted when empty). All other metadata is defaulted so `TDLib` measures the
    /// file on upload; this never blocks on probing the media locally.
    #[must_use]
    pub fn to_tdlib(&self) -> TdInputMessageContent {
        match self {
            Self::Photo { path, caption } => {
                TdInputMessageContent::InputMessagePhoto(InputMessagePhoto {
                    photo: local_file(path),
                    thumbnail: None,
                    added_sticker_file_ids: vec![],
                    width: 0,
                    height: 0,
                    caption: optional_caption(caption),
                    show_caption_above_media: false,
                    self_destruct_type: None,
                    has_spoiler: false,
                })
            }
            Self::Video { path, caption } => {
                TdInputMessageContent::InputMessageVideo(InputMessageVideo {
                    video: local_file(path),
                    thumbnail: None,
                    cover: None,
                    start_timestamp: 0,
                    added_sticker_file_ids: vec![],
                    duration: 0,
                    width: 0,
                    height: 0,
                    supports_streaming: false,
                    caption: optional_caption(caption),
                    show_caption_above_media: false,
                    self_destruct_type: None,
                    has_spoiler: false,
                })
            }
            Self::Document { path, caption } => {
                TdInputMessageContent::InputMessageDocument(InputMessageDocument {
                    document: local_file(path),
                    thumbnail: None,
                    disable_content_type_detection: false,
                    caption: optional_caption(caption),
                })
            }
            Self::Audio { path, caption } => {
                TdInputMessageContent::InputMessageAudio(InputMessageAudio {
                    audio: local_file(path),
                    album_cover_thumbnail: None,
                    duration: 0,
                    title: String::new(),
                    performer: String::new(),
                    caption: optional_caption(caption),
                })
            }
            Self::Voice { path, caption } => {
                TdInputMessageContent::InputMessageVoiceNote(InputMessageVoiceNote {
                    voice_note: local_file(path),
                    duration: 0,
                    waveform: String::new(),
                    caption: optional_caption(caption),
                    self_destruct_type: None,
                })
            }
            Self::Animation { path, caption } => {
                TdInputMessageContent::InputMessageAnimation(InputMessageAnimation {
                    animation: local_file(path),
                    thumbnail: None,
                    added_sticker_file_ids: vec![],
                    duration: 0,
                    width: 0,
                    height: 0,
                    caption: optional_caption(caption),
                    show_caption_above_media: false,
                    has_spoiler: false,
                })
            }
        }
    }
}

/// Wrap a local path as a `TDLib` [`InputFile::Local`](TdInputFile::Local).
fn local_file(path: &str) -> TdInputFile {
    TdInputFile::Local(InputFileLocal {
        path: path.to_owned(),
    })
}

/// Project a caption, omitting it when empty: `TDLib` reads a `None` caption as no
/// caption, so an empty [`FormattedText`] must not be sent as an empty body.
fn optional_caption(caption: &FormattedText) -> Option<TdFormattedText> {
    (!caption.text.is_empty()).then(|| caption.to_tdlib())
}

/// A reaction's identity — tuigram's projection of `TDLib`'s `ReactionType`.
///
/// Total over the `TDLib` enum: a standard [`Emoji`](Self::Emoji), a
/// [`CustomEmoji`](Self::CustomEmoji) by its sticker id, or the channel
/// [`Paid`](Self::Paid) star reaction. Only the emoji case is sent over the
/// request seam ([`add_message_reaction`](crate::ReactionRequests::add_message_reaction));
/// the other two are read-only projections of reactions already on a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReactionKind {
    /// A standard emoji reaction, e.g. `"👍"`.
    Emoji(String),
    /// A custom emoji reaction, by its custom-emoji (sticker) id.
    CustomEmoji(i64),
    /// The channel paid ("star") reaction.
    Paid,
}

impl ReactionKind {
    /// Project `TDLib`'s `ReactionType`.
    #[must_use]
    pub fn from_tdlib(kind: &TdReactionType) -> Self {
        match kind {
            TdReactionType::Emoji(e) => Self::Emoji(crate::sanitize::scrub_line(&e.emoji)),
            TdReactionType::CustomEmoji(c) => Self::CustomEmoji(c.custom_emoji_id),
            TdReactionType::Paid => Self::Paid,
        }
    }

    /// Lower back to `TDLib`'s `ReactionType`, for adding or removing a reaction
    /// over the request seam — the inverse of [`from_tdlib`](Self::from_tdlib).
    #[must_use]
    pub fn to_tdlib(&self) -> TdReactionType {
        match self {
            Self::Emoji(emoji) => TdReactionType::Emoji(ReactionTypeEmoji {
                emoji: emoji.clone(),
            }),
            Self::CustomEmoji(id) => TdReactionType::CustomEmoji(ReactionTypeCustomEmoji {
                custom_emoji_id: *id,
            }),
            Self::Paid => TdReactionType::Paid,
        }
    }
}

/// One reaction bucket on a message — tuigram's projection of `TDLib`'s
/// `MessageReaction`: which reaction it is, how many added it, and whether
/// tuigram's own account is one of them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reaction {
    /// Which reaction this bucket counts.
    pub kind: ReactionKind,
    /// How many times the reaction was added.
    pub count: i32,
    /// Whether tuigram's account chose this reaction.
    pub is_chosen: bool,
}

impl Reaction {
    /// Project `TDLib`'s `MessageReaction`. The recent-sender list and paid-reactor
    /// details are dropped — a headless client needs the kind, the count, and
    /// whether it is our own choice, not who else reacted.
    #[must_use]
    pub fn from_tdlib(reaction: &TdMessageReaction) -> Self {
        Self {
            kind: ReactionKind::from_tdlib(&reaction.r#type),
            count: reaction.total_count,
            is_chosen: reaction.is_chosen,
        }
    }
}

/// Project a message's reactions out of its optional interaction info: the
/// buckets in `interaction_info.reactions`, in `TDLib`'s order, or empty when
/// either the interaction info or its reaction list is absent. Shared by
/// [`Message::from_tdlib`] and the `updateMessageInteractionInfo` fold in
/// [`MessageStore`](crate::messages::MessageStore).
pub(crate) fn reactions_from(info: Option<&TdMessageInteractionInfo>) -> Vec<Reaction> {
    info.and_then(|i| i.reactions.as_ref())
        .map(|r| r.reactions.iter().map(Reaction::from_tdlib).collect())
        .unwrap_or_default()
}

/// What a message replies to — tuigram's projection of `TDLib`'s
/// `MessageReplyTo` (#210). A construction-time field like
/// [`Message::content`]/[`Message::sender`]: `TDLib` has no live "reply
/// changed" update, so unlike [`Reaction`] this needs no store-reducer fold.
///
/// Resolving *who* was replied to and *what they said* is deliberately not
/// done here: it is a render-time lookup against the currently loaded
/// history (so it naturally catches up once a history page brings the
/// target message in, per #207's re-projection fix), not a value cached onto
/// this projection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReplyTo {
    /// A reply to a message, possibly in another chat/topic.
    Message {
        /// The chat the replied-to message belongs to (may differ from this
        /// message's own chat for a cross-chat/topic reply).
        chat_id: i64,
        /// The replied-to message's id.
        message_id: i64,
        /// The sender's manually-chosen quoted excerpt, if any (sanitized
        /// like any other prose).
        quote: Option<String>,
    },
    /// A reply to a story — out of scope to resolve or render; carried only
    /// so the projection stays total over `TDLib`'s enum, matching
    /// [`MessageContent::Unsupported`]'s convention.
    Unsupported(&'static str),
}

impl ReplyTo {
    /// Project `TDLib`'s `MessageReplyTo`.
    #[must_use]
    pub fn from_tdlib(reply: &TdMessageReplyTo) -> Self {
        match reply {
            TdMessageReplyTo::Message(m) => Self::Message {
                chat_id: m.chat_id,
                message_id: m.message_id,
                quote: m
                    .quote
                    .as_ref()
                    .map(|q| crate::sanitize::scrub_prose(&q.text.text)),
            },
            TdMessageReplyTo::Story(_) => Self::Unsupported("messageReplyToStory"),
        }
    }
}

/// A single message — tuigram's projection of `TDLib`'s `Message`.
///
/// Not `Eq`: a [`MessageContent::Location`] carries `f64` coordinates, so the
/// content — and everything that embeds it — is only `PartialEq`.
#[derive(Clone, Debug, PartialEq)]
pub struct Message {
    /// Message id, unique within its chat.
    pub id: i64,
    /// Id of the chat the message belongs to.
    pub chat_id: i64,
    /// Who sent the message.
    pub sender: Sender,
    /// Unix timestamp when the message was sent (`0` for unsent/scheduled).
    pub date: i32,
    /// Unix timestamp of the last edit (`0` if never edited).
    pub edit_date: i32,
    /// Whether tuigram's account sent this message.
    pub is_outgoing: bool,
    /// The message's content.
    pub content: MessageContent,
    /// Delivery state for outgoing messages.
    pub send_state: SendState,
    /// Reactions added to the message, one bucket per reaction, in `TDLib`'s
    /// order. Empty when the message has no reactions.
    pub reactions: Vec<Reaction>,
    /// What this message replies to, if it is a reply (#210).
    pub reply_to: Option<ReplyTo>,
}

impl Message {
    /// Project `TDLib`'s `Message`.
    #[must_use]
    pub fn from_tdlib(message: &TdMessage) -> Self {
        Self {
            id: message.id,
            chat_id: message.chat_id,
            sender: Sender::from_tdlib(&message.sender_id),
            date: message.date,
            edit_date: message.edit_date,
            is_outgoing: message.is_outgoing,
            content: MessageContent::from_tdlib(&message.content),
            send_state: SendState::from_tdlib(message.sending_state.as_ref()),
            reactions: reactions_from(message.interaction_info.as_ref()),
            reply_to: message.reply_to.as_ref().map(ReplyTo::from_tdlib),
        }
    }

    /// The message's text, if it is a text message — a convenience for the
    /// headless harness and tests.
    #[must_use]
    pub fn text(&self) -> Option<&str> {
        match &self.content {
            MessageContent::Text(t) => Some(&t.text),
            // A media caption is not the message's text body; this accessor is
            // about text messages, so anything else has no text to return.
            _ => None,
        }
    }
}

/// A chat's unsent compose draft — tuigram's projection of `TDLib`'s
/// `DraftMessage`. Telegram syncs this half-typed message across the account's
/// devices, so it is **chat state, not history**: it lives on the
/// [`Chat`](super::chat::Chat) snapshot and never enters the message store.
///
/// Phase 3 models a **text** draft — the realistic case for a keyboard-driven
/// client. `TDLib` also allows voice/video-note drafts, which carry no text and
/// project with an empty [`text`](Self::text); modeling those is a follow-up,
/// the same scope line as [`MessageContent`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Draft {
    /// The draft text with its formatting entities (empty for a non-text draft).
    pub text: FormattedText,
    /// The message this draft replies to, if any (by id, in the same chat).
    pub reply_to_message_id: Option<i64>,
    /// Unix timestamp when the draft was created.
    pub date: i32,
}

impl Draft {
    /// Project `TDLib`'s `DraftMessage`. A non-text draft (voice/video note, which
    /// Phase 3 does not model) keeps an empty `text`; a reply target other than
    /// an in-chat message (an external message or a story) is dropped, as those
    /// reply kinds are out of scope.
    #[must_use]
    pub fn from_tdlib(draft: &TdDraftMessage) -> Self {
        let text = match &draft.input_message_text {
            TdInputMessageContent::InputMessageText(t) => FormattedText::from_tdlib(&t.text),
            _ => FormattedText::default(),
        };
        let reply_to_message_id = match &draft.reply_to {
            Some(TdInputMessageReplyTo::Message(m)) => Some(m.message_id),
            _ => None,
        };
        Self {
            text,
            reply_to_message_id,
            date: draft.date,
        }
    }

    /// Lower back to `TDLib`'s `DraftMessage`, for pushing a draft over the seam.
    /// The inverse of [`from_tdlib`](Self::from_tdlib): the text becomes an
    /// `inputMessageText` (never clearing the draft itself — clearing is a
    /// `None` draft at the request, not a flag here), and a reply target becomes
    /// an in-chat `inputMessageReplyToMessage`.
    #[must_use]
    pub fn to_tdlib(&self) -> TdDraftMessage {
        TdDraftMessage {
            reply_to: self.reply_to_message_id.map(|message_id| {
                TdInputMessageReplyTo::Message(InputMessageReplyToMessage {
                    message_id,
                    quote: None,
                    checklist_task_id: 0,
                })
            }),
            date: self.date,
            input_message_text: TdInputMessageContent::InputMessageText(InputMessageText {
                text: self.text.to_tdlib(),
                link_preview_options: None,
                clear_draft: false,
            }),
            effect_id: 0,
            suggested_post_info: None,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // tests: panicking on a broken assumption is the point
mod tests {
    use super::*;
    use crate::model::richtext::{EntityKind, TextEntity};
    use crate::model::test_support::{td_file, td_message, td_text};
    use tdlib_rs::enums::{MessageSender as TdMessageSender, TextEntityType as TdTextEntityType};
    use tdlib_rs::types::{
        Error as TdError, FormattedText as TdFormattedTextT, MessageSenderUser,
        MessageSendingStateFailed, TextEntity as TdTextEntityT, TextEntityTypeTextUrl,
    };

    #[test]
    fn text_content_projects_with_its_entities() {
        let entities = vec![
            TdTextEntityT {
                offset: 0,
                length: 4,
                r#type: TdTextEntityType::Bold,
            },
            TdTextEntityT {
                offset: 5,
                length: 3,
                r#type: TdTextEntityType::TextUrl(TextEntityTypeTextUrl {
                    url: "https://t.me".to_owned(),
                }),
            },
        ];
        let content = MessageContent::from_tdlib(&td_text("bold ftw", entities));
        assert_eq!(
            content,
            MessageContent::Text(FormattedText {
                text: "bold ftw".to_owned(),
                entities: vec![
                    TextEntity {
                        offset: 0,
                        length: 4,
                        kind: EntityKind::Bold,
                    },
                    TextEntity {
                        offset: 5,
                        length: 3,
                        kind: EntityKind::TextUrl {
                            url: "https://t.me".to_owned(),
                        },
                    },
                ],
            })
        );
    }

    #[test]
    fn non_text_content_is_unsupported_with_its_tdlib_name() {
        // A bare service-message variant, no payload to build.
        assert_eq!(
            MessageContent::from_tdlib(&TdMessageContent::MessageScreenshotTaken),
            MessageContent::Unsupported("messageScreenshotTaken")
        );
        // TDLib's own "client too old to render this" content round-trips by name.
        assert_eq!(
            MessageContent::from_tdlib(&TdMessageContent::MessageUnsupported),
            MessageContent::Unsupported("messageUnsupported")
        );
        // A payload-bearing variant that tuigram deliberately does not model.
        let dice = TdMessageContent::MessageDice(tdlib_rs::types::MessageDice::default());
        assert_eq!(
            MessageContent::from_tdlib(&dice),
            MessageContent::Unsupported("messageDice")
        );
    }

    #[test]
    fn text_projection_scrubs_terminal_escapes_at_the_boundary() {
        // The seam is wired: a body with an ESC-introduced control sequence is
        // neutralized by the projection, so the store never holds hostile bytes.
        let content = td_text("hi\u{1b}]0;pwned\u{07}there", vec![]);
        let MessageContent::Text(text) = MessageContent::from_tdlib(&content) else {
            panic!("text content");
        };
        assert!(
            !text.text.chars().any(|c| c.is_control() && c != '\n'),
            "no control byte stored: {:?}",
            text.text
        );
        assert_eq!(text.text, "hi\u{fffd}]0;pwned\u{fffd}there");
    }

    #[test]
    fn unmodeled_media_still_falls_through_to_unsupported() {
        // Video notes and paid media are deliberately not modeled by #45; the
        // total mapping must keep surfacing them by name, never mis-map them as
        // one of the seven types above.
        let video_note = TdMessageContent::MessageVideoNote(tdlib_rs::types::MessageVideoNote {
            video_note: tdlib_rs::types::VideoNote {
                duration: 5,
                waveform: String::new(),
                length: 240,
                minithumbnail: None,
                thumbnail: None,
                speech_recognition_result: None,
                video: td_file(8),
            },
            is_viewed: false,
            is_secret: false,
        });
        assert_eq!(
            MessageContent::from_tdlib(&video_note),
            MessageContent::Unsupported("messageVideoNote")
        );
    }

    #[test]
    fn send_state_projects_none_pending_failed() {
        assert_eq!(SendState::from_tdlib(None), SendState::Sent);
        assert_eq!(
            SendState::from_tdlib(Some(&TdMessageSendingState::Pending(
                tdlib_rs::types::MessageSendingStatePending::default()
            ))),
            SendState::Pending
        );
        let failed = TdMessageSendingState::Failed(MessageSendingStateFailed {
            error: TdError {
                code: 403,
                message: "CHAT_WRITE_FORBIDDEN".to_owned(),
            },
            ..Default::default()
        });
        assert_eq!(
            SendState::from_tdlib(Some(&failed)),
            SendState::Failed {
                code: 403,
                message: "CHAT_WRITE_FORBIDDEN".to_owned(),
            }
        );
    }

    #[test]
    fn message_projects_all_fields_and_text_helper() {
        let td = td_message(
            42,
            -100,
            TdMessageSender::User(MessageSenderUser { user_id: 7 }),
            td_text("hello", vec![]),
            None,
            true,
        );
        let msg = Message::from_tdlib(&td);
        assert_eq!(msg.id, 42);
        assert_eq!(msg.chat_id, -100);
        assert_eq!(msg.sender, Sender::User(7));
        assert!(msg.is_outgoing);
        assert_eq!(msg.send_state, SendState::Sent);
        assert_eq!(msg.text(), Some("hello"));

        // A non-text message has no text.
        let photo = td_message(
            43,
            -100,
            TdMessageSender::User(MessageSenderUser { user_id: 7 }),
            TdMessageContent::MessageScreenshotTaken,
            None,
            false,
        );
        assert_eq!(Message::from_tdlib(&photo).text(), None);
    }

    #[test]
    fn draft_projects_text_and_reply_target_and_round_trips() {
        let td = TdDraftMessage {
            reply_to: Some(TdInputMessageReplyTo::Message(InputMessageReplyToMessage {
                message_id: 99,
                quote: None,
                checklist_task_id: 0,
            })),
            date: 1_700_000_500,
            input_message_text: TdInputMessageContent::InputMessageText(InputMessageText {
                text: TdFormattedTextT {
                    text: "half-typed".to_owned(),
                    entities: vec![],
                },
                link_preview_options: None,
                clear_draft: false,
            }),
            effect_id: 0,
            suggested_post_info: None,
        };
        let draft = Draft::from_tdlib(&td);
        assert_eq!(draft.text.text, "half-typed");
        assert_eq!(draft.reply_to_message_id, Some(99));
        assert_eq!(draft.date, 1_700_000_500);

        // to_tdlib then back is the identity over the fields the model carries.
        assert_eq!(Draft::from_tdlib(&draft.to_tdlib()), draft);
    }

    #[test]
    fn draft_without_a_reply_and_non_text_content_projects_empty() {
        // No reply target → None; a non-text draft (unmodeled) → empty text.
        let td = TdDraftMessage {
            reply_to: None,
            date: 0,
            input_message_text: TdInputMessageContent::InputMessageLocation(
                tdlib_rs::types::InputMessageLocation::default(),
            ),
            effect_id: 0,
            suggested_post_info: None,
        };
        let draft = Draft::from_tdlib(&td);
        assert_eq!(draft, Draft::default());
        assert!(draft.text.text.is_empty());
        assert_eq!(draft.reply_to_message_id, None);
    }

    /// The local path an `inputMessage*`'s file carries, for asserting it threads
    /// through unchanged. Panics on a non-local input file — the projection only
    /// ever builds [`TdInputFile::Local`].
    fn local_path(file: &TdInputFile) -> &str {
        match file {
            TdInputFile::Local(local) => &local.path,
            other => panic!("expected a local input file, got {other:?}"),
        }
    }

    fn caption_text(caption: &str) -> FormattedText {
        FormattedText {
            text: caption.to_owned(),
            entities: vec![],
        }
    }

    #[test]
    fn outgoing_photo_carries_its_local_path_and_caption() {
        let content = OutgoingMedia::Photo {
            path: "/tmp/cat.jpg".to_owned(),
            caption: caption_text("a cat"),
        }
        .to_tdlib();

        let TdInputMessageContent::InputMessagePhoto(photo) = content else {
            panic!("expected an input photo");
        };
        assert_eq!(local_path(&photo.photo), "/tmp/cat.jpg");
        assert_eq!(photo.caption.unwrap().text, "a cat");
    }

    #[test]
    fn outgoing_document_carries_its_local_path_and_caption() {
        // The document struct names its file field differently, so it is worth its
        // own check that the path lands in the right place.
        let content = OutgoingMedia::Document {
            path: "/tmp/report.pdf".to_owned(),
            caption: caption_text("q3"),
        }
        .to_tdlib();

        let TdInputMessageContent::InputMessageDocument(doc) = content else {
            panic!("expected an input document");
        };
        assert_eq!(local_path(&doc.document), "/tmp/report.pdf");
        assert_eq!(doc.caption.unwrap().text, "q3");
    }

    #[test]
    fn outgoing_media_omits_an_empty_caption() {
        // An empty caption must project to None, not an empty body — TDLib reads
        // None as "no caption".
        let content = OutgoingMedia::Voice {
            path: "/tmp/note.ogg".to_owned(),
            caption: caption_text(""),
        }
        .to_tdlib();

        let TdInputMessageContent::InputMessageVoiceNote(voice) = content else {
            panic!("expected an input voice note");
        };
        assert_eq!(local_path(&voice.voice_note), "/tmp/note.ogg");
        assert!(voice.caption.is_none());
    }

    #[test]
    fn each_outgoing_media_variant_maps_to_its_input_content() {
        let cap = caption_text("c");
        let path = "/tmp/x".to_owned();
        assert!(matches!(
            OutgoingMedia::Photo {
                path: path.clone(),
                caption: cap.clone(),
            }
            .to_tdlib(),
            TdInputMessageContent::InputMessagePhoto(_)
        ));
        assert!(matches!(
            OutgoingMedia::Video {
                path: path.clone(),
                caption: cap.clone(),
            }
            .to_tdlib(),
            TdInputMessageContent::InputMessageVideo(_)
        ));
        assert!(matches!(
            OutgoingMedia::Document {
                path: path.clone(),
                caption: cap.clone(),
            }
            .to_tdlib(),
            TdInputMessageContent::InputMessageDocument(_)
        ));
        assert!(matches!(
            OutgoingMedia::Audio {
                path: path.clone(),
                caption: cap.clone(),
            }
            .to_tdlib(),
            TdInputMessageContent::InputMessageAudio(_)
        ));
        assert!(matches!(
            OutgoingMedia::Voice {
                path: path.clone(),
                caption: cap.clone(),
            }
            .to_tdlib(),
            TdInputMessageContent::InputMessageVoiceNote(_)
        ));
        assert!(matches!(
            OutgoingMedia::Animation { path, caption: cap }.to_tdlib(),
            TdInputMessageContent::InputMessageAnimation(_)
        ));
    }

    #[test]
    fn reaction_kind_round_trips_through_every_variant() {
        use tdlib_rs::types::{ReactionTypeCustomEmoji, ReactionTypeEmoji};
        for kind in [
            ReactionKind::Emoji("👍".to_owned()),
            ReactionKind::CustomEmoji(987),
            ReactionKind::Paid,
        ] {
            // Projection is total and lossless for the kinds we model.
            assert_eq!(ReactionKind::from_tdlib(&kind.to_tdlib()), kind);
        }
        // And the projection reads each TDLib variant.
        assert_eq!(
            ReactionKind::from_tdlib(&TdReactionType::Emoji(ReactionTypeEmoji {
                emoji: "🔥".to_owned(),
            })),
            ReactionKind::Emoji("🔥".to_owned())
        );
        assert_eq!(
            ReactionKind::from_tdlib(&TdReactionType::CustomEmoji(ReactionTypeCustomEmoji {
                custom_emoji_id: 5,
            })),
            ReactionKind::CustomEmoji(5)
        );
        assert_eq!(
            ReactionKind::from_tdlib(&TdReactionType::Paid),
            ReactionKind::Paid
        );
    }

    #[test]
    fn message_projects_its_reactions_and_defaults_to_none() {
        use tdlib_rs::types::{
            MessageInteractionInfo, MessageReaction, MessageReactions, MessageText,
        };
        let text = TdMessageContent::MessageText(MessageText {
            text: TdFormattedTextT::default(),
            link_preview: None,
            link_preview_options: None,
        });
        // No interaction info → no reactions.
        let bare = td_message(
            1,
            10,
            TdMessageSender::User(MessageSenderUser { user_id: 1 }),
            text.clone(),
            None,
            false,
        );
        assert!(Message::from_tdlib(&bare).reactions.is_empty());

        // Interaction info with reaction buckets → projected in order.
        let mut with_reactions = td_message(
            2,
            10,
            TdMessageSender::User(MessageSenderUser { user_id: 1 }),
            text,
            None,
            false,
        );
        with_reactions.interaction_info = Some(MessageInteractionInfo {
            reactions: Some(MessageReactions {
                reactions: vec![MessageReaction {
                    r#type: TdReactionType::Emoji(tdlib_rs::types::ReactionTypeEmoji {
                        emoji: "👍".to_owned(),
                    }),
                    total_count: 4,
                    is_chosen: true,
                    used_sender_id: None,
                    recent_sender_ids: vec![],
                }],
                ..Default::default()
            }),
            ..Default::default()
        });
        assert_eq!(
            Message::from_tdlib(&with_reactions).reactions,
            vec![Reaction {
                kind: ReactionKind::Emoji("👍".to_owned()),
                count: 4,
                is_chosen: true,
            }]
        );
    }

    /// A message with no `reply_to` at all projects to `None` — the common
    /// case, and what every other `td_message`-built fixture already gets.
    #[test]
    fn message_with_no_reply_to_projects_to_none() {
        let bare = td_message(
            1,
            10,
            TdMessageSender::User(MessageSenderUser { user_id: 1 }),
            td_text("hi", vec![]),
            None,
            false,
        );
        assert_eq!(Message::from_tdlib(&bare).reply_to, None);
    }

    /// A reply within the same chat projects its target id and no quote (#210).
    #[test]
    fn message_projects_a_same_chat_reply() {
        use tdlib_rs::types::MessageReplyToMessage;

        let mut replying = td_message(
            2,
            10,
            TdMessageSender::User(MessageSenderUser { user_id: 1 }),
            td_text("sure", vec![]),
            None,
            false,
        );
        replying.reply_to = Some(TdMessageReplyTo::Message(MessageReplyToMessage {
            chat_id: 10,
            message_id: 1,
            quote: None,
            ..Default::default()
        }));
        assert_eq!(
            Message::from_tdlib(&replying).reply_to,
            Some(ReplyTo::Message {
                chat_id: 10,
                message_id: 1,
                quote: None,
            })
        );
    }

    /// A reply that carries an explicit, sender-chosen quote projects the
    /// quote's (sanitized) text alongside the target.
    #[test]
    fn message_projects_a_reply_s_explicit_quote() {
        use tdlib_rs::types::{MessageReplyToMessage, TextQuote};

        let mut replying = td_message(
            2,
            10,
            TdMessageSender::User(MessageSenderUser { user_id: 1 }),
            td_text("sure", vec![]),
            None,
            false,
        );
        replying.reply_to = Some(TdMessageReplyTo::Message(MessageReplyToMessage {
            chat_id: 10,
            message_id: 1,
            quote: Some(TextQuote {
                text: TdFormattedTextT {
                    text: "the important bit".to_owned(),
                    entities: vec![],
                },
                position: 0,
                is_manual: true,
            }),
            ..Default::default()
        }));
        assert_eq!(
            Message::from_tdlib(&replying).reply_to,
            Some(ReplyTo::Message {
                chat_id: 10,
                message_id: 1,
                quote: Some("the important bit".to_owned()),
            })
        );
    }

    /// A cross-chat reply carries the origin chat id, distinct from the
    /// reply's own chat — the render layer's cue that the target is not in
    /// the currently open chat's loaded window.
    #[test]
    fn message_projects_a_cross_chat_reply_s_origin_chat() {
        use tdlib_rs::types::MessageReplyToMessage;

        let mut replying = td_message(
            2,
            10,
            TdMessageSender::User(MessageSenderUser { user_id: 1 }),
            td_text("sure", vec![]),
            None,
            false,
        );
        replying.reply_to = Some(TdMessageReplyTo::Message(MessageReplyToMessage {
            chat_id: 99,
            message_id: 1,
            quote: None,
            ..Default::default()
        }));
        assert_eq!(
            Message::from_tdlib(&replying).reply_to,
            Some(ReplyTo::Message {
                chat_id: 99,
                message_id: 1,
                quote: None,
            })
        );
    }

    /// A reply to a story is out of scope to resolve/render, but the
    /// projection stays total over `TDLib`'s enum rather than silently
    /// dropping it.
    #[test]
    fn message_projects_a_story_reply_as_unsupported() {
        use tdlib_rs::types::MessageReplyToStory;

        let mut replying = td_message(
            2,
            10,
            TdMessageSender::User(MessageSenderUser { user_id: 1 }),
            td_text("nice story", vec![]),
            None,
            false,
        );
        replying.reply_to = Some(TdMessageReplyTo::Story(MessageReplyToStory {
            story_poster_chat_id: 5,
            story_id: 42,
        }));
        assert_eq!(
            Message::from_tdlib(&replying).reply_to,
            Some(ReplyTo::Unsupported("messageReplyToStory"))
        );
    }

    #[test]
    fn message_content_exposes_the_backing_file_only_for_media() {
        let caption = FormattedText {
            text: String::new(),
            entities: vec![],
        };
        // Each media variant surfaces its own file id; a document is representative.
        let photo = MessageContent::Photo(Photo {
            caption: caption.clone(),
            file: FileRef::new(7),
            width: 0,
            height: 0,
        });
        assert_eq!(photo.file(), Some(FileRef::new(7)));
        let document = MessageContent::Document(Document {
            caption,
            file: FileRef::new(9),
            file_name: "x".to_owned(),
            mime_type: String::new(),
        });
        assert_eq!(document.file(), Some(FileRef::new(9)));
        // Non-file content has none.
        assert_eq!(
            MessageContent::Text(FormattedText {
                text: "hi".to_owned(),
                entities: vec![],
            })
            .file(),
            None
        );
        assert_eq!(MessageContent::Unsupported("messageDice").file(), None);
    }
}
