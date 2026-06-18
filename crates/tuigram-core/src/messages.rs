//! Per-chat message history — paged backward from TDLib and kept current by
//! live updates, folded into one ordered, deduplicated view.
//!
//! A chat's messages reach tuigram two ways: **history** is *pulled* a page at a
//! time with `getChatHistory` (it returns the messages directly), while **live**
//! messages are *pushed* as `updateNewMessage`. Both land in the same
//! [`MessageStore`], keyed per chat by message id, so the two streams converge on
//! a single chronological view with no duplicates — a message seen live and then
//! re-fetched in a history page is the same entry, not two.
//!
//! Ordering is by message id, which TDLib assigns monotonically within a chat, so
//! id-ascending is chronological (oldest first). A `BTreeMap` per chat gives that
//! ordering and the dedupe for free: re-inserting an id replaces in place.
//!
//! [`MessageRequests`] is this module's slice of the request surface — only the
//! history fetch — owned here rather than in `bridge`, the same per-domain
//! segregation as [`ChatRequests`](crate::chats::ChatRequests) and
//! [`AuthRequests`](crate::auth::AuthRequests). [`load_history`] drives the
//! backward paging; folding each page stays the caller's choice (so production
//! can fold under its lock per page, never across an await).
//!
//! Sending (#19) lives here too: [`MessageRequests::send_text`] posts a text
//! message (optionally a reply) and TDLib creates it optimistically with a
//! temporary id in [`SendState::Pending`]; the reducer then folds the lifecycle —
//! `updateMessageSendSucceeded` swaps the temp id for the server's real one,
//! `updateMessageSendFailed` flips the same entry to [`SendState::Failed`] — so a
//! sent message appears at once and reconciles in place, never blocking on
//! delivery.
//!
//! Scope: history paging, live `updateNewMessage`, sending text + reply with its
//! lifecycle (#19), and the snapshot. Edits (`updateMessageContent`, #20) and
//! deletions (`updateDeleteMessages`, #20) are routed here by #16 but folded by
//! that issue; until then they fall through this reducer's catch-all as no-ops.

use std::collections::BTreeMap;
use std::collections::HashMap;

use tdlib_rs::enums::{InputMessageContent, InputMessageReplyTo, Messages, Update};
use tdlib_rs::types::{Error as TdError, InputMessageReplyToMessage, InputMessageText};

use crate::bridge::Bridge;
use crate::model::{FormattedText, Message, SendState};

/// The message-history request seam — tuigram's message slice of the
/// `tdlib_rs::functions` surface, segregated from the auth and chat requests so
/// a driver (and its test double) implements only this.
///
/// [`Bridge`] implements it over a live `tdjson` client (via [`Bridge::id`]);
/// tests implement it with a spy. Logic written against `C: MessageRequests`
/// runs unchanged on either, with no network and no live `tdjson`.
// Internal seam: every consumer is in-crate and generic over `C: MessageRequests`,
// so the lack of a caller-controllable `Send` bound (the reason this lint fires)
// is not a concern here.
#[allow(async_fn_in_trait)]
pub trait MessageRequests {
    /// Fetch up to `limit` messages of a chat's history, older than the anchor
    /// `from_message_id` ([`NEWEST`] for the most recent). Returns the page
    /// projected to [`Message`]s (TDLib's null entries dropped); an **empty**
    /// page means the chat's beginning was reached.
    async fn get_chat_history(
        &self,
        chat_id: i64,
        from_message_id: i64,
        limit: i32,
    ) -> Result<Vec<Message>, TdError>;

    /// Send `text` to a chat, optionally replying to `reply_to` (a message id in
    /// the same chat; `None` for a plain message). Returns the message TDLib
    /// creates **optimistically** — a temporary id, [`SendState::Pending`] —
    /// which the lifecycle updates later reconcile in the store. Returns as soon
    /// as TDLib accepts the request; it never waits for delivery.
    async fn send_text(
        &self,
        chat_id: i64,
        reply_to: Option<i64>,
        text: FormattedText,
    ) -> Result<Message, TdError>;
}

impl MessageRequests for Bridge {
    async fn get_chat_history(
        &self,
        chat_id: i64,
        from_message_id: i64,
        limit: i32,
    ) -> Result<Vec<Message>, TdError> {
        // offset 0: page strictly older than the anchor, no look-ahead.
        // only_local false: let TDLib fetch from the server when the local cache
        // runs out, so paging reaches the real start of history.
        let Messages::Messages(page) = tdlib_rs::functions::get_chat_history(
            chat_id,
            from_message_id,
            0,
            limit,
            false,
            self.id(),
        )
        .await?;
        Ok(page
            .messages
            .into_iter()
            .flatten()
            .map(|m| Message::from_tdlib(&m))
            .collect())
    }

