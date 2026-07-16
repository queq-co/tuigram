//! Synthetic fixture generators for the #183 benchmark suite, shared by both
//! crates' `benches/` — and reused as-is by the later real-session profiling
//! exercise (#185) that issue's setup notes call out.
//!
//! `tuigram-core`'s stores only fold `TDLib` updates (`ChatStore`/`MessageStore`
//! have no bulk constructor — see their `reduce`), so the chat/folding fixtures
//! build minimal `TDLib` update values the same way the crate's own private
//! test helpers do (`chats.rs`, `model.rs`), just re-exposed here as a
//! reusable crate instead of being locked behind `#[cfg(test)]` in each module.

use tuigram_core::enums::{
    ChatAvailableReactions, ChatList as TdChatList, ChatType as TdChatType,
    MessageContent as TdMessageContent, MessageSender as TdMessageSender, Update,
};
use tuigram_core::types::{
    Chat as TdChat, ChatAvailableReactionsAll, ChatNotificationSettings, ChatPermissions,
    ChatPosition as TdChatPosition, ChatTypePrivate, FormattedText as TdFormattedText,
    Message as TdMessage, MessageSenderUser, MessageText, UpdateChatPosition, UpdateNewChat,
    UpdateNewMessage, VideoChat,
};
use tuigram_core::{ChatStore, FormattedText, Message, MessageContent, SendState, Sender};

/// A `TDLib` `Chat` with every field zeroed but id/title/type — mirrors the
/// shape of `tuigram_core::chats`'s own private test helper of the same name.
fn td_chat(id: i64, title: &str) -> TdChat {
    TdChat {
        id,
        r#type: TdChatType::Private(ChatTypePrivate { user_id: id }),
        title: title.to_owned(),
        photo: None,
        accent_color_id: 0,
        background_custom_emoji_id: 0,
        upgraded_gift_colors: None,
        profile_accent_color_id: 0,
        profile_background_custom_emoji_id: 0,
        permissions: ChatPermissions::default(),
        last_message: None,
        positions: vec![],
        chat_lists: vec![],
        message_sender_id: None,
        block_list: None,
        has_protected_content: false,
        is_translatable: false,
        is_marked_as_unread: false,
        view_as_topics: false,
        has_scheduled_messages: false,
        can_be_deleted_only_for_self: false,
        can_be_deleted_for_all_users: false,
        can_be_reported: false,
        default_disable_notification: false,
        unread_count: 0,
        last_read_inbox_message_id: 0,
        last_read_outbox_message_id: 0,
        unread_mention_count: 0,
        unread_reaction_count: 0,
        notification_settings: ChatNotificationSettings::default(),
        available_reactions: ChatAvailableReactions::All(ChatAvailableReactionsAll::default()),
        message_auto_delete_time: 0,
        emoji_status: None,
        background: None,
        theme: None,
        action_bar: None,
        business_bot_manage_bar: None,
        video_chat: VideoChat::default(),
        pending_join_requests: None,
        reply_markup_message_id: 0,
        draft_message: None,
        client_data: String::new(),
    }
}

fn new_chat_update(id: i64, title: &str) -> Update {
    Update::NewChat(UpdateNewChat {
        chat: td_chat(id, title),
    })
}

fn main_position_update(chat_id: i64, order: i64) -> Update {
    Update::ChatPosition(UpdateChatPosition {
        chat_id,
        position: TdChatPosition {
            list: TdChatList::Main,
            order,
            is_pinned: false,
            source: None,
        },
    })
}

