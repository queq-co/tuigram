//! Chat actions — the transient "who is typing…" view, kept apart from history.
//!
//! `TDLib` streams `updateChatAction` whenever a sender starts or stops an activity
//! in a chat (typing, recording a voice note, uploading a photo, …) and a
//! matching `chatActionCancel` when they stop or it times out. These are
//! **advisory, ephemeral signals**: they are never part of a chat's message
//! history and must never be persisted into the [`MessageStore`](crate::messages::MessageStore).
//! [`ChatActionStore`] is that separate, transient view — the single update router
//! folds each action-route update into it via [`ChatActionStore::reduce`], and a
//! caller reads back who is currently acting in a chat (and what they are doing)
//! to render an "X is typing…" line.
//!
//! Folding is **idempotent**: re-applying the same action converges on the same
//! per-chat actor set rather than accreting, and a cancel for a sender that is not
//! acting is a harmless no-op. A cancel ([`ChatAction::from_tdlib`] returning
//! `None`) clears just that one sender, leaving everyone else in the chat untouched.
//!
//! [`ChatActionRequests`] is this module's slice of the request surface — only
//! broadcasting *our own* activity — owned here rather than in `bridge` so the
//! bridge stays pure transport and a driver depends on just the requests it makes,
//! exactly as [`UserRequests`](crate::users::UserRequests) and the message seams
//! do. The send path is fire-and-forget: an action is advisory, so a driver issues
//! it alongside composing or sending and **never blocks the send/read path on it**.

use std::collections::HashMap;

use tdlib_rs::enums::Update;
use tdlib_rs::types::Error as TdError;

use crate::bridge::Bridge;
use crate::model::{ChatAction, Sender};

/// The chat-action request seam — tuigram's slice of the `tdlib_rs::functions`
/// surface for broadcasting our own typing/recording/uploading activity,
/// segregated from the auth, chat, message, and user requests so a driver (and
/// its test double) implements only this.
///
/// [`Bridge`] implements it over a live `tdjson` client (via [`Bridge::id`]);
/// tests implement it with a spy. Logic written against `C: ChatActionRequests`
/// runs unchanged on either, with no network and no live `tdjson`.
// Internal seam: every consumer is in-crate and generic over the trait, so the
// lack of a caller-controllable `Send` bound (the reason this lint fires) is not
// a concern here.
#[allow(async_fn_in_trait)]
pub trait ChatActionRequests {
    /// Broadcast a chat action to `chat_id`: `Some(action)` announces the
    /// activity, `None` cancels it (`TDLib`'s `chatActionCancel`).
    ///
    /// Advisory and best-effort — the server rebroadcasts it to the chat's other
    /// members and expires it on its own after a few seconds, so a client repeats
    /// it while still active. It never blocks sending or reading; a driver fires
    /// it and moves on. There is no resulting fold for *our* action — `TDLib` does
    /// not echo our own `updateChatAction` back to us.
    async fn send_chat_action(
        &self,
        chat_id: i64,
        action: Option<ChatAction>,
    ) -> Result<(), TdError>;
}

impl ChatActionRequests for Bridge {
    async fn send_chat_action(
        &self,
        chat_id: i64,
        action: Option<ChatAction>,
    ) -> Result<(), TdError> {
        // topic_id None: the action targets the chat as a whole, not a forum topic.
        tdlib_rs::functions::send_chat_action(
            chat_id,
            None,
            action.map(|a| a.to_tdlib()),
            self.id(),
        )
        .await
    }
}

/// The folded chat-action state: per chat, which senders are currently acting and
/// what they are doing.
///
/// Deliberately *not* part of [`MessageStore`](crate::messages::MessageStore):
/// this is transient presence-style state, dropped as soon as a sender cancels,
/// and never written into history.
#[derive(Debug, Default)]
pub struct ChatActionStore {
    by_chat: HashMap<i64, HashMap<Sender, ChatAction>>,
}

