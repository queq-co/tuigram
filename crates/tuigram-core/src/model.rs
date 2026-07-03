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
//! The model covers **text** in full (with its formatting entities), the common
//! file-backed **media** types ([`Photo`], [`Video`], [`Document`], [`Audio`],
//! [`Voice`], [`Sticker`], [`Animation`]), and the **structured** types
//! ([`Location`], [`Venue`], [`Contact`], [`Poll`]); rarer content is
//! `Unsupported`, for follow-up issues. Message **reactions** are modeled (#51,
//! see [`Reaction`]); forwards, replies, and service messages are out of scope
//! for this model.

use tdlib_rs::enums::{
    ChatAction as TdChatAction, ChatList as TdChatList, ChatType as TdChatType,
    InputFile as TdInputFile, InputMessageContent as TdInputMessageContent,
    InputMessageReplyTo as TdInputMessageReplyTo, MessageContent as TdMessageContent,
    MessageSender as TdMessageSender, MessageSendingState as TdMessageSendingState,
    PollType as TdPollType, ReactionType as TdReactionType, SecretChatState as TdSecretChatState,
    TextEntityType as TdTextEntityType, UserStatus as TdUserStatus, UserType as TdUserType,
};
use tdlib_rs::types::{
    Chat as TdChat, ChatFolderInfo as TdChatFolderInfo, ChatListFolder,
    ChatPosition as TdChatPosition, Contact as TdContact, DraftMessage as TdDraftMessage,
    File as TdFile, FormattedText as TdFormattedText, InputFileLocal, InputMessageAnimation,
    InputMessageAudio, InputMessageDocument, InputMessagePhoto, InputMessageReplyToMessage,
    InputMessageText, InputMessageVideo, InputMessageVoiceNote, Location as TdLocation,
    Message as TdMessage, MessageAnimation as TdMessageAnimation, MessageAudio as TdMessageAudio,
    MessageDocument as TdMessageDocument, MessageInteractionInfo as TdMessageInteractionInfo,
    MessagePhoto as TdMessagePhoto, MessageReaction as TdMessageReaction,
    MessageSenderChat as TdMessageSenderChat, MessageSenderUser as TdMessageSenderUser,
    MessageSticker as TdMessageSticker, MessageVideo as TdMessageVideo,
    MessageVoiceNote as TdMessageVoiceNote, Poll as TdPoll, PollOption as TdPollOption,
    ReactionTypeCustomEmoji, ReactionTypeEmoji, SecretChat as TdSecretChat,
    TextEntity as TdTextEntity, User as TdUser, Venue as TdVenue,
};

