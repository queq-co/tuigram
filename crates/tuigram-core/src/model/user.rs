//! Users and their transient chat activity: [`Sender`], [`Presence`],
//! [`ChatAction`], [`UserKind`], [`User`].

use tdlib_rs::enums::{
    ChatAction as TdChatAction, MessageSender as TdMessageSender, UserStatus as TdUserStatus,
    UserType as TdUserType,
};
use tdlib_rs::types::{
    MessageSenderChat as TdMessageSenderChat, MessageSenderUser as TdMessageSenderUser,
    User as TdUser,
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
    /// Project `TDLib`'s `MessageSender`.
    #[must_use]
    pub fn from_tdlib(sender: &TdMessageSender) -> Self {
        match sender {
            TdMessageSender::User(u) => Self::User(u.user_id),
            TdMessageSender::Chat(c) => Self::Chat(c.chat_id),
        }
    }

    /// Lower back to `TDLib`'s `MessageSender`, for requests that filter by sender
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

/// A user's online presence — tuigram's projection of `TDLib`'s `UserStatus`.
///
/// Total over the enum with no catch-all, the same discipline as the message
/// content projection: a new `UserStatus` variant fails to compile here until it
/// is classified. The "recently / last week / last month" buckets carry no
/// timestamp on purpose — `TDLib` hides the exact time for those and surfaces only
/// the bucket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Presence {
    /// Status never set, or hidden from us entirely.
    Never,
    /// Online until `expires` (Unix timestamp).
    Online {
        /// Unix timestamp the online status expires at.
        expires: i32,
    },
    /// Offline; last seen at `was_online` (Unix timestamp).
    Offline {
        /// Unix timestamp of when the user was last online.
        was_online: i32,
    },
    /// Online recently — within a few days — with the exact time hidden.
    Recently,
    /// Online within the last week, with the exact time hidden.
    LastWeek,
    /// Online within the last month, with the exact time hidden.
    LastMonth,
}

impl Presence {
    /// Project `TDLib`'s `UserStatus`.
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
/// of `TDLib`'s `ChatAction`, the "X is typing…" / "X is sending a photo…" status.
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
    /// Project `TDLib`'s `ChatAction`. Returns `None` for `chatActionCancel`, which
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

    /// Lower back to `TDLib`'s `ChatAction`, for broadcasting our own activity over
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

/// What kind of account a [`User`] is — tuigram's projection of `TDLib`'s
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
    /// `TDLib` says to treat it exactly like a deleted user.
    Unknown,
}

impl UserKind {
    /// Project `TDLib`'s `UserType`.
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

/// Decode a `TDLib` `Minithumbnail`'s base64 JPEG payload to raw bytes (#201,
/// #208). Shared by every content type that carries one (`User`'s profile
/// photo, `Video`, `Animation`) so the base64 handling lives in one place.
/// `None` when there is no minithumbnail, or its payload fails to decode.
pub(super) fn decode_minithumbnail(
    thumb: Option<&tdlib_rs::types::Minithumbnail>,
) -> Option<Vec<u8>> {
    use base64::Engine as _;
    thumb.and_then(|thumb| {
        base64::engine::general_purpose::STANDARD
            .decode(&thumb.data)
            .ok()
    })
}

/// A user — tuigram's projection of `TDLib`'s `User`, carrying what a sender line
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
    /// Identifier of the accent color for the user's name and header tint
    /// (#194): `TDLib`'s fixed built-in ids are `0..=6`; `>=7` are Telegram
    /// Premium custom colors this crate does not resolve to an exact RGB yet.
    pub accent_color_id: i32,
    /// The user's profile-photo minithumbnail, decoded to raw JPEG bytes
    /// (#201): a small inline preview `TDLib` delivers with the user record
    /// itself, needing no `downloadFile` round trip. `None` when the user has
    /// no profile photo, or has one with no minithumbnail attached.
    pub avatar_minithumbnail: Option<Vec<u8>>,
}

impl User {
    /// Project `TDLib`'s `User`. An empty phone number becomes `None`, and the
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
            accent_color_id: user.accent_color_id,
            avatar_minithumbnail: user
                .profile_photo
                .as_ref()
                .and_then(|photo| decode_minithumbnail(photo.minithumbnail.as_ref())),
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

#[cfg(test)]
#[allow(clippy::unwrap_used)] // tests: panicking on a broken assumption is the point
mod tests {
    use super::*;
    use tdlib_rs::types::{File as TdFile, MessageSenderChat, MessageSenderUser};

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

    /// A `TDLib` `User` with every field zeroed but the ones a test cares about.
    #[allow(clippy::too_many_arguments)]
    fn td_user(
        id: i64,
        first: &str,
        last: &str,
        usernames: Vec<&str>,
        phone: &str,
        kind: TdUserType,
        status: TdUserStatus,
        accent_color_id: i32,
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
            accent_color_id,
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
            3,
        ));
        assert_eq!(user.id, 7);
        assert_eq!(user.username(), Some("ada"));
        assert_eq!(user.usernames, vec!["ada", "countess"]);
        assert_eq!(user.phone_number.as_deref(), Some("+15551234"));
        assert_eq!(user.kind, UserKind::Regular);
        assert_eq!(user.status, Presence::Online { expires: 5 });
        assert_eq!(user.accent_color_id, 3);
        assert_eq!(user.avatar_minithumbnail, None);

        // No usernames and an empty phone collapse to None/empty, not "".
        let bare = User::from_tdlib(&td_user(
            8,
            "Grace",
            "",
            vec![],
            "",
            TdUserType::Regular,
            TdUserStatus::Empty,
            0,
        ));
        assert_eq!(bare.username(), None);
        assert!(bare.usernames.is_empty());
        assert_eq!(bare.phone_number, None);
    }

    #[test]
    fn user_decodes_a_profile_photo_minithumbnail_when_present() {
        use base64::Engine as _;
        let raw = b"not really a jpeg, just test bytes".to_vec();
        let mut td = td_user(
            7,
            "Ada",
            "Lovelace",
            vec![],
            "",
            TdUserType::Regular,
            TdUserStatus::Empty,
            0,
        );
        td.profile_photo = Some(tdlib_rs::types::ProfilePhoto {
            id: 1,
            small: TdFile::default(),
            big: TdFile::default(),
            minithumbnail: Some(tdlib_rs::types::Minithumbnail {
                width: 8,
                height: 8,
                data: base64::engine::general_purpose::STANDARD.encode(&raw),
            }),
            has_animation: false,
            is_personal: false,
        });
        let user = User::from_tdlib(&td);
        assert_eq!(user.avatar_minithumbnail, Some(raw));

        // A profile photo with no minithumbnail attached still projects to None.
        td.profile_photo.as_mut().unwrap().minithumbnail = None;
        assert_eq!(User::from_tdlib(&td).avatar_minithumbnail, None);
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
            0,
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
            0,
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
            0,
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
            0,
        ));
        assert_eq!(anon.display_name(), "User 10");
    }
}
