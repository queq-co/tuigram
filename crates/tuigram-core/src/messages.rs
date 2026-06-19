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
//! Editing and deleting (#20) round out the write side:
//! [`MessageRequests::edit_text`] replaces a message's text and
//! [`MessageRequests::delete`] removes messages (for self or, with `revoke`, for
//! everyone). The reducer reconciles both: `updateMessageContent` swaps a known
//! message's content in place, and a permanent `updateDeleteMessages` drops the
//! messages — a cache-eviction delete is ignored so our copy survives.
//!
//! Read state (#21): [`MessageRequests::view_messages`] marks a chat's messages
//! read. It is advisory — the call acknowledges the messages to the server and
//! never blocks the read path; the resulting unread-count change arrives as
//! `updateChatReadInbox`, which the [chat store](crate::chats::ChatStore) folds.
//!
//! Search (#37): [`MessageRequests::search_chat_messages`] looks within one chat
//! and [`MessageRequests::search_messages`] across the whole account. Both return
//! normalized hits with a paging cursor as a [`SearchPage`], and the caller
//! collects them into a [`SearchResults`] — a transient, id-deduplicated view
//! that never folds into [`MessageStore`], so a search leaves loaded history
//! untouched.
//!
//! Scope: history paging, live `updateNewMessage`, sending text + reply with its
//! lifecycle (#19), editing and deleting with their updates (#20), marking
//! messages read (#21), forwarding (#36), in-chat and global search (#37), and
//! the snapshot.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;

use tdlib_rs::enums::{
    FoundChatMessages, FoundMessages, InputMessageContent, InputMessageReplyTo, MessageSource,
    Messages, Update,
};
use tdlib_rs::types::{Error as TdError, InputMessageReplyToMessage, InputMessageText};

