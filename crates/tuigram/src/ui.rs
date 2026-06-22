//! The render function: a pure projection of [`App`] onto a `Frame`. It reads an
//! `App` snapshot and never awaits — all blocking lives below the UI, so the
//! draw path stays synchronous and `TestBackend`-snapshottable.
//!
//! This is the three-pane chat skeleton (issue #79): an outer horizontal split
//! of a **chat list** (left) and a **conversation** (right), with the right pane
//! split vertically into a scrolling **message history** over a fixed-height
//! **composer**. The chat-list pane (issue #80), the conversation history
//! (issue #81), and the composer (issue #82) are live, each writing its tests
//! against the `TestBackend` harness below.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Margin, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Clear, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState,
};

use tuigram_core::model::{
    Chat, File, FileRef, FormattedText, Message, MessageContent, ReactionKind, Sender,
};

use crate::app::App;
use crate::composer::ComposerMode;
use crate::conversation::ConversationView;
use crate::keymap::{self, Focus, Overlay};
use crate::mediaform::MediaField;

/// Chat-list pane width, as a percentage of the terminal; the conversation pane
/// fills the remainder. (The research doc allows fixed *or* percentage width;
/// percentage keeps the skeleton responsive across terminal sizes.)
const CHAT_LIST_PERCENT: u16 = 30;

/// Composer height in rows: one input line framed by a border.
const COMPOSER_HEIGHT: u16 = 3;

/// Marker drawn to the left of the selected chat row.
const SELECTED_SYMBOL: &str = "▶ ";

/// Hint shown in the composer while its buffer is empty.
const COMPOSER_PLACEHOLDER: &str = "type a message…";

/// Marker prefixed to the focused pane's border title.
const FOCUS_MARKER: &str = "●";

/// Render the whole UI for one frame from the current `App` state.
pub fn ui(frame: &mut Frame, app: &App) {
    // Outer split: chat list | conversation (fills the rest).
    let [list_area, convo_area] = Layout::horizontal([
        Constraint::Percentage(CHAT_LIST_PERCENT),
        Constraint::Min(0),
    ])
    .areas(frame.area());

    // Conversation split: message history over a fixed composer line.
    let [history_area, composer_area] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(COMPOSER_HEIGHT)])
            .areas(convo_area);

    render_chat_list(frame, list_area, app);
    render_conversation(frame, history_area, app);
    render_composer(frame, composer_area, app);

    // A modal overlay floats above the panes, capturing input while open.
    match app.overlay() {
        Overlay::None => {}
        Overlay::Help => render_help(frame, frame.area()),
        Overlay::SearchInput => render_search_input(frame, frame.area(), app),
        Overlay::SearchResults => render_search_results(frame, frame.area(), app),
        Overlay::Forward => render_forward(frame, frame.area(), app),
        Overlay::Reaction => render_reaction(frame, frame.area(), app),
        Overlay::SendMedia => render_send_media(frame, frame.area(), app),
    }
}

/// A pane's bordered block, with the focus highlight applied when `focused`: a
/// marker prefixed to the title and a bold border, so the active pane is obvious.
fn pane_block(title: String, focused: bool) -> Block<'static> {
    let block = Block::bordered();
    if focused {
        block
            .title(format!("{FOCUS_MARKER}{title}"))
            .border_style(Style::new().add_modifier(Modifier::BOLD))
    } else {
        block.title(title)
    }
}

/// Left pane: the chat list (#80). Renders the active list's chats — each a title
/// with an unread badge — under a title naming the active list, with the selected
/// row highlighted. An empty list shows a placeholder. List switching and moving
/// the selection are driven through [`App`]'s reducer by the keymap.
fn render_chat_list(frame: &mut Frame, area: Rect, app: &App) {
    let view = app.chat_list();
    let block = pane_block(
        format!(" Chats — {} ", view.active_label()),
        app.focus() == Focus::ChatList,
    );

    let chats = view.active_chats();
    if chats.is_empty() {
        frame.render_widget(Paragraph::new("(no chats yet)").block(block), area);
        return;
    }

    let items: Vec<ListItem> = chats.iter().map(chat_item).collect();
    let list = List::new(items)
        .block(block)
        .highlight_symbol(SELECTED_SYMBOL)
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED));

    // A fresh `ListState` each frame: the selection comes from `App`, and ratatui
    // scrolls the offset to keep it visible — so long lists window themselves
    // without the (immutable) render path holding mutable scroll state.
    let mut state = ListState::default().with_selected(Some(view.selected()));
    frame.render_stateful_widget(list, area, &mut state);
}

/// One chat row: the title, plus a bold unread badge when the chat has unread
/// incoming messages.
fn chat_item(chat: &Chat) -> ListItem<'static> {
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