impl ChatActionStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one action-route update into the store.
    ///
    /// `updateChatAction` carries the chat, the acting sender, and the action;
    /// [`ChatAction::from_tdlib`] projects the action, mapping `chatActionCancel`
    /// to `None`. `Some(action)` records the sender as acting (replacing any prior
    /// action); `None` clears just that sender.
    ///
    /// The catch-all stays inert — the router owns classification, this owns only
    /// the fold — so any other variant reaching here is a harmless no-op.
    pub fn reduce(&mut self, update: &Update) {
        if let Update::ChatAction(u) = update {
            let sender = Sender::from_tdlib(&u.sender_id);
            match ChatAction::from_tdlib(&u.action) {
                Some(action) => self.set(u.chat_id, sender, action),
                None => self.clear(u.chat_id, &sender),
            }
        }
    }

    /// The current action of one sender in a chat, if they are acting.
    #[must_use]
    pub fn action(&self, chat_id: i64, sender: &Sender) -> Option<&ChatAction> {
        self.by_chat.get(&chat_id)?.get(sender)
    }

    /// Everyone currently acting in a chat, paired with what they are doing — the
    /// "X is typing…" view for one chat. Empty when no one is acting.
    ///
    /// The order is unspecified (it follows the backing map); a caller that
    /// renders a stable line sorts or picks as it sees fit.
    #[must_use]
    pub fn actors(&self, chat_id: i64) -> Vec<(&Sender, &ChatAction)> {
        self.by_chat
            .get(&chat_id)
            .map(|actors| actors.iter().collect())
            .unwrap_or_default()
    }

    /// Whether anyone is currently acting in a chat.
    #[must_use]
    pub fn is_acting(&self, chat_id: i64) -> bool {
        self.by_chat.contains_key(&chat_id)
    }

    /// Number of chats with at least one sender currently acting.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_chat.len()
    }

    /// Whether no chat has anyone acting.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_chat.is_empty()
    }

    /// Record `sender` as performing `action` in `chat_id`, replacing any earlier
    /// action for that sender (`TDLib` sends the latest activity, e.g. typing then
    /// uploading-a-photo, so a replace is correct).
    fn set(&mut self, chat_id: i64, sender: Sender, action: ChatAction) {
        self.by_chat
            .entry(chat_id)
            .or_default()
            .insert(sender, action);
    }

    /// Clear one sender's action in a chat (a cancel). Removes the chat entry once
    /// its last actor is gone, so [`is_acting`](Self::is_acting) and
    /// [`len`](Self::len) reflect only chats with live activity — and a cancel for
    /// a sender (or chat) that is not acting changes nothing.
    fn clear(&mut self, chat_id: i64, sender: &Sender) {
        if let Some(actors) = self.by_chat.get_mut(&chat_id) {
            actors.remove(sender);
            if actors.is_empty() {
                self.by_chat.remove(&chat_id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use tdlib_rs::enums::{ChatAction as TdChatAction, MessageSender as TdMessageSender};
    use tdlib_rs::types::{
        ChatActionUploadingPhoto, MessageSenderUser, UpdateChatAction, UpdateDeleteMessages,
    };

    /// An `updateChatAction` for `sender` in `chat_id` performing `action`.
    fn chat_action(chat_id: i64, sender_id: i64, action: TdChatAction) -> Update {
        Update::ChatAction(UpdateChatAction {
            chat_id,
            topic_id: None,
            sender_id: TdMessageSender::User(MessageSenderUser { user_id: sender_id }),
            action,
        })
    }

    #[test]
    fn typing_folds_into_the_per_chat_view() {
        let mut store = ChatActionStore::new();
        store.reduce(&chat_action(10, 7, TdChatAction::Typing));

        assert_eq!(
            store.action(10, &Sender::User(7)),
            Some(&ChatAction::Typing)
        );
        assert!(store.is_acting(10));
        assert_eq!(
            store.actors(10),
            vec![(&Sender::User(7), &ChatAction::Typing)]
        );
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn distinct_senders_coexist_in_one_chat() {
        let mut store = ChatActionStore::new();
        store.reduce(&chat_action(10, 7, TdChatAction::Typing));
        store.reduce(&chat_action(
            10,
            8,
            TdChatAction::UploadingPhoto(ChatActionUploadingPhoto { progress: 40 }),
        ));

        assert_eq!(
            store.action(10, &Sender::User(7)),
            Some(&ChatAction::Typing)
        );
        assert_eq!(
            store.action(10, &Sender::User(8)),
            // the upload progress is dropped: the view keeps the activity, not %.
            Some(&ChatAction::UploadingPhoto)
        );
        assert_eq!(store.actors(10).len(), 2);
        // still one chat, two actors within it.
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn a_later_action_replaces_the_senders_earlier_one() {
        let mut store = ChatActionStore::new();
        store.reduce(&chat_action(10, 7, TdChatAction::Typing));
        store.reduce(&chat_action(10, 7, TdChatAction::RecordingVoiceNote));

        assert_eq!(
            store.action(10, &Sender::User(7)),
            Some(&ChatAction::RecordingVoiceNote)
        );
        // replaced in place, not duplicated.
        assert_eq!(store.actors(10).len(), 1);
    }

    #[test]
    fn repeated_identical_action_is_idempotent() {
        let mut store = ChatActionStore::new();
        store.reduce(&chat_action(10, 7, TdChatAction::Typing));
        store.reduce(&chat_action(10, 7, TdChatAction::Typing));

        assert_eq!(store.actors(10).len(), 1);
        assert_eq!(
            store.action(10, &Sender::User(7)),
            Some(&ChatAction::Typing)
        );
    }

    #[test]
    fn cancel_clears_just_that_sender() {
        let mut store = ChatActionStore::new();
        store.reduce(&chat_action(10, 7, TdChatAction::Typing));
        store.reduce(&chat_action(10, 8, TdChatAction::Typing));

        store.reduce(&chat_action(10, 7, TdChatAction::Cancel));

        // 7 is gone, 8 still typing; the chat is still active.
        assert_eq!(store.action(10, &Sender::User(7)), None);
        assert_eq!(
            store.action(10, &Sender::User(8)),
            Some(&ChatAction::Typing)
        );
        assert!(store.is_acting(10));
    }

    #[test]
    fn cancelling_the_last_actor_drops_the_chat() {
        let mut store = ChatActionStore::new();
        store.reduce(&chat_action(10, 7, TdChatAction::Typing));
        store.reduce(&chat_action(10, 7, TdChatAction::Cancel));

        assert!(!store.is_acting(10));
        assert_eq!(store.actors(10), vec![]);
        assert!(store.is_empty());
    }

    #[test]
    fn cancel_for_an_unknown_sender_is_a_no_op() {
        let mut store = ChatActionStore::new();
        // cancel with nothing recorded: synthesizes nothing.
        store.reduce(&chat_action(10, 7, TdChatAction::Cancel));
        assert!(store.is_empty());
        assert!(!store.is_acting(10));
    }

    #[test]
    fn actions_are_isolated_per_chat() {
        let mut store = ChatActionStore::new();
        store.reduce(&chat_action(10, 7, TdChatAction::Typing));
        store.reduce(&chat_action(20, 7, TdChatAction::Typing));

        // cancelling in one chat leaves the other untouched.
        store.reduce(&chat_action(10, 7, TdChatAction::Cancel));
        assert!(!store.is_acting(10));
        assert_eq!(
            store.action(20, &Sender::User(7)),
            Some(&ChatAction::Typing)
        );
    }

    #[test]
    fn non_action_updates_are_ignored_by_the_reducer() {
        let mut store = ChatActionStore::new();
        store.reduce(&chat_action(10, 7, TdChatAction::Typing));
        // a message-route update reaching this reducer (shouldn't happen, but the
        // catch-all must be inert) leaves the view untouched.
        store.reduce(&Update::DeleteMessages(UpdateDeleteMessages {
            chat_id: 10,
            message_ids: vec![1],
            is_permanent: true,
            from_cache: false,
        }));
        assert_eq!(store.actors(10).len(), 1);
    }

    /// A spy `ChatActionRequests` that records each broadcast (chat + action) so a
    /// test asserts the send path drives the seam, including a cancel as `None`.
    #[derive(Default)]
    struct SendActionSpy {
        sent: RefCell<Vec<(i64, Option<ChatAction>)>>,
    }

    impl ChatActionRequests for SendActionSpy {
        async fn send_chat_action(
            &self,
            chat_id: i64,
            action: Option<ChatAction>,
        ) -> Result<(), TdError> {
            self.sent.borrow_mut().push((chat_id, action));
            Ok(())
        }
    }

    #[tokio::test]
    async fn send_chat_action_broadcasts_an_activity_and_a_cancel() {
        let spy = SendActionSpy::default();
        spy.send_chat_action(10, Some(ChatAction::Typing))
            .await
            .unwrap();
        spy.send_chat_action(10, None).await.unwrap();

        assert_eq!(
            *spy.sent.borrow(),
            vec![(10, Some(ChatAction::Typing)), (10, None)]
        );
    }
}
