//! The ambient-feedback layer (#88): the persistent status bar's connection state
//! and the transient toast/notification queue.
//!
//! [`ConnectionState`] is the core link's lifecycle, mirrored from TDLib's
//! `connectionState` — the status bar's left field. The toasts are short-lived
//! one-off messages (a send failed, a download finished, an auth error *code*):
//! they float over the panes **without capturing input**, so the loop never
//! blocks, and they leave either by timing out (a fixed number of heartbeats) or
//! by being dismissed.
//!
//! Errors surface a fixed action phrase and an optional core error *code* — never
//! the user's typed input, the same rule the login flow follows.

use std::collections::VecDeque;

/// The core link's connection lifecycle, mirrored from TDLib's `connectionState`.
/// A total set, so a status read is always classified; the real
/// `updateConnectionState` folds into it in Phase 6. Some variants are only
/// constructed by that Phase-6 fold (and the tests) for now, so the whole set is
/// kept regardless of current construction.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConnectionState {
    /// No network reachable; waiting for one.
    WaitingForNetwork,
    /// Establishing the link to Telegram (the landing state before core is up).
    #[default]
    Connecting,
    /// Connected; fetching the initial state (chats, pending updates).
    Updating,
    /// Fully connected and up to date.
    Ready,
}

impl ConnectionState {
    /// The short human label shown in the status bar.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::WaitingForNetwork => "waiting for network",
            Self::Connecting => "connecting…",
            Self::Updating => "updating…",
            Self::Ready => "online",
        }
    }

    /// The dot drawn before the label: filled once ready, hollow while not.
    #[must_use]
    pub fn symbol(self) -> &'static str {
        if self.is_ready() { "●" } else { "○" }
    }

    /// Whether the link is fully connected and up to date.
    #[must_use]
    pub fn is_ready(self) -> bool {
        self == Self::Ready
    }
}

/// A toast's severity, which selects its marker (and, in render, its emphasis).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeLevel {
    /// A neutral one-off event ("download complete").
    Info,
    /// A completed action ("message sent").
    Success,
    /// A failure, carrying a category and optional core code — never user input.
    Error,
}

impl NoticeLevel {
    /// The marker drawn before the message.
    #[must_use]
    pub fn marker(self) -> &'static str {
        match self {
            Self::Info => "ℹ",
            Self::Success => "✓",
            Self::Error => "✗",
        }
    }
}

/// The default lifetime of a toast, in heartbeats (~1s each): long enough to read
/// a short line, short enough not to linger.
const DEFAULT_TTL: u32 = 5;

/// One transient toast: a severity, a message, and the remaining heartbeats before
/// it auto-dismisses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    level: NoticeLevel,
    text: String,
    ttl: u32,
}

// The toast constructors are the Phase-6 building API: core calls them (through
// [`App::notify`]) when an action fails or a one-off event lands. Until that
// wiring exists they are reached only from the tests, so they are dead in the
// binary — annotated like `notify` itself.
impl Notice {
    /// An informational toast (a one-off event like "download complete").
    #[allow(dead_code)]
    #[must_use]
    pub fn info(text: impl Into<String>) -> Self {
        Self::new(NoticeLevel::Info, text.into())
    }

    /// A success toast (a completed action like "message sent").
    #[allow(dead_code)]
    #[must_use]
    pub fn success(text: impl Into<String>) -> Self {
        Self::new(NoticeLevel::Success, text.into())
    }

    /// An error toast naming a failed `action` and an optional core error `code`
    /// (e.g. `error("send", Some("FLOOD_WAIT"))` → "send failed (FLOOD_WAIT)").
    /// Built only from a fixed action phrase and a core code — never the user's
    /// typed input, the same rule the login flow follows.
    #[allow(dead_code)]
    #[must_use]
    pub fn error(action: &str, code: Option<&str>) -> Self {
        let text = match code {
            Some(code) => format!("{action} failed ({code})"),
            None => format!("{action} failed"),
        };
        Self::new(NoticeLevel::Error, text)
    }

    #[allow(dead_code)]
    fn new(level: NoticeLevel, text: String) -> Self {
        Self {
            level,
            text,
            ttl: DEFAULT_TTL,
        }
    }

    /// This toast's severity, for the render emphasis.
    #[must_use]
    pub fn level(&self) -> NoticeLevel {
        self.level
    }

    /// The full line shown in the toast: the level marker and the message.
    #[must_use]
    pub fn line(&self) -> String {
        format!("{} {}", self.level.marker(), self.text)
    }
}