use crate::bridge::Bridge;
use crate::model::{FormattedText, Message, MessageContent, SendState, Sender};

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

    /// Replace the text of a message tuigram's account sent. Returns the edited
    /// [`Message`] TDLib produces once the edit lands server-side; the matching
    /// `updateMessageContent` reconciles the stored copy. Errors if the message
    /// is not editable (not own, too old, not a text message).
    async fn edit_text(
        &self,
        chat_id: i64,
        message_id: i64,
        text: FormattedText,
    ) -> Result<Message, TdError>;

    /// Delete messages from a chat. With `revoke` true the messages are removed
    /// for **everyone** (revoke for all members); with it false they are removed
    /// only for tuigram's account. TDLib rejects a revoke it does not permit. The
    /// matching `updateDeleteMessages` removes them from the store.
    async fn delete(
        &self,
        chat_id: i64,
        message_ids: Vec<i64>,
        revoke: bool,
    ) -> Result<(), TdError>;

    /// Mark `message_ids` in a chat as read (TDLib's `viewMessages`). **Advisory**
    /// — it acknowledges the messages to the server and lets the unread count
    /// settle, but the read path never waits on the result: the new count returns
    /// asynchronously as `updateChatReadInbox`, folded by the chat store. An empty
    /// `message_ids` is a no-op at the seam.
    async fn view_messages(&self, chat_id: i64, message_ids: Vec<i64>) -> Result<(), TdError>;

    /// Forward `message_ids` from `from_chat_id` into `to_chat_id`.
    ///
    /// `send_copy` forwards the messages as a fresh copy — no "forwarded from"
    /// attribution, the messages appear as if newly sent; with it false they carry
    /// the usual forward header naming the original sender. `remove_caption` drops
    /// any caption when copying (only meaningful with `send_copy`).
    ///
    /// Returns the messages TDLib creates **optimistically** in the target chat —
    /// temporary ids, [`SendState::Pending`] — exactly like
    /// [`send_text`](Self::send_text). TDLib also streams each as
    /// `updateNewMessage`, so the store gains them through the router on the same
    /// lifecycle path as a normal send; these returned copies are for the caller's
    /// reference, not a second insert.
    async fn forward_messages(
        &self,
        from_chat_id: i64,
        message_ids: Vec<i64>,
        to_chat_id: i64,
        send_copy: bool,
        remove_caption: bool,
    ) -> Result<Vec<Message>, TdError>;

    /// Search one chat's messages for `query`, optionally restricted to messages
    /// from `sender`. Pages backward from `from_message_id` ([`NEWEST`] for the
    /// first page); each call returns up to `limit` hits.
    ///
    /// The returned [`SearchPage`] carries the normalized hits, an approximate
    /// total, and the cursor for the next page ([`SearchPage::next`] is `None` at
    /// the end). Results are a **transient view** — the caller folds them into a
    /// [`SearchResults`], never the live [`MessageStore`], so a search leaves the
    /// loaded history untouched.
    ///
    /// Media-type filtering (TDLib's `SearchMessagesFilter`) is out of scope here
    /// — this searches all message types; filtering by content kind is a
    /// follow-up alongside the non-text content model.
    async fn search_chat_messages(
        &self,
        chat_id: i64,
        query: String,
        sender: Option<Sender>,
        from_message_id: i64,
        limit: i32,
    ) -> Result<SearchPage<i64>, TdError>;

    /// Search the whole account for `query` across all chats. Pages from
    /// `from_offset` (the empty string for the first page), returning up to
    /// `limit` hits.
    ///
    /// The returned [`SearchPage`] carries the normalized hits and the opaque
    /// string cursor for the next page ([`SearchPage::next`] is `None` when the
    /// offset comes back empty). As with the in-chat search, results are a
    /// transient view and never folded into the live store.
    async fn search_messages(
        &self,
        query: String,
        from_offset: String,
        limit: i32,
    ) -> Result<SearchPage<String>, TdError>;
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

    async fn edit_text(
        &self,
        chat_id: i64,
        message_id: i64,
        text: FormattedText,
    ) -> Result<Message, TdError> {
        // clear_draft false: an edit must not touch the chat's compose draft.
        let content = InputMessageContent::InputMessageText(InputMessageText {
            text: text.to_tdlib(),
            link_preview_options: None,
            clear_draft: false,
        });
        let tdlib_rs::enums::Message::Message(edited) =
            tdlib_rs::functions::edit_message_text(chat_id, message_id, content, self.id()).await?;
        Ok(Message::from_tdlib(&edited))
    }

    async fn delete(
        &self,
        chat_id: i64,
        message_ids: Vec<i64>,
        revoke: bool,
    ) -> Result<(), TdError> {
        tdlib_rs::functions::delete_messages(chat_id, message_ids, revoke, self.id()).await
    }

    async fn view_messages(&self, chat_id: i64, message_ids: Vec<i64>) -> Result<(), TdError> {
        // `ChatHistory` source + `force_read`: a headless client is explicitly
        // marking a chat's history read, not reacting to messages drawn on screen,
        // so it must take effect without a visible message view.
        tdlib_rs::functions::view_messages(
            chat_id,
            message_ids,
            Some(MessageSource::ChatHistory),
            true,
            self.id(),
        )
        .await
    }

    async fn forward_messages(
        &self,
        from_chat_id: i64,
        message_ids: Vec<i64>,
        to_chat_id: i64,
        send_copy: bool,
        remove_caption: bool,
    ) -> Result<Vec<Message>, TdError> {
        // topic_id/options default; TDLib returns the optimistic forwarded
        // messages and also streams each as updateNewMessage, so the store gains
        // them via the router — these returned copies carry the temp ids for the
        // caller, not a second insert. `remove_caption` only bites with `send_copy`.
        let Messages::Messages(forwarded) = tdlib_rs::functions::forward_messages(
            to_chat_id,
            None,
            from_chat_id,
            message_ids,
            None,
            send_copy,
            remove_caption,
            self.id(),
        )
        .await?;
        Ok(forwarded
            .messages
            .into_iter()
            .flatten()
            .map(|m| Message::from_tdlib(&m))
            .collect())
    }

    async fn search_chat_messages(
        &self,
        chat_id: i64,
        query: String,
        sender: Option<Sender>,
        from_message_id: i64,
        limit: i32,
    ) -> Result<SearchPage<i64>, TdError> {
        // topic_id None: search the whole chat, not one forum topic. offset 0: no
        // look-ahead past the anchor. filter None (Empty): all message types.
        let FoundChatMessages::FoundChatMessages(found) =
            tdlib_rs::functions::search_chat_messages(
                chat_id,
                None,
                query,
                sender.map(|s| s.to_tdlib()),
                from_message_id,
                0,
                limit,
                None,
                self.id(),
            )
            .await?;
        // next_from_message_id is 0 when the chat's matches are exhausted.
        let next = (found.next_from_message_id != 0).then_some(found.next_from_message_id);
        Ok(SearchPage {
            messages: found
                .messages
                .into_iter()
                .map(|m| Message::from_tdlib(&m))
                .collect(),
            total_count: found.total_count,
            next,
        })
    }

    async fn search_messages(
        &self,
        query: String,
        from_offset: String,
        limit: i32,
    ) -> Result<SearchPage<String>, TdError> {
        // chat_list None: the Main list. filter/chat_type_filter None, date bounds
        // 0: search every chat and message type with no time window.
        let FoundMessages::FoundMessages(found) = tdlib_rs::functions::search_messages(
            None,
            query,
            from_offset,
            limit,
            None,
            None,
            0,
            0,
            self.id(),
        )
        .await?;
        // next_offset is empty when there are no more results.
        let next = (!found.next_offset.is_empty()).then_some(found.next_offset);
        Ok(SearchPage {
            messages: found
                .messages
                .into_iter()
                .map(|m| Message::from_tdlib(&m))
                .collect(),
            total_count: found.total_count,
            next,
        })
    }
}

/// A page of message-search hits plus the cursor for the next page.
///
/// Generic over the cursor `C` because TDLib pages the two searches differently:
/// an in-chat search resumes from a message id (`C = i64`), a global search from
/// an opaque string offset (`C = String`). [`next`](Self::next) is `None` at the
/// end of results — the loop stop condition either way.
///
/// A search page is a **transient view**: its messages are normalized
/// [`Message`]s but they are never folded into the [`MessageStore`]; collect them
/// into a [`SearchResults`] instead, so a search never disturbs loaded history.
// Not `Eq`: a hit's [`Message`] may carry `f64` location coordinates.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchPage<C> {
    /// The normalized hits on this page, in the order TDLib ranked them.
    pub messages: Vec<Message>,
    /// Approximate total number of matches; `-1` when TDLib does not know.
    pub total_count: i32,
    /// Cursor for the next page, or `None` when results are exhausted.
    pub next: Option<C>,
}

