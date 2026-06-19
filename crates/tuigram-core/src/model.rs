//! tuigram's normalized headless data model.
//!
//! The rest of the crate — and the Ratatui front-end in Phases 4–5 — depends on
//! these types, not on `tdlib_rs` shapes directly. That is the same insulation
//! [`AuthState`](crate::auth::AuthState) gives the login flow: a stable, minimal
//! surface we own, projected from TDLib by a `from_tdlib` constructor.
//!
//! Each projection is **total** over its TDLib enum. Anything Phase 3 does not
//! model maps to an explicit `Unsupported(name)` carrying TDLib's own type name,
//! and the projections use no catch-all `_` arm — so a `tdlib-rs` upgrade that
//! adds a variant fails to compile here until it is classified, never silently
//! dropped or misclassified.
//!
//! Phase 3 models **text** in full (with its formatting entities); every other
//! message content is `Unsupported`, for follow-up issues. Media, reactions,
//! forwards, replies, and service messages are out of scope for this model.

use tdlib_rs::enums::{
    ChatList as TdChatList, ChatType as TdChatType, MessageContent as TdMessageContent,
    MessageSender as TdMessageSender, MessageSendingState as TdMessageSendingState,
    TextEntityType as TdTextEntityType, UserStatus as TdUserStatus, UserType as TdUserType,
};
use tdlib_rs::types::{
    Chat as TdChat, ChatPosition as TdChatPosition, FormattedText as TdFormattedText,
    Message as TdMessage, TextEntity as TdTextEntity, User as TdUser,
};

/// Who sent a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Sender {
    /// A user, by user id.
    User(i64),
    /// A chat — channel posts and anonymous group admins — by chat id.
    Chat(i64),
}

impl Sender {
    /// Project TDLib's `MessageSender`.
    #[must_use]
    pub fn from_tdlib(sender: &TdMessageSender) -> Self {
        match sender {
            TdMessageSender::User(u) => Self::User(u.user_id),
            TdMessageSender::Chat(c) => Self::Chat(c.chat_id),
        }
    }
}

/// A user's online presence — tuigram's projection of TDLib's `UserStatus`.
///
/// Total over the enum with no catch-all, the same discipline as the message
/// content projection: a new `UserStatus` variant fails to compile here until it
/// is classified. The "recently / last week / last month" buckets carry no
/// timestamp on purpose — TDLib hides the exact time for those and surfaces only
/// the bucket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Presence {
    /// Status never set, or hidden from us entirely.
    Never,
    /// Online until `expires` (Unix timestamp).
    Online { expires: i32 },
    /// Offline; last seen at `was_online` (Unix timestamp).
    Offline { was_online: i32 },
    /// Online recently — within a few days — with the exact time hidden.
    Recently,
    /// Online within the last week, with the exact time hidden.
    LastWeek,
    /// Online within the last month, with the exact time hidden.
    LastMonth,
}

impl Presence {
    /// Project TDLib's `UserStatus`.
    #[must_use]
    pub fn from_tdlib(status: &TdUserStatus) -> Self {
        match status {
            TdUserStatus::Empty => Self::Never,
            TdUserStatus::Online(s) => Self::Online { expires: s.expires },
            TdUserStatus::Offline(s) => Self::Offline {
                was_online: s.was_online,
            },
            TdUserStatus::Recently(_) => Self::Recently,
            TdUserStatus::LastWeek(_) => Self::LastWeek,
            TdUserStatus::LastMonth(_) => Self::LastMonth,
        }
    }
}

/// What kind of account a [`User`] is — tuigram's projection of TDLib's
/// `UserType`. Total over the enum, no catch-all. The bot payload is dropped:
/// the model only needs to know *that* an account is a bot, not its bot details.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UserKind {
    /// A regular user account.
    Regular,
    /// A deleted account — only the id survives; renders as "Deleted Account".
    Deleted,
    /// A bot.
    Bot,
    /// An inaccessible account: not deleted, but with no information available.
    /// TDLib says to treat it exactly like a deleted user.
    Unknown,
}

