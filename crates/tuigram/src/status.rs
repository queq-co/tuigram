//! The ambient-feedback layer (#88): the persistent status bar's connection state
//! and the transient toast/notification queue.
//!
//! [`ConnectionState`] is the core link's lifecycle, mirrored from `TDLib`'s
//! `connectionState` — the status bar's left field. The toasts are short-lived
//! one-off messages (a send failed, a download finished, an auth error *code*):
//! they float over the panes **without capturing input**, so the loop never
//! blocks, and they leave either by timing out (a fixed number of heartbeats) or
//! by being dismissed.
//!
//! Errors surface a fixed action phrase and an optional core error *code* — never
//! the user's typed input, the same rule the login flow follows.

use std::borrow::Cow;
use std::collections::VecDeque;

/// The core link's connection lifecycle, mirrored from `TDLib`'s `connectionState`.
/// A total set, so a status read is always classified. The real
/// `updateConnectionState` folds into it live (#112): the core source projects
/// every `TDLib` connection state onto a variant here
/// ([`project_connection`](crate::event)), so each one is constructed in the
/// binary, not just the tests.
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

/// The lifetime of an info/success toast, in heartbeats (~1s each): long enough to
/// read a short line, short enough not to linger.
const DEFAULT_TTL: u32 = 5;

/// The lifetime of an error toast, in heartbeats (~1s each): longer than
/// [`DEFAULT_TTL`] because a failure is something the user may need to read and
/// act on, so it should not vanish as quickly as a routine event. It still ages
/// out on its own and Ctrl-G dismisses it early, per the non-capturing contract.
const ERROR_TTL: u32 = 12;

/// One transient toast: a severity, a message, and the remaining heartbeats before
/// it auto-dismisses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    level: NoticeLevel,
    text: String,
    ttl: u32,
}

