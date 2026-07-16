//! Per-chat message history — paged backward from `TDLib` and kept current by
//! live updates, folded into one ordered, deduplicated view.
//!
//! A chat's messages reach tuigram two ways: **history** is *pulled* a page at a
//! time with `getChatHistory` (it returns the messages directly), while **live**
//! messages are *pushed* as `updateNewMessage`. Both land in the same
//! [`MessageStore`], keyed per chat by message id, so the two streams converge on
//! a single chronological view with no duplicates — a message seen live and then
//! re-fetched in a history page is the same entry, not two.
//!
//! Ordering is by message id, which `TDLib` assigns monotonically within a chat, so
//! id-ascending is chronological (oldest first). A `BTreeMap` per chat gives that
//! ordering and the dedupe for free: re-inserting an id replaces in place.
//!
//! This module owns the message slice of the request surface — the same
//! per-domain segregation as [`ChatRequests`](crate::chats::ChatRequests) and
//! [`AuthRequests`](crate::auth::AuthRequests), but segmented one level further:
//! the seam is split into per-capability traits ([`HistoryRequests`],
//! [`SendRequests`], [`EditRequests`], …) so a consumer binds only what it uses
//! and a test double implements only what it exercises. [`MessageRequests`]
//! bundles them all for a caller that wants the whole surface. [`load_history`]
//! drives the backward paging; folding each page stays the caller's choice (so
//! production can fold under its lock per page, never across an await).
//!
//! Sending (#19) lives here too: [`SendRequests::send_text`] posts a text
//! message (optionally a reply) and `TDLib` creates it optimistically with a
//! temporary id in [`SendState::Pending`]; the reducer then folds the lifecycle —
//! `updateMessageSendSucceeded` swaps the temp id for the server's real one,
//! `updateMessageSendFailed` flips the same entry to [`SendState::Failed`] — so a
//! sent message appears at once and reconciles in place, never blocking on
//! delivery.
//!
//! Editing and deleting (#20) round out the write side:
//! [`EditRequests::edit_text`] replaces a message's text and
//! [`DeleteRequests::delete`] removes messages (for self or, with `revoke`, for
//! everyone). The reducer reconciles both: `updateMessageContent` swaps a known
//! message's content in place, and a permanent `updateDeleteMessages` drops the
//! messages — a cache-eviction delete is ignored so our copy survives.
//!
//! Read state (#21): [`ReadRequests::view_messages`] marks a chat's messages
//! read. It is advisory — the call acknowledges the messages to the server and
//! never blocks the read path; the resulting unread-count change arrives as
//! `updateChatReadInbox`, which the [chat store](crate::chats::ChatStore) folds.
//!
//! Search (#37): [`SearchRequests::search_chat_messages`] looks within one chat
//! and [`SearchRequests::search_messages`] across the whole account. Both return
//! normalized hits with a paging cursor as a [`SearchPage`], and the caller
//! collects them into a [`SearchResults`] — a transient, id-deduplicated view
//! that never folds into [`MessageStore`], so a search leaves loaded history
//! untouched. [`search_chat`] and [`search_global`] drive either search to
//! exhaustion into that view, the search counterpart to [`load_history`].
//!
//! Reactions and pins (#51): [`ReactionRequests::add_message_reaction`] /
//! [`ReactionRequests::remove_message_reaction`] react to a message, and
//! [`PinRequests::pin_chat_message`] / [`PinRequests::unpin_chat_message`]
//! pin it. Both are advisory, like the read path: a reaction's new counts arrive
//! as `updateMessageInteractionInfo`, which this store folds onto the message
//! (replacing its reaction buckets in place); a pin's `updateMessageIsPinned`
//! is chat state, folded by the [chat store](crate::chats::ChatStore) onto
//! [`Chat::pinned_message_ids`](crate::model::Chat::pinned_message_ids), not here.
//!
//! Scope: history paging, live `updateNewMessage`, sending text + reply with its
//! lifecycle (#19), editing and deleting with their updates (#20), marking
//! messages read (#21), forwarding (#36), in-chat and global search (#37),
//! reactions and pins (#51), and the snapshot.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;

use tdlib_rs::enums::{
    FoundChatMessages, FoundMessages, InputMessageContent, InputMessageReplyTo, MessageSource,
    Messages, ReactionType, TextParseMode, Update,
};
use tdlib_rs::types::{
    Error as TdError, InputMessageReplyToMessage, InputMessageText, ReactionTypeEmoji,
    TextParseModeMarkdown,
};

use crate::bridge::Bridge;
use crate::model::{
    FormattedText, Message, MessageContent, OutgoingMedia, Reaction, SendState, Sender,
};

// The message request seam — tuigram's message slice of the
// `tdlib_rs::functions` surface, segregated from the auth and chat requests.
//
// Rather than one monolithic trait, the seam is split into per-capability
// request traits (history, send, edit, …). A consumer binds only the capability
// it uses (`load_history` needs [`HistoryRequests`], not the rest), and a test
// double implements only the capability it exercises — so a new method lands in
// one focused trait and touches one spy, not all of them. [`Bridge`] implements
// every capability over a live `tdjson` client (via [`Bridge::id`]); the
// [`MessageRequests`] aggregate bundles them for a caller that wants the whole
// surface and keeps `Bridge` provably complete.
//
// Logic written against these traits runs unchanged on the live bridge or a
// spy, with no network and no live `tdjson`. Every consumer is in-crate and
// generic over the capability it needs, so the lack of a caller-controllable
// `Send` bound (the reason `async_fn_in_trait` fires) is not a concern on any
// of them.

/// Read a chat's message history, page by page.
#[allow(async_fn_in_trait)]
pub trait HistoryRequests {
    /// Fetch up to `limit` messages of a chat's history, older than the anchor
    /// `from_message_id` ([`NEWEST`] for the most recent). Returns the page
    /// projected to [`Message`]s (`TDLib`'s null entries dropped); an **empty**
    /// page means the chat's beginning was reached.
    async fn get_chat_history(
        &self,
        chat_id: i64,
        from_message_id: i64,
        limit: i32,
    ) -> Result<Vec<Message>, TdError>;
}

/// Send new messages — plain text and file-backed media.
#[allow(async_fn_in_trait)]
pub trait SendRequests {
    /// Send `text` to a chat, optionally replying to `reply_to` (a message id in
    /// the same chat; `None` for a plain message). Returns the message `TDLib`
    /// creates **optimistically** — a temporary id, [`SendState::Pending`] —
    /// which the lifecycle updates later reconcile in the store. Returns as soon
    /// as `TDLib` accepts the request; it never waits for delivery.
    async fn send_text(
        &self,
        chat_id: i64,
        reply_to: Option<i64>,
        text: FormattedText,
    ) -> Result<Message, TdError>;

    /// Send a file-backed media message (photo, video, document, audio, voice, or
    /// animation) from a local path, optionally replying to `reply_to` (a message
    /// id in the same chat). The [`OutgoingMedia`] carries the local path and an
    /// optional caption; `TDLib` uploads the file and measures its metadata.
    ///
    /// Returns the message `TDLib` creates **optimistically** — a temporary id,
    /// [`SendState::Pending`] — exactly like [`send_text`](Self::send_text). The
    /// upload then streams as `updateFile` (folded by the
    /// [`FileStore`](crate::files::FileStore)) and the send settles via
    /// `updateMessageSendSucceeded`/`updateMessageSendFailed` (folded by the
    /// [`MessageStore`]), so a caller observes progress and reconciliation through
    /// the router rather than awaiting this. It never waits for the upload.
    async fn send_media(
        &self,
        chat_id: i64,
        reply_to: Option<i64>,
        media: OutgoingMedia,
    ) -> Result<Message, TdError>;
}

/// Edit messages tuigram's account already sent.
#[allow(async_fn_in_trait)]
pub trait EditRequests {
    /// Replace the text of a message tuigram's account sent. Returns the edited
    /// [`Message`] `TDLib` produces once the edit lands server-side; the matching
    /// `updateMessageContent` reconciles the stored copy. Errors if the message
    /// is not editable (not own, too old, not a text message).
    async fn edit_text(
        &self,
        chat_id: i64,
        message_id: i64,
        text: FormattedText,
    ) -> Result<Message, TdError>;

    /// Replace the caption of a media message tuigram's account sent (an empty
    /// `caption` clears it). Returns the edited [`Message`] `TDLib` produces once the
    /// edit lands; the matching `updateMessageContent` reconciles the stored copy,
    /// the same fold as [`edit_text`](Self::edit_text). Errors if the message is
    /// not editable (not own, too old, not a captioned message).
    async fn edit_caption(
        &self,
        chat_id: i64,
        message_id: i64,
        caption: FormattedText,
    ) -> Result<Message, TdError>;
}

/// Parse composer text into formatting entities before it is sent (#212).
#[allow(async_fn_in_trait)]
pub trait FormatRequests {
    /// Parse `text` as markdown and send it through `TDLib`'s
    /// `parseTextEntities` with `TextParseMode::Markdown { version: 2 }`
    /// (Telegram's `MarkdownV2`: `*bold*`, `_italic_`, `__underline__`,
    /// `~strikethrough~`, `` `code` ``, ```` ```pre``` ````, `[text](url)`,
    /// `||spoiler||`). Implementations should first rewrite the common
    /// "doubled-marker" convention (`**bold**`, `~~strikethrough~~`, plain
    /// `*italic*`) into that syntax — see `to_markdown_v2` in this module —
    /// since `MarkdownV2` alone has no doubled-marker forms and a literal
    /// `**bold**` fails to parse. `MarkdownV2` still requires escaping reserved
    /// punctuation anywhere it appears, so ordinary prose containing
    /// unescaped punctuation can still error here — callers must treat that
    /// as expected and fall back to sending `text` plain
    /// ([`send_formatted_text`]/[`edit_formatted_text`] already do), never as
    /// a reason to block the send.
    async fn parse_markdown(&self, text: String) -> Result<FormattedText, TdError>;
}

/// Delete messages from a chat.
#[allow(async_fn_in_trait)]
pub trait DeleteRequests {
    /// Delete messages from a chat. With `revoke` true the messages are removed
    /// for **everyone** (revoke for all members); with it false they are removed
    /// only for tuigram's account. `TDLib` rejects a revoke it does not permit. The
    /// matching `updateDeleteMessages` removes them from the store.
    async fn delete(
        &self,
        chat_id: i64,
        message_ids: Vec<i64>,
        revoke: bool,
    ) -> Result<(), TdError>;
}

/// Acknowledge messages as read.
#[allow(async_fn_in_trait)]
pub trait ReadRequests {
    /// Mark `message_ids` in a chat as read (`TDLib`'s `viewMessages`). **Advisory**
    /// — it acknowledges the messages to the server and lets the unread count
    /// settle, but the read path never waits on the result: the new count returns
    /// asynchronously as `updateChatReadInbox`, folded by the chat store. An empty
    /// `message_ids` is a no-op at the seam.
    async fn view_messages(&self, chat_id: i64, message_ids: Vec<i64>) -> Result<(), TdError>;
}

/// Forward messages into another chat.
#[allow(async_fn_in_trait)]
pub trait ForwardRequests {
    /// Forward `message_ids` from `from_chat_id` into `to_chat_id`.
    ///
    /// `send_copy` forwards the messages as a fresh copy — no "forwarded from"
    /// attribution, the messages appear as if newly sent; with it false they carry
    /// the usual forward header naming the original sender. `remove_caption` drops
    /// any caption when copying (only meaningful with `send_copy`).
    ///
    /// Returns the messages `TDLib` creates **optimistically** in the target chat —
    /// temporary ids, [`SendState::Pending`] — exactly like
    /// [`send_text`](SendRequests::send_text). `TDLib` also streams each as
    /// `updateNewMessage`, so the store gains them through the router on the same
    /// lifecycle path as a normal send; these returned copies are for the caller's
    /// reference, not a second insert.
    async fn forward_messages(
        &self,
        from_chat_id: i64,
        message_ids: Vec<i64>,
        to_chat_id: i64,
        send_copy: bool,
        remove_caption: bool,
    ) -> Result<Vec<Message>, TdError>;
}

