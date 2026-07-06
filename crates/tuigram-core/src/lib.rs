//! Headless core for **tuigram** — Telegram client logic built on TDLib.
//!
//! This crate is intentionally free of any terminal/UI concerns so it can be
//! unit-tested without a TTY. Phases 2–3 (auth, chats, messages) live here;
//! the Ratatui front-end (Phases 4–5) depends on this crate.

pub mod actions;
pub mod auth;
pub mod bridge;
pub mod chats;
pub mod client;
pub mod command_surface;
pub mod connection;
pub mod contacts;
pub mod credentials;
pub mod files;
pub mod messages;
pub mod model;
pub mod router;
pub mod sanitize;
pub mod secret_chats;
pub mod session;
pub mod settings;
pub mod users;

pub use actions::{ChatActionRequests, ChatActionStore};
pub use auth::{AuthRequests, AuthState, Login};
pub use bridge::{Bridge, ClientParameters, RouterEvent, RouterStream, TgClient, UpdateStream};
pub use chats::{
    CHATS_EXHAUSTED, ChatLifecycleRequests, ChatRequests, ChatStore, load_archive_list,
    load_folder_list, load_main_list,
};
pub use client::{AccountState, Client};
pub use command_surface::REPL_COMMANDS;
pub use connection::{ConnectionState, ConnectionStore};
pub use contacts::ContactRequests;
pub use credentials::{
    ApiCredentials, CredentialError, CredentialResolver, Onboarding, is_api_id_published_flood,
};
pub use files::{
    DOWNLOAD_PRIORITY, FileRequests, FileStore, SWEEP_IMMUNITY_DELAY, StorageRequests,
};
pub use messages::{
    DeleteRequests, EditRequests, ForwardRequests, HistoryRequests, MessageRequests, MessageStore,
    NEWEST, PinRequests, ReactionRequests, ReadRequests, SearchPage, SearchRequests, SearchResults,
    SendRequests, load_history, search_chat, search_global,
};
pub use model::{
    Animation, Audio, Chat, ChatAction, ChatFolderInfo, ChatKind, ChatListKind, ChatPosition,
    Contact, Document, Draft, EntityKind, File, FileRef, FormattedText, Location, Message,
    MessageContent, OutgoingMedia, Photo, Poll, PollKind, PollOption, Presence, Reaction,
    ReactionKind, SecretChat, SecretChatState, SendState, Sender, Sticker, TextEntity, User,
    UserKind, Venue, Video, Voice,
};
pub use router::{Router, UpdateSink};
pub use sanitize::{scrub_line, scrub_prose};
pub use secret_chats::{SecretChatRequests, SecretChatStore};
pub use session::{EncryptionKey, SessionError, SessionStorage};
pub use settings::{CacheCap, InterfaceSettings, KeepMedia, StorageSettings};
pub use users::{UserRequests, UserStore};

/// TDLib's typed request API and data model, re-exported so callers depend on
/// it through tuigram-core rather than reaching for `tdlib-rs` directly. Drive
/// `functions::*` with a [`Bridge::id`]; the bridge's receive loop resolves the
/// futures they return.
pub use tdlib_rs::{enums, functions, types};

/// Crate version, sourced from Cargo at build time.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Returns the `tuigram-core` version string.
#[must_use]
pub fn version() -> &'static str {
    VERSION
}

/// Serializes tests that create a real `tdjson` client (#223).
///
/// `tdjson`'s receive queue and observer are process-global —
/// [`Bridge`](bridge::Bridge)'s own module doc documents "at most one
/// `Bridge` per process" as an invariant — but Rust's default test harness
/// runs `#[test]`/`#[tokio::test]` functions in parallel across threads
/// within one process. [`prebuilt_tdjson_loads_and_creates_a_client`] (below)
/// and `bridge::tests::live_request_correlates_and_updates_stream` both
/// create a client, so without this lock they can race each other, which is
/// the suspected cause of a Windows-only CI hang (#223 — unconfirmed on other
/// platforms, since it hasn't been observed there). A `tokio::sync::Mutex`
/// (not `std::sync::Mutex`) because the async test holds the guard across
/// `.await` points.
#[cfg(test)]
pub(crate) static TDJSON_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(test)]
mod tests {
    use super::version;

    #[test]
    fn version_is_reported() {
        assert_eq!(version(), env!("CARGO_PKG_VERSION"));
        assert!(!version().is_empty());
    }

    /// Runtime proof that our configured prebuilt `tdjson` actually loads and
    /// its C ABI is callable on this host — not merely that it links. Creating
    /// a client dynamically loads `libtdjson` (and its OpenSSL 3 / zlib deps,
    /// the per-target runtime contract in docs/research/tdlib.md) and calls into
    /// it. The async request/update bridge over this client lands in #5.
    #[test]
    fn prebuilt_tdjson_loads_and_creates_a_client() {
        let _guard = crate::TDJSON_TEST_LOCK.blocking_lock();
        let client_id = tdlib_rs::create_client();
        assert!(client_id >= 0, "tdjson returned an invalid client id");
    }
}