/// The transient-notification queue: a small stack of toasts shown one at a time
/// (the front), so a burst never blocks the loop. Each heartbeat ages the front
/// toast; when it expires it pops and the next one shows.
#[derive(Debug, Default)]
pub struct Notifications {
    queue: VecDeque<Notice>,
}

impl Notifications {
    /// Enqueue a toast behind any already showing.
    pub fn push(&mut self, notice: Notice) {
        self.queue.push_back(notice);
    }

    /// The toast currently shown (the front of the queue), if any.
    #[must_use]
    pub fn current(&self) -> Option<&Notice> {
        self.queue.front()
    }

    /// The number of toasts still queued behind the current one, for a "+N" hint.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.queue.len().saturating_sub(1)
    }

    /// Dismiss the current toast immediately, revealing the next.
    pub fn dismiss(&mut self) {
        self.queue.pop_front();
    }

    /// Age the current toast by one heartbeat, dropping it when it expires.
    /// Returns whether a toast was removed — the only change the render path must
    /// repaint for, since a still-counting toast looks the same.
    pub fn tick(&mut self) -> bool {
        if let Some(front) = self.queue.front_mut() {
            front.ttl = front.ttl.saturating_sub(1);
            if front.ttl == 0 {
                self.queue.pop_front();
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_states_label_and_mark_readiness() {
        assert_eq!(ConnectionState::default(), ConnectionState::Connecting);
        assert!(!ConnectionState::Connecting.is_ready());
        assert!(!ConnectionState::WaitingForNetwork.is_ready());
        assert!(!ConnectionState::Updating.is_ready());
        assert!(ConnectionState::Ready.is_ready());

        assert_eq!(ConnectionState::Ready.label(), "online");
        assert_eq!(ConnectionState::Ready.symbol(), "●");
        // Every not-ready state shows the hollow dot.
        assert_eq!(ConnectionState::Connecting.symbol(), "○");
        assert_eq!(ConnectionState::WaitingForNetwork.symbol(), "○");
    }

    #[test]
    fn notices_carry_a_marker_and_message() {
        let info = Notice::info("download complete");
        assert_eq!(info.level(), NoticeLevel::Info);
        assert_eq!(info.line(), "ℹ download complete");

        let ok = Notice::success("message sent");
        assert_eq!(ok.line(), "✓ message sent");
    }

    #[test]
    fn an_error_notice_names_the_action_and_code_never_input() {
        let with_code = Notice::error("send", Some("FLOOD_WAIT"));
        assert_eq!(with_code.level(), NoticeLevel::Error);
        assert_eq!(with_code.line(), "✗ send failed (FLOOD_WAIT)");

        // No code (an unclassified failure) still reads cleanly.
        let bare = Notice::error("download", None);
        assert_eq!(bare.line(), "✗ download failed");
    }

    #[test]
    fn the_queue_shows_one_toast_and_counts_the_rest() {
        let mut notes = Notifications::default();
        assert!(notes.current().is_none());
        assert_eq!(notes.pending(), 0);

        notes.push(Notice::info("first"));
        notes.push(Notice::info("second"));
        // The front shows; the other waits.
        assert_eq!(notes.current().unwrap().line(), "ℹ first");
        assert_eq!(notes.pending(), 1);
    }

    #[test]
    fn a_toast_times_out_after_its_lifetime() {
        let mut notes = Notifications::default();
        notes.push(Notice::info("hi"));
        // It stays for DEFAULT_TTL - 1 ticks, then the expiring tick pops it.
        for _ in 0..DEFAULT_TTL - 1 {
            assert!(!notes.tick(), "still showing");
            assert!(notes.current().is_some());
        }
        assert!(notes.tick(), "the expiring tick reports the change");
        assert!(notes.current().is_none());
        // Ticking an empty queue is a no-op.
        assert!(!notes.tick());
    }

    #[test]
    fn the_next_toast_shows_once_the_first_expires() {
        let mut notes = Notifications::default();
        notes.push(Notice::info("first"));
        notes.push(Notice::info("second"));
        for _ in 0..DEFAULT_TTL {
            notes.tick();
        }
        assert_eq!(notes.current().unwrap().line(), "ℹ second");
        assert_eq!(notes.pending(), 0);
    }

    #[test]
    fn dismiss_drops_the_current_toast_at_once() {
        let mut notes = Notifications::default();
        notes.push(Notice::info("first"));
        notes.push(Notice::info("second"));
        notes.dismiss();
        assert_eq!(notes.current().unwrap().line(), "ℹ second");
        notes.dismiss();
        assert!(notes.current().is_none());
    }
}