/// Who sent a message.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
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

    /// Lower back to TDLib's `MessageSender`, for requests that filter by sender
    /// (e.g. searching a chat for one person's messages). The inverse of
    /// [`from_tdlib`](Self::from_tdlib).
    #[must_use]
    pub fn to_tdlib(&self) -> TdMessageSender {
        match self {
            Self::User(id) => TdMessageSender::User(TdMessageSenderUser { user_id: *id }),
            Self::Chat(id) => TdMessageSender::Chat(TdMessageSenderChat { chat_id: *id }),
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

/// A transient activity a sender is performing in a chat — tuigram's projection
/// of TDLib's `ChatAction`, the "X is typing…" / "X is sending a photo…" status.
///
/// Total over the enum with no catch-all, the same discipline as [`Presence`]: a
/// new `ChatAction` variant fails to compile here until it is classified. Two
/// deliberate projections: the upload-progress percentage and the watched emoji
/// are dropped — the view needs to know *what* a sender is doing, not how far
/// along — and `chatActionCancel` maps to `None` in
/// [`from_tdlib`](Self::from_tdlib) rather than to a variant, because a cancel is
/// the *absence* of an activity (it clears the sender from the typing view), not
/// an activity of its own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChatAction {
    /// Typing a text message.
    Typing,
    /// Recording a video.
    RecordingVideo,
    /// Uploading a video.
    UploadingVideo,
    /// Recording a voice note.
    RecordingVoiceNote,
    /// Uploading a voice note.
    UploadingVoiceNote,
    /// Uploading a photo.
    UploadingPhoto,
    /// Uploading a document.
    UploadingDocument,
    /// Picking a sticker to send.
    ChoosingSticker,
    /// Picking a location or venue to send.
    ChoosingLocation,
    /// Picking a contact to send.
    ChoosingContact,
    /// Started to play a game.
    StartPlayingGame,
    /// Recording a round video note.
    RecordingVideoNote,
    /// Uploading a round video note.
    UploadingVideoNote,
    /// Watching animations sent by the other party (an animated emoji tap).
    WatchingAnimations,
}

impl ChatAction {
    /// Project TDLib's `ChatAction`. Returns `None` for `chatActionCancel`, which
    /// the [chat-action store](crate::actions::ChatActionStore) folds as "this
    /// sender stopped" rather than as an activity.
    #[must_use]
    pub fn from_tdlib(action: &TdChatAction) -> Option<Self> {
        match action {
            TdChatAction::Typing => Some(Self::Typing),
            TdChatAction::RecordingVideo => Some(Self::RecordingVideo),
            TdChatAction::UploadingVideo(_) => Some(Self::UploadingVideo),
            TdChatAction::RecordingVoiceNote => Some(Self::RecordingVoiceNote),
            TdChatAction::UploadingVoiceNote(_) => Some(Self::UploadingVoiceNote),
            TdChatAction::UploadingPhoto(_) => Some(Self::UploadingPhoto),
            TdChatAction::UploadingDocument(_) => Some(Self::UploadingDocument),
            TdChatAction::ChoosingSticker => Some(Self::ChoosingSticker),
            TdChatAction::ChoosingLocation => Some(Self::ChoosingLocation),
            TdChatAction::ChoosingContact => Some(Self::ChoosingContact),
            TdChatAction::StartPlayingGame => Some(Self::StartPlayingGame),
            TdChatAction::RecordingVideoNote => Some(Self::RecordingVideoNote),
            TdChatAction::UploadingVideoNote(_) => Some(Self::UploadingVideoNote),
            TdChatAction::WatchingAnimations(_) => Some(Self::WatchingAnimations),
            TdChatAction::Cancel => None,
        }
    }

    /// Lower back to TDLib's `ChatAction`, for broadcasting our own activity over
    /// [`ChatActionRequests::send_chat_action`](crate::actions::ChatActionRequests::send_chat_action).
    /// The dropped upload progress is sent as `0` and the watched emoji as empty —
    /// the model carries neither — which is harmless for an advisory status. The
    /// inverse of [`from_tdlib`](Self::from_tdlib) over the activity variants;
    /// cancel is expressed by sending `None`, never by this method.
    #[must_use]
    pub fn to_tdlib(&self) -> TdChatAction {
        use tdlib_rs::types::{
            ChatActionUploadingDocument, ChatActionUploadingPhoto, ChatActionUploadingVideo,
            ChatActionUploadingVideoNote, ChatActionUploadingVoiceNote,
            ChatActionWatchingAnimations,
        };
        match self {
            Self::Typing => TdChatAction::Typing,
            Self::RecordingVideo => TdChatAction::RecordingVideo,
            Self::UploadingVideo => {
                TdChatAction::UploadingVideo(ChatActionUploadingVideo { progress: 0 })
            }
            Self::RecordingVoiceNote => TdChatAction::RecordingVoiceNote,
            Self::UploadingVoiceNote => {
                TdChatAction::UploadingVoiceNote(ChatActionUploadingVoiceNote { progress: 0 })
            }
            Self::UploadingPhoto => {
                TdChatAction::UploadingPhoto(ChatActionUploadingPhoto { progress: 0 })
            }
            Self::UploadingDocument => {
                TdChatAction::UploadingDocument(ChatActionUploadingDocument { progress: 0 })
            }
            Self::ChoosingSticker => TdChatAction::ChoosingSticker,
            Self::ChoosingLocation => TdChatAction::ChoosingLocation,
            Self::ChoosingContact => TdChatAction::ChoosingContact,
            Self::StartPlayingGame => TdChatAction::StartPlayingGame,
            Self::RecordingVideoNote => TdChatAction::RecordingVideoNote,
            Self::UploadingVideoNote => {
                TdChatAction::UploadingVideoNote(ChatActionUploadingVideoNote { progress: 0 })
            }
            Self::WatchingAnimations => {
                TdChatAction::WatchingAnimations(ChatActionWatchingAnimations {
                    emoji: String::new(),
                })
            }
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
            first_name: crate::sanitize::scrub_line(&user.first_name),
            last_name: crate::sanitize::scrub_line(&user.last_name),
            usernames: user
                .usernames
                .as_ref()
                .map(|u| {
                    u.active_usernames
                        .iter()
                        .map(|name| crate::sanitize::scrub_line(name))
                        .collect()
                })
                .unwrap_or_default(),
            phone_number: Some(crate::sanitize::scrub_line(&user.phone_number))
                .filter(|p| !p.is_empty()),
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

/// The lifecycle state of a [`SecretChat`] — tuigram's projection of TDLib's
/// `SecretChatState`. Total over the enum, no catch-all, the same discipline as
/// [`Presence`]: a new state fails to compile here until it is classified.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecretChatState {
    /// Not yet established — waiting for the partner to come online and complete
    /// the key exchange.
    Pending,
    /// Established and usable for end-to-end encrypted messaging.
    Ready,
    /// Closed by either party; no longer usable.
    Closed,
}

impl SecretChatState {
    /// Project TDLib's `SecretChatState`.
    #[must_use]
    pub fn from_tdlib(state: &TdSecretChatState) -> Self {
        match state {
            TdSecretChatState::Pending => Self::Pending,
            TdSecretChatState::Ready => Self::Ready,
            TdSecretChatState::Closed => Self::Closed,
        }
    }
}

/// An end-to-end encrypted secret chat — tuigram's projection of TDLib's
/// `SecretChat`, the encryption state behind a
/// [`ChatKind::Secret`](ChatKind::Secret) chat in the snapshot.
///
/// A secret chat has its own id space (`i32`, distinct from a chat's `i64`); a
/// `ChatKind::Secret` carries the `secret_chat_id` that keys back to this record.
/// The protocol `layer` is dropped — the model tracks the chat's *lifecycle and
/// identity*, not the partner app's feature level — keeping the same minimal
/// projection discipline as the rest of the model. The `key_hash` is retained raw
/// for a caller to render the key-verification image or hex fingerprint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecretChat {
    /// Secret chat identifier (the key in [`ChatKind::Secret`]).
    pub id: i32,
    /// The chat partner's user id.
    pub user_id: i64,
    /// Where the chat is in its lifecycle.
    pub state: SecretChatState,
    /// Whether the current user created the chat (`true`) or accepted it (`false`).
    pub is_outbound: bool,
    /// Raw key hash, for rendering the key-verification fingerprint. Empty until
    /// the chat is [`Ready`](SecretChatState::Ready).
    pub key_hash: String,
}

impl SecretChat {
    /// Project TDLib's `SecretChat`.
    #[must_use]
    pub fn from_tdlib(chat: &TdSecretChat) -> Self {
        Self {
            id: chat.id,
            user_id: chat.user_id,
            state: SecretChatState::from_tdlib(&chat.state),
            is_outbound: chat.is_outbound,
            key_hash: chat.key_hash.clone(),
        }
    }

    /// Whether the chat is established and usable for messaging.
    ///
    /// Both text and media sends only succeed once the key exchange has
    /// completed — TDLib rejects a send to a [`Pending`](SecretChatState::Pending)
    /// or [`Closed`](SecretChatState::Closed) chat. A driver gates the compose path
    /// on this so it never posts into a chat the server will refuse; the message
    /// itself then flows through the ordinary
    /// [`MessageStore`](crate::messages::MessageStore) keyed by the chat's id, no
    /// secret-chat-specific routing required for either.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.state == SecretChatState::Ready
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

    /// Build TDLib's `ChatList`, for the request side (e.g. selecting which list
    /// to page with `loadChats`). Total, mirroring [`from_tdlib`](Self::from_tdlib):
    /// a new variant added here must be handled rather than defaulting.
    #[must_use]
    pub fn to_tdlib(&self) -> TdChatList {
        match self {
            Self::Main => TdChatList::Main,
            Self::Archive => TdChatList::Archive,
            Self::Folder(id) => TdChatList::Folder(ChatListFolder {
                chat_folder_id: *id,
            }),
        }
    }
}

/// Metadata for one user-defined chat folder, as listed by `updateChatFolders`
/// (#49). The folder's chats are not carried here — they arrive as per-list
/// `updateChatPosition`s for [`ChatListKind::Folder`] and read back ordered via
/// [`ChatStore::folder_list`](crate::ChatStore::folder_list); this is only the
/// folder's identity, for presenting the set of folders.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatFolderInfo {
    /// Unique folder id — the `id` carried by [`ChatListKind::Folder`].
    pub id: i32,
    /// The folder's display title: its name's plain text. A folder name may
    /// carry only custom-emoji entities, which tuigram drops — the bare text is
    /// the title shown.
    pub title: String,
}

impl ChatFolderInfo {
    /// Project TDLib's `chatFolderInfo` down to the id and display title tuigram
    /// lists. A partial projection by design — the icon, color, and share state
    /// are not modelled (follow-up issues); the title is the name's plain text.
    #[must_use]
    pub fn from_tdlib(info: &TdChatFolderInfo) -> Self {
        Self {
            id: info.id,
            title: info.name.text.text.clone(),
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
        // Trust boundary: message bodies and captions are attacker-controlled and
        // end up in terminal cells, so neutralize control sequences here — once,
        // where every text/caption/poll projection funnels through. Replacing
        // controls one-for-one keeps the entities' UTF-16 offsets aligned.
        Self {
            text: crate::sanitize::scrub_prose(&text.text),
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

/// A reference to a TDLib file, as held by media message content.
///
/// Media (a photo, video, document, …) carries only this id; the bytes and the
/// download/upload state live in the [`FileStore`](crate::files::FileStore),
/// which the single update router keeps current from `updateFile`. This is the
/// same indirection [`Sender::User`] uses for people: content stays a cheap,
/// `Copy` reference and the mutable file state is resolved out of one store —
/// `store.get(file_ref)` — rather than duplicated into every message snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FileRef {
    /// TDLib's per-session file id (the key into the [`FileStore`]).
    pub id: i32,
}

impl FileRef {
    /// Wrap a TDLib file id.
    #[must_use]
    pub fn new(id: i32) -> Self {
        Self { id }
    }
}

/// A file tuigram knows about — its size and its local/remote transfer state,
/// flattened from TDLib's `File`/`LocalFile`/`RemoteFile` trio into the subset a
/// caller needs to show a thumbnail, a download/upload bar, or open the bytes.
///
/// The projection is **total** (it reads every nested field it surfaces), and
/// folding the same `updateFile` twice converges, so the [`FileStore`] can
/// re-apply TDLib's repeated emissions idempotently.
///
/// [`FileStore`]: crate::files::FileStore
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct File {
    /// TDLib's per-session file id.
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
    /// Project TDLib's `File`, flattening its local and remote sub-records.
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

    /// The best known total size in bytes: the exact `size` when TDLib has it,
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
/// TDLib sends a photo as several pre-scaled [`sizes`](TdMessagePhoto); a
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
    /// Project TDLib's `messagePhoto`, keeping its largest size.
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
}

impl Video {
    /// Project TDLib's `messageVideo`.
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
    /// Project TDLib's `messageDocument`.
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
    /// Project TDLib's `messageAudio`.
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
    /// Project TDLib's `messageVoiceNote`.
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
/// caption in TDLib, so there is none here.
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
}

impl Sticker {
    /// Project TDLib's `messageSticker`.
    #[must_use]
    pub fn from_tdlib(m: &TdMessageSticker) -> Self {
        Self {
            file: FileRef::new(m.sticker.sticker.id),
            width: m.sticker.width,
            height: m.sticker.height,
            emoji: crate::sanitize::scrub_line(&m.sticker.emoji),
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
}

impl Animation {
    /// Project TDLib's `messageAnimation`.
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
        }
    }
}

/// A geographic point — tuigram's projection of TDLib's `Location`. Reused by
/// both a [`MessageContent::Location`] message and a [`Venue`].
///
/// This is the static point only; TDLib's live-location fields (update period,
/// heading, proximity radius) live on the message wrapper and are dropped — a
/// live location and a static one project alike here.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Location {
    /// Latitude, in degrees.
    pub latitude: f64,
    /// Longitude, in degrees.
    pub longitude: f64,
    /// Estimated horizontal accuracy, in meters; `0.0` if the sender gave none.
    pub horizontal_accuracy: f64,
}

impl Location {
    /// Project TDLib's `location`.
    #[must_use]
    pub fn from_tdlib(l: &TdLocation) -> Self {
        Self {
            latitude: l.latitude,
            longitude: l.longitude,
            horizontal_accuracy: l.horizontal_accuracy,
        }
    }
}

/// A venue — a named place at a [`Location`] — projecting TDLib's `Venue`. The
/// provider-database fields (`provider`, `id`, `type`) are dropped; a client
/// shows the title, address, and point.
#[derive(Clone, Debug, PartialEq)]
pub struct Venue {
    /// Where the venue is.
    pub location: Location,
    /// Venue name, as given by the sender (may be empty).
    pub title: String,
    /// Venue address, as given by the sender (may be empty).
    pub address: String,
}

impl Venue {
    /// Project TDLib's `venue`.
    #[must_use]
    pub fn from_tdlib(v: &TdVenue) -> Self {
        Self {
            location: Location::from_tdlib(&v.location),
            title: crate::sanitize::scrub_line(&v.title),
            address: crate::sanitize::scrub_line(&v.address),
        }
    }
}

/// A shared contact card — tuigram's projection of TDLib's `Contact`. The vCard
/// blob is dropped; the model keeps the name, phone, and the Telegram user id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Contact {
    /// First name (1–64 characters).
    pub first_name: String,
    /// Last name (may be empty).
    pub last_name: String,
    /// Phone number.
    pub phone_number: String,
    /// The contact's Telegram user id, or `0` if it is not a known user.
    pub user_id: i64,
}

impl Contact {
    /// Project TDLib's `contact`.
    #[must_use]
    pub fn from_tdlib(c: &TdContact) -> Self {
        Self {
            first_name: crate::sanitize::scrub_line(&c.first_name),
            last_name: crate::sanitize::scrub_line(&c.last_name),
            phone_number: crate::sanitize::scrub_line(&c.phone_number),
            user_id: c.user_id,
        }
    }
}

/// One answer option in a [`Poll`] — tuigram's projection of TDLib's
/// `pollOption`. Vote counts are meaningful only once the poll is voted in or
/// closed; the transient "being chosen by a pending request" flag is dropped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PollOption {
    /// The option text.
    pub text: FormattedText,
    /// Number of voters who chose this option.
    pub voter_count: i32,
    /// Share of the vote for this option, 0–100.
    pub vote_percentage: i32,
    /// Whether this account chose this option.
    pub is_chosen: bool,
}

impl PollOption {
    /// Project TDLib's `pollOption`.
    #[must_use]
    pub fn from_tdlib(o: &TdPollOption) -> Self {
        Self {
            text: FormattedText::from_tdlib(&o.text),
            voter_count: o.voter_count,
            vote_percentage: o.vote_percentage,
            is_chosen: o.is_chosen,
        }
    }
}

/// What kind of poll a [`Poll`] is — tuigram's projection of TDLib's `PollType`.
/// Total over the enum, no catch-all: a new poll type fails to compile here
/// until it is classified.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PollKind {
    /// A regular poll.
    Regular {
        /// Whether more than one option may be chosen at once.
        allow_multiple_answers: bool,
    },
    /// A quiz: exactly one correct option, answerable once.
    Quiz {
        /// 0-based index of the correct option; `-1` until answered.
        correct_option_id: i32,
        /// Text shown after an incorrect answer (may be empty).
        explanation: FormattedText,
    },
}

