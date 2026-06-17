//! `tuigram` binary entry point.
//!
//! The Ratatui TUI lands in Phase 4. For now this is a placeholder that proves
//! the workspace wiring (binary → core library) builds and runs.

fn main() {
    println!(
        "tuigram {} (core {})",
        env!("CARGO_PKG_VERSION"),
        tuigram_core::version()
    );
}
