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
//!
//! Split by pane/overlay group (#182b) into [`render`]: `chat_list`,
//! `conversation`, `composer`, `help`, `status`, `overlays`, plus a `common`
//! module for the primitives more than one of those share. This file stays
//! the orchestrator: the frame layout ([`PaneLayout`]/[`pane_layout`]), the
//! row maps the loop hit-tests clicks against, and [`ui`] itself, which calls
//! into every pane/overlay's render function for one frame.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};

use crate::app::App;
use crate::keymap::{Focus, Overlay};

mod render;

use render::{
    convo_body_width, render_chat_list, render_composer, render_contact_search_input,
    render_contact_search_results, render_conversation, render_delete_confirm, render_forward,
    render_help, render_logout_confirm, render_reaction, render_search_input,
    render_search_results, render_secret_chat, render_send_media, render_settings,
    render_status_bar, render_toast,
};

pub use render::message_lines;
pub(crate) use render::{hint_line, input_line, media_cols};

/// Chat-list pane width, as a percentage of the terminal; the conversation pane
/// fills the remainder. (The research doc allows fixed *or* percentage width;
/// percentage keeps the skeleton responsive across terminal sizes.)
const CHAT_LIST_PERCENT: u16 = 30;

/// The composer's height at its 1-row floor (an empty or single-row buffer):
/// one input line framed by a border. [`pane_layout`] computes the composer's
/// actual height dynamically, up to [`MAX_COMPOSER_ROWS`] (#215) — this is
/// only the fixed baseline tests render against with an empty composer.
#[cfg(test)]
const COMPOSER_HEIGHT: u16 = 3;

/// The composer's growth cap, in visual rows of text (#215): beyond this,
/// the pane stops growing and scrolls internally instead.
const MAX_COMPOSER_ROWS: u16 = 5;

/// Status-bar height in rows: a single strip across the bottom (#88).
const STATUS_HEIGHT: u16 = 1;

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
///
/// `chat_list_collapsed` (#213) zeroes the list column so the conversation side
/// takes the full width; `Rect::contains` on a zero-area rect is always `false`,
/// so mouse hit-testing and rendering into that rect both fall out for free —
/// no separate handling needed elsewhere.
pub fn pane_layout(area: Rect, composer_text: &str, chat_list_collapsed: bool) -> PaneLayout {
    // Outer split: the three panes over a one-row status bar pinned to the bottom.
    let [content_area, status] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(STATUS_HEIGHT)]).areas(area);

    // Content split: chat list | conversation (fills the rest).
    let list_constraint = if chat_list_collapsed {
        Constraint::Length(0)
    } else {
        Constraint::Percentage(CHAT_LIST_PERCENT)
    };
    let [list, convo_area] =
        Layout::horizontal([list_constraint, Constraint::Min(0)]).areas(content_area);

    // Conversation split: message history over the composer, whose height
    // grows with its draft's wrapped row count (1..=MAX_COMPOSER_ROWS) plus
    // its 2-row border (#215) — computed here, synchronously, since the
    // composer's width (== convo_area's width) is already fixed by the
    // horizontal split above, before this vertical split runs.
    let composer_width = convo_area.width.saturating_sub(2).max(1) as usize;
    let composer_rows = crate::wrap::layout_rows(composer_text, composer_width)
        .len()
        .clamp(1, MAX_COMPOSER_ROWS as usize) as u16;
    let composer_height = composer_rows + 2;
    let [history, composer] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(composer_height)])
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
    /// [`convo_body_width`](render::convo_body_width) — the budget message
    /// bodies wrap against.
    pub convo_width: usize,
    /// The pane rectangles this frame was drawn into.
    pub panes: PaneLayout,
    /// Row → chat id map this frame drew.
    pub chat_rows: ChatRows,
    /// Row-range → message id map this frame drew.
    pub history_rows: HistoryRows,
    /// Whether this frame drew at least one avatar or inline-media image in the
    /// history pane (#278) — the Kitty-scroll-ghosting mitigation's signal for
    /// whether a scroll step actually had anything on screen worth protecting
    /// with an extra clear; see `should_clear_for_graphics` in `lib.rs`.
    pub history_has_visible_images: bool,
    /// Row/column → list-index map for the open overlay, if any (empty when none
    /// is open, or the open one has no selectable list).
    pub overlay_rows: OverlayRows,
}

