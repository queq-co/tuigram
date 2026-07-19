//! Modal overlays (#84/#85/#87/#146/#195/#197/#209): search, contact search,
//! forward, reaction, send-media, settings, secret-chat, delete/logout
//! confirm, and the shared list-modal/field-line helpers they build on.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph};

use crate::app::App;
use crate::mediaform::MediaField;
use crate::settingsform::SettingsField;
use crate::ui::OverlayRows;

use super::chat_list::chat_item;
use super::common::{SELECTED_SYMBOL, centered_rect, hint_line, input_line, truncate};

/// Width of the search/forward modal popups, clamped to the terminal by
/// [`centered_rect`].
const OVERLAY_WIDTH: u16 = 56;

/// The search query line (#84): a centred modal with the editable query over a key
/// hint. The query reuses the composer's [`input_line`] so the cursor renders
/// identically; an empty query shows a dim prompt instead.
pub(crate) fn render_search_input(frame: &mut Frame, area: Rect, app: &App) {
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
pub(crate) fn render_search_results(frame: &mut Frame, area: Rect, app: &App) -> OverlayRows {
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
pub(crate) fn render_contact_search_input(frame: &mut Frame, area: Rect, app: &App) {
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
pub(crate) fn render_contact_search_results(
    frame: &mut Frame,
    area: Rect,
    app: &App,
) -> OverlayRows {
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
pub(crate) fn render_forward(frame: &mut Frame, area: Rect, app: &App) -> OverlayRows {
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
pub(crate) fn render_reaction(frame: &mut Frame, area: Rect, app: &App) -> OverlayRows {
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
pub(crate) fn render_send_media(frame: &mut Frame, area: Rect, app: &App) {
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
pub(crate) fn render_settings(frame: &mut Frame, area: Rect, app: &App) {
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
pub(crate) fn render_secret_chat(frame: &mut Frame, area: Rect, app: &App) {
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
pub(crate) fn render_delete_confirm(frame: &mut Frame, area: Rect, app: &App) {
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
pub(crate) fn render_logout_confirm(frame: &mut Frame, area: Rect, _app: &App) {
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

#[cfg(test)]
#[allow(clippy::unwrap_used)] // tests: panicking on a broken assumption is the point
mod tests {
    use tuigram_core::model::ChatKind;

    use crate::app::Action;
    use crate::keymap::Overlay;
    use crate::search::SearchHit;

    use super::super::test_support::{
        app_with_history, app_with_lists, flatten, render, render_output, row_containing,
        text_message, view_with_one_chat,
    };
    use super::*;

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
}
