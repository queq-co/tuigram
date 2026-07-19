//! The client-side message store: the paged/live-message view
//! ([`MessageStore`]) and the transient search-hit views
//! ([`SearchPage`], [`SearchResults`]) built on top of the request seam in
//! [`super::requests`]. See [`super`] for how the two sides fit together.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;

use tdlib_rs::enums::Update;

use crate::model::{Message, MessageContent, Reaction, SendState};

/// A page of message-search hits plus the cursor for the next page.
///
/// Generic over the cursor `C` because `TDLib` pages the two searches differently:
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
    /// The normalized hits on this page, in the order `TDLib` ranked them.
    pub messages: Vec<Message>,
    /// Approximate total number of matches; `-1` when `TDLib` does not know.
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
/// screen, collapse onto one entry — while preserving `TDLib`'s result ordering.
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
    /// - `updateMessageInteractionInfo` — a reaction change (#51); the known
    ///   message's reactions are replaced in place with the update's buckets, or
    ///   cleared when it carries none (unknown message: ignored).
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
            Update::MessageInteractionInfo(u) => {
                // A reaction change: replace the known message's reactions with
                // this update's buckets (empty when the info or its reactions are
                // absent — i.e. the last reaction was removed).
                self.set_reactions(
                    u.chat_id,
                    u.message_id,
                    crate::model::reactions_from(u.interaction_info.as_ref()),
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
    ///
    /// A re-fetched page **replaces** an already-known message rather than
    /// skipping it: `getChatHistory` is server-authoritative, and is the one
    /// documented recovery path (#207) for a message whose reactions or content
    /// changed while its chat was closed and so never arrived as a live update —
    /// opening the chat and re-paging its history is what catches those up mid
    /// session, exactly as a restart does. A live fold ([`reduce`](Self::reduce))
    /// racing a page fetch for the same id is a narrower, separate concern than
    /// disabling that recovery path is worth trading away here.
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
    /// fold). A no-op if the message is unknown: `TDLib` only edits the content of
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

    /// Replace a known message's reactions in place (the
    /// `updateMessageInteractionInfo` fold). A no-op if the message is unknown —
    /// the update carries no sender/date/content, so we never synthesize a partial
    /// entry from one, the same rule as [`edit_content`](Self::edit_content).
    /// Idempotent: re-applying sets the same buckets; the empty list clears them.
    fn set_reactions(&mut self, chat_id: i64, message_id: i64, reactions: Vec<Reaction>) {
        if let Some(message) = self
            .by_chat
            .get_mut(&chat_id)
            .and_then(|chat| chat.get_mut(&message_id))
        {
            message.reactions = reactions;
        }
    }
}
