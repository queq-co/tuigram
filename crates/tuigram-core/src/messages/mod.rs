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
//! temporary id in [`SendState::Pending`](crate::model::SendState::Pending); the reducer then folds the lifecycle —
//! `updateMessageSendSucceeded` swaps the temp id for the server's real one,
//! `updateMessageSendFailed` flips the same entry to [`SendState::Failed`](crate::model::SendState::Failed) — so a
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
//!
//! Split (#182c) along the request/store seam: `requests` holds the
//! per-capability request traits, `Bridge`'s live implementation of each, and
//! the paging drivers; `store` holds the client-side [`MessageStore`] and the
//! transient search views. This file re-exports both so every existing
//! `crate::messages::X` path keeps resolving.

mod requests;
mod store;

pub use requests::{
    DeleteRequests, EditRequests, FormatRequests, ForwardRequests, HistoryRequests,
    MessageRequests, NEWEST, PinRequests, ReactionRequests, ReadRequests, SearchRequests,
    SendRequests, edit_formatted_text, load_history, search_chat, search_global,
    send_formatted_text,
};
pub use store::{MessageStore, SearchPage, SearchResults};