impl UserKind {
    /// Project TDLib's `UserType`.
    #[must_use]
    pub fn from_tdlib(kind: &TdUserType) -> Self {
        match kind {
            TdUserType::Regular => Self::Regular,
            TdUserType::Deleted => Self::Deleted,
            TdUserType::Bot(_) => Self::Bot,
            TdUserType::Unknown => Self::Unknown,
        }
    }
}

/// A user — tuigram's projection of TDLib's `User`, carrying what a sender line
/// and a private-chat header need to read as a name instead of a bare id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct User {
    /// User id.
    pub id: i64,
    /// First name (may be empty for a deleted account).
    pub first_name: String,
    /// Last name (often empty).
    pub last_name: String,
    /// Active usernames, primary first; empty if the user has none.
    pub usernames: Vec<String>,
    /// Phone number, if the user shares one with this account.
    pub phone_number: Option<String>,
    /// Whether the user is in this account's contacts.
    pub is_contact: bool,
    /// What kind of account this is.
    pub kind: UserKind,
    /// Current online presence.
    pub status: Presence,
}

impl User {
    /// Project TDLib's `User`. An empty phone number becomes `None`, and the
    /// usernames flatten to the active list (primary first).
    #[must_use]
    pub fn from_tdlib(user: &TdUser) -> Self {
        Self {
            id: user.id,
            first_name: user.first_name.clone(),
            last_name: user.last_name.clone(),
            usernames: user
                .usernames
                .as_ref()
                .map(|u| u.active_usernames.clone())
                .unwrap_or_default(),
            phone_number: Some(user.phone_number.clone()).filter(|p| !p.is_empty()),
            is_contact: user.is_contact,
            kind: UserKind::from_tdlib(&user.r#type),
            status: Presence::from_tdlib(&user.status),
        }
    }

    /// The user's primary username (without the leading `@`), if any.
    #[must_use]
    pub fn username(&self) -> Option<&str> {
        self.usernames.first().map(String::as_str)
    }

    /// A human-readable name to render in place of the user's id: the full name
    /// if set, else the primary `@username`, else `"Deleted Account"` for a
    /// deleted or inaccessible account, else a bare `User {id}` as a last resort.
    #[must_use]
    pub fn display_name(&self) -> String {
        let full = format!("{} {}", self.first_name, self.last_name);
        let full = full.trim();
        if !full.is_empty() {
            return full.to_owned();
        }
        if let Some(username) = self.username() {
            return format!("@{username}");
        }
        if matches!(self.kind, UserKind::Deleted | UserKind::Unknown) {
            return "Deleted Account".to_owned();
        }
        format!("User {}", self.id)
    }
}

/// A chat's classification, with the underlying TDLib id for its kind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChatKind {
    /// One-to-one chat with a user.
    Private { user_id: i64 },
    /// Basic group (up to 200 members).
    BasicGroup { basic_group_id: i64 },
    /// Supergroup (large group).
    Supergroup { supergroup_id: i64 },
    /// Broadcast channel — a supergroup flagged as a channel.
    Channel { supergroup_id: i64 },
    /// End-to-end encrypted secret chat. Out of Phase 3 messaging scope.
    Secret { secret_chat_id: i32, user_id: i64 },
}

impl ChatKind {
    /// Project TDLib's `ChatType`. A supergroup with `is_channel` set becomes a
    /// [`ChatKind::Channel`]; the two share TDLib's supergroup id space.
    #[must_use]
    pub fn from_tdlib(kind: &TdChatType) -> Self {
        match kind {
            TdChatType::Private(p) => Self::Private { user_id: p.user_id },
            TdChatType::BasicGroup(b) => Self::BasicGroup {
                basic_group_id: b.basic_group_id,
            },
            TdChatType::Supergroup(s) if s.is_channel => Self::Channel {
                supergroup_id: s.supergroup_id,
            },
            TdChatType::Supergroup(s) => Self::Supergroup {
                supergroup_id: s.supergroup_id,
            },
            TdChatType::Secret(s) => Self::Secret {
                secret_chat_id: s.secret_chat_id,
                user_id: s.user_id,
            },
        }
    }
}

/// Which chat list a [`ChatPosition`] belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChatListKind {
    /// The Main list — tuigram's primary view.
    Main,
    /// The Archive list.
    Archive,
    /// A user-defined folder, by its folder id.
    Folder(i32),
}

