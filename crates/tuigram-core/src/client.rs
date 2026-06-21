//! The `Client` facade — the long-lived owner of account state and the router.
//!
//! A live tuigram session is: a [`Bridge`] (the `tdjson` transport), the account
//! content folded from its updates ([`AccountState`]), and the single
//! [`Router`](crate::router::Router) task that does the folding. This module ties
//! those together so the rest of the app holds one handle instead of wiring the
//! broadcast stream, the router task, and the shared state by hand.
//!
//! **Lifecycle.** A session is assembled in order: open secure storage
//! ([`SessionStorage`](crate::session::SessionStorage)), run login to `Ready`
//! ([`Login`](crate::auth::Login)) over the bridge, then hand the authenticated
//! bridge to [`Client::start`], which spawns the router. Login is interactive, so
//! that step stays with its caller (the harness today, the TUI later); the facade
//! takes over once the account is authenticated.
//!
//! **Scope (#16).** This is the keystone the chat (#17) and message (#18) domains
//! plug into: [`AccountState`] is the composition root and [`Client::read`] the
//! read seam, but both are empty until those domains add their stores and the
//! router's [`reduce_chat`](crate::router::UpdateSink::reduce_chat) /
//! [`reduce_message`](crate::router::UpdateSink::reduce_message) arms land with
//! them. Write actions (send/edit/delete/mark-read, #19–#21) are driven over the
//! bridge's per-domain request traits ([`Client::bridge`]) and reconcile through
//! the router; the one fetch that returns directly rather than as updates —
//! history paging — folds in via [`Client::merge_history`].
//!
//! **Search & forward (#50).** Two of the message domain's request paths get a
//! facade helper rather than going through `bridge()` directly. Search
//! ([`Client::search_chat`], [`Client::search_messages`]) returns its hits — like
//! history — directly rather than as updates, but must **never** fold into the
//! account state, so the facade pages them into a transient
//! [`SearchResults`](crate::messages::SearchResults) it returns instead of through
//! `merge_history`. Forwarding ([`Client::forward_messages`]) is a write whose
//! results *do* reconcile through the router (as `updateNewMessage`), exposed here
//! for symmetry; its returned copies are the optimistic entries, not a fold.

use std::sync::{Arc, Mutex};

use tdlib_rs::enums::Update;
use tdlib_rs::types::Error as TdError;
use tokio::task::JoinHandle;

use crate::actions::ChatActionStore;
use crate::bridge::Bridge;
use crate::chats::ChatStore;
use crate::files::FileStore;
use crate::messages::{ForwardRequests, MessageStore, SearchResults};
use crate::model::{Message, Sender};
use crate::router::{Router, UpdateSink};
use crate::secret_chats::SecretChatStore;
use crate::users::UserStore;

/// The account content the router folds updates into: the chat list (#17) and
/// per-chat messages (#18).
///
/// Each domain adds its store and the matching reduce arm as it lands, so this
/// type grows in one place rather than the router accreting state.
#[derive(Default)]
pub struct AccountState {
    chats: ChatStore,
    messages: MessageStore,
    users: UserStore,
    files: FileStore,
    actions: ChatActionStore,
    secret_chats: SecretChatStore,
}

impl AccountState {
    /// The folded chat-list store, for the facade's read side (e.g.
    /// `client.read(|s| s.chats().main_list())`).
    #[must_use]
    pub fn chats(&self) -> &ChatStore {
        &self.chats
    }

    /// The folded per-chat message store, for the facade's read side (e.g.
    /// `client.read(|s| s.messages().history(chat_id))`).
    #[must_use]
    pub fn messages(&self) -> &MessageStore {
        &self.messages
    }

    /// The folded users store, for the facade's read side — the join that turns
    /// the bare ids on senders and private chats into names (e.g.
    /// `client.read(|s| s.users().display_name(user_id))`).
    #[must_use]
    pub fn users(&self) -> &UserStore {
        &self.users
    }

    /// The folded files store, for the facade's read side — the transfer state
    /// behind a media `FileRef` (e.g.
    /// `client.read(|s| s.files().get(file_ref.id).is_some_and(File::is_present))`).
    #[must_use]
    pub fn files(&self) -> &FileStore {
        &self.files
    }

    /// The transient chat-action view, for the facade's read side — who is
    /// currently typing/recording/uploading in a chat (e.g.
    /// `client.read(|s| s.actions().actors(chat_id))`). Never history.
    #[must_use]
    pub fn actions(&self) -> &ChatActionStore {
        &self.actions
    }

    /// The secret-chat store, for the facade's read side — the encryption state
    /// behind a [`ChatKind::Secret`](crate::model::ChatKind::Secret) chat (e.g.
    /// `client.read(|s| s.secret_chats().get(secret_chat_id))`).
    #[must_use]
    pub fn secret_chats(&self) -> &SecretChatStore {
        &self.secret_chats
    }

    /// Fold a chat-list update into the chat store.
    fn reduce_chat(&mut self, update: &Update) {
        self.chats.reduce(update);
    }