impl PollKind {
    /// Project TDLib's `PollType`.
    #[must_use]
    pub fn from_tdlib(kind: &TdPollType) -> Self {
        match kind {
            TdPollType::Regular(r) => Self::Regular {
                allow_multiple_answers: r.allow_multiple_answers,
            },
            TdPollType::Quiz(q) => Self::Quiz {
                correct_option_id: q.correct_option_id,
                explanation: FormattedText::from_tdlib(&q.explanation),
            },
        }
    }
}

/// A poll or quiz — tuigram's projection of TDLib's `Poll`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Poll {
    /// The poll question.
    pub question: FormattedText,
    /// The answer options, in the order TDLib lists them.
    pub options: Vec<PollOption>,
    /// Total number of voters across all options.
    pub total_voter_count: i32,
    /// Whether votes are anonymous.
    pub is_anonymous: bool,
    /// Whether the poll is closed (no longer accepting votes).
    pub is_closed: bool,
    /// Whether this is a regular poll or a quiz.
    pub kind: PollKind,
}

impl Poll {
    /// Project TDLib's `poll`.
    #[must_use]
    pub fn from_tdlib(p: &TdPoll) -> Self {
        Self {
            question: FormattedText::from_tdlib(&p.question),
            options: p.options.iter().map(PollOption::from_tdlib).collect(),
            total_voter_count: p.total_voter_count,
            is_anonymous: p.is_anonymous,
            is_closed: p.is_closed,
            kind: PollKind::from_tdlib(&p.r#type),
        }
    }
}

/// The content of a message. tuigram models text, the common file-backed media
/// types ([`Photo`], [`Video`], [`Document`], [`Audio`], [`Voice`], [`Sticker`],
/// [`Animation`]), and the structured types ([`Location`], [`Venue`],
/// [`Contact`], [`Poll`]); everything else is [`MessageContent::Unsupported`]
/// carrying TDLib's content type name.
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
    /// A content type tuigram does not model yet. Carries TDLib's type name
    /// (e.g. `"messageVideoNote"`) so callers can report it precisely.
    Unsupported(&'static str),
}

impl MessageContent {
    /// Project TDLib's `MessageContent`. Total over the enum: a new TDLib
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
/// [`FormattedText`] when there is none). The remaining TDLib metadata a
/// `inputMessage*` accepts — dimensions, duration, thumbnails — is left for TDLib
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
    /// Project into the matching TDLib `inputMessage*` content, wrapping the local
    /// path as an [`InputFile::Local`](TdInputFile::Local) and carrying the caption
    /// (omitted when empty). All other metadata is defaulted so TDLib measures the
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

/// Wrap a local path as a TDLib [`InputFile::Local`](TdInputFile::Local).
fn local_file(path: &str) -> TdInputFile {
    TdInputFile::Local(InputFileLocal {
        path: path.to_owned(),
    })
}

/// Project a caption, omitting it when empty: TDLib reads a `None` caption as no
/// caption, so an empty [`FormattedText`] must not be sent as an empty body.
fn optional_caption(caption: &FormattedText) -> Option<TdFormattedText> {
    (!caption.text.is_empty()).then(|| caption.to_tdlib())
}

/// A reaction's identity — tuigram's projection of TDLib's `ReactionType`.
///
/// Total over the TDLib enum: a standard [`Emoji`](Self::Emoji), a
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
    /// Project TDLib's `ReactionType`.
    #[must_use]
    pub fn from_tdlib(kind: &TdReactionType) -> Self {
        match kind {
            TdReactionType::Emoji(e) => Self::Emoji(crate::sanitize::scrub_line(&e.emoji)),
            TdReactionType::CustomEmoji(c) => Self::CustomEmoji(c.custom_emoji_id),
            TdReactionType::Paid => Self::Paid,
        }
    }

