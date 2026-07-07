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
use ratatui::layout::{Alignment, Constraint, Layout, Margin, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Clear, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState,
};
use ratatui_image::Image;
use ratatui_image::protocol::Protocol;

use tuigram_core::model::{
    Chat, ChatAction, ChatKind, File, FormattedText, Message, MessageContent, ReactionKind,
    ReplyTo, SecretChatState, SendState, Sender, TextEntity,
};

use crate::chat_list::ChatListView;

use crate::app::App;
use crate::composer::ComposerMode;
use crate::conversation::ConversationView;
use crate::keymap::{self, Focus, Overlay};
use crate::mediaform::MediaField;
use crate::settingsform::SettingsField;
use crate::status::NoticeLevel;

/// Chat-list pane width, as a percentage of the terminal; the conversation pane
/// fills the remainder. (The research doc allows fixed *or* percentage width;
/// percentage keeps the skeleton responsive across terminal sizes.)
const CHAT_LIST_PERCENT: u16 = 30;

/// Composer height in rows: one input line framed by a border.
const COMPOSER_HEIGHT: u16 = 3;

/// Status-bar height in rows: a single strip across the bottom (#88).
const STATUS_HEIGHT: u16 = 1;

/// Marker drawn to the left of the selected chat row.
const SELECTED_SYMBOL: &str = "▶ ";

/// Hint shown in the composer while its buffer is empty.
const COMPOSER_PLACEHOLDER: &str = "type a message…";

/// Marker prefixed to the focused pane's border title.
const FOCUS_MARKER: &str = "●";

/// The four top-level pane rectangles a frame is laid out into — the single source
/// of truth for [`ui`]'s layout, recorded back onto `App` so a mouse event can be
/// hit-tested to a pane without re-running (or duplicating) the layout (#161/#162).
#[derive(Debug, Clone, Copy, Default)]
pub struct PaneLayout {
    /// Left pane: the chat list (#80).
    pub list: Rect,
    /// Right-top pane: the scrolling message history (#81).
    pub history: Rect,
    /// Right-bottom pane: the fixed-height composer (#82).
    pub composer: Rect,
    /// Bottom strip: the status bar (#88). Not a focus target.
    pub status: Rect,
}

impl PaneLayout {
    /// The focusable pane a point at `(col, row)` lands in, or `None` for the
    /// status bar or any gap. Focus-only per #161: the status strip and anything
    /// outside the three panes are not focus targets.
    #[must_use]
    pub fn focus_at(&self, col: u16, row: u16) -> Option<Focus> {
        let at = Position::new(col, row);
        if self.list.contains(at) {
            Some(Focus::ChatList)
        } else if self.history.contains(at) {
            Some(Focus::History)
        } else if self.composer.contains(at) {
            Some(Focus::Composer)
        } else {
            None
        }
    }
}

/// Split `area` into the four top-level pane rectangles. The one place the frame
/// layout is defined: [`ui`] renders into these rects and the loop records the same
/// ones on `App` (via [`RenderOutput`]) for mouse hit-testing, so what was drawn and
/// what is hit-tested can never drift.
pub fn pane_layout(area: Rect) -> PaneLayout {
    // Outer split: the three panes over a one-row status bar pinned to the bottom.
    let [content_area, status] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(STATUS_HEIGHT)]).areas(area);

    // Content split: chat list | conversation (fills the rest).
    let [list, convo_area] = Layout::horizontal([
        Constraint::Percentage(CHAT_LIST_PERCENT),
        Constraint::Min(0),
    ])
    .areas(content_area);

    // Conversation split: message history over a fixed composer line.
    let [history, composer] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(COMPOSER_HEIGHT)])
            .areas(convo_area);

    PaneLayout {
        list,
        history,
        composer,
        status,
    }
}

/// Row → chat id map for the chat-list pane, recorded from the last render so a
/// click on an actual chat row can open that chat directly rather than just
/// focusing the pane (extends #161/#162's pane-level hit-testing). Built from
/// the [`ListState`] offset the widget itself scrolled to during rendering, so
/// this always matches what was actually drawn even when the selection isn't at
/// the top of the viewport.
#[derive(Debug, Clone, Default)]
pub struct ChatRows(Vec<(u16, i64)>);

impl ChatRows {
    /// The chat drawn at frame row `row`, if any.
    #[must_use]
    pub fn chat_at(&self, row: u16) -> Option<i64> {
        self.0.iter().find(|&&(r, _)| r == row).map(|&(_, id)| id)
    }
}

/// Row-range → message id map for the history pane, recorded from the last
/// render so a click on an actual message row can select it directly (extends
/// #161/#162's pane-level hit-testing). Unlike [`ChatRows`], each message spans
/// a variable number of rows (header, content, reactions), so a hit is a row
/// *range* containment rather than an exact-row match.
#[derive(Debug, Clone, Default)]
pub struct HistoryRows(Vec<(u16, u16, i64)>);

impl HistoryRows {
    /// The message drawn at frame row `row`, if any.
    #[must_use]
    pub fn message_at(&self, row: u16) -> Option<i64> {
        self.0
            .iter()
            .find(|&&(start, end, _)| (start..end).contains(&row))
            .map(|&(_, _, id)| id)
    }
}

/// Row → list-index map for a modal overlay's selectable list (search results,
/// forward targets, the reaction palette, contact results), recorded from the
/// last render so a click on an actual row can select-and-confirm it directly
/// (#217; extends #161/#162's/#216's row-map pattern). A hit requires both the
/// row **and** the column to fall inside the popup's list area — the popup is
/// centred and narrower than the screen, and the panes underneath stay visible
/// (there is no full-screen backdrop), so a stray click at the same row but off
/// to the side must miss rather than resolve to that row's item.
#[derive(Debug, Clone, Default)]
pub struct OverlayRows {
    /// The list area's `[x, x + width)` column range.
    columns: (u16, u16),
    /// Row → index into the overlay's item list.
    rows: Vec<(u16, usize)>,
}

impl OverlayRows {
    /// The list index drawn at frame position `(col, row)`, if any.
    #[must_use]
    pub fn index_at(&self, col: u16, row: u16) -> Option<usize> {
        if col < self.columns.0 || col >= self.columns.1 {
            return None;
        }
        self.rows
            .iter()
            .find(|&&(r, _)| r == row)
            .map(|&(_, idx)| idx)
    }
}

/// What one render measured for the loop to record back onto `App`: the history
/// pane's inner height (#158), the pane rectangles for mouse hit-testing
/// (#161/#162), the chat/message row maps for click-to-open/click-to-select, and
/// the overlay row map for click-to-select in modal list overlays (#217). The
/// renderer stays a pure snapshot; the loop owns feeding these back through
/// [`App::set_conversation_viewport`](crate::app::App::set_conversation_viewport),
/// [`App::set_pane_layout`](crate::app::App::set_pane_layout),
/// [`App::set_chat_rows`](crate::app::App::set_chat_rows),
/// [`App::set_history_rows`](crate::app::App::set_history_rows), and
/// [`App::set_overlay_rows`](crate::app::App::set_overlay_rows).
#[derive(Debug, Clone, Default)]
pub struct RenderOutput {
    /// The history pane's inner height (rows) — the number of visible message rows
    /// the bottom-anchoring walk (#158) sums against.
    pub convo_viewport: usize,
    /// The history pane's inner body width (columns, #214) — see
    /// [`convo_body_width`] — the budget message bodies wrap against.
    pub convo_width: usize,
    /// The pane rectangles this frame was drawn into.
    pub panes: PaneLayout,
    /// Row → chat id map this frame drew.
    pub chat_rows: ChatRows,
    /// Row-range → message id map this frame drew.
    pub history_rows: HistoryRows,
    /// Row/column → list-index map for the open overlay, if any (empty when none
    /// is open, or the open one has no selectable list).
    pub overlay_rows: OverlayRows,
}