    /// Fold a message update into the message store.
    fn reduce_message(&mut self, update: &Update) {
        self.messages.reduce(update);
    }

    /// Fold a user update into the users store.
    fn reduce_user(&mut self, update: &Update) {
        self.users.reduce(update);
    }

    /// Fold a file update into the files store.
    fn reduce_file(&mut self, update: &Update) {
        self.files.reduce(update);
    }

    /// Fold a chat-action update into the transient typing view.
    fn reduce_action(&mut self, update: &Update) {
        self.actions.reduce(update);
    }

    /// Fold a secret-chat update into the secret-chat store.
    fn reduce_secret_chat(&mut self, update: &Update) {
        self.secret_chats.reduce(update);
    }

    /// Merge a fetched history page into the message store. Unlike live messages,
    /// `getChatHistory` returns its page in the response rather than as updates,
    /// so the fetcher folds it in here — alongside the live messages the router
    /// folds — through the same deduping [`MessageStore::merge`].
    fn merge_history(&mut self, page: Vec<Message>) {
        self.messages.merge(page);
    }

    /// Recover from a dropped-update gap by re-querying the affected state.
    /// The re-query requests belong to the chat/message domains (#17/#18); until
    /// then this records nothing and simply does not pretend the gap didn't
    /// happen — the router still surfaces every lag here rather than swallowing
    /// it upstream.
    fn resync_after_lag(&mut self, _skipped: u64) {}
}

/// Thread-safe handle to the account state, shared between the router task
/// (which folds updates into it) and the facade (which reads snapshots from it).
type SharedState = Arc<Mutex<AccountState>>;

/// The shared handle is the production [`UpdateSink`]: each routed update is
/// folded under the lock the facade reads through, so the router task and the
/// reader never see a torn state.
impl UpdateSink for SharedState {
    fn reduce_chat(&mut self, update: &Update) {
        self.lock()
            .expect("account state mutex poisoned")
            .reduce_chat(update);
    }

    fn reduce_message(&mut self, update: &Update) {
        self.lock()
            .expect("account state mutex poisoned")
            .reduce_message(update);
    }

    fn reduce_user(&mut self, update: &Update) {
        self.lock()
            .expect("account state mutex poisoned")
            .reduce_user(update);
    }

    fn reduce_file(&mut self, update: &Update) {
        self.lock()
            .expect("account state mutex poisoned")
            .reduce_file(update);
    }

    fn reduce_action(&mut self, update: &Update) {
        self.lock()
            .expect("account state mutex poisoned")
            .reduce_action(update);
    }

    fn reduce_secret_chat(&mut self, update: &Update) {
        self.lock()
            .expect("account state mutex poisoned")
            .reduce_secret_chat(update);
    }

    fn resync_after_lag(&mut self, skipped: u64) {
        self.lock()
            .expect("account state mutex poisoned")
            .resync_after_lag(skipped);
    }
}

/// A live, authenticated tuigram session: the bridge, the folded account state,
/// and the router task that keeps the latter current.
pub struct Client {
    bridge: Bridge,
    state: SharedState,
    router: JoinHandle<()>,
}

impl Client {
    /// Start the update router over an already-authenticated `bridge`.
    ///
    /// Subscribes to the bridge's lagged-aware stream and spawns the single
    /// always-on router task, which folds every update into the shared
    /// [`AccountState`] until the bridge is dropped. Must be called from within
    /// a Tokio runtime (it spawns a task).
    #[must_use]
    pub fn start(bridge: Bridge) -> Self {
        let state: SharedState = Arc::new(Mutex::new(AccountState::default()));
        let events = bridge.router_events();
        let router_sink = Arc::clone(&state);
        let router = tokio::spawn(async move {
            Router::new(router_sink).run(events).await;
        });
        Self {
            bridge,
            state,
            router,
        }
    }

    /// The bridge, for drivers that issue requests over it (login already ran;
    /// the chat/message request traits in #17–#21 are driven through this).
    #[must_use]
    pub fn bridge(&self) -> &Bridge {
        &self.bridge
    }

    /// Read the current account state under the shared lock.
    ///
    /// The router folds updates into the same state on its own task; this is the
    /// facade's read side. The domain snapshot accessors (the chat list in #17,
    /// a chat's messages in #18) are thin wrappers over this.
    pub fn read<R>(&self, reader: impl FnOnce(&AccountState) -> R) -> R {
        reader(&self.state.lock().expect("account state mutex poisoned"))
    }

    /// Fold a fetched history page into the account's message store, under the
    /// same lock the router folds through and [`read`](Self::read) reads from.
    ///
    /// `getChatHistory` (driven via
    /// [`load_history`](crate::messages::load_history) or a single
    /// [`get_chat_history`](crate::messages::HistoryRequests::get_chat_history)
    /// call) returns each page in the response, not as updates — so a caller
    /// paging a chat's history hands each page here to merge it into the store the
    /// facade reads back. This is the "production fold" `load_history` leaves to
    /// its caller; merging is deduped, so an overlapping re-page is idempotent.
    pub fn merge_history(&self, page: Vec<Message>) {
        self.state
            .lock()
            .expect("account state mutex poisoned")
            .merge_history(page);
    }

