//! Secret chats — the end-to-end encrypted chat lifecycle deferred from Phase 3.
//!
//! A [`ChatKind::Secret`](crate::model::ChatKind::Secret) chat in the snapshot
//! carries only a `secret_chat_id`; the encryption state behind it — whether the
//! key exchange is pending, the chat is ready, or it has been closed, plus the key
//! hash for fingerprint verification — lives in a separate `secretChat` record
//! TDLib streams as `updateSecretChat`. [`SecretChatStore`] is that kept state: the
//! single update router folds each secret-chat update into it via
//! [`SecretChatStore::reduce`], and [`SecretChatStore::get`] resolves the
//! `secret_chat_id` a `ChatKind::Secret` holds back to its [`SecretChat`] — the
//! join that surfaces a secret chat's lifecycle in the chat snapshot.
//!
//! Folding is **idempotent**: TDLib re-announces a `secretChat` on every state
//! change (and on reconnect), so re-applying converges on the latest record rather
//! than accreting. A chat advances pending → ready → closed; the store always
//! holds the most recent state for each id.
//!
//! [`SecretChatRequests`] is this module's slice of the request surface — opening
//! and closing a secret chat — owned here rather than in `bridge`, exactly as
//! [`UserRequests`](crate::users::UserRequests) and the message seams are, so the
//! bridge stays pure transport and a driver depends on just the requests it makes.
//! Creating one returns the newly created [`Chat`] (which also arrives as
//! `updateNewChat` and folds into the [chat store](crate::chats::ChatStore)); the
//! secret-chat record itself follows as `updateSecretChat`.

use std::collections::HashMap;

use tdlib_rs::enums::{Chat as TdChatEnum, Update};
use tdlib_rs::types::Error as TdError;

use crate::bridge::Bridge;
use crate::model::{Chat, SecretChat};

/// The secret-chat request seam — tuigram's slice of the `tdlib_rs::functions`
/// surface for the secret-chat lifecycle, segregated from the auth, chat,
/// message, user, and chat-action requests so a driver (and its test double)
/// implements only this.
///
/// [`Bridge`] implements it over a live `tdjson` client (via [`Bridge::id`]);
/// tests implement it with a spy. Logic written against `C: SecretChatRequests`
/// runs unchanged on either, with no network and no live `tdjson`.
// Internal seam: every consumer is in-crate and generic over the trait, so the
// lack of a caller-controllable `Send` bound (the reason this lint fires) is not
// a concern here.
#[allow(async_fn_in_trait)]
pub trait SecretChatRequests {
    /// Open a new secret chat with `user_id`, returning the created [`Chat`].
    ///
    /// The chat starts [`Pending`](crate::model::SecretChatState::Pending) until
    /// the partner comes online and completes the key exchange. The returned chat
    /// also arrives as `updateNewChat` (folded by the chat store) and the
    /// encryption record as `updateSecretChat` (folded by [`SecretChatStore`]);
    /// this returned copy is for the caller that needs the new chat synchronously.
    async fn create_new_secret_chat(&self, user_id: i64) -> Result<Chat, TdError>;

    /// Close the secret chat `secret_chat_id`, moving it to
    /// [`Closed`](crate::model::SecretChatState::Closed).
    ///
    /// The resulting state change arrives as `updateSecretChat`, which the store
    /// folds; this only acknowledges the request.
    async fn close_secret_chat(&self, secret_chat_id: i32) -> Result<(), TdError>;
}

impl SecretChatRequests for Bridge {
    async fn create_new_secret_chat(&self, user_id: i64) -> Result<Chat, TdError> {
        let TdChatEnum::Chat(chat) =
            tdlib_rs::functions::create_new_secret_chat(user_id, self.id()).await?;
        Ok(Chat::from_tdlib(&chat))
    }

    async fn close_secret_chat(&self, secret_chat_id: i32) -> Result<(), TdError> {
        tdlib_rs::functions::close_secret_chat(secret_chat_id, self.id()).await
    }
}

/// The folded secret-chat state: every known secret chat, keyed by its id.
#[derive(Debug, Default)]
pub struct SecretChatStore {
    chats: HashMap<i32, SecretChat>,
}

