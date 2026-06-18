//! Headless core for **tuigram** — Telegram client logic built on TDLib.
//!
//! This crate is intentionally free of any terminal/UI concerns so it can be
//! unit-tested without a TTY. Phases 2–3 (auth, chats, messages) live here;
//! the Ratatui front-end (Phases 4–5) depends on this crate.

pub mod auth;
pub mod bridge;

pub use auth::{AuthState, Login};
pub use bridge::{Bridge, ClientParameters, TgClient, UpdateStream};

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
        let client_id = tdlib_rs::create_client();
        assert!(client_id >= 0, "tdjson returned an invalid client id");
    }
}