/// Render the whole UI for one frame from the current `App` state, returning what
/// the loop records back onto `App` (see [`RenderOutput`]).
pub fn ui(frame: &mut Frame, app: &App) -> RenderOutput {
    let panes = pane_layout(frame.area());

    let chat_rows = render_chat_list(frame, panes.list, app);
    let history_rows = render_conversation(frame, panes.history, app);
    render_composer(frame, panes.composer, app);
    render_status_bar(frame, panes.status, app);

    // A modal overlay floats above the panes, capturing input while open. Only
    // the list overlays (#217) return a non-empty row map; the rest render and
    // fall back to `OverlayRows::default()`, which hit-tests every click to
    // `None`.
    let overlay_rows = match app.overlay() {
        Overlay::None => OverlayRows::default(),
        Overlay::Help => {
            render_help(frame, frame.area(), app);
            OverlayRows::default()
        }
        Overlay::SearchInput => {
            render_search_input(frame, frame.area(), app);
            OverlayRows::default()
        }
        Overlay::SearchResults => render_search_results(frame, frame.area(), app),
        Overlay::Forward => render_forward(frame, frame.area(), app),
        Overlay::Reaction => render_reaction(frame, frame.area(), app),
        Overlay::SendMedia => {
            render_send_media(frame, frame.area(), app);
            OverlayRows::default()
        }
        Overlay::SecretChat => {
            render_secret_chat(frame, frame.area(), app);
            OverlayRows::default()
        }
        Overlay::Settings => {
            render_settings(frame, frame.area(), app);
            OverlayRows::default()
        }
        Overlay::DeleteConfirm => {
            render_delete_confirm(frame, frame.area(), app);
            OverlayRows::default()
        }
        Overlay::LogoutConfirm => {
            render_logout_confirm(frame, frame.area(), app);
            OverlayRows::default()
        }
        Overlay::ContactSearchInput => {
            render_contact_search_input(frame, frame.area(), app);
            OverlayRows::default()
        }
        Overlay::ContactSearchResults => render_contact_search_results(frame, frame.area(), app),
    };

    // A transient toast floats over the content too, but — unlike a modal overlay
    // — it never captures input, so the loop keeps responding while it shows. The
    // content region is the frame minus the bottom status strip.
    if app.notifications().current().is_some() {
        let content_area = Rect {
            height: frame.area().height.saturating_sub(STATUS_HEIGHT),
            ..frame.area()
        };
        render_toast(frame, content_area, app);
    }

    RenderOutput {
        // The history pane's inner height (excluding the block's top and bottom
        // borders) — the row budget the bottom-anchoring walk (#158) fits messages into.
        convo_viewport: panes.history.height.saturating_sub(2) as usize,
        convo_width: convo_body_width(panes.history, app.avatar_gutter_cols()),
        panes,
        chat_rows,
        history_rows,
        overlay_rows,
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
fn render_chat_list(frame: &mut Frame, area: Rect, app: &App) -> ChatRows {
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
fn delivery_glyph(send_state: &SendState, message_id: i64, last_read_outbox: i64) -> &'static str {
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
fn action_phrase(action: &ChatAction) -> &'static str {
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

/// The inline-media box's actual column width for a history pane `area_width`
/// cols wide with a `gutter_cols`-wide avatar gutter reserved: never wider than
/// [`crate::conversation::MEDIA_COLS`], and never wider than the pane can
/// actually draw into (borders + gutter subtracted). Shared by
/// `render_conversation` and `drive_inline_media`'s (`main.rs`) encode-time
/// sizing so the two can never drift out of sync — a mismatch here is exactly
/// what let `allow_clipping` (#222) silently crop the right edge on any
/// terminal narrower than `MEDIA_COLS` (#226).
pub(crate) fn media_cols(area_width: u16, gutter_cols: usize) -> usize {
    crate::conversation::MEDIA_COLS.min(
        (area_width as usize)
            .saturating_sub(2)
            .saturating_sub(gutter_cols),
    )
}

/// The conversation body's available column width (#214): the history pane's
/// `area` (borders included, same convention as `inner_rows` above) minus the
/// block's left/right borders and the avatar gutter, floored at `1` so a
/// pathologically narrow terminal still makes wrapping progress rather than
/// wrapping at `0`. `render_conversation` wraps message bodies against this,
/// and [`ui`] records the same value back onto `App` via
/// [`RenderOutput::convo_width`] — both call this one formula so the two can
/// never drift.
fn convo_body_width(area: Rect, gutter_cols: usize) -> usize {
    (area.width.saturating_sub(2 + gutter_cols as u16)).max(1) as usize
}

/// Right/top pane: the conversation history (#81). Renders the open chat's
/// messages — each a sender/timestamp header, a body or media placeholder, and a
/// reaction line — windowed forward from the scroll offset so a long history never
/// builds the whole buffer, with a scrollbar tracking the offset. With no chat open
/// the view is empty, so the pane falls through to the empty-state placeholder (#188).
fn render_conversation(frame: &mut Frame, area: Rect, app: &App) -> HistoryRows {
    let view = app.conversation();
    if view.is_empty() {
        render_conversation_placeholder(frame, area, app);
        return HistoryRows::default();
    }

    // Window forward from the offset: format messages until the visible rows are
    // full, building at most one message past the boundary — never the whole
    // history. `inner` excludes the block's top and bottom borders.
    let inner_rows = area.height.saturating_sub(2) as usize;
    let gutter_cols = app.avatar_gutter_cols();
    let width = convo_body_width(area, gutter_cols);
    let mut lines: Vec<Line> = Vec::new();
    // Row offset (within `inner_rows`) and built protocol for each visible
    // message whose sender's avatar has already been encoded this session
    // (#201) — recorded here, alongside the text, so a second pass below can
    // place the `Image` widget precisely on the header's first row. A message
    // whose avatar has not been encoded yet (or has none, or graphics support
    // is off) simply renders a blank gutter — [`drive_avatars`] in `main.rs`
    // kicks off the encode and a later frame draws it once cached.
    let mut avatars: Vec<(usize, &Protocol)> = Vec::new();
    // Row offset and built protocol for each visible message whose inline
    // media has already been decoded this session (#208) — same shape as
    // `avatars` above, placed at the row `message_lines` reserved right after
    // the placeholder/caption rather than the header.
    let mut media: Vec<(usize, &Protocol)> = Vec::new();
    // Row range (within `inner_rows`) each visible message occupies, alongside
    // its id, so a click on any of its rows (header, body, or reaction line)
    // resolves to that message rather than just the header row.
    let mut message_rows: Vec<(usize, usize, i64)> = Vec::new();
    // The message at the offset (the first built) is the selected one — the
    // cursor the reaction/pin affordances act on — so it carries the marker.
    //
    // Row-granular scrolling (#222): only the first message can be showing a
    // partial view — `view.row_skip()` rows already scrolled past its own
    // top. Everything (the unread separator, the header, media) is built into
    // one block first, then that many lines are dropped from its front before
    // it joins `lines` — the header/marker/avatar/media simply fall out of
    // the slice once scrolled past, with no separate "hide it" special case.
    for (i, message) in view.messages().iter().skip(view.offset()).enumerate() {
        if lines.len() >= inner_rows {
            break;
        }
        let row_skip = if i == 0 { view.row_skip() } else { 0 };

        let mut block: Vec<Line> = Vec::new();
        if view.unread_separator_before(message.id) {
            block.push(indent_line(unread_separator_line(), gutter_cols));
        }
        let header_row_in_block = block.len();
        let media_rows = media_rows_for(app, view, &message.content);
        let quote_rows = usize::from(message.reply_to.is_some());
        let media_row_in_block = header_row_in_block
            + quote_rows
            + 1
            + content_lines(&message.content, i == 0, width).len();
        block.extend(message_lines(
            view,
            message,
            i == 0,
            gutter_cols,
            media_rows,
            width,
        ));

        let row = lines.len();
        if gutter_cols > 0
            && let Sender::User(user_id) = message.sender
            && let Some(protocol) = app.cached_avatar(user_id)
            && header_row_in_block >= row_skip
        {
            avatars.push((row + (header_row_in_block - row_skip), protocol));
        }
        if media_rows > 0
            && let Some(protocol) = app.cached_media(message.id)
            && media_row_in_block >= row_skip
        {
            let media_row = row + (media_row_in_block - row_skip);
            // A long caption can push the media box past the truncated view's
            // bottom edge — skip the overlay rather than underflow the height
            // clip below.
            if media_row < inner_rows {
                media.push((media_row, protocol));
            }
        }
        // `media_row_in_block < row_skip` (the box's top already scrolled
        // past the fold) and the header/avatar's symmetric case are handled
        // by the two guards above — the box is hidden rather than faked as a
        // top-crop, which ratatui-image's fixed-raster `Image` widget cannot
        // do without re-encoding on every scroll tick (too slow to be
        // interactive). A bottom-edge partial (scrolling up, box growing from
        // the pane's bottom) still renders correctly via the height-clipped
        // rect below.
        let visible: Vec<Line> = block.into_iter().skip(row_skip).collect();
        message_rows.push((row, row + visible.len(), message.id));
        lines.extend(visible);
    }
    lines.truncate(inner_rows);

    // The conversation header doubles as the chat-action indicator (#87): the pane
    // title names the transient "typing…" activity when someone is acting.
    let title = match view.chat_action() {
        Some(action) => format!(" Conversation — {} ", action_phrase(action)),
        None => " Conversation ".to_owned(),
    };
    let block = pane_block(title, app.focus() == Focus::History);
    let history = Paragraph::new(lines).block(block);
    frame.render_widget(history, area);

    // Second pass (#201): overlay one 2-row `Image` widget per visible
    // avatar-bearing message, at the inner column/row the header's leading
    // gutter span reserved for it. Clip the height to what `truncate` above
    // actually kept, so a message half-cut off at the pane's bottom edge
    // never draws its bubble past the border.
    for (row, protocol) in avatars {
        let rect = Rect {
            x: area.x + 1,
            y: area.y + 1 + row as u16,
            width: gutter_cols as u16,
            height: 2.min((inner_rows - row) as u16),
        };
        // `allow_clipping` (#222): without it, `Image` silently renders
        // nothing at all whenever the rect is shorter than the encoded
        // protocol's own size — which the reduced `height` above already
        // asks for whenever a bubble sits at the pane's edge.
        frame.render_widget(Image::new(protocol).allow_clipping(true), rect);
    }

    // Second pass (#208): overlay one inline-media `Image` per visible,
    // already-decoded message, in the fixed-size box its own row reservation
    // above made room for — same clip-to-truncated-view treatment as avatars.
    // Bounded to a fraction of the pane's own width (never wider than the body
    // column left after the gutter), not just `MEDIA_COLS`, so a narrow
    // terminal never draws past its own border.
    let media_cols = media_cols(area.width, gutter_cols);
    for (row, protocol) in media {
        let rect = Rect {
            x: area.x + 1 + gutter_cols as u16,
            y: area.y + 1 + row as u16,
            width: media_cols as u16,
            height: (crate::conversation::MEDIA_ROWS).min(inner_rows - row) as u16,
        };
        // `allow_clipping` (#222): required for the bottom-edge partial
        // visibility this reduced `height` computes — only Kitty/Halfblocks
        // protocols actually honor it (upstream limitation); Sixel/iTerm2
        // still render all-or-nothing.
        frame.render_widget(Image::new(protocol).allow_clipping(true), rect);
    }

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

    // Absolute-row rows for hit-testing (matches the avatar overlay's own
    // `area.y + 1 + row` above), clipped to what `truncate` above actually kept
    // so a message half-cut off at the pane's bottom edge never claims rows past
    // the border.
    HistoryRows(
        message_rows
            .into_iter()
            .map(|(start, end, id)| {
                (
                    area.y + 1 + start as u16,
                    area.y + 1 + end.min(inner_rows) as u16,
                    id,
                )
            })
            .collect(),
    )
}

/// The conversation pane's empty state: shown while no chat is open (#188).
/// Carries the app identity + version and a call to action naming the real
/// binding — Enter, which opens the selected chat (`open_chat_id`). We deliberately
/// stop here rather than auto-opening the first chat: a full auto-open would mark
/// that chat's unread messages read on every launch (#115), and a read-safe preview
/// would reopen the eager-loading question `open_chat_id` avoids (#114). Aligning
/// with the official client, launching lands the user on the list to choose.
fn render_conversation_placeholder(frame: &mut Frame, area: Rect, app: &App) {
    let body = format!(
        "tuigram v{}\n\nSelect a chat and press Enter to start messaging",
        tuigram_core::version()
    );
    let block = pane_block(" tuigram ".to_owned(), app.focus() == Focus::History)
        .title_alignment(Alignment::Center);
    let widget = Paragraph::new(body)
        .alignment(Alignment::Center)
        .block(block);
    frame.render_widget(widget, area);
}

/// The persistent status bar (#88): a one-row reverse-video strip with the core
/// connection state and current chat/context on the left and the always-available
/// quit/help hint on the right. It takes over the quit hint the conversation
/// placeholder used to carry, so it is present on every screen, with or without
/// data.
fn render_status_bar(frame: &mut Frame, area: Rect, app: &App) {
    // A reverse-video bar sets the status row apart from the panes without relying
    // on colour, matching the rest of the UI's Modifier-only styling.
    let bar = Style::new().add_modifier(Modifier::REVERSED);
    let conn = app.connection();
    let hint = "? help · q quit ";

    // One line: connection + context on the left, the quit/help hint pushed to the
    // right by a padding span sized to fill the row.
    let mut spans = vec![
        Span::raw(" "),
        Span::styled(
            format!("{} {}", conn.symbol(), conn.label()),
            bar.add_modifier(Modifier::BOLD),
        ),
        Span::raw("  ·  "),
        Span::raw(status_context(app)),
    ];
    let used = spans
        .iter()
        .map(|s| s.content.chars().count())
        .sum::<usize>()
        + hint.chars().count();
    let pad = (area.width as usize).saturating_sub(used);
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(Span::raw(hint));

    frame.render_widget(Paragraph::new(Line::from(spans)).style(bar), area);
}

/// The status bar's context field: the selected chat's title and the focused
/// pane (or open overlay), so the bar always says where input is going.
fn status_context(app: &App) -> String {
    let chat = app
        .chat_list()
        .selected_chat()
        .map_or_else(|| "no chat selected".to_owned(), |c| c.title.clone());
    format!("{chat} — {}", mode_label(app))
}

/// The current input mode for the status bar: the open overlay's name, or the
/// focused pane when none is up.
fn mode_label(app: &App) -> &'static str {
    match app.overlay() {
        Overlay::None => match app.focus() {
            Focus::ChatList => "chats",
            Focus::History => "history",
            Focus::Composer => "compose",
        },
        Overlay::Help => "help",
        Overlay::SearchInput | Overlay::SearchResults => "search",
        Overlay::Forward => "forward",
        Overlay::Reaction => "react",
        Overlay::SendMedia => "attach",
        Overlay::SecretChat => "secret chat",
        Overlay::Settings => "settings",
        Overlay::DeleteConfirm => "delete",
        Overlay::LogoutConfirm => "logout",
        Overlay::ContactSearchInput | Overlay::ContactSearchResults => "new secret chat",
    }
}

/// Toast width as a share of the content width, clamped so a long line wraps
/// rather than spanning the screen.
const TOAST_MAX_WIDTH: u16 = 44;

/// A transient toast (#88), anchored top-right over the content: the current
/// notice's marker and message in a small bordered box, with a "+N" title when
/// more are queued and a dim dismiss hint. Errors are bolded. It draws nothing
/// when the queue is empty (the caller guards that) and never captures input.
fn render_toast(frame: &mut Frame, area: Rect, app: &App) {
    let notes = app.notifications();
    let Some(notice) = notes.current() else {
        return;
    };

    let line = notice.line();
    let line_cols = line.chars().count();
    let pending = notes.pending();
    let title = if pending > 0 {
        format!(" Notice (+{pending}) ")
    } else {
        " Notice ".to_owned()
    };
    // The dismiss affordance (#139), on the bottom border: a toast also ages out on
    // its own, but this tells the user how to clear one immediately.
    let hint = " Ctrl-G to dismiss ";

    // Width fits the message, the title, or the hint — whichever is widest — plus
    // borders, clamped to a readable maximum and to the content area.
    let content_cols = (line_cols
        .max(title.chars().count())
        .max(hint.chars().count())
        + 2) as u16;
    let width = content_cols
        .clamp(1, TOAST_MAX_WIDTH)
        .min(area.width.saturating_sub(2));
    // Height: the message wrapped to the inner width, plus borders, capped to the
    // content area.
    let inner = width.saturating_sub(2).max(1) as usize;
    let rows = line_cols.div_ceil(inner).max(1) as u16;
    let height = (rows + 2).min(area.height);

    // Top-right, one cell in from the border so it does not sit on the corner.
    let x = area.x + area.width.saturating_sub(width + 1);
    let y = area.y + 1;
    let rect = Rect {
        x,
        y: y.min(area.y + area.height.saturating_sub(height)),
        width,
        height,
    };

    // Errors bold the message; the hint stays dim regardless, so each carries its own
    // style rather than styling the whole widget.
    let emphasis = if notice.level() == NoticeLevel::Error {
        Style::new().add_modifier(Modifier::BOLD)
    } else {
        Style::new()
    };
    let dim = Style::new().add_modifier(Modifier::DIM);
    let block = Block::bordered()
        .title(title)
        .title_bottom(Line::styled(hint, dim).right_aligned());
    let body = Paragraph::new(Line::styled(line, emphasis))
        .wrap(ratatui::widgets::Wrap { trim: false })
        .block(block);
    frame.render_widget(Clear, rect);
    frame.render_widget(body, rect);
}

/// The lines for one message: a bold `HH:MM Name (@handle)` header (with a
/// selection marker when this is the cursor message and a pin marker when
/// pinned), the sender's accent color tinting the name/handle so the timestamp
/// always sits in a fixed column regardless of name length (#194), the body or
/// a media placeholder, a download-progress line for media being fetched, an
/// optional reaction line, and a blank separator below. When `gutter_cols` is
/// non-zero (real graphics support, #201), every line is prefixed with a blank
/// span that width wide, reserving a left margin for the avatar bubble the
/// caller overlays afterward — the marker/pin prefix that used to start in
/// column 0 now starts just past the gutter instead. `gutter_cols == 0`
/// renders byte-identical to pre-#201 output.
fn message_lines(
    view: &ConversationView,
    message: &Message,
    selected: bool,
    gutter_cols: usize,
    media_rows: usize,
    width: usize,
) -> Vec<Line<'static>> {
    let mut prefix = String::new();
    if selected {
        prefix.push_str(SELECTED_SYMBOL);
    }
    if view.is_pinned(message.id) {
        prefix.push_str("📌 ");
    }
    prefix.push_str(&hour_minute(message.date));
    if message.is_outgoing {
        prefix.push(' ');
        prefix.push_str(delivery_glyph(
            &message.send_state,
            message.id,
            view.last_read_outbox(),
        ));
    }
    prefix.push(' ');

    let label = view.sender_label(message);
    let bold = Style::new().add_modifier(Modifier::BOLD);
    let name_span = match label.color {
        Some(color) => Span::styled(label.label, bold.fg(color)),
        None => Span::styled(label.label, bold),
    };
    let mut header = gutter_span(gutter_cols);
    header.push(Span::styled(prefix, bold));
    header.push(name_span);

    let mut lines = vec![Line::from(header)];
    lines.extend(
        quote_lines(view, message)
            .into_iter()
            .map(|line| indent_line(line, gutter_cols)),
    );
    lines.extend(
        content_lines(&message.content, selected, width)
            .into_iter()
            .map(|line| indent_line(line, gutter_cols)),
    );
    // The inline-media box (#208): `media_rows` blank lines reserved right after
    // the placeholder/caption — additive, not a replacement, so a pending,
    // failed, or non-graphics render's placeholder is unchanged. The second
    // render pass in `render_conversation` overlays the actual `Image` here once
    // decoded; until then (or if it never decodes) the reserved rows just stay
    // blank, same as an avatar's uncached gutter.
    lines.extend((0..media_rows).map(|_| indent_line(Line::from(""), gutter_cols)));
    if let Some(progress) = download_line(view, &message.content) {
        lines.push(indent_line(progress, gutter_cols));
    }
    if let Some(failure) = failed_send_line(message) {
        lines.push(indent_line(failure, gutter_cols));
    }
    if let Some(reactions) = reaction_line(message) {
        lines.push(indent_line(reactions, gutter_cols));
    }
    lines.push(Line::from(""));
    lines
}

/// The greentext quote line above a reply's body (#210) — one line for a
/// reply, none for a plain message; `message_lines` and the media-row offset
/// math both rely on this being 0 or 1 line, never more.
///
/// Resolution is **render-time**, not cached on the model: it looks up the
/// quoted message in `view`'s currently loaded history, so a target that
/// loads later (a history page paged in after this reply was first seen)
/// naturally resolves on the next render — the mechanism #207 left in place
/// for exactly this kind of live catch-up. A target in another chat, one
/// deleted or outside the loaded window, or a reply to a story all fall back
/// to a bare `>reply`.
fn quote_lines(view: &ConversationView, message: &Message) -> Vec<Line<'static>> {
    let Some(reply) = &message.reply_to else {
        return Vec::new();
    };
    let style = Style::new().fg(Color::Green).add_modifier(Modifier::DIM);
    let text = match reply {
        // TDLib documents `chat_id` as "may be 0 if the replied message is in
        // unknown chat" — so `0` is treated as same-chat (not a cross-chat
        // reply) rather than a mismatch, or an ordinary same-chat reply could
        // silently never resolve depending on whether TDLib actually fills in
        // the real id here. A genuinely different, known chat id is still
        // cross-chat and never in `view`'s single-chat window anyway.
        ReplyTo::Message {
            chat_id,
            message_id,
            ..
        } if *chat_id == 0 || *chat_id == message.chat_id => view
            .messages()
            .iter()
            .find(|m| m.id == *message_id)
            .map(|quoted| {
                let sender = view.sender_label(quoted).label;
                format!(
                    ">{sender}: {}",
                    truncate(&content_snippet(&quoted.content), 60)
                )
            })
            .unwrap_or_else(|| ">reply".to_owned()),
        ReplyTo::Message { .. } | ReplyTo::Unsupported(_) => ">reply".to_owned(),
    };
    vec![Line::from(Span::styled(text, style))]
}

