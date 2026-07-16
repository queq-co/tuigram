//! The connection-state projection: `TDLib`'s transport link/sync status.
//!
//! `TDLib` reports its connection to Telegram through `updateConnectionState`,
//! cycling `waitingForNetwork`/`connecting` → `updating` (fetching the updates
//! missed while offline) → `ready` (fully caught up). It is the honest answer to
//! "is what I'm looking at current yet?" right after launch or a network blip —
//! the official clients render it in the title bar.
//!
//! This is *not* account content: it carries only the transport's liveness, so
//! it folds into its own tiny [`ConnectionStore`] rather than onto any domain
//! snapshot. The status bar (#88) reads it to show a "Connecting…/Updating…"
//! indicator; the data itself stays fresh on its own, through the normal folded
//! update stream — this just lets the UI say whether that catch-up has settled.

use tdlib_rs::enums::Update;

/// `TDLib`'s connection/sync status, projected from `updateConnectionState`.
///
/// A reduced mirror of [`tdlib_rs::enums::ConnectionState`]. Only
/// [`Ready`](Self::Ready) means the client is fully connected and caught up;
/// every other state is a cue that the view may still be settling.
///
/// Total by *exhaustive* match in [`from_tdlib`](Self::from_tdlib) — a state
/// added by a future `TDLib` version is a compile error here, never a silent
/// misclassification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConnectionState {
    /// No network connectivity; `TDLib` is waiting for it. The startup default,
    /// before the first `updateConnectionState` arrives.
    #[default]
    WaitingForNetwork,
    /// Establishing a connection to Telegram directly.
    Connecting,
    /// Establishing a connection through a configured proxy.
    ConnectingToProxy,
    /// Connected, fetching the updates missed while offline — the "Updating…"
    /// state, where the folded snapshot is still catching up to the server.
    Updating,
    /// Connected and fully in sync; no indicator needed.
    Ready,
}

impl ConnectionState {
    /// Project a `TDLib` [`ConnectionState`](tdlib_rs::enums::ConnectionState) onto
    /// tuigram's.
    #[must_use]
    pub fn from_tdlib(state: &tdlib_rs::enums::ConnectionState) -> Self {
        use tdlib_rs::enums::ConnectionState as Tc;
        match state {
            Tc::WaitingForNetwork => Self::WaitingForNetwork,
            Tc::Connecting => Self::Connecting,
            Tc::ConnectingToProxy => Self::ConnectingToProxy,
            Tc::Updating => Self::Updating,
            Tc::Ready => Self::Ready,
        }
    }

    /// Whether the client is fully connected and caught up (`Ready`). When false,
    /// the snapshot may still be settling and a UI should show a sync indicator.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }
}

/// Holds the latest [`ConnectionState`], folded from `updateConnectionState`.
///
/// A single-value store: the most recent state wins. Defaults to
/// [`WaitingForNetwork`](ConnectionState::WaitingForNetwork), the pre-connection
/// state before `TDLib` has reported in.
#[derive(Debug, Clone, Default)]
pub struct ConnectionStore {
    state: ConnectionState,
}

impl ConnectionStore {
    /// The current connection/sync state.
    #[must_use]
    pub fn state(&self) -> ConnectionState {
        self.state
    }

    /// Whether the client is fully connected and in sync.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.state.is_ready()
    }

    /// Fold an `updateConnectionState`; any other update is ignored.
    pub fn reduce(&mut self, update: &Update) {
        if let Update::ConnectionState(u) = update {
            self.state = ConnectionState::from_tdlib(&u.state);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tdlib_rs::enums::ConnectionState as Tc;
    use tdlib_rs::types::{UpdateConnectionState, UpdateDeleteMessages};

    fn connection(state: Tc) -> Update {
        Update::ConnectionState(UpdateConnectionState { state })
    }

    #[test]
    fn default_is_waiting_for_network_and_not_ready() {
        let store = ConnectionStore::default();
        assert_eq!(store.state(), ConnectionState::WaitingForNetwork);
        assert!(!store.is_ready());
    }

    #[test]
    fn projects_each_tdlib_state() {
        let cases = [
            (Tc::WaitingForNetwork, ConnectionState::WaitingForNetwork),
            (Tc::Connecting, ConnectionState::Connecting),
            (Tc::ConnectingToProxy, ConnectionState::ConnectingToProxy),
            (Tc::Updating, ConnectionState::Updating),
            (Tc::Ready, ConnectionState::Ready),
        ];
        for (input, expected) in cases {
            assert_eq!(ConnectionState::from_tdlib(&input), expected);
        }
    }

    #[test]
    fn reduce_folds_the_latest_connection_state() {
        let mut store = ConnectionStore::default();

        store.reduce(&connection(Tc::Updating));
        assert_eq!(store.state(), ConnectionState::Updating);
        assert!(!store.is_ready());

        store.reduce(&connection(Tc::Ready));
        assert_eq!(store.state(), ConnectionState::Ready);
        assert!(store.is_ready());
    }

    #[test]
    fn reduce_ignores_unrelated_updates() {
        let mut store = ConnectionStore::default();
        store.reduce(&connection(Tc::Ready));
        // A non-connection update leaves the connection state untouched.
        store.reduce(&Update::DeleteMessages(UpdateDeleteMessages {
            chat_id: 1,
            message_ids: vec![1],
            is_permanent: true,
            from_cache: false,
        }));
        assert_eq!(store.state(), ConnectionState::Ready);
    }
}
