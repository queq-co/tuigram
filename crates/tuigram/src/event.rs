//! Application events from the core layer, and the live source that produces
//! them. This is the Phase 5 ↔ 6 seam: Phase 5 fed the loop's mpsc arm from a
//! fake heartbeat, and Phase 6 (#110) feeds it from the real
//! [`tuigram_core::Client`] update stream via [`spawn_core_source`]. The loop's
//! `tokio::select!` shape is unchanged — the same mpsc receiver, the same
//! `on_app_event → Action → dispatch` path.
//!
//! [`AppEvent`] is a *signal*, not the data: each variant means "this domain may
//! have changed, repaint", and the projection of the folded account state into
//! the panes reads it back from the `Client` (later Phase 6 issues). The one
//! exception is [`AppEvent::Connection`], which carries the already-projected
//! state so the status bar folds it without a second core read.

use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tuigram_core::enums::Update;
use tuigram_core::{Client, RouterEvent};

use crate::status::ConnectionState;

/// An event originating below the UI: a redraw-worthy signal classified from the
/// live update feed. Most variants are bare nudges — the data is read back from
/// the `Client` when the panes project it — except [`Connection`](AppEvent::Connection),
/// which carries the projected state the status bar folds directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppEvent {
    /// `updateConnectionState`: the transport's link/sync state, projected onto
    /// the status bar's [`ConnectionState`].
    Connection(ConnectionState),
    /// `updateAuthorizationState`: the session's authorization changed after login
    /// (e.g. logged out from another device, or the session is closing).
    Auth,
    /// A chat-list update folded by core: the chat list may have changed.
    Chats,
    /// A message update folded by core: a chat's history may have changed.
    Messages,
    /// `updateFile`: a download or upload made progress.
    File,
    /// The live feed reported a dropped-update gap (a broadcast overflow): some
    /// change signals were missed, so the UI repaints to be safe. This is the
    /// stream-level error signal — the only failure the update feed itself raises.
    Lagged,
}

/// Depth of the core → loop mpsc channel. Deep enough to absorb a burst of
/// updates between frames without backpressuring the forwarder, bounded so a
/// flood can't grow it without limit; the loop coalesces the backlog through the
/// dirty flag and the frame tick regardless.
const CORE_CHANNEL_DEPTH: usize = 256;

/// Spawn the live core source: subscribe to `client`'s lagged-aware update feed,
/// classify each event into an [`AppEvent`], and forward it onto the loop's mpsc
/// arm. Updates the UI does not react to (connectivity/metadata) are dropped at
/// the source, so the loop only wakes for redraw-worthy signals.
///
/// The returned receiver is the loop's core channel. The task ends when the loop
/// drops it (the next send fails) or when the bridge closes its broadcast (the
/// session shut down) — the latter is the clean-exit path on quit.
pub fn spawn_core_source(client: &Client) -> mpsc::Receiver<AppEvent> {
    let (tx, rx) = mpsc::channel(CORE_CHANNEL_DEPTH);
    // Our own lagged-aware subscription, independent of the router's: the router
    // keeps folding the account state on its subscription; this one only nudges
    // the UI to repaint, so its lag is harmless (see `AppEvent::Lagged`).
    let mut events = client.bridge().router_events();
    tokio::spawn(async move {
        while let Some(event) = events.next().await {
            if let Some(app_event) = classify(event)
                && tx.send(app_event).await.is_err()
            {
                break;
            }
        }
    });
    rx
}

/// Classify one [`RouterEvent`] from the live feed into a redraw-worthy
/// [`AppEvent`], or `None` for updates the UI does not react to.
fn classify(event: RouterEvent) -> Option<AppEvent> {
    match event {
        RouterEvent::Update(update) => classify_update(&update),
        // A gap in *this* subscription means we may have missed change signals;
        // the account state itself is still folded correctly on the router's own
        // subscription, so the safe reaction is simply to repaint.
        RouterEvent::Lagged(_) => Some(AppEvent::Lagged),
    }
}