/// A `TDLib` `Message` with every field zeroed but the ones a fixture cares
/// about: a plain-text body from `sender_user_id` at `date`. Mirrors the shape
/// of `tuigram_core::model`'s own private test helper of the same name.
fn td_message(id: i64, chat_id: i64, sender_user_id: i64, text: &str, date: i32) -> TdMessage {
    TdMessage {
        id,
        sender_id: TdMessageSender::User(MessageSenderUser {
            user_id: sender_user_id,
        }),
        chat_id,
        sending_state: None,
        scheduling_state: None,
        is_outgoing: false,
        is_pinned: false,
        is_from_offline: false,
        can_be_saved: false,
        has_timestamped_media: false,
        is_channel_post: false,
        is_paid_star_suggested_post: false,
        is_paid_ton_suggested_post: false,
        contains_unread_mention: false,
        date,
        edit_date: 0,
        forward_info: None,
        import_info: None,
        interaction_info: None,
        unread_reactions: vec![],
        fact_check: None,
        suggested_post_info: None,
        reply_to: None,
        topic_id: None,
        self_destruct_type: None,
        self_destruct_in: 0.0,
        auto_delete_in: 0.0,
        via_bot_user_id: 0,
        sender_business_bot_user_id: 0,
        sender_boost_count: 0,
        paid_message_star_count: 0,
        author_signature: String::new(),
        media_album_id: 0,
        effect_id: 0,
        restriction_info: None,
        summary_language_code: String::new(),
        content: TdMessageContent::MessageText(MessageText {
            text: TdFormattedText {
                text: text.to_owned(),
                entities: vec![],
            },
            link_preview: None,
            link_preview_options: None,
        }),
        reply_markup: None,
    }
}

fn new_message_update(id: i64, chat_id: i64, sender_user_id: i64, text: &str, date: i32) -> Update {
    Update::NewMessage(UpdateNewMessage {
        message: td_message(id, chat_id, sender_user_id, text, date),
    })
}

/// A folded [`ChatStore`] with `n` Main-list chats, ordered highest-first
/// (chat `0` sorts first) — built through `reduce`, the store's only
/// ingestion path, the same way a real session folds `updateNewChat` /
/// `updateChatPosition` off the wire.
#[must_use]
pub fn chat_store(n: usize) -> ChatStore {
    let mut store = ChatStore::new();
    for i in 0..n {
        let id = i as i64 + 1;
        store.reduce(&new_chat_update(id, &format!("Chat {i}")));
        store.reduce(&main_position_update(id, (n - i) as i64));
    }
    store
}

/// A mixed burst simulating "joined a busy group" (#183): `n_chats` chats
/// appearing on the Main list, each immediately getting `messages_per_chat`
/// live messages — the shape `ChatStore::reduce`/`MessageStore::reduce` fold
/// identically off the same real `Update` stream in the running app.
#[must_use]
pub fn busy_group_burst(n_chats: usize, messages_per_chat: usize) -> Vec<Update> {
    let mut updates = Vec::with_capacity(n_chats * (2 + messages_per_chat));
    for i in 0..n_chats {
        let chat_id = i as i64 + 1;
        updates.push(new_chat_update(chat_id, &format!("Chat {i}")));
        updates.push(main_position_update(chat_id, (n_chats - i) as i64));
        for m in 0..messages_per_chat {
            let message_id = (m as i64 + 1) * 1000;
            let date = 1_700_000_000 + m as i32;
            updates.push(new_message_update(
                message_id,
                chat_id,
                chat_id,
                &format!("message {m} in chat {i}"),
                date,
            ));
        }
    }
    updates
}

/// `n` plain-text [`Message`] values for `chat_id`, oldest first, cycling
/// through an ordinary line, an emoji-heavy line, and a CJK line so a
/// projection bench exercises the same text variety the wrapping bench does
/// (`ConversationView::project` re-measures each message's wrapped height).
#[must_use]
pub fn fake_messages(n: usize, chat_id: i64) -> Vec<Message> {
    (0..n)
        .map(|i| {
            let text = match i % 3 {
                0 => format!("Message number {i}: a short, ordinary line of chat text."),
                1 => format!("Message {i} 🎉🚀😄 with a run of emoji through the body 🔥✨🙌"),
                _ => format!("消息 {i}：这是一段中日韩宽字符组成的示例文本，用来撑满换行逻辑。"),
            };
            Message {
                id: i as i64 + 1,
                chat_id,
                sender: Sender::User((i % 7) as i64 + 1),
                date: 1_700_000_000 + i as i32,
                edit_date: 0,
                is_outgoing: i % 5 == 0,
                content: MessageContent::Text(FormattedText {
                    text,
                    entities: vec![],
                }),
                send_state: SendState::Sent,
                reactions: vec![],
                reply_to: None,
            }
        })
        .collect()
}