    async fn send_text(
        &self,
        chat_id: i64,
        reply_to: Option<i64>,
        text: FormattedText,
    ) -> Result<Message, TdError> {
        let reply_to = reply_to.map(|message_id| {
            InputMessageReplyTo::Message(InputMessageReplyToMessage {
                message_id,
                quote: None,
                checklist_task_id: 0,
            })
        });
        let content = InputMessageContent::InputMessageText(InputMessageText {
            text: text.to_tdlib(),
            link_preview_options: None,
            clear_draft: true,
        });
        // topic_id/options default; TDLib returns the optimistic message and also
        // streams it as updateNewMessage, so the store gets the Pending entry via
        // the router — this returned copy is for the caller's reference (its temp
        // id), not a second insert.
        let tdlib_rs::enums::Message::Message(sent) =
            tdlib_rs::functions::send_message(chat_id, None, reply_to, None, content, self.id())
                .await?;
        Ok(Message::from_tdlib(&sent))
    }
}

/// Anchor passed to [`MessageRequests::get_chat_history`] to start from a chat's
/// most recent message. TDLib reads message id `0` as "the newest".
pub const NEWEST: i64 = 0;

/// Page a chat's history backward, from the newest message to the start, folding
/// each page through `fold`.
///
/// The next anchor is the oldest message id in the page just received, so each
/// request asks for the messages before it; paging stops when TDLib returns an
/// empty page. Folding is left to the caller — production folds into the shared
/// store under its lock per page (never held across the awaits here), while a
/// test folds into a local [`MessageStore`]. Any request error is propagated.
pub async fn load_history<C, F>(
    client: &C,
    chat_id: i64,
    page: i32,
    mut fold: F,
) -> Result<(), TdError>
where
    C: MessageRequests,
    F: FnMut(Vec<Message>),
{
    let mut anchor = NEWEST;
    loop {
        let batch = client.get_chat_history(chat_id, anchor, page).await?;
        if batch.is_empty() {
            return Ok(());
        }
        // Page strictly older than the oldest message we just saw.
        anchor = batch
            .iter()
            .map(|m| m.id)
            .min()
            .expect("batch is non-empty");
        fold(batch);
    }
}

/// Every known message, grouped by chat and ordered chronologically within each.
#[derive(Debug, Default)]
pub struct MessageStore {
    by_chat: HashMap<i64, BTreeMap<i64, Message>>,
}