impl ChatListKind {
    /// Project TDLib's `ChatList`.
    #[must_use]
    pub fn from_tdlib(list: &TdChatList) -> Self {
        match list {
            TdChatList::Main => Self::Main,
            TdChatList::Archive => Self::Archive,
            TdChatList::Folder(f) => Self::Folder(f.chat_folder_id),
        }
    }
}

/// A chat's position in one chat list. The `(order, chat id)` pair sorts a list
/// in descending order; pinned chats float to the top.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatPosition {
    /// The list this position is in.
    pub list: ChatListKind,
    /// Ordering key within the list; higher sorts first.
    pub order: i64,
    /// Whether the chat is pinned in this list.
    pub is_pinned: bool,
}

impl ChatPosition {
    /// Project TDLib's `ChatPosition`.
    #[must_use]
    pub fn from_tdlib(position: &TdChatPosition) -> Self {
        Self {
            list: ChatListKind::from_tdlib(&position.list),
            order: position.order,
            is_pinned: position.is_pinned,
        }
    }
}

/// The delivery state of a message tuigram is sending.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SendState {
    /// Delivered to the server — TDLib carries no sending state.
    Sent,
    /// Optimistically created locally, awaiting the server's acknowledgement.
    Pending,
    /// The server rejected the send; carries the error for display and retry.
    Failed { code: i32, message: String },
}

impl SendState {
    /// Project TDLib's optional `MessageSendingState` (`None` ⇒ delivered).
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

/// The kind of a formatting [`TextEntity`] — tuigram's projection of TDLib's
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
    PreCode { language: String },
    /// A block quote.
    BlockQuote,
    /// A collapsible block quote.
    ExpandableBlockQuote,
    /// A text link to `url`.
    TextUrl { url: String },
    /// A mention of a user with no username, by id.
    MentionName { user_id: i64 },
    /// A custom emoji, by sticker id.
    CustomEmoji { custom_emoji_id: i64 },
    /// A clickable media timestamp, in seconds.
    MediaTimestamp { media_timestamp: i32 },
}

impl EntityKind {
    /// Project TDLib's `TextEntityType`.
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

    /// Project back to TDLib's `TextEntityType`, for entities on outgoing text.
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
/// UTF-16 code units, as TDLib reports them.
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
    /// Project TDLib's `TextEntity`.
    #[must_use]
    pub fn from_tdlib(entity: &TdTextEntity) -> Self {
        Self {
            offset: entity.offset,
            length: entity.length,
            kind: EntityKind::from_tdlib(&entity.r#type),
        }
    }

    /// Project back to TDLib's `TextEntity`, for an outgoing formatted message.
    #[must_use]
    pub fn to_tdlib(&self) -> TdTextEntity {
        TdTextEntity {
            offset: self.offset,
            length: self.length,
            r#type: self.kind.to_tdlib(),
        }
    }
}

/// Text with its formatting entities — tuigram's projection of TDLib's
/// `FormattedText`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FormattedText {
    /// The raw text.
    pub text: String,
    /// Formatting spans over `text`.
    pub entities: Vec<TextEntity>,
}

impl FormattedText {
    /// Project TDLib's `FormattedText`.
    #[must_use]
    pub fn from_tdlib(text: &TdFormattedText) -> Self {
        Self {
            text: text.text.clone(),
            entities: text.entities.iter().map(TextEntity::from_tdlib).collect(),
        }
    }

    /// Project back to TDLib's `FormattedText`, for sending. A plain string with
    /// no entities round-trips as bare text.
    #[must_use]
    pub fn to_tdlib(&self) -> TdFormattedText {
        TdFormattedText {
            text: self.text.clone(),
            entities: self.entities.iter().map(TextEntity::to_tdlib).collect(),
        }
    }
}

/// The content of a message. Phase 3 models text; everything else is
/// [`MessageContent::Unsupported`] carrying TDLib's content type name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MessageContent {
    /// A text message, with its formatting entities.
    Text(FormattedText),
    /// A content type tuigram does not model yet. Carries TDLib's type name
    /// (e.g. `"messagePhoto"`) so callers can report it precisely.
    Unsupported(&'static str),
}