    /// Lower back to TDLib's `ReactionType`, for adding or removing a reaction
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

/// One reaction bucket on a message — tuigram's projection of TDLib's
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
    /// Project TDLib's `MessageReaction`. The recent-sender list and paid-reactor
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
/// buckets in `interaction_info.reactions`, in TDLib's order, or empty when
/// either the interaction info or its reaction list is absent. Shared by
/// [`Message::from_tdlib`] and the `updateMessageInteractionInfo` fold in
/// [`MessageStore`](crate::messages::MessageStore).
pub(crate) fn reactions_from(info: Option<&TdMessageInteractionInfo>) -> Vec<Reaction> {
    info.and_then(|i| i.reactions.as_ref())
        .map(|r| r.reactions.iter().map(Reaction::from_tdlib).collect())
        .unwrap_or_default()
}

/// A single message — tuigram's projection of TDLib's `Message`.
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
    /// Reactions added to the message, one bucket per reaction, in TDLib's
    /// order. Empty when the message has no reactions.
    pub reactions: Vec<Reaction>,
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
            reactions: reactions_from(message.interaction_info.as_ref()),
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

/// A chat's unsent compose draft — tuigram's projection of TDLib's
/// `DraftMessage`. Telegram syncs this half-typed message across the account's
/// devices, so it is **chat state, not history**: it lives on the [`Chat`]
/// snapshot and never enters the message store.
///
/// Phase 3 models a **text** draft — the realistic case for a keyboard-driven
/// client. TDLib also allows voice/video-note drafts, which carry no text and
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
    /// Project TDLib's `DraftMessage`. A non-text draft (voice/video note, which
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

