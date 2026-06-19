//! Chat-list state — the Main list, folded from TDLib's chat-update family.
//!
//! TDLib never hands over "the chat list" as a value; it streams a sequence of
//! updates (`updateNewChat`, `updateChatPosition`, `updateChatLastMessage`,
//! `updateChatReadInbox`, …) and expects the client to maintain the list itself.
//! [`ChatStore`] is that maintained state: the single update router folds each
//! chat-route update into it via [`ChatStore::reduce`], and [`ChatStore::main_list`]
//! reads back an ordered snapshot for the chat list view.
//!
//! Folding is **idempotent** — TDLib repeats and reorders updates freely (on
//! reconnect, on resync, or just because order changed), so re-applying any
//! update converges to the same state rather than double-counting.
//!
//! [`ChatRequests`] is this module's slice of the request surface — only the
//! chat-list requests — owned here rather than in `bridge` so the bridge stays
//! pure transport and a driver depends on just the requests it makes, exactly as
//! [`AuthRequests`](crate::auth::AuthRequests) does for login. The chats arrive
//! asynchronously as updates; the request side only *asks* for more of them
//! ([`load_main_list`]).
//!
//! Scope (#17): the **Main** list only. Archived chats, folders, and secret
//! chats are out of scope (follow-up issues); chats with no Main-list position
//! are simply not part of [`ChatStore::main_list`].

use std::collections::HashMap;

use tdlib_rs::enums::{ChatList, Update};
use tdlib_rs::types::Error as TdError;

use crate::bridge::Bridge;
use crate::model::{Chat, ChatPosition, Message};

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
    /// Ask TDLib to load up to `limit` more chats from the **Main** list.
    ///
    /// This does not return the chats: TDLib loads them into its own state and
    /// emits `updateNewChat` / `updateChatPosition` for any the client did not
    /// already know, which [`ChatStore`] folds. Once the list is fully loaded,
    /// TDLib answers with error [`CHATS_EXHAUSTED`] (404) — the normal end of
    /// paging, which [`load_main_list`] treats as success.
    async fn load_chats(&self, limit: i32) -> Result<(), TdError>;
}

impl ChatRequests for Bridge {
    async fn load_chats(&self, limit: i32) -> Result<(), TdError> {
        // Always the Main list — tuigram's primary view. `Some(Main)` rather
        // than `None` (which TDLib also reads as Main) to keep the intent
        // explicit at the seam.
        tdlib_rs::functions::load_chats(Some(ChatList::Main), limit, self.id()).await
    }
}

/// The TDLib error code returned by `loadChats` once every chat in the list has
/// been loaded. Not a failure — the natural terminal condition of paging.
pub const CHATS_EXHAUSTED: i32 = 404;

/// Page the entire Main chat list, asking for `page` chats at a time until TDLib
/// reports there are no more ([`CHATS_EXHAUSTED`]).
///
/// Only the *requests* are driven here; the chats themselves arrive on the
/// update stream and are folded by [`ChatStore`] on the router task. Any error
/// other than the exhausted sentinel is propagated.
pub async fn load_main_list<C: ChatRequests>(client: &C, page: i32) -> Result<(), TdError> {
    loop {
        match client.load_chats(page).await {
            Ok(()) => {}
            Err(e) if e.code == CHATS_EXHAUSTED => return Ok(()),
            Err(e) => return Err(e),
        }
    }
}