/// Search messages — within one chat or across the whole account.
#[allow(async_fn_in_trait)]
pub trait SearchRequests {
    /// Search one chat's messages for `query`, optionally restricted to messages
    /// from `sender`. Pages backward from `from_message_id` ([`NEWEST`] for the
    /// first page); each call returns up to `limit` hits.
    ///
    /// The returned [`SearchPage`] carries the normalized hits, an approximate
    /// total, and the cursor for the next page ([`SearchPage::next`] is `None` at
    /// the end). Results are a **transient view** — the caller folds them into a
    /// [`SearchResults`], never the live [`MessageStore`], so a search leaves the
    /// loaded history untouched.
    ///
    /// Media-type filtering (`TDLib`'s `SearchMessagesFilter`) is out of scope here
    /// — this searches all message types; filtering by content kind is a
    /// follow-up alongside the non-text content model.
    async fn search_chat_messages(
        &self,
        chat_id: i64,
        query: String,
        sender: Option<Sender>,
        from_message_id: i64,
        limit: i32,
    ) -> Result<SearchPage<i64>, TdError>;

    /// Search the whole account for `query` across all chats. Pages from
    /// `from_offset` (the empty string for the first page), returning up to
    /// `limit` hits.
    ///
    /// The returned [`SearchPage`] carries the normalized hits and the opaque
    /// string cursor for the next page ([`SearchPage::next`] is `None` when the
    /// offset comes back empty). As with the in-chat search, results are a
    /// transient view and never folded into the live store.
    async fn search_messages(
        &self,
        query: String,
        from_offset: String,
        limit: i32,
    ) -> Result<SearchPage<String>, TdError>;
}

/// Add or remove tuigram's account's emoji reaction on a message.
#[allow(async_fn_in_trait)]
pub trait ReactionRequests {
    /// Add tuigram's account's reaction to a message — a standard `emoji`
    /// (e.g. `"👍"`), `TDLib`'s `addMessageReaction`. **Advisory**, like
    /// [`view_messages`](ReadRequests::view_messages): it acknowledges the reaction to the
    /// server and never blocks; the resulting reaction counts arrive as
    /// `updateMessageInteractionInfo`, which the [`MessageStore`] folds onto the
    /// message. Only emoji reactions are sent here — custom-emoji and paid
    /// reactions ([`ReactionKind`](crate::model::ReactionKind)) are read-only in
    /// this model.
    async fn add_message_reaction(
        &self,
        chat_id: i64,
        message_id: i64,
        emoji: String,
    ) -> Result<(), TdError>;

    /// Remove tuigram's account's emoji reaction from a message, `TDLib`'s
    /// `removeMessageReaction`. The counterpart to
    /// [`add_message_reaction`](Self::add_message_reaction); the matching
    /// `updateMessageInteractionInfo` folds the new counts.
    async fn remove_message_reaction(
        &self,
        chat_id: i64,
        message_id: i64,
        emoji: String,
    ) -> Result<(), TdError>;
}

/// Pin and unpin a chat's messages.
#[allow(async_fn_in_trait)]
pub trait PinRequests {
    /// Pin a message in a chat, `TDLib`'s `pinChatMessage`. `disable_notification`
    /// pins silently; `only_for_self` pins only for tuigram's account (a private
    /// pin). The resulting `updateMessageIsPinned` folds the chat's pinned-message
    /// set ([`Chat::pinned_message_ids`](crate::model::Chat::pinned_message_ids)).
    async fn pin_chat_message(
        &self,
        chat_id: i64,
        message_id: i64,
        disable_notification: bool,
        only_for_self: bool,
    ) -> Result<(), TdError>;

    /// Unpin a message in a chat, `TDLib`'s `unpinChatMessage`. The counterpart to
    /// [`pin_chat_message`](Self::pin_chat_message); the matching
    /// `updateMessageIsPinned` (with `is_pinned` false) folds the message out of
    /// the chat's pinned set.
    async fn unpin_chat_message(&self, chat_id: i64, message_id: i64) -> Result<(), TdError>;
}

/// The full message request seam — every per-capability request trait in one
/// bound. A caller that genuinely needs the whole surface (or wants to assert a
/// driver is complete) binds this; everything else binds the narrow trait it
/// uses. The blanket impl makes any type that satisfies every capability a
/// `MessageRequests` automatically, so [`Bridge`] earns it by implementing the
/// parts — and the day a capability is added to this bundle, `Bridge` fails to
/// compile until it implements that part too.
pub trait MessageRequests:
    HistoryRequests
    + SendRequests
    + EditRequests
    + FormatRequests
    + DeleteRequests
    + ReadRequests
    + ForwardRequests
    + SearchRequests
    + ReactionRequests
    + PinRequests
{
}

impl<T> MessageRequests for T where
    T: HistoryRequests
        + SendRequests
        + EditRequests
        + FormatRequests
        + DeleteRequests
        + ReadRequests
        + ForwardRequests
        + SearchRequests
        + ReactionRequests
        + PinRequests
{
}

impl Bridge {
    /// Shared send plumbing: post `content` to `chat_id` with an optional reply
    /// target and return the optimistic message `TDLib` creates. The text and media
    /// send paths differ only in the `content` they build, so the `send_message`
    /// call — reply mapping, the defaulted topic/options, the `from_tdlib`
    /// projection — lives here once. `TDLib` also streams the message as
    /// `updateNewMessage`, so the store gains the `Pending` entry via the router;
    /// the returned copy is for the caller's reference (its temp id), not a second
    /// insert.
    async fn send_content(
        &self,
        chat_id: i64,
        reply_to: Option<i64>,
        content: InputMessageContent,
    ) -> Result<Message, TdError> {
        let reply_to = reply_to.map(|message_id| {
            InputMessageReplyTo::Message(InputMessageReplyToMessage {
                message_id,
                quote: None,
                checklist_task_id: 0,
            })
        });
        let tdlib_rs::enums::Message::Message(sent) =
            tdlib_rs::functions::send_message(chat_id, None, reply_to, None, content, self.id())
                .await?;
        Ok(Message::from_tdlib(&sent))
    }
}

impl HistoryRequests for Bridge {
    async fn get_chat_history(
        &self,
        chat_id: i64,
        from_message_id: i64,
        limit: i32,
    ) -> Result<Vec<Message>, TdError> {
        // offset 0: page strictly older than the anchor, no look-ahead.
        // only_local false: let TDLib fetch from the server when the local cache
        // runs out, so paging reaches the real start of history.
        let Messages::Messages(page) = tdlib_rs::functions::get_chat_history(
            chat_id,
            from_message_id,
            0,
            limit,
            false,
            self.id(),
        )
        .await?;
        Ok(page
            .messages
            .into_iter()
            .flatten()
            .map(|m| Message::from_tdlib(&m))
            .collect())
    }
}

impl SendRequests for Bridge {
    async fn send_text(
        &self,
        chat_id: i64,
        reply_to: Option<i64>,
        text: FormattedText,
    ) -> Result<Message, TdError> {
        let content = InputMessageContent::InputMessageText(InputMessageText {
            text: text.to_tdlib(),
            link_preview_options: None,
            clear_draft: true,
        });
        self.send_content(chat_id, reply_to, content).await
    }

    async fn send_media(
        &self,
        chat_id: i64,
        reply_to: Option<i64>,
        media: OutgoingMedia,
    ) -> Result<Message, TdError> {
        // The media variant builds the inputMessage* content (local file + caption,
        // metadata left for TDLib to measure); the send lifecycle is then identical
        // to text, so it shares send_content.
        self.send_content(chat_id, reply_to, media.to_tdlib()).await
    }
}

impl EditRequests for Bridge {
    async fn edit_text(
        &self,
        chat_id: i64,
        message_id: i64,
        text: FormattedText,
    ) -> Result<Message, TdError> {
        // clear_draft false: an edit must not touch the chat's compose draft.
        let content = InputMessageContent::InputMessageText(InputMessageText {
            text: text.to_tdlib(),
            link_preview_options: None,
            clear_draft: false,
        });
        let tdlib_rs::enums::Message::Message(edited) =
            tdlib_rs::functions::edit_message_text(chat_id, message_id, content, self.id()).await?;
        Ok(Message::from_tdlib(&edited))
    }

    async fn edit_caption(
        &self,
        chat_id: i64,
        message_id: i64,
        caption: FormattedText,
    ) -> Result<Message, TdError> {
        // None clears the caption; show_caption_above_media false keeps the caption
        // in its usual place below the media.
        let caption = (!caption.text.is_empty()).then(|| caption.to_tdlib());
        let tdlib_rs::enums::Message::Message(edited) = tdlib_rs::functions::edit_message_caption(
            chat_id,
            message_id,
            caption,
            false,
            self.id(),
        )
        .await?;
        Ok(Message::from_tdlib(&edited))
    }
}

impl FormatRequests for Bridge {
    async fn parse_markdown(&self, text: String) -> Result<FormattedText, TdError> {
        let tdlib_rs::enums::FormattedText::FormattedText(parsed) =
            tdlib_rs::functions::parse_text_entities(
                to_markdown_v2(&text),
                TextParseMode::Markdown(TextParseModeMarkdown { version: 2 }),
                self.id(),
            )
            .await?;
        Ok(FormattedText::from_tdlib(&parsed))
    }
}

/// Length of the run of `marker` starting at `chars[start]` (0 if
/// `chars[start]` isn't `marker`).
fn run_length(chars: &[char], start: usize, marker: char) -> usize {
    chars[start..].iter().take_while(|&&c| c == marker).count()
}

/// Index of the next run of exactly `len` consecutive `marker` chars at or
/// after `start`, skipping over shorter/longer runs (a code span's closing
/// backtick fence must match the opening fence's length exactly, per
/// `CommonMark`).
fn find_exact_run(chars: &[char], start: usize, marker: char, len: usize) -> Option<usize> {
    let mut i = start;
    while i < chars.len() {
        if chars[i] == marker {
            let n = run_length(chars, i, marker);
            if n == len {
                return Some(i);
            }
            i += n;
        } else {
            i += 1;
        }
    }
    None
}

