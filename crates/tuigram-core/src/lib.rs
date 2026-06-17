//! Headless core for **tuigram** — Telegram client logic built on TDLib.
//!
//! This crate is intentionally free of any terminal/UI concerns so it can be
//! unit-tested without a TTY. Phases 2–3 (auth, chats, messages) live here;
//! the Ratatui front-end (Phases 4–5) depends on this crate.

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
}
