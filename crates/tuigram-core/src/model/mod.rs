//! tuigram's normalized headless data model.
//!
//! The rest of the crate — and the Ratatui front-end in Phases 4–5 — depends on
//! these types, not on `tdlib_rs` shapes directly. That is the same insulation
//! [`AuthState`](crate::auth::AuthState) gives the login flow: a stable, minimal
//! surface we own, projected from `TDLib` by a `from_tdlib` constructor.
//!
//! Each projection is **total** over its `TDLib` enum. Anything Phase 3 does not
//! model maps to an explicit `Unsupported(name)` carrying `TDLib`'s own type name,
//! and the projections use no catch-all `_` arm — so a `tdlib-rs` upgrade that
//! adds a variant fails to compile here until it is classified, never silently
//! dropped or misclassified.
//!
//! The model covers **text** in full (with its formatting entities), the common
//! file-backed **media** types ([`Photo`], [`Video`], [`Document`], [`Audio`],
//! [`Voice`], [`Sticker`], [`Animation`]), and the **structured** types
//! ([`Location`], [`Venue`], [`Contact`], [`Poll`]); rarer content is
//! `Unsupported`, for follow-up issues. Message **reactions** are modeled (#51,
//! see [`Reaction`]) and so are **replies** (#210, see [`ReplyTo`]); forwards
//! and service messages are still out of scope for this model.
//!
//! Split by domain (#182a) rather than kept as one file: [`user`], [`chat`],
//! [`richtext`], [`media`], [`content`], [`message`]. Every path that was
//! previously `tuigram_core::model::X` still resolves — this module re-exports
//! each domain's public items under the same names.

mod chat;
mod content;
mod media;
mod message;
mod richtext;
mod user;

#[cfg(test)]
mod test_support;

pub use chat::{
    Chat, ChatFolderInfo, ChatKind, ChatListKind, ChatPosition, SecretChat, SecretChatState,
};
pub use content::{Contact, Location, Poll, PollKind, PollOption, Venue};
pub use media::{Animation, Audio, Document, File, FileRef, Photo, Sticker, Video, Voice};
pub(crate) use message::reactions_from;
pub use message::{
    Draft, Message, MessageContent, OutgoingMedia, Reaction, ReactionKind, ReplyTo, SendState,
};
pub use richtext::{EntityKind, FormattedText, TextEntity};
pub use user::{ChatAction, Presence, Sender, User, UserKind};