/// Rewrite the common "doubled-marker" markdown convention (`**bold**`,
/// `~~strikethrough~~`, plain `*italic*`) into the `MarkdownV2` syntax `TDLib`'s
/// `parseTextEntities` actually expects (`*bold*`, `~strikethrough~`,
/// `_italic_`). Users commonly type the GitHub/Discord-style convention
/// double-`*` for bold, single-`*` for italic — but `MarkdownV2` has no
/// doubled-marker forms and reserves single `*` for bold, so a literal
/// `**bold**` fails to parse (two adjacent `*` close an empty entity) and
/// the whole send falls back to plain text (see [`FormatRequests::
/// parse_markdown`]).
///
/// `_italic_`, `__underline__`, `||spoiler||`, code spans/blocks, and links
/// are already identical between the two conventions, so only `*`/`**` and
/// `~`/`~~` runs need remapping; everything else passes through unchanged.
/// A run of 3+ consecutive markers (e.g. `***text***`) is treated the same
/// as a run of 2 (bold/strikethrough) — nested bold+italic via a tripled
/// marker isn't supported, the same as plain `MarkdownV2` today.
///
/// Code spans (`` `...` ``) and pre blocks (```` ```...``` ````) are copied
/// through verbatim, however long their backtick run, so markers inside
/// code are never rewritten or treated as entity delimiters. A
/// backslash-escaped char (`MarkdownV2`'s own escape, e.g. `\*` for a literal
/// asterisk) is likewise copied through untouched, so an explicit escape
/// still suppresses formatting after this rewrite.
fn to_markdown_v2(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '`' => {
                let fence_len = run_length(&chars, i, '`');
                let fence: String = chars[i..i + fence_len].iter().collect();
                out.push_str(&fence);
                i += fence_len;
                // Unmatched fence falls through with nothing more to skip —
                // let TDLib's own parser handle (and likely reject) it.
                if let Some(close) = find_exact_run(&chars, i, '`', fence_len) {
                    out.extend(&chars[i..close]);
                    out.push_str(&fence);
                    i = close + fence_len;
                }
            }
            '*' => {
                let run = run_length(&chars, i, '*');
                out.push(if run >= 2 { '*' } else { '_' });
                i += run;
            }
            '~' => {
                let run = run_length(&chars, i, '~');
                out.push('~');
                i += run;
            }
            '\\' => {
                // A backslash-escaped char (MarkdownV2's own escape, e.g.
                // `\*` for a literal asterisk) is copied verbatim, both
                // chars at once, so the escaped marker never reaches the
                // `*`/`~` arms above and gets reinterpreted as formatting.
                out.push('\\');
                i += 1;
                if let Some(&escaped) = chars.get(i) {
                    out.push(escaped);
                    i += 1;
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// Parse `text` as markdown ([`FormatRequests::parse_markdown`]) and send it,
/// optionally as a reply to `reply_to`. A parse failure — expected for
/// ordinary prose containing unescaped `MarkdownV2` punctuation, not just
/// malformed input — must never block the send, so it falls back to sending
/// `text` plain, with no entities, exactly as an unparsed composer send did
/// before #212.
///
/// # Errors
///
/// Returns an error if `TDLib` rejects the send.
pub async fn send_formatted_text<C: FormatRequests + SendRequests>(
    client: &C,
    chat_id: i64,
    reply_to: Option<i64>,
    text: String,
) -> Result<Message, TdError> {
    let formatted = client
        .parse_markdown(text.clone())
        .await
        .unwrap_or(FormattedText {
            text,
            entities: Vec::new(),
        });
    client.send_text(chat_id, reply_to, formatted).await
}

/// Parse `text` as markdown and edit a message's text with it — the edit
/// counterpart to [`send_formatted_text`], with the same parse-failure
/// fallback to plain text.
///
/// # Errors
///
/// Returns an error if `TDLib` rejects the edit.
pub async fn edit_formatted_text<C: FormatRequests + EditRequests>(
    client: &C,
    chat_id: i64,
    message_id: i64,
    text: String,
) -> Result<Message, TdError> {
    let formatted = client
        .parse_markdown(text.clone())
        .await
        .unwrap_or(FormattedText {
            text,
            entities: Vec::new(),
        });
    client.edit_text(chat_id, message_id, formatted).await
}

impl DeleteRequests for Bridge {
    async fn delete(
        &self,
        chat_id: i64,
        message_ids: Vec<i64>,
        revoke: bool,
    ) -> Result<(), TdError> {
        tdlib_rs::functions::delete_messages(chat_id, message_ids, revoke, self.id()).await
    }
}

impl ReadRequests for Bridge {
    async fn view_messages(&self, chat_id: i64, message_ids: Vec<i64>) -> Result<(), TdError> {
        // `ChatHistory` source + `force_read`: a headless client is explicitly
        // marking a chat's history read, not reacting to messages drawn on screen,
        // so it must take effect without a visible message view.
        tdlib_rs::functions::view_messages(
            chat_id,
            message_ids,
            Some(MessageSource::ChatHistory),
            true,
            self.id(),
        )
        .await
    }
}

impl ForwardRequests for Bridge {
    async fn forward_messages(
        &self,
        from_chat_id: i64,
        message_ids: Vec<i64>,
        to_chat_id: i64,
        send_copy: bool,
        remove_caption: bool,
    ) -> Result<Vec<Message>, TdError> {
        // topic_id/options default; TDLib returns the optimistic forwarded
        // messages and also streams each as updateNewMessage, so the store gains
        // them via the router — these returned copies carry the temp ids for the
        // caller, not a second insert. `remove_caption` only bites with `send_copy`.
        let Messages::Messages(forwarded) = tdlib_rs::functions::forward_messages(
            to_chat_id,
            None,
            from_chat_id,
            message_ids,
            None,
            send_copy,
            remove_caption,
            self.id(),
        )
        .await?;
        Ok(forwarded
            .messages
            .into_iter()
            .flatten()
            .map(|m| Message::from_tdlib(&m))
            .collect())
    }
}

impl SearchRequests for Bridge {
    async fn search_chat_messages(
        &self,
        chat_id: i64,
        query: String,
        sender: Option<Sender>,
        from_message_id: i64,
        limit: i32,
    ) -> Result<SearchPage<i64>, TdError> {
        // topic_id None: search the whole chat, not one forum topic. offset 0: no
        // look-ahead past the anchor. filter None (Empty): all message types.
        let FoundChatMessages::FoundChatMessages(found) =
            tdlib_rs::functions::search_chat_messages(
                chat_id,
                None,
                query,
                sender.map(|s| s.to_tdlib()),
                from_message_id,
                0,
                limit,
                None,
                self.id(),
            )
            .await?;
        // next_from_message_id is 0 when the chat's matches are exhausted.
        let next = (found.next_from_message_id != 0).then_some(found.next_from_message_id);
        Ok(SearchPage {
            messages: found
                .messages
                .into_iter()
                .map(|m| Message::from_tdlib(&m))
                .collect(),
            total_count: found.total_count,
            next,
        })
    }

    async fn search_messages(
        &self,
        query: String,
        from_offset: String,
        limit: i32,
    ) -> Result<SearchPage<String>, TdError> {
        // chat_list None: the Main list. filter/chat_type_filter None, date bounds
        // 0: search every chat and message type with no time window.
        let FoundMessages::FoundMessages(found) = tdlib_rs::functions::search_messages(
            None,
            query,
            from_offset,
            limit,
            None,
            None,
            0,
            0,
            self.id(),
        )
        .await?;
        // next_offset is empty when there are no more results.
        let next = (!found.next_offset.is_empty()).then_some(found.next_offset);
        Ok(SearchPage {
            messages: found
                .messages
                .into_iter()
                .map(|m| Message::from_tdlib(&m))
                .collect(),
            total_count: found.total_count,
            next,
        })
    }
}

impl ReactionRequests for Bridge {
    async fn add_message_reaction(
        &self,
        chat_id: i64,
        message_id: i64,
        emoji: String,
    ) -> Result<(), TdError> {
        // is_big false: a normal (non-animated) reaction. update_recent_reactions
        // true: reacting promotes the emoji in the account's recent set, the usual
        // client behaviour.
        tdlib_rs::functions::add_message_reaction(
            chat_id,
            message_id,
            ReactionType::Emoji(ReactionTypeEmoji { emoji }),
            false,
            true,
            self.id(),
        )
        .await
    }

    async fn remove_message_reaction(
        &self,
        chat_id: i64,
        message_id: i64,
        emoji: String,
    ) -> Result<(), TdError> {
        tdlib_rs::functions::remove_message_reaction(
            chat_id,
            message_id,
            ReactionType::Emoji(ReactionTypeEmoji { emoji }),
            self.id(),
        )
        .await
    }
}

impl PinRequests for Bridge {
    async fn pin_chat_message(
        &self,
        chat_id: i64,
        message_id: i64,
        disable_notification: bool,
        only_for_self: bool,
    ) -> Result<(), TdError> {
        tdlib_rs::functions::pin_chat_message(
            chat_id,
            message_id,
            disable_notification,
            only_for_self,
            self.id(),
        )
        .await
    }

    async fn unpin_chat_message(&self, chat_id: i64, message_id: i64) -> Result<(), TdError> {
        tdlib_rs::functions::unpin_chat_message(chat_id, message_id, self.id()).await
    }
}

/// A page of message-search hits plus the cursor for the next page.
///
/// Generic over the cursor `C` because `TDLib` pages the two searches differently:
/// an in-chat search resumes from a message id (`C = i64`), a global search from
/// an opaque string offset (`C = String`). [`next`](Self::next) is `None` at the
/// end of results — the loop stop condition either way.
///
/// A search page is a **transient view**: its messages are normalized
/// [`Message`]s but they are never folded into the [`MessageStore`]; collect them
/// into a [`SearchResults`] instead, so a search never disturbs loaded history.
// Not `Eq`: a hit's [`Message`] may carry `f64` location coordinates.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchPage<C> {
    /// The normalized hits on this page, in the order `TDLib` ranked them.
    pub messages: Vec<Message>,
    /// Approximate total number of matches; `-1` when `TDLib` does not know.
    pub total_count: i32,
    /// Cursor for the next page, or `None` when results are exhausted.
    pub next: Option<C>,
}

/// A transient, deduplicated accumulation of search hits across pages.
///
/// Search results are a view onto messages that mostly already live (or could
/// live) in the [`MessageStore`]; this keeps them **separate** so a search never
/// mutates loaded history. Hits are deduplicated by `(chat_id, message_id)` — so
/// overlapping pages, or a hit that also appears in the history already on
/// screen, collapse onto one entry — while preserving `TDLib`'s result ordering.
#[derive(Debug, Default)]
pub struct SearchResults {
    messages: Vec<Message>,
    seen: HashSet<(i64, i64)>,
}

impl SearchResults {
    /// An empty result set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a page of hits, dropping any whose `(chat_id, message_id)` was
    /// already collected. Order is preserved (the first occurrence wins), so
    /// re-appending an overlapping page — or [`extend`](Self::extend)ing with a
    /// hit already on screen — is idempotent.
    pub fn extend(&mut self, page: impl IntoIterator<Item = Message>) {
        for message in page {
            if self.seen.insert((message.chat_id, message.id)) {
                self.messages.push(message);
            }
        }
    }

    /// The collected hits, in result order.
    #[must_use]
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Whether a given message has already been collected — the dedupe a caller
    /// uses to avoid showing a hit twice when it also sits in loaded history.
    #[must_use]
    pub fn contains(&self, chat_id: i64, message_id: i64) -> bool {
        self.seen.contains(&(chat_id, message_id))
    }

    /// Number of distinct hits collected.
    #[must_use]
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    /// Whether no hits have been collected.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }
}

/// Anchor passed to [`HistoryRequests::get_chat_history`] to start from a chat's
/// most recent message. `TDLib` reads message id `0` as "the newest".
pub const NEWEST: i64 = 0;

/// Page a chat's history backward, from the newest message to the start, folding
/// each page through `fold`.
///
/// The next anchor is the oldest message id in the page just received, so each
/// request asks for the messages before it; paging stops when `TDLib` returns an
/// empty page. Folding is left to the caller — production folds into the shared
/// store under its lock per page (never held across the awaits here), while a
/// test folds into a local [`MessageStore`]. Any request error is propagated.
///
/// # Errors
///
/// Returns an error if `TDLib` fails a page request.
///
/// # Panics
///
/// Never in practice: the internal `.expect()` is guarded by the preceding
/// empty-batch check, so it only runs on a non-empty page.
pub async fn load_history<C, F>(
    client: &C,
    chat_id: i64,
    page: i32,
    mut fold: F,
) -> Result<(), TdError>
where
    C: HistoryRequests,
    F: FnMut(Vec<Message>),
{
    let mut anchor = NEWEST;
    loop {
        let batch = client.get_chat_history(chat_id, anchor, page).await?;
        if batch.is_empty() {
            return Ok(());
        }
        // Page strictly older than the oldest message we just saw.
        anchor = batch
            .iter()
            .map(|m| m.id)
            .min()
            .expect("batch is non-empty");
        fold(batch);
    }
}

/// Page an in-chat search to exhaustion, collecting every hit into a transient
/// [`SearchResults`].
///
/// Mirrors [`load_history`]'s paging, but for search and with one deliberate
/// difference: the hits are **never** folded into a [`MessageStore`]. Each call
/// resumes from the previous page's [`SearchPage::next`] cursor (the first from
/// [`NEWEST`]) and stops when the cursor comes back `None`; the hits accumulate —
/// deduplicated by `(chat_id, message_id)` — in the returned [`SearchResults`], so
/// a search leaves loaded history untouched. Any request error is propagated.
///
/// # Errors
///
/// Returns an error if `TDLib` fails a page of the search.
pub async fn search_chat<C>(
    client: &C,
    chat_id: i64,
    query: String,
    sender: Option<Sender>,
    page: i32,
) -> Result<SearchResults, TdError>
where
    C: SearchRequests,
{
    let mut results = SearchResults::new();
    let mut anchor = NEWEST;
    loop {
        let hits = client
            .search_chat_messages(chat_id, query.clone(), sender.clone(), anchor, page)
            .await?;
        let next = hits.next;
        results.extend(hits.messages);
        match next {
            // Page strictly older than the last hit of this page.
            Some(cursor) => anchor = cursor,
            None => return Ok(results),
        }
    }
}

/// Page a global (whole-account) search to exhaustion, collecting every hit into
/// a transient [`SearchResults`].
///
/// The account-wide counterpart to [`search_chat`]: it resumes from the opaque
/// string offset `TDLib` returns (the first page from the empty string) and stops
/// when [`SearchPage::next`] comes back `None`. Hits across different chats stay
/// distinct — the dedupe keys on `(chat_id, message_id)` — and, as with the
/// in-chat search, never fold into the live [`MessageStore`]. Errors propagate.
///
/// # Errors
///
/// Returns an error if `TDLib` fails a page of the search.
pub async fn search_global<C>(
    client: &C,
    query: String,
    page: i32,
) -> Result<SearchResults, TdError>
where
    C: SearchRequests,
{
    let mut results = SearchResults::new();
    let mut offset = String::new();
    loop {
        let hits = client.search_messages(query.clone(), offset, page).await?;
        let next = hits.next;
        results.extend(hits.messages);
        match next {
            Some(cursor) => offset = cursor,
            None => return Ok(results),
        }
    }
}

/// Every known message, grouped by chat and ordered chronologically within each.
#[derive(Debug, Default)]
pub struct MessageStore {
    by_chat: HashMap<i64, BTreeMap<i64, Message>>,
}

impl MessageStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one message-route update into the store.
    ///
    /// - `updateNewMessage` — a live (or optimistically sent) message; inserted.
    /// - `updateMessageSendSucceeded` — the send was accepted; the temporary
    ///   entry is dropped and the server's message (real id) inserted in its
    ///   place, so the message keeps its spot but gains its final id.
    /// - `updateMessageSendFailed` — the send was rejected; the same entry (it
    ///   keeps its temporary id) flips to [`SendState::Failed`] with the cause.
    /// - `updateMessageContent` — an edit; the known message's content is swapped
    ///   in place (unknown message: ignored).
    /// - `updateMessageInteractionInfo` — a reaction change (#51); the known
    ///   message's reactions are replaced in place with the update's buckets, or
    ///   cleared when it carries none (unknown message: ignored).
    /// - `updateDeleteMessages` — a deletion; when permanent, the messages are
    ///   removed. A cache-eviction delete (`is_permanent` false) is ignored.
    ///
    /// Every arm is idempotent: re-applying converges (a reconcile whose temp
    /// entry is already gone just re-inserts the real message; a failure re-marks
    /// in place; a re-edit re-sets the same content; a re-delete of an absent id
    /// is a no-op).
    pub fn reduce(&mut self, update: &Update) {
        match update {
            Update::NewMessage(u) => self.insert(Message::from_tdlib(&u.message)),
            Update::MessageSendSucceeded(u) => {
                let message = Message::from_tdlib(&u.message);
                // The temp message lived in the same chat under the old id.
                self.remove(message.chat_id, u.old_message_id);
                self.insert(message);
            }
            Update::MessageSendFailed(u) => {
                // The failed message keeps its temporary id; flip it in place and
                // carry TDLib's error so callers can surface and retry it.
                let mut message = Message::from_tdlib(&u.message);
                message.send_state = SendState::Failed {
                    code: u.error.code,
                    message: u.error.message.clone(),
                };
                self.insert(message);
            }
            Update::MessageContent(u) => {
                // An edit: swap the known message's content in place.
                self.edit_content(
                    u.chat_id,
                    u.message_id,
                    MessageContent::from_tdlib(&u.new_content),
                );
            }
            Update::MessageInteractionInfo(u) => {
                // A reaction change: replace the known message's reactions with
                // this update's buckets (empty when the info or its reactions are
                // absent — i.e. the last reaction was removed).
                self.set_reactions(
                    u.chat_id,
                    u.message_id,
                    crate::model::reactions_from(u.interaction_info.as_ref()),
                );
            }
            // A real deletion removes the messages; a cache-eviction delete
            // (`is_permanent` false — TDLib unloading its own cache) leaves our
            // copy intact, so only the permanent case folds here.
            Update::DeleteMessages(u) if u.is_permanent => {
                for &message_id in &u.message_ids {
                    self.remove(u.chat_id, message_id);
                }
            }
            _ => {}
        }
    }

    /// Merge a history page (or any batch of messages) into the store. Each
    /// message is filed under its own chat and id, so re-merging an overlapping
    /// page is idempotent — duplicates collapse onto the same entry.
    ///
    /// A re-fetched page **replaces** an already-known message rather than
    /// skipping it: `getChatHistory` is server-authoritative, and is the one
    /// documented recovery path (#207) for a message whose reactions or content
    /// changed while its chat was closed and so never arrived as a live update —
    /// opening the chat and re-paging its history is what catches those up mid
    /// session, exactly as a restart does. A live fold ([`reduce`](Self::reduce))
    /// racing a page fetch for the same id is a narrower, separate concern than
    /// disabling that recovery path is worth trading away here.
    pub fn merge(&mut self, messages: impl IntoIterator<Item = Message>) {
        for message in messages {
            self.insert(message);
        }
    }

    /// A chat's messages, oldest first. Empty if the chat is unknown.
    #[must_use]
    pub fn history(&self, chat_id: i64) -> Vec<&Message> {
        self.by_chat
            .get(&chat_id)
            .map(|m| m.values().collect())
            .unwrap_or_default()
    }

    /// Look up a single message within a chat.
    #[must_use]
    pub fn get(&self, chat_id: i64, message_id: i64) -> Option<&Message> {
        self.by_chat.get(&chat_id)?.get(&message_id)
    }

    /// Number of messages known for a chat.
    #[must_use]
    pub fn count(&self, chat_id: i64) -> usize {
        self.by_chat.get(&chat_id).map_or(0, BTreeMap::len)
    }

    /// Whether no messages are known for any chat.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_chat.values().all(BTreeMap::is_empty)
    }

    /// File a message under its chat and id, replacing any existing entry with
    /// the same id (the dedupe across the live and history streams).
    fn insert(&mut self, message: Message) {
        self.by_chat
            .entry(message.chat_id)
            .or_default()
            .insert(message.id, message);
    }

    /// Drop a message from a chat by id. A no-op if the chat or id is unknown, so
    /// reconciling an already-reconciled send — or replaying a delete — is
    /// idempotent.
    fn remove(&mut self, chat_id: i64, message_id: i64) {
        if let Some(chat) = self.by_chat.get_mut(&chat_id) {
            chat.remove(&message_id);
        }
    }

    /// Replace a known message's content in place (the `updateMessageContent`
    /// fold). A no-op if the message is unknown: `TDLib` only edits the content of
    /// a message it already delivered, and a content-only update carries no
    /// sender/date, so we never synthesize a partial entry from one.
    fn edit_content(&mut self, chat_id: i64, message_id: i64, content: MessageContent) {
        if let Some(message) = self
            .by_chat
            .get_mut(&chat_id)
            .and_then(|chat| chat.get_mut(&message_id))
        {
            message.content = content;
        }
    }

    /// Replace a known message's reactions in place (the
    /// `updateMessageInteractionInfo` fold). A no-op if the message is unknown —
    /// the update carries no sender/date/content, so we never synthesize a partial
    /// entry from one, the same rule as [`edit_content`](Self::edit_content).
    /// Idempotent: re-applying sets the same buckets; the empty list clears them.
    fn set_reactions(&mut self, chat_id: i64, message_id: i64, reactions: Vec<Reaction>) {
        if let Some(message) = self
            .by_chat
            .get_mut(&chat_id)
            .and_then(|chat| chat.get_mut(&message_id))
        {
            message.reactions = reactions;
        }
    }
}

