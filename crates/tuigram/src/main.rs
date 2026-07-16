//! `tuigram` — a Ratatui Telegram client.
//!
//! The binary is a one-line forward into [`tuigram_client::run_app`], which
//! holds the whole Phase 5 spine (terminal guard, login, the run loop). See
//! that crate's root doc for the real picture; splitting main.rs down to this
//! (#183) gives the crate a library target so `benches/` can link it.

use std::process::ExitCode;

fn main() -> ExitCode {
    tuigram_client::run_app()
}