/// Right/top pane: the conversation history (#81). Renders the open chat's
/// messages — each a sender/timestamp header, a body or media placeholder, and a
/// reaction line — windowed forward from the scroll offset so a long history never
/// builds the whole buffer, with a scrollbar tracking the offset. An empty history
/// (the Phase 5 pre-data state) keeps the welcome/liveness placeholder.
fn render_conversation(frame: &mut Frame, area: Rect, app: &App) {
    let view = app.conversation();
    if view.is_empty() {
        render_conversation_placeholder(frame, area, app);
        return;
    }

    // Window forward from the offset: format messages until the visible rows are
    // full, building at most one message past the boundary — never the whole
    // history. `inner` excludes the block's top and bottom borders.
    let inner_rows = area.height.saturating_sub(2) as usize;
    let mut lines: Vec<Line> = Vec::new();
    // The message at the offset (the first built) is the selected one — the
    // cursor the reaction/pin affordances act on — so it carries the marker.
    for (i, message) in view.messages().iter().skip(view.offset()).enumerate() {
        if lines.len() >= inner_rows {
            break;
        }
        lines.extend(message_lines(view, message, i == 0));
    }
    lines.truncate(inner_rows);

    let block = pane_block(" Conversation ".to_owned(), app.focus() == Focus::History);
    let history = Paragraph::new(lines).block(block);
    frame.render_widget(history, area);

    // The scrollbar tracks the message offset, inset one row so it rides the
    // right border between the block's corners.
    let mut scrollbar_state = ScrollbarState::new(view.len()).position(view.offset());
    frame.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None),
        area.inner(Margin {
            vertical: 1,
            horizontal: 0,
        }),
        &mut scrollbar_state,
    );
}

/// The pre-data conversation pane: a welcome banner that doubles as the liveness
/// view, echoing the core heartbeat count until real history (Phase 6) replaces
/// it and the status bar (#88) takes over the heartbeat/quit hint.
fn render_conversation_placeholder(frame: &mut Frame, area: Rect, app: &App) {
    let body = format!(
        "tuigram — Phase 5 TUI skeleton\n\ncore heartbeats: {}\n\npress ? for help · q / Ctrl-C to quit",
        app.beats()
    );
    let block = pane_block(" tuigram ".to_owned(), app.focus() == Focus::History)
        .title_alignment(Alignment::Center);
    let widget = Paragraph::new(body)
        .alignment(Alignment::Center)
        .block(block);
    frame.render_widget(widget, area);
}

/// The lines for one message: a bold sender/timestamp header (with a selection
/// marker when this is the cursor message and a pin marker when pinned), the body
/// or a media placeholder, a download-progress line for media being fetched, an
/// optional reaction line, and a blank separator below.
fn message_lines(view: &ConversationView, message: &Message, selected: bool) -> Vec<Line<'static>> {
    let mut header = String::new();
    if selected {
        header.push_str(SELECTED_SYMBOL);
    }
    if view.is_pinned(message.id) {
        header.push_str("📌 ");
    }
    header.push_str(&sender_label(message));
    header.push_str("  ");
    header.push_str(&hour_minute(message.date));

    let mut lines = vec![Line::from(Span::styled(
        header,
        Style::new().add_modifier(Modifier::BOLD),
    ))];
    lines.extend(content_lines(&message.content));
    if let Some(progress) = download_line(view, &message.content) {
        lines.push(progress);
    }
    if let Some(reactions) = reaction_line(message) {
        lines.push(reactions);
    }
    lines.push(Line::from(""));
    lines
}

/// The file a media message references, if any — the key into the download store
/// for the progress indicator. Non-file content (text, location, poll, …) has none.
fn content_file(content: &MessageContent) -> Option<FileRef> {
    match content {
        MessageContent::Photo(p) => Some(p.file),
        MessageContent::Video(v) => Some(v.file),
        MessageContent::Document(d) => Some(d.file),
        MessageContent::Audio(a) => Some(a.file),
        MessageContent::Voice(v) => Some(v.file),
        MessageContent::Sticker(s) => Some(s.file),
        MessageContent::Animation(a) => Some(a.file),
        MessageContent::Text(_)
        | MessageContent::Location(_)
        | MessageContent::Venue(_)
        | MessageContent::Contact(_)
        | MessageContent::Poll(_)
        | MessageContent::Unsupported(_) => None,
    }
}

/// The download-progress line for a media message, driven by the file's transfer
/// state: a dim percentage while a download is active, a saved marker once it is
/// present, or `None` when the file is unknown or not being fetched.
fn download_line(view: &ConversationView, content: &MessageContent) -> Option<Line<'static>> {
    let file = view.download(content_file(content)?.id)?;
    let text = if file.is_downloading_active {
        format!("⬇ downloading {}%", percent(file))
    } else if file.is_present() {
        "✓ saved".to_owned()
    } else {
        return None;
    };
    Some(Line::from(Span::styled(
        text,
        Style::new().add_modifier(Modifier::DIM),
    )))
}

/// A file's download progress as a whole percentage of its best-known total size,
/// clamped to 0–100; `0` when the total is unknown.
fn percent(file: &File) -> i64 {
    let total = file.total_size();
    if total <= 0 {
        return 0;
    }
    (file.downloaded_size * 100 / total).clamp(0, 100)
}