/// The folded chat-list state: every known chat, keyed by id, with an ordered
/// [`main_list`](Self::main_list) view derived from each chat's Main position.
#[derive(Debug, Default)]
pub struct ChatStore {
    chats: HashMap<i64, Chat>,
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
    /// it: `updateNewChat`, `updateChatPosition`, `updateChatLastMessage`, and the
    /// read-state pair `updateChatReadInbox` / `updateChatReadOutbox` (#21). The
    /// catch-all stays inert — the router owns classification, this owns only the
    /// fold — so any other variant reaching here is a harmless no-op.
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
            _ => {}
        }
    }

    /// The Main list, ordered the way TDLib intends it shown: by descending
    /// position order (pinned chats carry higher orders, so they float to the
    /// top), with chat id as a stable tiebreaker. Chats with no Main position
    /// (archived, or not yet positioned) are excluded.
    #[must_use]
    pub fn main_list(&self) -> Vec<&Chat> {
        let mut ordered: Vec<&Chat> = self
            .chats
            .values()
            .filter(|c| c.main_order().is_some())
            .collect();
        // Both keys are `Some` here (filtered above); compare descending, then
        // break ties by id descending so the order is total and stable.
        ordered.sort_by(|a, b| b.main_order().cmp(&a.main_order()).then(b.id.cmp(&a.id)));
        ordered
    }

    /// Look up a chat by id, whatever list it is in.
    #[must_use]
    pub fn get(&self, chat_id: i64) -> Option<&Chat> {
        self.chats.get(&chat_id)
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
    /// TDLib sends `updateNewChat` once, before any position update, so it
    /// usually carries empty positions. If a repeat arrives after positions were
    /// learned (e.g. on resync), keeping the already-folded positions makes the
    /// re-application idempotent rather than wiping the chat off the list.
    fn upsert(&mut self, mut chat: Chat) {
        if chat.positions.is_empty()
            && let Some(existing) = self.chats.get(&chat.id)
        {
            chat.positions = existing.positions.clone();
        }
        self.chats.insert(chat.id, chat);
    }

    /// Apply a single-list position change from `updateChatPosition`. An order of
    /// `0` removes the chat from that list. Unknown chats are ignored: TDLib
    /// always announces a chat before positioning it, so this is only a stale or
    /// out-of-order update, safe to drop.
    fn reposition(&mut self, chat_id: i64, position: ChatPosition) {
        if let Some(chat) = self.chats.get_mut(&chat_id) {
            merge_position(&mut chat.positions, position);
        }
    }

    /// Fold `updateChatLastMessage`: refresh the chat's last message and merge
    /// any positions it carries (TDLib reorders a chat when its last message
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

    /// Fold `updateChatReadOutbox`: the peer has read up to this outgoing message,
    /// so the chat's last-read-outbox marker advances. Unlike the inbox side this
    /// carries no unread counter — it only moves the read horizon for our own
    /// sent messages. Idempotent: re-applying sets the same id.
    fn mark_outbox_read(&mut self, chat_id: i64, last_read_outbox_message_id: i64) {
        if let Some(chat) = self.chats.get_mut(&chat_id) {
            chat.last_read_outbox_message_id = last_read_outbox_message_id;
        }
    }
}

/// Merge one position into a chat's position list: replace any existing position
/// for the same list, dropping it entirely when the new order is `0` (TDLib's
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
    use std::cell::Cell;
    use tdlib_rs::enums::{ChatList as TdChatList, ChatType as TdChatType};
    use tdlib_rs::types::{
        ChatPosition as TdChatPosition, ChatTypePrivate, UpdateChatLastMessage, UpdateChatPosition,
        UpdateChatReadInbox, UpdateChatReadOutbox, UpdateNewChat,
    };

    /// A TDLib `Chat` with every field zeroed but id/title and an empty position
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
            available_reactions: tdlib_rs::enums::ChatAvailableReactions::All(Default::default()),
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
    /// exhausted sentinel, counting every call.
    struct PagingSpy {
        ok_pages: Cell<u32>,
        calls: Cell<u32>,
    }

    impl PagingSpy {
        fn new(ok_pages: u32) -> Self {
            Self {
                ok_pages: Cell::new(ok_pages),
                calls: Cell::new(0),
            }
        }
    }

    impl ChatRequests for PagingSpy {
        async fn load_chats(&self, _limit: i32) -> Result<(), TdError> {
            self.calls.set(self.calls.get() + 1);
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
    }

    #[tokio::test]
    async fn paging_loads_until_exhausted_then_stops() {
        let spy = PagingSpy::new(2);
        load_main_list(&spy, 20).await.unwrap();
        // Two successful pages, then the 404 that ends paging: three calls total.
        assert_eq!(spy.calls.get(), 3);
    }

    /// A non-404 error stops paging and propagates, rather than looping forever.
    struct FailingSpy;

    impl ChatRequests for FailingSpy {
        async fn load_chats(&self, _limit: i32) -> Result<(), TdError> {
            Err(TdError {
                code: 420,
                message: "FLOOD_WAIT".to_owned(),
            })
        }
    }

    #[tokio::test]
    async fn paging_propagates_a_real_error() {
        let err = load_main_list(&FailingSpy, 20).await.unwrap_err();
        assert_eq!(err.code, 420);
    }
}
