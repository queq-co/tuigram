//! The chat-list pane (#80): the left column of chats, with the #87/#160
//! per-row markers (unread badge, secret-chat lifecycle, chat-type icon,
//! transient action) and the #165 delivery glyph on our own last message.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph};

use tuigram_core::model::{Chat, ChatAction, ChatKind, SecretChatState, SendState};

use crate::app::App;
use crate::chat_list::ChatListView;
use crate::keymap::Focus;
use crate::ui::ChatRows;

use super::common::{SELECTED_SYMBOL, pane_block};

/// Left pane: the chat list (#80). Renders the active list's chats — each a title
/// with an unread badge — under a title naming the active list, with the selected
/// row highlighted. An empty list shows a placeholder. List switching and moving
/// the selection are driven through [`App`]'s reducer by the keymap.
pub(crate) fn render_chat_list(frame: &mut Frame, area: Rect, app: &App) -> ChatRows {
    let view = app.chat_list();
    let block = pane_block(
        format!(" Chats — {} ", view.active_label()),
        app.focus() == Focus::ChatList,
    );

    let chats = view.active_chats();
    if chats.is_empty() {
        frame.render_widget(Paragraph::new("(no chats yet)").block(block), area);
        return ChatRows::default();
    }

    let items: Vec<ListItem> = chats
        .iter()
        .map(|chat| chat_list_item(view, chat))
        .collect();
    let list = List::new(items)
        .block(block)
        .highlight_symbol(SELECTED_SYMBOL)
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED));

    // A fresh `ListState` each frame: the selection comes from `App`, and ratatui
    // scrolls the offset to keep it visible — so long lists window themselves
    // without the (immutable) render path holding mutable scroll state.
    let mut state = ListState::default().with_selected(Some(view.selected()));
    frame.render_stateful_widget(list, area, &mut state);

    // Row → chat id map: `render_stateful_widget` above settles `state`'s offset
    // to whatever it actually scrolled to, so reading it back here can never
    // drift from what was drawn.
    let top = area.y + 1;
    let visible_rows = area.height.saturating_sub(2) as usize;
    let rows = chats
        .iter()
        .enumerate()
        .skip(state.offset())
        .take(visible_rows)
        .map(|(i, chat)| (top + (i - state.offset()) as u16, chat.id))
        .collect();
    ChatRows(rows)
}

/// One chat row: the title, plus a bold unread badge when the chat has unread
/// incoming messages. Used by the forward target picker, which lists plain chats;
/// the chat-list pane uses [`chat_list_item`], which also draws the #87 markers.
pub(super) fn chat_item(chat: &Chat) -> ListItem<'static> {
    let mut spans = vec![Span::raw(chat.title.clone())];
    if chat.unread_count > 0 {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("({})", chat.unread_count),
            Style::new().add_modifier(Modifier::BOLD),
        ));
    }
    ListItem::new(Line::from(spans))
}

/// The leading row marker for a chat, mimicking the official app's chat-type icons
/// (#160): a secret chat's 🔒, 👥 for a group (basic or super), 📣 for a channel, and
/// 🤖 for a private chat whose peer is a bot (resolved per chat id from the user
/// store). Ordinary private chats and Saved Messages get no marker (`None`), the way
/// the app leaves person-to-person chats unadorned. Secret takes precedence over the
/// private-bot check since a secret chat is its own kind.
fn chat_marker(kind: &ChatKind, is_bot: bool) -> Option<&'static str> {
    match kind {
        ChatKind::Secret { .. } => Some("🔒"),
        ChatKind::BasicGroup { .. } | ChatKind::Supergroup { .. } => Some("👥"),
        ChatKind::Channel { .. } => Some("📣"),
        ChatKind::Private { .. } if is_bot => Some("🤖"),
        ChatKind::Private { .. } => None,
    }
}