/// A one-line, unstyled snippet of a message's content — its text's first
/// line, or a media placeholder — for the quote line. Reuses
/// [`content_lines`] rather than re-deriving the placeholder labels, so the
/// snippet always matches what the quoted message would show as its own
/// first body line.
fn content_snippet(content: &MessageContent) -> String {
    // width=0: the quote line is a one-line, 60-char-truncated preview (see
    // `quote_lines`), never itself wrapped.
    content_lines(content, false, 0)
        .into_iter()
        .next()
        .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
        .unwrap_or_default()
}

/// A blank leading span reserving the avatar gutter's width (#201), or no span
/// at all when `gutter_cols` is `0` — kept as its own `Vec` (not a `Line`) so
/// [`message_lines`]'s header can push more spans after it on the same line.
fn gutter_span(gutter_cols: usize) -> Vec<Span<'static>> {
    if gutter_cols == 0 {
        Vec::new()
    } else {
        vec![Span::raw(" ".repeat(gutter_cols))]
    }
}

/// Prefix an already-built line with the avatar gutter (see [`gutter_span`]);
/// a no-op when `gutter_cols` is `0`.
fn indent_line(line: Line<'static>, gutter_cols: usize) -> Line<'static> {
    if gutter_cols == 0 {
        return line;
    }
    let mut spans = gutter_span(gutter_cols);
    spans.extend(line.spans);
    Line::from(spans)
}

/// The download-progress line for a media message, driven by the file's transfer
/// state: a dim percentage while a download is active, a saved marker once it is
/// present, or `None` when the file is unknown or not being fetched.
fn download_line(view: &ConversationView, content: &MessageContent) -> Option<Line<'static>> {
    let file = view.download(content.file()?.id)?;
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

/// Whether a message's content has raster bytes ready to render inline
/// (#208). Mirrors [`crate::conversation::media_ready`] independently — same
/// convention as `content_rows`/`content_lines` — reading graphics capability
/// from `App` and file presence from the view's projected downloads, since
/// the render path has both directly rather than a stored bool.
fn media_ready(app: &App, view: &ConversationView, content: &MessageContent) -> bool {
    if !app.graphics_active() {
        return false;
    }
    let file_present = |file_id: i32| view.download(file_id).is_some_and(File::is_present);
    match content {
        MessageContent::Photo(p) => file_present(p.file.id),
        MessageContent::Sticker(s) => s.is_static && file_present(s.file.id),
        MessageContent::Video(v) => v.minithumbnail.is_some(),
        MessageContent::Animation(a) => a.minithumbnail.is_some(),
        _ => false,
    }
}

/// The rows [`message_lines`] should reserve for a message's inline-media box
/// (#208): [`crate::conversation::MEDIA_ROWS`] when [`media_ready`], else `0`.
fn media_rows_for(app: &App, view: &ConversationView, content: &MessageContent) -> usize {
    if media_ready(app, view, content) {
        crate::conversation::MEDIA_ROWS
    } else {
        0
    }
}

/// The unread-messages rule (#164): drawn once, immediately above the first
/// incoming message that was unread as of this chat's open (see
/// [`ConversationView::unread_separator_before`]). Not part of any message's own
/// hit-test range — a click on the rule resolves to no message, same as the
/// blank line already trailing each one.
fn unread_separator_line() -> Line<'static> {
    Line::from(Span::styled(
        "── unread ──",
        Style::new().add_modifier(Modifier::DIM),
    ))
}

