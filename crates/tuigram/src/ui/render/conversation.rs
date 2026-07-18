//! The conversation pane (#81): the scrolling message history — headers,
//! bodies/media placeholders, quotes (#210), reactions (#51), and the inline
//! avatar/media image overlays (#201/#208) — plus its empty state (#188).

use ratatui::Frame;
use ratatui::layout::{Alignment, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState};
use ratatui_image::Image;
use ratatui_image::protocol::Protocol;

use tuigram_core::model::{
    File, FormattedText, Message, MessageContent, ReactionKind, ReplyTo, SendState, Sender,
    TextEntity,
};

use crate::app::App;
use crate::conversation::ConversationView;
use crate::keymap::Focus;
use crate::ui::HistoryRows;

use super::chat_list::{action_phrase, delivery_glyph};
use super::common::{SELECTED_SYMBOL, pane_block, truncate};

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
/// and [`ui`](crate::ui::ui) records the same value back onto `App` via
/// [`RenderOutput::convo_width`](crate::ui::RenderOutput::convo_width) — both
/// call this one formula so the two can never drift.
pub(crate) fn convo_body_width(area: Rect, gutter_cols: usize) -> usize {
    (area.width.saturating_sub(2 + gutter_cols as u16)).max(1) as usize
}

/// Right/top pane: the conversation history (#81). Renders the open chat's
/// messages — each a sender/timestamp header, a body or media placeholder, and a
/// reaction line — windowed forward from the scroll offset so a long history never
/// builds the whole buffer, with a scrollbar tracking the offset. With no chat open
/// the view is empty, so the pane falls through to the empty-state placeholder (#188).
pub(crate) fn render_conversation(frame: &mut Frame, area: Rect, app: &App) -> HistoryRows {
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
        let quote_rows = quote_lines(view, message, width).len();
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

    // New-messages indicator: mirrors the official app's down-arrow, drawn as
    // a small rounded badge in the pane's bottom-right corner whenever a
    // message has arrived below the fold while the reader was scrolled away
    // from the newest one (cleared as soon as they scroll or jump back down —
    // see `ConversationView::project`). Anchored so its bottom-right cell
    // sits one column left of the scrollbar and one row above the bottom
    // border, the same spot the earlier single-glyph version occupied.
    if view.has_new_messages_below() && area.width > 6 && area.height > 5 {
        const BADGE: [&str; 3] = ["╭─╮", "│▼│", "╰─╯"];
        let rect = Rect {
            x: area.x + area.width - 5,
            y: area.y + area.height - 4,
            width: 3,
            height: 3,
        };
        let style = Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD);
        let lines: Vec<Line> = BADGE
            .iter()
            .map(|row| Line::from(Span::styled(*row, style)))
            .collect();
        frame.render_widget(Paragraph::new(lines), rect);
    }

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
#[must_use]
pub fn message_lines(
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
        quote_lines(view, message, width)
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

/// The greentext quote line(s) above a reply's body (#210, word-wrapped at
/// `width` since #214) — none for a plain message, otherwise the preview
/// text word-wrapped the same way a message body is (`width == 0` skips
/// wrapping, one line, matching pre-#214 behavior).
/// [`ConversationView::message_height`] adds the same row count via its own
/// independent `quote_rows`, guarded by a drift-guard test below.
///
/// Resolution is **render-time**, not cached on the model: it looks up the
/// quoted message in `view`'s currently loaded history, so a target that
/// loads later (a history page paged in after this reply was first seen)
/// naturally resolves on the next render — the mechanism #207 left in place
/// for exactly this kind of live catch-up. A target in another chat, one
/// deleted or outside the loaded window, or a reply to a story all fall back
/// to a bare `>reply`.
fn quote_lines(view: &ConversationView, message: &Message, width: usize) -> Vec<Line<'static>> {
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
            .map_or_else(
                || ">reply".to_owned(),
                |quoted| {
                    let sender = view.sender_label(quoted).label;
                    format!(
                        ">{sender}: {}",
                        truncate(&content_snippet(&quoted.content), 60)
                    )
                },
            ),
        ReplyTo::Message { .. } | ReplyTo::Unsupported(_) => ">reply".to_owned(),
    };
    if width == 0 {
        return vec![Line::from(Span::styled(text, style))];
    }
    let breaks = crate::wrap::wrap_breaks(&text, width);
    breaks
        .iter()
        .enumerate()
        .map(|(i, &start)| {
            let end = breaks.get(i + 1).copied().unwrap_or(text.len());
            Line::from(Span::styled(text[start..end].to_owned(), style))
        })
        .collect()
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

/// The failed-send detail line (#163): `TDLib`'s error code and message, shown
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

#[cfg(test)]
#[allow(clippy::unwrap_used)] // tests: panicking on a broken assumption is the point
mod tests {
    use std::collections::{HashMap, HashSet};