    /// Lower back to TDLib's `DraftMessage`, for pushing a draft over the seam.
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

/// A chat — tuigram's projection of TDLib's `Chat`, carrying what the chat list
/// and a conversation header need. Not `Eq`: its [`last_message`](Self::last_message)
/// may carry `f64` location coordinates (see [`Message`]).
#[derive(Clone, Debug, PartialEq)]
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
    /// The unsent compose draft synced for this chat, if any.
    pub draft: Option<Draft>,
    /// Ids of the chat's pinned messages, ascending. Folded from
    /// `updateMessageIsPinned` (#51); TDLib's `Chat` does not carry them inline,
    /// so this starts empty on projection and the pin/unpin updates maintain it.
    pub pinned_message_ids: Vec<i64>,
}

impl Chat {
    /// Project TDLib's `Chat`.
    #[must_use]
    pub fn from_tdlib(chat: &TdChat) -> Self {
        Self {
            id: chat.id,
            title: crate::sanitize::scrub_line(&chat.title),
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
            draft: chat.draft_message.as_ref().map(Draft::from_tdlib),
            // TDLib delivers pinned-message ids via updateMessageIsPinned, not on
            // the Chat object; the chat store folds them in.
            pinned_message_ids: Vec::new(),
        }
    }

    /// This chat's ordering key in `list`, if it has a position there. The chat
    /// list module sorts each list's view by this.
    #[must_use]
    pub fn order_in(&self, list: &ChatListKind) -> Option<i64> {
        self.positions
            .iter()
            .find(|p| &p.list == list)
            .map(|p| p.order)
    }