/// The failed-send detail line (#163): TDLib's error code and message, shown
/// under one of our own messages whose send failed — always visible, not gated
/// on selection, since a delivery failure is important to notice without hunting
/// for it. No retry affordance here; that is explicitly out of scope (backlog),
/// this line only surfaces what went wrong.
fn failed_send_line(message: &Message) -> Option<Line<'static>> {
    if !message.is_outgoing {
        return None;
    }
    let SendState::Failed {
        code,
        message: text,
    } = &message.send_state
    else {
        return None;
    };
    Some(Line::from(Span::styled(
        format!("✗ send failed ({code}): {text}"),
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

/// Format a Unix timestamp as `HH:MM` in the viewer's local timezone (#194).
fn hour_minute(date: i32) -> String {
    format_time_in(date, &chrono::Local)
}

/// `hour_minute`'s conversion, generic over the target timezone so the offset
/// arithmetic is unit-testable without depending on the host machine's local
/// timezone: tests pass a fixed offset, production passes [`chrono::Local`].
fn format_time_in<Tz: chrono::TimeZone>(date: i32, tz: &Tz) -> String
where
    Tz::Offset: std::fmt::Display,
{
    use chrono::TimeZone as _;
    chrono::Utc
        .timestamp_opt(i64::from(date), 0)
        .single()
        .map(|utc| utc.with_timezone(tz).format("%H:%M").to_string())
        .unwrap_or_default()
}

/// The body lines for a message's content: the text for a text message, or a
/// `[Kind]` placeholder for media (with its caption, when set, on the lines
/// below). Media bytes are not rendered in a terminal; the placeholder names what
/// the message carries.
fn content_lines(content: &MessageContent, selected: bool, width: usize) -> Vec<Line<'static>> {
    match content {
        MessageContent::Text(text) => text_lines(text, selected, width),
        MessageContent::Photo(p) => placeholder_lines("[Photo]", &p.caption, selected, width),
        MessageContent::Video(v) => placeholder_lines("[▶ video]", &v.caption, selected, width),
        MessageContent::Document(d) => placeholder_lines(
            &format!("[Document {}]", trimmed_name(&d.file_name)),
            &d.caption,
            selected,
            width,
        ),
        MessageContent::Audio(a) => placeholder_lines("[Audio]", &a.caption, selected, width),
        MessageContent::Voice(v) => placeholder_lines("[Voice]", &v.caption, selected, width),
        MessageContent::Sticker(s) => one_line(format!("[Sticker {}]", s.emoji).trim_end()),
        MessageContent::Animation(a) => placeholder_lines("[GIF]", &a.caption, selected, width),
        MessageContent::Location(_) => one_line("[Location]"),
        MessageContent::Venue(v) => one_line(format!("[Venue {}]", v.title).trim_end()),
        MessageContent::Contact(c) => {
            one_line(format!("[Contact {} {}]", c.first_name, c.last_name).trim_end())
        }
        MessageContent::Poll(p) => one_line(format!("[Poll] {}", p.question.text)),
        MessageContent::Unsupported(name) => one_line(format!("[{name}]")),
    }
}

/// The lines of a text body, preserving its own line breaks, word-wrapping
/// each of those logical lines at `width` (#214; `width == 0` skips wrapping
/// — see [`crate::conversation::message_height`]), and rendering formatting
/// entities (#211) — bold/italic/code/strikethrough/spoiler, with overlapping
/// entities composed rather than one silently overwriting another (see
/// [`crate::richtext::styled_spans`]). `selected` gates spoiler reveal. Empty
/// text still yields one (empty) line so the header is not left bodyless.
///
/// Wraps each logical line's *raw* text first (via [`crate::wrap::wrap_breaks`]
/// — the exact same call [`crate::conversation::message_height`] makes for its
/// row count) and only then re-derives that row's entities and hands the raw
/// row substring to `styled_spans`. Deliberately never wraps the
/// already-selection-substituted spoiler glyphs — doing so would let this
/// pick different break points from `message_height`'s raw-text count
/// whenever a spoiler's glyph substitution doesn't preserve display width
/// (e.g. concealing a wide CJK/emoji character behind a narrow glyph).
/// Wrapping the raw text first, unconditionally, keeps the two in lockstep
/// regardless of selection state or spoiler content.
fn text_lines(text: &FormattedText, selected: bool, width: usize) -> Vec<Line<'static>> {
    // Entity offsets are UTF-16 code units into the *whole* `text.text`
    // (TDLib's convention); track each split-out line's UTF-16 span so every
    // entity can be intersected against it and re-offset to be line-local —
    // `styled_spans` then treats each line (or, once wrapped, each row of a
    // line) as a standalone string.
    let mut utf16_pos: i32 = 0;
    let mut lines = Vec::new();
    for line in text.text.split('\n') {
        let line_start = utf16_pos;
        let line_end = utf16_pos + line.encode_utf16().count() as i32;
        let local_entities: Vec<TextEntity> = text
            .entities
            .iter()
            .filter_map(|e| {
                let start = e.offset.max(line_start);
                let end = (e.offset + e.length).min(line_end);
                (start < end).then(|| TextEntity {
                    offset: start - line_start,
                    length: end - start,
                    kind: e.kind.clone(),
                })
            })
            .collect();
        utf16_pos = line_end + 1; // the '\n' consumed by `split` is one UTF-16 unit

        if width == 0 {
            lines.push(Line::from(crate::richtext::styled_spans(
                line,
                &local_entities,
                selected,
            )));
            continue;
        }

        let breaks = crate::wrap::wrap_breaks(line, width);
        for (i, &row_start) in breaks.iter().enumerate() {
            let row_end = breaks.get(i + 1).copied().unwrap_or(line.len());
            let row_text = &line[row_start..row_end];
            let row_start_u16 = byte_to_utf16(line, row_start);
            let row_end_u16 = byte_to_utf16(line, row_end);
            let row_entities: Vec<TextEntity> = local_entities
                .iter()
                .filter_map(|e| {
                    let start = e.offset.max(row_start_u16);
                    let end = (e.offset + e.length).min(row_end_u16);
                    (start < end).then(|| TextEntity {
                        offset: start - row_start_u16,
                        length: end - start,
                        kind: e.kind.clone(),
                    })
                })
                .collect();
            lines.push(Line::from(crate::richtext::styled_spans(
                row_text,
                &row_entities,
                selected,
            )));
        }
    }
    lines
}

/// The UTF-16 code-unit offset in `s` corresponding to byte offset
/// `byte_offset` — the inverse of `richtext`'s UTF-16→byte conversion, needed
/// to re-clip a logical line's UTF-16-offset entities to a wrapped row's byte
/// range (from [`crate::wrap::wrap_breaks`]). `byte_offset` must land on a
/// char boundary — true of every offset `wrap_breaks` returns.
fn byte_to_utf16(s: &str, byte_offset: usize) -> i32 {
    s[..byte_offset].encode_utf16().count() as i32
}

/// A media placeholder line, with the caption's (wrapped, #214) lines below it
/// when non-empty. The label itself never wraps — only the caption text does
/// — so [`crate::conversation::content_rows`]'s `1 + caption_rows` stays exact
/// even for a long document filename.
fn placeholder_lines(
    label: &str,
    caption: &FormattedText,
    selected: bool,
    width: usize,
) -> Vec<Line<'static>> {
    let mut lines = one_line(label);
    if !caption.text.is_empty() {
        lines.extend(text_lines(caption, selected, width));
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
/// the caret shows in the `TestBackend` buffer, then the remainder. Shared with the
/// login screens (#86) so every text field renders its caret identically.
pub(crate) fn input_line(text: &str, cursor: usize) -> Line<'static> {
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
/// generated from the keymap so it always matches what the keys actually do. On a
/// terminal too short to show every binding the body scrolls (`app.help_scroll`),
/// with a fixed hint row along the bottom; the border and hint stay put while the
/// bindings slide under them.
fn render_help(frame: &mut Frame, area: Rect, app: &App) {
    let lines = help_lines();
    let content_width = lines.iter().map(Line::width).max().unwrap_or(0) as u16;
    // Border (2) + the hint row (1) frame the scrollable body; `centered_rect` clamps
    // the height to the terminal, so a tall cheatsheet becomes a scroll viewport.
    let popup = centered_rect(content_width + 4, lines.len() as u16 + 3, area);
    // `Clear` wipes the panes underneath so the overlay reads as a modal.
    frame.render_widget(Clear, popup);

    let block = Block::bordered()
        .title(" Help ")
        .title_alignment(Alignment::Center);
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let [body_area, hint_area] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(inner);

    // `scroll` offsets the body; ratatui clips it to `body_area`, so the offset picks
    // the first visible binding line. The offset is already clamped to the last line
    // by the reducer.
    frame.render_widget(
        Paragraph::new(lines).scroll((app.help_scroll(), 0)),
        body_area,
    );
    frame.render_widget(
        Paragraph::new(hint_line("j / k scroll · ? / q / Esc close")),
        hint_area,
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

/// A dim hint line, for the key reminder along the bottom of a modal (and the
/// login screens, #86).
pub(crate) fn hint_line(hint: &'static str) -> Line<'static> {
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
fn render_search_results(frame: &mut Frame, area: Rect, app: &App) -> OverlayRows {
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
        return OverlayRows::default();
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
        "j / k move · Enter open · f forward · Esc close",
    )
}

/// The contact-search query line (#197): a centred modal with the editable query
/// over a key hint. Mirrors [`render_search_input`].
fn render_contact_search_input(frame: &mut Frame, area: Rect, app: &App) {
    let contacts = app.contacts();
    let query = if contacts.query().is_empty() {
        Line::from(Span::styled(
            "type a name to search contacts…",
            Style::new().add_modifier(Modifier::DIM),
        ))
    } else {
        input_line(contacts.query(), contacts.cursor())
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
                .title(" New secret chat — search contacts ")
                .title_alignment(Alignment::Center),
        ),
        popup,
    );
}

/// The contact-search results overlay (#197): a centred modal listing the
/// matching contacts. Confirming one opens the secret-chat confirm
/// ([`render_secret_chat`]) for that user. Mirrors [`render_search_results`].
fn render_contact_search_results(frame: &mut Frame, area: Rect, app: &App) -> OverlayRows {
    let contacts = app.contacts();
    let title = format!(
        " Contacts — \"{}\" ({}) ",
        truncate(contacts.query(), 30),
        contacts.results().len()
    );
    if contacts.results().is_empty() {
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
        return OverlayRows::default();
    }

    let items: Vec<ListItem> = contacts
        .results()
        .iter()
        .map(|hit| ListItem::new(hit.display_name.clone()))
        .collect();
    render_list_modal(
        frame,
        area,
        title,
        items,
        contacts.selected(),
        "j / k move · Enter start secret chat · Esc close",
    )
}

/// The forward target picker (#84): a centred modal that **reuses the chat-list
/// widget** to choose where the selected message(s) go, with a key hint along the
/// bottom.
fn render_forward(frame: &mut Frame, area: Rect, app: &App) -> OverlayRows {
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
    )
}

/// The reaction picker (#85): a centred modal listing the emoji palette with the
/// selected one marked (palette mode), or the custom-emoji entry line (custom mode,
/// #119). Confirming toggles the effective emoji on the selected message.
fn render_reaction(frame: &mut Frame, area: Rect, app: &App) -> OverlayRows {
    let picker = app.reaction();
    match picker.custom_input() {
        Some(buffer) => {
            render_reaction_custom(frame, area, buffer);
            OverlayRows::default()
        }
        None => render_reaction_palette(frame, area, picker),
    }
}

/// Palette mode: the emoji list with the selected one marked, over a dim affordance
/// for the custom-emoji line and the key hint.
fn render_reaction_palette(
    frame: &mut Frame,
    area: Rect,
    picker: &crate::reactions::ReactionPicker,
) -> OverlayRows {
    let palette_len = picker.palette().len();
    let items: Vec<ListItem> = picker
        .palette()
        .iter()
        .map(|emoji| ListItem::new((*emoji).to_owned()))
        .collect();

    // Border (2) + the palette rows + the custom affordance (1) + the hint row (1).
    let height = items.len() as u16 + 4;
    let popup = centered_rect(OVERLAY_WIDTH, height, area);
    frame.render_widget(Clear, popup);

    let block = Block::bordered()
        .title(" React ")
        .title_alignment(Alignment::Center);
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let [list_area, custom_area, hint_area] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);

    let list = List::new(items)
        .highlight_symbol(SELECTED_SYMBOL)
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
    let mut state = ListState::default().with_selected(Some(picker.selected()));
    frame.render_stateful_widget(list, list_area, &mut state);
    frame.render_widget(
        Paragraph::new(hint_line("c  type a custom emoji")),
        custom_area,
    );
    frame.render_widget(
        Paragraph::new(hint_line("j / k move · Enter react · Esc cancel")),
        hint_area,
    );

    OverlayRows {
        columns: (list_area.x, list_area.x + list_area.width),
        rows: (0..palette_len)
            .skip(state.offset())
            .take(list_area.height as usize)
            .enumerate()
            .map(|(i, idx)| (list_area.y + i as u16, idx))
            .collect(),
    }
}

/// Custom mode: the editable custom-emoji line (with the caret) over the key hint.
/// The buffer takes whatever the OS emoji picker or a paste emits, so the caret sits
/// at its end.
fn render_reaction_custom(frame: &mut Frame, area: Rect, buffer: &str) {
    let cursor = buffer.chars().count();
    let mut spans = vec![Span::styled(
        "custom ",
        Style::new().add_modifier(Modifier::DIM),
    )];
    spans.extend(input_line(buffer, cursor).spans);
    let lines = vec![
        Line::from(spans),
        Line::from(""),
        hint_line("type or paste an emoji · Enter react · Esc back"),
    ];

    let popup = centered_rect(OVERLAY_WIDTH, lines.len() as u16 + 2, area);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .title(" React ")
                .title_alignment(Alignment::Center),
        ),
        popup,
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

/// The settings editor (#146, plus the graphics toggle, #209): a centred modal
/// with the three per-kind TTL fields, the global cache-cap field, and the
/// graphics on/off field, pre-filled with the live values. The focused field
/// shows the caret; a rejected confirm surfaces its reason on a red line above
/// the key hint, so an invalid value is corrected in place rather than saved.
fn render_settings(frame: &mut Frame, area: Rect, app: &App) {
    let settings = app.settings();
    let field_line = |field: SettingsField| {
        settings_field_line(
            field.label(),
            settings.value(field),
            settings.field() == field,
            settings.cursor(),
        )
    };
    let mut lines = vec![
        field_line(SettingsField::KeepPrivate),
        field_line(SettingsField::KeepGroups),
        field_line(SettingsField::KeepChannels),
        field_line(SettingsField::MaxCache),
        field_line(SettingsField::Graphics),
        Line::from(""),
    ];
    if let Some(error) = settings.error() {
        lines.push(Line::from(Span::styled(
            error.to_owned(),
            Style::new().fg(Color::Red),
        )));
    }
    lines.push(hint_line(
        "Tab next field · Enter save · Esc cancel · forever/3d/1w · 2GB/unbounded · on/off",
    ));

    let popup = centered_rect(OVERLAY_WIDTH, lines.len() as u16 + 2, area);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .title(" Settings ")
                .title_alignment(Alignment::Center),
        ),
        popup,
    );
}

/// One labelled field of the settings editor: a padded label then the value — the
/// focused field with a caret (via [`input_line`]), the rest their plain text. Every
/// field is pre-filled, so there is no placeholder branch.
fn settings_field_line(label: &str, text: &str, focused: bool, cursor: usize) -> Line<'static> {
    let mut spans = vec![Span::styled(
        format!("{label:<10}"),
        Style::new().add_modifier(Modifier::BOLD),
    )];
    if focused {
        spans.extend(input_line(text, cursor).spans);
    } else {
        spans.push(Span::raw(text.to_owned()));
    }
    Line::from(spans)
}

/// The secret-chat lifecycle confirm overlay (#87): a centred modal posing the
/// start/close question for the selected chat, over a key hint. Confirming runs the
/// core seam (Phase 6); the prompt reads only the chat's kind and lifecycle state,
/// never any key material.
fn render_secret_chat(frame: &mut Frame, area: Rect, app: &App) {
    let Some(prompt) = app.secret() else {
        return;
    };
    let lines = vec![
        Line::from(prompt.prompt()),
        Line::from(""),
        hint_line("Enter confirm · Esc cancel"),
    ];
    let popup = centered_rect(OVERLAY_WIDTH, lines.len() as u16 + 2, area);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .title(" Secret chat ")
                .title_alignment(Alignment::Center),
        ),
        popup,
    );
}

/// The delete-message confirm (#195): the target message's preview and the scope
/// the confirm will use, gating the destructive delete behind an explicit step.
/// "For everyone" is offered only for our own messages — the only ones Telegram can
/// revoke — so a message from someone else shows just the "for me" delete.
fn render_delete_confirm(frame: &mut Frame, area: Rect, app: &App) {
    let Some(prompt) = app.delete() else {
        return;
    };
    let scope = if prompt.revoke() {
        "for everyone"
    } else {
        "for me"
    };
    let mut lines = vec![
        Line::from(format!("Delete this message {scope}?")),
        Line::from(""),
        Line::from(prompt.preview().to_owned()),
        Line::from(""),
    ];
    lines.push(if prompt.can_revoke() {
        hint_line("Enter delete · Tab toggle scope · Esc cancel")
    } else {
        hint_line("Enter delete · Esc cancel")
    });
    let popup = centered_rect(OVERLAY_WIDTH, lines.len() as u16 + 2, area);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .title(" Delete message ")
                .title_alignment(Alignment::Center),
        ),
        popup,
    );
}