// The toast constructors are the Phase-6 building API: core calls them (through
// [`App::notify`]) when an action fails or a one-off event lands.
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
    /// (e.g. `error("send", Some("FLOOD_WAIT"))` → "send failed (`FLOOD_WAIT`)").
    /// Built only from a fixed action phrase and a core code — never the user's
    /// typed input, the same rule the login flow follows.
    #[must_use]
    pub fn error(action: &str, code: Option<&str>) -> Self {
        let text = match code {
            Some(code) => format!("{action} failed ({code})"),
            None => format!("{action} failed"),
        };
        Self::new(NoticeLevel::Error, text)
    }

    /// Build an error toast for a failed core request (#122): the fixed `action`
    /// phrase plus the raw `TDLib` error `message`, folded through
    /// [`normalize_error`] into a short readable code. This is the single entry
    /// point every Phase-6 send path funnels through, so a flood-wait, a
    /// permission denial, or a dropped link all read the same way regardless of
    /// which request failed. Like [`error`](Self::error) it shows only the action
    /// and a normalized code — never the user's input.
    #[must_use]
    pub fn from_core_error(action: &str, message: &str) -> Self {
        let code = normalize_error(message);
        Self::error(action, (!code.is_empty()).then_some(code.as_ref()))
    }

    /// Build a notice, giving errors the longer [`ERROR_TTL`] so a failure lingers
    /// long enough to read while routine info/success toasts clear on [`DEFAULT_TTL`].
    fn new(level: NoticeLevel, text: String) -> Self {
        let ttl = match level {
            NoticeLevel::Error => ERROR_TTL,
            NoticeLevel::Info | NoticeLevel::Success => DEFAULT_TTL,
        };
        Self { level, text, ttl }
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

/// Fold a raw `TDLib` error message into a short, readable code for an error toast
/// (#122).
///
/// `TDLib` names a rejection with a fixed code or phrase — `FLOOD_WAIT_42`,
/// `CHAT_WRITE_FORBIDDEN`, `USER_PRIVACY_RESTRICTED` — never the user's input, so
/// the raw string is always safe to show; it just doesn't read well in a toast.
/// This collapses the common families (rate limits, permission/privacy denials,
/// transport failures) each onto one plain phrase, matching case-insensitively so
/// a family's variants (`FLOOD_WAIT`, `SLOWMODE_WAIT`; the several `*_FORBIDDEN`s)
/// all normalize together. Anything unrecognized keeps its original message — still
/// a safe fixed code — so no failure is ever swallowed into a silent toast. An
/// empty message (`TDLib` gave only a numeric code) yields an empty code, which
/// [`Notice::from_core_error`] renders as a bare "… failed".
///
/// Ordering matters where families overlap: `USER_PRIVACY_RESTRICTED` is caught by
/// the privacy arm before the permission arm's broader `RESTRICTED`.
#[must_use]
pub fn normalize_error(message: &str) -> Cow<'_, str> {
    let upper = message.to_ascii_uppercase();
    if upper.contains("FLOOD_WAIT") || upper.contains("SLOWMODE_WAIT") {
        Cow::Borrowed("rate limited")
    } else if upper.contains("PRIVACY") || upper.contains("BLOCKED") {
        Cow::Borrowed("privacy")
    } else if upper.contains("FORBIDDEN")
        || upper.contains("RESTRICTED")
        || upper.contains("ADMIN_REQUIRED")
    {
        Cow::Borrowed("not allowed")
    } else if upper.contains("CONNECT") || upper.contains("NETWORK") || upper.contains("TIMEOUT") {
        Cow::Borrowed("network")
    } else {
        Cow::Borrowed(message)
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

    /// Age the current toast by one tick, dropping it when it expires. Returns
    /// whether a toast was removed — the only change the render path must repaint
    /// for, since a still-counting toast looks the same.
    ///
    /// Driven by the loop's wall-clock notice interval (#139): the Phase-5 heartbeat
    /// that first drove this was removed with the fake source in #110, and the toast
    /// producers came back (#116, #120) without it, so toasts never aged out until
    /// the interval was restored here.
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
    fn normalize_error_folds_the_common_tdlib_families() {
        // Rate limits: the flood/slowmode waits (with their trailing seconds) both
        // read as one phrase.
        assert_eq!(normalize_error("FLOOD_WAIT_42"), "rate limited");
        assert_eq!(normalize_error("SLOWMODE_WAIT_10"), "rate limited");
        // Privacy/blocked, checked before the broader permission arm so
        // USER_PRIVACY_RESTRICTED reads as privacy, not "not allowed".
        assert_eq!(normalize_error("USER_PRIVACY_RESTRICTED"), "privacy");
        assert_eq!(normalize_error("USER_IS_BLOCKED"), "privacy");
        // Permission denials, across the several *_FORBIDDEN / admin variants.
        assert_eq!(normalize_error("CHAT_WRITE_FORBIDDEN"), "not allowed");
        assert_eq!(normalize_error("CHAT_ADMIN_REQUIRED"), "not allowed");
        assert_eq!(normalize_error("CHAT_FORWARDS_RESTRICTED"), "not allowed");
        // Transport failures.
        assert_eq!(normalize_error("Connection closed"), "network");
    }

    #[test]
    fn normalize_error_keeps_an_unrecognized_code_verbatim() {
        // An unmodelled but still-safe TDLib code passes through unchanged rather
        // than being swallowed, so the toast always says something.
        assert_eq!(normalize_error("CHAT_NOT_FOUND"), "CHAT_NOT_FOUND");
        // An empty message (only a numeric code came back) yields an empty code.
        assert_eq!(normalize_error(""), "");
    }

    #[test]
    fn from_core_error_normalizes_and_reads_uniformly() {
        // The single send-path entry point: a raw TDLib message becomes a readable
        // toast, the same shape for every failed action.
        let flood = Notice::from_core_error("send", "FLOOD_WAIT_42");
        assert_eq!(flood.level(), NoticeLevel::Error);
        assert_eq!(flood.line(), "✗ send failed (rate limited)");

        let forbidden = Notice::from_core_error("pin", "CHAT_WRITE_FORBIDDEN");
        assert_eq!(forbidden.line(), "✗ pin failed (not allowed)");

        // A code-only failure (blank message) drops to a bare "… failed".
        let bare = Notice::from_core_error("reaction", "");
        assert_eq!(bare.line(), "✗ reaction failed");
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

    // Errors must outlast routine toasts (ERROR_TTL > DEFAULT_TTL), checked at
    // compile time so the constants can't drift the wrong way.
    const _: () = assert!(ERROR_TTL > DEFAULT_TTL);

    #[test]
    fn an_error_toast_outlives_an_info_toast() {
        // Errors carry the longer ERROR_TTL so a failure lingers long enough to
        // read; info/success clear on the shorter DEFAULT_TTL.
        let mut notes = Notifications::default();
        notes.push(Notice::error("send", Some("FLOOD_WAIT")));
        // It survives every tick an info toast would have expired on.
        for _ in 0..DEFAULT_TTL {
            notes.tick();
        }
        assert!(
            notes.current().is_some(),
            "error still showing past DEFAULT_TTL"
        );
        // And clears once its own longer lifetime runs out.
        for _ in DEFAULT_TTL..ERROR_TTL {
            notes.tick();
        }
        assert!(notes.current().is_none(), "error clears on ERROR_TTL");
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