#[cfg(test)]
mod markdown_v2_tests {
    use super::to_markdown_v2;

    #[test]
    fn doubled_asterisk_becomes_single_asterisk_bold() {
        assert_eq!(to_markdown_v2("**bold**"), "*bold*");
    }

    #[test]
    fn single_asterisk_becomes_underscore_italic() {
        assert_eq!(to_markdown_v2("*italic*"), "_italic_");
    }

    #[test]
    fn doubled_tilde_becomes_single_tilde_strikethrough() {
        assert_eq!(to_markdown_v2("~~strike~~"), "~strike~");
    }

    #[test]
    fn single_tilde_passes_through() {
        assert_eq!(to_markdown_v2("~strike~"), "~strike~");
    }

    #[test]
    fn underscore_forms_pass_through_unchanged() {
        assert_eq!(to_markdown_v2("_italic_"), "_italic_");
        assert_eq!(to_markdown_v2("__underline__"), "__underline__");
    }

    #[test]
    fn spoiler_code_and_links_pass_through_unchanged() {
        assert_eq!(to_markdown_v2("||spoiler||"), "||spoiler||");
        assert_eq!(to_markdown_v2("`code`"), "`code`");
        assert_eq!(
            to_markdown_v2("[text](https://example.com)"),
            "[text](https://example.com)"
        );
    }

    #[test]
    fn markers_inside_a_code_span_are_left_untouched() {
        assert_eq!(to_markdown_v2("`a**b~~c`"), "`a**b~~c`");
    }

    #[test]
    fn markers_inside_a_pre_block_are_left_untouched() {
        assert_eq!(
            to_markdown_v2("```\n**not bold**\n```"),
            "```\n**not bold**\n```"
        );
    }

    #[test]
    fn mixed_message_translates_each_run_independently() {
        assert_eq!(
            to_markdown_v2("**bold** and *italic* and ~~strike~~ and `code**not bold**`"),
            "*bold* and _italic_ and ~strike~ and `code**not bold**`"
        );
    }

    #[test]
    fn triple_run_degrades_to_the_doubled_meaning() {
        // A run of 3+ maps the same as a run of exactly 2 (bold), which
        // itself translates to a single MarkdownV2 `*` — so both a doubled
        // and a tripled run collapse to the same single-asterisk output.
        assert_eq!(to_markdown_v2("***text***"), "*text*");
        assert_eq!(to_markdown_v2("**text**"), "*text*");
    }

    #[test]
    fn unmatched_backtick_is_left_as_is() {
        assert_eq!(to_markdown_v2("a ` b"), "a ` b");
    }

