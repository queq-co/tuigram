//! Shared `TDLib` fixture builders used by more than one domain submodule's
//! tests — e.g. [`chat`](super::chat)'s `Chat` test builds a last message via
//! [`td_message`], which otherwise belongs to [`message`](super::message).
//! Kept separate rather than duplicated so each fixture has one definition.

use tdlib_rs::enums::{
    MessageContent as TdMessageContent, MessageSender as TdMessageSender,
    MessageSendingState as TdMessageSendingState,
};
use tdlib_rs::types::{
    File as TdFile, FormattedText as TdFormattedTextT, Message as TdMessage,
    TextEntity as TdTextEntityT,
};

/// A `TDLib` `Message` with every field zeroed but the ones a test cares
/// about. Only `sender_id` and `content` are non-defaultable, so they (and a
/// few useful fields) are parameters; the rest are inert.
pub(super) fn td_message(
    id: i64,
    chat_id: i64,
    sender_id: TdMessageSender,
    content: TdMessageContent,
    sending_state: Option<TdMessageSendingState>,
    is_outgoing: bool,
) -> TdMessage {
    TdMessage {
        id,
        sender_id,
        chat_id,
        sending_state,
        scheduling_state: None,
        is_outgoing,
        is_pinned: false,
        is_from_offline: false,
        can_be_saved: false,
        has_timestamped_media: false,
        is_channel_post: false,
        is_paid_star_suggested_post: false,
        is_paid_ton_suggested_post: false,
        contains_unread_mention: false,
        date: 1_700_000_000,
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
        content,
        reply_markup: None,
    }
}

pub(super) fn td_text(body: &str, entities: Vec<TdTextEntityT>) -> TdMessageContent {
    TdMessageContent::MessageText(tdlib_rs::types::MessageText {
        text: TdFormattedTextT {
            text: body.to_owned(),
            entities,
        },
        link_preview: None,
        link_preview_options: None,
    })
}

/// A `TDLib` `File` is a deep record; tests only care about its id (what a
/// [`FileRef`] keeps), so build one with the rest zeroed.
pub(super) fn td_file(id: i32) -> TdFile {
    TdFile {
        id,
        ..Default::default()
    }
}