impl MessageContent {
    /// Project TDLib's `MessageContent`. Total over the enum: a new TDLib
    /// content variant will fail to compile here until it is classified.
    #[must_use]
    pub fn from_tdlib(content: &TdMessageContent) -> Self {
        match content {
            TdMessageContent::MessageText(t) => Self::Text(FormattedText::from_tdlib(&t.text)),
            TdMessageContent::MessageAnimation(_) => Self::Unsupported("messageAnimation"),
            TdMessageContent::MessageAudio(_) => Self::Unsupported("messageAudio"),
            TdMessageContent::MessageDocument(_) => Self::Unsupported("messageDocument"),
            TdMessageContent::MessagePaidMedia(_) => Self::Unsupported("messagePaidMedia"),
            TdMessageContent::MessagePhoto(_) => Self::Unsupported("messagePhoto"),
            TdMessageContent::MessageSticker(_) => Self::Unsupported("messageSticker"),
            TdMessageContent::MessageVideo(_) => Self::Unsupported("messageVideo"),
            TdMessageContent::MessageVideoNote(_) => Self::Unsupported("messageVideoNote"),
            TdMessageContent::MessageVoiceNote(_) => Self::Unsupported("messageVoiceNote"),
            TdMessageContent::MessageExpiredPhoto => Self::Unsupported("messageExpiredPhoto"),
            TdMessageContent::MessageExpiredVideo => Self::Unsupported("messageExpiredVideo"),
            TdMessageContent::MessageExpiredVideoNote => {
                Self::Unsupported("messageExpiredVideoNote")
            }
            TdMessageContent::MessageExpiredVoiceNote => {
                Self::Unsupported("messageExpiredVoiceNote")
            }
            TdMessageContent::MessageLocation(_) => Self::Unsupported("messageLocation"),
            TdMessageContent::MessageVenue(_) => Self::Unsupported("messageVenue"),
            TdMessageContent::MessageContact(_) => Self::Unsupported("messageContact"),
            TdMessageContent::MessageAnimatedEmoji(_) => Self::Unsupported("messageAnimatedEmoji"),
            TdMessageContent::MessageDice(_) => Self::Unsupported("messageDice"),
            TdMessageContent::MessageGame(_) => Self::Unsupported("messageGame"),
            TdMessageContent::MessagePoll(_) => Self::Unsupported("messagePoll"),
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
}

/// A single message — tuigram's projection of TDLib's `Message`.
#[derive(Clone, Debug, PartialEq, Eq)]
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
}

impl Message {
    /// Project TDLib's `Message`.
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
        }
    }

    /// The message's text, if it is a text message — a convenience for the
    /// headless harness and tests.
    #[must_use]
    pub fn text(&self) -> Option<&str> {
        match &self.content {
            MessageContent::Text(t) => Some(&t.text),
            MessageContent::Unsupported(_) => None,
        }
    }
}

/// A chat — tuigram's projection of TDLib's `Chat`, carrying what the chat list
/// and a conversation header need.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chat {
    /// Chat id.
    pub id: i64,
    /// Display title.
    pub title: String,
    /// Chat classification.
    pub kind: ChatKind,
    /// The most recent message, if known.
    pub last_message: Option<Message>,
    /// Number of unread incoming messages.
    pub unread_count: i32,
    /// Number of unread messages mentioning the user.
    pub unread_mention_count: i32,
    /// Id of the last message the user has read in this chat (inbox).
    pub last_read_inbox_message_id: i64,
    /// Id of the last message of the user that the peer has read (outbox).
    pub last_read_outbox_message_id: i64,
    /// The chat's positions across the lists it appears in.
    pub positions: Vec<ChatPosition>,
}

