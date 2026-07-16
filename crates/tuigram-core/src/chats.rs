//! Chat-list state — the Main list, folded from `TDLib`'s chat-update family.
//!
//! `TDLib` never hands over "the chat list" as a value; it streams a sequence of
//! updates (`updateNewChat`, `updateChatPosition`, `updateChatLastMessage`,
//! `updateChatReadInbox`, `updateChatDraftMessage`, …) and expects the client to
//! maintain the list itself. [`ChatStore`] is that maintained state: the single
//! update router folds each chat-route update into it via [`ChatStore::reduce`],
//! and [`ChatStore::main_list`] reads back an ordered snapshot for the chat list
//! view. A chat's synced compose draft (#38) rides this same family — it is chat
//! state, surfaced on the [`Chat`] snapshot, never in the message store.
//!
//! Folding is **idempotent** — `TDLib` repeats and reorders updates freely (on
//! reconnect, on resync, or just because order changed), so re-applying any
//! update converges to the same state rather than double-counting.
//!
//! [`ChatRequests`] is this module's slice of the request surface — only the
//! chat-list requests — owned here rather than in `bridge` so the bridge stays
//! pure transport and a driver depends on just the requests it makes, exactly as
//! [`AuthRequests`](crate::auth::AuthRequests) does for login. The chats arrive
//! asynchronously as updates; the request side only *asks* for more of them
//! ([`load_main_list`], [`load_archive_list`], [`load_folder_list`]).
//!
//! Scope: the **Main** (#17), **Archive** (#48), and user-defined **folder**
//! (#49) lists. All three fold the same per-list `updateChatPosition` family;
//! [`ChatStore::main_list`], [`ChatStore::archive_list`], and
//! [`ChatStore::folder_list`] read each back ordered. The set of folders itself
//! arrives as `updateChatFolders`, folded into [`ChatStore::folders`]. Secret
//! chats remain out of scope (follow-up issues); a chat is in a list's snapshot
//! only when it has a position there.

use std::collections::HashMap;

use tdlib_rs::enums::Update;
use tdlib_rs::types::Error as TdError;

use crate::bridge::Bridge;
use crate::model::{Chat, ChatFolderInfo, ChatListKind, ChatPosition, Draft, Message};

/// The chat-list request seam — tuigram's chat slice of the
/// `tdlib_rs::functions` surface, segregated from the auth and message requests
/// so a driver (and its test double) implements only these.
///
/// [`Bridge`] implements it over a live `tdjson` client (via [`Bridge::id`]);
/// tests implement it with a spy. Logic written against `C: ChatRequests` runs
/// unchanged on either, with no network and no live `tdjson`.
// Internal seam: every consumer is in-crate and generic over `C: ChatRequests`,
// so the lack of a caller-controllable `Send` bound (the reason this lint fires)
// is not a concern here.
#[allow(async_fn_in_trait)]
pub trait ChatRequests {
    /// Ask `TDLib` to load up to `limit` more chats from `list` (Main, Archive, or
    /// a folder).
    ///
    /// This does not return the chats: `TDLib` loads them into its own state and
    /// emits `updateNewChat` / `updateChatPosition` for any the client did not
    /// already know, which [`ChatStore`] folds. Once the list is fully loaded,
    /// `TDLib` answers with error [`CHATS_EXHAUSTED`] (404) — the normal end of
    /// paging, which the paging drivers treat as success.
    async fn load_chats(&self, list: ChatListKind, limit: i32) -> Result<(), TdError>;

    /// Push a compose draft to a chat, or clear it with `None`.
    ///
    /// `TDLib` persists the draft and syncs it across the account's devices, then
    /// echoes `updateChatDraftMessage`, which [`ChatStore`] folds — so this only
    /// *writes*; the snapshot updates through the router, the same one-way shape
    /// as the read-state and send requests. Idempotent: setting the same draft,
    /// or clearing an absent one, converges.
    async fn set_chat_draft_message(
        &self,
        chat_id: i64,
        draft: Option<Draft>,
    ) -> Result<(), TdError>;
}

impl ChatRequests for Bridge {
    async fn load_chats(&self, list: ChatListKind, limit: i32) -> Result<(), TdError> {
        // `Some(list)` rather than `None` (which TDLib reads as Main) to keep the
        // selected list explicit at the seam.
        tdlib_rs::functions::load_chats(Some(list.to_tdlib()), limit, self.id()).await
    }

    async fn set_chat_draft_message(
        &self,
        chat_id: i64,
        draft: Option<Draft>,
    ) -> Result<(), TdError> {
        // `topic_id` None: the chat's main draft, not a forum-topic draft.
        tdlib_rs::functions::set_chat_draft_message(
            chat_id,
            None,
            draft.map(|d| d.to_tdlib()),
            self.id(),
        )
        .await
    }
}

/// Tell `TDLib` which chat, if any, tuigram currently has open (#207).
///
/// `TDLib` documents several update families — including `updateMessageInteractionInfo`
/// (reactions) and `updateMessageContent` (edits) from *other* devices/users — as
/// guaranteed only for a chat it considers open; a chat never marked open may
/// simply never receive them, leaving the local copy stale until the next full
/// history refetch (e.g. a restart). This is advisory, like the read and reaction
/// seams: it never blocks on a reply, and the live updates it unlocks fold through
/// the usual router path, not through this trait.
#[allow(async_fn_in_trait)]
pub trait ChatLifecycleRequests {
    /// Mark a chat open, `TDLib`'s `openChat`. Call once per genuine open (a chat
    /// switch), not on every re-projection of an already-open chat.
    async fn open_chat(&self, chat_id: i64) -> Result<(), TdError>;