/// One chat-list row (#80, extended in #87 and #160): a leading chat-type marker
/// (secret 🔒, group 👥, channel 📣, bot 🤖 — see [`chat_marker`]), the title, the
/// unread badge, a secret-chat lifecycle word, and a transient "typing…" indicator
/// when someone is acting in the chat. The lifecycle state, the action, and the
/// private-bot flag are projected per chat id from the core stores (Phase 6); no
/// encryption-key material is ever read or shown — only the [`SecretChatState`].
fn chat_list_item(view: &ChatListView, chat: &Chat) -> ListItem<'static> {
    let dim = Style::new().add_modifier(Modifier::DIM);
    let mut spans = Vec::new();
    if let Some(marker) = chat_marker(&chat.kind, view.is_bot_chat(chat.id)) {
        spans.push(Span::raw(format!("{marker} ")));
    }
    spans.push(Span::raw(chat.title.clone()));
    // Delivery status of our own last message (#165), reusing #163's glyph
    // helper — no preview text, just the checkmark/hourglass/cross real chat
    // clients show so "did they read it" is visible without opening the chat.
    if let Some(last) = &chat.last_message
        && last.is_outgoing
    {
        spans.push(Span::raw("  "));
        spans.push(Span::raw(delivery_glyph(
            &last.send_state,
            last.id,
            chat.last_read_outbox_message_id,
        )));
    }
    if chat.unread_count > 0 {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("({})", chat.unread_count),
            Style::new().add_modifier(Modifier::BOLD),
        ));
    }
    if let Some(state) = view.secret_state(chat.id) {
        spans.push(Span::styled(format!("  · {}", secret_suffix(state)), dim));
    }
    if let Some(action) = view.action(chat.id) {
        spans.push(Span::styled(format!("  {}", action_phrase(action)), dim));
    }
    ListItem::new(Line::from(spans))
}

/// The delivery-status glyph for one of our own outgoing messages (#163, #165):
/// `⌛` while the send is still in flight, `✗` if the server rejected it, `✓✓` once
/// the peer's outbox watermark has passed it (read), else a plain `✓` (sent, not
/// yet read). Shared by the conversation header ([`message_lines`]) and the
/// chat-list's last-message line ([`chat_list_item`]) so both read the same
/// mapping from one source of truth.
pub(super) fn delivery_glyph(
    send_state: &SendState,
    message_id: i64,
    last_read_outbox: i64,
) -> &'static str {
    match send_state {
        SendState::Failed { .. } => "✗",
        SendState::Pending => "⌛",
        SendState::Sent if message_id <= last_read_outbox => "✓✓",
        SendState::Sent => "✓",
    }
}

/// The lifecycle word shown after a secret chat's title (#87).
fn secret_suffix(state: SecretChatState) -> &'static str {
    match state {
        SecretChatState::Pending => "pending",
        SecretChatState::Ready => "ready",
        SecretChatState::Closed => "closed",
    }
}

