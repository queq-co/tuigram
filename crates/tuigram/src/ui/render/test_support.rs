//! Test fixtures shared by more than one pane/overlay's test module: the
//! `TestBackend` snapshot harness and buffer-reading helpers, plus a few
//! `App`/`Message` builders reused across panes.

use std::collections::HashSet;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::{Buffer, Cell};

use tuigram_core::model::{
    ChatKind, ChatListKind, File, FormattedText, Message, MessageContent, SecretChatState,
};

use crate::app::App;
use crate::chat_list::{ChatList, ChatListView, sample_chat};
use crate::conversation::{ConversationView, sample_message};
use crate::ui::{RenderOutput, ui};

/// The TTY-free snapshot harness: render a known `App` into an in-memory
/// `Buffer` at a fixed size. Every later `ui:` issue asserts against this.
pub(crate) fn render(app: &App, width: u16, height: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| {
            ui(frame, app);
        })
        .unwrap();
    terminal.backend().buffer().clone()
}

/// Like [`render`], but returns the [`RenderOutput`] the frame measured
/// instead of the buffer — for tests on the pane rects and chat/message row
/// maps a click resolves against (#161/#162).
pub(crate) fn render_output(app: &App, width: u16, height: u16) -> RenderOutput {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    let mut output = RenderOutput::default();
    terminal
        .draw(|frame| {
            output = ui(frame, app);
        })
        .unwrap();
    output
}

/// Whole-buffer text, for substring assertions on rendered content.
pub(crate) fn flatten(buffer: &Buffer) -> String {
    buffer.content().iter().map(Cell::symbol).collect()
}

/// Text of a single buffer row, for positional (layout) assertions.
pub(crate) fn row_text(buffer: &Buffer, y: u16) -> String {
    (0..buffer.area.width)
        .map(|x| buffer[(x, y)].symbol())
        .collect()
}

/// Find the first buffer row whose text contains `needle`.
pub(super) fn row_containing(buffer: &Buffer, needle: &str) -> String {
    (0..buffer.area.height)
        .map(|y| row_text(buffer, y))
        .find(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("no row contains {needle:?}"))
}

/// A text message with the given id and body.
pub(super) fn text_message(id: i64, body: &str) -> Message {
    sample_message(
        id,
        MessageContent::Text(FormattedText {
            text: body.to_owned(),
            entities: Vec::new(),
        }),
    )
}

/// An app whose history holds `messages`, none pinned.
pub(super) fn app_with_history(messages: Vec<Message>) -> App {
    App::with_conversation(ConversationView::from_messages(messages, HashSet::new()))
}

/// The number of frame rows [`HistoryRows`](crate::ui::HistoryRows) maps to
/// `message_id` — the render-level counterpart to
/// [`ConversationView::message_height`], used below to check the
/// inline-media box's row growth without inspecting an `Image` widget's
/// actual pixel content (which `TestBackend` cannot meaningfully snapshot;
/// see `graphics_avatar_support_indents_the_header_by_the_gutter_width` for
/// the same limitation on the avatar path).
pub(super) fn rendered_row_count(output: &RenderOutput, message_id: i64) -> usize {
    (0..u16::MAX)
        .filter(|&row| output.history_rows.message_at(row) == Some(message_id))
        .count()
}

pub(super) fn graphics_picker() -> ratatui_image::picker::Picker {
    use ratatui_image::picker::{Picker, ProtocolType};
    let mut picker = Picker::halfblocks();
    picker.set_protocol_type(ProtocolType::Kitty);
    picker
}

pub(super) fn present_file(id: i32) -> File {
    File {
        id,
        size: 10,
        downloaded_size: 10,
        is_downloading_completed: true,
        local_path: format!("/tmp/{id}"),
        ..File::default()
    }
}

pub(super) fn chat_list(kind: ChatListKind, label: &str, titles: &[(&str, i32)]) -> ChatList {
    ChatList {
        kind,
        label: label.to_owned(),
        chats: titles
            .iter()
            .enumerate()
            .map(|(i, (t, unread))| sample_chat(i as i64, t, *unread))
            .collect(),
    }
}

/// An app whose chat-list pane holds a Main list and an Archive list.
pub(crate) fn app_with_lists() -> App {
    let view = ChatListView::from_lists(vec![
        chat_list(
            ChatListKind::Main,
            "Main",
            &[("Alice", 0), ("Bob", 3), ("Carol", 0)],
        ),
        chat_list(ChatListKind::Archive, "Archive", &[("Old Friend", 0)]),
    ]);
    App::with_chat_list(view)
}

/// A chat-list view holding one chat of `kind`, with an optional secret state.
pub(super) fn view_with_one_chat(
    title: &str,
    kind: ChatKind,
    state: Option<SecretChatState>,
) -> ChatListView {
    let mut chat = sample_chat(5, title, 0);
    chat.kind = kind;
    let mut view = ChatListView::from_lists(vec![ChatList {
        kind: ChatListKind::Main,
        label: "Main".to_owned(),
        chats: vec![chat],
    }]);
    if let Some(state) = state {
        view.set_secret_state(5, state);
    }
    view
}
