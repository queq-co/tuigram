//! The single update router — tuigram's one always-on subscriber of the bridge.
//!
//! `TDLib` pushes a firehose of unsolicited updates. Rather than let each
//! subsystem subscribe and clone the whole stream, Phase 3 routes everything
//! through **one** long-lived task ([`Router::run`]): it drains the bridge's
//! lagged-aware stream once, classifies each update with a single match, and
//! dispatches it O(1) to the owning domain's reducer behind the [`UpdateSink`]
//! seam.
//!
//! The router holds **no business logic**. `classify` only tags an update with
//! the domain that owns it; the actual fold (which field changes, how state is
//! ordered) lives in the domain reducer the tag points at. That keeps the
//! reducers independently unit-testable — a domain test drives its reducer with
//! synthetic updates directly, never through the router — and keeps this file
//! from accreting per-domain knowledge as Phase 3 grows.

use tdlib_rs::enums::Update;
use tokio_stream::{Stream, StreamExt};

use crate::bridge::RouterEvent;

/// Where the router folds account-content updates. The chat-list reducer (#17)
/// and the per-chat message reducer (#18) compose into the real sink (the
/// `Client`'s account state); tests use a spy.
///
/// The router calls **exactly one** method per event — one of the `reduce_*`
/// for an update it owns, or [`resync_after_lag`](Self::resync_after_lag) for a
/// dropped-update gap — so implementors hold only fold logic and never repeat
/// the router's classification.
pub trait UpdateSink {
    /// Fold a chat-list update (new chat, position/order, last message, read
    /// state, draft, folder list) into the chat snapshot.
    fn reduce_chat(&mut self, update: &Update);

    /// Fold a message update (new message, send-lifecycle, content edit,
    /// deletion) into the per-chat message store.
    fn reduce_message(&mut self, update: &Update);

    /// Fold a user update (new/changed user record, presence change) into the
    /// users store, so senders and private chats resolve to names.
    fn reduce_user(&mut self, update: &Update);

    /// Fold a file update (`updateFile`: download/upload progress, local path)
    /// into the files store, so media content resolves to transferable bytes.
    fn reduce_file(&mut self, update: &Update);

    /// Fold a chat-action update (`updateChatAction`: a sender started or
    /// cancelled an activity) into the transient typing view. Advisory state,
    /// never persisted into the message store.
    fn reduce_action(&mut self, update: &Update);

    /// Fold a secret-chat update (`updateSecretChat`: lifecycle/key state of an
    /// end-to-end encrypted chat) into the secret-chat store.
    fn reduce_secret_chat(&mut self, update: &Update);

    /// Fold a connection-state update (`updateConnectionState`: the transport's
    /// link/sync status) into the connection store, for a "Connecting…/Updating…"
    /// indicator. Carries no account content, only transport liveness.
    fn reduce_connection(&mut self, update: &Update);

    /// Recover from a broadcast overflow: `skipped` updates were dropped before
    /// the router caught up, so the folded state may be stale and must be
    /// re-queried. Handling is mandatory — a lag is never silently ignored.
    fn resync_after_lag(&mut self, skipped: u64);
}

/// The domain that owns an update, as decided by [`classify`].
///
/// A pure routing tag: it carries no payload and no logic, only "who folds
/// this". Most updates (auth, connectivity, user/option metadata, …) are
/// [`Ignored`](Route::Ignored) by this layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    /// Folded by the chat-list reducer (#17).
    Chat,
    /// Folded by the per-chat message reducer (#18).
    Message,
    /// Folded by the users reducer (#35).
    User,
    /// Folded by the files reducer (#44).
    File,
    /// Folded by the chat-action reducer (#52) — the transient typing view.
    Action,
    /// Folded by the secret-chat reducer (#53) — the E2E chat lifecycle.
    SecretChat,
    /// Folded by the connection reducer (#99) — the transport's sync status.
    Connection,
    /// Not account content this router folds; dropped here.
    Ignored,
}

