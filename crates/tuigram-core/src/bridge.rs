//! Async bridge between TDLib (`tdjson`) and tuigram's logic.
//!
//! `tdjson` is driven by two C calls: a non-blocking `td_send` and a *blocking*
//! `td_receive` poll. `tdlib-rs` already layers the request/response half on top
//! of that — every `tdlib_rs::functions::*` is an `async fn` that tags the
//! request with an `@extra` id, registers a `oneshot` in a global observer, and
//! awaits it. The one thing it does **not** provide is the loop that actually
//! pumps `receive()`: without it, the observer is never notified and those
//! futures hang forever, and no unsolicited updates ever surface.
//!
//! That loop is this module. [`Bridge`] owns a dedicated OS thread that calls
//! `tdlib_rs::receive()` forever. Each call does double duty:
//!
//! * responses (carrying `@extra`) are routed by `receive()` straight into the
//!   global observer, completing the `oneshot` a `functions::*` call is awaiting
//!   — so the **request API is just the typed `functions`**, re-exported from
//!   the crate root and called with [`Bridge::id`];
//! * unsolicited updates are returned to us and fanned out over a tokio
//!   `broadcast` channel, exposed as the [`UpdateStream`].
//!
//! The [`TgClient`] trait is the seam logic depends on: real code uses
//! [`Bridge`], tests use a mock, so the auth state machine (#6) and everything
//! above it are unit-testable without a network or even a live `tdjson`.
//!
//! **Invariant:** at most one [`Bridge`] per process. `tdjson`'s receive queue
//! and observer are process-global, so two concurrent receive loops would steal
//! each other's updates. tuigram is a single-account client, so this holds.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use tdlib_rs::enums::{AuthorizationState, Update};
use tdlib_rs::types::Error as TdError;
use tokio::sync::broadcast;
use tokio_stream::Stream;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

/// Buffer of recent updates the broadcast channel retains per subscriber. A
/// slow consumer that falls this far behind starts losing the oldest updates
/// (surfaced as a lagged event the stream skips); generous because TDLib bursts
/// updates on startup and resync.
const UPDATE_BUFFER: usize = 1024;

/// The async seam between TDLib and tuigram's logic.
///
/// Implemented by [`Bridge`] over a live `tdjson` client, and by mocks in tests.
/// Methods mirror the `tdlib_rs::functions` we drive through the bridge; the set
/// grows as higher layers (auth, chats) need more requests.
// Internal seam: every consumer is in-crate and generic over `C: TgClient`, so
// the lack of a caller-controllable `Send` bound (the reason this lint fires)
// is not a concern here.
#[allow(async_fn_in_trait)]
pub trait TgClient {
    /// Subscribe to the stream of unsolicited updates pushed by TDLib.
    ///
    /// Each call yields an independent subscription; updates emitted before a
    /// subscription is created are not replayed to it.
    fn updates(&self) -> UpdateStream;

    /// Fetch the current authorization state.
    ///
    /// The canonical proof that request/response correlation works: TDLib
    /// answers it immediately from local state with no network, so a successful
    /// round-trip means the receive loop is correctly notifying the observer.
    async fn authorization_state(&self) -> Result<AuthorizationState, TdError>;
}

/// A live connection to a single `tdjson` client and its update pump.
///
/// Created with [`Bridge::new`], which spins up the receive thread immediately.
/// Dropping the bridge signals that thread to stop and joins it.
pub struct Bridge {
    client_id: i32,
    updates_tx: broadcast::Sender<Update>,
    shutdown: Arc<AtomicBool>,
    receive_thread: Option<JoinHandle<()>>,
}

impl Bridge {
    /// Create a fresh `tdjson` client and start pumping its updates.
    #[must_use]
    pub fn new() -> Self {
        Self::with_client(tdlib_rs::create_client())
    }

    /// Start the bridge over an already-created `tdjson` client id.
    #[must_use]
    pub fn with_client(client_id: i32) -> Self {
        let (updates_tx, _) = broadcast::channel(UPDATE_BUFFER);
        let shutdown = Arc::new(AtomicBool::new(false));
        let receive_thread = spawn_receive_loop(client_id, updates_tx.clone(), shutdown.clone());
        Self {
            client_id,
            updates_tx,
            shutdown,
            receive_thread: Some(receive_thread),
        }
    }

    /// The `tdjson` client id, for use with the `tdlib_rs::functions` API.
    #[must_use]
    pub fn id(&self) -> i32 {
        self.client_id
    }
}

impl Default for Bridge {
    fn default() -> Self {
        Self::new()
    }
}

impl TgClient for Bridge {
    fn updates(&self) -> UpdateStream {
        UpdateStream(BroadcastStream::new(self.updates_tx.subscribe()))
    }

    async fn authorization_state(&self) -> Result<AuthorizationState, TdError> {
        tdlib_rs::functions::get_authorization_state(self.client_id).await
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        // The loop checks this flag each time `receive()` returns, which it does
        // at least every poll-timeout (~2s), so the join blocks at most that long.
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(handle) = self.receive_thread.take() {
            let _ = handle.join();
        }
    }
}

