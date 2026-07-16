//! Async bridge between `TDLib` (`tdjson`) and tuigram's logic.
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

use tdlib_rs::enums::Update;
use tokio::sync::broadcast;
use tokio_stream::Stream;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

/// Buffer of recent updates the broadcast channel retains per subscriber. A
/// slow consumer that falls this far behind starts losing the oldest updates
/// (surfaced as a lagged event the stream skips); generous because `TDLib` bursts
/// updates on startup and resync.
const UPDATE_BUFFER: usize = 1024;

/// Parameters for `setTdlibParameters`, the answer to `WaitTdlibParameters`.
///
/// A plain data bundle: the `api_id`/`api_hash` come from the user's own
/// Telegram app registration (#7), and `database_directory` / `files_directory`
/// / `database_encryption_key` from secure session storage (#8). This struct is
/// the seam where those land; the bridge supplies the remaining, non-secret
/// initialization flags.
#[derive(Clone, Debug)]
pub struct ClientParameters {
    /// Telegram API id from <https://my.telegram.org>.
    pub api_id: i32,
    /// Telegram API hash from <https://my.telegram.org>.
    pub api_hash: String,
    /// Directory for the persistent database.
    pub database_directory: String,
    /// Directory for downloaded files (often the same as the database dir).
    pub files_directory: String,
    /// Key the on-disk database is encrypted with (#8). Moved straight into the
    /// request; never logged.
    pub database_encryption_key: String,
    /// IETF language tag of the user's OS language, e.g. `en`.
    pub system_language_code: String,
    /// Human-readable device model shown in the user's active-sessions list.
    pub device_model: String,
    /// tuigram's version string, shown alongside the device model.
    pub application_version: String,
    /// Use Telegram's test data center instead of production.
    pub use_test_dc: bool,
}

/// The update-subscription seam between `TDLib` and tuigram's logic.
///
/// This is the transport's *read* side: a source of the unsolicited updates
/// `tdjson` pushes. [`Bridge`] implements it over a live client; tests implement
/// it to feed synthetic updates. The single update router (#16) is its one
/// always-on consumer.
///
/// Requests (the *write* side) are **not** here. They are segregated into
/// per-domain capability traits owned by the module that drives them —
/// [`AuthRequests`](crate::auth::AuthRequests) for login, and `ChatRequests` /
/// `MessageRequests` for Phase 3 — each implemented for [`Bridge`] in its module
/// via the public [`Bridge::id`]. That keeps this file pure transport and lets a
/// driver (and its test double) depend on only the slice it uses.
// Internal seam: every consumer is in-crate and generic over `C: TgClient`, so
// the lack of a caller-controllable `Send` bound (the reason this lint fires)
// is not a concern here.
#[allow(async_fn_in_trait)]
pub trait TgClient {
    /// Subscribe to the stream of unsolicited updates pushed by `TDLib`.
    ///
    /// Each call yields an independent subscription; updates emitted before a
    /// subscription is created are not replayed to it.
    fn updates(&self) -> UpdateStream;
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

    /// Subscribe to a **lagged-aware** view of the update stream, for the single
    /// update router (#16).
    ///
    /// Unlike [`updates`](Self::updates) — which skips lag silently because its
    /// consumers only want "the next update" — this surfaces a
    /// [`RouterEvent::Lagged`] when the broadcast buffer overflows, so the
    /// router can resync instead of folding a stream with holes in it. The
    /// router is the channel's one always-on subscriber, so it is the layer that
    /// must notice and recover from falling behind.
    #[must_use]
    pub fn router_events(&self) -> RouterStream {
        RouterStream(BroadcastStream::new(self.updates_tx.subscribe()))
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

/// A [`Stream`] of unsolicited `TDLib` [`Update`]s for one subscriber.
///
/// Lagged events (the consumer fell more than `UPDATE_BUFFER` behind) are
/// skipped rather than surfaced as errors: the stream's contract is "the next
/// update", and a stalled consumer is a higher-layer concern.
pub struct UpdateStream(BroadcastStream<Update>);

impl UpdateStream {
    /// An already-closed update stream that yields nothing. For tests and mocks
    /// whose logic does not consume updates.
    #[must_use]
    pub fn empty() -> Self {
        let (sender, receiver) = broadcast::channel(1);
        drop(sender);
        Self(BroadcastStream::new(receiver))
    }
}

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

/// One item from the [`RouterStream`]: either an update to fold, or notice that
/// the broadcast buffer overflowed and updates were lost.
///
/// The router matches on this so that a lag is a first-class event it handles
/// (by resyncing), never a silent gap in the folded state.
// `Update` is intrinsically large, but it already moves unboxed through the
// broadcast channel and `UpdateStream` (whose item is a bare `Update`). This is
// a transient per-item wrapper, not bulk storage, so boxing it just to even out
// a two-variant event would add a per-update heap allocation for no real gain.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum RouterEvent {
    /// An unsolicited update to fold into account state.
    Update(Update),
    /// The subscriber fell more than `UPDATE_BUFFER` behind; this many updates
    /// were dropped before it caught up. State may now be stale.
    Lagged(u64),
}

/// A lagged-aware [`Stream`] of updates for the single update router.
///
/// The counterpart to [`UpdateStream`]: where that one skips lag, this one
/// surfaces it as [`RouterEvent::Lagged`] so the router can resync. Obtained
/// from [`Bridge::router_events`].
pub struct RouterStream(BroadcastStream<Update>);

impl Stream for RouterStream {
    type Item = RouterEvent;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use std::task::Poll;
        match std::pin::Pin::new(&mut self.0).poll_next(cx) {
            Poll::Ready(Some(Ok(update))) => Poll::Ready(Some(RouterEvent::Update(update))),
            Poll::Ready(Some(Err(BroadcastStreamRecvError::Lagged(skipped)))) => {
                Poll::Ready(Some(RouterEvent::Lagged(skipped)))
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
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
    // The live round-trip checks `authorization_state`, which the transport
    // exposes through the auth capability trait.
    use crate::auth::AuthRequests;
    use std::time::Duration;
    use tdlib_rs::enums::AuthorizationState;
    use tdlib_rs::types::UpdateAuthorizationState;
    use tokio::sync::broadcast;
    use tokio_stream::StreamExt;

    /// Generous ceiling for the live `tdjson` round-trips: the operations are
    /// offline, so anything slower than this is a hang, not latency.
    const LIVE_TIMEOUT: Duration = Duration::from_secs(10);

    /// A network-free [`TgClient`]: it lets a test push arbitrary updates into
    /// its stream, so the update seam (and, later, the router) can be exercised
    /// without a live `tdjson`. Requests live on the per-domain traits and are
    /// tested with their own doubles (e.g. the auth `SpyClient`).
    struct MockClient {
        updates_tx: broadcast::Sender<Update>,
    }

    impl MockClient {
        fn new() -> Self {
            let (updates_tx, _) = broadcast::channel(16);
            Self { updates_tx }
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
    }

    #[tokio::test]
    async fn seam_streams_updates_without_tdjson() {
        let client = MockClient::new();
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
        // #223: serialized against `prebuilt_tdjson_loads_and_creates_a_client`
        // (crate::TDJSON_TEST_LOCK's doc) so the two real-`tdjson`-client tests
        // never run concurrently in the same process.
        let _guard = crate::TDJSON_TEST_LOCK.lock().await;
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
