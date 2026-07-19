//! Chats and their classification: [`ChatKind`], [`SecretChat`],
//! [`ChatListKind`], [`ChatFolderInfo`], [`ChatPosition`], [`Chat`].

use tdlib_rs::enums::{
    ChatList as TdChatList, ChatType as TdChatType, SecretChatState as TdSecretChatState,
};
use tdlib_rs::types::{
    Chat as TdChat, ChatFolderInfo as TdChatFolderInfo, ChatListFolder,
    ChatPosition as TdChatPosition, SecretChat as TdSecretChat,
};

use super::message::{Draft, Message};

/// A chat's classification, with the underlying `TDLib` id for its kind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChatKind {
    /// One-to-one chat with a user.
    Private {
        /// `TDLib` id of the other user in the chat.
        user_id: i64,
    },
    /// Basic group (up to 200 members).
    BasicGroup {
        /// `TDLib` id of the basic group.
        basic_group_id: i64,
    },
    /// Supergroup (large group).
    Supergroup {
        /// `TDLib` id of the supergroup.
        supergroup_id: i64,
    },
    /// Broadcast channel — a supergroup flagged as a channel.
    Channel {
        /// `TDLib` id of the underlying supergroup (channels share the
        /// supergroup id space).
        supergroup_id: i64,
    },
    /// End-to-end encrypted secret chat. Out of Phase 3 messaging scope.
    Secret {
        /// `TDLib` id of the secret chat.
        secret_chat_id: i32,
        /// `TDLib` id of the other user in the secret chat.
        user_id: i64,
    },
}

impl ChatKind {
    /// Project `TDLib`'s `ChatType`. A supergroup with `is_channel` set becomes a
    /// [`ChatKind::Channel`]; the two share `TDLib`'s supergroup id space.
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

/// The lifecycle state of a [`SecretChat`] — tuigram's projection of `TDLib`'s
/// `SecretChatState`. Total over the enum, no catch-all, the same discipline as
/// [`Presence`](super::user::Presence): a new state fails to compile here
/// until it is classified.
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
    /// Project `TDLib`'s `SecretChatState`.
    #[must_use]
    pub fn from_tdlib(state: &TdSecretChatState) -> Self {
        match state {
            TdSecretChatState::Pending => Self::Pending,
            TdSecretChatState::Ready => Self::Ready,
            TdSecretChatState::Closed => Self::Closed,
        }
    }
}

/// An end-to-end encrypted secret chat — tuigram's projection of `TDLib`'s
/// `SecretChat`, the encryption state behind a
/// [`ChatKind::Secret`] chat in the snapshot.
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
    /// Project `TDLib`'s `SecretChat`.
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
    /// completed — `TDLib` rejects a send to a [`Pending`](SecretChatState::Pending)
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
    /// Project `TDLib`'s `ChatList`.
    #[must_use]
    pub fn from_tdlib(list: &TdChatList) -> Self {
        match list {
            TdChatList::Main => Self::Main,
            TdChatList::Archive => Self::Archive,
            TdChatList::Folder(f) => Self::Folder(f.chat_folder_id),
        }
    }

    /// Build `TDLib`'s `ChatList`, for the request side (e.g. selecting which list
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
    /// Project `TDLib`'s `chatFolderInfo` down to the id and display title tuigram
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
    /// Project `TDLib`'s `ChatPosition`.
    #[must_use]
    pub fn from_tdlib(position: &TdChatPosition) -> Self {
        Self {
            list: ChatListKind::from_tdlib(&position.list),
            order: position.order,
            is_pinned: position.is_pinned,
        }
    }
}

/// A chat — tuigram's projection of `TDLib`'s `Chat`, carrying what the chat list
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
    /// `updateMessageIsPinned` (#51); `TDLib`'s `Chat` does not carry them inline,
    /// so this starts empty on projection and the pin/unpin updates maintain it.
    pub pinned_message_ids: Vec<i64>,
}

impl Chat {
    /// Project `TDLib`'s `Chat`.
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
#[allow(clippy::unwrap_used)] // tests: panicking on a broken assumption is the point
mod tests {
    use super::*;
    use crate::model::test_support::{td_message, td_text};
    use tdlib_rs::enums::{ChatAvailableReactions, MessageSender as TdMessageSender};
    use tdlib_rs::types::{
        ChatPosition as TdChatPositionT, ChatTypePrivate, ChatTypeSupergroup,
        FormattedText as TdFormattedTextT, Message as TdMessage, MessageSenderUser,
    };

    /// A `TDLib` `Chat` with every field zeroed but the ones a test cares about.
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
            available_reactions: ChatAvailableReactions::All(
                tdlib_rs::types::ChatAvailableReactionsAll::default(),
            ),
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
}
