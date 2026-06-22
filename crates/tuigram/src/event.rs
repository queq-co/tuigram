//! Application events from the core layer, and the temporary fake source that
//! produces them. In Phase 6 the real `tuigram_core::Client` update stream
//! replaces [`spawn_fake_source`]; the loop's mpsc arm and the [`AppEvent`] seam
//! stay exactly as they are.

use std::time::Duration;

use tokio::sync::mpsc;

/// An event originating below the UI. Currently only a liveness heartbeat — this
/// is the seam the real core update stream (new messages, auth-state changes, …)
/// plugs into in Phase 6 without the event loop changing shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppEvent {
    /// A heartbeat from core.
    Beat,
}

/// Spawn a placeholder core source that emits a heartbeat on `period`, so the
/// loop's mpsc arm is exercised end-to-end before a real `Client` exists. The
/// returned receiver is the loop's core channel; once the loop drops it, the
/// next send fails and the task ends quietly.
pub fn spawn_fake_source(period: Duration) -> mpsc::Receiver<AppEvent> {
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(period);
        loop {
            ticker.tick().await;
            if tx.send(AppEvent::Beat).await.is_err() {
                break;
            }
        }
    });
    rx
}