/// The phrase for a transient chat action (#87) — the "X is typing…" text, shown
/// in the chat-list row and the conversation header. Total over [`ChatAction`] with
/// no catch-all, mirroring the core projection: a new activity fails to compile here
/// until it is given a phrase.
pub(super) fn action_phrase(action: &ChatAction) -> &'static str {
    match action {
        ChatAction::Typing => "typing…",
        ChatAction::RecordingVideo => "recording video…",
        ChatAction::UploadingVideo => "sending a video…",
        ChatAction::RecordingVoiceNote => "recording a voice message…",
        ChatAction::UploadingVoiceNote => "sending a voice message…",
        ChatAction::UploadingPhoto => "sending a photo…",
        ChatAction::UploadingDocument => "sending a file…",
        ChatAction::ChoosingSticker => "choosing a sticker…",
        ChatAction::ChoosingLocation => "choosing a location…",
        ChatAction::ChoosingContact => "choosing a contact…",
        ChatAction::StartPlayingGame => "playing a game…",
        ChatAction::RecordingVideoNote => "recording a video message…",
        ChatAction::UploadingVideoNote => "sending a video message…",
        ChatAction::WatchingAnimations => "watching animations…",
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // tests: panicking on a broken assumption is the point
mod tests {
    use std::collections::HashSet;

    use tuigram_core::model::ChatListKind;

    use crate::app::Action;
    use crate::chat_list::{ChatList, sample_chat};

    use super::super::test_support::{
        app_with_lists, chat_list, flatten, render, render_output, row_containing,
        view_with_one_chat,
    };
    use super::*;

    #[test]
    fn empty_chat_list_shows_the_placeholder_and_active_label() {
        let text = flatten(&render(&App::new(), 80, 24));
        assert!(text.contains("Chats — Main"), "active list label");
        assert!(text.contains("(no chats yet)"), "empty placeholder");
    }

    #[test]
    fn chat_titles_render_in_the_list() {
        let text = flatten(&render(&app_with_lists(), 80, 24));
        assert!(text.contains("Alice"));
        assert!(text.contains("Bob"));
        assert!(text.contains("Carol"));
    }

    #[test]
    fn unread_chats_show_a_badge_count() {
        let buffer = render(&app_with_lists(), 80, 24);
        // Bob has 3 unread; the badge sits on Bob's row, read chats carry none.
        assert!(
            row_containing(&buffer, "Bob").contains("(3)"),
            "unread badge"
        );
        assert!(
            !row_containing(&buffer, "Alice").contains('('),
            "no badge if read"
        );
    }

    #[test]
    fn the_chat_list_shows_our_last_message_s_delivery_glyph() {
        // #165: reuses #163's glyph helper — no preview text, just the checkmark
        // on the row when the chat's last message is ours.
        use crate::conversation::sample_message;
        use tuigram_core::model::{FormattedText, MessageContent, SendState};

        let outgoing = |id: i64, state: SendState| {
            let mut m = sample_message(
                id,
                MessageContent::Text(FormattedText {
                    text: "hi".to_owned(),
                    entities: Vec::new(),
                }),
            );
            m.is_outgoing = true;
            m.send_state = state;
            m
        };
        let mut read = sample_chat(5, "Read", 0);
        read.last_message = Some(outgoing(1, SendState::Sent));
        read.last_read_outbox_message_id = 1;
        let mut pending = sample_chat(6, "Pending", 0);
        pending.last_message = Some(outgoing(2, SendState::Pending));

        let view = ChatListView::from_lists(vec![ChatList {
            kind: ChatListKind::Main,
            label: "Main".to_owned(),
            chats: vec![read, pending],
        }]);
        let app = App::with_chat_list(view);
        let buffer = render(&app, 80, 24);
        assert!(row_containing(&buffer, "Read").contains("✓✓"));
        assert!(row_containing(&buffer, "Pending").contains('⌛'));
    }

    #[test]
    fn an_incoming_last_message_shows_no_delivery_glyph() {
        use crate::conversation::sample_message;
        use tuigram_core::model::{FormattedText, MessageContent};

        let mut chat = sample_chat(5, "Alice", 0);
        chat.last_message = Some(sample_message(
            1,
            MessageContent::Text(FormattedText {
                text: "hi".to_owned(),
                entities: Vec::new(),
            }),
        ));
        let view = ChatListView::from_lists(vec![ChatList {
            kind: ChatListKind::Main,
            label: "Main".to_owned(),
            chats: vec![chat],
        }]);
        let text = flatten(&render(&App::with_chat_list(view), 80, 24));
        assert!(!text.contains('⌛') && !text.contains('✓') && !text.contains('✗'));
    }

    #[test]
    fn the_selected_row_carries_the_highlight_marker() {
        let mut app = app_with_lists();
        // Move the selection onto the second row (Bob).
        app.dispatch(Action::SelectNext);
        let buffer = render(&app, 80, 24);
        assert!(
            row_containing(&buffer, "Bob").contains('▶'),
            "marker on the selected row"
        );
        assert!(
            !row_containing(&buffer, "Alice").contains('▶'),
            "no marker on unselected rows"
        );
    }

    #[test]
    fn switching_lists_repaints_the_other_list() {
        let mut app = app_with_lists();
        app.dispatch(Action::NextList);
        let text = flatten(&render(&app, 80, 24));
        // The pane now shows Archive and its chat, not the Main list's chats.
        assert!(text.contains("Chats — Archive"), "archive label");
        assert!(text.contains("Old Friend"), "archive chat");
        assert!(!text.contains("Alice"), "main chats gone");
    }

    #[test]
    fn chat_rows_follows_the_list_once_it_scrolls() {
        // More chats than an 80×24 frame fits, selection moved to the last one so
        // ratatui's `List` scrolls its offset — the case `ChatRows` reads
        // `state.offset()` back for. A stale (always-zero) offset would map every
        // row to the wrong chat once the list has scrolled.
        let titles: Vec<(&str, i32)> = vec![("Chat", 0); 30];
        let view = ChatListView::from_lists(vec![chat_list(ChatListKind::Main, "Main", &titles)]);
        let mut app = App::with_chat_list(view);
        for _ in 0..29 {
            app.dispatch(Action::SelectNext);
        }

        let output = render_output(&app, 80, 24);
        let top = output.panes.list.y + 1;
        let visible_rows = output.panes.list.height.saturating_sub(2);
        assert!(
            (top..top + visible_rows).any(|row| output.chat_rows.chat_at(row) == Some(29)),
            "the selected (last) chat scrolled into view"
        );
        assert!(
            (top..top + visible_rows).all(|row| output.chat_rows.chat_at(row) != Some(0)),
            "the first chat scrolled out of view"
        );
    }

    #[test]
    fn chat_rows_maps_each_visible_row_to_its_chat_id() {
        // Alice (id 0), Bob (id 1), Carol (id 2), one row each below the list's
        // top border — a click on a row should resolve to that row's chat, not
        // just focus the pane (extends #161/#162).
        let output = render_output(&app_with_lists(), 80, 24);
        let top = output.panes.list.y + 1;
        assert_eq!(output.chat_rows.chat_at(top), Some(0), "Alice's row");
        assert_eq!(output.chat_rows.chat_at(top + 1), Some(1), "Bob's row");
        assert_eq!(output.chat_rows.chat_at(top + 2), Some(2), "Carol's row");
        // Empty list space below the last chat, still inside the pane, is not a hit.
        assert_eq!(output.chat_rows.chat_at(top + 3), None);
    }

    #[test]
    fn a_secret_chat_shows_the_lock_marker_and_its_lifecycle_state() {
        let view = view_with_one_chat(
            "Mallory",
            ChatKind::Secret {
                secret_chat_id: 9,
                user_id: 7,
            },
            Some(SecretChatState::Pending),
        );
        let buffer = render(&App::with_chat_list(view), 120, 24);
        let row = row_containing(&buffer, "Mallory");
        assert!(row.contains('🔒'), "secret-chat marker");
        assert!(row.contains("pending"), "lifecycle state");
    }

    #[test]
    fn a_ready_secret_chat_renders_its_state_and_never_key_material() {
        // The view carries only the SecretChatState, never the secret chat's
        // key_hash, so a fingerprint can never reach the screen by construction.
        let view = view_with_one_chat(
            "Mallory",
            ChatKind::Secret {
                secret_chat_id: 9,
                user_id: 7,
            },
            Some(SecretChatState::Ready),
        );
        let text = flatten(&render(&App::with_chat_list(view), 120, 24));
        assert!(text.contains("ready"), "ready state shown");
    }

    #[test]
    fn chat_marker_maps_each_kind_to_its_app_icon() {
        assert_eq!(
            chat_marker(&ChatKind::BasicGroup { basic_group_id: 1 }, false),
            Some("👥")
        );
        assert_eq!(
            chat_marker(&ChatKind::Supergroup { supergroup_id: 1 }, false),
            Some("👥")
        );
        assert_eq!(
            chat_marker(&ChatKind::Channel { supergroup_id: 1 }, false),
            Some("📣")
        );
        // A private chat with a bot peer is 🤖; an ordinary private chat (or Saved
        // Messages) is unmarked, whatever the bot flag would say.
        assert_eq!(
            chat_marker(&ChatKind::Private { user_id: 5 }, true),
            Some("🤖")
        );
        assert_eq!(chat_marker(&ChatKind::Private { user_id: 5 }, false), None);
        assert_eq!(
            chat_marker(
                &ChatKind::Secret {
                    secret_chat_id: 9,
                    user_id: 7
                },
                false
            ),
            Some("🔒")
        );
    }

    #[test]
    fn a_group_row_shows_the_people_marker() {
        let view = view_with_one_chat(
            "Rustaceans",
            ChatKind::BasicGroup { basic_group_id: 3 },
            None,
        );
        let row = row_containing(&render(&App::with_chat_list(view), 120, 24), "Rustaceans");
        assert!(row.contains('👥'), "group marker");
    }

    #[test]
    fn a_channel_row_shows_the_megaphone_marker() {
        let view = view_with_one_chat("Rust Blog", ChatKind::Channel { supergroup_id: 3 }, None);
        let row = row_containing(&render(&App::with_chat_list(view), 120, 24), "Rust Blog");
        assert!(row.contains('📣'), "channel marker");
    }

    #[test]
    fn a_bot_chat_shows_the_robot_marker_while_a_user_chat_shows_none() {
        // Same private kind; only the projected bot flag differs.
        let mut bot = view_with_one_chat("Weather Bot", ChatKind::Private { user_id: 5 }, None);
        bot.set_bot_chats(HashSet::from([5]));
        let bot_row = row_containing(&render(&App::with_chat_list(bot), 120, 24), "Weather Bot");
        assert!(bot_row.contains('🤖'), "bot marker");

        let user = view_with_one_chat("Ada", ChatKind::Private { user_id: 5 }, None);
        let user_row = row_containing(&render(&App::with_chat_list(user), 120, 24), "Ada");
        assert!(
            !user_row.contains('🤖') && !user_row.contains('👥') && !user_row.contains('📣'),
            "an ordinary private chat is unmarked"
        );
    }

    #[test]
    fn a_typing_sender_shows_an_indicator_in_the_chat_list() {
        let mut view = view_with_one_chat("Alice", ChatKind::Private { user_id: 5 }, None);
        view.set_action(5, Some(ChatAction::Typing));
        let buffer = render(&App::with_chat_list(view), 120, 24);
        assert!(
            row_containing(&buffer, "Alice").contains("typing"),
            "chat-list typing indicator"
        );
    }
}
