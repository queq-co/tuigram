//! The REPL's canonical command-name list (#197) — the one thing both
//! front-ends can import to cross-check their command surfaces.
//!
//! The REPL (`crates/tuigram/examples/repl.rs`) and the TUI
//! (`crates/tuigram/src/app.rs`) cannot see each other's private tables: the
//! `tuigram` crate has no `[lib]` target (only a `[[bin]]` and a
//! `required-features`-gated `[[example]]`), so an example can't import from
//! the binary's modules, and the binary can't import from an example.
//! `tuigram-core` is the one library both compile against, so this plain data
//! list — not behavior, just names — is the shared source of truth the TUI's
//! command-parity guard (`crates/tuigram/src/parity.rs`) reads, and the REPL's
//! own help/completion-table guard cross-checks against.

/// Every command name `repl.rs`'s `run_repl` dispatches on. Kept in sync with
/// its `COMMANDS` help/completion table (guarded by a test there) and with the
/// TUI's parity guard (`crates/tuigram/src/parity.rs`), which the docs at
/// `docs/repl-tui-divergences.md` describe.
pub const REPL_COMMANDS: &[&str] = &[
    "chats",
    "open",
    "history",
    "send",
    "reply",
    "edit",
    "delete",
    "read",
    "search",
    "forward",
    "download",
    "file",
    "sendmedia",
    "archive",
    "folders",
    "folder",
    "react",
    "unreact",
    "pin",
    "unpin",
    "typing",
    "secret-new",
    "secrets",
    "secret-close",
    "status",
    "probe",
    "resync",
    "logout",
    "help",
    "quit",
];