impl Chat {
    /// Project TDLib's `Chat`.
    #[must_use]
    pub fn from_tdlib(chat: &TdChat) -> Self {
        Self {
            id: chat.id,
            title: chat.title.clone(),
            kind: ChatKind::from_tdlib(&chat.r#type),
            last_message: chat.last_message.as_ref().map(Message::from_tdlib),
            unread_count: chat.unread_count,
            unread_mention_count: chat.unread_mention_count,
            last_read_inbox_message_id: chat.last_read_inbox_message_id,
            last_read_outbox_message_id: chat.last_read_outbox_message_id,
            positions: chat
                .positions
                .iter()
                .map(ChatPosition::from_tdlib)
                .collect(),
        }
    }

    /// This chat's ordering key in the Main list, if it has a position there.
    /// The chat list module (#17) sorts the Main view by this.
    #[must_use]
    pub fn main_order(&self) -> Option<i64> {
        self.positions
            .iter()
            .find(|p| p.list == ChatListKind::Main)
            .map(|p| p.order)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tdlib_rs::enums::ChatAvailableReactions;
    use tdlib_rs::types::{
        ChatPosition as TdChatPositionT, ChatTypePrivate, ChatTypeSupergroup, Error as TdError,
        FormattedText as TdFormattedTextT, MessageContact, MessageSenderChat, MessageSenderUser,
        MessageSendingStateFailed, TextEntity as TdTextEntityT, TextEntityTypeTextUrl,
    };

    /// A TDLib `Message` with every field zeroed but the ones a test cares
    /// about. Only `sender_id` and `content` are non-defaultable, so they (and a
    /// few useful fields) are parameters; the rest are inert.
    fn td_message(
        id: i64,
        chat_id: i64,
        sender_id: TdMessageSender,
        content: TdMessageContent,
        sending_state: Option<TdMessageSendingState>,
        is_outgoing: bool,
    ) -> TdMessage {
        TdMessage {
            id,
            sender_id,
            chat_id,
            sending_state,
            scheduling_state: None,
            is_outgoing,
            is_pinned: false,
            is_from_offline: false,
            can_be_saved: false,
            has_timestamped_media: false,
            is_channel_post: false,
            is_paid_star_suggested_post: false,
            is_paid_ton_suggested_post: false,
            contains_unread_mention: false,
            date: 1_700_000_000,
            edit_date: 0,
            forward_info: None,
            import_info: None,
            interaction_info: None,
            unread_reactions: vec![],
            fact_check: None,
            suggested_post_info: None,
            reply_to: None,
            topic_id: None,
            self_destruct_type: None,
            self_destruct_in: 0.0,
            auto_delete_in: 0.0,
            via_bot_user_id: 0,
            sender_business_bot_user_id: 0,
            sender_boost_count: 0,
            paid_message_star_count: 0,
            author_signature: String::new(),
            media_album_id: 0,
            effect_id: 0,
            restriction_info: None,
            summary_language_code: String::new(),
            content,
            reply_markup: None,
        }
    }

    /// A TDLib `Chat` with every field zeroed but the ones a test cares about.
    fn td_chat(
        id: i64,
        title: &str,
        kind: TdChatType,
        positions: Vec<TdChatPosition>,
        unread_count: i32,
        last_message: Option<TdMessage>,
    ) -> TdChat {
        TdChat {
            id,
            r#type: kind,
            title: title.to_owned(),
            photo: None,
            accent_color_id: 0,
            background_custom_emoji_id: 0,
            upgraded_gift_colors: None,
            profile_accent_color_id: 0,
            profile_background_custom_emoji_id: 0,
            permissions: tdlib_rs::types::ChatPermissions::default(),
            last_message,
            positions,
            chat_lists: vec![],
            message_sender_id: None,
            block_list: None,
            has_protected_content: false,
            is_translatable: false,
            is_marked_as_unread: false,
            view_as_topics: false,
            has_scheduled_messages: false,
            can_be_deleted_only_for_self: false,
            can_be_deleted_for_all_users: false,
            can_be_reported: false,
            default_disable_notification: false,
            unread_count,
            last_read_inbox_message_id: 0,
            last_read_outbox_message_id: 0,
            unread_mention_count: 0,
            unread_reaction_count: 0,
            notification_settings: tdlib_rs::types::ChatNotificationSettings::default(),
            available_reactions: ChatAvailableReactions::All(Default::default()),
            message_auto_delete_time: 0,
            emoji_status: None,
            background: None,
            theme: None,
            action_bar: None,
            business_bot_manage_bar: None,
            video_chat: tdlib_rs::types::VideoChat::default(),
            pending_join_requests: None,
            reply_markup_message_id: 0,
            draft_message: None,
            client_data: String::new(),
        }
    }

    fn td_text(body: &str, entities: Vec<TdTextEntityT>) -> TdMessageContent {
        TdMessageContent::MessageText(tdlib_rs::types::MessageText {
            text: TdFormattedTextT {
                text: body.to_owned(),
                entities,
            },
            link_preview: None,
            link_preview_options: None,
        })
    }

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
        // A payload-bearing media variant.
        let contact = TdMessageContent::MessageContact(MessageContact {
            contact: tdlib_rs::types::Contact::default(),
        });
        assert_eq!(
            MessageContent::from_tdlib(&contact),
            MessageContent::Unsupported("messageContact")
        );
    }

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

    #[test]
    fn sender_projects_user_and_chat() {
        assert_eq!(
            Sender::from_tdlib(&TdMessageSender::User(MessageSenderUser { user_id: 7 })),
            Sender::User(7)
        );
        assert_eq!(
            Sender::from_tdlib(&TdMessageSender::Chat(MessageSenderChat { chat_id: -100 })),
            Sender::Chat(-100)
        );
    }

    #[test]
    fn supergroup_channel_flag_splits_kind() {
        let group = TdChatType::Supergroup(ChatTypeSupergroup {
            supergroup_id: 1,
            is_channel: false,
        });
        let channel = TdChatType::Supergroup(ChatTypeSupergroup {
            supergroup_id: 2,
            is_channel: true,
        });
        assert_eq!(
            ChatKind::from_tdlib(&group),
            ChatKind::Supergroup { supergroup_id: 1 }
        );
        assert_eq!(
            ChatKind::from_tdlib(&channel),
            ChatKind::Channel { supergroup_id: 2 }
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
    fn chat_projects_fields_last_message_and_main_order() {
        let positions = vec![
            TdChatPositionT {
                list: TdChatList::Archive,
                order: 5,
                is_pinned: false,
                source: None,
            },
            TdChatPositionT {
                list: TdChatList::Main,
                order: 99,
                is_pinned: true,
                source: None,
            },
        ];
        let last = td_message(
            1,
            10,
            TdMessageSender::User(MessageSenderUser { user_id: 7 }),
            td_text("last", vec![]),
            None,
            false,
        );
        let td = td_chat(
            10,
            "Friends",
            TdChatType::Private(ChatTypePrivate { user_id: 7 }),
            positions,
            3,
            Some(last),
        );
        let chat = Chat::from_tdlib(&td);
        assert_eq!(chat.id, 10);
        assert_eq!(chat.title, "Friends");
        assert_eq!(chat.kind, ChatKind::Private { user_id: 7 });
        assert_eq!(chat.unread_count, 3);
        assert_eq!(chat.main_order(), Some(99));
        assert_eq!(
            chat.last_message.and_then(|m| m.text().map(str::to_owned)),
            Some("last".to_owned())
        );
    }

    /// A TDLib `User` with every field zeroed but the ones a test cares about.
    fn td_user(
        id: i64,
        first: &str,
        last: &str,
        usernames: Vec<&str>,
        phone: &str,
        kind: TdUserType,
        status: TdUserStatus,
    ) -> TdUser {
        TdUser {
            id,
            first_name: first.to_owned(),
            last_name: last.to_owned(),
            usernames: (!usernames.is_empty()).then(|| tdlib_rs::types::Usernames {
                active_usernames: usernames.into_iter().map(str::to_owned).collect(),
                ..Default::default()
            }),
            phone_number: phone.to_owned(),
            status,
            profile_photo: None,
            accent_color_id: 0,
            background_custom_emoji_id: 0,
            upgraded_gift_colors: None,
            profile_accent_color_id: 0,
            profile_background_custom_emoji_id: 0,
            emoji_status: None,
            is_contact: false,
            is_mutual_contact: false,
            is_close_friend: false,
            verification_status: None,
            is_premium: false,
            is_support: false,
            restriction_info: None,
            active_story_state: None,
            restricts_new_chats: false,
            paid_message_star_count: 0,
            have_access: true,
            r#type: kind,
            language_code: String::new(),
            added_to_attachment_menu: false,
        }
    }

    #[test]
    fn user_status_projects_every_bucket() {
        use tdlib_rs::types::{
            UserStatusLastMonth, UserStatusLastWeek, UserStatusOffline, UserStatusOnline,
            UserStatusRecently,
        };
        assert_eq!(Presence::from_tdlib(&TdUserStatus::Empty), Presence::Never);
        assert_eq!(
            Presence::from_tdlib(&TdUserStatus::Online(UserStatusOnline { expires: 99 })),
            Presence::Online { expires: 99 }
        );
        assert_eq!(
            Presence::from_tdlib(&TdUserStatus::Offline(UserStatusOffline { was_online: 42 })),
            Presence::Offline { was_online: 42 }
        );
        assert_eq!(
            Presence::from_tdlib(&TdUserStatus::Recently(UserStatusRecently::default())),
            Presence::Recently
        );
        assert_eq!(
            Presence::from_tdlib(&TdUserStatus::LastWeek(UserStatusLastWeek::default())),
            Presence::LastWeek
        );
        assert_eq!(
            Presence::from_tdlib(&TdUserStatus::LastMonth(UserStatusLastMonth::default())),
            Presence::LastMonth
        );
    }

    #[test]
    fn user_kind_projects_every_variant() {
        assert_eq!(
            UserKind::from_tdlib(&TdUserType::Regular),
            UserKind::Regular
        );
        assert_eq!(
            UserKind::from_tdlib(&TdUserType::Deleted),
            UserKind::Deleted
        );
        assert_eq!(
            UserKind::from_tdlib(&TdUserType::Bot(tdlib_rs::types::UserTypeBot::default())),
            UserKind::Bot
        );
        assert_eq!(
            UserKind::from_tdlib(&TdUserType::Unknown),
            UserKind::Unknown
        );
    }

    #[test]
    fn user_projects_fields_with_optional_username_and_phone() {
        let user = User::from_tdlib(&td_user(
            7,
            "Ada",
            "Lovelace",
            vec!["ada", "countess"],
            "+15551234",
            TdUserType::Regular,
            TdUserStatus::Online(tdlib_rs::types::UserStatusOnline { expires: 5 }),
        ));
        assert_eq!(user.id, 7);
        assert_eq!(user.username(), Some("ada"));
        assert_eq!(user.usernames, vec!["ada", "countess"]);
        assert_eq!(user.phone_number.as_deref(), Some("+15551234"));
        assert_eq!(user.kind, UserKind::Regular);
        assert_eq!(user.status, Presence::Online { expires: 5 });

        // No usernames and an empty phone collapse to None/empty, not "".
        let bare = User::from_tdlib(&td_user(
            8,
            "Grace",
            "",
            vec![],
            "",
            TdUserType::Regular,
            TdUserStatus::Empty,
        ));
        assert_eq!(bare.username(), None);
        assert!(bare.usernames.is_empty());
        assert_eq!(bare.phone_number, None);
    }

    #[test]
    fn display_name_falls_back_name_then_username_then_deleted_then_id() {
        let named = User::from_tdlib(&td_user(
            7,
            "Ada",
            "Lovelace",
            vec!["ada"],
            "",
            TdUserType::Regular,
            TdUserStatus::Empty,
        ));
        assert_eq!(named.display_name(), "Ada Lovelace");

        // No name → primary username.
        let handle = User::from_tdlib(&td_user(
            8,
            "",
            "",
            vec!["grace"],
            "",
            TdUserType::Regular,
            TdUserStatus::Empty,
        ));
        assert_eq!(handle.display_name(), "@grace");

        // No name, no username, deleted → the conventional label.
        let gone = User::from_tdlib(&td_user(
            9,
            "",
            "",
            vec![],
            "",
            TdUserType::Deleted,
            TdUserStatus::Empty,
        ));
        assert_eq!(gone.display_name(), "Deleted Account");

        // No name, no username, still a regular account → the bare id.
        let anon = User::from_tdlib(&td_user(
            10,
            "",
            "",
            vec![],
            "",
            TdUserType::Regular,
            TdUserStatus::Empty,
        ));
        assert_eq!(anon.display_name(), "User 10");
    }
}