impl SecretChatStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one secret-chat update into the store.
    ///
    /// `updateSecretChat` carries the full record; inserted or replaced, so a
    /// lifecycle advance (pending → ready → closed) overwrites the prior state in
    /// place. The catch-all stays inert — the router owns classification, this
    /// owns only the fold — so any other variant reaching here is a harmless no-op.
    pub fn reduce(&mut self, update: &Update) {
        if let Update::SecretChat(u) = update {
            self.upsert(SecretChat::from_tdlib(&u.secret_chat));
        }
    }

    /// Resolve a secret chat by id — the join a
    /// [`ChatKind::Secret`](crate::model::ChatKind::Secret) snapshot uses to
    /// surface its encryption state and key hash.
    #[must_use]
    pub fn get(&self, secret_chat_id: i32) -> Option<&SecretChat> {
        self.chats.get(&secret_chat_id)
    }

    /// Number of known secret chats.
    #[must_use]
    pub fn len(&self) -> usize {
        self.chats.len()
    }

    /// Whether no secret chats are known yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.chats.is_empty()
    }

    /// Insert or replace a secret chat from `updateSecretChat`. TDLib sends the
    /// full record on every change, so a replace is correct.
    fn upsert(&mut self, chat: SecretChat) {
        self.chats.insert(chat.id, chat);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ChatKind, SecretChatState};
    use std::cell::RefCell;
    use tdlib_rs::enums::SecretChatState as TdSecretChatState;
    use tdlib_rs::types::{SecretChat as TdSecretChat, UpdateDeleteMessages, UpdateSecretChat};

    /// A bare secret [`Chat`] for the create-seam spy to hand back — only the id
    /// and kind matter to the assertions.
    fn stub_secret_chat(id: i64, user_id: i64) -> Chat {
        Chat {
            id,
            title: String::new(),
            kind: ChatKind::Secret {
                secret_chat_id: id as i32,
                user_id,
            },
            last_message: None,
            unread_count: 0,
            unread_mention_count: 0,
            last_read_inbox_message_id: 0,
            last_read_outbox_message_id: 0,
            positions: vec![],
            draft: None,
            pinned_message_ids: vec![],
        }
    }

    /// An `updateSecretChat` carrying a record in `state` (key hash present only
    /// once ready, as TDLib does).
    fn secret_chat(id: i32, user_id: i64, state: TdSecretChatState, is_outbound: bool) -> Update {
        let key_hash = if matches!(state, TdSecretChatState::Ready) {
            "fingerprint".to_owned()
        } else {
            String::new()
        };
        Update::SecretChat(UpdateSecretChat {
            secret_chat: TdSecretChat {
                id,
                user_id,
                state,
                is_outbound,
                key_hash,
                layer: 144,
            },
        })
    }

    #[test]
    fn update_secret_chat_folds_a_pending_chat() {
        let mut store = SecretChatStore::new();
        store.reduce(&secret_chat(5, 7, TdSecretChatState::Pending, true));

        let chat = store.get(5).unwrap();
        assert_eq!(chat.id, 5);
        assert_eq!(chat.user_id, 7);
        assert_eq!(chat.state, SecretChatState::Pending);
        assert!(chat.is_outbound);
        assert!(chat.key_hash.is_empty());
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn state_advances_pending_then_ready_then_closed_in_place() {
        let mut store = SecretChatStore::new();
        store.reduce(&secret_chat(5, 7, TdSecretChatState::Pending, true));
        assert_eq!(store.get(5).unwrap().state, SecretChatState::Pending);

        // Ready brings the verification key hash with it.
        store.reduce(&secret_chat(5, 7, TdSecretChatState::Ready, true));
        let ready = store.get(5).unwrap();
        assert_eq!(ready.state, SecretChatState::Ready);
        assert_eq!(ready.key_hash, "fingerprint");

        // Closing advances it again; still one record, replaced in place.
        store.reduce(&secret_chat(5, 7, TdSecretChatState::Closed, true));
        assert_eq!(store.get(5).unwrap().state, SecretChatState::Closed);
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn repeated_update_is_idempotent() {
        let mut store = SecretChatStore::new();
        store.reduce(&secret_chat(5, 7, TdSecretChatState::Ready, false));
        store.reduce(&secret_chat(5, 7, TdSecretChatState::Ready, false));

        assert_eq!(store.len(), 1);
        assert_eq!(store.get(5).unwrap().state, SecretChatState::Ready);
        // is_outbound is preserved from the record (an accepted, inbound chat).
        assert!(!store.get(5).unwrap().is_outbound);
    }

    #[test]
    fn distinct_secret_chats_coexist() {
        let mut store = SecretChatStore::new();
        store.reduce(&secret_chat(5, 7, TdSecretChatState::Ready, true));
        store.reduce(&secret_chat(6, 8, TdSecretChatState::Pending, false));

        assert_eq!(store.len(), 2);
        assert_eq!(store.get(5).unwrap().user_id, 7);
        assert_eq!(store.get(6).unwrap().state, SecretChatState::Pending);
        // An unknown id resolves to nothing rather than panicking.
        assert!(store.get(404).is_none());
    }

    #[test]
    fn non_secret_chat_updates_are_ignored_by_the_reducer() {
        let mut store = SecretChatStore::new();
        store.reduce(&secret_chat(5, 7, TdSecretChatState::Ready, true));
        // A message-route update reaching this reducer (shouldn't happen, but the
        // catch-all must be inert) leaves the store untouched.
        store.reduce(&Update::DeleteMessages(UpdateDeleteMessages {
            chat_id: 10,
            message_ids: vec![1],
            is_permanent: true,
            from_cache: false,
        }));
        assert_eq!(store.len(), 1);
    }

    /// A spy `SecretChatRequests` recording each lifecycle call, so a test asserts
    /// the seam is driven without a live `tdjson`.
    #[derive(Default)]
    struct SecretChatSpy {
        created_with: RefCell<Vec<i64>>,
        closed: RefCell<Vec<i32>>,
    }

    impl SecretChatRequests for SecretChatSpy {
        async fn create_new_secret_chat(&self, user_id: i64) -> Result<Chat, TdError> {
            self.created_with.borrow_mut().push(user_id);
            Ok(stub_secret_chat(-user_id, user_id))
        }

        async fn close_secret_chat(&self, secret_chat_id: i32) -> Result<(), TdError> {
            self.closed.borrow_mut().push(secret_chat_id);
            Ok(())
        }
    }

    #[tokio::test]
    async fn seam_creates_and_closes_secret_chats() {
        let spy = SecretChatSpy::default();
        let chat = spy.create_new_secret_chat(7).await.unwrap();
        assert_eq!(chat.id, -7);
        spy.close_secret_chat(5).await.unwrap();

        assert_eq!(*spy.created_with.borrow(), vec![7]);
        assert_eq!(*spy.closed.borrow(), vec![5]);
    }
}