/// The header's name for a message: "You" for our own messages, else the sender's
/// id. Resolving ids to display names needs the user/chat store, which Phase 6
/// wires; until then the id keeps the header unambiguous.
fn sender_label(message: &Message) -> String {
    if message.is_outgoing {
        return "You".to_owned();
    }
    match message.sender {
        Sender::User(id) => format!("User {id}"),
        Sender::Chat(id) => format!("Chat {id}"),
    }
}

/// Format a Unix timestamp as `HH:MM` in UTC. Local-time conversion needs a
/// timezone database the core does not carry yet (a follow-up); UTC keeps the
/// header deterministic and snapshot-testable in the meantime.
fn hour_minute(date: i32) -> String {
    let seconds = i64::from(date).rem_euclid(86_400);
    format!("{:02}:{:02}", seconds / 3600, (seconds % 3600) / 60)
}

/// The body lines for a message's content: the text for a text message, or a
/// `[Kind]` placeholder for media (with its caption, when set, on the lines
/// below). Media bytes are not rendered in a terminal; the placeholder names what
/// the message carries.
fn content_lines(content: &MessageContent) -> Vec<Line<'static>> {
    match content {
        MessageContent::Text(text) => text_lines(text),
        MessageContent::Photo(p) => placeholder_lines("[Photo]", &p.caption),
        MessageContent::Video(v) => placeholder_lines("[Video]", &v.caption),
        MessageContent::Document(d) => placeholder_lines(
            &format!("[Document {}]", trimmed_name(&d.file_name)),
            &d.caption,
        ),
        MessageContent::Audio(a) => placeholder_lines("[Audio]", &a.caption),
        MessageContent::Voice(v) => placeholder_lines("[Voice]", &v.caption),
        MessageContent::Sticker(s) => one_line(format!("[Sticker {}]", s.emoji).trim_end()),
        MessageContent::Animation(a) => placeholder_lines("[GIF]", &a.caption),
        MessageContent::Location(_) => one_line("[Location]"),
        MessageContent::Venue(v) => one_line(format!("[Venue {}]", v.title).trim_end()),
        MessageContent::Contact(c) => {
            one_line(format!("[Contact {} {}]", c.first_name, c.last_name).trim_end())
        }
        MessageContent::Poll(p) => one_line(format!("[Poll] {}", p.question.text)),
        MessageContent::Unsupported(name) => one_line(format!("[{name}]")),
    }
}

/// The lines of a text body, preserving its own line breaks. Empty text still
/// yields one (empty) line so the header is not left bodyless.
fn text_lines(text: &FormattedText) -> Vec<Line<'static>> {
    text.text
        .split('\n')
        .map(|line| Line::from(line.to_owned()))
        .collect()
}

/// A media placeholder line, with the caption's lines below it when non-empty.
fn placeholder_lines(label: &str, caption: &FormattedText) -> Vec<Line<'static>> {
    let mut lines = one_line(label);
    if !caption.text.is_empty() {
        lines.extend(text_lines(caption));
    }
    lines
}

/// A single owned line from a string.
fn one_line(text: impl Into<String>) -> Vec<Line<'static>> {
    vec![Line::from(text.into())]
}

/// A file name with surrounding whitespace removed, for the document placeholder.
fn trimmed_name(name: &str) -> &str {
    name.trim()
}

/// The reaction line for a message, or `None` when it has none. Each bucket reads
/// `{emoji×count}`, with a trailing `*` inside the braces for the reaction this
/// account chose, and chosen buckets drawn bold.
fn reaction_line(message: &Message) -> Option<Line<'static>> {
    if message.reactions.is_empty() {
        return None;
    }
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (i, reaction) in message.reactions.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" "));
        }
        let chosen = if reaction.is_chosen { "*" } else { "" };
        let chip = format!(
            "{{{}×{}{}}}",
            reaction_symbol(&reaction.kind),
            reaction.count,
            chosen
        );
        let style = if reaction.is_chosen {
            Style::new().add_modifier(Modifier::BOLD)
        } else {
            Style::new()
        };
        spans.push(Span::styled(chip, style));
    }
    Some(Line::from(spans))
}

/// The glyph shown for a reaction bucket: the emoji itself, a star for the paid
/// channel reaction, or a generic marker for a custom emoji (its sticker can't be
/// drawn in a terminal).
fn reaction_symbol(kind: &ReactionKind) -> String {
    match kind {
        ReactionKind::Emoji(emoji) => emoji.clone(),
        ReactionKind::Paid => "⭐".to_owned(),
        ReactionKind::CustomEmoji(_) => "🧩".to_owned(),
    }
}

/// Right/bottom pane: the message composer (#82). The border title is the mode
/// indicator — " Message " when composing, the reply target when replying, an edit
/// marker when editing — and the inner line is the input: a dim placeholder while
/// empty, otherwise the text with a reverse-video block marking the cursor.
fn render_composer(frame: &mut Frame, area: Rect, app: &App) {
    let composer = app.composer();
    let block = pane_block(
        composer_title(composer.mode()),
        app.focus() == Focus::Composer,
    );

    let line = if composer.is_empty() {
        Line::from(Span::styled(
            COMPOSER_PLACEHOLDER,
            Style::new().add_modifier(Modifier::DIM),
        ))
    } else {
        input_line(composer.text(), composer.cursor())
    };
    frame.render_widget(Paragraph::new(line).block(block), area);
}

