//! Render functions grouped by pane/overlay (#182b). [`super::ui`] is the
//! orchestrator; each submodule owns one pane or overlay group and the
//! primitives (`common`) more than one of them shares.

mod chat_list;
mod common;
mod composer;
mod conversation;
mod help;
mod overlays;
mod status;

#[cfg(test)]
pub(super) mod test_support;

pub(super) use chat_list::render_chat_list;
pub(super) use composer::render_composer;
pub(super) use conversation::{convo_body_width, render_conversation};
pub(super) use help::render_help;
pub(super) use overlays::{
    render_contact_search_input, render_contact_search_results, render_delete_confirm,
    render_forward, render_logout_confirm, render_reaction, render_search_input,
    render_search_results, render_secret_chat, render_send_media, render_settings,
};
pub(super) use status::{render_status_bar, render_toast};

// Re-exported further up to crate scope by `ui.rs` — `lib.rs` and `login.rs`
// call these as `crate::ui::{message_lines, media_cols, hint_line, input_line}`.
pub(crate) use common::{hint_line, input_line};
pub(crate) use conversation::media_cols;
pub use conversation::message_lines;