    use ratatui::buffer::Buffer;

    use tuigram_core::model::{ChatAction, FileRef, Photo, Reaction};

    use crate::app::Action;
    use crate::conversation::{SenderLabel, sample_message};
    use crate::terminal::AvatarSupport;

    use super::super::test_support::{
        app_with_history, flatten, graphics_picker, present_file, render, render_output,
        rendered_row_count, row_containing, row_text, text_message,
    };
    use super::*;

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

        let lines = quote_lines(&view, &reply, 0);
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

        let lines = quote_lines(&view, &reply, 0);
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
        let before_text: String = quote_lines(&before, &reply, 0)[0]
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
        let after_text: String = quote_lines(&after, &reply, 0)[0]
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
        let message = sample_message(
            1,
            MessageContent::Text(FormattedText {
                text: "hi".to_owned(),
                entities: Vec::new(),
            }),
        );
        let view = ConversationView::from_messages(vec![message.clone()], HashSet::new());
        assert!(quote_lines(&view, &message, 0).is_empty());
    }

    /// #210: `TDLib` documents `MessageReplyToMessage.chat_id` as "may be 0 if
    /// the replied message is in unknown chat" — a same-chat reply must still
    /// resolve rather than always falling back to bare `>reply` if `TDLib`
    /// reports `0` here instead of the real (matching) chat id.
    #[test]
    fn quote_lines_resolves_a_reply_whose_chat_id_is_reported_as_zero() {
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

        let lines = quote_lines(&view, &reply, 0);
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("original message"), "snippet: {text:?}");
    }

    /// #214: a narrow pane wraps the greentext preview across multiple rows
    /// instead of letting ratatui silently clip it — the bug report that
    /// motivated giving `quote_lines` a `width` at all.
    #[test]
    fn quote_lines_wraps_a_long_preview_at_a_narrow_width() {
        use tuigram_core::model::ReplyTo;

        let original = sample_message(
            1,
            MessageContent::Text(FormattedText {
                text: "a fairly long original message that will need to wrap".to_owned(),
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

        let lines = quote_lines(&view, &reply, 10);
        assert!(
            lines.len() > 1,
            "expected the preview to wrap across multiple rows, got {}",
            lines.len()
        );
        // Nothing is dropped — every wrapped row's content, joined back
        // together, reconstructs the original unwrapped preview text.
        let unwrapped: String = quote_lines(&view, &reply, 0)[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        let rewrapped: String = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(rewrapped, unwrapped);
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
    fn the_new_messages_indicator_renders_in_the_bottom_right_corner_when_set() {
        // Scroll away from the newest message, then project a genuinely new
        // one arriving at the tail — `has_new_messages_below` is now set, and
        // the render pass should draw the rounded badge one column left of
        // the scrollbar and one row above the bottom border.
        let mut view = ConversationView::default();
        view.set_viewport_height(6); // fits two 3-row messages
        view.project(
            10,
            (1..=4).map(|i| text_message(i, "m")).collect(),
            HashSet::new(),
            HashMap::new(),
            i64::MAX,
            0,
            true,
        );
        view.scroll_up();
        view.project(
            10,
            (1..=5).map(|i| text_message(i, "m")).collect(),
            HashSet::new(),
            HashMap::new(),
            i64::MAX,
            0,
            false,
        );
        assert!(view.has_new_messages_below());

        let app = App::with_conversation(view);
        let output = render_output(&app, 80, 24);
        let buffer = render(&app, 80, 24);
        let area = output.panes.history;
        let x = area.x + area.width - 5;
        let y = area.y + area.height - 4;
        assert_eq!(buffer[(x, y)].symbol(), "╭", "top-left corner");
        assert_eq!(buffer[(x + 2, y)].symbol(), "╮", "top-right corner");
        assert_eq!(buffer[(x + 1, y + 1)].symbol(), "▼", "centered arrow");
        assert_eq!(buffer[(x, y + 2)].symbol(), "╰", "bottom-left corner");
        assert_eq!(buffer[(x + 2, y + 2)].symbol(), "╯", "bottom-right corner");
    }

    #[test]
    fn the_new_messages_indicator_is_absent_when_not_set() {
        let output = render_output(&app_with_history(vec![text_message(1, "m1")]), 80, 24);
        let buffer = render(&app_with_history(vec![text_message(1, "m1")]), 80, 24);
        let area = output.panes.history;
        let x = area.x + area.width - 5;
        let y = area.y + area.height - 4;
        assert_ne!(buffer[(x + 1, y + 1)].symbol(), "▼");
        assert_ne!(buffer[(x, y)].symbol(), "╭");
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

    /// Reported: scrolling up through a media message into one from a sender
    /// with a long username left a fragment of that name "stuck", duplicating
    /// into a column of garbage as scrolling up continued, recovering when
    /// scrolling back down. This drives one `Terminal` across many draws
    /// (unlike `render`/`render_output`, which each start a fresh one) so it
    /// actually exercises ratatui's own cross-frame buffer diffing — the same
    /// as the real run loop's persistent `Terminal`. If a stale-cell bug lived
    /// in this codebase's own render/scroll path (rather than a real
    /// terminal's graphics-protocol pixel ghosting, which `TestBackend` never
    /// executes at all), this is where it would show up: the long name's text
    /// appearing in more than one row at once.
    #[test]
    fn scrolling_past_media_never_leaves_a_long_username_in_two_rows_at_once() {
        let long_user = Sender::User(99);
        let long_name = "Alexandrapetrovnakuznetsovaverylongusername";
        let photo = sample_message(
            1,
            MessageContent::Photo(Photo {
                caption: FormattedText::default(),
                file: FileRef::new(7),
                width: 0,
                height: 0,
            }),
        );
        let named = Message {
            id: 2,
            chat_id: 1,
            sender: long_user.clone(),
            date: 0,
            edit_date: 0,
            is_outgoing: false,
            content: MessageContent::Text(FormattedText {
                text: "hi".to_owned(),
                entities: Vec::new(),
            }),
            send_state: SendState::Sent,
            reactions: Vec::new(),
            reply_to: None,
        };
        let mut messages = vec![photo, named];
        messages.extend((3..15).map(|i| text_message(i, &format!("filler {i}"))));

        let mut senders = HashMap::new();
        senders.insert(
            long_user,
            SenderLabel {
                label: long_name.to_owned(),
                color: None,
            },
        );
        let mut view = ConversationView::default();
        view.project(1, messages, HashSet::new(), senders, 0, 0, true);

        let mut app = App::with_conversation(view);
        app.set_avatar_support(AvatarSupport::Graphics(graphics_picker()));
        app.project_downloads(vec![present_file(7)]);
        let picker = graphics_picker();
        let build_protocol = |picker: &ratatui_image::picker::Picker| {
            picker
                .new_protocol(
                    image::DynamicImage::ImageRgb8(image::RgbImage::new(4, 4)),
                    ratatui::layout::Size::new(4, 4),
                    ratatui_image::Resize::Fit(None),
                )
                .expect("halfblocks protocol always encodes")
        };
        app.cache_media(1, build_protocol(&picker));
        // Cache an avatar for every sender (including the long-username one),
        // so scrolling actually shifts different avatar images through the
        // same gutter rows across frames — the specific mechanism a stale-cell
        // bug would show up in, per the #229 ghosting precedent (avatars sit
        // in the gutter, on the header row, as a second-pass `Image` overlay).
        for user_id in [1, 99].into_iter().chain(3..15) {
            app.cache_avatar(user_id, build_protocol(&picker));
        }

        // A short, distinctive slice of the long name — enough to identify a
        // fragment without depending on where a wrap/truncation would cut it.
        let needle = &long_name[..15];

        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 15)).unwrap();
        terminal
            .draw(|frame| drop(crate::ui::ui(frame, &app)))
            .unwrap();

        // Scroll up several times, redrawing on the *same* terminal each step —
        // exactly where a leftover-cell bug (if this codebase has one) would
        // show the fragment surviving into a row it no longer belongs to.
        for step in 0..12 {
            app.dispatch(Action::ScrollUp);
            terminal
                .draw(|frame| drop(crate::ui::ui(frame, &app)))
                .unwrap();
            let buffer = terminal.backend().buffer();
            let rows_with_fragment: Vec<u16> = (0..buffer.area.height)
                .filter(|&y| row_text(buffer, y).contains(needle))
                .collect();
            assert!(
                rows_with_fragment.len() <= 1,
                "step {step}: the long username fragment appeared in more than \
                 one row at once: {rows_with_fragment:?}"
            );
        }
    }

    #[test]
    fn a_pinned_message_shows_the_pin_marker() {
        let view =
            ConversationView::from_messages(vec![text_message(7, "pinned")], HashSet::from([7]));
        let text = flatten(&render(&App::with_conversation(view), 80, 24));
        assert!(text.contains("📌"), "pin marker on the pinned message");
    }

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
        use tuigram_core::model::ReplyTo;

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
        // A reply (#210/#214): its greentext preview line is another row
        // `message_height` must count, on top of the header/body/separator —
        // the gap this drift guard is here to catch.
        let mut replying = text_message(5, "sure thing");
        replying.reply_to = Some(ReplyTo::Message {
            chat_id: replying.chat_id,
            message_id: 1,
            quote: None,
        });
        let messages = vec![
            text_message(1, "single line"),
            text_message(2, "line one\nline two\nline three"),
            photo,
            reacted,
            replying,
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
        use tuigram_core::model::{Sticker, Video};

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
        use tuigram_core::model::Video;

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
        let time_at = row.find(':').map_or(usize::MAX, |colon| colon - 2);
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
}