/// Render the whole UI for one frame from the current `App` state, returning what
/// the loop records back onto `App` (see [`RenderOutput`]).
pub fn ui(frame: &mut Frame, app: &App) -> RenderOutput {
    let panes = pane_layout(
        frame.area(),
        app.composer().text(),
        app.chat_list_collapsed(),
    );

    let chat_rows = render_chat_list(frame, panes.list, app);
    let (history_rows, history_has_visible_images) = render_conversation(frame, panes.history, app);
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
        history_has_visible_images,
        overlay_rows,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // tests: panicking on a broken assumption is the point
mod tests {
    use super::*;
    use render::test_support::{app_with_lists, flatten, render, render_output, row_text};

    #[test]
    fn pane_layout_hit_tests_each_pane() {
        // The same 80×24 geometry the skeleton renders at, so this exercises the
        // exact rects `ui` draws into. A point lands in the pane whose rect holds
        // it; the status strip and out-of-bounds are not focus targets (#161).
        let panes = pane_layout(Rect::new(0, 0, 80, 24), "", false);
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
    fn pane_layout_collapses_the_chat_list_to_zero_width() {
        // #213: collapsing zeroes the list column; the conversation side's
        // `Constraint::Min(0)` absorbs the freed width, so history alone now
        // spans the full content width.
        let expanded = pane_layout(Rect::new(0, 0, 80, 24), "", false);
        let collapsed = pane_layout(Rect::new(0, 0, 80, 24), "", true);

        assert_eq!(collapsed.list.width, 0);
        assert_eq!(collapsed.history.x, 0);
        assert_eq!(
            collapsed.history.width,
            expanded.list.width + expanded.history.width
        );
        // Composer/status geometry is untouched by the horizontal split.
        assert_eq!(collapsed.composer.height, expanded.composer.height);
        assert_eq!(collapsed.status, expanded.status);
    }

    #[test]
    fn pane_layout_hit_tests_route_around_the_collapsed_chat_list() {
        // Same point that hit the chat list when expanded (#213) now falls
        // inside history, which has grown to cover that column; nothing
        // resolves to `ChatList` while collapsed.
        let panes = pane_layout(Rect::new(0, 0, 80, 24), "", true);
        assert_eq!(panes.focus_at(1, 1), Some(Focus::History));
        assert_ne!(panes.focus_at(1, 1), Some(Focus::ChatList));
    }

    #[test]
    fn composer_height_grows_with_wrapped_rows_up_to_the_cap() {
        // A drift-guard against `wrap::layout_rows`, mirroring `content_rows`'s
        // own drift-guard tests: each expected row count is computed
        // independently (a hard-broken run of exactly `width` chars per row,
        // per `wrap.rs`'s own tests) rather than re-deriving `pane_layout`'s math.
        let width = pane_layout(Rect::new(0, 0, 80, 24), "", false)
            .composer
            .width as usize
            - 2;
        let row = "x".repeat(width);

        assert_eq!(
            pane_layout(Rect::new(0, 0, 80, 24), "hi", false)
                .composer
                .height,
            3
        );
        assert_eq!(
            pane_layout(Rect::new(0, 0, 80, 24), &row.repeat(3), false)
                .composer
                .height,
            5
        );
        assert_eq!(
            pane_layout(Rect::new(0, 0, 80, 24), &row.repeat(5), false)
                .composer
                .height,
            7
        );
        // Capped at MAX_COMPOSER_ROWS: far beyond it, height stops growing.
        assert_eq!(
            pane_layout(Rect::new(0, 0, 80, 24), &row.repeat(8), false)
                .composer
                .height,
            7
        );
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
        let composer_row = row_text(&buffer, 24 - STATUS_HEIGHT - COMPOSER_HEIGHT);
        assert!(composer_row.contains('●'), "focused composer is marked");
        assert!(
            !row_text(&buffer, 0).contains('●'),
            "unfocused chat list is unmarked"
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

    #[test]
    fn overlay_rows_is_empty_with_no_overlay_open() {
        let output = render_output(&app_with_lists(), 80, 24);
        assert_eq!(output.overlay_rows.index_at(40, 5), None);
    }
}