    /// Mark a chat no longer open, `TDLib`'s `closeChat` — the counterpart to
    /// [`open_chat`](Self::open_chat). Call when the chat stops being the one
    /// shown (switching away, or leaving the history pane), including when it is
    /// replaced by a different chat becoming open.
    async fn close_chat(&self, chat_id: i64) -> Result<(), TdError>;
}

impl ChatLifecycleRequests for Bridge {
    async fn open_chat(&self, chat_id: i64) -> Result<(), TdError> {
        tdlib_rs::functions::open_chat(chat_id, self.id()).await
    }

    async fn close_chat(&self, chat_id: i64) -> Result<(), TdError> {
        tdlib_rs::functions::close_chat(chat_id, self.id()).await
    }
}

/// The `TDLib` error code returned by `loadChats` once every chat in the list has
/// been loaded. Not a failure — the natural terminal condition of paging.
pub const CHATS_EXHAUSTED: i32 = 404;

/// Page an entire chat list, asking for `page` chats at a time until `TDLib`
/// reports there are no more ([`CHATS_EXHAUSTED`]).
///
/// Only the *requests* are driven here; the chats themselves arrive on the
/// update stream and are folded by [`ChatStore`] on the router task. Any error
/// other than the exhausted sentinel is propagated. [`load_main_list`],
/// [`load_archive_list`], and [`load_folder_list`] are the per-list entry points.
async fn load_list<C: ChatRequests>(
    client: &C,
    list: ChatListKind,
    page: i32,
) -> Result<(), TdError> {
    loop {
        match client.load_chats(list.clone(), page).await {
            Ok(()) => {}
            Err(e) if e.code == CHATS_EXHAUSTED => return Ok(()),
            Err(e) => return Err(e),
        }
    }
}

/// Page the entire **Main** chat list to exhaustion. See `load_list`.
///
/// # Errors
///
/// Returns an error if `TDLib` fails a page load for a reason other than the
/// list being exhausted.
pub async fn load_main_list<C: ChatRequests>(client: &C, page: i32) -> Result<(), TdError> {
    load_list(client, ChatListKind::Main, page).await
}

/// Page the entire **Archive** chat list to exhaustion (#48). See `load_list`.
///
/// # Errors
///
/// Returns an error if `TDLib` fails a page load for a reason other than the
/// list being exhausted.
pub async fn load_archive_list<C: ChatRequests>(client: &C, page: i32) -> Result<(), TdError> {
    load_list(client, ChatListKind::Archive, page).await
}

/// Page the user-defined **folder** `folder_id` to exhaustion (#49). The folder
/// metadata arrives separately as `updateChatFolders` (see
/// [`ChatStore::folders`]); this pages the chats positioned in it, which fold
/// into [`ChatStore::folder_list`]. See `load_list`.
///
/// # Errors
///
/// Returns an error if `TDLib` fails a page load for a reason other than the
/// list being exhausted.
pub async fn load_folder_list<C: ChatRequests>(
    client: &C,
    folder_id: i32,
    page: i32,
) -> Result<(), TdError> {
    load_list(client, ChatListKind::Folder(folder_id), page).await
}

/// The folded chat-list state: every known chat, keyed by id, with an ordered
/// [`main_list`](Self::main_list) view derived from each chat's Main position,
/// plus the set of user-defined [`folders`](Self::folders) (#49).
#[derive(Debug, Default)]
pub struct ChatStore {
    chats: HashMap<i64, Chat>,
    folders: Vec<ChatFolderInfo>,
}