/// Tag an update with the domain that owns it.
///
/// This is deliberately a *routing* match, not a model projection, so a
/// catch-all `Ignored` arm is correct: the vast majority of `TDLib`'s update
/// variants are connectivity/metadata the router does not fold, and a new
/// variant defaulting to `Ignored` is safe (it is simply not routed). Contrast
/// `model::*::from_tdlib`, which is total *on purpose* so a new content variant
/// must be classified before it compiles.
fn classify(update: &Update) -> Route {
    match update {
        Update::NewChat(_)
        | Update::ChatPosition(_)
        | Update::ChatLastMessage(_)
        | Update::ChatReadInbox(_)
        | Update::ChatReadOutbox(_)
        | Update::ChatDraftMessage(_)
        | Update::ChatFolders(_)
        // A message's pinned state is chat state (#51): the chat store folds it
        // onto the chat's pinned-message set, not the per-message store.
        | Update::MessageIsPinned(_) => Route::Chat,
        Update::NewMessage(_)
        | Update::MessageSendSucceeded(_)
        | Update::MessageSendFailed(_)
        | Update::MessageContent(_)
        // A reaction change (#51) folds onto the message itself.
        | Update::MessageInteractionInfo(_)
        | Update::DeleteMessages(_) => Route::Message,
        Update::User(_) | Update::UserStatus(_) => Route::User,
        Update::File(_) => Route::File,
        // updateChatAction is transient typing/recording presence (#52): the
        // chat-action store folds it into a separate view, never into history.
        Update::ChatAction(_) => Route::Action,
        // updateSecretChat is the E2E chat lifecycle (#53), folded into the
        // secret-chat store keyed by secret_chat_id.
        Update::SecretChat(_) => Route::SecretChat,
        // updateConnectionState is the transport's link/sync status (#99),
        // folded into the connection store for a sync indicator.
        Update::ConnectionState(_) => Route::Connection,
        _ => Route::Ignored,
    }
}

/// The single update router: drains the bridge's lagged-aware stream and folds
/// each event into `S`.
///
/// Generic over the sink so it is exercised in tests with a spy and in
/// production with the `Client`'s shared account state — the drain/dispatch
/// plumbing is identical either way.
pub struct Router<S: UpdateSink> {
    sink: S,
}

impl<S: UpdateSink> Router<S> {
    /// Wrap a sink in a router.
    pub fn new(sink: S) -> Self {
        Self { sink }
    }

    /// Classify and dispatch one update. Synchronous and side-effect-free beyond
    /// the sink call, so the routing table is unit-testable without a stream,
    /// a runtime, or a live `tdjson`.
    pub fn apply(&mut self, update: &Update) {
        match classify(update) {
            Route::Chat => self.sink.reduce_chat(update),
            Route::Message => self.sink.reduce_message(update),
            Route::User => self.sink.reduce_user(update),
            Route::File => self.sink.reduce_file(update),
            Route::Action => self.sink.reduce_action(update),
            Route::SecretChat => self.sink.reduce_secret_chat(update),
            Route::Connection => self.sink.reduce_connection(update),
            Route::Ignored => {}
        }
    }