    /// Search one chat for `query`, paging to exhaustion into a transient
    /// [`SearchResults`] (`page` hits per request).
    ///
    /// Unlike history, search results are a **read-only view that never folds
    /// into the account state**: this returns them in a [`SearchResults`] of its
    /// own rather than through [`merge_history`](Self::merge_history), so a search
    /// leaves the live [`MessageStore`] — and what [`read`](Self::read) sees —
    /// untouched. `sender` optionally restricts hits to one sender. The paging
    /// itself lives in [`search_chat`](crate::messages::search_chat); the facade
    /// only binds it to this session's bridge.
    pub async fn search_chat(
        &self,
        chat_id: i64,
        query: String,
        sender: Option<Sender>,
        page: i32,
    ) -> Result<SearchResults, TdError> {
        crate::messages::search_chat(&self.bridge, chat_id, query, sender, page).await
    }

    /// Search the whole account for `query`, paging to exhaustion into a transient
    /// [`SearchResults`] (`page` hits per request).
    ///
    /// The account-wide counterpart to [`search_chat`](Self::search_chat), with
    /// the same discipline: the hits are a transient view and never fold into the
    /// live [`MessageStore`]. Paging lives in
    /// [`search_global`](crate::messages::search_global).
    pub async fn search_messages(
        &self,
        query: String,
        page: i32,
    ) -> Result<SearchResults, TdError> {
        crate::messages::search_global(&self.bridge, query, page).await
    }

    /// Forward `message_ids` from `from_chat_id` into `to_chat_id`.
    ///
    /// `send_copy` forwards a fresh copy (no "forwarded from" attribution);
    /// `remove_caption` drops captions when copying. Unlike search, a forward is a
    /// **write that reconciles through the router**: TDLib streams each forwarded
    /// message into the target chat as `updateNewMessage`, which the router folds
    /// into the [`MessageStore`] on the same optimistic-send lifecycle as
    /// [`SendRequests::send_text`](crate::messages::SendRequests::send_text). The
    /// returned [`Message`]s are the caller's
    /// reference copies of those optimistic entries (temporary ids,
    /// [`SendState::Pending`](crate::model::SendState::Pending)), not a second
    /// insert. It returns as soon as TDLib accepts the request.
    pub async fn forward_messages(
        &self,
        from_chat_id: i64,
        message_ids: Vec<i64>,
        to_chat_id: i64,
        send_copy: bool,
        remove_caption: bool,
    ) -> Result<Vec<Message>, TdError> {
        self.bridge
            .forward_messages(
                from_chat_id,
                message_ids,
                to_chat_id,
                send_copy,
                remove_caption,
            )
            .await
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        // Dropping the bridge would close the stream and end the task on its own,
        // but abort makes teardown prompt and order-independent.
        self.router.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::RouterEvent;
    use tdlib_rs::types::UpdateChatReadInbox;

    /// The production sink type (`Arc<Mutex<AccountState>>`) drives the router
    /// end to end: it folds updates and handles a lag under the same lock the
    /// facade reads through, without panicking or deadlocking.
    #[tokio::test]
    async fn shared_state_sink_drives_the_router() {
        let state: SharedState = Arc::new(Mutex::new(AccountState::default()));
        let events = tokio_stream::iter([
            RouterEvent::Update(Update::ChatReadInbox(UpdateChatReadInbox {
                chat_id: 1,
                last_read_inbox_message_id: 1,
                unread_count: 0,
            })),
            RouterEvent::Lagged(3),
        ]);

        Router::new(Arc::clone(&state)).run(events).await;

        // Readable through the same lock the facade's `read` uses.
        let _guard = state.lock().expect("mutex usable after the router ran");
    }

    /// A fetched history page folds into the message store and is readable back,
    /// deduping an overlapping re-page (the property the facade's `merge_history`
    /// passes through to the store).
    #[test]
    fn merge_history_folds_a_page_into_the_readable_store() {
        use crate::model::{FormattedText, MessageContent, SendState, Sender};

        let msg = |id: i64| Message {
            id,
            chat_id: 10,
            sender: Sender::User(1),
            date: 0,
            edit_date: 0,
            is_outgoing: false,
            content: MessageContent::Text(FormattedText::default()),
            send_state: SendState::Sent,
            reactions: vec![],
        };

        let mut state = AccountState::default();
        state.merge_history(vec![msg(2), msg(1)]);
        // An overlapping page collapses onto the same ids, ordered chronologically.
        state.merge_history(vec![msg(2), msg(3)]);

        let ids: Vec<i64> = state.messages().history(10).iter().map(|m| m.id).collect();
        assert_eq!(ids, vec![1, 2, 3]);
    }
}