/// The composer's border title, doubling as the mode indicator: a plain label when
/// composing, the reply target when replying (so the user sees which message), and
/// an edit marker when editing the prefilled buffer.
fn composer_title(mode: &ComposerMode) -> String {
    match mode {
        ComposerMode::Compose => " Message ".to_owned(),
        ComposerMode::Reply { preview, .. } => format!(" Reply ↩ {} ", truncate(preview, 40)),
        ComposerMode::Edit { .. } => " Edit ✎ ".to_owned(),
    }
}

/// The input line with a visible cursor: the text up to the cursor, then the
/// character under it (or a trailing space at end-of-line) drawn reverse-video so
/// the caret shows in the `TestBackend` buffer, then the remainder.
fn input_line(text: &str, cursor: usize) -> Line<'static> {
    let chars: Vec<char> = text.chars().collect();
    let cursor = cursor.min(chars.len());
    let cursor_style = Style::new().add_modifier(Modifier::REVERSED);

    let left: String = chars[..cursor].iter().collect();
    let mut spans = vec![Span::raw(left)];
    match chars.get(cursor) {
        Some(&c) => {
            spans.push(Span::styled(c.to_string(), cursor_style));
            let right: String = chars[cursor + 1..].iter().collect();
            if !right.is_empty() {
                spans.push(Span::raw(right));
            }
        }
        None => spans.push(Span::styled(" ".to_owned(), cursor_style)),
    }
    Line::from(spans)
}

/// The help overlay: a centred, bordered popup listing the active key bindings,
/// generated from the keymap so it always matches what the keys actually do.
fn render_help(frame: &mut Frame, area: Rect) {
    let lines = help_lines();
    let content_width = lines.iter().map(Line::width).max().unwrap_or(0) as u16;
    let popup = centered_rect(content_width + 4, lines.len() as u16 + 2, area);
    // `Clear` wipes the panes underneath so the overlay reads as a modal.
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .title(" Help ")
                .title_alignment(Alignment::Center),
        ),
        popup,
    );
}

/// The help overlay's body, one block per [`keymap::HelpSection`]: a bold heading
/// then each binding's keys and description.
fn help_lines() -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for (i, section) in keymap::help_sections().into_iter().enumerate() {
        if i > 0 {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(Span::styled(
            section.title,
            Style::new().add_modifier(Modifier::BOLD),
        )));
        for entry in section.entries {
            lines.push(Line::from(format!(
                "  {:<13}{}",
                entry.keys, entry.description
            )));
        }
    }
    lines
}

/// A `width × height` rectangle centred within `area`, clamped to fit it.
fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height.saturating_sub(h) / 2,
        width: w,
        height: h,
    }
}

/// Width of the search/forward modal popups, clamped to the terminal by
/// [`centered_rect`].
const OVERLAY_WIDTH: u16 = 56;

/// A dim hint line, for the key reminder along the bottom of a modal.
fn hint_line(hint: &'static str) -> Line<'static> {
    Line::from(Span::styled(hint, Style::new().add_modifier(Modifier::DIM)))
}

/// The search query line (#84): a centred modal with the editable query over a key
/// hint. The query reuses the composer's [`input_line`] so the cursor renders
/// identically; an empty query shows a dim prompt instead.
fn render_search_input(frame: &mut Frame, area: Rect, app: &App) {
    let search = app.search();
    let query = if search.query().is_empty() {
        Line::from(Span::styled(
            "type to search messages…",
            Style::new().add_modifier(Modifier::DIM),
        ))
    } else {
        input_line(search.query(), search.cursor())
    };
    let lines = vec![
        query,
        Line::from(""),
        hint_line("Enter to search · Esc to cancel"),
    ];

    let popup = centered_rect(OVERLAY_WIDTH, lines.len() as u16 + 2, area);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .title(" Search ")
                .title_alignment(Alignment::Center),
        ),
        popup,
    );
}

/// The search results overlay (#84): a centred modal listing the hits — a separate
/// view over the conversation, never a rewrite of the history pane — with the
/// selected hit marked. An empty result set shows a "no matches" note.
fn render_search_results(frame: &mut Frame, area: Rect, app: &App) {
    let search = app.search();
    let title = format!(
        " Results — \"{}\" ({}) ",
        truncate(search.query(), 30),
        search.results().len()
    );
    if search.results().is_empty() {
        let popup = centered_rect(OVERLAY_WIDTH, 3, area);
        frame.render_widget(Clear, popup);
        frame.render_widget(
            Paragraph::new("no matches").block(
                Block::bordered()
                    .title(title)
                    .title_alignment(Alignment::Center),
            ),
            popup,
        );
        return;
    }

    let items: Vec<ListItem> = search
        .results()
        .iter()
        .map(|hit| ListItem::new(hit.preview.clone()))
        .collect();
    render_list_modal(
        frame,
        area,
        title,
        items,
        search.selected(),
        "j / k move · f forward · Esc close",
    );
}