impl ChatStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one chat-route update into the store.
    ///
    /// Projects the update into tuigram's [model](crate::model) types and applies
    /// it: `updateNewChat`, `updateChatPosition`, `updateChatLastMessage`, the
    /// read-state pair `updateChatReadInbox` / `updateChatReadOutbox` (#21), the
    /// compose `updateChatDraftMessage` (#38), the folder set
    /// `updateChatFolders` (#49), and a message's pin state
    /// `updateMessageIsPinned` (#51), which is chat state — it maintains the
    /// chat's pinned-message set. The catch-all stays inert — the router owns
    /// classification, this owns only the fold — so any other variant reaching
    /// here is a harmless no-op.
    pub fn reduce(&mut self, update: &Update) {
        match update {
            Update::NewChat(u) => self.upsert(Chat::from_tdlib(&u.chat)),
            Update::ChatPosition(u) => {
                self.reposition(u.chat_id, ChatPosition::from_tdlib(&u.position));
            }
            Update::ChatLastMessage(u) => self.relink_last_message(
                u.chat_id,
                u.last_message.as_ref().map(Message::from_tdlib),
                u.positions.iter().map(ChatPosition::from_tdlib).collect(),
            ),
            Update::ChatReadInbox(u) => {
                self.mark_inbox_read(u.chat_id, u.last_read_inbox_message_id, u.unread_count);
            }
            Update::ChatReadOutbox(u) => {
                self.mark_outbox_read(u.chat_id, u.last_read_outbox_message_id);
            }
            Update::ChatDraftMessage(u) => self.set_draft(
                u.chat_id,
                u.draft_message.as_ref().map(Draft::from_tdlib),
                u.positions.iter().map(ChatPosition::from_tdlib).collect(),
            ),
            Update::ChatFolders(u) => self.set_folders(
                u.chat_folders
                    .iter()
                    .map(ChatFolderInfo::from_tdlib)
                    .collect(),
            ),
            Update::MessageIsPinned(u) => {
                self.set_message_pinned(u.chat_id, u.message_id, u.is_pinned);
            }
            _ => {}
        }
    }

    /// One chat list, ordered the way `TDLib` intends it shown: by descending
    /// position order (pinned chats carry higher orders, so they float to the
    /// top), with chat id as a stable tiebreaker. Chats with no position in
    /// `list` are excluded. The per-list views ([`main_list`](Self::main_list),
    /// [`archive_list`](Self::archive_list)) are this, fixed to one list.
    fn ordered_by(&self, list: &ChatListKind) -> Vec<&Chat> {
        let mut ordered: Vec<&Chat> = self
            .chats
            .values()
            .filter(|c| c.order_in(list).is_some())
            .collect();
        // Both keys are `Some` here (filtered above); compare descending, then
        // break ties by id descending so the order is total and stable.
        ordered.sort_by(|a, b| {
            b.order_in(list)
                .cmp(&a.order_in(list))
                .then(b.id.cmp(&a.id))
        });
        ordered
    }

    /// The Main list, ordered highest-first (#17). Chats with no Main position
    /// (archived, or not yet positioned) are excluded.
    #[must_use]
    pub fn main_list(&self) -> Vec<&Chat> {
        self.ordered_by(&ChatListKind::Main)
    }

    /// The Archive list, ordered highest-first (#48). Chats with no Archive
    /// position are excluded; the Main snapshot is independent of this one.
    #[must_use]
    pub fn archive_list(&self) -> Vec<&Chat> {
        self.ordered_by(&ChatListKind::Archive)
    }

    /// One user-defined folder's list, ordered highest-first (#49). Chats with
    /// no position in folder `folder_id` are excluded; each folder's snapshot is
    /// independent of Main, Archive, and the other folders. The folder need not
    /// be a known [`folder`](Self::folders) — a position alone lists a chat here.
    #[must_use]
    pub fn folder_list(&self, folder_id: i32) -> Vec<&Chat> {
        self.ordered_by(&ChatListKind::Folder(folder_id))
    }

    /// The user-defined chat folders, in the order `TDLib` lists them (#49). The
    /// folder *contents* read back via [`folder_list`](Self::folder_list); this
    /// is the set of folders themselves, from the last `updateChatFolders`.
    #[must_use]
    pub fn folders(&self) -> &[ChatFolderInfo] {
        &self.folders
    }

    /// Look up a chat by id, whatever list it is in.
    #[must_use]
    pub fn get(&self, chat_id: i64) -> Option<&Chat> {
        self.chats.get(&chat_id)
    }

    /// Every loaded chat, in no particular order — unlike the per-list views
    /// ([`main_list`](Self::main_list) et al.) this is not filtered by list position,
    /// so it includes chats that have no position yet. The retention sweep (#120)
    /// reads this to group chats by [`ChatKind`](crate::model::ChatKind); it reflects
    /// only what has been paged in, since the app loads chats lazily.
    pub fn iter(&self) -> impl Iterator<Item = &Chat> {
        self.chats.values()
    }

    /// Number of known chats, across all lists.
    #[must_use]
    pub fn len(&self) -> usize {
        self.chats.len()
    }

    /// Whether no chats are known yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.chats.is_empty()
    }

    /// Insert or replace a chat from `updateNewChat`.
    ///
    /// `TDLib` sends `updateNewChat` once, before any position update, so it
    /// usually carries empty positions. If a repeat arrives after positions were
    /// learned (e.g. on resync), keeping the already-folded positions makes the
    /// re-application idempotent rather than wiping the chat off the list.
    fn upsert(&mut self, mut chat: Chat) {
        if chat.positions.is_empty()
            && let Some(existing) = self.chats.get(&chat.id)
        {
            chat.positions.clone_from(&existing.positions);
        }
        self.chats.insert(chat.id, chat);
    }

    /// Apply a single-list position change from `updateChatPosition`. An order of
    /// `0` removes the chat from that list. Unknown chats are ignored: `TDLib`
    /// always announces a chat before positioning it, so this is only a stale or
    /// out-of-order update, safe to drop.
    fn reposition(&mut self, chat_id: i64, position: ChatPosition) {
        if let Some(chat) = self.chats.get_mut(&chat_id) {
            merge_position(&mut chat.positions, position);
        }
    }

    /// Fold `updateChatLastMessage`: refresh the chat's last message and merge
    /// any positions it carries (`TDLib` reorders a chat when its last message
    /// changes, delivering the new positions on the same update).
    fn relink_last_message(
        &mut self,
        chat_id: i64,
        last_message: Option<Message>,
        positions: Vec<ChatPosition>,
    ) {
        if let Some(chat) = self.chats.get_mut(&chat_id) {
            chat.last_message = last_message;
            for position in positions {
                merge_position(&mut chat.positions, position);
            }
        }
    }

    /// Fold `updateChatReadInbox`: the user's last-read message and the unread
    /// counter both advance together.
    fn mark_inbox_read(
        &mut self,
        chat_id: i64,
        last_read_inbox_message_id: i64,
        unread_count: i32,
    ) {
        if let Some(chat) = self.chats.get_mut(&chat_id) {
            chat.last_read_inbox_message_id = last_read_inbox_message_id;
            chat.unread_count = unread_count;
        }
    }

    /// Fold `updateChatDraftMessage`: set or clear the chat's compose draft, and
    /// merge any positions it carries (setting a draft floats the chat up the
    /// list, so `TDLib` delivers the new positions on the same update). A `None`
    /// draft clears it. Idempotent — re-applying sets the same draft. Drafts are
    /// chat state: they land here on the [`Chat`] snapshot and never in the
    /// message store, so a draft is never confused with a sent message. Unknown
    /// chats are ignored (`TDLib` announces a chat before drafting into it).
    fn set_draft(&mut self, chat_id: i64, draft: Option<Draft>, positions: Vec<ChatPosition>) {
        if let Some(chat) = self.chats.get_mut(&chat_id) {
            chat.draft = draft;
            for position in positions {
                merge_position(&mut chat.positions, position);
            }
        }
    }

    /// Fold `updateChatFolders`: `TDLib` delivers the **entire** new folder list on
    /// every change, so this is a wholesale replace — adding, removing, renaming,
    /// or reordering a folder all arrive the same way. Idempotent: re-applying the
    /// same list converges. The chats inside each folder are unaffected; they ride
    /// their own `updateChatPosition`s and read back via
    /// [`folder_list`](Self::folder_list).
    fn set_folders(&mut self, folders: Vec<ChatFolderInfo>) {
        self.folders = folders;
    }

    /// Fold `updateChatReadOutbox`: the peer has read up to this outgoing message,
    /// so the chat's last-read-outbox marker advances. Unlike the inbox side this
    /// carries no unread counter — it only moves the read horizon for our own
    /// sent messages. Idempotent: re-applying sets the same id.
    fn mark_outbox_read(&mut self, chat_id: i64, last_read_outbox_message_id: i64) {
        if let Some(chat) = self.chats.get_mut(&chat_id) {
            chat.last_read_outbox_message_id = last_read_outbox_message_id;
        }
    }

    /// Fold `updateMessageIsPinned` (#51): add `message_id` to the chat's pinned
    /// set when `is_pinned`, or remove it when not. The set is kept sorted and
    /// deduplicated, so the order is deterministic and re-applying either
    /// transition converges — pinning an already-pinned message, or unpinning one
    /// not in the set, is a no-op. Unknown chats are ignored (`TDLib` announces a
    /// chat before pinning within it).
    fn set_message_pinned(&mut self, chat_id: i64, message_id: i64, is_pinned: bool) {
        if let Some(chat) = self.chats.get_mut(&chat_id) {
            match chat.pinned_message_ids.binary_search(&message_id) {
                Ok(idx) if !is_pinned => {
                    chat.pinned_message_ids.remove(idx);
                }
                Err(idx) if is_pinned => chat.pinned_message_ids.insert(idx, message_id),
                // Already in the wanted state: pin of a pinned id / unpin of an
                // absent id — nothing to do.
                _ => {}
            }
        }
    }
}