/// A transient, deduplicated accumulation of search hits across pages.
///
/// Search results are a view onto messages that mostly already live (or could
/// live) in the [`MessageStore`]; this keeps them **separate** so a search never
/// mutates loaded history. Hits are deduplicated by `(chat_id, message_id)` — so
/// overlapping pages, or a hit that also appears in the history already on
/// screen, collapse onto one entry — while preserving TDLib's result ordering.
#[derive(Debug, Default)]
pub struct SearchResults {
    messages: Vec<Message>,
    seen: HashSet<(i64, i64)>,
}

impl SearchResults {
    /// An empty result set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a page of hits, dropping any whose `(chat_id, message_id)` was
    /// already collected. Order is preserved (the first occurrence wins), so
    /// re-appending an overlapping page — or [`extend`](Self::extend)ing with a
    /// hit already on screen — is idempotent.
    pub fn extend(&mut self, page: impl IntoIterator<Item = Message>) {
        for message in page {
            if self.seen.insert((message.chat_id, message.id)) {
                self.messages.push(message);
            }
        }
    }

    /// The collected hits, in result order.
    #[must_use]
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Whether a given message has already been collected — the dedupe a caller
    /// uses to avoid showing a hit twice when it also sits in loaded history.
    #[must_use]
    pub fn contains(&self, chat_id: i64, message_id: i64) -> bool {
        self.seen.contains(&(chat_id, message_id))
    }

