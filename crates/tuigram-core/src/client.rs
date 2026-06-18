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
//! them. Write actions (send/edit/delete/mark-read) arrive with their request
//! traits in #19–#21.

use std::sync::{Arc, Mutex};

use tdlib_rs::enums::Update;
use tokio::task::JoinHandle;

use crate::bridge::Bridge;
use crate::chats::ChatStore;
use crate::messages::MessageStore;
use crate::router::{Router, UpdateSink};

/// The account content the router folds updates into: the chat list (#17) and
/// per-chat messages (#18).
///
/// Each domain adds its store and the matching reduce arm as it lands, so this
/// type grows in one place rather than the router accreting state.
#[derive(Default)]
pub struct AccountState {
    chats: ChatStore,
    messages: MessageStore,
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

    /// Fold a chat-list update into the chat store.
    fn reduce_chat(&mut self, update: &Update) {
        self.chats.reduce(update);
    }

    /// Fold a message update into the message store.
    fn reduce_message(&mut self, update: &Update) {
        self.messages.reduce(update);
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
}
