//! Secret-chat lifecycle prompts (#87): the start/close action offered for the
//! chat-list's selected chat, and the small confirm overlay that runs it.
//!
//! Core owns the secret-chat lifecycle seam
//! ([`SecretChatRequests`](tuigram_core::SecretChatRequests):
//! `create_new_secret_chat` / `close_secret_chat`) and the folded state
//! ([`SecretChatStore`](tuigram_core::SecretChatStore)). This is the TUI side:
//! [`SecretLifecycle::for_chat`] reads the selected chat (and its folded
//! [`SecretChatState`], when it is a secret chat) and decides which single
//! lifecycle action is meaningful — start a new secret chat with a private chat's
//! user (or with a closed secret chat's partner), or close a still-open one — and
//! [`SecretChatPrompt`] is the confirm overlay's state. Confirm records the chosen
//! [`SecretLifecycle`] as the app's pending secret action, which the loop drains and
//! dispatches on the core seam (#121); the resulting `updateSecretChat` folds back
//! and re-projects the row's state.
//!
//! No encryption-key material is ever involved: the decision reads only the chat
//! kind and the lifecycle state, never the secret chat's `key_hash`.

use tuigram_core::model::{Chat, ChatKind, SecretChatState};

/// The single lifecycle action offered for a selected chat, or `None` when none
/// applies (a group or channel, which has no one-to-one secret chat).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretLifecycle {
    /// Start a new secret chat with `user_id` — offered for a private chat, and
    /// for a secret chat that has been closed (start a fresh one with the partner).
    Start { user_id: i64 },
    /// Close the still-open (pending or ready) secret chat `secret_chat_id`.
    Close { secret_chat_id: i32 },
}

impl SecretLifecycle {
    /// Decide the lifecycle action for `chat`, given its folded secret-chat
    /// `state` (only meaningful, and only `Some`, for a [`ChatKind::Secret`]):
    ///
    /// - a private chat → start a secret chat with its user;
    /// - an open (pending/ready) secret chat → close it;
    /// - a closed secret chat → start a fresh one with the same partner;
    /// - anything else (group, channel) → nothing.
    ///
    /// Reads only the chat's kind and lifecycle state — never any key material.
    #[must_use]
    pub fn for_chat(chat: &Chat, state: Option<SecretChatState>) -> Option<Self> {
        match chat.kind {
            ChatKind::Private { user_id } => Some(Self::Start { user_id }),
            ChatKind::Secret {
                secret_chat_id,
                user_id,
            } => match state {
                // A closed chat is dead; the only move left is a fresh one.
                Some(SecretChatState::Closed) => Some(Self::Start { user_id }),
                _ => Some(Self::Close { secret_chat_id }),
            },
            ChatKind::BasicGroup { .. }
            | ChatKind::Supergroup { .. }
            | ChatKind::Channel { .. } => None,
        }
    }
}

/// The secret-chat confirm overlay's state: the chosen [`SecretLifecycle`] and the
/// title of the chat it acts on, for the prompt text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretChatPrompt {
    lifecycle: SecretLifecycle,
    chat_title: String,
}

impl SecretChatPrompt {
    /// Build a prompt for `lifecycle`, naming `chat_title` in its question.
    #[must_use]
    pub fn new(lifecycle: SecretLifecycle, chat_title: String) -> Self {
        Self {
            lifecycle,
            chat_title,
        }
    }

    /// The lifecycle action this prompt confirms — the seam the loop dispatches on
    /// confirm (`create_new_secret_chat` / `close_secret_chat`), recorded as the
    /// app's pending secret action (#121).
    #[must_use]
    pub fn lifecycle(&self) -> SecretLifecycle {
        self.lifecycle
    }

    /// The confirm question shown in the overlay, naming the action and the chat.
    #[must_use]
    pub fn prompt(&self) -> String {
        let title = self.chat_title.trim();
        let title = if title.is_empty() { "this chat" } else { title };
        match self.lifecycle {
            SecretLifecycle::Start { .. } => format!("Start a new secret chat with {title}?"),
            SecretLifecycle::Close { .. } => format!("Close the secret chat with {title}?"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuigram_core::model::Chat;

    /// A minimal chat of the given kind for the lifecycle decision tests.
    fn chat(id: i64, title: &str, kind: ChatKind) -> Chat {
        Chat {
            id,
            title: title.to_owned(),
            kind,
            last_message: None,
            unread_count: 0,
            unread_mention_count: 0,
            last_read_inbox_message_id: 0,
            last_read_outbox_message_id: 0,
            positions: Vec::new(),
            draft: None,
            pinned_message_ids: Vec::new(),
        }
    }

    #[test]
    fn a_private_chat_offers_to_start_a_secret_chat() {
        let private = chat(7, "Alice", ChatKind::Private { user_id: 7 });
        assert_eq!(
            SecretLifecycle::for_chat(&private, None),
            Some(SecretLifecycle::Start { user_id: 7 })
        );
    }

    #[test]
    fn an_open_secret_chat_offers_to_close() {
        let secret = chat(
            -5,
            "Mallory",
            ChatKind::Secret {
                secret_chat_id: 9,
                user_id: 7,
            },
        );
        // Pending and ready are both "open" — the action is to close.
        for state in [SecretChatState::Pending, SecretChatState::Ready] {
            assert_eq!(
                SecretLifecycle::for_chat(&secret, Some(state)),
                Some(SecretLifecycle::Close { secret_chat_id: 9 })
            );
        }
    }

    #[test]
    fn a_closed_secret_chat_offers_to_start_a_fresh_one() {
        let secret = chat(
            -5,
            "Mallory",
            ChatKind::Secret {
                secret_chat_id: 9,
                user_id: 7,
            },
        );
        assert_eq!(
            SecretLifecycle::for_chat(&secret, Some(SecretChatState::Closed)),
            Some(SecretLifecycle::Start { user_id: 7 }),
            "closed → start a new one with the same partner"
        );
    }

    #[test]
    fn groups_and_channels_offer_nothing() {
        let group = chat(1, "Team", ChatKind::BasicGroup { basic_group_id: 1 });
        let channel = chat(2, "News", ChatKind::Channel { supergroup_id: 2 });
        assert_eq!(SecretLifecycle::for_chat(&group, None), None);
        assert_eq!(SecretLifecycle::for_chat(&channel, None), None);
    }

    #[test]
    fn the_prompt_names_the_action_and_the_chat() {
        let start =
            SecretChatPrompt::new(SecretLifecycle::Start { user_id: 7 }, "Alice".to_owned());
        assert!(start.prompt().contains("Start"));
        assert!(start.prompt().contains("Alice"));

        let close = SecretChatPrompt::new(
            SecretLifecycle::Close { secret_chat_id: 9 },
            "Mallory".to_owned(),
        );
        assert!(close.prompt().contains("Close"));
        assert!(close.prompt().contains("Mallory"));
    }

    #[test]
    fn a_blank_title_falls_back_to_a_generic_phrase() {
        let prompt = SecretChatPrompt::new(SecretLifecycle::Start { user_id: 7 }, "  ".to_owned());
        assert!(prompt.prompt().contains("this chat"));
    }
}