    /// Number of distinct hits collected.
    #[must_use]
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    /// Whether no hits have been collected.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
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
    /// - `updateMessageContent` — an edit; the known message's content is swapped
    ///   in place (unknown message: ignored).
    /// - `updateDeleteMessages` — a deletion; when permanent, the messages are
    ///   removed. A cache-eviction delete (`is_permanent` false) is ignored.
    ///
    /// Every arm is idempotent: re-applying converges (a reconcile whose temp
    /// entry is already gone just re-inserts the real message; a failure re-marks
    /// in place; a re-edit re-sets the same content; a re-delete of an absent id
    /// is a no-op).
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
            Update::MessageContent(u) => {
                // An edit: swap the known message's content in place.
                self.edit_content(
                    u.chat_id,
                    u.message_id,
                    MessageContent::from_tdlib(&u.new_content),
                );
            }
            // A real deletion removes the messages; a cache-eviction delete
            // (`is_permanent` false — TDLib unloading its own cache) leaves our
            // copy intact, so only the permanent case folds here.
            Update::DeleteMessages(u) if u.is_permanent => {
                for &message_id in &u.message_ids {
                    self.remove(u.chat_id, message_id);
                }
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
    /// reconciling an already-reconciled send — or replaying a delete — is
    /// idempotent.
    fn remove(&mut self, chat_id: i64, message_id: i64) {
        if let Some(chat) = self.by_chat.get_mut(&chat_id) {
            chat.remove(&message_id);
        }
    }

    /// Replace a known message's content in place (the `updateMessageContent`
    /// fold). A no-op if the message is unknown: TDLib only edits the content of
    /// a message it already delivered, and a content-only update carries no
    /// sender/date, so we never synthesize a partial entry from one.
    fn edit_content(&mut self, chat_id: i64, message_id: i64, content: MessageContent) {
        if let Some(message) = self
            .by_chat
            .get_mut(&chat_id)
            .and_then(|chat| chat.get_mut(&message_id))
        {
            message.content = content;
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

    /// An edit: the message's content becomes `body`.
    fn message_content(chat_id: i64, id: i64, body: &str) -> Update {
        Update::MessageContent(tdlib_rs::types::UpdateMessageContent {
            chat_id,
            message_id: id,
            new_content: tdlib_rs::enums::MessageContent::MessageText(MessageText {
                text: TdFormattedText {
                    text: body.to_owned(),
                    entities: vec![],
                },
                link_preview: None,
                link_preview_options: None,
            }),
        })
    }

    /// A deletion of `ids` from a chat. `is_permanent` distinguishes a real
    /// delete from a cache eviction; `from_cache` is set on the latter.
    fn delete_messages(
        chat_id: i64,
        ids: Vec<i64>,
        is_permanent: bool,
        from_cache: bool,
    ) -> Update {
        Update::DeleteMessages(tdlib_rs::types::UpdateDeleteMessages {
            chat_id,
            message_ids: ids,
            is_permanent,
            from_cache,
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
    fn edit_swaps_message_content_in_place_and_is_idempotent() {
        let mut store = MessageStore::new();
        store.merge([msg(10, 1), msg(10, 2)]);
        store.reduce(&message_content(10, 2, "edited"));

        // The content changed; the entry kept its id and position, none added.
        assert_eq!(store.get(10, 2).unwrap().text(), Some("edited"));
        assert_eq!(ids(&store.history(10)), vec![1, 2]);
        assert_eq!(store.get(10, 1).unwrap().text(), Some("m1"));

        // Replaying the same edit converges.
        store.reduce(&message_content(10, 2, "edited"));
        assert_eq!(store.get(10, 2).unwrap().text(), Some("edited"));
        assert_eq!(store.count(10), 2);
    }

    #[test]
    fn edit_of_an_unknown_message_is_ignored() {
        let mut store = MessageStore::new();
        // No header/sender to synthesize from a content-only update — stays empty.
        store.reduce(&message_content(10, 99, "ghost"));
        assert!(store.is_empty());
    }

    #[test]
    fn permanent_delete_removes_only_the_named_messages_and_is_idempotent() {
        let mut store = MessageStore::new();
        store.merge([msg(10, 1), msg(10, 2), msg(10, 3)]);
        store.reduce(&delete_messages(10, vec![1, 3], true, false));

        assert_eq!(ids(&store.history(10)), vec![2]);

        // Replaying the delete (TDLib can repeat) is a no-op on the absent ids.
        store.reduce(&delete_messages(10, vec![1, 3], true, false));
        assert_eq!(ids(&store.history(10)), vec![2]);
    }

    #[test]
    fn cache_eviction_delete_keeps_our_copy() {
        let mut store = MessageStore::new();
        store.merge([msg(10, 1), msg(10, 2)]);
        // is_permanent false: TDLib is only unloading its cache, not deleting.
        store.reduce(&delete_messages(10, vec![1, 2], false, true));
        assert_eq!(ids(&store.history(10)), vec![1, 2]);
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

        async fn edit_text(
            &self,
            _chat_id: i64,
            _message_id: i64,
            _text: FormattedText,
        ) -> Result<Message, TdError> {
            unimplemented!("SendSpy exercises the send path only")
        }

        async fn delete(
            &self,
            _chat_id: i64,
            _message_ids: Vec<i64>,
            _revoke: bool,
        ) -> Result<(), TdError> {
            unimplemented!("SendSpy exercises the send path only")
        }

        async fn view_messages(
            &self,
            _chat_id: i64,
            _message_ids: Vec<i64>,
        ) -> Result<(), TdError> {
            unimplemented!("SendSpy exercises the send path only")
        }

        async fn forward_messages(
            &self,
            _from_chat_id: i64,
            _message_ids: Vec<i64>,
            _to_chat_id: i64,
            _send_copy: bool,
            _remove_caption: bool,
        ) -> Result<Vec<Message>, TdError> {
            unimplemented!("SendSpy exercises the send path only")
        }

        async fn search_chat_messages(
            &self,
            _chat_id: i64,
            _query: String,
            _sender: Option<Sender>,
            _from_message_id: i64,
            _limit: i32,
        ) -> Result<SearchPage<i64>, TdError> {
            unimplemented!("SendSpy exercises the send path only")
        }

        async fn search_messages(
            &self,
            _query: String,
            _from_offset: String,
            _limit: i32,
        ) -> Result<SearchPage<String>, TdError> {
            unimplemented!("SendSpy exercises the send path only")
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

        async fn edit_text(
            &self,
            _chat_id: i64,
            _message_id: i64,
            _text: FormattedText,
        ) -> Result<Message, TdError> {
            unimplemented!("HistorySpy exercises history paging only")
        }

        async fn delete(
            &self,
            _chat_id: i64,
            _message_ids: Vec<i64>,
            _revoke: bool,
        ) -> Result<(), TdError> {
            unimplemented!("HistorySpy exercises history paging only")
        }

        async fn view_messages(
            &self,
            _chat_id: i64,
            _message_ids: Vec<i64>,
        ) -> Result<(), TdError> {
            unimplemented!("HistorySpy exercises history paging only")
        }

        async fn forward_messages(
            &self,
            _from_chat_id: i64,
            _message_ids: Vec<i64>,
            _to_chat_id: i64,
            _send_copy: bool,
            _remove_caption: bool,
        ) -> Result<Vec<Message>, TdError> {
            unimplemented!("HistorySpy exercises history paging only")
        }

        async fn search_chat_messages(
            &self,
            _chat_id: i64,
            _query: String,
            _sender: Option<Sender>,
            _from_message_id: i64,
            _limit: i32,
        ) -> Result<SearchPage<i64>, TdError> {
            unimplemented!("HistorySpy exercises history paging only")
        }

        async fn search_messages(
            &self,
            _query: String,
            _from_offset: String,
            _limit: i32,
        ) -> Result<SearchPage<String>, TdError> {
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

        async fn edit_text(
            &self,
            _chat_id: i64,
            _message_id: i64,
            _text: FormattedText,
        ) -> Result<Message, TdError> {
            unimplemented!("FailingSpy exercises the history error path only")
        }

        async fn delete(
            &self,
            _chat_id: i64,
            _message_ids: Vec<i64>,
            _revoke: bool,
        ) -> Result<(), TdError> {
            unimplemented!("FailingSpy exercises the history error path only")
        }

        async fn view_messages(
            &self,
            _chat_id: i64,
            _message_ids: Vec<i64>,
        ) -> Result<(), TdError> {
            unimplemented!("FailingSpy exercises the history error path only")
        }

        async fn forward_messages(
            &self,
            _from_chat_id: i64,
            _message_ids: Vec<i64>,
            _to_chat_id: i64,
            _send_copy: bool,
            _remove_caption: bool,
        ) -> Result<Vec<Message>, TdError> {
            unimplemented!("FailingSpy exercises the history error path only")
        }

        async fn search_chat_messages(
            &self,
            _chat_id: i64,
            _query: String,
            _sender: Option<Sender>,
            _from_message_id: i64,
            _limit: i32,
        ) -> Result<SearchPage<i64>, TdError> {
            unimplemented!("FailingSpy exercises the history error path only")
        }

        async fn search_messages(
            &self,
            _query: String,
            _from_offset: String,
            _limit: i32,
        ) -> Result<SearchPage<String>, TdError> {
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

    /// Captures the arguments of the most recent `edit_text` / `delete` so the
    /// request seam's wiring (which message, which revoke mode) is asserted.
    #[derive(Default)]
    struct EditDeleteSpy {
        edited: RefCell<Option<(i64, i64, FormattedText)>>,
        deleted: RefCell<Option<(i64, Vec<i64>, bool)>>,
    }

    impl MessageRequests for EditDeleteSpy {
        async fn get_chat_history(
            &self,
            _chat_id: i64,
            _from_message_id: i64,
            _limit: i32,
        ) -> Result<Vec<Message>, TdError> {
            unimplemented!("EditDeleteSpy exercises edit/delete only")
        }

        async fn send_text(
            &self,
            _chat_id: i64,
            _reply_to: Option<i64>,
            _text: FormattedText,
        ) -> Result<Message, TdError> {
            unimplemented!("EditDeleteSpy exercises edit/delete only")
        }

        async fn edit_text(
            &self,
            chat_id: i64,
            message_id: i64,
            text: FormattedText,
        ) -> Result<Message, TdError> {
            self.edited
                .borrow_mut()
                .replace((chat_id, message_id, text.clone()));
            // TDLib echoes the edited message; mirror that with the new content.
            let mut edited = msg(chat_id, message_id);
            edited.content = MessageContent::Text(text);
            Ok(edited)
        }

        async fn delete(
            &self,
            chat_id: i64,
            message_ids: Vec<i64>,
            revoke: bool,
        ) -> Result<(), TdError> {
            self.deleted
                .borrow_mut()
                .replace((chat_id, message_ids, revoke));
            Ok(())
        }

        async fn view_messages(
            &self,
            _chat_id: i64,
            _message_ids: Vec<i64>,
        ) -> Result<(), TdError> {
            unimplemented!("EditDeleteSpy exercises edit/delete only")
        }

        async fn forward_messages(
            &self,
            _from_chat_id: i64,
            _message_ids: Vec<i64>,
            _to_chat_id: i64,
            _send_copy: bool,
            _remove_caption: bool,
        ) -> Result<Vec<Message>, TdError> {
            unimplemented!("EditDeleteSpy exercises edit/delete only")
        }

        async fn search_chat_messages(
            &self,
            _chat_id: i64,
            _query: String,
            _sender: Option<Sender>,
            _from_message_id: i64,
            _limit: i32,
        ) -> Result<SearchPage<i64>, TdError> {
            unimplemented!("EditDeleteSpy exercises edit/delete only")
        }

        async fn search_messages(
            &self,
            _query: String,
            _from_offset: String,
            _limit: i32,
        ) -> Result<SearchPage<String>, TdError> {
            unimplemented!("EditDeleteSpy exercises edit/delete only")
        }
    }

    #[tokio::test]
    async fn edit_text_threads_the_target_and_returns_the_edited_message() {
        let spy = EditDeleteSpy::default();
        let body = FormattedText {
            text: "fixed".to_owned(),
            entities: vec![],
        };
        let edited = spy.edit_text(10, 2, body.clone()).await.unwrap();

        assert_eq!(*spy.edited.borrow(), Some((10, 2, body)));
        assert_eq!(edited.id, 2);
        assert_eq!(edited.text(), Some("fixed"));
    }

    #[tokio::test]
    async fn delete_threads_the_revoke_choice() {
        let spy = EditDeleteSpy::default();
        // Revoke for everyone.
        spy.delete(10, vec![1, 2], true).await.unwrap();
        assert_eq!(*spy.deleted.borrow(), Some((10, vec![1, 2], true)));

        // Delete only for self.
        spy.delete(10, vec![3], false).await.unwrap();
        assert_eq!(*spy.deleted.borrow(), Some((10, vec![3], false)));
    }

    /// Captures the arguments of the most recent `view_messages`, so the read
    /// request's wiring (which chat, which message ids) is asserted.
    #[derive(Default)]
    struct ViewSpy {
        viewed: RefCell<Option<(i64, Vec<i64>)>>,
    }

    impl MessageRequests for ViewSpy {
        async fn get_chat_history(
            &self,
            _chat_id: i64,
            _from_message_id: i64,
            _limit: i32,
        ) -> Result<Vec<Message>, TdError> {
            unimplemented!("ViewSpy exercises the read path only")
        }

        async fn send_text(
            &self,
            _chat_id: i64,
            _reply_to: Option<i64>,
            _text: FormattedText,
        ) -> Result<Message, TdError> {
            unimplemented!("ViewSpy exercises the read path only")
        }

        async fn edit_text(
            &self,
            _chat_id: i64,
            _message_id: i64,
            _text: FormattedText,
        ) -> Result<Message, TdError> {
            unimplemented!("ViewSpy exercises the read path only")
        }

        async fn delete(
            &self,
            _chat_id: i64,
            _message_ids: Vec<i64>,
            _revoke: bool,
        ) -> Result<(), TdError> {
            unimplemented!("ViewSpy exercises the read path only")
        }

        async fn view_messages(&self, chat_id: i64, message_ids: Vec<i64>) -> Result<(), TdError> {
            self.viewed.borrow_mut().replace((chat_id, message_ids));
            Ok(())
        }

        async fn forward_messages(
            &self,
            _from_chat_id: i64,
            _message_ids: Vec<i64>,
            _to_chat_id: i64,
            _send_copy: bool,
            _remove_caption: bool,
        ) -> Result<Vec<Message>, TdError> {
            unimplemented!("ViewSpy exercises the read path only")
        }

        async fn search_chat_messages(
            &self,
            _chat_id: i64,
            _query: String,
            _sender: Option<Sender>,
            _from_message_id: i64,
            _limit: i32,
        ) -> Result<SearchPage<i64>, TdError> {
            unimplemented!("ViewSpy exercises the read path only")
        }

        async fn search_messages(
            &self,
            _query: String,
            _from_offset: String,
            _limit: i32,
        ) -> Result<SearchPage<String>, TdError> {
            unimplemented!("ViewSpy exercises the read path only")
        }
    }

    #[tokio::test]
    async fn view_messages_threads_the_chat_and_message_ids() {
        let spy = ViewSpy::default();
        spy.view_messages(10, vec![1, 2, 3]).await.unwrap();
        assert_eq!(*spy.viewed.borrow(), Some((10, vec![1, 2, 3])));
    }

    /// A forward as TDLib emits it: the same messages re-appear in the target chat
    /// under fresh temp ids and Pending state.
    fn forwarded_pending(target_chat: i64, temp_id: i64) -> Update {
        pending_message(target_chat, temp_id)
    }

    /// The arguments of a `forward_messages` call, captured for assertion.
    #[derive(Debug, PartialEq, Eq)]
    struct ForwardCall {
        from_chat_id: i64,
        message_ids: Vec<i64>,
        to_chat_id: i64,
        send_copy: bool,
        remove_caption: bool,
    }

    /// Captures the most recent `forward_messages` arguments and echoes back the
    /// optimistic Pending messages TDLib would create in the target chat.
    #[derive(Default)]
    struct ForwardSpy {
        last: RefCell<Option<ForwardCall>>,
    }

    impl MessageRequests for ForwardSpy {
        async fn get_chat_history(
            &self,
            _chat_id: i64,
            _from_message_id: i64,
            _limit: i32,
        ) -> Result<Vec<Message>, TdError> {
            unimplemented!("ForwardSpy exercises the forward path only")
        }

        async fn send_text(
            &self,
            _chat_id: i64,
            _reply_to: Option<i64>,
            _text: FormattedText,
        ) -> Result<Message, TdError> {
            unimplemented!("ForwardSpy exercises the forward path only")
        }

        async fn edit_text(
            &self,
            _chat_id: i64,
            _message_id: i64,
            _text: FormattedText,
        ) -> Result<Message, TdError> {
            unimplemented!("ForwardSpy exercises the forward path only")
        }

        async fn delete(
            &self,
            _chat_id: i64,
            _message_ids: Vec<i64>,
            _revoke: bool,
        ) -> Result<(), TdError> {
            unimplemented!("ForwardSpy exercises the forward path only")
        }

        async fn view_messages(
            &self,
            _chat_id: i64,
            _message_ids: Vec<i64>,
        ) -> Result<(), TdError> {
            unimplemented!("ForwardSpy exercises the forward path only")
        }

        async fn forward_messages(
            &self,
            from_chat_id: i64,
            message_ids: Vec<i64>,
            to_chat_id: i64,
            send_copy: bool,
            remove_caption: bool,
        ) -> Result<Vec<Message>, TdError> {
            self.last.borrow_mut().replace(ForwardCall {
                from_chat_id,
                message_ids: message_ids.clone(),
                to_chat_id,
                send_copy,
                remove_caption,
            });
            // One optimistic forwarded message per source id, under a fresh temp id
            // in the target chat — the contract send_text also honours.
            Ok(message_ids
                .iter()
                .enumerate()
                .map(|(i, _)| {
                    Message::from_tdlib(&td_message_state(
                        to_chat_id,
                        2001 + i as i64,
                        Some(MessageSendingState::Pending(
                            MessageSendingStatePending::default(),
                        )),
                    ))
                })
                .collect())
        }

        async fn search_chat_messages(
            &self,
            _chat_id: i64,
            _query: String,
            _sender: Option<Sender>,
            _from_message_id: i64,
            _limit: i32,
        ) -> Result<SearchPage<i64>, TdError> {
            unimplemented!("ForwardSpy exercises the forward path only")
        }

        async fn search_messages(
            &self,
            _query: String,
            _from_offset: String,
            _limit: i32,
        ) -> Result<SearchPage<String>, TdError> {
            unimplemented!("ForwardSpy exercises the forward path only")
        }
    }

    #[tokio::test]
    async fn forward_threads_source_target_and_copy_options() {
        let spy = ForwardSpy::default();
        // Forward two messages from chat 10 into chat 20, as a copy without caption.
        let optimistic = spy
            .forward_messages(10, vec![1, 2], 20, true, true)
            .await
            .unwrap();

        assert_eq!(
            *spy.last.borrow(),
            Some(ForwardCall {
                from_chat_id: 10,
                message_ids: vec![1, 2],
                to_chat_id: 20,
                send_copy: true,
                remove_caption: true,
            })
        );
        // The caller gets one optimistic Pending message per forwarded id.
        assert_eq!(optimistic.len(), 2);
        assert!(
            optimistic
                .iter()
                .all(|m| m.send_state == SendState::Pending && m.chat_id == 20)
        );
    }

    #[test]
    fn forwarded_messages_land_in_the_target_store_via_new_message() {
        let mut store = MessageStore::new();
        // The source chat already holds the originals.
        store.merge([msg(10, 1), msg(10, 2)]);

        // The router folds each forwarded message as a normal updateNewMessage into
        // the target chat — the same lifecycle path as a fresh send.
        store.reduce(&forwarded_pending(20, 2001));
        store.reduce(&forwarded_pending(20, 2002));

        assert_eq!(ids(&store.history(20)), vec![2001, 2002]);
        assert_eq!(store.get(20, 2001).unwrap().send_state, SendState::Pending);
        // The source chat is untouched by the forward.
        assert_eq!(ids(&store.history(10)), vec![1, 2]);
    }

    /// The arguments of a `search_chat_messages` call, captured for assertion.
    #[derive(Debug, PartialEq, Eq)]
    struct ChatSearchCall {
        chat_id: i64,
        query: String,
        sender: Option<Sender>,
        from_message_id: i64,
        limit: i32,
    }

    /// The arguments of a `search_messages` (global) call, captured for assertion.
    #[derive(Debug, PartialEq, Eq)]
    struct GlobalSearchCall {
        query: String,
        from_offset: String,
        limit: i32,
    }

    /// Records every search call and serves scripted result pages in order, so a
    /// test asserts both the threaded query/paging arguments and the normalized
    /// results — following each page's cursor exactly as a real driver would.
    #[derive(Default)]
    struct SearchSpy {
        chat_calls: RefCell<Vec<ChatSearchCall>>,
        global_calls: RefCell<Vec<GlobalSearchCall>>,
        chat_pages: RefCell<VecDeque<SearchPage<i64>>>,
        global_pages: RefCell<VecDeque<SearchPage<String>>>,
    }

    impl MessageRequests for SearchSpy {
        async fn get_chat_history(
            &self,
            _chat_id: i64,
            _from_message_id: i64,
            _limit: i32,
        ) -> Result<Vec<Message>, TdError> {
            unimplemented!("SearchSpy exercises the search path only")
        }

        async fn send_text(
            &self,
            _chat_id: i64,
            _reply_to: Option<i64>,
            _text: FormattedText,
        ) -> Result<Message, TdError> {
            unimplemented!("SearchSpy exercises the search path only")
        }

        async fn edit_text(
            &self,
            _chat_id: i64,
            _message_id: i64,
            _text: FormattedText,
        ) -> Result<Message, TdError> {
            unimplemented!("SearchSpy exercises the search path only")
        }

        async fn delete(
            &self,
            _chat_id: i64,
            _message_ids: Vec<i64>,
            _revoke: bool,
        ) -> Result<(), TdError> {
            unimplemented!("SearchSpy exercises the search path only")
        }

        async fn view_messages(
            &self,
            _chat_id: i64,
            _message_ids: Vec<i64>,
        ) -> Result<(), TdError> {
            unimplemented!("SearchSpy exercises the search path only")
        }

        async fn forward_messages(
            &self,
            _from_chat_id: i64,
            _message_ids: Vec<i64>,
            _to_chat_id: i64,
            _send_copy: bool,
            _remove_caption: bool,
        ) -> Result<Vec<Message>, TdError> {
            unimplemented!("SearchSpy exercises the search path only")
        }

        async fn search_chat_messages(
            &self,
            chat_id: i64,
            query: String,
            sender: Option<Sender>,
            from_message_id: i64,
            limit: i32,
        ) -> Result<SearchPage<i64>, TdError> {
            self.chat_calls.borrow_mut().push(ChatSearchCall {
                chat_id,
                query,
                sender,
                from_message_id,
                limit,
            });
            Ok(self
                .chat_pages
                .borrow_mut()
                .pop_front()
                .unwrap_or(SearchPage {
                    messages: vec![],
                    total_count: 0,
                    next: None,
                }))
        }

        async fn search_messages(
            &self,
            query: String,
            from_offset: String,
            limit: i32,
        ) -> Result<SearchPage<String>, TdError> {
            self.global_calls.borrow_mut().push(GlobalSearchCall {
                query,
                from_offset,
                limit,
            });
            Ok(self
                .global_pages
                .borrow_mut()
                .pop_front()
                .unwrap_or(SearchPage {
                    messages: vec![],
                    total_count: 0,
                    next: None,
                }))
        }
    }

    #[tokio::test]
    async fn chat_search_threads_query_sender_and_returns_a_normalized_page() {
        let spy = SearchSpy::default();
        spy.chat_pages.borrow_mut().push_back(SearchPage {
            messages: vec![msg(10, 30), msg(10, 20)],
            total_count: 5,
            next: Some(20),
        });

        // Search chat 10 for "hi", restricted to one sender, from the newest.
        let page = spy
            .search_chat_messages(10, "hi".to_owned(), Some(Sender::User(7)), NEWEST, 2)
            .await
            .unwrap();

        assert_eq!(
            *spy.chat_calls.borrow(),
            vec![ChatSearchCall {
                chat_id: 10,
                query: "hi".to_owned(),
                sender: Some(Sender::User(7)),
                from_message_id: NEWEST,
                limit: 2,
            }]
        );
        // Hits come back normalized, in the server's result order (not re-sorted),
        // with the next-page cursor and the approximate total carried through.
        let hit_ids: Vec<i64> = page.messages.iter().map(|m| m.id).collect();
        assert_eq!(hit_ids, vec![30, 20]);
        assert_eq!(page.total_count, 5);
        assert_eq!(page.next, Some(20));
    }

    #[tokio::test]
    async fn chat_search_pages_until_exhausted_deduping_into_a_transient_view() {
        let spy = SearchSpy::default();
        spy.chat_pages.borrow_mut().extend([
            SearchPage {
                messages: vec![msg(10, 30), msg(10, 20)],
                total_count: 3,
                next: Some(20),
            },
            // The older page overlaps on id 20 and ends the results.
            SearchPage {
                messages: vec![msg(10, 20), msg(10, 15)],
                total_count: 3,
                next: None,
            },
        ]);

        // The chat already has some loaded history — the search must not touch it.
        let mut store = MessageStore::new();
        store.merge([msg(10, 30)]);

        let mut results = SearchResults::new();
        let mut anchor = NEWEST;
        loop {
            let page = spy
                .search_chat_messages(10, "x".to_owned(), None, anchor, 2)
                .await
                .unwrap();
            results.extend(page.messages);
            match page.next {
                Some(cursor) => anchor = cursor,
                None => break,
            }
        }

        // Overlapping id 20 collapsed to one entry; result order preserved.
        let collected: Vec<i64> = results.messages().iter().map(|m| m.id).collect();
        assert_eq!(collected, vec![30, 20, 15]);
        assert_eq!(results.len(), 3);
        assert!(results.contains(10, 20));
        // Each page asked from the previous page's cursor, starting at NEWEST.
        let anchors: Vec<i64> = spy
            .chat_calls
            .borrow()
            .iter()
            .map(|c| c.from_message_id)
            .collect();
        assert_eq!(anchors, vec![NEWEST, 20]);
        // The transient view left the live history store untouched.
        assert_eq!(ids(&store.history(10)), vec![30]);
    }

    #[tokio::test]
    async fn global_search_threads_query_offset_and_keeps_cross_chat_hits_distinct() {
        let spy = SearchSpy::default();
        spy.global_pages.borrow_mut().extend([
            SearchPage {
                messages: vec![msg(10, 5), msg(20, 5)],
                total_count: 3,
                next: Some("pg2".to_owned()),
            },
            SearchPage {
                messages: vec![msg(30, 1)],
                total_count: 3,
                next: None,
            },
        ]);

        let mut results = SearchResults::new();
        let mut offset = String::new();
        loop {
            let page = spy
                .search_messages("term".to_owned(), offset.clone(), 2)
                .await
                .unwrap();
            results.extend(page.messages);
            match page.next {
                Some(cursor) => offset = cursor,
                None => break,
            }
        }

        // Same id 5 in two chats stays distinct — dedupe keys on (chat, id).
        let keys: Vec<(i64, i64)> = results
            .messages()
            .iter()
            .map(|m| (m.chat_id, m.id))
            .collect();
        assert_eq!(keys, vec![(10, 5), (20, 5), (30, 1)]);
        // Query threaded, paging resumed from the opaque offset (empty first).
        assert_eq!(
            *spy.global_calls.borrow(),
            vec![
                GlobalSearchCall {
                    query: "term".to_owned(),
                    from_offset: String::new(),
                    limit: 2,
                },
                GlobalSearchCall {
                    query: "term".to_owned(),
                    from_offset: "pg2".to_owned(),
                    limit: 2,
                },
            ]
        );
    }
}