/// A [`Stream`] of unsolicited TDLib [`Update`]s for one subscriber.
///
/// Lagged events (the consumer fell more than [`UPDATE_BUFFER`] behind) are
/// skipped rather than surfaced as errors: the stream's contract is "the next
/// update", and a stalled consumer is a higher-layer concern.
pub struct UpdateStream(BroadcastStream<Update>);

impl Stream for UpdateStream {
    type Item = Update;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use std::task::Poll;
        loop {
            return match std::pin::Pin::new(&mut self.0).poll_next(cx) {
                Poll::Ready(Some(Ok(update))) => Poll::Ready(Some(update)),
                // Dropped some updates because this subscriber lagged; skip the
                // marker and keep delivering the freshest ones.
                Poll::Ready(Some(Err(BroadcastStreamRecvError::Lagged(_)))) => continue,
                Poll::Ready(None) => Poll::Ready(None),
                Poll::Pending => Poll::Pending,
            };
        }
    }
}

/// Spawn the dedicated blocking thread that pumps `tdjson` for this client.
///
/// `receive()` is shared across all `tdjson` clients in the process, so it may
/// hand us updates for other clients; we forward only this bridge's. (See the
/// single-bridge invariant in the module docs.) Sending into the broadcast
/// channel is best-effort: with no current subscribers it returns an error we
/// intentionally ignore.
fn spawn_receive_loop(
    client_id: i32,
    updates_tx: broadcast::Sender<Update>,
    shutdown: Arc<AtomicBool>,
) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("tdlib-receive".to_owned())
        .spawn(move || {
            while !shutdown.load(Ordering::Relaxed) {
                if let Some((update, source_client_id)) = tdlib_rs::receive()
                    && source_client_id == client_id
                {
                    let _ = updates_tx.send(update);
                }
            }
        })
        .expect("spawn tdlib-receive thread")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tdlib_rs::types::UpdateAuthorizationState;
    use tokio::sync::broadcast;
    use tokio_stream::StreamExt;

    /// Generous ceiling for the live `tdjson` round-trips: the operations are
    /// offline, so anything slower than this is a hang, not latency.
    const LIVE_TIMEOUT: Duration = Duration::from_secs(10);

    /// A network-free [`TgClient`] for unit-testing logic over the seam: it
    /// answers `authorization_state` with a scripted value and lets a test push
    /// arbitrary updates into its stream.
    struct MockClient {
        state: AuthorizationState,
        updates_tx: broadcast::Sender<Update>,
    }

    impl MockClient {
        fn new(state: AuthorizationState) -> Self {
            let (updates_tx, _) = broadcast::channel(16);
            Self { state, updates_tx }
        }

        /// Push an update to every current subscriber.
        fn emit(&self, update: Update) {
            let _ = self.updates_tx.send(update);
        }
    }

    impl TgClient for MockClient {
        fn updates(&self) -> UpdateStream {
            UpdateStream(BroadcastStream::new(self.updates_tx.subscribe()))
        }

        async fn authorization_state(&self) -> Result<AuthorizationState, TdError> {
            Ok(self.state.clone())
        }
    }

    /// Logic written against the seam, exercised below with the mock and usable
    /// unchanged against a real [`Bridge`].
    async fn awaiting_login<C: TgClient>(client: &C) -> bool {
        !matches!(
            client.authorization_state().await,
            Ok(AuthorizationState::Ready)
        )
    }

    #[tokio::test]
    async fn seam_request_is_unit_testable_without_tdjson() {
        let client = MockClient::new(AuthorizationState::WaitPhoneNumber);
        assert!(awaiting_login(&client).await);

        let ready = MockClient::new(AuthorizationState::Ready);
        assert!(!awaiting_login(&ready).await);
    }

    #[tokio::test]
    async fn seam_streams_updates_without_tdjson() {
        let client = MockClient::new(AuthorizationState::WaitTdlibParameters);
        let mut updates = client.updates();

        client.emit(Update::AuthorizationState(UpdateAuthorizationState {
            authorization_state: AuthorizationState::WaitPhoneNumber,
        }));

        let update = updates.next().await;
        assert!(matches!(
            update,
            Some(Update::AuthorizationState(UpdateAuthorizationState {
                authorization_state: AuthorizationState::WaitPhoneNumber,
            }))
        ));
    }

    /// End-to-end against a real `tdjson`, fully offline: a fresh client reports
    /// `WaitTdlibParameters` and, once poked, bursts unsolicited updates. Proves
    /// (1) a request correlates to its response via the receive loop + observer,
    /// and (2) updates fan out to the stream. Kept as a single test so only one
    /// receive loop runs (see the single-bridge invariant).
    #[tokio::test]
    async fn live_request_correlates_and_updates_stream() {
        let bridge = Bridge::new();
        // Subscribe before the request so the startup update burst it triggers
        // is captured (the broadcast does not replay pre-subscription updates).
        let mut updates = bridge.updates();

        let state = tokio::time::timeout(LIVE_TIMEOUT, bridge.authorization_state())
            .await
            .expect("getAuthorizationState timed out — receive loop not draining responses")
            .expect("getAuthorizationState returned an error");
        assert_eq!(state, AuthorizationState::WaitTdlibParameters);

        let update = tokio::time::timeout(LIVE_TIMEOUT, updates.next())
            .await
            .expect("no unsolicited update streamed — receive loop not forwarding updates");
        assert!(update.is_some(), "update stream closed unexpectedly");
    }
}