/// The forward target picker (#84): a centred modal that **reuses the chat-list
/// widget** to choose where the selected message(s) go, with a key hint along the
/// bottom.
fn render_forward(frame: &mut Frame, area: Rect, app: &App) {
    let forward = app.forward();
    let title = format!(" Forward {} message(s) to… ", forward.count());
    let items: Vec<ListItem> = forward
        .targets()
        .active_chats()
        .iter()
        .map(chat_item)
        .collect();
    render_list_modal(
        frame,
        area,
        title,
        items,
        forward.targets().selected(),
        "j / k pick · Enter send · Esc cancel",
    );
}

/// The reaction picker (#85): a centred modal listing the emoji palette with the
/// selected one marked. Confirming toggles it on the selected message.
fn render_reaction(frame: &mut Frame, area: Rect, app: &App) {
    let picker = app.reaction();
    let items: Vec<ListItem> = picker
        .palette()
        .iter()
        .map(|emoji| ListItem::new((*emoji).to_owned()))
        .collect();
    render_list_modal(
        frame,
        area,
        " React ".to_owned(),
        items,
        picker.selected(),
        "j / k move · Enter toggle · Esc cancel",
    );
}

/// The send-media prompt (#85): a centred modal with a local-path field over an
/// optional caption field — paths only, never bytes — and a key hint. The focused
/// field shows the caret via the reused [`input_line`]; the other shows its text or
/// a dim placeholder.
fn render_send_media(frame: &mut Frame, area: Rect, app: &App) {
    let media = app.media();
    let lines = vec![
        media_field_line(
            "path",
            media.path(),
            media.field() == MediaField::Path,
            media.cursor(),
            "(local file path)",
        ),
        media_field_line(
            "caption",
            media.caption(),
            media.field() == MediaField::Caption,
            media.cursor(),
            "(optional)",
        ),
        Line::from(""),
        hint_line("Tab switch field · Enter send · Esc cancel"),
    ];

    let popup = centered_rect(OVERLAY_WIDTH, lines.len() as u16 + 2, area);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .title(" Send media ")
                .title_alignment(Alignment::Center),
        ),
        popup,
    );
}

/// One labelled field of the send-media prompt: a padded label then the value —
/// the focused field with a caret (via [`input_line`]), an unfocused empty field a
/// dim placeholder, otherwise the plain text.
fn media_field_line(
    label: &str,
    text: &str,
    focused: bool,
    cursor: usize,
    placeholder: &'static str,
) -> Line<'static> {
    let mut spans = vec![Span::styled(
        format!("{label:<9}"),
        Style::new().add_modifier(Modifier::BOLD),
    )];
    if focused {
        spans.extend(input_line(text, cursor).spans);
    } else if text.is_empty() {
        spans.push(Span::styled(
            placeholder,
            Style::new().add_modifier(Modifier::DIM),
        ));
    } else {
        spans.push(Span::raw(text.to_owned()));
    }
    Line::from(spans)
}

/// A centred modal holding a selectable list over a dim key hint — the shared shape
/// of the search-results and forward-target overlays. Sized to the items, clamped
/// to `area`.
fn render_list_modal(
    frame: &mut Frame,
    area: Rect,
    title: String,
    items: Vec<ListItem>,
    selected: usize,
    hint: &'static str,
) {
    // Border (2) + the hint row (1) frame the list rows.
    let height = items.len() as u16 + 3;
    let popup = centered_rect(OVERLAY_WIDTH, height, area);
    frame.render_widget(Clear, popup);

    let block = Block::bordered()
        .title(title)
        .title_alignment(Alignment::Center);
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let [list_area, hint_area] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(inner);

    let list = List::new(items)
        .highlight_symbol(SELECTED_SYMBOL)
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
    let mut state = ListState::default().with_selected(Some(selected));
    frame.render_stateful_widget(list, list_area, &mut state);
    frame.render_widget(Paragraph::new(hint_line(hint)), hint_area);
}