    /// Drain the event stream to completion, folding each event into the sink.
    ///
    /// This is the always-on router task: it returns only when the stream closes
    /// (the bridge was dropped). A [`RouterEvent::Lagged`] is routed to
    /// [`UpdateSink::resync_after_lag`], never dropped. Returns the sink so a
    /// test can inspect the folded result after the stream ends.
    pub async fn run(mut self, events: impl Stream<Item = RouterEvent>) -> S {
        let mut events = std::pin::pin!(events);
        while let Some(event) = events.next().await {
            match event {
                RouterEvent::Update(update) => self.apply(&update),
                RouterEvent::Lagged(skipped) => self.sink.resync_after_lag(skipped),
            }
        }
        self.sink
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tdlib_rs::enums::AuthorizationState;
    use tdlib_rs::types::{
        UpdateAuthorizationState, UpdateChatLastMessage, UpdateChatReadInbox, UpdateDeleteMessages,
    };

    /// Records which reducer the router dispatched each update to, and any lag,
    /// so a test asserts routing without any real fold logic.
    #[derive(Default)]
    struct SpySink {
        chat: u32,
        message: u32,
        user: u32,
        file: u32,
        action: u32,
        secret_chat: u32,
        connection: u32,
        lagged: Vec<u64>,
    }

    impl UpdateSink for SpySink {
        fn reduce_chat(&mut self, _update: &Update) {
            self.chat += 1;
        }
        fn reduce_message(&mut self, _update: &Update) {
            self.message += 1;
        }
        fn reduce_user(&mut self, _update: &Update) {
            self.user += 1;
        }
        fn reduce_file(&mut self, _update: &Update) {
            self.file += 1;
        }
        fn reduce_action(&mut self, _update: &Update) {
            self.action += 1;
        }
        fn reduce_secret_chat(&mut self, _update: &Update) {
            self.secret_chat += 1;
        }
        fn reduce_connection(&mut self, _update: &Update) {
            self.connection += 1;
        }
        fn resync_after_lag(&mut self, skipped: u64) {
            self.lagged.push(skipped);
        }
    }

    // Representatives chosen for cheap construction (primitives / `None` only):
    // a full `Chat`/`Message` payload is irrelevant here since the router only
    // matches the variant, never its contents.
    fn chat_read_inbox() -> Update {
        Update::ChatReadInbox(UpdateChatReadInbox {
            chat_id: 1,
            last_read_inbox_message_id: 10,
            unread_count: 0,
        })
    }

    fn chat_last_message() -> Update {
        Update::ChatLastMessage(UpdateChatLastMessage {
            chat_id: 1,
            last_message: None,
            positions: Vec::new(),
        })
    }

    fn chat_draft_message() -> Update {
        Update::ChatDraftMessage(tdlib_rs::types::UpdateChatDraftMessage {
            chat_id: 1,
            draft_message: None,
            positions: Vec::new(),
        })
    }

    fn chat_folders() -> Update {
        Update::ChatFolders(tdlib_rs::types::UpdateChatFolders {
            chat_folders: Vec::new(),
            main_chat_list_position: 0,
            are_tags_enabled: false,
        })
    }

    fn delete_messages() -> Update {
        Update::DeleteMessages(UpdateDeleteMessages {
            chat_id: 1,
            message_ids: vec![1],
            is_permanent: true,
            from_cache: false,
        })
    }

    fn message_interaction_info() -> Update {
        Update::MessageInteractionInfo(tdlib_rs::types::UpdateMessageInteractionInfo {
            chat_id: 1,
            message_id: 2,
            interaction_info: None,
        })
    }

    fn message_is_pinned() -> Update {
        Update::MessageIsPinned(tdlib_rs::types::UpdateMessageIsPinned {
            chat_id: 1,
            message_id: 2,
            is_pinned: true,
        })
    }

    fn user_status() -> Update {
        Update::UserStatus(tdlib_rs::types::UpdateUserStatus {
            user_id: 7,
            status: tdlib_rs::enums::UserStatus::Recently(
                tdlib_rs::types::UserStatusRecently::default(),
            ),
        })
    }

    fn file_update() -> Update {
        Update::File(tdlib_rs::types::UpdateFile {
            file: tdlib_rs::types::File {
                id: 7,
                ..Default::default()
            },
        })
    }

    fn chat_action() -> Update {
        Update::ChatAction(tdlib_rs::types::UpdateChatAction {
            chat_id: 1,
            topic_id: None,
            sender_id: tdlib_rs::enums::MessageSender::User(tdlib_rs::types::MessageSenderUser {
                user_id: 7,
            }),
            action: tdlib_rs::enums::ChatAction::Typing,
        })
    }

    fn secret_chat() -> Update {
        Update::SecretChat(tdlib_rs::types::UpdateSecretChat {
            secret_chat: tdlib_rs::types::SecretChat {
                id: 5,
                user_id: 7,
                state: tdlib_rs::enums::SecretChatState::Pending,
                is_outbound: true,
                key_hash: String::new(),
                layer: 144,
            },
        })
    }

    fn connection_state() -> Update {
        Update::ConnectionState(tdlib_rs::types::UpdateConnectionState {
            state: tdlib_rs::enums::ConnectionState::Updating,
        })
    }

    /// An update the router does not fold, to prove the `Ignored` arm dispatches
    /// to neither reducer.
    fn unrelated() -> Update {
        Update::AuthorizationState(UpdateAuthorizationState {
            authorization_state: AuthorizationState::Ready,
        })
    }

    #[test]
    fn chat_updates_route_to_the_chat_reducer() {
        let mut router = Router::new(SpySink::default());
        router.apply(&chat_read_inbox());
        router.apply(&chat_last_message());
        router.apply(&chat_folders());
        let sink = router.sink;
        assert_eq!(sink.chat, 3);
        assert_eq!(sink.message, 0);
    }

    #[test]
    fn draft_updates_route_to_the_chat_reducer_not_the_message_store() {
        // A synced compose draft is chat state: it must reach the chat reducer
        // and never the message store, so it is never confused with a sent
        // message (#38).
        let mut router = Router::new(SpySink::default());
        router.apply(&chat_draft_message());
        let sink = router.sink;
        assert_eq!(sink.chat, 1);
        assert_eq!(sink.message, 0);
    }

    #[test]
    fn message_updates_route_to_the_message_reducer() {
        let mut router = Router::new(SpySink::default());
        router.apply(&delete_messages());
        let sink = router.sink;
        assert_eq!(sink.message, 1);
        assert_eq!(sink.chat, 0);
    }

    #[test]
    fn reaction_updates_route_to_the_message_reducer() {
        // updateMessageInteractionInfo folds onto the message (its reactions).
        let mut router = Router::new(SpySink::default());
        router.apply(&message_interaction_info());
        let sink = router.sink;
        assert_eq!(sink.message, 1);
        assert_eq!(sink.chat, 0);
    }

    #[test]
    fn message_pin_updates_route_to_the_chat_reducer() {
        // updateMessageIsPinned is chat state (the chat's pinned-message set), so
        // it routes to the chat reducer, not the message store.
        let mut router = Router::new(SpySink::default());
        router.apply(&message_is_pinned());
        let sink = router.sink;
        assert_eq!(sink.chat, 1);
        assert_eq!(sink.message, 0);
    }

    #[test]
    fn user_updates_route_to_the_user_reducer() {
        let mut router = Router::new(SpySink::default());
        router.apply(&user_status());
        let sink = router.sink;
        assert_eq!(sink.user, 1);
        assert_eq!(sink.chat, 0);
        assert_eq!(sink.message, 0);
    }

    #[test]
    fn file_updates_route_to_the_file_reducer() {
        let mut router = Router::new(SpySink::default());
        router.apply(&file_update());
        let sink = router.sink;
        assert_eq!(sink.file, 1);
        assert_eq!(sink.chat, 0);
        assert_eq!(sink.message, 0);
        assert_eq!(sink.user, 0);
    }

    #[test]
    fn chat_action_updates_route_to_the_action_reducer() {
        // updateChatAction is transient typing presence, folded into its own view.
        let mut router = Router::new(SpySink::default());
        router.apply(&chat_action());
        let sink = router.sink;
        assert_eq!(sink.action, 1);
        assert_eq!(sink.chat, 0);
        assert_eq!(sink.message, 0);
        assert_eq!(sink.user, 0);
    }

    #[test]
    fn secret_chat_updates_route_to_the_secret_chat_reducer() {
        // updateSecretChat is the E2E chat lifecycle, folded into its own store.
        let mut router = Router::new(SpySink::default());
        router.apply(&secret_chat());
        let sink = router.sink;
        assert_eq!(sink.secret_chat, 1);
        assert_eq!(sink.chat, 0);
        assert_eq!(sink.message, 0);
    }

    #[test]
    fn connection_updates_route_to_the_connection_reducer() {
        // updateConnectionState is transport sync status, folded into its own store.
        let mut router = Router::new(SpySink::default());
        router.apply(&connection_state());
        let sink = router.sink;
        assert_eq!(sink.connection, 1);
        assert_eq!(sink.chat, 0);
        assert_eq!(sink.message, 0);
    }

    #[test]
    fn unrelated_updates_route_to_no_reducer() {
        let mut router = Router::new(SpySink::default());
        router.apply(&unrelated());
        let sink = router.sink;
        assert_eq!(sink.chat, 0);
        assert_eq!(sink.message, 0);
        assert_eq!(sink.user, 0);
        assert_eq!(sink.file, 0);
        assert_eq!(sink.action, 0);
        assert_eq!(sink.secret_chat, 0);
        assert_eq!(sink.connection, 0);
    }

    #[tokio::test]
    async fn run_drains_a_mixed_stream_and_dispatches_each_event() {
        let events = tokio_stream::iter(vec![
            RouterEvent::Update(chat_read_inbox()),
            RouterEvent::Update(delete_messages()),
            RouterEvent::Lagged(7),
            RouterEvent::Update(chat_last_message()),
        ]);

        let sink = Box::pin(Router::new(SpySink::default()).run(events)).await;

        assert_eq!(sink.chat, 2);
        assert_eq!(sink.message, 1);
        // Lag is handled, not swallowed: the exact skip count reaches the sink.
        assert_eq!(sink.lagged, vec![7]);
    }
}