    #[test]
    fn backslash_escaped_markers_are_left_untouched() {
        // `\*` is MarkdownV2's own escape for a literal asterisk — must
        // survive the rewrite unconverted, or the escape gets silently
        // broken (a lone `*` would otherwise become `_`).
        assert_eq!(to_markdown_v2(r"5 \* 3 = 15"), r"5 \* 3 = 15");
        assert_eq!(
            to_markdown_v2(r"\~not strikethrough\~"),
            r"\~not strikethrough\~"
        );
        assert_eq!(
            to_markdown_v2(r"trailing backslash\"),
            r"trailing backslash\"
        );
    }

    #[test]
    fn plain_prose_is_unaffected() {
        assert_eq!(
            to_markdown_v2("hello world, no markup here."),
            "hello world, no markup here."
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::OutgoingMedia;
    use crate::model::{MessageContent, Sender};
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use tdlib_rs::enums::MessageSendingState;
    use tdlib_rs::types::{
        FormattedText as TdFormattedText, MessagePhoto as TdMessagePhoto, MessageSenderUser,
        MessageSendingStatePending, MessageText, UpdateMessageContent, UpdateMessageSendFailed,
        UpdateMessageSendSucceeded, UpdateNewMessage,
    };

    /// A model message with a distinct text body, for asserting order and dedupe.
    fn msg(chat_id: i64, id: i64) -> Message {
        Message {
            id,
            chat_id,
            sender: Sender::User(1),
            date: 0,
            edit_date: 0,
            is_outgoing: false,
            content: MessageContent::Text(FormattedText {
                text: format!("m{id}"),
                entities: vec![],
            }),
            send_state: SendState::Sent,
            reactions: vec![],
            reply_to: None,
        }
    }

    /// A `TDLib` `Message` with every field zeroed but id/chat and a text body, for
    /// driving the live `updateNewMessage` reducer. `sending_state` lets a test
    /// build an optimistic (Pending) message for the send lifecycle.
    fn td_message(chat_id: i64, id: i64) -> tdlib_rs::types::Message {
        td_message_state(chat_id, id, None)
    }

    fn td_message_state(
        chat_id: i64,
        id: i64,
        sending_state: Option<MessageSendingState>,
    ) -> tdlib_rs::types::Message {
        tdlib_rs::types::Message {
            id,
            sender_id: tdlib_rs::enums::MessageSender::User(MessageSenderUser { user_id: 1 }),
            chat_id,
            sending_state,
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
            date: 0,
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
            content: tdlib_rs::enums::MessageContent::MessageText(MessageText {
                text: TdFormattedText {
                    text: format!("m{id}"),
                    entities: vec![],
                },
                link_preview: None,
                link_preview_options: None,
            }),
            reply_markup: None,
        }
    }

    fn new_message(chat_id: i64, id: i64) -> Update {
        Update::NewMessage(UpdateNewMessage {
            message: td_message(chat_id, id),
        })
    }

    /// An optimistically-sent message: a live `updateNewMessage` carrying a temp
    /// id and a Pending sending state, as `TDLib` emits right after `sendMessage`.
    fn pending_message(chat_id: i64, temp_id: i64) -> Update {
        Update::NewMessage(UpdateNewMessage {
            message: td_message_state(
                chat_id,
                temp_id,
                Some(MessageSendingState::Pending(
                    MessageSendingStatePending::default(),
                )),
            ),
        })
    }

    /// The server's acknowledgement: the temp id is replaced by `real_id`.
    fn send_succeeded(chat_id: i64, temp_id: i64, real_id: i64) -> Update {
        Update::MessageSendSucceeded(UpdateMessageSendSucceeded {
            message: td_message(chat_id, real_id),
            old_message_id: temp_id,
        })
    }

    /// A send rejection: the message keeps its temp id, with the error cause.
    fn send_failed(chat_id: i64, temp_id: i64, code: i32, message: &str) -> Update {
        Update::MessageSendFailed(UpdateMessageSendFailed {
            message: td_message(chat_id, temp_id),
            old_message_id: temp_id,
            error: TdError {
                code,
                message: message.to_owned(),
            },
        })
    }

    /// An edit: the message's content becomes `body`.
    fn message_content(chat_id: i64, id: i64, body: &str) -> Update {
        Update::MessageContent(tdlib_rs::types::UpdateMessageContent {
            chat_id,
            message_id: id,
            new_content: tdlib_rs::enums::MessageContent::MessageText(MessageText {
                text: TdFormattedText {
                    text: body.to_owned(),
                    entities: vec![],
                },
                link_preview: None,
                link_preview_options: None,
            }),
        })
    }

    /// A deletion of `ids` from a chat. `is_permanent` distinguishes a real
    /// delete from a cache eviction; `from_cache` is set on the latter.
    fn delete_messages(
        chat_id: i64,
        ids: Vec<i64>,
        is_permanent: bool,
        from_cache: bool,
    ) -> Update {
        Update::DeleteMessages(tdlib_rs::types::UpdateDeleteMessages {
            chat_id,
            message_ids: ids,
            is_permanent,
            from_cache,
        })
    }

    fn ids(messages: &[&Message]) -> Vec<i64> {
        messages.iter().map(|m| m.id).collect()
    }

    #[test]
    fn merge_orders_messages_chronologically_regardless_of_arrival() {
        let mut store = MessageStore::new();
        // Arrive newest-first (as a history page does); stored oldest-first.
        store.merge([msg(10, 30), msg(10, 10), msg(10, 20)]);

        assert_eq!(ids(&store.history(10)), vec![10, 20, 30]);
        assert_eq!(store.count(10), 3);
    }

    #[test]
    fn overlapping_pages_dedupe_by_id() {
        let mut store = MessageStore::new();
        store.merge([msg(10, 10), msg(10, 20)]);
        // A second page overlapping on id 20 must not double it.
        store.merge([msg(10, 20), msg(10, 30)]);

        assert_eq!(ids(&store.history(10)), vec![10, 20, 30]);
    }

    #[test]
    fn a_re_fetched_page_refreshes_reactions_missed_while_the_chat_was_closed() {
        // #207: opening a chat and re-paging its history is the documented
        // recovery path for reactions that changed while the chat was closed —
        // TDLib only guarantees live `updateMessageInteractionInfo` delivery for
        // an open chat, so a message reacted-to while closed can only catch up
        // through a fresh, server-authoritative `getChatHistory` page. `merge`
        // must let that page win over the stale (reaction-less) copy already
        // known from a live `updateNewMessage`.
        let mut store = MessageStore::new();
        store.merge([msg(10, 1), msg(10, 2)]);
        assert!(store.get(10, 2).unwrap().reactions.is_empty());

        // The chat is (re)opened; the landing page comes back with the reaction
        // that landed while it was closed.
        let mut reacted = msg(10, 2);
        reacted.reactions = vec![Reaction {
            kind: crate::model::ReactionKind::Emoji("👍".to_owned()),
            count: 1,
            is_chosen: true,
        }];
        store.merge([msg(10, 1), reacted]);

        assert!(
            !store.get(10, 2).unwrap().reactions.is_empty(),
            "a re-fetched page must refresh an already-known message"
        );
    }

    #[test]
    fn messages_are_partitioned_per_chat() {
        let mut store = MessageStore::new();
        store.merge([msg(10, 1), msg(20, 1), msg(10, 2)]);

        // Same id 1 in two chats are distinct entries.
        assert_eq!(ids(&store.history(10)), vec![1, 2]);
        assert_eq!(ids(&store.history(20)), vec![1]);
        assert!(store.history(999).is_empty());
    }

    #[test]
    fn live_new_message_is_folded_in_order() {
        let mut store = MessageStore::new();
        store.merge([msg(10, 10), msg(10, 20)]);
        // A live message newer than the loaded history appends at the end.
        store.reduce(&new_message(10, 30));
        assert_eq!(ids(&store.history(10)), vec![10, 20, 30]);

        // A live message that was also in history collapses onto the same entry.
        store.reduce(&new_message(10, 20));
        assert_eq!(ids(&store.history(10)), vec![10, 20, 30]);
        assert_eq!(store.get(10, 20).unwrap().text(), Some("m20"));
    }

    #[test]
    fn edit_swaps_message_content_in_place_and_is_idempotent() {
        let mut store = MessageStore::new();
        store.merge([msg(10, 1), msg(10, 2)]);
        store.reduce(&message_content(10, 2, "edited"));

        // The content changed; the entry kept its id and position, none added.
        assert_eq!(store.get(10, 2).unwrap().text(), Some("edited"));
        assert_eq!(ids(&store.history(10)), vec![1, 2]);
        assert_eq!(store.get(10, 1).unwrap().text(), Some("m1"));

        // Replaying the same edit converges.
        store.reduce(&message_content(10, 2, "edited"));
        assert_eq!(store.get(10, 2).unwrap().text(), Some("edited"));
        assert_eq!(store.count(10), 2);
    }

    #[test]
    fn edit_of_an_unknown_message_is_ignored() {
        let mut store = MessageStore::new();
        // No header/sender to synthesize from a content-only update — stays empty.
        store.reduce(&message_content(10, 99, "ghost"));
        assert!(store.is_empty());
    }

    #[test]
    fn permanent_delete_removes_only_the_named_messages_and_is_idempotent() {
        let mut store = MessageStore::new();
        store.merge([msg(10, 1), msg(10, 2), msg(10, 3)]);
        store.reduce(&delete_messages(10, vec![1, 3], true, false));

        assert_eq!(ids(&store.history(10)), vec![2]);

        // Replaying the delete (TDLib can repeat) is a no-op on the absent ids.
        store.reduce(&delete_messages(10, vec![1, 3], true, false));
        assert_eq!(ids(&store.history(10)), vec![2]);
    }

    #[test]
    fn cache_eviction_delete_keeps_our_copy() {
        let mut store = MessageStore::new();
        store.merge([msg(10, 1), msg(10, 2)]);
        // is_permanent false: TDLib is only unloading its cache, not deleting.
        store.reduce(&delete_messages(10, vec![1, 2], false, true));
        assert_eq!(ids(&store.history(10)), vec![1, 2]);
    }

    #[test]
    fn sent_message_appears_optimistically_then_reconciles_to_its_real_id() {
        let mut store = MessageStore::new();
        // The optimistic message lands at once, Pending, under a temp id.
        store.reduce(&pending_message(10, 1001));
        assert_eq!(store.get(10, 1001).unwrap().send_state, SendState::Pending);
        assert_eq!(ids(&store.history(10)), vec![1001]);

        // The server confirms: temp 1001 becomes real id 5 (server ids sort below
        // temp ids), Sent — one entry, re-keyed, not duplicated.
        store.reduce(&send_succeeded(10, 1001, 5));
        assert!(store.get(10, 1001).is_none());
        assert_eq!(store.get(10, 5).unwrap().send_state, SendState::Sent);
        assert_eq!(ids(&store.history(10)), vec![5]);
        assert_eq!(store.count(10), 1);
    }

    #[test]
    fn failed_send_flips_the_optimistic_message_in_place() {
        let mut store = MessageStore::new();
        store.reduce(&pending_message(10, 1001));
        store.reduce(&send_failed(10, 1001, 403, "CHAT_WRITE_FORBIDDEN"));

        // Same id, no duplicate — the entry carries the failure for retry.
        assert_eq!(
            store.get(10, 1001).unwrap().send_state,
            SendState::Failed {
                code: 403,
                message: "CHAT_WRITE_FORBIDDEN".to_owned(),
            }
        );
        assert_eq!(ids(&store.history(10)), vec![1001]);
    }

    #[test]
    fn replayed_send_succeeded_is_idempotent() {
        let mut store = MessageStore::new();
        store.reduce(&pending_message(10, 1001));
        store.reduce(&send_succeeded(10, 1001, 5));
        // TDLib can repeat updates; a second reconcile (temp already gone) just
        // re-affirms the real message rather than resurrecting or doubling it.
        store.reduce(&send_succeeded(10, 1001, 5));
        assert_eq!(ids(&store.history(10)), vec![5]);
        assert_eq!(store.count(10), 1);
    }

    /// A spy that captures the arguments of the most recent `send_text` and
    /// echoes back the optimistic Pending message `TDLib` would return.
    struct SendSpy {
        last: RefCell<Option<(i64, Option<i64>, FormattedText)>>,
    }

    impl SendRequests for SendSpy {
        async fn send_text(
            &self,
            chat_id: i64,
            reply_to: Option<i64>,
            text: FormattedText,
        ) -> Result<Message, TdError> {
            self.last
                .borrow_mut()
                .replace((chat_id, reply_to, text.clone()));
            Ok(Message::from_tdlib(&td_message_state(
                chat_id,
                1001,
                Some(MessageSendingState::Pending(
                    MessageSendingStatePending::default(),
                )),
            )))
        }

        async fn send_media(
            &self,
            _chat_id: i64,
            _reply_to: Option<i64>,
            _media: OutgoingMedia,
        ) -> Result<Message, TdError> {
            unimplemented!("SendSpy exercises the send path only")
        }
    }

    #[tokio::test]
    async fn send_text_threads_reply_target_and_returns_a_pending_message() {
        let spy = SendSpy {
            last: RefCell::new(None),
        };
        let body = FormattedText {
            text: "ack".to_owned(),
            entities: vec![],
        };
        // A reply targets a message id in the same chat.
        let optimistic = spy.send_text(10, Some(42), body.clone()).await.unwrap();

        assert_eq!(*spy.last.borrow(), Some((10, Some(42), body)));
        // The seam's contract: the caller gets an optimistic Pending message back.
        assert_eq!(optimistic.send_state, SendState::Pending);
    }

    /// A spy scripted with a `parse_markdown` result (success or failure) that
    /// also captures the `send_text`/`edit_text` call it drives — #212's
    /// request-seam test double, verifying the parsed (or, on failure,
    /// fallback-plain) `FormattedText` actually rides the send/edit call.
    struct FormatSpy {
        parse_result: Result<FormattedText, TdError>,
        last_send: RefCell<Option<(i64, Option<i64>, FormattedText)>>,
        last_edit: RefCell<Option<(i64, i64, FormattedText)>>,
    }

    impl FormatRequests for FormatSpy {
        async fn parse_markdown(&self, _text: String) -> Result<FormattedText, TdError> {
            self.parse_result.clone()
        }
    }

    impl SendRequests for FormatSpy {
        async fn send_text(
            &self,
            chat_id: i64,
            reply_to: Option<i64>,
            text: FormattedText,
        ) -> Result<Message, TdError> {
            self.last_send
                .borrow_mut()
                .replace((chat_id, reply_to, text));
            Ok(Message::from_tdlib(&td_message_state(
                chat_id,
                1001,
                Some(MessageSendingState::Pending(
                    MessageSendingStatePending::default(),
                )),
            )))
        }

        async fn send_media(
            &self,
            _chat_id: i64,
            _reply_to: Option<i64>,
            _media: OutgoingMedia,
        ) -> Result<Message, TdError> {
            unimplemented!("FormatSpy exercises the send-text path only")
        }
    }

    impl EditRequests for FormatSpy {
        async fn edit_text(
            &self,
            chat_id: i64,
            message_id: i64,
            text: FormattedText,
        ) -> Result<Message, TdError> {
            self.last_edit
                .borrow_mut()
                .replace((chat_id, message_id, text));
            Ok(Message::from_tdlib(&td_message_state(
                chat_id, message_id, None,
            )))
        }

        async fn edit_caption(
            &self,
            _chat_id: i64,
            _message_id: i64,
            _caption: FormattedText,
        ) -> Result<Message, TdError> {
            unimplemented!("FormatSpy exercises the edit-text path only")
        }
    }

    #[tokio::test]
    async fn send_formatted_text_threads_a_successful_parse_s_entities_onto_send_text() {
        use crate::model::TextEntity;
        let parsed = FormattedText {
            text: "bold".to_owned(),
            entities: vec![TextEntity {
                offset: 0,
                length: 4,
                kind: crate::model::EntityKind::Bold,
            }],
        };
        let spy = FormatSpy {
            parse_result: Ok(parsed.clone()),
            last_send: RefCell::new(None),
            last_edit: RefCell::new(None),
        };
        send_formatted_text(&spy, 10, Some(42), "*bold*".to_owned())
            .await
            .unwrap();
        assert_eq!(*spy.last_send.borrow(), Some((10, Some(42), parsed)));
    }

    #[tokio::test]
    async fn send_formatted_text_falls_back_to_plain_text_on_a_parse_error() {
        let spy = FormatSpy {
            parse_result: Err(TdError {
                code: 400,
                message: "Can't parse entities: character '.' is reserved".to_owned(),
            }),
            last_send: RefCell::new(None),
            last_edit: RefCell::new(None),
        };
        send_formatted_text(&spy, 10, None, "hi.".to_owned())
            .await
            .unwrap();
        assert_eq!(
            *spy.last_send.borrow(),
            Some((
                10,
                None,
                FormattedText {
                    text: "hi.".to_owned(),
                    entities: vec![],
                }
            )),
            "a parse failure must still send, plain"
        );
    }

    #[tokio::test]
    async fn edit_formatted_text_threads_a_successful_parse_s_entities_onto_edit_text() {
        use crate::model::TextEntity;
        let parsed = FormattedText {
            text: "code".to_owned(),
            entities: vec![TextEntity {
                offset: 0,
                length: 4,
                kind: crate::model::EntityKind::Code,
            }],
        };
        let spy = FormatSpy {
            parse_result: Ok(parsed.clone()),
            last_send: RefCell::new(None),
            last_edit: RefCell::new(None),
        };
        edit_formatted_text(&spy, 10, 7, "`code`".to_owned())
            .await
            .unwrap();
        assert_eq!(*spy.last_edit.borrow(), Some((10, 7, parsed)));
    }

    #[tokio::test]
    async fn edit_formatted_text_falls_back_to_plain_text_on_a_parse_error() {
        let spy = FormatSpy {
            parse_result: Err(TdError {
                code: 400,
                message: "Can't parse entities: unmatched *".to_owned(),
            }),
            last_send: RefCell::new(None),
            last_edit: RefCell::new(None),
        };
        edit_formatted_text(&spy, 10, 7, "*oops".to_owned())
            .await
            .unwrap();
        assert_eq!(
            *spy.last_edit.borrow(),
            Some((
                10,
                7,
                FormattedText {
                    text: "*oops".to_owned(),
                    entities: vec![],
                }
            ))
        );
    }

    /// A spy that returns scripted history pages in order, then empty pages. It
    /// ignores the anchor (the driver's anchor maths is exercised separately by
    /// the assertion that every scripted message lands deduped and ordered).
    struct HistorySpy {
        pages: RefCell<VecDeque<Vec<Message>>>,
        calls: RefCell<u32>,
    }

    impl HistorySpy {
        fn new(pages: Vec<Vec<Message>>) -> Self {
            Self {
                pages: RefCell::new(pages.into()),
                calls: RefCell::new(0),
            }
        }
    }

    impl HistoryRequests for HistorySpy {
        async fn get_chat_history(
            &self,
            _chat_id: i64,
            _from_message_id: i64,
            _limit: i32,
        ) -> Result<Vec<Message>, TdError> {
            *self.calls.borrow_mut() += 1;
            Ok(self.pages.borrow_mut().pop_front().unwrap_or_default())
        }
    }

    #[tokio::test]
    async fn load_history_pages_until_empty_and_folds_each_page() {
        let spy = HistorySpy::new(vec![
            vec![msg(10, 30), msg(10, 20)], // newest page
            vec![msg(10, 20), msg(10, 10)], // older page, overlaps on 20
        ]);
        let mut store = MessageStore::new();

        load_history(&spy, 10, 2, |page| store.merge(page))
            .await
            .unwrap();

        // Two non-empty pages folded, deduped and ordered; third call hits empty.
        assert_eq!(ids(&store.history(10)), vec![10, 20, 30]);
        assert_eq!(*spy.calls.borrow(), 3);
    }

    /// A history fetch that fails stops paging and propagates the error.
    struct FailingSpy;

    impl HistoryRequests for FailingSpy {
        async fn get_chat_history(
            &self,
            _chat_id: i64,
            _from_message_id: i64,
            _limit: i32,
        ) -> Result<Vec<Message>, TdError> {
            Err(TdError {
                code: 400,
                message: "CHANNEL_PRIVATE".to_owned(),
            })
        }
    }

    #[tokio::test]
    async fn load_history_propagates_a_request_error() {
        let mut store = MessageStore::new();
        let err = load_history(&FailingSpy, 10, 2, |page| store.merge(page))
            .await
            .unwrap_err();
        assert_eq!(err.code, 400);
        assert!(store.is_empty());
    }

    /// Captures the arguments of the most recent `edit_text` / `delete` so the
    /// request seam's wiring (which message, which revoke mode) is asserted.
    #[derive(Default)]
    struct EditDeleteSpy {
        edited: RefCell<Option<(i64, i64, FormattedText)>>,
        deleted: RefCell<Option<(i64, Vec<i64>, bool)>>,
    }

    impl EditRequests for EditDeleteSpy {
        async fn edit_text(
            &self,
            chat_id: i64,
            message_id: i64,
            text: FormattedText,
        ) -> Result<Message, TdError> {
            self.edited
                .borrow_mut()
                .replace((chat_id, message_id, text.clone()));
            // TDLib echoes the edited message; mirror that with the new content.
            let mut edited = msg(chat_id, message_id);
            edited.content = MessageContent::Text(text);
            Ok(edited)
        }

        async fn edit_caption(
            &self,
            _chat_id: i64,
            _message_id: i64,
            _caption: FormattedText,
        ) -> Result<Message, TdError> {
            unimplemented!("EditDeleteSpy exercises edit/delete only")
        }
    }

    impl DeleteRequests for EditDeleteSpy {
        async fn delete(
            &self,
            chat_id: i64,
            message_ids: Vec<i64>,
            revoke: bool,
        ) -> Result<(), TdError> {
            self.deleted
                .borrow_mut()
                .replace((chat_id, message_ids, revoke));
            Ok(())
        }
    }

    #[tokio::test]
    async fn edit_text_threads_the_target_and_returns_the_edited_message() {
        let spy = EditDeleteSpy::default();
        let body = FormattedText {
            text: "fixed".to_owned(),
            entities: vec![],
        };
        let edited = spy.edit_text(10, 2, body.clone()).await.unwrap();

        assert_eq!(*spy.edited.borrow(), Some((10, 2, body)));
        assert_eq!(edited.id, 2);
        assert_eq!(edited.text(), Some("fixed"));
    }

    #[tokio::test]
    async fn delete_threads_the_revoke_choice() {
        let spy = EditDeleteSpy::default();
        // Revoke for everyone.
        spy.delete(10, vec![1, 2], true).await.unwrap();
        assert_eq!(*spy.deleted.borrow(), Some((10, vec![1, 2], true)));

        // Delete only for self.
        spy.delete(10, vec![3], false).await.unwrap();
        assert_eq!(*spy.deleted.borrow(), Some((10, vec![3], false)));
    }

    /// Captures the arguments of the most recent `view_messages`, so the read
    /// request's wiring (which chat, which message ids) is asserted.
    #[derive(Default)]
    struct ViewSpy {
        viewed: RefCell<Option<(i64, Vec<i64>)>>,
    }

    impl ReadRequests for ViewSpy {
        async fn view_messages(&self, chat_id: i64, message_ids: Vec<i64>) -> Result<(), TdError> {
            self.viewed.borrow_mut().replace((chat_id, message_ids));
            Ok(())
        }
    }

    #[tokio::test]
    async fn view_messages_threads_the_chat_and_message_ids() {
        let spy = ViewSpy::default();
        spy.view_messages(10, vec![1, 2, 3]).await.unwrap();
        assert_eq!(*spy.viewed.borrow(), Some((10, vec![1, 2, 3])));
    }

    /// A forward as `TDLib` emits it: the same messages re-appear in the target chat
    /// under fresh temp ids and Pending state.
    fn forwarded_pending(target_chat: i64, temp_id: i64) -> Update {
        pending_message(target_chat, temp_id)
    }

    /// The arguments of a `forward_messages` call, captured for assertion.
    #[derive(Debug, PartialEq, Eq)]
    struct ForwardCall {
        from_chat_id: i64,
        message_ids: Vec<i64>,
        to_chat_id: i64,
        send_copy: bool,
        remove_caption: bool,
    }

    /// Captures the most recent `forward_messages` arguments and echoes back the
    /// optimistic Pending messages `TDLib` would create in the target chat.
    #[derive(Default)]
    struct ForwardSpy {
        last: RefCell<Option<ForwardCall>>,
    }

    impl ForwardRequests for ForwardSpy {
        async fn forward_messages(
            &self,
            from_chat_id: i64,
            message_ids: Vec<i64>,
            to_chat_id: i64,
            send_copy: bool,
            remove_caption: bool,
        ) -> Result<Vec<Message>, TdError> {
            self.last.borrow_mut().replace(ForwardCall {
                from_chat_id,
                message_ids: message_ids.clone(),
                to_chat_id,
                send_copy,
                remove_caption,
            });
            // One optimistic forwarded message per source id, under a fresh temp id
            // in the target chat — the contract send_text also honours.
            Ok(message_ids
                .iter()
                .enumerate()
                .map(|(i, _)| {
                    Message::from_tdlib(&td_message_state(
                        to_chat_id,
                        2001 + i as i64,
                        Some(MessageSendingState::Pending(
                            MessageSendingStatePending::default(),
                        )),
                    ))
                })
                .collect())
        }
    }

    #[tokio::test]
    async fn forward_threads_source_target_and_copy_options() {
        let spy = ForwardSpy::default();
        // Forward two messages from chat 10 into chat 20, as a copy without caption.
        let optimistic = spy
            .forward_messages(10, vec![1, 2], 20, true, true)
            .await
            .unwrap();

        assert_eq!(
            *spy.last.borrow(),
            Some(ForwardCall {
                from_chat_id: 10,
                message_ids: vec![1, 2],
                to_chat_id: 20,
                send_copy: true,
                remove_caption: true,
            })
        );
        // The caller gets one optimistic Pending message per forwarded id.
        assert_eq!(optimistic.len(), 2);
        assert!(
            optimistic
                .iter()
                .all(|m| m.send_state == SendState::Pending && m.chat_id == 20)
        );
    }

    #[test]
    fn forwarded_messages_land_in_the_target_store_via_new_message() {
        let mut store = MessageStore::new();
        // The source chat already holds the originals.
        store.merge([msg(10, 1), msg(10, 2)]);

        // The router folds each forwarded message as a normal updateNewMessage into
        // the target chat — the same lifecycle path as a fresh send.
        store.reduce(&forwarded_pending(20, 2001));
        store.reduce(&forwarded_pending(20, 2002));

        assert_eq!(ids(&store.history(20)), vec![2001, 2002]);
        assert_eq!(store.get(20, 2001).unwrap().send_state, SendState::Pending);
        // The source chat is untouched by the forward.
        assert_eq!(ids(&store.history(10)), vec![1, 2]);
    }

    /// The arguments of a `search_chat_messages` call, captured for assertion.
    #[derive(Debug, PartialEq, Eq)]
    struct ChatSearchCall {
        chat_id: i64,
        query: String,
        sender: Option<Sender>,
        from_message_id: i64,
        limit: i32,
    }

    /// The arguments of a `search_messages` (global) call, captured for assertion.
    #[derive(Debug, PartialEq, Eq)]
    struct GlobalSearchCall {
        query: String,
        from_offset: String,
        limit: i32,
    }

    /// Records every search call and serves scripted result pages in order, so a
    /// test asserts both the threaded query/paging arguments and the normalized
    /// results — following each page's cursor exactly as a real driver would.
    #[derive(Default)]
    struct SearchSpy {
        chat_calls: RefCell<Vec<ChatSearchCall>>,
        global_calls: RefCell<Vec<GlobalSearchCall>>,
        chat_pages: RefCell<VecDeque<SearchPage<i64>>>,
        global_pages: RefCell<VecDeque<SearchPage<String>>>,
    }

    impl SearchRequests for SearchSpy {
        async fn search_chat_messages(
            &self,
            chat_id: i64,
            query: String,
            sender: Option<Sender>,
            from_message_id: i64,
            limit: i32,
        ) -> Result<SearchPage<i64>, TdError> {
            self.chat_calls.borrow_mut().push(ChatSearchCall {
                chat_id,
                query,
                sender,
                from_message_id,
                limit,
            });
            Ok(self
                .chat_pages
                .borrow_mut()
                .pop_front()
                .unwrap_or(SearchPage {
                    messages: vec![],
                    total_count: 0,
                    next: None,
                }))
        }

        async fn search_messages(
            &self,
            query: String,
            from_offset: String,
            limit: i32,
        ) -> Result<SearchPage<String>, TdError> {
            self.global_calls.borrow_mut().push(GlobalSearchCall {
                query,
                from_offset,
                limit,
            });
            Ok(self
                .global_pages
                .borrow_mut()
                .pop_front()
                .unwrap_or(SearchPage {
                    messages: vec![],
                    total_count: 0,
                    next: None,
                }))
        }
    }

    #[tokio::test]
    async fn chat_search_threads_query_sender_and_returns_a_normalized_page() {
        let spy = SearchSpy::default();
        spy.chat_pages.borrow_mut().push_back(SearchPage {
            messages: vec![msg(10, 30), msg(10, 20)],
            total_count: 5,
            next: Some(20),
        });

        // Search chat 10 for "hi", restricted to one sender, from the newest.
        let page = spy
            .search_chat_messages(10, "hi".to_owned(), Some(Sender::User(7)), NEWEST, 2)
            .await
            .unwrap();

        assert_eq!(
            *spy.chat_calls.borrow(),
            vec![ChatSearchCall {
                chat_id: 10,
                query: "hi".to_owned(),
                sender: Some(Sender::User(7)),
                from_message_id: NEWEST,
                limit: 2,
            }]
        );
        // Hits come back normalized, in the server's result order (not re-sorted),
        // with the next-page cursor and the approximate total carried through.
        let hit_ids: Vec<i64> = page.messages.iter().map(|m| m.id).collect();
        assert_eq!(hit_ids, vec![30, 20]);
        assert_eq!(page.total_count, 5);
        assert_eq!(page.next, Some(20));
    }

    #[tokio::test]
    async fn chat_search_pages_until_exhausted_deduping_into_a_transient_view() {
        let spy = SearchSpy::default();
        spy.chat_pages.borrow_mut().extend([
            SearchPage {
                messages: vec![msg(10, 30), msg(10, 20)],
                total_count: 3,
                next: Some(20),
            },
            // The older page overlaps on id 20 and ends the results.
            SearchPage {
                messages: vec![msg(10, 20), msg(10, 15)],
                total_count: 3,
                next: None,
            },
        ]);

        // The chat already has some loaded history — the search must not touch it.
        let mut store = MessageStore::new();
        store.merge([msg(10, 30)]);

        // The paging driver follows each page's cursor to exhaustion, collecting
        // into the transient view — it never sees the store.
        let results = search_chat(&spy, 10, "x".to_owned(), None, 2)
            .await
            .unwrap();

        // Overlapping id 20 collapsed to one entry; result order preserved.
        let collected: Vec<i64> = results.messages().iter().map(|m| m.id).collect();
        assert_eq!(collected, vec![30, 20, 15]);
        assert_eq!(results.len(), 3);
        assert!(results.contains(10, 20));
        // Each page asked from the previous page's cursor, starting at NEWEST.
        let anchors: Vec<i64> = spy
            .chat_calls
            .borrow()
            .iter()
            .map(|c| c.from_message_id)
            .collect();
        assert_eq!(anchors, vec![NEWEST, 20]);
        // The transient view left the live history store untouched.
        assert_eq!(ids(&store.history(10)), vec![30]);
    }

    #[tokio::test]
    async fn global_search_threads_query_offset_and_keeps_cross_chat_hits_distinct() {
        let spy = SearchSpy::default();
        spy.global_pages.borrow_mut().extend([
            SearchPage {
                messages: vec![msg(10, 5), msg(20, 5)],
                total_count: 3,
                next: Some("pg2".to_owned()),
            },
            SearchPage {
                messages: vec![msg(30, 1)],
                total_count: 3,
                next: None,
            },
        ]);

        let results = search_global(&spy, "term".to_owned(), 2).await.unwrap();

        // Same id 5 in two chats stays distinct — dedupe keys on (chat, id).
        let keys: Vec<(i64, i64)> = results
            .messages()
            .iter()
            .map(|m| (m.chat_id, m.id))
            .collect();
        assert_eq!(keys, vec![(10, 5), (20, 5), (30, 1)]);
        // Query threaded, paging resumed from the opaque offset (empty first).
        assert_eq!(
            *spy.global_calls.borrow(),
            vec![
                GlobalSearchCall {
                    query: "term".to_owned(),
                    from_offset: String::new(),
                    limit: 2,
                },
                GlobalSearchCall {
                    query: "term".to_owned(),
                    from_offset: "pg2".to_owned(),
                    limit: 2,
                },
            ]
        );
    }

    /// A `TDLib` `Message` carrying a photo with `caption`, reusing the zeroed
    /// scaffold the text helper builds and swapping only the content. `sending_state`
    /// drives the optimistic (Pending) vs settled distinction the send lifecycle
    /// needs.
    fn td_photo_message(
        chat_id: i64,
        id: i64,
        caption: &str,
        sending_state: Option<MessageSendingState>,
    ) -> tdlib_rs::types::Message {
        let mut message = td_message_state(chat_id, id, sending_state);
        message.content = tdlib_rs::enums::MessageContent::MessagePhoto(TdMessagePhoto {
            caption: TdFormattedText {
                text: caption.to_owned(),
                entities: vec![],
            },
            ..Default::default()
        });
        message
    }

    /// A spy that captures the arguments of the most recent `send_media` /
    /// `edit_caption` and echoes back the optimistic Pending photo `TDLib` returns.
    #[derive(Default)]
    struct MediaSpy {
        sent: RefCell<Option<(i64, Option<i64>, OutgoingMedia)>>,
        captioned: RefCell<Option<(i64, i64, FormattedText)>>,
    }

    impl SendRequests for MediaSpy {
        async fn send_text(
            &self,
            _chat_id: i64,
            _reply_to: Option<i64>,
            _text: FormattedText,
        ) -> Result<Message, TdError> {
            unimplemented!("MediaSpy exercises the media send path only")
        }

        async fn send_media(
            &self,
            chat_id: i64,
            reply_to: Option<i64>,
            media: OutgoingMedia,
        ) -> Result<Message, TdError> {
            self.sent
                .borrow_mut()
                .replace((chat_id, reply_to, media.clone()));
            Ok(Message::from_tdlib(&td_photo_message(
                chat_id,
                1001,
                "",
                Some(MessageSendingState::Pending(
                    MessageSendingStatePending::default(),
                )),
            )))
        }
    }

    impl EditRequests for MediaSpy {
        async fn edit_text(
            &self,
            _chat_id: i64,
            _message_id: i64,
            _text: FormattedText,
        ) -> Result<Message, TdError> {
            unimplemented!("MediaSpy exercises the media send path only")
        }

        async fn edit_caption(
            &self,
            chat_id: i64,
            message_id: i64,
            caption: FormattedText,
        ) -> Result<Message, TdError> {
            self.captioned
                .borrow_mut()
                .replace((chat_id, message_id, caption.clone()));
            Ok(Message::from_tdlib(&td_photo_message(
                chat_id, message_id, "edited", None,
            )))
        }
    }

    #[tokio::test]
    async fn send_media_threads_reply_target_and_returns_a_pending_message() {
        let spy = MediaSpy::default();
        let media = OutgoingMedia::Photo {
            path: "/tmp/pic.jpg".to_owned(),
            caption: FormattedText {
                text: "look".to_owned(),
                entities: vec![],
            },
        };
        // A reply targets a message id in the same chat.
        let optimistic = spy.send_media(10, Some(42), media.clone()).await.unwrap();

        assert_eq!(*spy.sent.borrow(), Some((10, Some(42), media)));
        // The seam's contract: the caller gets an optimistic Pending message back,
        // exactly like a text send.
        assert_eq!(optimistic.send_state, SendState::Pending);
    }

    #[tokio::test]
    async fn edit_caption_threads_its_target_and_new_caption() {
        let spy = MediaSpy::default();
        let caption = FormattedText {
            text: "fixed".to_owned(),
            entities: vec![],
        };
        spy.edit_caption(10, 7, caption.clone()).await.unwrap();
        assert_eq!(*spy.captioned.borrow(), Some((10, 7, caption)));
    }

    #[test]
    fn media_send_reconciles_from_pending_to_the_server_id() {
        let mut store = MessageStore::new();
        // The optimistic photo send lands with a temporary id, Pending — the same
        // lifecycle entry a text send creates, but carrying photo content.
        store.reduce(&Update::NewMessage(UpdateNewMessage {
            message: td_photo_message(
                10,
                5001,
                "pic",
                Some(MessageSendingState::Pending(
                    MessageSendingStatePending::default(),
                )),
            ),
        }));
        let pending = store.get(10, 5001).unwrap();
        assert_eq!(pending.send_state, SendState::Pending);
        assert!(matches!(pending.content, MessageContent::Photo(_)));

        // The upload finishes and the send is accepted: the temp id is swapped for
        // the server's real one, in place, and the photo content survives.
        store.reduce(&Update::MessageSendSucceeded(UpdateMessageSendSucceeded {
            message: td_photo_message(10, 8001, "pic", None),
            old_message_id: 5001,
        }));
        assert!(store.get(10, 5001).is_none());
        let settled = store.get(10, 8001).unwrap();
        assert_eq!(settled.send_state, SendState::Sent);
        assert!(matches!(settled.content, MessageContent::Photo(_)));
        assert_eq!(store.count(10), 1);
    }

    #[test]
    fn media_send_failure_flips_the_pending_entry_in_place() {
        let mut store = MessageStore::new();
        store.reduce(&Update::NewMessage(UpdateNewMessage {
            message: td_photo_message(
                10,
                5001,
                "pic",
                Some(MessageSendingState::Pending(
                    MessageSendingStatePending::default(),
                )),
            ),
        }));
        // A rejected upload flips the same temporary entry to Failed, carrying the
        // cause, without dropping the message.
        store.reduce(&Update::MessageSendFailed(UpdateMessageSendFailed {
            message: td_photo_message(10, 5001, "pic", None),
            old_message_id: 5001,
            error: TdError {
                code: 420,
                message: "FILE_PARTS_INVALID".to_owned(),
            },
        }));
        let failed = store.get(10, 5001).unwrap();
        assert_eq!(
            failed.send_state,
            SendState::Failed {
                code: 420,
                message: "FILE_PARTS_INVALID".to_owned(),
            }
        );
        assert!(matches!(failed.content, MessageContent::Photo(_)));
    }

    #[test]
    fn caption_edit_swaps_the_caption_in_place() {
        let mut store = MessageStore::new();
        store.reduce(&Update::NewMessage(UpdateNewMessage {
            message: td_photo_message(10, 7, "old", None),
        }));
        // editMessageCaption echoes updateMessageContent with the new caption; the
        // known message's content is swapped in place, same fold as a text edit.
        store.reduce(&Update::MessageContent(UpdateMessageContent {
            chat_id: 10,
            message_id: 7,
            new_content: tdlib_rs::enums::MessageContent::MessagePhoto(TdMessagePhoto {
                caption: TdFormattedText {
                    text: "new".to_owned(),
                    entities: vec![],
                },
                ..Default::default()
            }),
        }));
        let MessageContent::Photo(photo) = &store.get(10, 7).unwrap().content else {
            panic!("expected a photo message");
        };
        assert_eq!(photo.caption.text, "new");
    }

    /// An `updateMessageInteractionInfo` carrying `reactions` as
    /// `(emoji, count, is_chosen)` triples — the reaction-count change `TDLib`
    /// streams after a reaction is added or removed.
    fn reaction_update(chat_id: i64, message_id: i64, reactions: &[(&str, i32, bool)]) -> Update {
        use tdlib_rs::types::{
            MessageInteractionInfo, MessageReaction, MessageReactions, UpdateMessageInteractionInfo,
        };
        Update::MessageInteractionInfo(UpdateMessageInteractionInfo {
            chat_id,
            message_id,
            interaction_info: Some(MessageInteractionInfo {
                reactions: Some(MessageReactions {
                    reactions: reactions
                        .iter()
                        .map(|&(emoji, total_count, is_chosen)| MessageReaction {
                            r#type: ReactionType::Emoji(ReactionTypeEmoji {
                                emoji: emoji.to_owned(),
                            }),
                            total_count,
                            is_chosen,
                            used_sender_id: None,
                            recent_sender_ids: vec![],
                        })
                        .collect(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        })
    }

    /// An `updateMessageInteractionInfo` with no interaction info at all — `TDLib`'s
    /// signal that a message's last reaction (and other interactions) is gone.
    fn reaction_cleared(chat_id: i64, message_id: i64) -> Update {
        Update::MessageInteractionInfo(tdlib_rs::types::UpdateMessageInteractionInfo {
            chat_id,
            message_id,
            interaction_info: None,
        })
    }

    /// The reaction kinds and counts a message carries, for terse assertions.
    fn reactions(message: &Message) -> Vec<(crate::model::ReactionKind, i32, bool)> {
        message
            .reactions
            .iter()
            .map(|r| (r.kind.clone(), r.count, r.is_chosen))
            .collect()
    }

    #[test]
    fn interaction_info_folds_reactions_onto_a_known_message_in_place_and_is_idempotent() {
        use crate::model::ReactionKind::Emoji;
        let mut store = MessageStore::new();
        store.merge([msg(10, 1), msg(10, 2)]);

        store.reduce(&reaction_update(
            10,
            2,
            &[("👍", 3, true), ("🔥", 1, false)],
        ));

        // The reactions land on the message in TDLib's order; no entry added.
        assert_eq!(
            reactions(store.get(10, 2).unwrap()),
            vec![
                (Emoji("👍".to_owned()), 3, true),
                (Emoji("🔥".to_owned()), 1, false)
            ]
        );
        assert_eq!(ids(&store.history(10)), vec![1, 2]);
        // The sibling message is untouched.
        assert!(store.get(10, 1).unwrap().reactions.is_empty());

        // Replaying the same update converges.
        store.reduce(&reaction_update(
            10,
            2,
            &[("👍", 3, true), ("🔥", 1, false)],
        ));
        assert_eq!(store.get(10, 2).unwrap().reactions.len(), 2);
    }

    #[test]
    fn interaction_info_replaces_then_clears_a_messages_reactions() {
        use crate::model::ReactionKind::Emoji;
        let mut store = MessageStore::new();
        store.merge([msg(10, 1)]);

        // A later update wholly replaces the previous buckets (count bumped, a new
        // reaction chosen) — it is not merged with the prior state.
        store.reduce(&reaction_update(10, 1, &[("👍", 1, false)]));
        store.reduce(&reaction_update(10, 1, &[("👍", 2, true)]));
        assert_eq!(
            reactions(store.get(10, 1).unwrap()),
            vec![(Emoji("👍".to_owned()), 2, true)]
        );

        // The last reaction removed: TDLib drops the interaction info, clearing them.
        store.reduce(&reaction_cleared(10, 1));
        assert!(store.get(10, 1).unwrap().reactions.is_empty());
    }

    #[test]
    fn interaction_info_for_an_unknown_message_is_ignored() {
        let mut store = MessageStore::new();
        // No header/sender to synthesize from an interaction-only update — like an
        // edit of an unknown message, it leaves the store empty.
        store.reduce(&reaction_update(10, 99, &[("👍", 1, false)]));
        assert!(store.is_empty());
    }

    /// The arguments of a reaction or pin call, captured for assertion.
    #[derive(Debug, Default, PartialEq, Eq)]
    struct ReactionPinCalls {
        added: Option<(i64, i64, String)>,
        removed: Option<(i64, i64, String)>,
        pinned: Option<(i64, i64, bool, bool)>,
        unpinned: Option<(i64, i64)>,
    }

    /// Captures the most recent reaction/pin request arguments so the seam's
    /// wiring (which message, which emoji, which pin flags) is asserted.
    #[derive(Default)]
    struct ReactionPinSpy {
        calls: RefCell<ReactionPinCalls>,
    }

    impl ReactionRequests for ReactionPinSpy {
        async fn add_message_reaction(
            &self,
            chat_id: i64,
            message_id: i64,
            emoji: String,
        ) -> Result<(), TdError> {
            self.calls.borrow_mut().added = Some((chat_id, message_id, emoji));
            Ok(())
        }

        async fn remove_message_reaction(
            &self,
            chat_id: i64,
            message_id: i64,
            emoji: String,
        ) -> Result<(), TdError> {
            self.calls.borrow_mut().removed = Some((chat_id, message_id, emoji));
            Ok(())
        }
    }

    impl PinRequests for ReactionPinSpy {
        async fn pin_chat_message(
            &self,
            chat_id: i64,
            message_id: i64,
            disable_notification: bool,
            only_for_self: bool,
        ) -> Result<(), TdError> {
            self.calls.borrow_mut().pinned =
                Some((chat_id, message_id, disable_notification, only_for_self));
            Ok(())
        }

        async fn unpin_chat_message(&self, chat_id: i64, message_id: i64) -> Result<(), TdError> {
            self.calls.borrow_mut().unpinned = Some((chat_id, message_id));
            Ok(())
        }
    }

    #[tokio::test]
    async fn reaction_requests_thread_the_target_message_and_emoji() {
        let spy = ReactionPinSpy::default();
        spy.add_message_reaction(10, 2, "👍".to_owned())
            .await
            .unwrap();
        spy.remove_message_reaction(10, 2, "👎".to_owned())
            .await
            .unwrap();

        assert_eq!(spy.calls.borrow().added, Some((10, 2, "👍".to_owned())));
        assert_eq!(spy.calls.borrow().removed, Some((10, 2, "👎".to_owned())));
    }

    #[tokio::test]
    async fn pin_requests_thread_the_target_and_pin_flags() {
        let spy = ReactionPinSpy::default();
        // Pin silently, only for this account.
        spy.pin_chat_message(10, 5, true, true).await.unwrap();
        spy.unpin_chat_message(10, 5).await.unwrap();

        assert_eq!(spy.calls.borrow().pinned, Some((10, 5, true, true)));
        assert_eq!(spy.calls.borrow().unpinned, Some((10, 5)));
    }

    /// Secret chat text messaging (#54). A secret chat is reached by an ordinary
    /// chat id, so text sent and received in one rides the same [`MessageStore`]
    /// and send lifecycle as any chat — there is no secret-chat-specific routing.
    /// These exercise that path on a secret chat's chat id; the only rule the
    /// lifecycle adds is that a send waits for the chat to be ready.
    mod secret_chat_text {
        use super::*;
        use crate::model::{SecretChat, SecretChatState};

        /// A secret chat in `state`, with the ordinary chat id it is reached by.
        /// The id is just another `i64` to the store; readiness lives on the record.
        fn secret_chat(state: SecretChatState) -> (SecretChat, i64) {
            let key_hash = if matches!(state, SecretChatState::Ready) {
                "fingerprint".to_owned()
            } else {
                String::new()
            };
            (
                SecretChat {
                    id: 7,
                    user_id: 42,
                    state,
                    is_outbound: true,
                    key_hash,
                },
                -7, // the chat id behind the secret chat
            )
        }

        #[test]
        fn only_a_ready_secret_chat_is_messageable() {
            // The compose gate: text only flows once the key exchange completes.
            assert!(secret_chat(SecretChatState::Ready).0.is_ready());
            assert!(!secret_chat(SecretChatState::Pending).0.is_ready());
            assert!(!secret_chat(SecretChatState::Closed).0.is_ready());
        }

        #[test]
        fn text_received_in_a_secret_chat_folds_like_any_chat() {
            let (chat, chat_id) = secret_chat(SecretChatState::Ready);
            assert!(chat.is_ready());

            // An incoming secret-chat message arrives as updateNewMessage on the
            // chat id and folds into the store with no special handling.
            let mut store = MessageStore::new();
            store.reduce(&new_message(chat_id, 30));

            assert_eq!(ids(&store.history(chat_id)), vec![30]);
            assert_eq!(store.get(chat_id, 30).unwrap().text(), Some("m30"));
        }

        #[tokio::test]
        async fn text_sent_to_a_secret_chat_reconciles_through_the_send_lifecycle() {
            let (chat, chat_id) = secret_chat(SecretChatState::Ready);
            assert!(chat.is_ready());

            // Compose only when ready: drive the send seam on the chat id; the
            // caller gets the optimistic Pending message back, as for any chat.
            let spy = SendSpy {
                last: RefCell::new(None),
            };
            let body = FormattedText {
                text: "ack".to_owned(),
                entities: vec![],
            };
            let optimistic = spy.send_text(chat_id, None, body.clone()).await.unwrap();
            assert_eq!(*spy.last.borrow(), Some((chat_id, None, body)));
            assert_eq!(optimistic.send_state, SendState::Pending);

            // The lifecycle then folds identically: the optimistic message lands
            // Pending under a temp id, then reconciles to its real id on success.
            let mut store = MessageStore::new();
            store.reduce(&pending_message(chat_id, 1001));
            assert_eq!(
                store.get(chat_id, 1001).unwrap().send_state,
                SendState::Pending
            );

            store.reduce(&send_succeeded(chat_id, 1001, 5));
            assert!(store.get(chat_id, 1001).is_none());
            assert_eq!(store.get(chat_id, 5).unwrap().send_state, SendState::Sent);
            assert_eq!(ids(&store.history(chat_id)), vec![5]);
        }
    }

    /// Secret chat media messaging — the follow-up deferred from #54. Media rides
    /// the secret chat's ordinary chat id just as text does:
    /// [`send_media`](SendRequests::send_media) posts a file-backed message
    /// optimistically and the lifecycle reconciles it through the same
    /// [`MessageStore`], the file-backed content surviving the temp→real swap. The
    /// readiness gate is shared with text — there is no media-specific rule — so
    /// these reuse [`SecretChat::is_ready`] rather than restating one.
    mod secret_chat_media {
        use super::*;
        use crate::model::{SecretChat, SecretChatState};

        /// A ready secret chat and the ordinary chat id it is reached by.
        fn ready_secret_chat() -> (SecretChat, i64) {
            (
                SecretChat {
                    id: 7,
                    user_id: 42,
                    state: SecretChatState::Ready,
                    is_outbound: true,
                    key_hash: "fingerprint".to_owned(),
                },
                -7, // the chat id behind the secret chat
            )
        }

        #[tokio::test]
        async fn media_sent_to_a_secret_chat_returns_an_optimistic_pending_message() {
            let (chat, chat_id) = ready_secret_chat();
            // Compose only when ready — the same gate text uses, no media-specific one.
            assert!(chat.is_ready());

            // Drive the media seam on the secret chat's chat id; the caller gets the
            // same optimistic Pending message back as for any chat.
            let spy = MediaSpy::default();
            let media = OutgoingMedia::Photo {
                path: "/tmp/secret.jpg".to_owned(),
                caption: FormattedText {
                    text: "for your eyes only".to_owned(),
                    entities: vec![],
                },
            };
            let optimistic = spy.send_media(chat_id, None, media.clone()).await.unwrap();

            assert_eq!(*spy.sent.borrow(), Some((chat_id, None, media)));
            assert_eq!(optimistic.send_state, SendState::Pending);
        }

        #[test]
        fn media_in_a_secret_chat_reconciles_and_keeps_its_content() {
            let (chat, chat_id) = ready_secret_chat();
            assert!(chat.is_ready());

            // The optimistic photo lands Pending under a temp id, carrying photo
            // content, on the secret chat's chat id — no special handling.
            let mut store = MessageStore::new();
            store.reduce(&Update::NewMessage(UpdateNewMessage {
                message: td_photo_message(
                    chat_id,
                    5001,
                    "pic",
                    Some(MessageSendingState::Pending(
                        MessageSendingStatePending::default(),
                    )),
                ),
            }));
            let pending = store.get(chat_id, 5001).unwrap();
            assert_eq!(pending.send_state, SendState::Pending);
            assert!(matches!(pending.content, MessageContent::Photo(_)));

            // Success swaps the temp id for the server's, in place, and the photo
            // content survives — identical to a non-secret chat, no special routing.
            store.reduce(&Update::MessageSendSucceeded(UpdateMessageSendSucceeded {
                message: td_photo_message(chat_id, 8001, "pic", None),
                old_message_id: 5001,
            }));
            assert!(store.get(chat_id, 5001).is_none());
            let settled = store.get(chat_id, 8001).unwrap();
            assert_eq!(settled.send_state, SendState::Sent);
            assert!(matches!(settled.content, MessageContent::Photo(_)));
            assert_eq!(store.count(chat_id), 1);
        }
    }
}
