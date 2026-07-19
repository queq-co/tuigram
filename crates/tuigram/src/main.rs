//! `tuigram` — a Ratatui Telegram client.
//!
//! The binary is a one-line forward into [`tuigram_client::run_app`], which
//! holds the whole Phase 5 spine (terminal guard, login, the run loop). See
//! that crate's root doc for the real picture; splitting main.rs down to this
//! (#183) gives the crate a library target so `benches/` can link it.

use std::process::ExitCode;

// dhat needs to own the global allocator to see every allocation (#185,
// `profile-dhat` feature) — a normal build keeps the system allocator.
#[cfg(feature = "profile-dhat")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

fn main() -> ExitCode {
    // Heap profiling (#185, `profile-dhat` feature): the guard must outlive the
    // whole run, so it's held here in `main` rather than inside `run_app` —
    // dropping it (on return) is what flushes `dhat-heap.json`.
    #[cfg(feature = "profile-dhat")]
    let _profiler = dhat::Profiler::new_heap();

    tuigram_client::run_app()
}