/// The logout confirm (#195): a deliberately spare, destructive-action confirm —
/// logging out clears the local session, so it is gated behind an explicit Enter.
fn render_logout_confirm(frame: &mut Frame, area: Rect, _app: &App) {
    let lines = vec![
        Line::from("Log out of this account?"),
        Line::from(""),
        Line::from("This clears the local session on this device."),
        Line::from("The next launch will sign in fresh."),
        Line::from(""),
        hint_line("Enter log out · Esc cancel"),
    ];
    let popup = centered_rect(OVERLAY_WIDTH, lines.len() as u16 + 2, area);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .title(" Log out ")
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
) -> OverlayRows {
    let item_count = items.len();
    // Border (2) + the hint row (1) frame the list rows.
    let height = item_count as u16 + 3;
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

    OverlayRows {
        columns: (list_area.x, list_area.x + list_area.width),
        rows: (0..item_count)
            .skip(state.offset())
            .take(list_area.height as usize)
            .enumerate()
            .map(|(i, idx)| (list_area.y + i as u16, idx))
            .collect(),
    }
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
    use crate::terminal::AvatarSupport;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::{Buffer, Cell};

    /// The TTY-free snapshot harness: render a known `App` into an in-memory
    /// `Buffer` at a fixed size. Every later `ui:` issue asserts against this.
    fn render(app: &App, width: u16, height: u16) -> Buffer {
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
    fn render_output(app: &App, width: u16, height: u16) -> RenderOutput {
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
    fn pane_layout_hit_tests_each_pane() {
        // The same 80×24 geometry the skeleton renders at, so this exercises the
        // exact rects `ui` draws into. A point lands in the pane whose rect holds
        // it; the status strip and out-of-bounds are not focus targets (#161).
        let panes = pane_layout(Rect::new(0, 0, 80, 24));
        // Chat list is the left column.
        assert_eq!(panes.focus_at(1, 1), Some(Focus::ChatList));
        // History fills the right column above the composer.
        assert_eq!(panes.focus_at(50, 1), Some(Focus::History));
        // Composer is the fixed block just above the status bar.
        assert_eq!(panes.focus_at(50, panes.composer.y), Some(Focus::Composer));
        // The bottom status strip is not a focus target.
        assert_eq!(panes.focus_at(0, panes.status.y), None);
        // A point past the right edge hits nothing.
        assert_eq!(panes.focus_at(80, 0), None);
    }

    #[test]
    fn media_cols_shrinks_below_the_fixed_size_on_a_narrow_pane() {
        // At a realistic narrow width (#226) the available space (borders +
        // gutter subtracted) is below `MEDIA_COLS`, so the box must shrink to
        // fit rather than staying at the fixed size and getting cropped later
        // by `allow_clipping`.
        let narrow = media_cols(50, 5);
        assert!(
            narrow < crate::conversation::MEDIA_COLS,
            "expected a narrow pane to shrink the box below MEDIA_COLS, got {narrow}"
        );
        assert_eq!(narrow, 50usize.saturating_sub(2).saturating_sub(5));
    }

    #[test]
    fn media_cols_caps_at_the_fixed_size_on_a_wide_pane() {
        // A generously wide pane must not inflate the box past `MEDIA_COLS` —
        // it's a cap, not a fill-the-pane target.
        assert_eq!(media_cols(200, 5), crate::conversation::MEDIA_COLS);
    }

    #[test]
    fn renders_three_pane_skeleton() {
        let text = flatten(&render(&App::new(), 80, 24));
        assert!(text.contains("Chats"), "chat-list pane");
        assert!(text.contains("tuigram"), "conversation pane");
        assert!(text.contains("Message"), "composer pane");
        assert!(text.contains("quit"), "quit hint");
    }

    /// End-to-end inertness: text that has passed through the core sanitizer
    /// (as it will, arriving via `from_tdlib`) renders with no control byte
    /// reaching a cell, and with a visible replacement marker where the escape
    /// was. This is the render half of the escape-injection defense (#174); the
    /// core `sanitize` and `model` tests cover the projection half.
    #[test]
    fn a_hostile_message_renders_inert() {
        let hostile = "hello\u{1b}]0;pwned\u{07}\u{1b}[2Jworld";
        let content = MessageContent::Text(FormattedText {
            text: tuigram_core::scrub_prose(hostile),
            entities: Vec::new(),
        });
        let rendered: String = content_lines(&content, false, 0)
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect();
        assert!(
            !rendered.chars().any(|c| c.is_control() && c != '\n'),
            "no control byte reaches a cell: {rendered:?}"
        );
        assert!(rendered.contains('\u{fffd}'), "tampering is marked");
        assert!(rendered.contains("hello") && rendered.contains("world"));
    }

    /// #211: a bold entity styles only its own span, leaving the rest of the
    /// line plain — the render half of `richtext::styled_spans`'s unit
    /// coverage, exercised through the real `content_lines` seam.
    #[test]
    fn formatting_entities_style_the_conversation_pane() {
        use tuigram_core::model::EntityKind;

        let content = MessageContent::Text(FormattedText {
            text: "bold text".to_owned(),
            entities: vec![TextEntity {
                offset: 0,
                length: 4,
                kind: EntityKind::Bold,
            }],
        });
        let spans: Vec<Span> = content_lines(&content, false, 0)
            .into_iter()
            .flat_map(|line| line.spans.into_iter())
            .collect();
        let bold = spans
            .iter()
            .find(|s| s.content.as_ref() == "bold")
            .expect("bold span");
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
        let rest = spans
            .iter()
            .find(|s| s.content.as_ref() == " text")
            .expect("plain span");
        assert!(!rest.style.add_modifier.contains(Modifier::BOLD));
    }

    /// #211: a spoiler stays concealed while its message is not selected, and
    /// reveals once it is — rendering depends on UI selection state, not just
    /// the message's own content.
    #[test]
    fn a_spoiler_conceals_by_default_and_reveals_when_selected() {
        use tuigram_core::model::EntityKind;

        let content = MessageContent::Text(FormattedText {
            text: "secret".to_owned(),
            entities: vec![TextEntity {
                offset: 0,
                length: 6,
                kind: EntityKind::Spoiler,
            }],
        });
        let concealed: String = content_lines(&content, false, 0)
            .into_iter()
            .flat_map(|line| line.spans.into_iter())
            .map(|s| s.content.into_owned())
            .collect();
        assert!(!concealed.contains("secret"), "concealed: {concealed:?}");

        let revealed: String = content_lines(&content, true, 0)
            .into_iter()
            .flat_map(|line| line.spans.into_iter())
            .map(|s| s.content.into_owned())
            .collect();
        assert!(
            revealed.contains("secret"),
            "revealed on selection: {revealed:?}"
        );
    }

    /// #214 drift-guard: a spoiler's concealed glyph substitution
    /// (`richtext::SPOILER_GLYPH`, one per hidden character) doesn't
    /// necessarily preserve a wide source character's display width — but
    /// `text_lines` wraps the *raw* text before substituting, the same raw
    /// text `ConversationView::message_height` wraps, so the two stay in
    /// lockstep regardless. Exercised at a narrow width, in both selection
    /// states, with wide CJK characters as the spoiler's content.
    #[test]
    fn a_spoiler_wrapping_wide_characters_does_not_drift_the_height() {
        use tuigram_core::model::EntityKind;

        let content = MessageContent::Text(FormattedText {
            text: "中中中中中中".to_owned(),
            entities: vec![TextEntity {
                offset: 0,
                length: 6,
                kind: EntityKind::Spoiler,
            }],
        });
        let mut view =
            ConversationView::from_messages(vec![sample_message(1, content)], HashSet::new());
        view.set_viewport_width(4);

        for message in view.messages() {
            for selected in [false, true] {
                assert_eq!(
                    message_lines(&view, message, selected, 0, 0, 4).len(),
                    view.message_height(message),
                    "height drifts from the renderer, selected={selected}"
                );
            }
        }
    }

    /// #210: a reply to a message loaded in the same chat resolves its
    /// sender and a snippet of its body onto the greentext quote line.
    #[test]
    fn quote_lines_resolves_the_sender_and_snippet_of_a_loaded_target() {
        use crate::conversation::sample_message;
        use std::collections::HashSet;
        use tuigram_core::model::ReplyTo;

        let original = sample_message(
            1,
            MessageContent::Text(FormattedText {
                text: "original message".to_owned(),
                entities: Vec::new(),
            }),
        );
        let mut reply = sample_message(
            2,
            MessageContent::Text(FormattedText {
                text: "sure thing".to_owned(),
                entities: Vec::new(),
            }),
        );
        reply.reply_to = Some(ReplyTo::Message {
            chat_id: original.chat_id,
            message_id: 1,
            quote: None,
        });
        let view = ConversationView::from_messages(vec![original, reply.clone()], HashSet::new());

        let lines = quote_lines(&view, &reply);
        assert_eq!(lines.len(), 1);
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("original message"), "snippet: {text:?}");
        assert!(text.starts_with('>'), "greentext marker: {text:?}");
    }

    /// #210: a reply whose target is not in the loaded window (deleted, or
    /// simply not paged in yet) falls back to a bare `>reply`, never blocking
    /// or erroring.
    #[test]
    fn quote_lines_falls_back_to_bare_reply_for_an_unloaded_target() {
        use crate::conversation::sample_message;
        use std::collections::HashSet;
        use tuigram_core::model::ReplyTo;

        let mut reply = sample_message(
            2,
            MessageContent::Text(FormattedText {
                text: "sure thing".to_owned(),
                entities: Vec::new(),
            }),
        );
        reply.reply_to = Some(ReplyTo::Message {
            chat_id: reply.chat_id,
            message_id: 999,
            quote: None,
        });
        let view = ConversationView::from_messages(vec![reply.clone()], HashSet::new());

        let lines = quote_lines(&view, &reply);
        assert_eq!(lines.len(), 1);
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, ">reply");
    }

    /// #210: once a re-projection brings the previously-unloaded target into
    /// `view`'s history (e.g. paging up), the *same* reply message resolves
    /// its quote line without any change to the reply itself — the
    /// render-time lookup, not a cached value, is what catches up.
    #[test]
    fn quote_lines_catches_up_once_the_target_loads() {
        use crate::conversation::sample_message;
        use std::collections::HashSet;
        use tuigram_core::model::ReplyTo;

        let mut reply = sample_message(
            2,
            MessageContent::Text(FormattedText {
                text: "sure thing".to_owned(),
                entities: Vec::new(),
            }),
        );
        reply.reply_to = Some(ReplyTo::Message {
            chat_id: reply.chat_id,
            message_id: 1,
            quote: None,
        });

        let before = ConversationView::from_messages(vec![reply.clone()], HashSet::new());
        let before_text: String = quote_lines(&before, &reply)[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(before_text, ">reply", "not loaded yet");

        let original = sample_message(
            1,
            MessageContent::Text(FormattedText {
                text: "original message".to_owned(),
                entities: Vec::new(),
            }),
        );
        let after = ConversationView::from_messages(vec![original, reply.clone()], HashSet::new());
        let after_text: String = quote_lines(&after, &reply)[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(
            after_text.contains("original message"),
            "resolved after the target loaded: {after_text:?}"
        );
    }

    /// A plain (non-reply) message has no quote line at all.
    #[test]
    fn quote_lines_is_empty_for_a_plain_message() {
        use crate::conversation::sample_message;
        use std::collections::HashSet;

        let message = sample_message(
            1,
            MessageContent::Text(FormattedText {
                text: "hi".to_owned(),
                entities: Vec::new(),
            }),
        );
        let view = ConversationView::from_messages(vec![message.clone()], HashSet::new());
        assert!(quote_lines(&view, &message).is_empty());
    }

    /// #210: TDLib documents `MessageReplyToMessage.chat_id` as "may be 0 if
    /// the replied message is in unknown chat" — a same-chat reply must still
    /// resolve rather than always falling back to bare `>reply` if TDLib
    /// reports `0` here instead of the real (matching) chat id.
    #[test]
    fn quote_lines_resolves_a_reply_whose_chat_id_is_reported_as_zero() {
        use crate::conversation::sample_message;
        use std::collections::HashSet;
        use tuigram_core::model::ReplyTo;

        let original = sample_message(
            1,
            MessageContent::Text(FormattedText {
                text: "original message".to_owned(),
                entities: Vec::new(),
            }),
        );
        let mut reply = sample_message(
            2,
            MessageContent::Text(FormattedText {
                text: "sure thing".to_owned(),
                entities: Vec::new(),
            }),
        );
        reply.reply_to = Some(ReplyTo::Message {
            chat_id: 0,
            message_id: 1,
            quote: None,
        });
        let view = ConversationView::from_messages(vec![original, reply.clone()], HashSet::new());

        let lines = quote_lines(&view, &reply);
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("original message"), "snippet: {text:?}");
    }

    #[test]
    fn the_focused_pane_carries_the_focus_marker() {
        use crate::keymap::Focus;
        let mut app = App::new();
        app.dispatch(crate::app::Action::SetFocus(Focus::Composer));
        let buffer = render(&app, 80, 24);
        // The composer (focused) shows the marker on its border; the chat-list
        // title row (top, unfocused) does not.
        let composer_row = row_text(&buffer, 24 - STATUS_HEIGHT - COMPOSER_HEIGHT);
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
        assert!(text.contains("scroll"), "the footer hints the scroll keys");
    }

    #[test]
    fn the_settings_overlay_shows_the_prefilled_fields_and_hint() {
        use tuigram_core::{CacheCap, KeepMedia, StorageSettings};
        let mut app = App::new();
        app.set_storage_settings(StorageSettings {
            keep_private: KeepMedia::Forever,
            keep_groups: KeepMedia::Days(7),
            keep_channels: KeepMedia::Days(3),
            max_cache: CacheCap::Bytes(2 * 1024 * 1024 * 1024),
        });
        app.dispatch(crate::app::Action::SettingsOpen);
        let text = flatten(&render(&app, 80, 24));
        assert!(text.contains("Settings"), "overlay title");
        assert!(text.contains("channels"), "a field label");
        assert!(
            text.contains("2GB"),
            "the live max-cache value is pre-filled"
        );
        assert!(text.contains("graphics"), "the graphics field label");
        assert!(
            text.contains("on"),
            "graphics is pre-filled from the live setting"
        );
        assert!(text.contains("Enter save"), "the key hint");
    }

    #[test]
    fn the_settings_overlay_surfaces_a_rejected_value_in_place() {
        let mut app = App::new();
        app.dispatch(crate::app::Action::SettingsOpen);
        // Replace the private field with an unparseable value, then confirm.
        for _ in 0.."forever".len() {
            app.dispatch(crate::app::Action::SettingsBackspace);
        }
        for c in "nope".chars() {
            app.dispatch(crate::app::Action::SettingsInput(c));
        }
        app.dispatch(crate::app::Action::SettingsConfirm);
        assert_eq!(
            app.overlay(),
            Overlay::Settings,
            "still open after rejection"
        );
        let text = flatten(&render(&app, 80, 24));
        assert!(
            text.contains("private:"),
            "the reason names the offending field"
        );
    }

    #[test]
    fn help_line_count_matches_the_rendered_body() {
        // The scroll clamp keys off `keymap::help_line_count`; it must track the lines
        // `help_lines` actually produces, or a scroll could stop short or overrun.
        assert_eq!(keymap::help_line_count(), help_lines().len());
    }

    #[test]
    fn a_short_terminal_scrolls_the_help_body() {
        let mut app = App::new();
        app.dispatch(crate::app::Action::ToggleHelp);
        // A terminal too short to show the whole cheatsheet: the first section shows
        // at the top, the last does not.
        let top = flatten(&render(&app, 80, 10));
        assert!(top.contains("Global"), "the first section is at the top");
        assert!(
            !top.contains("Chat list & history"),
            "the last section is off-screen"
        );
        // Scroll down enough to move the first section out of the viewport; the
        // overlay stays open (scrolling never closes it) and the footer hint is fixed.
        for _ in 0..7 {
            app.dispatch(crate::app::Action::HelpScrollDown);
        }
        assert!(app.help_visible(), "scrolling keeps the overlay open");
        let scrolled = flatten(&render(&app, 80, 10));
        assert!(
            !scrolled.contains("Global"),
            "the first section scrolled out of view"
        );
        assert!(
            scrolled.contains("scroll"),
            "the footer hint stays put while the body scrolls"
        );
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
        // The composer sits just above the one-row status bar; its bordered title
        // row is the first of its COMPOSER_HEIGHT rows.
        let composer_top = row_text(&buffer, 24 - STATUS_HEIGHT - COMPOSER_HEIGHT);
        assert!(
            composer_top.contains("Message"),
            "composer above the status bar"
        );
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

    // --- conversation / history pane (#81) ---

    use crate::conversation::{ConversationView, SenderLabel, sample_message};
    use std::collections::{HashMap, HashSet};
    use tuigram_core::model::{FileRef, Photo, Reaction, Sender};

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
    fn history_rows_maps_a_multi_row_message_range_to_its_id() {
        // Each message spans several rows (header, body, trailing blank line),
        // not just its header — a click anywhere in that range should resolve to
        // the message, not just its first row (extends #161/#162).
        let output = render_output(
            &app_with_history(vec![text_message(1, "m1"), text_message(2, "m2")]),
            80,
            24,
        );
        let top = output.panes.history.y + 1;
        assert_eq!(output.history_rows.message_at(top), Some(1), "header row");
        let message_1_rows: Vec<u16> = (top..top + 10)
            .filter(|&row| output.history_rows.message_at(row) == Some(1))
            .collect();
        assert!(
            message_1_rows.len() > 1,
            "message 1's block spans more than just its header row"
        );
        // Message 2 starts on the row right after message 1's range ends.
        let message_2_row = top + message_1_rows.len() as u16;
        assert_eq!(output.history_rows.message_at(message_2_row), Some(2));
    }

    #[test]
    fn overlay_rows_is_empty_with_no_overlay_open() {
        let output = render_output(&app_with_lists(), 80, 24);
        assert_eq!(output.overlay_rows.index_at(40, 5), None);
    }

    #[test]
    fn overlay_rows_maps_search_result_rows_and_rejects_a_stray_column() {
        // #217: a click on the search-results popup should resolve to the hit's
        // index, but only inside the popup's own (centred, narrower-than-screen)
        // column range — the panes underneath stay visible with no backdrop, so a
        // click at the same row but off to the side must miss.
        let output = render_output(&app_on_results(), 80, 24);
        let row = (0..24)
            .find(|&r| output.overlay_rows.index_at(40, r) == Some(1))
            .expect("Bob's hit resolves to index 1 at a column inside the popup");
        assert_eq!(
            output.overlay_rows.index_at(1, row),
            None,
            "same row, but a column outside the centred popup misses"
        );
    }

    #[test]
    fn graphics_avatar_support_indents_the_header_by_the_gutter_width() {
        use ratatui_image::picker::{Picker, ProtocolType};

        let mut picker = Picker::halfblocks();
        picker.set_protocol_type(ProtocolType::Kitty);
        let mut app = app_with_history(vec![text_message(1, "hi")]);
        app.set_avatar_support(AvatarSupport::Graphics(picker));
        let cols = app.avatar_support().gutter_cols();
        let buffer = render(&app, 80, 24);

        // Row 1 is the history pane's first inner row. The selected marker
        // used to start right after that pane's left border; with graphics
        // support active it now starts `cols` blank columns later, reserving
        // the avatar bubble's left margin — check the chars immediately
        // preceding the marker, rather than the row's absolute start, since
        // the chat-list pane (a separate, narrower pane) precedes it on the
        // same row.
        let row: Vec<char> = row_text(&buffer, 1).chars().collect();
        let marker_pos = row
            .iter()
            .position(|&c| c == '▶')
            .expect("selected marker present");
        let gutter: String = row[marker_pos - cols..marker_pos].iter().collect();
        assert_eq!(gutter, " ".repeat(cols), "the gutter itself is blank");
        assert_eq!(
            row[marker_pos - cols - 1],
            '│',
            "the gutter starts right after the pane's left border"
        );
    }

    #[test]
    fn graphics_setting_off_collapses_the_gutter_even_on_a_graphics_terminal() {
        // #209: the user's `graphics` setting overrides a graphics-capable
        // terminal — off means off, same as a non-graphics terminal (#194).
        use ratatui_image::picker::{Picker, ProtocolType};

        let mut picker = Picker::halfblocks();
        picker.set_protocol_type(ProtocolType::Kitty);
        let mut app = app_with_history(vec![text_message(1, "hi")]);
        app.set_avatar_support(AvatarSupport::Graphics(picker));
        assert!(
            app.avatar_gutter_cols() > 0,
            "capable and enabled by default"
        );

        app.set_graphics_enabled(false);
        assert_eq!(app.avatar_gutter_cols(), 0, "the setting forces it to zero");
        let row: Vec<char> = row_text(&render(&app, 80, 24), 1).chars().collect();
        let marker_pos = row
            .iter()
            .position(|&c| c == '▶')
            .expect("selected marker present");
        assert_eq!(
            row[marker_pos - 1],
            '│',
            "no gutter at all: the marker sits right after the pane's border"
        );
    }

    #[test]
    fn no_open_chat_shows_the_empty_state_placeholder() {
        // With no open chat, the pane is the empty state (#188): app identity +
        // version and a CTA naming the real binding (Enter opens the selected chat).
        let text = flatten(&render(&App::new(), 80, 24));
        assert!(
            text.contains(&format!("tuigram v{}", tuigram_core::version())),
            "app identity + version"
        );
        assert!(
            text.contains("Select a chat and press Enter to start messaging"),
            "CTA naming the real key binding"
        );
        // The developer-era scaffolding copy is gone.
        assert!(!text.contains("Phase 5"), "no phase codename");
        assert!(text.contains("quit"), "status-bar quit hint");
    }

    #[test]
    fn open_chat_replaces_the_empty_state_with_history() {
        // The empty-state copy appears only with no chat open: an open chat's
        // history takes the pane, and the placeholder CTA is gone.
        let text = flatten(&render(
            &app_with_history(vec![text_message(1, "hi")]),
            80,
            24,
        ));
        assert!(text.contains("hi"), "history body shown");
        assert!(
            !text.contains("Select a chat and press Enter"),
            "no empty-state CTA once a chat is open"
        );
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
        // #222: ScrollDown is now a row step, not a message step. Each
        // message here is 3 rows (header, body, blank separator), so 120
        // steps advance exactly 40 messages.
        for _ in 0..120 {
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

    /// The number of frame rows [`HistoryRows`] maps to `message_id` — the
    /// render-level counterpart to [`ConversationView::message_height`], used
    /// below to check the inline-media box's row growth without inspecting an
    /// `Image` widget's actual pixel content (which `TestBackend` cannot
    /// meaningfully snapshot; see `graphics_avatar_support_indents_the_header_by_
    /// the_gutter_width` for the same limitation on the avatar path).
    fn rendered_row_count(output: &RenderOutput, message_id: i64) -> usize {
        (0..u16::MAX)
            .filter(|&row| output.history_rows.message_at(row) == Some(message_id))
            .count()
    }

    fn graphics_picker() -> ratatui_image::picker::Picker {
        use ratatui_image::picker::{Picker, ProtocolType};
        let mut picker = Picker::halfblocks();
        picker.set_protocol_type(ProtocolType::Kitty);
        picker
    }

    fn present_file(id: i32) -> File {
        File {
            id,
            size: 10,
            downloaded_size: 10,
            is_downloading_completed: true,
            local_path: format!("/tmp/{id}"),
            ..File::default()
        }
    }

    #[test]
    fn a_ready_photo_grows_by_the_media_box_and_keeps_its_placeholder() {
        let photo = sample_message(
            1,
            MessageContent::Photo(Photo {
                caption: FormattedText::default(),
                file: FileRef::new(7),
                width: 0,
                height: 0,
            }),
        );
        let mut app = app_with_history(vec![photo]);
        // The file is present from the start, so the pre-existing "✓ saved"
        // download line's own contribution to the row count stays constant
        // across the toggle below — isolating the delta to the media box alone.
        app.project_downloads(vec![present_file(7)]);
        // Tall enough that the full media box (16 rows) never gets clipped by
        // the pane's own truncation, which would otherwise confound the delta
        // this test isolates.
        let before = rendered_row_count(&render_output(&app, 80, 40), 1);

        app.set_avatar_support(AvatarSupport::Graphics(graphics_picker()));
        let output = render_output(&app, 80, 40);
        let after = rendered_row_count(&output, 1);

        assert_eq!(after, before + crate::conversation::MEDIA_ROWS);
        let text = flatten(&render(&app, 80, 40));
        assert!(
            text.contains("[Photo]"),
            "the placeholder stays even once the box is ready"
        );
    }

    #[test]
    fn a_pending_photo_on_a_graphics_terminal_stays_at_the_placeholder_height() {
        // Graphics-capable, but the file has not finished downloading yet.
        let photo = sample_message(
            1,
            MessageContent::Photo(Photo {
                caption: FormattedText::default(),
                file: FileRef::new(7),
                width: 0,
                height: 0,
            }),
        );
        let mut app = app_with_history(vec![photo]);
        app.set_avatar_support(AvatarSupport::Graphics(graphics_picker()));
        app.project_downloads(vec![File {
            id: 7,
            size: 100,
            downloaded_size: 40,
            is_downloading_active: true,
            ..File::default()
        }]);
        let output = render_output(&app, 80, 24);
        assert_eq!(
            rendered_row_count(&output, 1),
            1 + 1 + 1 + 1,
            "header + placeholder + download line + trailing blank, no media box"
        );
    }

    #[test]
    fn a_present_file_on_a_non_graphics_terminal_never_grows() {
        let photo = sample_message(
            1,
            MessageContent::Photo(Photo {
                caption: FormattedText::default(),
                file: FileRef::new(7),
                width: 0,
                height: 0,
            }),
        );
        let mut app = app_with_history(vec![photo]);
        // No `set_avatar_support` call: `AvatarSupport::None`, today's default.
        app.project_downloads(vec![present_file(7)]);
        let output = render_output(&app, 80, 24);
        assert_eq!(
            rendered_row_count(&output, 1),
            1 + 1 + 1 + 1,
            "a present file with no graphics support falls back cleanly"
        );
    }

    #[test]
    fn a_present_file_never_grows_when_the_graphics_setting_is_off() {
        // #209: the same fallback as a non-graphics terminal, but on a
        // graphics-capable one with the user's setting off — off means off even
        // though the terminal itself could render it.
        let photo = sample_message(
            1,
            MessageContent::Photo(Photo {
                caption: FormattedText::default(),
                file: FileRef::new(7),
                width: 0,
                height: 0,
            }),
        );
        let mut app = app_with_history(vec![photo]);
        app.set_avatar_support(AvatarSupport::Graphics(graphics_picker()));
        app.set_graphics_enabled(false);
        app.project_downloads(vec![present_file(7)]);
        let output = render_output(&app, 80, 24);
        assert_eq!(
            rendered_row_count(&output, 1),
            1 + 1 + 1 + 1,
            "graphics off falls back cleanly even on a graphics-capable terminal"
        );
    }

    #[test]
    fn a_video_still_needs_only_a_minithumbnail_no_download() {
        use tuigram_core::model::Video;

        let with_still = sample_message(
            1,
            MessageContent::Video(Video {
                caption: FormattedText::default(),
                file: FileRef::new(7),
                width: 0,
                height: 0,
                duration: 0,
                file_name: String::new(),
                mime_type: "video/mp4".to_owned(),
                minithumbnail: Some(b"jpeg bytes".to_vec()),
            }),
        );
        let without_still = sample_message(
            2,
            MessageContent::Video(Video {
                caption: FormattedText::default(),
                file: FileRef::new(8),
                width: 0,
                height: 0,
                duration: 0,
                file_name: String::new(),
                mime_type: "video/mp4".to_owned(),
                minithumbnail: None,
            }),
        );
        let mut app = app_with_history(vec![with_still, without_still]);
        app.set_avatar_support(AvatarSupport::Graphics(graphics_picker()));
        // Neither file is ever downloaded — a still needs none. Tall enough
        // that both messages (one with a 16-row media box) fit without
        // truncation.
        let output = render_output(&app, 80, 40);

        let placeholder_only = 1 + 1 + 1;
        assert_eq!(
            rendered_row_count(&output, 1),
            placeholder_only + crate::conversation::MEDIA_ROWS,
            "a minithumbnail alone is enough, no download required"
        );
        assert_eq!(
            rendered_row_count(&output, 2),
            placeholder_only,
            "no minithumbnail means no still, regardless of graphics support"
        );
        let text = flatten(&render(&app, 80, 24));
        assert!(text.contains("[▶ video]"), "the video badge");
    }

    #[test]
    fn an_animated_sticker_never_gets_a_still() {
        use tuigram_core::model::Sticker;

        let animated = sample_message(
            1,
            MessageContent::Sticker(Sticker {
                file: FileRef::new(7),
                width: 0,
                height: 0,
                emoji: "😀".to_owned(),
                is_static: false,
            }),
        );
        let mut app = app_with_history(vec![animated]);
        app.set_avatar_support(AvatarSupport::Graphics(graphics_picker()));
        // Even with its file fully downloaded, TDLib gives an animated sticker no
        // minithumbnail — #208 scopes a real still for it out (see conversation::media_ready).
        app.project_downloads(vec![present_file(7)]);
        let output = render_output(&app, 80, 24);
        assert_eq!(
            rendered_row_count(&output, 1),
            1 + 1 + 1 + 1,
            "header + placeholder + \"✓ saved\" line + trailing blank, no media box"
        );
    }

    #[test]
    fn a_ready_media_box_overlays_the_image_without_panicking() {
        // Exercises the second render pass's `Image` overlay itself (not just the
        // row reservation the tests above check), with a real (if trivial)
        // decoded `Protocol` — same stubbed-picker technique as the avatar tests,
        // since `TestBackend` cannot meaningfully snapshot graphics protocol
        // pixel content either way.
        let photo = sample_message(
            1,
            MessageContent::Photo(Photo {
                caption: FormattedText::default(),
                file: FileRef::new(7),
                width: 0,
                height: 0,
            }),
        );
        let mut app = app_with_history(vec![photo]);
        let picker = graphics_picker();
        app.set_avatar_support(AvatarSupport::Graphics(picker.clone()));
        app.project_downloads(vec![present_file(7)]);
        let image = image::DynamicImage::ImageRgb8(image::RgbImage::new(4, 4));
        let protocol = picker
            .new_protocol(
                image,
                ratatui::layout::Size::new(4, 4),
                ratatui_image::Resize::Fit(None),
            )
            .expect("halfblocks protocol always encodes");
        app.cache_media(1, protocol);

        let text = flatten(&render(&app, 80, 24));
        assert!(
            text.contains("[Photo]"),
            "placeholder still present alongside the decoded image"
        );
    }

    #[test]
    fn scrolling_into_a_media_message_shrinks_its_visible_row_count() {
        // #222: row-granular scrolling should show fewer of a message's own
        // rows as its header scrolls past the top, not jump it in/out all at
        // once — the bug this issue fixes, checked at the render level (the
        // conversation.rs tests already check the offset/row_skip math
        // itself).
        let photo = sample_message(
            1,
            MessageContent::Photo(Photo {
                caption: FormattedText::default(),
                file: FileRef::new(7),
                width: 0,
                height: 0,
            }),
        );
        let mut app = app_with_history(vec![photo, text_message(2, "after")]);
        app.set_avatar_support(AvatarSupport::Graphics(graphics_picker()));
        app.project_downloads(vec![present_file(7)]);

        let full = rendered_row_count(&render_output(&app, 80, 40), 1);
        assert!(
            full > crate::conversation::MEDIA_ROWS,
            "the ready photo's own block is taller than just the media box"
        );

        for _ in 0..5 {
            app.dispatch(Action::ScrollDown);
        }
        let partial = rendered_row_count(&render_output(&app, 80, 40), 1);
        assert_eq!(
            partial,
            full - 5,
            "5 row-steps shows exactly 5 fewer of message 1's rows"
        );
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
        // The composer's input row is the middle of its three rows, which now sit
        // just above the one-row status bar.
        let y = buffer.area.height - STATUS_HEIGHT - COMPOSER_HEIGHT + 1;
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
        for c in "kenobi".chars() {
            app.dispatch(Action::SearchInput(c));
        }
        app.dispatch(Action::SearchSubmit);
        // The hits arrive from the core search once it completes; inject them here.
        app.inject_search_results(vec![
            SearchHit::new(1, 10, "Alice: hello there"),
            SearchHit::new(2, 20, "Bob: general kenobi"),
        ]);
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
        app.dispatch(Action::SearchInput('q')); // a query whose search returns nothing
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
        view.set_downloads(vec![File {
            id: 7,
            size: 100,
            downloaded_size: 45,
            is_downloading_active: true,
            ..File::default()
        }]);
        let text = flatten(&render(&App::with_conversation(view), 80, 24));
        assert!(text.contains("[Photo]"), "media placeholder");
        assert!(
            text.contains("downloading 45%"),
            "download progress indicator"
        );
    }

    #[test]
    fn message_height_matches_the_rendered_line_count() {
        // The bottom-anchoring walk (#158) sums `ConversationView::message_height`;
        // this guards it against drifting from what `message_lines` actually renders —
        // the two are a single source split across the view model and the renderer.
        use tuigram_core::model::{FileRef, Photo, ReactionKind};

        let mut reacted = text_message(4, "nice");
        reacted.reactions = vec![Reaction {
            kind: ReactionKind::Emoji("👍".to_owned()),
            count: 2,
            is_chosen: true,
        }];
        let photo = sample_message(
            3,
            MessageContent::Photo(Photo {
                caption: FormattedText {
                    text: "a caption\non two lines".to_owned(),
                    entities: Vec::new(),
                },
                file: FileRef::new(7),
                width: 0,
                height: 0,
            }),
        );
        let messages = vec![
            text_message(1, "single line"),
            text_message(2, "line one\nline two\nline three"),
            photo,
            reacted,
        ];
        // One pin (height-neutral) and an active download that adds a progress line.
        let mut view = ConversationView::from_messages(messages, HashSet::from([1]));
        view.set_downloads(vec![File {
            id: 7,
            size: 100,
            downloaded_size: 40,
            is_downloading_active: true,
            ..File::default()
        }]);

        // Width `0` is the pre-#214 unwrapped case; `40` fits every message's
        // longest line unwrapped too (a width-independence sanity check); `5`
        // is narrow enough to force real wrapping on the multi-word text and
        // the two-line caption, exercising the actual drift guard #214 adds.
        for width in [0, 40, 5] {
            view.set_viewport_width(width);
            for message in view.messages() {
                // The selection marker only prefixes the header and never
                // changes the row count. Neither does a non-zero gutter
                // (#201) — it only prepends a span to existing lines, never
                // adds one — so both are checked here. No graphics support is
                // seeded, so every message's media_rows is `0` here; the
                // ready-media case gets its own test below, since it needs an
                // `App` (graphics capability lives there, not on the bare view).
                for selected in [false, true] {
                    for gutter_cols in [0, 4] {
                        assert_eq!(
                            message_lines(&view, message, selected, gutter_cols, 0, width).len(),
                            view.message_height(message),
                            "height drifts from the renderer for message {} at width {width}",
                            message.id
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn message_height_matches_the_rendered_line_count_with_a_ready_media_box() {
        // Same drift guard as above, but for the cases that change a message's
        // height at all (#208): graphics-capable and the content ready
        // (Photo's file present, Video's embedded minithumbnail) — plus one
        // that stays *not* ready despite graphics support and a present file
        // (an animated Sticker), so height/render parity is pinned for every
        // #208 content type this PR touches, not just Photo by coincidence of
        // a shared constant. Stubbed picker, same technique as
        // `graphics_avatar_support_indents_the_header_by_the_gutter_width`.
        use ratatui_image::picker::{Picker, ProtocolType};
        use tuigram_core::model::{FileRef, Photo, Sticker, Video};

        let photo = sample_message(
            1,
            MessageContent::Photo(Photo {
                caption: FormattedText::default(),
                file: FileRef::new(7),
                width: 0,
                height: 0,
            }),
        );
        let video = sample_message(
            2,
            MessageContent::Video(Video {
                caption: FormattedText::default(),
                file: FileRef::new(8),
                width: 0,
                height: 0,
                duration: 0,
                file_name: String::new(),
                mime_type: "video/mp4".to_owned(),
                minithumbnail: Some(b"jpeg bytes".to_vec()),
            }),
        );
        let animated_sticker = sample_message(
            3,
            MessageContent::Sticker(Sticker {
                file: FileRef::new(9),
                width: 0,
                height: 0,
                emoji: "😀".to_owned(),
                is_static: false,
            }),
        );
        let mut app = app_with_history(vec![photo, video, animated_sticker]);
        let mut picker = Picker::halfblocks();
        picker.set_protocol_type(ProtocolType::Kitty);
        app.set_avatar_support(AvatarSupport::Graphics(picker));
        app.project_downloads(vec![
            File {
                id: 7,
                size: 10,
                downloaded_size: 10,
                is_downloading_completed: true,
                local_path: "/tmp/7".to_owned(),
                ..File::default()
            },
            // The animated sticker's file is present too, to prove it is
            // `media_ready`'s missing minithumbnail keeping it unreserved, not
            // an incidentally-absent download.
            File {
                id: 9,
                size: 10,
                downloaded_size: 10,
                is_downloading_completed: true,
                local_path: "/tmp/9".to_owned(),
                ..File::default()
            },
        ]);

        let view = app.conversation();
        let expected_ready = [true, true, false];
        for (message, expect_ready) in view.messages().iter().zip(expected_ready) {
            let media_rows = media_rows_for(&app, view, &message.content);
            assert_eq!(
                media_rows > 0,
                expect_ready,
                "readiness mismatch for message {}",
                message.id
            );
            assert_eq!(
                message_lines(view, message, true, 0, media_rows, 0).len(),
                view.message_height(message),
                "height drifts from the renderer for message {}",
                message.id
            );
        }
    }

    #[test]
    fn message_height_matches_the_rendered_line_count_with_graphics_setting_off() {
        // #209: same drift guard, but graphics-capable terminal with the user's
        // setting off — every message collapses to its placeholder height on
        // both independent gates (`ui::media_ready`'s `app.graphics_active()`
        // and `ConversationView.graphics_capable`, kept in sync via
        // `App::sync_graphics_capable`), not just when the terminal itself
        // lacks graphics support.
        use ratatui_image::picker::{Picker, ProtocolType};
        use tuigram_core::model::{FileRef, Photo, Video};

        let photo = sample_message(
            1,
            MessageContent::Photo(Photo {
                caption: FormattedText::default(),
                file: FileRef::new(7),
                width: 0,
                height: 0,
            }),
        );
        let video = sample_message(
            2,
            MessageContent::Video(Video {
                caption: FormattedText::default(),
                file: FileRef::new(8),
                width: 0,
                height: 0,
                duration: 0,
                file_name: String::new(),
                mime_type: "video/mp4".to_owned(),
                minithumbnail: Some(b"jpeg bytes".to_vec()),
            }),
        );
        let mut app = app_with_history(vec![photo, video]);
        let mut picker = Picker::halfblocks();
        picker.set_protocol_type(ProtocolType::Kitty);
        app.set_avatar_support(AvatarSupport::Graphics(picker));
        app.set_graphics_enabled(false);
        app.project_downloads(vec![File {
            id: 7,
            size: 10,
            downloaded_size: 10,
            is_downloading_completed: true,
            local_path: "/tmp/7".to_owned(),
            ..File::default()
        }]);

        let view = app.conversation();
        for message in view.messages() {
            let media_rows = media_rows_for(&app, view, &message.content);
            assert_eq!(
                media_rows, 0,
                "graphics off: never ready, message {}",
                message.id
            );
            assert_eq!(
                message_lines(view, message, true, 0, media_rows, 0).len(),
                view.message_height(message),
                "height drifts from the renderer for message {}",
                message.id
            );
        }
    }

    #[test]
    fn a_long_message_wraps_across_multiple_rows_in_the_render() {
        // #214 end-to-end: a message body too long for the pane's width
        // actually wraps when rendered, rather than being cut off by
        // ratatui's default paragraph behavior — the render-level complement
        // to the height/drift unit tests above. A live loop feeds the real
        // measured width back through `App::set_conversation_width`, so an
        // `App` built directly (as here) never wraps on its very first,
        // never-yet-measured render — seed the width the way the loop would
        // after one frame.
        let long = "wraps across several rows because it is much longer than the pane";
        let mut app = app_with_history(vec![text_message(1, long)]);
        let output = render_output(&app, 40, 20);
        app.set_conversation_width(output.convo_width);
        let buffer = render(&app, 40, 20);
        let text = flatten(&buffer);
        // Every word survives the wrap (nothing silently dropped or truncated).
        for word in long.split(' ') {
            assert!(text.contains(word), "word {word:?} missing from the render");
        }
        // The body spans at least two distinct rows of the pane.
        let body_rows = (0..buffer.area.height)
            .filter(|&y| {
                long.split(' ')
                    .any(|word| row_text(&buffer, y).contains(word))
            })
            .count();
        assert!(
            body_rows >= 2,
            "body should wrap across multiple rows, got {body_rows}"
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
        assert!(text.contains("Enter react"), "key hint");
        assert!(text.contains("custom emoji"), "the custom-entry affordance");
        // The first palette entry is selected.
        assert!(
            row_containing(&buffer, "👍").contains('▶'),
            "first emoji selected"
        );
    }

    #[test]
    fn the_reaction_picker_shows_the_custom_entry_line() {
        let mut app = app_with_history(vec![text_message(1, "nice")]);
        app.dispatch(Action::ReactionOpen);
        // Enter the custom line and type an emoji.
        app.dispatch(Action::ReactionKey('c'));
        app.dispatch(Action::ReactionKey('🥳'));
        let buffer = render(&app, 80, 24);
        let text = flatten(&buffer);
        assert!(text.contains("custom"), "the custom-entry label");
        assert!(text.contains('🥳'), "the typed emoji");
        assert!(
            text.contains("Esc back"),
            "custom-mode hint returns to palette"
        );
        // The palette list is not shown while typing a custom emoji.
        assert!(!text.contains("j / k move"), "palette hint is gone");
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

    // --- secret chats & chat-action indicators (#87) ---

    use tuigram_core::model::{ChatAction, ChatKind, SecretChatState};

    /// A chat-list view holding one chat of `kind`, with an optional secret state.
    fn view_with_one_chat(
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

    #[test]
    fn the_conversation_header_shows_a_typing_indicator() {
        let mut view = ConversationView::from_messages(vec![text_message(1, "hi")], HashSet::new());
        view.set_chat_action(Some(ChatAction::RecordingVoiceNote));
        let text = flatten(&render(&App::with_conversation(view), 80, 24));
        assert!(text.contains("Conversation"), "pane header");
        assert!(
            text.contains("recording a voice message"),
            "header indicator"
        );
    }

    #[test]
    fn the_conversation_header_shows_the_resolved_sender_name() {
        // A projected sender label (#160) replaces the numeric `User {id}` in the
        // bold message header.
        let mut view = ConversationView::default();
        view.project(
            10,
            vec![text_message(1, "hi")],
            HashSet::new(),
            HashMap::from([(
                Sender::User(1),
                SenderLabel {
                    label: "Ada Lovelace (@ada)".to_owned(),
                    color: Some(Color::Red),
                },
            )]),
            i64::MAX,
            0,
            true,
        );
        let text = flatten(&render(&App::with_conversation(view), 80, 24));
        assert!(
            text.contains("Ada Lovelace (@ada)"),
            "resolved sender name in the header"
        );
        assert!(!text.contains("User 1"), "numeric fallback replaced");
    }

    #[test]
    fn the_conversation_header_shows_the_timestamp_before_the_sender_name() {
        // The header reads `HH:MM Name (@handle)` (#194) — the timestamp always
        // comes first so it lines up in a fixed column regardless of name length.
        let mut view = ConversationView::default();
        view.project(
            10,
            vec![text_message(1, "hi")],
            HashSet::new(),
            HashMap::from([(
                Sender::User(1),
                SenderLabel {
                    label: "Ada Lovelace (@ada)".to_owned(),
                    color: Some(Color::Red),
                },
            )]),
            i64::MAX,
            0,
            true,
        );
        let buffer = render(&App::with_conversation(view), 80, 24);
        let row = row_containing(&buffer, "Ada Lovelace (@ada)");
        let time_at = row.find(':').map(|colon| colon - 2).unwrap_or(usize::MAX);
        let name_at = row.find("Ada").unwrap_or(usize::MAX);
        assert!(
            time_at < name_at,
            "timestamp should render before the sender name, got row {row:?}"
        );
    }

    /// The index of the first row whose text contains `needle`, for locating a
    /// message's header row relative to its (uniquely worded) body row below it.
    fn row_index_containing(buffer: &Buffer, needle: &str) -> u16 {
        (0..buffer.area.height)
            .find(|&y| row_text(buffer, y).contains(needle))
            .unwrap_or_else(|| panic!("no row contains {needle:?}"))
    }

    #[test]
    fn outgoing_messages_show_their_delivery_status_glyph() {
        // #163: ⌛ pending, a plain ✓ once sent but not yet read, ✓✓ once the
        // chat's outbox watermark has passed the message's id (read), and ✗ for a
        // rejected send — each read off the message's own header row, one row
        // above its (uniquely worded) body so the right glyph is checked.
        let outgoing = |id: i64, body: &str, state: SendState| {
            let mut m = text_message(id, body);
            m.is_outgoing = true;
            m.send_state = state;
            m
        };
        let mut view = ConversationView::default();
        // A generous viewport so the bottom-anchoring open (#158) lands on the
        // first message rather than the one-frame fallback of the newest alone.
        view.set_viewport_height(100);
        view.project(
            10,
            vec![
                outgoing(1, "pending msg", SendState::Pending),
                outgoing(2, "read msg", SendState::Sent),
                outgoing(3, "sent msg", SendState::Sent),
                outgoing(
                    4,
                    "failed msg",
                    SendState::Failed {
                        code: 400,
                        message: "FLOOD_WAIT".to_owned(),
                    },
                ),
            ],
            HashSet::new(),
            HashMap::new(),
            0,
            2, // last_read_outbox: message 2 has been read, message 3 has not
            true,
        );
        let buffer = render(&App::with_conversation(view), 80, 40);
        let header_above =
            |body_needle| row_text(&buffer, row_index_containing(&buffer, body_needle) - 1);

        assert!(
            header_above("pending msg").contains('⌛'),
            "a send in flight shows the hourglass"
        );
        assert!(
            header_above("sent msg").contains('✓') && !header_above("sent msg").contains("✓✓"),
            "sent but not yet read shows a plain check"
        );
        assert!(
            header_above("read msg").contains("✓✓"),
            "read (id at or before the outbox watermark) shows the double check"
        );
        assert!(
            header_above("failed msg").contains('✗'),
            "a rejected send shows the cross"
        );
        assert!(
            flatten(&buffer).contains("send failed (400): FLOOD_WAIT"),
            "the failed message's error detail surfaces inline"
        );
    }

    #[test]
    fn incoming_messages_never_show_a_delivery_glyph() {
        let view = ConversationView::from_messages(vec![text_message(1, "hi")], HashSet::new());
        let text = flatten(&render(&App::with_conversation(view), 80, 24));
        assert!(!text.contains('⌛') && !text.contains('✓') && !text.contains('✗'));
    }

    #[test]
    fn opening_a_chat_with_unread_messages_draws_the_rule_above_the_first_one() {
        // #164: last_read_inbox = 1, so message 2 is the first unread message —
        // the rule renders on the row immediately above its header.
        let mut view = ConversationView::default();
        view.set_viewport_height(100);
        view.project(
            10,
            vec![text_message(1, "read msg"), text_message(2, "second msg")],
            HashSet::new(),
            HashMap::new(),
            1,
            0,
            true,
        );
        let buffer = render(&App::with_conversation(view), 80, 40);
        // Line order for message 2: rule, header, body ("second msg") — the rule
        // sits two rows above the body, directly above the header.
        let second_row = row_index_containing(&buffer, "second msg");
        assert!(
            row_text(&buffer, second_row - 2).contains("── unread ──"),
            "the rule sits immediately above the first unread message's header"
        );
        assert_eq!(
            flatten(&buffer).matches("── unread ──").count(),
            1,
            "the rule appears exactly once"
        );
    }

    #[test]
    fn a_fully_read_chat_shows_no_unread_rule() {
        let mut view = ConversationView::default();
        view.set_viewport_height(100);
        view.project(
            10,
            vec![text_message(1, "hi"), text_message(2, "there")],
            HashSet::new(),
            HashMap::new(),
            i64::MAX,
            0,
            true,
        );
        let text = flatten(&render(&App::with_conversation(view), 80, 40));
        assert!(!text.contains("── unread ──"));
    }

    #[test]
    fn format_time_in_converts_using_the_given_timezone() {
        // 2024-01-01T23:30:00Z, viewed at a fixed UTC+9 offset, reads 08:30 the
        // next day — verified independent of the host machine's local timezone
        // (which `hour_minute` uses in production via `chrono::Local`).
        let date = 1_704_151_800;
        let tz = chrono::FixedOffset::east_opt(9 * 3600).unwrap();
        assert_eq!(format_time_in(date, &tz), "08:30");
    }

    #[test]
    fn the_secret_chat_overlay_poses_the_lifecycle_question() {
        // sample_chat(7, …) is a private chat → the offered action is "start".
        let view = view_with_one_chat("Alice", ChatKind::Private { user_id: 7 }, None);
        let mut app = App::with_chat_list(view);
        app.dispatch(Action::SecretOpen);
        let text = flatten(&render(&app, 80, 24));
        assert!(text.contains("Secret chat"), "overlay title");
        assert!(text.contains("Start"), "the lifecycle action");
        assert!(text.contains("Alice"), "names the chat");
        assert!(text.contains("Enter confirm"), "key hint");
    }

    // --- status bar & notifications (#88) ---

    use crate::status::{ConnectionState, Notice};

    /// The bottom status row of a rendered buffer.
    fn status_row(buffer: &Buffer) -> String {
        row_text(buffer, buffer.area.height - 1)
    }

    #[test]
    fn the_status_bar_sits_on_the_bottom_row_with_connection_and_hint() {
        let buffer = render(&App::new(), 80, 24);
        let bar = status_row(&buffer);
        // Default connection is "connecting…", and the quit/help hint the
        // placeholder used to carry now lives here.
        assert!(bar.contains("connecting"), "connection state: {bar:?}");
        assert!(bar.contains("quit"), "quit hint on the bar");
        assert!(bar.contains("help"), "help hint on the bar");
    }

    #[test]
    fn the_status_bar_reflects_the_connection_state() {
        let mut app = App::new();
        app.set_connection(ConnectionState::Ready);
        assert!(status_row(&render(&app, 80, 24)).contains("online"));
    }

    #[test]
    fn the_status_bar_names_the_current_chat_and_mode() {
        let view = view_with_one_chat("Alice", ChatKind::Private { user_id: 7 }, None);
        let app = App::with_chat_list(view);
        let bar = status_row(&render(&app, 80, 24));
        assert!(bar.contains("Alice"), "current chat: {bar:?}");
        assert!(bar.contains("chats"), "focused-pane mode");
    }

    #[test]
    fn no_toast_renders_with_an_empty_queue() {
        // The placeholder banner is the only thing in the conversation pane.
        let text = flatten(&render(&App::new(), 80, 24));
        assert!(!text.contains("Notice"), "no toast box without a notice");
    }

    #[test]
    fn a_toast_floats_over_the_panes_with_its_message() {
        let mut app = App::new();
        app.notify(Notice::info("download complete"));
        let text = flatten(&render(&app, 80, 24));
        assert!(text.contains("Notice"), "toast box title");
        assert!(text.contains("download complete"), "toast message");
    }

    #[test]
    fn a_toast_shows_how_to_dismiss_it() {
        // The box carries the dismiss affordance (#139) so the user is never left
        // wondering how to clear a notice.
        let mut app = App::new();
        app.notify(Notice::info("download complete"));
        let text = flatten(&render(&app, 80, 24));
        assert!(text.contains("Ctrl-G"), "dismiss hint on the toast");
    }

    #[test]
    fn an_error_toast_surfaces_its_code() {
        let mut app = App::new();
        app.notify(Notice::error("send", Some("FLOOD_WAIT")));
        let text = flatten(&render(&app, 80, 24));
        assert!(text.contains("send failed"), "error category");
        assert!(text.contains("FLOOD_WAIT"), "error code");
    }

    #[test]
    fn a_queued_toast_shows_a_pending_count() {
        let mut app = App::new();
        app.notify(Notice::info("first"));
        app.notify(Notice::info("second"));
        let text = flatten(&render(&app, 80, 24));
        // The front shows; the title hints at the one waiting behind it.
        assert!(text.contains("first"), "front toast");
        assert!(text.contains("+1"), "pending count");
    }
}