/// Tag a single update with the UI signal it produces, or `None` to ignore it.
///
/// Mirrors the core router's own routing (chats/messages/files/connection) so the
/// UI's notion of "what changed" stays aligned with what the account state folds,
/// plus the post-login `updateAuthorizationState` the router deliberately ignores
/// but the UI cares about. A new, unmodelled update defaults to `None` (no
/// repaint), the same safe default the router takes.
fn classify_update(update: &Update) -> Option<AppEvent> {
    match update {
        Update::ConnectionState(u) => Some(AppEvent::Connection(project_connection(&u.state))),
        Update::AuthorizationState(_) => Some(AppEvent::Auth),
        Update::NewChat(_)
        | Update::ChatPosition(_)
        | Update::ChatLastMessage(_)
        | Update::ChatReadInbox(_)
        | Update::ChatReadOutbox(_)
        | Update::ChatDraftMessage(_)
        | Update::ChatFolders(_)
        | Update::MessageIsPinned(_) => Some(AppEvent::Chats),
        Update::NewMessage(_)
        | Update::MessageSendSucceeded(_)
        | Update::MessageSendFailed(_)
        | Update::MessageContent(_)
        | Update::MessageInteractionInfo(_)
        | Update::DeleteMessages(_) => Some(AppEvent::Messages),
        Update::File(_) => Some(AppEvent::File),
        _ => None,
    }
}

/// Project TDLib's connection state onto the status-bar [`ConnectionState`].
///
/// Reuses core's [`from_tdlib`](tuigram_core::ConnectionState::from_tdlib) for the
/// raw-enum mapping (one source of truth for that), then collapses core's
/// proxy-connecting state onto plain connecting — the status bar draws no proxy
/// distinction. Exhaustive on core's enum, so a new core variant is a compile
/// error here, not a silent miss.
fn project_connection(state: &tuigram_core::enums::ConnectionState) -> ConnectionState {
    use tuigram_core::ConnectionState as Core;
    match tuigram_core::ConnectionState::from_tdlib(state) {
        Core::WaitingForNetwork => ConnectionState::WaitingForNetwork,
        Core::Connecting | Core::ConnectingToProxy => ConnectionState::Connecting,
        Core::Updating => ConnectionState::Updating,
        Core::Ready => ConnectionState::Ready,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuigram_core::enums::ConnectionState as Tc;
    use tuigram_core::types::{
        UpdateAuthorizationState, UpdateChatReadInbox, UpdateConnectionState, UpdateDeleteMessages,
        UpdateFile, UpdateUserStatus,
    };

    #[test]
    fn connection_updates_project_onto_the_status_state() {
        let event = |state| {
            classify(RouterEvent::Update(Update::ConnectionState(
                UpdateConnectionState { state },
            )))
        };
        assert_eq!(
            event(Tc::Ready),
            Some(AppEvent::Connection(ConnectionState::Ready))
        );
        assert_eq!(
            event(Tc::Updating),
            Some(AppEvent::Connection(ConnectionState::Updating))
        );
        // The proxy-connecting state collapses onto plain connecting.
        assert_eq!(
            event(Tc::ConnectingToProxy),
            Some(AppEvent::Connection(ConnectionState::Connecting))
        );
        assert_eq!(
            event(Tc::WaitingForNetwork),
            Some(AppEvent::Connection(ConnectionState::WaitingForNetwork))
        );
    }

    #[test]
    fn domain_updates_classify_to_their_signal() {
        let signal = |u| classify(RouterEvent::Update(u));
        assert_eq!(
            signal(Update::ChatReadInbox(UpdateChatReadInbox {
                chat_id: 1,
                last_read_inbox_message_id: 10,
                unread_count: 0,
            })),
            Some(AppEvent::Chats)
        );
        assert_eq!(
            signal(Update::DeleteMessages(UpdateDeleteMessages {
                chat_id: 1,
                message_ids: vec![1],
                is_permanent: true,
                from_cache: false,
            })),
            Some(AppEvent::Messages)
        );
        assert_eq!(
            signal(Update::File(UpdateFile {
                file: tuigram_core::types::File {
                    id: 7,
                    ..Default::default()
                },
            })),
            Some(AppEvent::File)
        );
        assert_eq!(
            signal(Update::AuthorizationState(UpdateAuthorizationState {
                authorization_state: tuigram_core::enums::AuthorizationState::Ready,
            })),
            Some(AppEvent::Auth)
        );
    }

    #[test]
    fn unreacted_updates_are_dropped_at_the_source() {
        // updateUserStatus is folded by core (the users store) but the UI does not
        // repaint for it, so the source drops it rather than waking the loop.
        assert_eq!(
            classify(RouterEvent::Update(Update::UserStatus(UpdateUserStatus {
                user_id: 1,
                status: tuigram_core::enums::UserStatus::Recently(Default::default()),
            }))),
            None
        );
    }

    #[test]
    fn a_broadcast_gap_surfaces_as_the_lagged_signal() {
        assert_eq!(classify(RouterEvent::Lagged(7)), Some(AppEvent::Lagged));
    }
}