    /// This chat's ordering key in the Main list, if any (#17).
    #[must_use]
    pub fn main_order(&self) -> Option<i64> {
        self.order_in(&ChatListKind::Main)
    }

    /// This chat's ordering key in the Archive list, if any (#48).
    #[must_use]
    pub fn archive_order(&self) -> Option<i64> {
        self.order_in(&ChatListKind::Archive)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tdlib_rs::enums::ChatAvailableReactions;
    use tdlib_rs::types::{
        ChatPosition as TdChatPositionT, ChatTypePrivate, ChatTypeSupergroup, Error as TdError,
        FormattedText as TdFormattedTextT, MessageSenderChat, MessageSenderUser,
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
        // A payload-bearing variant that tuigram deliberately does not model.
        let dice = TdMessageContent::MessageDice(tdlib_rs::types::MessageDice::default());
        assert_eq!(
            MessageContent::from_tdlib(&dice),
            MessageContent::Unsupported("messageDice")
        );
    }

    /// A TDLib `File` is a deep record; tests only care about its id (what a
    /// [`FileRef`] keeps), so build one with the rest zeroed.
    fn td_file(id: i32) -> TdFile {
        TdFile {
            id,
            ..Default::default()
        }
    }

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
            })
        );
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
            })
        );
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
    fn location_content_projects_coordinates() {
        // Live-location fields on the message wrapper are dropped; only the point
        // survives, so a live and a static location project identically.
        let content = TdMessageContent::MessageLocation(tdlib_rs::types::MessageLocation {
            location: TdLocation {
                latitude: 51.5,
                longitude: -0.12,
                horizontal_accuracy: 8.0,
            },
            live_period: 900,
            expires_in: 60,
            heading: 90,
            proximity_alert_radius: 0,
        });
        assert_eq!(
            MessageContent::from_tdlib(&content),
            MessageContent::Location(Location {
                latitude: 51.5,
                longitude: -0.12,
                horizontal_accuracy: 8.0,
            })
        );
    }

    #[test]
    fn venue_content_projects_title_address_and_point() {
        let content = TdMessageContent::MessageVenue(tdlib_rs::types::MessageVenue {
            venue: TdVenue {
                location: TdLocation {
                    latitude: 40.0,
                    longitude: -73.0,
                    horizontal_accuracy: 0.0,
                },
                title: "Central Park".to_owned(),
                address: "New York".to_owned(),
                provider: "foursquare".to_owned(),
                id: "abc123".to_owned(),
                r#type: "park".to_owned(),
            },
        });
        assert_eq!(
            MessageContent::from_tdlib(&content),
            MessageContent::Venue(Venue {
                location: Location {
                    latitude: 40.0,
                    longitude: -73.0,
                    horizontal_accuracy: 0.0,
                },
                title: "Central Park".to_owned(),
                address: "New York".to_owned(),
            })
        );
    }

    #[test]
    fn contact_content_projects_name_phone_and_user_id() {
        let content = TdMessageContent::MessageContact(tdlib_rs::types::MessageContact {
            contact: TdContact {
                phone_number: "+15551234".to_owned(),
                first_name: "Ada".to_owned(),
                last_name: "Lovelace".to_owned(),
                vcard: "BEGIN:VCARD…".to_owned(),
                user_id: 7,
            },
        });
        assert_eq!(
            MessageContent::from_tdlib(&content),
            MessageContent::Contact(Contact {
                first_name: "Ada".to_owned(),
                last_name: "Lovelace".to_owned(),
                phone_number: "+15551234".to_owned(),
                user_id: 7,
            })
        );
    }

    /// A TDLib `pollOption` with the fields a test cares about.
    fn td_poll_option(text: &str, voter_count: i32, percentage: i32, chosen: bool) -> TdPollOption {
        TdPollOption {
            text: TdFormattedTextT {
                text: text.to_owned(),
                entities: vec![],
            },
            voter_count,
            vote_percentage: percentage,
            is_chosen: chosen,
            is_being_chosen: false,
        }
    }

    #[test]
    fn poll_content_projects_question_options_and_votes() {
        let content = TdMessageContent::MessagePoll(tdlib_rs::types::MessagePoll {
            poll: TdPoll {
                id: 99,
                question: TdFormattedTextT {
                    text: "Tabs or spaces?".to_owned(),
                    entities: vec![],
                },
                options: vec![
                    td_poll_option("Tabs", 3, 30, false),
                    td_poll_option("Spaces", 7, 70, true),
                ],
                total_voter_count: 10,
                recent_voter_ids: vec![],
                is_anonymous: true,
                r#type: TdPollType::Regular(tdlib_rs::types::PollTypeRegular {
                    allow_multiple_answers: false,
                }),
                open_period: 0,
                close_date: 0,
                is_closed: false,
            },
        });
        assert_eq!(
            MessageContent::from_tdlib(&content),
            MessageContent::Poll(Poll {
                question: FormattedText {
                    text: "Tabs or spaces?".to_owned(),
                    entities: vec![],
                },
                options: vec![
                    PollOption {
                        text: FormattedText {
                            text: "Tabs".to_owned(),
                            entities: vec![],
                        },
                        voter_count: 3,
                        vote_percentage: 30,
                        is_chosen: false,
                    },
                    PollOption {
                        text: FormattedText {
                            text: "Spaces".to_owned(),
                            entities: vec![],
                        },
                        voter_count: 7,
                        vote_percentage: 70,
                        is_chosen: true,
                    },
                ],
                total_voter_count: 10,
                is_anonymous: true,
                is_closed: false,
                kind: PollKind::Regular {
                    allow_multiple_answers: false,
                },
            })
        );
    }

    #[test]
    fn quiz_poll_projects_quiz_kind_with_correct_option_and_explanation() {
        let content = TdMessageContent::MessagePoll(tdlib_rs::types::MessagePoll {
            poll: TdPoll {
                id: 1,
                question: TdFormattedTextT {
                    text: "2 + 2?".to_owned(),
                    entities: vec![],
                },
                options: vec![td_poll_option("4", 0, 0, false)],
                total_voter_count: 0,
                recent_voter_ids: vec![],
                is_anonymous: false,
                r#type: TdPollType::Quiz(tdlib_rs::types::PollTypeQuiz {
                    correct_option_id: 0,
                    explanation: TdFormattedTextT {
                        text: "basic arithmetic".to_owned(),
                        entities: vec![],
                    },
                }),
                open_period: 0,
                close_date: 0,
                is_closed: true,
            },
        });
        let MessageContent::Poll(poll) = MessageContent::from_tdlib(&content) else {
            panic!("expected a poll");
        };
        assert_eq!(
            poll.kind,
            PollKind::Quiz {
                correct_option_id: 0,
                explanation: FormattedText {
                    text: "basic arithmetic".to_owned(),
                    entities: vec![],
                },
            }
        );
        assert!(poll.is_closed);
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
        // The same chat carries a separate Archive position, read independently.
        assert_eq!(chat.archive_order(), Some(5));
        assert_eq!(
            chat.last_message.and_then(|m| m.text().map(str::to_owned)),
            Some("last".to_owned())
        );
    }

    #[test]
    fn chat_list_kind_round_trips_through_tdlib() {
        // to_tdlib then from_tdlib is the identity over every variant — the
        // request side and the fold side agree on each list.
        for kind in [
            ChatListKind::Main,
            ChatListKind::Archive,
            ChatListKind::Folder(7),
        ] {
            assert_eq!(ChatListKind::from_tdlib(&kind.to_tdlib()), kind);
        }
    }

    #[test]
    fn chat_folder_info_projects_id_and_title() {
        // The projection keeps the folder's id and its name's plain text, and
        // drops the icon/color/share metadata tuigram does not model.
        let info = TdChatFolderInfo {
            id: 7,
            name: tdlib_rs::types::ChatFolderName {
                text: TdFormattedTextT {
                    text: "Work".to_owned(),
                    entities: vec![],
                },
                animate_custom_emoji: true,
            },
            icon: tdlib_rs::types::ChatFolderIcon {
                name: "Work".to_owned(),
            },
            color_id: 3,
            is_shareable: true,
            has_my_invite_links: false,
        };

        let folder = ChatFolderInfo::from_tdlib(&info);
        assert_eq!(folder.id, 7);
        assert_eq!(folder.title, "Work");
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