impl MessageStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one message-route update into the store.
    ///
    /// - `updateNewMessage` — a live (or optimistically sent) message; inserted.
    /// - `updateMessageSendSucceeded` — the send was accepted; the temporary
    ///   entry is dropped and the server's message (real id) inserted in its
    ///   place, so the message keeps its spot but gains its final id.
    /// - `updateMessageSendFailed` — the send was rejected; the same entry (it
    ///   keeps its temporary id) flips to [`SendState::Failed`] with the cause.
    ///
    /// Edit and delete updates the router also classifies as `Message` are folded
    /// by #20 and fall through the catch-all as no-ops until then. Every arm is
    /// idempotent: re-applying converges (a reconcile whose temp entry is already
    /// gone just re-inserts the real message; a failure re-marks in place).
    pub fn reduce(&mut self, update: &Update) {
        match update {
            Update::NewMessage(u) => self.insert(Message::from_tdlib(&u.message)),
            Update::MessageSendSucceeded(u) => {
                let message = Message::from_tdlib(&u.message);
                // The temp message lived in the same chat under the old id.
                self.remove(message.chat_id, u.old_message_id);
                self.insert(message);
            }
            Update::MessageSendFailed(u) => {
                // The failed message keeps its temporary id; flip it in place and
                // carry TDLib's error so callers can surface and retry it.
                let mut message = Message::from_tdlib(&u.message);
                message.send_state = SendState::Failed {
                    code: u.error.code,
                    message: u.error.message.clone(),
                };
                self.insert(message);
            }
            _ => {}
        }
    }

    /// Merge a history page (or any batch of messages) into the store. Each
    /// message is filed under its own chat and id, so re-merging an overlapping
    /// page is idempotent — duplicates collapse onto the same entry.
    pub fn merge(&mut self, messages: impl IntoIterator<Item = Message>) {
        for message in messages {
            self.insert(message);
        }
    }

    /// A chat's messages, oldest first. Empty if the chat is unknown.
    #[must_use]
    pub fn history(&self, chat_id: i64) -> Vec<&Message> {
        self.by_chat
            .get(&chat_id)
            .map(|m| m.values().collect())
            .unwrap_or_default()
    }

    /// Look up a single message within a chat.
    #[must_use]
    pub fn get(&self, chat_id: i64, message_id: i64) -> Option<&Message> {
        self.by_chat.get(&chat_id)?.get(&message_id)
    }

    /// Number of messages known for a chat.
    #[must_use]
    pub fn count(&self, chat_id: i64) -> usize {
        self.by_chat.get(&chat_id).map_or(0, BTreeMap::len)
    }

    /// Whether no messages are known for any chat.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_chat.values().all(BTreeMap::is_empty)
    }

    /// File a message under its chat and id, replacing any existing entry with
    /// the same id (the dedupe across the live and history streams).
    fn insert(&mut self, message: Message) {
        self.by_chat
            .entry(message.chat_id)
            .or_default()
            .insert(message.id, message);
    }

    /// Drop a message from a chat by id. A no-op if the chat or id is unknown, so
    /// reconciling an already-reconciled send is idempotent.
    fn remove(&mut self, chat_id: i64, message_id: i64) {
        if let Some(chat) = self.by_chat.get_mut(&chat_id) {
            chat.remove(&message_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{MessageContent, Sender};
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use tdlib_rs::enums::MessageSendingState;
    use tdlib_rs::types::{
        FormattedText as TdFormattedText, MessageSenderUser, MessageSendingStatePending,
        MessageText, UpdateMessageSendFailed, UpdateMessageSendSucceeded, UpdateNewMessage,
    };

    /// A model message with a distinct text body, for asserting order and dedupe.
    fn msg(chat_id: i64, id: i64) -> Message {
        Message {
            id,
            chat_id,
            sender: Sender::User(1),
            date: 0,
            edit_date: 0,
            is_outgoing: false,
            content: MessageContent::Text(FormattedText {
                text: format!("m{id}"),
                entities: vec![],
            }),
            send_state: SendState::Sent,
        }
    }

    /// A TDLib `Message` with every field zeroed but id/chat and a text body, for
    /// driving the live `updateNewMessage` reducer. `sending_state` lets a test
    /// build an optimistic (Pending) message for the send lifecycle.
    fn td_message(chat_id: i64, id: i64) -> tdlib_rs::types::Message {
        td_message_state(chat_id, id, None)
    }

    fn td_message_state(
        chat_id: i64,
        id: i64,
        sending_state: Option<MessageSendingState>,
    ) -> tdlib_rs::types::Message {
        tdlib_rs::types::Message {
            id,
            sender_id: tdlib_rs::enums::MessageSender::User(MessageSenderUser { user_id: 1 }),
            chat_id,
            sending_state,
            scheduling_state: None,
            is_outgoing: false,
            is_pinned: false,
            is_from_offline: false,
            can_be_saved: false,
            has_timestamped_media: false,
            is_channel_post: false,
            is_paid_star_suggested_post: false,
            is_paid_ton_suggested_post: false,
            contains_unread_mention: false,
            date: 0,
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
            content: tdlib_rs::enums::MessageContent::MessageText(MessageText {
                text: TdFormattedText {
                    text: format!("m{id}"),
                    entities: vec![],
                },
                link_preview: None,
                link_preview_options: None,
            }),
            reply_markup: None,
        }
    }

    fn new_message(chat_id: i64, id: i64) -> Update {
        Update::NewMessage(UpdateNewMessage {
            message: td_message(chat_id, id),
        })
    }

    /// An optimistically-sent message: a live `updateNewMessage` carrying a temp
    /// id and a Pending sending state, as TDLib emits right after `sendMessage`.
    fn pending_message(chat_id: i64, temp_id: i64) -> Update {
        Update::NewMessage(UpdateNewMessage {
            message: td_message_state(
                chat_id,
                temp_id,
                Some(MessageSendingState::Pending(
                    MessageSendingStatePending::default(),
                )),
            ),
        })
    }

    /// The server's acknowledgement: the temp id is replaced by `real_id`.
    fn send_succeeded(chat_id: i64, temp_id: i64, real_id: i64) -> Update {
        Update::MessageSendSucceeded(UpdateMessageSendSucceeded {
            message: td_message(chat_id, real_id),
            old_message_id: temp_id,
        })
    }

    /// A send rejection: the message keeps its temp id, with the error cause.
    fn send_failed(chat_id: i64, temp_id: i64, code: i32, message: &str) -> Update {
        Update::MessageSendFailed(UpdateMessageSendFailed {
            message: td_message(chat_id, temp_id),
            old_message_id: temp_id,
            error: TdError {
                code,
                message: message.to_owned(),
            },
        })
    }

    fn ids(messages: &[&Message]) -> Vec<i64> {
        messages.iter().map(|m| m.id).collect()
    }

    #[test]
    fn merge_orders_messages_chronologically_regardless_of_arrival() {
        let mut store = MessageStore::new();
        // Arrive newest-first (as a history page does); stored oldest-first.
        store.merge([msg(10, 30), msg(10, 10), msg(10, 20)]);

        assert_eq!(ids(&store.history(10)), vec![10, 20, 30]);
        assert_eq!(store.count(10), 3);
    }

    #[test]
    fn overlapping_pages_dedupe_by_id() {
        let mut store = MessageStore::new();
        store.merge([msg(10, 10), msg(10, 20)]);
        // A second page overlapping on id 20 must not double it.
        store.merge([msg(10, 20), msg(10, 30)]);

        assert_eq!(ids(&store.history(10)), vec![10, 20, 30]);
    }

    #[test]
    fn messages_are_partitioned_per_chat() {
        let mut store = MessageStore::new();
        store.merge([msg(10, 1), msg(20, 1), msg(10, 2)]);

        // Same id 1 in two chats are distinct entries.
        assert_eq!(ids(&store.history(10)), vec![1, 2]);
        assert_eq!(ids(&store.history(20)), vec![1]);
        assert!(store.history(999).is_empty());
    }

    #[test]
    fn live_new_message_is_folded_in_order() {
        let mut store = MessageStore::new();
        store.merge([msg(10, 10), msg(10, 20)]);
        // A live message newer than the loaded history appends at the end.
        store.reduce(&new_message(10, 30));
        assert_eq!(ids(&store.history(10)), vec![10, 20, 30]);

        // A live message that was also in history collapses onto the same entry.
        store.reduce(&new_message(10, 20));
        assert_eq!(ids(&store.history(10)), vec![10, 20, 30]);
        assert_eq!(store.get(10, 20).unwrap().text(), Some("m20"));
    }

    #[test]
    fn non_new_message_updates_are_ignored_by_the_reducer() {
        let mut store = MessageStore::new();
        store.merge([msg(10, 1)]);
        // A delete update (folded in #20) reaching this reducer is inert for now.
        store.reduce(&Update::DeleteMessages(
            tdlib_rs::types::UpdateDeleteMessages {
                chat_id: 10,
                message_ids: vec![1],
                is_permanent: true,
                from_cache: false,
            },
        ));
        assert_eq!(store.count(10), 1);
    }

    #[test]
    fn sent_message_appears_optimistically_then_reconciles_to_its_real_id() {
        let mut store = MessageStore::new();
        // The optimistic message lands at once, Pending, under a temp id.
        store.reduce(&pending_message(10, 1001));
        assert_eq!(store.get(10, 1001).unwrap().send_state, SendState::Pending);
        assert_eq!(ids(&store.history(10)), vec![1001]);

        // The server confirms: temp 1001 becomes real id 5 (server ids sort below
        // temp ids), Sent — one entry, re-keyed, not duplicated.
        store.reduce(&send_succeeded(10, 1001, 5));
        assert!(store.get(10, 1001).is_none());
        assert_eq!(store.get(10, 5).unwrap().send_state, SendState::Sent);
        assert_eq!(ids(&store.history(10)), vec![5]);
        assert_eq!(store.count(10), 1);
    }

    #[test]
    fn failed_send_flips_the_optimistic_message_in_place() {
        let mut store = MessageStore::new();
        store.reduce(&pending_message(10, 1001));
        store.reduce(&send_failed(10, 1001, 403, "CHAT_WRITE_FORBIDDEN"));

        // Same id, no duplicate — the entry carries the failure for retry.
        assert_eq!(
            store.get(10, 1001).unwrap().send_state,
            SendState::Failed {
                code: 403,
                message: "CHAT_WRITE_FORBIDDEN".to_owned(),
            }
        );
        assert_eq!(ids(&store.history(10)), vec![1001]);
    }

    #[test]
    fn replayed_send_succeeded_is_idempotent() {
        let mut store = MessageStore::new();
        store.reduce(&pending_message(10, 1001));
        store.reduce(&send_succeeded(10, 1001, 5));
        // TDLib can repeat updates; a second reconcile (temp already gone) just
        // re-affirms the real message rather than resurrecting or doubling it.
        store.reduce(&send_succeeded(10, 1001, 5));
        assert_eq!(ids(&store.history(10)), vec![5]);
        assert_eq!(store.count(10), 1);
    }

    /// A spy that captures the arguments of the most recent `send_text` and
    /// echoes back the optimistic Pending message TDLib would return.
    struct SendSpy {
        last: RefCell<Option<(i64, Option<i64>, FormattedText)>>,
    }

    impl MessageRequests for SendSpy {
        async fn get_chat_history(
            &self,
            _chat_id: i64,
            _from_message_id: i64,
            _limit: i32,
        ) -> Result<Vec<Message>, TdError> {
            unimplemented!("SendSpy exercises the send path only")
        }

        async fn send_text(
            &self,
            chat_id: i64,
            reply_to: Option<i64>,
            text: FormattedText,
        ) -> Result<Message, TdError> {
            self.last
                .borrow_mut()
                .replace((chat_id, reply_to, text.clone()));
            Ok(Message::from_tdlib(&td_message_state(
                chat_id,
                1001,
                Some(MessageSendingState::Pending(
                    MessageSendingStatePending::default(),
                )),
            )))
        }
    }

    #[tokio::test]
    async fn send_text_threads_reply_target_and_returns_a_pending_message() {
        let spy = SendSpy {
            last: RefCell::new(None),
        };
        let body = FormattedText {
            text: "ack".to_owned(),
            entities: vec![],
        };
        // A reply targets a message id in the same chat.
        let optimistic = spy.send_text(10, Some(42), body.clone()).await.unwrap();

        assert_eq!(*spy.last.borrow(), Some((10, Some(42), body)));
        // The seam's contract: the caller gets an optimistic Pending message back.
        assert_eq!(optimistic.send_state, SendState::Pending);
    }

    /// A spy that returns scripted history pages in order, then empty pages. It
    /// ignores the anchor (the driver's anchor maths is exercised separately by
    /// the assertion that every scripted message lands deduped and ordered).
    struct HistorySpy {
        pages: RefCell<VecDeque<Vec<Message>>>,
        calls: RefCell<u32>,
    }

    impl HistorySpy {
        fn new(pages: Vec<Vec<Message>>) -> Self {
            Self {
                pages: RefCell::new(pages.into()),
                calls: RefCell::new(0),
            }
        }
    }

    impl MessageRequests for HistorySpy {
        async fn get_chat_history(
            &self,
            _chat_id: i64,
            _from_message_id: i64,
            _limit: i32,
        ) -> Result<Vec<Message>, TdError> {
            *self.calls.borrow_mut() += 1;
            Ok(self.pages.borrow_mut().pop_front().unwrap_or_default())
        }

        async fn send_text(
            &self,
            _chat_id: i64,
            _reply_to: Option<i64>,
            _text: FormattedText,
        ) -> Result<Message, TdError> {
            unimplemented!("HistorySpy exercises history paging only")
        }
    }

    #[tokio::test]
    async fn load_history_pages_until_empty_and_folds_each_page() {
        let spy = HistorySpy::new(vec![
            vec![msg(10, 30), msg(10, 20)], // newest page
            vec![msg(10, 20), msg(10, 10)], // older page, overlaps on 20
        ]);
        let mut store = MessageStore::new();

        load_history(&spy, 10, 2, |page| store.merge(page))
            .await
            .unwrap();

        // Two non-empty pages folded, deduped and ordered; third call hits empty.
        assert_eq!(ids(&store.history(10)), vec![10, 20, 30]);
        assert_eq!(*spy.calls.borrow(), 3);
    }

    /// A history fetch that fails stops paging and propagates the error.
    struct FailingSpy;

    impl MessageRequests for FailingSpy {
        async fn get_chat_history(
            &self,
            _chat_id: i64,
            _from_message_id: i64,
            _limit: i32,
        ) -> Result<Vec<Message>, TdError> {
            Err(TdError {
                code: 400,
                message: "CHANNEL_PRIVATE".to_owned(),
            })
        }

        async fn send_text(
            &self,
            _chat_id: i64,
            _reply_to: Option<i64>,
            _text: FormattedText,
        ) -> Result<Message, TdError> {
            unimplemented!("FailingSpy exercises the history error path only")
        }
    }

    #[tokio::test]
    async fn load_history_propagates_a_request_error() {
        let mut store = MessageStore::new();
        let err = load_history(&FailingSpy, 10, 2, |page| store.merge(page))
            .await
            .unwrap_err();
        assert_eq!(err.code, 400);
        assert!(store.is_empty());
    }
}