/// Shorten `s` to at most `max` characters, ending in an ellipsis when clipped, so
/// a long reply preview cannot overrun the composer's border title.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let head: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::{Buffer, Cell};

    /// The TTY-free snapshot harness: render a known `App` into an in-memory
    /// `Buffer` at a fixed size. Every later `ui:` issue asserts against this.
    fn render(app: &App, width: u16, height: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| ui(frame, app)).unwrap();
        terminal.backend().buffer().clone()
    }

    /// Whole-buffer text, for substring assertions on rendered content.
    fn flatten(buffer: &Buffer) -> String {
        buffer.content().iter().map(Cell::symbol).collect()
    }

    /// Text of a single buffer row, for positional (layout) assertions.
    fn row_text(buffer: &Buffer, y: u16) -> String {
        (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect()
    }

    #[test]
    fn renders_three_pane_skeleton() {
        let text = flatten(&render(&App::new(), 80, 24));
        assert!(text.contains("Chats"), "chat-list pane");
        assert!(text.contains("tuigram"), "conversation pane");
        assert!(text.contains("Message"), "composer pane");
        assert!(text.contains("quit"), "quit hint");
    }

    #[test]
    fn the_focused_pane_carries_the_focus_marker() {
        use crate::keymap::Focus;
        let mut app = App::new();
        app.dispatch(crate::app::Action::SetFocus(Focus::Composer));
        let buffer = render(&app, 80, 24);
        // The composer (focused) shows the marker on its border; the chat-list
        // title row (top, unfocused) does not.
        let composer_row = row_text(&buffer, 24 - COMPOSER_HEIGHT);
        assert!(composer_row.contains('●'), "focused composer is marked");
        assert!(
            !row_text(&buffer, 0).contains('●'),
            "unfocused chat list is unmarked"
        );
    }

    #[test]
    fn the_help_overlay_lists_bindings_when_toggled() {
        let mut app = App::new();
        assert!(
            !flatten(&render(&app, 80, 24)).contains(" Help "),
            "no overlay until toggled"
        );
        app.dispatch(crate::app::Action::ToggleHelp);
        let text = flatten(&render(&app, 80, 24));
        assert!(text.contains("Help"), "overlay title");
        assert!(text.contains("quit"), "documents a binding");
        assert!(
            text.contains("focus next pane"),
            "documents focus switching"
        );
    }

    #[test]
    fn shows_heartbeat_count() {
        let mut app = App::new();
        app.dispatch(crate::app::Action::Beat);
        app.dispatch(crate::app::Action::Beat);
        assert!(flatten(&render(&app, 80, 24)).contains("heartbeats: 2"));
    }

    #[test]
    fn chat_list_sits_on_the_left() {
        let buffer = render(&App::new(), 80, 24);
        let top = row_text(&buffer, 0);
        let chats_col = top.find("Chats").expect("Chats title on the top row");
        // The list title must fall inside the left ~30% of an 80-col terminal.
        assert!(chats_col < (80 * CHAT_LIST_PERCENT / 100) as usize);
    }

    #[test]
    fn composer_is_pinned_to_the_bottom() {
        let buffer = render(&App::new(), 80, 24);
        // The composer is the bottom COMPOSER_HEIGHT rows; its bordered title row
        // is the first of those.
        let composer_top = row_text(&buffer, 24 - COMPOSER_HEIGHT);
        assert!(composer_top.contains("Message"), "composer at the bottom");
    }

    // --- chat-list pane (#80) ---

    use crate::app::Action;
    use crate::chat_list::{ChatList, ChatListView, sample_chat};
    use tuigram_core::model::ChatListKind;

    fn chat_list(kind: ChatListKind, label: &str, titles: &[(&str, i32)]) -> ChatList {
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
    fn app_with_lists() -> App {
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

    /// Find the first buffer row whose text contains `needle`.
    fn row_containing(buffer: &Buffer, needle: &str) -> String {
        (0..buffer.area.height)
            .map(|y| row_text(buffer, y))
            .find(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("no row contains {needle:?}"))
    }

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

    // --- conversation / history pane (#81) ---

    use crate::conversation::{ConversationView, sample_message};
    use std::collections::HashSet;
    use tuigram_core::model::{FileRef, Photo, Reaction};

    /// A text message with the given id and body.
    fn text_message(id: i64, body: &str) -> Message {
        sample_message(
            id,
            MessageContent::Text(FormattedText {
                text: body.to_owned(),
                entities: Vec::new(),
            }),
        )
    }

    /// An app whose history holds `messages`, none pinned.
    fn app_with_history(messages: Vec<Message>) -> App {
        App::with_conversation(ConversationView::from_messages(messages, HashSet::new()))
    }

    #[test]
    fn empty_history_keeps_the_welcome_placeholder() {
        // With no open chat, the pane is the welcome/liveness view, not history.
        let text = flatten(&render(&App::new(), 80, 24));
        assert!(text.contains("tuigram — Phase 5"), "welcome banner");
        assert!(text.contains("quit"), "quit hint");
    }

    #[test]
    fn short_history_renders_each_message_body() {
        let app = app_with_history(vec![
            text_message(1, "hello there"),
            text_message(2, "general kenobi"),
        ]);
        let text = flatten(&render(&app, 80, 24));
        assert!(text.contains("Conversation"), "pane title");
        assert!(text.contains("hello there"), "first body");
        assert!(text.contains("general kenobi"), "second body");
    }

    #[test]
    fn long_history_is_windowed_not_fully_built() {
        // 100 messages into a short pane: only the top slice is built, so a
        // far-down message is never rendered (the whole buffer is not assembled).
        let messages = (0..100)
            .map(|i| text_message(i, &format!("msg-{i}")))
            .collect();
        let text = flatten(&render(&app_with_history(messages), 40, 10));
        assert!(text.contains("msg-0"), "top of the window present");
        assert!(!text.contains("msg-50"), "far-down message not built");
    }

    #[test]
    fn scrolling_down_advances_the_visible_window() {
        let messages = (0..100)
            .map(|i| text_message(i, &format!("msg-{i}")))
            .collect();
        let mut app = app_with_history(messages);
        for _ in 0..40 {
            app.dispatch(Action::ScrollDown);
        }
        let text = flatten(&render(&app, 40, 10));
        assert!(!text.contains("msg-0"), "scrolled past the top");
        assert!(text.contains("msg-40"), "new offset is visible");
    }

    #[test]
    fn a_reaction_renders_as_a_chip_with_a_chosen_marker() {
        let mut message = text_message(1, "nice");
        message.reactions = vec![Reaction {
            kind: ReactionKind::Emoji("👍".to_owned()),
            count: 3,
            is_chosen: true,
        }];
        let buffer = render(&app_with_history(vec![message]), 80, 24);
        // `{👍×3*}` — the `*` flags the reaction this account chose. A wide emoji
        // occupies two cells (the trailing one a space), so assert on the emoji
        // and the space-free `×3*` tail rather than their adjacency.
        let row = row_containing(&buffer, "×3*");
        assert!(row.contains('👍'), "reaction emoji");
        assert!(row.contains("×3*"), "count and chosen marker");
    }

    #[test]
    fn a_photo_renders_as_a_media_placeholder() {
        let photo = sample_message(
            1,
            MessageContent::Photo(Photo {
                caption: FormattedText::default(),
                file: FileRef::new(0),
                width: 0,
                height: 0,
            }),
        );
        let text = flatten(&render(&app_with_history(vec![photo]), 80, 24));
        assert!(text.contains("[Photo]"), "photo placeholder");
    }

    #[test]
    fn a_pinned_message_shows_the_pin_marker() {
        let view =
            ConversationView::from_messages(vec![text_message(7, "pinned")], HashSet::from([7]));
        let text = flatten(&render(&App::with_conversation(view), 80, 24));
        assert!(text.contains("📌"), "pin marker on the pinned message");
    }

    // --- composer / text input (#82) ---

    use crate::composer::Composer;

    /// A composer with `body` typed in and the cursor left at the end.
    fn typed_composer(body: &str) -> Composer {
        let mut composer = Composer::default();
        for c in body.chars() {
            composer.insert(c);
        }
        composer
    }

    /// The symbol of the reverse-video cursor cell on the composer's input row, if
    /// one is drawn. The input row sits just above the composer's bottom border.
    fn cursor_symbol(buffer: &Buffer) -> Option<String> {
        let y = buffer.area.height - 2;
        (0..buffer.area.width).find_map(|x| {
            let cell = &buffer[(x, y)];
            cell.modifier
                .contains(Modifier::REVERSED)
                .then(|| cell.symbol().to_owned())
        })
    }

    #[test]
    fn empty_composer_shows_the_placeholder_under_the_message_title() {
        let buffer = render(&App::with_composer(Composer::default()), 80, 24);
        let text = flatten(&buffer);
        assert!(text.contains("Message"), "compose-mode title");
        assert!(text.contains("type a message"), "empty placeholder");
        // No caret while empty — the placeholder owns the line.
        assert_eq!(cursor_symbol(&buffer), None);
    }

    #[test]
    fn typing_shows_the_text_with_the_cursor_on_the_character() {
        let mut composer = typed_composer("hi");
        // Put the caret on the 'i' so the cursor cell carries a stable symbol.
        composer.move_left();
        let buffer = render(&App::with_composer(composer), 80, 24);
        assert!(flatten(&buffer).contains("hi"), "typed text rendered");
        assert_eq!(
            cursor_symbol(&buffer).as_deref(),
            Some("i"),
            "cursor highlights the character it sits on"
        );
    }

    #[test]
    fn reply_mode_names_the_target_in_the_indicator() {
        let mut composer = Composer::default();
        composer.reply_to(7, "User 7: general kenobi".to_owned());
        let text = flatten(&render(&App::with_composer(composer), 80, 24));
        assert!(text.contains("Reply"), "reply indicator");
        assert!(
            text.contains("User 7: general kenobi"),
            "the replied-to message"
        );
    }

    #[test]
    fn edit_mode_prefills_the_buffer_and_marks_the_indicator() {
        let mut composer = Composer::default();
        composer.edit(9, "old message".to_owned());
        let buffer = render(&App::with_composer(composer), 80, 24);
        let text = flatten(&buffer);
        assert!(text.contains("Edit"), "edit indicator");
        assert!(text.contains("old message"), "prefilled buffer");
        // The prefilled buffer is non-empty, so the caret is drawn.
        assert!(
            cursor_symbol(&buffer).is_some(),
            "cursor on the prefilled text"
        );
    }

    // --- search & forward overlays (#84) ---

    use crate::search::SearchHit;

    /// An app sitting on the search results overlay: two chats and two hits, after
    /// opening search and submitting. The state a forward is started from.
    fn app_on_results() -> App {
        let mut app = app_with_lists(); // Main: Alice/Bob/Carol, Archive: Old Friend
        app.dispatch(Action::SearchOpen);
        app.inject_search_results(vec![
            SearchHit::new(1, 10, "Alice: hello there"),
            SearchHit::new(2, 20, "Bob: general kenobi"),
        ]);
        app.dispatch(Action::SearchSubmit);
        app
    }

    #[test]
    fn the_search_input_overlay_shows_the_typed_query() {
        let mut app = App::new();
        app.dispatch(Action::SearchOpen);
        for c in "kenobi".chars() {
            app.dispatch(Action::SearchInput(c));
        }
        let text = flatten(&render(&app, 80, 24));
        assert!(text.contains("Search"), "search overlay title");
        assert!(text.contains("kenobi"), "the typed query");
        assert!(text.contains("Enter to search"), "key hint");
    }

    #[test]
    fn the_search_input_overlay_prompts_while_the_query_is_empty() {
        let mut app = App::new();
        app.dispatch(Action::SearchOpen);
        let text = flatten(&render(&app, 80, 24));
        assert!(text.contains("type to search"), "empty-query prompt");
    }

    #[test]
    fn the_results_overlay_lists_hits_as_a_separate_view() {
        let buffer = render(&app_on_results(), 80, 24);
        let text = flatten(&buffer);
        assert!(text.contains("Results"), "results overlay title");
        assert!(text.contains("(2)"), "hit count in the title");
        assert!(text.contains("Alice: hello there"), "first hit");
        assert!(text.contains("Bob: general kenobi"), "second hit");
        // The selected (first) hit carries the marker; navigation moves it.
        assert!(
            row_containing(&buffer, "Alice: hello there").contains('▶'),
            "selected hit marked"
        );
    }

    #[test]
    fn the_results_overlay_reports_no_matches_when_empty() {
        let mut app = App::new();
        app.dispatch(Action::SearchOpen);
        app.dispatch(Action::SearchSubmit); // no hits injected
        let text = flatten(&render(&app, 80, 24));
        assert!(text.contains("Results"), "results overlay title");
        assert!(text.contains("no matches"), "empty-results note");
    }

    #[test]
    fn the_forward_picker_reuses_the_chat_list_as_targets() {
        let mut app = app_on_results();
        app.dispatch(Action::ForwardOpen);
        let buffer = render(&app, 80, 24);
        let text = flatten(&buffer);
        assert!(text.contains("Forward"), "forward overlay title");
        assert!(text.contains("1 message"), "count of messages forwarded");
        // The picker shows the chat list's chats as destinations, first selected.
        assert!(text.contains("Alice"), "target chat from the chat list");
        assert!(text.contains("Bob"), "another target chat");
        assert!(
            row_containing(&buffer, "Alice").contains('▶'),
            "first target selected"
        );
        assert!(text.contains("Enter send"), "key hint");
    }

    // --- media, reactions & pins (#85) ---

    #[test]
    fn the_selected_history_message_carries_the_cursor_marker() {
        let buffer = render(
            &app_with_history(vec![text_message(1, "first"), text_message(2, "second")]),
            80,
            24,
        );
        // The cursor sits on the top (offset) message; the marker is on its header.
        assert!(
            row_containing(&buffer, "User 1").contains('▶'),
            "selected message marked"
        );
        assert!(
            !row_containing(&buffer, "User 2").contains('▶'),
            "unselected message unmarked"
        );
    }

    #[test]
    fn a_media_download_in_progress_shows_a_percentage() {
        let photo = sample_message(
            1,
            MessageContent::Photo(Photo {
                caption: FormattedText::default(),
                file: FileRef::new(7),
                width: 0,
                height: 0,
            }),
        );
        let mut view = ConversationView::from_messages(vec![photo], HashSet::new());
        view.set_download(File {
            id: 7,
            size: 100,
            downloaded_size: 45,
            is_downloading_active: true,
            ..File::default()
        });
        let text = flatten(&render(&App::with_conversation(view), 80, 24));
        assert!(text.contains("[Photo]"), "media placeholder");
        assert!(
            text.contains("downloading 45%"),
            "download progress indicator"
        );
    }

    #[test]
    fn the_reaction_picker_lists_the_emoji_palette() {
        let mut app = app_with_history(vec![text_message(1, "nice")]);
        app.dispatch(Action::ReactionOpen);
        let buffer = render(&app, 80, 24);
        let text = flatten(&buffer);
        assert!(text.contains("React"), "reaction overlay title");
        assert!(text.contains('👍'), "an emoji from the palette");
        assert!(text.contains("Enter toggle"), "key hint");
        // The first palette entry is selected.
        assert!(
            row_containing(&buffer, "👍").contains('▶'),
            "first emoji selected"
        );
    }

    #[test]
    fn the_send_media_prompt_shows_the_path_and_caption_fields() {
        let mut app = app_with_history(vec![text_message(1, "hi")]);
        app.dispatch(Action::AttachOpen);
        for c in "/tmp/a.png".chars() {
            app.dispatch(Action::AttachInput(c));
        }
        let text = flatten(&render(&app, 80, 24));
        assert!(text.contains("Send media"), "prompt title");
        assert!(text.contains("path"), "path field label");
        assert!(text.contains("/tmp/a.png"), "the typed path");
        assert!(text.contains("caption"), "caption field label");
        assert!(text.contains("(optional)"), "empty caption placeholder");
        assert!(text.contains("Tab switch"), "key hint");
    }
}