/// Merge one position into a chat's position list: replace any existing position
/// for the same list, dropping it entirely when the new order is `0` (`TDLib`'s
/// "remove from this list"). Idempotent — re-applying the same position is a
/// no-op.
fn merge_position(positions: &mut Vec<ChatPosition>, position: ChatPosition) {
    positions.retain(|p| p.list != position.list);
    if position.order != 0 {
        positions.push(position);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::FormattedText;
    use std::cell::{Cell, RefCell};
    use tdlib_rs::enums::{ChatList as TdChatList, ChatType as TdChatType};
    use tdlib_rs::types::{
        ChatPosition as TdChatPosition, ChatTypePrivate, UpdateChatDraftMessage,
        UpdateChatLastMessage, UpdateChatPosition, UpdateChatReadInbox, UpdateChatReadOutbox,
        UpdateNewChat,
    };

    /// A `TDLib` `Chat` with every field zeroed but id/title and an empty position
    /// list — positions arrive on their own updates, which is what we exercise.
    fn td_chat(id: i64, title: &str) -> tdlib_rs::types::Chat {
        tdlib_rs::types::Chat {
            id,
            r#type: TdChatType::Private(ChatTypePrivate { user_id: id }),
            title: title.to_owned(),
            photo: None,
            accent_color_id: 0,
            background_custom_emoji_id: 0,
            upgraded_gift_colors: None,
            profile_accent_color_id: 0,
            profile_background_custom_emoji_id: 0,
            permissions: tdlib_rs::types::ChatPermissions::default(),
            last_message: None,
            positions: vec![],
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
            unread_count: 0,
            last_read_inbox_message_id: 0,
            last_read_outbox_message_id: 0,
            unread_mention_count: 0,
            unread_reaction_count: 0,
            notification_settings: tdlib_rs::types::ChatNotificationSettings::default(),
            available_reactions: tdlib_rs::enums::ChatAvailableReactions::All(
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

    fn new_chat(id: i64, title: &str) -> Update {
        Update::NewChat(UpdateNewChat {
            chat: td_chat(id, title),
        })
    }

    /// A Main-list position update for `chat_id` with the given order.
    fn main_position(chat_id: i64, order: i64) -> Update {
        Update::ChatPosition(UpdateChatPosition {
            chat_id,
            position: TdChatPosition {
                list: TdChatList::Main,
                order,
                is_pinned: false,
                source: None,
            },
        })
    }

    /// An Archive-list position update for `chat_id` with the given order.
    fn archive_position(chat_id: i64, order: i64) -> Update {
        Update::ChatPosition(UpdateChatPosition {
            chat_id,
            position: TdChatPosition {
                list: TdChatList::Archive,
                order,
                is_pinned: false,
                source: None,
            },
        })
    }

    /// A folder-list position update for `chat_id` in folder `folder_id`.
    fn folder_position(chat_id: i64, folder_id: i32, order: i64) -> Update {
        Update::ChatPosition(UpdateChatPosition {
            chat_id,
            position: TdChatPosition {
                list: TdChatList::Folder(tdlib_rs::types::ChatListFolder {
                    chat_folder_id: folder_id,
                }),
                order,
                is_pinned: false,
                source: None,
            },
        })
    }

    /// An `updateChatFolders` carrying folders with the given `(id, title)`s.
    fn chat_folders(folders: &[(i32, &str)]) -> Update {
        Update::ChatFolders(tdlib_rs::types::UpdateChatFolders {
            chat_folders: folders
                .iter()
                .map(|&(id, title)| tdlib_rs::types::ChatFolderInfo {
                    id,
                    name: tdlib_rs::types::ChatFolderName {
                        text: tdlib_rs::types::FormattedText {
                            text: title.to_owned(),
                            entities: vec![],
                        },
                        animate_custom_emoji: false,
                    },
                    icon: tdlib_rs::types::ChatFolderIcon {
                        name: "Custom".to_owned(),
                    },
                    color_id: -1,
                    is_shareable: false,
                    has_my_invite_links: false,
                })
                .collect(),
            main_chat_list_position: 0,
            are_tags_enabled: false,
        })
    }

    fn read_inbox(chat_id: i64, last_read: i64, unread: i32) -> Update {
        Update::ChatReadInbox(UpdateChatReadInbox {
            chat_id,
            last_read_inbox_message_id: last_read,
            unread_count: unread,
        })
    }

    fn read_outbox(chat_id: i64, last_read: i64) -> Update {
        Update::ChatReadOutbox(UpdateChatReadOutbox {
            chat_id,
            last_read_outbox_message_id: last_read,
        })
    }

    /// A text-draft update for `chat_id` (replying to `reply_to`, if any), with
    /// no carried positions.
    fn draft_message(chat_id: i64, text: &str, reply_to: Option<i64>) -> Update {
        Update::ChatDraftMessage(UpdateChatDraftMessage {
            chat_id,
            draft_message: Some(tdlib_rs::types::DraftMessage {
                reply_to: reply_to.map(|message_id| {
                    tdlib_rs::enums::InputMessageReplyTo::Message(
                        tdlib_rs::types::InputMessageReplyToMessage {
                            message_id,
                            quote: None,
                            checklist_task_id: 0,
                        },
                    )
                }),
                date: 1_700_000_000,
                input_message_text: tdlib_rs::enums::InputMessageContent::InputMessageText(
                    tdlib_rs::types::InputMessageText {
                        text: tdlib_rs::types::FormattedText {
                            text: text.to_owned(),
                            entities: vec![],
                        },
                        link_preview_options: None,
                        clear_draft: false,
                    },
                ),
                effect_id: 0,
                suggested_post_info: None,
            }),
            positions: vec![],
        })
    }

    /// A draft-cleared update (`draft_message: None`) for `chat_id`.
    fn clear_draft(chat_id: i64) -> Update {
        Update::ChatDraftMessage(UpdateChatDraftMessage {
            chat_id,
            draft_message: None,
            positions: vec![],
        })
    }

    /// A store seeded with two known chats, neither positioned yet.
    fn seeded() -> ChatStore {
        let mut store = ChatStore::new();
        store.reduce(&new_chat(10, "Ten"));
        store.reduce(&new_chat(20, "Twenty"));
        store
    }

    fn ids(chats: &[&Chat]) -> Vec<i64> {
        chats.iter().map(|c| c.id).collect()
    }

    #[test]
    fn positions_order_the_main_list_highest_first() {
        let mut store = seeded();
        store.reduce(&main_position(10, 5));
        store.reduce(&main_position(20, 99));

        // Higher order sorts first.
        assert_eq!(ids(&store.main_list()), vec![20, 10]);
    }

    #[test]
    fn unpositioned_chats_are_absent_from_the_main_list() {
        let mut store = seeded();
        store.reduce(&main_position(10, 7));

        // 20 is known but has no Main position, so it is not in the list.
        assert_eq!(ids(&store.main_list()), vec![10]);
        assert!(store.get(20).is_some());
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn positions_order_the_archive_list_highest_first() {
        let mut store = seeded();
        store.reduce(&archive_position(10, 5));
        store.reduce(&archive_position(20, 99));

        // Same highest-first ordering as Main, read off the Archive positions.
        assert_eq!(ids(&store.archive_list()), vec![20, 10]);
    }

    #[test]
    fn the_two_lists_are_independent() {
        let mut store = seeded();
        store.reduce(&new_chat(30, "Thirty"));
        // 10 in Main only, 30 in Archive only, 20 in both (its Archive order
        // differs from its Main order — TDLib positions each list separately).
        store.reduce(&main_position(10, 5));
        store.reduce(&main_position(20, 99));
        store.reduce(&archive_position(20, 3));
        store.reduce(&archive_position(30, 50));

        // Each snapshot contains only chats positioned in that list, ordered by
        // that list's order — neither leaks into the other.
        assert_eq!(ids(&store.main_list()), vec![20, 10]);
        assert_eq!(ids(&store.archive_list()), vec![30, 20]);
    }

    #[test]
    fn archiving_a_chat_moves_it_between_the_lists() {
        let mut store = seeded();
        store.reduce(&main_position(10, 5));
        store.reduce(&main_position(20, 99));
        assert_eq!(ids(&store.main_list()), vec![20, 10]);

        // 10 leaves Main (order 0 removes the position) and gains an Archive one —
        // the move TDLib delivers as two per-list position updates.
        store.reduce(&main_position(10, 0));
        store.reduce(&archive_position(10, 7));
        assert_eq!(ids(&store.main_list()), vec![20]);
        assert_eq!(ids(&store.archive_list()), vec![10]);
        // The chat itself is still known, just relisted.
        assert!(store.get(10).is_some());
    }

    #[test]
    fn chat_folders_update_folds_the_folder_set() {
        let mut store = ChatStore::new();
        assert!(store.folders().is_empty());

        store.reduce(&chat_folders(&[(2, "Work"), (5, "Family")]));

        // The folder set is folded in TDLib's order, id and title projected.
        let folders: Vec<(i32, &str)> = store
            .folders()
            .iter()
            .map(|f| (f.id, f.title.as_str()))
            .collect();
        assert_eq!(folders, vec![(2, "Work"), (5, "Family")]);
    }

    #[test]
    fn chat_folders_update_replaces_the_whole_set() {
        let mut store = ChatStore::new();
        store.reduce(&chat_folders(&[(2, "Work"), (5, "Family")]));

        // TDLib re-sends the entire list on any change: a rename + removal here
        // is a wholesale replace, not a merge — the dropped folder is gone.
        store.reduce(&chat_folders(&[(2, "Job")]));
        let folders: Vec<(i32, &str)> = store
            .folders()
            .iter()
            .map(|f| (f.id, f.title.as_str()))
            .collect();
        assert_eq!(folders, vec![(2, "Job")]);

        // Re-applying the same list converges rather than duplicating.
        store.reduce(&chat_folders(&[(2, "Job")]));
        assert_eq!(store.folders().len(), 1);
    }

    #[test]
    fn positions_order_a_folder_list_highest_first() {
        let mut store = seeded();
        store.reduce(&folder_position(10, 3, 5));
        store.reduce(&folder_position(20, 3, 99));

        // Same highest-first ordering as the other lists, read off folder 3.
        assert_eq!(ids(&store.folder_list(3)), vec![20, 10]);
    }

    #[test]
    fn folder_lists_are_independent_of_each_other_and_the_main_list() {
        let mut store = seeded();
        store.reduce(&new_chat(30, "Thirty"));
        // 10 in Main, 20 in folder 3, 30 in folder 7 — and 20 also in folder 7
        // with a different order. No list leaks into another.
        store.reduce(&main_position(10, 5));
        store.reduce(&folder_position(20, 3, 99));
        store.reduce(&folder_position(20, 7, 1));
        store.reduce(&folder_position(30, 7, 50));

        assert_eq!(ids(&store.main_list()), vec![10]);
        assert_eq!(ids(&store.folder_list(3)), vec![20]);
        assert_eq!(ids(&store.folder_list(7)), vec![30, 20]);
        // A folder with no positioned chats is simply empty.
        assert!(store.folder_list(99).is_empty());
    }

    #[test]
    fn reapplying_a_position_reorders_idempotently() {
        let mut store = seeded();
        store.reduce(&main_position(10, 5));
        store.reduce(&main_position(20, 99));
        assert_eq!(ids(&store.main_list()), vec![20, 10]);

        // 10 jumps above 20 on a new position; the old one is replaced, not added.
        store.reduce(&main_position(10, 100));
        assert_eq!(ids(&store.main_list()), vec![10, 20]);
        assert_eq!(store.get(10).unwrap().positions.len(), 1);

        // Re-applying the identical update changes nothing.
        store.reduce(&main_position(10, 100));
        assert_eq!(ids(&store.main_list()), vec![10, 20]);
        assert_eq!(store.get(10).unwrap().positions.len(), 1);
    }

    #[test]
    fn order_zero_removes_the_chat_from_the_list() {
        let mut store = seeded();
        store.reduce(&main_position(10, 5));
        store.reduce(&main_position(20, 99));
        assert_eq!(ids(&store.main_list()), vec![20, 10]);

        store.reduce(&main_position(10, 0));
        assert_eq!(ids(&store.main_list()), vec![20]);
        // Still a known chat, just not in the Main list.
        assert!(store.get(10).unwrap().positions.is_empty());
    }

    #[test]
    fn read_inbox_advances_unread_count_and_last_read() {
        let mut store = seeded();
        store.reduce(&read_inbox(10, 42, 3));

        let chat = store.get(10).unwrap();
        assert_eq!(chat.unread_count, 3);
        assert_eq!(chat.last_read_inbox_message_id, 42);
    }

    #[test]
    fn read_outbox_advances_last_read_outbox_idempotently() {
        let mut store = seeded();
        store.reduce(&read_outbox(10, 77));
        assert_eq!(store.get(10).unwrap().last_read_outbox_message_id, 77);

        // The inbox side is untouched — outbox carries no unread counter.
        assert_eq!(store.get(10).unwrap().unread_count, 0);
        assert_eq!(store.get(10).unwrap().last_read_inbox_message_id, 0);

        // Re-applying the same horizon converges; a later one advances it.
        store.reduce(&read_outbox(10, 77));
        assert_eq!(store.get(10).unwrap().last_read_outbox_message_id, 77);
        store.reduce(&read_outbox(10, 120));
        assert_eq!(store.get(10).unwrap().last_read_outbox_message_id, 120);
    }

    #[test]
    fn read_outbox_for_unknown_chat_is_ignored() {
        let mut store = ChatStore::new();
        store.reduce(&read_outbox(999, 5));
        assert!(store.is_empty());
    }

    #[test]
    fn draft_set_update_and_clear_fold_onto_the_chat_snapshot() {
        let mut store = seeded();
        assert!(store.get(10).unwrap().draft.is_none());

        // Set a draft replying to a message.
        store.reduce(&draft_message(10, "half-typed", Some(99)));
        let draft = store.get(10).unwrap().draft.clone().unwrap();
        assert_eq!(draft.text.text, "half-typed");
        assert_eq!(draft.reply_to_message_id, Some(99));

        // Update it (more typed, reply target dropped) — the draft is replaced.
        store.reduce(&draft_message(10, "half-typed more", None));
        let draft = store.get(10).unwrap().draft.clone().unwrap();
        assert_eq!(draft.text.text, "half-typed more");
        assert_eq!(draft.reply_to_message_id, None);

        // Clearing (draft_message: None) removes it.
        store.reduce(&clear_draft(10));
        assert!(store.get(10).unwrap().draft.is_none());
    }

    #[test]
    fn reapplying_a_draft_is_idempotent() {
        let mut store = seeded();
        store.reduce(&draft_message(10, "hi", None));
        let first = store.get(10).unwrap().draft.clone();

        // The identical update converges rather than mutating the draft.
        store.reduce(&draft_message(10, "hi", None));
        assert_eq!(store.get(10).unwrap().draft, first);

        // A redundant clear on an already-absent draft is a no-op, not a panic.
        store.reduce(&clear_draft(20));
        assert!(store.get(20).unwrap().draft.is_none());
    }

    #[test]
    fn draft_for_unknown_chat_is_ignored() {
        let mut store = ChatStore::new();
        store.reduce(&draft_message(999, "ghost", None));
        assert!(store.is_empty());
    }

    #[test]
    fn draft_update_merges_its_positions() {
        let mut store = seeded();
        // A draft that also carries a Main position floats the chat into the list.
        store.reduce(&Update::ChatDraftMessage(UpdateChatDraftMessage {
            chat_id: 10,
            draft_message: None,
            positions: vec![TdChatPosition {
                list: TdChatList::Main,
                order: 50,
                is_pinned: false,
                source: None,
            }],
        }));
        assert_eq!(ids(&store.main_list()), vec![10]);
    }

    #[test]
    fn last_message_update_merges_its_positions() {
        let mut store = seeded();
        store.reduce(&Update::ChatLastMessage(UpdateChatLastMessage {
            chat_id: 10,
            last_message: None,
            positions: vec![TdChatPosition {
                list: TdChatList::Main,
                order: 50,
                is_pinned: false,
                source: None,
            }],
        }));

        // The carried position lands the chat in the Main list.
        assert_eq!(ids(&store.main_list()), vec![10]);
    }

    #[test]
    fn repeated_new_chat_keeps_learned_positions() {
        let mut store = seeded();
        store.reduce(&main_position(10, 5));
        assert_eq!(ids(&store.main_list()), vec![10]);

        // A second updateNewChat (empty positions) must not wipe the chat off
        // the list — re-application is idempotent, not destructive.
        store.reduce(&new_chat(10, "Ten renamed"));
        assert_eq!(ids(&store.main_list()), vec![10]);
        assert_eq!(store.get(10).unwrap().title, "Ten renamed");
    }

    #[test]
    fn position_for_unknown_chat_is_ignored() {
        let mut store = ChatStore::new();
        // No panic, nothing added.
        store.reduce(&main_position(999, 5));
        assert!(store.is_empty());
        assert!(store.main_list().is_empty());
    }

    #[test]
    fn non_chat_updates_are_ignored_by_the_reducer() {
        let mut store = seeded();
        // A message-route update reaching the chat reducer (shouldn't happen, but
        // the catch-all must be inert) leaves the store untouched.
        store.reduce(&Update::DeleteMessages(
            tdlib_rs::types::UpdateDeleteMessages {
                chat_id: 10,
                message_ids: vec![1],
                is_permanent: true,
                from_cache: false,
            },
        ));
        assert_eq!(store.len(), 2);
    }

    /// A spy `ChatRequests` that answers `ok_pages` successful loads and then the
    /// exhausted sentinel, counting every call and recording the list it was asked
    /// to page (so a test can assert which list a driver targets).
    struct PagingSpy {
        ok_pages: Cell<u32>,
        calls: Cell<u32>,
        last_list: RefCell<Option<ChatListKind>>,
    }

    impl PagingSpy {
        fn new(ok_pages: u32) -> Self {
            Self {
                ok_pages: Cell::new(ok_pages),
                calls: Cell::new(0),
                last_list: RefCell::new(None),
            }
        }
    }

    impl ChatRequests for PagingSpy {
        async fn load_chats(&self, list: ChatListKind, _limit: i32) -> Result<(), TdError> {
            self.calls.set(self.calls.get() + 1);
            self.last_list.borrow_mut().replace(list);
            if self.ok_pages.get() > 0 {
                self.ok_pages.set(self.ok_pages.get() - 1);
                Ok(())
            } else {
                Err(TdError {
                    code: CHATS_EXHAUSTED,
                    message: "Not Found".to_owned(),
                })
            }
        }

        async fn set_chat_draft_message(
            &self,
            _chat_id: i64,
            _draft: Option<Draft>,
        ) -> Result<(), TdError> {
            unimplemented!("PagingSpy exercises load paging, not drafts")
        }
    }

    #[tokio::test]
    async fn paging_loads_until_exhausted_then_stops() {
        let spy = PagingSpy::new(2);
        load_main_list(&spy, 20).await.unwrap();
        // Two successful pages, then the 404 that ends paging: three calls total.
        assert_eq!(spy.calls.get(), 3);
        assert_eq!(*spy.last_list.borrow(), Some(ChatListKind::Main));
    }

    #[tokio::test]
    async fn archive_paging_loads_the_archive_list_until_exhausted() {
        let spy = PagingSpy::new(1);
        load_archive_list(&spy, 20).await.unwrap();
        // One page, then the 404: two calls, and the Archive list was the target.
        assert_eq!(spy.calls.get(), 2);
        assert_eq!(*spy.last_list.borrow(), Some(ChatListKind::Archive));
    }

    #[tokio::test]
    async fn folder_paging_loads_that_folder_until_exhausted() {
        let spy = PagingSpy::new(1);
        load_folder_list(&spy, 7, 20).await.unwrap();
        // One page, then the 404: two calls, targeting folder 7 specifically.
        assert_eq!(spy.calls.get(), 2);
        assert_eq!(*spy.last_list.borrow(), Some(ChatListKind::Folder(7)));
    }

    /// A non-404 error stops paging and propagates, rather than looping forever.
    struct FailingSpy;

    impl ChatRequests for FailingSpy {
        async fn load_chats(&self, _list: ChatListKind, _limit: i32) -> Result<(), TdError> {
            Err(TdError {
                code: 420,
                message: "FLOOD_WAIT".to_owned(),
            })
        }

        async fn set_chat_draft_message(
            &self,
            _chat_id: i64,
            _draft: Option<Draft>,
        ) -> Result<(), TdError> {
            unimplemented!("FailingSpy exercises load paging, not drafts")
        }
    }

    #[tokio::test]
    async fn paging_propagates_a_real_error() {
        let err = load_main_list(&FailingSpy, 20).await.unwrap_err();
        assert_eq!(err.code, 420);
    }

    /// One recorded `set_chat_draft_message` call: the chat and the draft pushed
    /// (`None` for a clear).
    #[derive(Debug, PartialEq, Eq)]
    struct DraftCall {
        chat_id: i64,
        draft: Option<Draft>,
    }

    /// A spy `ChatRequests` that records every draft push/clear it is asked to
    /// make, so a test asserts what threaded through the seam.
    struct DraftSpy {
        calls: RefCell<Vec<DraftCall>>,
    }

    impl ChatRequests for DraftSpy {
        async fn load_chats(&self, _list: ChatListKind, _limit: i32) -> Result<(), TdError> {
            unimplemented!("DraftSpy exercises drafts, not load paging")
        }

        async fn set_chat_draft_message(
            &self,
            chat_id: i64,
            draft: Option<Draft>,
        ) -> Result<(), TdError> {
            self.calls.borrow_mut().push(DraftCall { chat_id, draft });
            Ok(())
        }
    }

    #[tokio::test]
    async fn setting_and_clearing_a_draft_threads_through_the_seam() {
        let spy = DraftSpy {
            calls: RefCell::new(Vec::new()),
        };

        let draft = Draft {
            text: FormattedText {
                text: "wip".to_owned(),
                entities: vec![],
            },
            reply_to_message_id: Some(42),
            date: 0,
        };
        spy.set_chat_draft_message(10, Some(draft.clone()))
            .await
            .unwrap();
        // Clearing is the same request with `None`.
        spy.set_chat_draft_message(10, None).await.unwrap();

        assert_eq!(
            *spy.calls.borrow(),
            vec![
                DraftCall {
                    chat_id: 10,
                    draft: Some(draft),
                },
                DraftCall {
                    chat_id: 10,
                    draft: None,
                },
            ]
        );
    }

    /// A spy `ChatLifecycleRequests` recording every open/close call in order, so
    /// a test can assert the exact lifecycle a chat switch drives (#207).
    struct LifecycleSpy {
        calls: RefCell<Vec<(i64, bool)>>,
    }

    impl LifecycleSpy {
        fn new() -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl ChatLifecycleRequests for LifecycleSpy {
        async fn open_chat(&self, chat_id: i64) -> Result<(), TdError> {
            self.calls.borrow_mut().push((chat_id, true));
            Ok(())
        }

        async fn close_chat(&self, chat_id: i64) -> Result<(), TdError> {
            self.calls.borrow_mut().push((chat_id, false));
            Ok(())
        }
    }

    #[tokio::test]
    async fn opening_then_closing_a_chat_threads_through_the_seam_in_order() {
        let spy = LifecycleSpy::new();
        spy.open_chat(10).await.unwrap();
        spy.close_chat(10).await.unwrap();
        spy.open_chat(20).await.unwrap();

        assert_eq!(
            *spy.calls.borrow(),
            vec![(10, true), (10, false), (20, true)]
        );
    }

    /// An `updateMessageIsPinned` toggling `message_id`'s pin state in a chat.
    fn message_is_pinned(chat_id: i64, message_id: i64, is_pinned: bool) -> Update {
        Update::MessageIsPinned(tdlib_rs::types::UpdateMessageIsPinned {
            chat_id,
            message_id,
            is_pinned,
        })
    }

    #[test]
    fn pin_updates_maintain_a_sorted_deduplicated_pinned_set_on_the_chat() {
        let mut store = ChatStore::new();
        store.reduce(&new_chat(10, "Group"));

        // Pins arrive out of order; the set is kept ascending.
        store.reduce(&message_is_pinned(10, 30, true));
        store.reduce(&message_is_pinned(10, 10, true));
        store.reduce(&message_is_pinned(10, 20, true));
        assert_eq!(store.get(10).unwrap().pinned_message_ids, vec![10, 20, 30]);

        // Re-pinning an already-pinned id is a no-op (no duplicate).
        store.reduce(&message_is_pinned(10, 20, true));
        assert_eq!(store.get(10).unwrap().pinned_message_ids, vec![10, 20, 30]);

        // Unpinning removes just that id; the rest keep their order.
        store.reduce(&message_is_pinned(10, 20, false));
        assert_eq!(store.get(10).unwrap().pinned_message_ids, vec![10, 30]);

        // Unpinning an absent id is a no-op.
        store.reduce(&message_is_pinned(10, 999, false));
        assert_eq!(store.get(10).unwrap().pinned_message_ids, vec![10, 30]);
    }

    #[test]
    fn pin_update_for_an_unknown_chat_is_ignored() {
        let mut store = ChatStore::new();
        // TDLib announces a chat before pinning within it; a stray pin is dropped.
        store.reduce(&message_is_pinned(404, 1, true));
        assert!(store.get(404).is_none());
    }
}
