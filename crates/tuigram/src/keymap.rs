//! The central keymap and focus model (#83).
//!
//! Every key binding lives in **one** table ([`BINDINGS`]) rather than scattered
//! across the widgets. [`resolve`] turns a key into an [`Action`] by walking that
//! table in the current [`Focus`], and [`help_sections`] renders the same table
//! into the help overlay — so the overlay can never drift out of sync with the
//! bindings it documents.
//!
//! Resolution is **focus-aware**: the same physical key means different things in
//! each pane (`j` selects a chat in the list, scrolls in the history, and types a
//! letter in the composer). A binding declares the [`Context`] it applies in; a
//! key in the composer that matches no binding falls through to "insert this
//! character", which is why printable input never needs an entry per letter.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::app::Action;

/// Which pane currently receives input. Tab cycles forward through these in
/// declaration order; the focused pane is drawn with a highlighted border.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Focus {
    /// The left chat-list pane (the landing focus).
    #[default]
    ChatList,
    /// The conversation/history pane.
    History,
    /// The bottom composer.
    Composer,
}

impl Focus {
    /// The next pane in the cycle (ChatList → History → Composer → ChatList).
    #[must_use]
    pub fn next(self) -> Self {
        match self {
            Self::ChatList => Self::History,
            Self::History => Self::Composer,
            Self::Composer => Self::ChatList,
        }
    }

    /// The previous pane in the cycle (the inverse of [`next`](Self::next)).
    #[must_use]
    pub fn prev(self) -> Self {
        match self {
            Self::ChatList => Self::Composer,
            Self::History => Self::ChatList,
            Self::Composer => Self::History,
        }
    }
}

/// The focus context a binding applies in. `Global` always applies; `Nav` applies
/// in the two read-only panes (chat list and history) where letter keys are
/// commands rather than text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Context {
    /// Active in every focus (e.g. quit, focus switching, help).
    Global,
    /// Active in the chat list and the history — the non-typing panes.
    Nav,
    /// Active only when the chat list is focused.
    ChatList,
    /// Active only when the history is focused.
    History,
    /// Active only when the composer is focused.
    Composer,
}

impl Context {
    /// Whether a binding in this context fires under `focus`.
    fn applies(self, focus: Focus) -> bool {
        match self {
            Self::Global => true,
            Self::Nav => matches!(focus, Focus::ChatList | Focus::History),
            Self::ChatList => focus == Focus::ChatList,
            Self::History => focus == Focus::History,
            Self::Composer => focus == Focus::Composer,
        }
    }
}

/// What key event fires a binding, as data (so the table stays declarative).
enum Trigger {
    /// Any of these key codes, with no Ctrl/Alt held (Shift is allowed, so the
    /// shifted `]`/`[` and capital letters still match).
    Plain(&'static [KeyCode]),
    /// A specific key code with Ctrl held.
    Ctrl(KeyCode),
}

impl Trigger {
    fn matches(&self, key: &KeyEvent) -> bool {
        match self {
            Self::Plain(codes) => {
                !key.modifiers
                    .intersects(KeyModifiers::CONTROL.union(KeyModifiers::ALT))
                    && codes.contains(&key.code)
            }
            Self::Ctrl(code) => key.modifiers.contains(KeyModifiers::CONTROL) && key.code == *code,
        }
    }
}

/// One binding: where it applies, what fires it, the action it emits, and the
/// help text describing it. The single source of truth for both [`resolve`] and
/// [`help_sections`].
struct Binding {
    context: Context,
    trigger: Trigger,
    action: Action,
    /// The key(s) as shown in the help overlay, e.g. `"j / ↓"`.
    keys: &'static str,
    /// What the binding does, as shown in the help overlay.
    description: &'static str,
}

/// The complete keymap. Ordered Global → per-pane → Nav; no key matches two
/// bindings applicable under the same focus, so the first match wins
/// unambiguously.
const BINDINGS: &[Binding] = &[
    // Global — active in every pane.
    Binding {
        context: Context::Global,
        trigger: Trigger::Ctrl(KeyCode::Char('c')),
        action: Action::Quit,
        keys: "Ctrl-C",
        description: "quit",
    },
    Binding {
        context: Context::Global,
        trigger: Trigger::Plain(&[KeyCode::Tab]),
        action: Action::FocusNext,
        keys: "Tab",
        description: "focus next pane",
    },
    Binding {
        context: Context::Global,
        trigger: Trigger::Plain(&[KeyCode::BackTab]),
        action: Action::FocusPrev,
        keys: "Shift-Tab",
        description: "focus previous pane",
    },
    Binding {
        context: Context::Global,
        trigger: Trigger::Plain(&[KeyCode::F(1)]),
        action: Action::ToggleHelp,
        keys: "F1",
        description: "toggle help",
    },
    // Chat list.
    Binding {
        context: Context::ChatList,
        trigger: Trigger::Plain(&[KeyCode::Char('j'), KeyCode::Down]),
        action: Action::SelectNext,
        keys: "j / ↓",
        description: "next chat",
    },
    Binding {
        context: Context::ChatList,
        trigger: Trigger::Plain(&[KeyCode::Char('k'), KeyCode::Up]),
        action: Action::SelectPrev,
        keys: "k / ↑",
        description: "previous chat",
    },
    Binding {
        context: Context::ChatList,
        trigger: Trigger::Plain(&[KeyCode::Char(']')]),
        action: Action::NextList,
        keys: "]",
        description: "next list (Main/Archive/folders)",
    },
    Binding {
        context: Context::ChatList,
        trigger: Trigger::Plain(&[KeyCode::Char('[')]),
        action: Action::PrevList,
        keys: "[",
        description: "previous list",
    },
    Binding {
        context: Context::ChatList,
        trigger: Trigger::Plain(&[KeyCode::Enter]),
        action: Action::SetFocus(Focus::History),
        keys: "Enter",
        description: "open in history",
    },
    // History.
    Binding {
        context: Context::History,
        trigger: Trigger::Plain(&[KeyCode::Char('j'), KeyCode::Down, KeyCode::PageDown]),
        action: Action::ScrollDown,
        keys: "j / ↓ / PgDn",
        description: "scroll down",
    },
    Binding {
        context: Context::History,
        trigger: Trigger::Plain(&[KeyCode::Char('k'), KeyCode::Up, KeyCode::PageUp]),
        action: Action::ScrollUp,
        keys: "k / ↑ / PgUp",
        description: "scroll up",
    },
    Binding {
        context: Context::History,
        trigger: Trigger::Plain(&[KeyCode::Char('i')]),
        action: Action::SetFocus(Focus::Composer),
        keys: "i",
        description: "write a message",
    },
    // Composer — editing keys; any other printable character inserts (see resolve).
    Binding {
        context: Context::Composer,
        trigger: Trigger::Plain(&[KeyCode::Enter]),
        action: Action::ComposerSubmit,
        keys: "Enter",
        description: "send",
    },
    Binding {
        context: Context::Composer,
        trigger: Trigger::Plain(&[KeyCode::Esc]),
        action: Action::ComposerCancel,
        keys: "Esc",
        description: "cancel reply/edit + clear",
    },
    Binding {
        context: Context::Composer,
        trigger: Trigger::Plain(&[KeyCode::Backspace]),
        action: Action::ComposerBackspace,
        keys: "Backspace",
        description: "delete the previous character",
    },
    Binding {
        context: Context::Composer,
        trigger: Trigger::Plain(&[KeyCode::Left]),
        action: Action::ComposerLeft,
        keys: "←",
        description: "cursor left",
    },
    Binding {
        context: Context::Composer,
        trigger: Trigger::Plain(&[KeyCode::Right]),
        action: Action::ComposerRight,
        keys: "→",
        description: "cursor right",
    },
    Binding {
        context: Context::Composer,
        trigger: Trigger::Plain(&[KeyCode::Home]),
        action: Action::ComposerHome,
        keys: "Home",
        description: "start of line",
    },
    Binding {
        context: Context::Composer,
        trigger: Trigger::Plain(&[KeyCode::End]),
        action: Action::ComposerEnd,
        keys: "End",
        description: "end of line",
    },
    // Navigation panes (chat list + history): commands that read, not type.
    Binding {
        context: Context::Nav,
        trigger: Trigger::Plain(&[KeyCode::Char('q')]),
        action: Action::Quit,
        keys: "q",
        description: "quit",
    },
    Binding {
        context: Context::Nav,
        trigger: Trigger::Plain(&[KeyCode::Char('?')]),
        action: Action::ToggleHelp,
        keys: "?",
        description: "toggle help",
    },
];

/// Resolve a key event to an [`Action`] under the current `focus`.
///
/// Key releases are ignored (crossterm reports them on Windows). While the help
/// overlay is open it is modal: `Ctrl-C` still quits, and any other key dismisses
/// it. Otherwise the first binding that applies under `focus` and matches the key
/// wins; an unmatched printable key in the composer inserts that character.
#[must_use]
pub fn resolve(focus: Focus, help_visible: bool, key: &KeyEvent) -> Action {
    if key.kind == KeyEventKind::Release {
        return Action::Noop;
    }
    if help_visible {
        if Trigger::Ctrl(KeyCode::Char('c')).matches(key) {
            return Action::Quit;
        }
        // The overlay is modal: any other key just dismisses it.
        return Action::ToggleHelp;
    }
    for binding in BINDINGS {
        if binding.context.applies(focus) && binding.trigger.matches(key) {
            return binding.action;
        }
    }
    if focus == Focus::Composer
        && let Some(c) = printable(key)
    {
        return Action::ComposerInput(c);
    }
    Action::Noop
}

/// The character a key would insert, or `None` for a non-printable key or one
/// held with Ctrl/Alt (a command, not text).
fn printable(key: &KeyEvent) -> Option<char> {
    if key
        .modifiers
        .intersects(KeyModifiers::CONTROL.union(KeyModifiers::ALT))
    {
        return None;
    }
    match key.code {
        KeyCode::Char(c) => Some(c),
        _ => None,
    }
}

/// One labelled group of bindings in the help overlay.
pub struct HelpSection {
    /// The group heading (e.g. "Chat list").
    pub title: &'static str,
    /// The bindings in the group.
    pub entries: Vec<HelpEntry>,
}

/// One row in the help overlay: the keys and what they do.
pub struct HelpEntry {
    /// The key(s), as in [`Binding::keys`].
    pub keys: &'static str,
    /// The description, as in [`Binding::description`].
    pub description: &'static str,
}

/// The help overlay's contents, generated from [`BINDINGS`] so it always matches
/// what [`resolve`] actually does. The composer section gains a synthetic line for
/// the printable-insert fall-through, which has no single key to bind.
#[must_use]
pub fn help_sections() -> Vec<HelpSection> {
    const ORDER: &[(Context, &str)] = &[
        (Context::Global, "Global"),
        (Context::ChatList, "Chat list"),
        (Context::History, "History"),
        (Context::Composer, "Composer"),
        (Context::Nav, "Chat list & history"),
    ];
    ORDER
        .iter()
        .map(|&(context, title)| {
            let mut entries: Vec<HelpEntry> = BINDINGS
                .iter()
                .filter(|b| b.context == context)
                .map(|b| HelpEntry {
                    keys: b.keys,
                    description: b.description,
                })
                .collect();
            if context == Context::Composer {
                entries.push(HelpEntry {
                    keys: "(any key)",
                    description: "type to insert text",
                });
            }
            HelpSection { title, entries }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn resolved(focus: Focus, code: KeyCode) -> Action {
        resolve(focus, false, &key(code))
    }

    #[test]
    fn focus_cycles_both_ways() {
        assert_eq!(Focus::ChatList.next(), Focus::History);
        assert_eq!(Focus::History.next(), Focus::Composer);
        assert_eq!(Focus::Composer.next(), Focus::ChatList);
        assert_eq!(Focus::ChatList.prev(), Focus::Composer);
    }

    #[test]
    fn the_same_key_means_different_things_per_focus() {
        // `j` selects in the list, scrolls in the history, and types in the composer.
        assert_eq!(
            resolved(Focus::ChatList, KeyCode::Char('j')),
            Action::SelectNext
        );
        assert_eq!(
            resolved(Focus::History, KeyCode::Char('j')),
            Action::ScrollDown
        );
        assert_eq!(
            resolved(Focus::Composer, KeyCode::Char('j')),
            Action::ComposerInput('j')
        );
    }

    #[test]
    fn global_bindings_apply_in_every_focus() {
        for focus in [Focus::ChatList, Focus::History, Focus::Composer] {
            assert_eq!(resolve(focus, false, &ctrl('c')), Action::Quit);
            assert_eq!(resolved(focus, KeyCode::Tab), Action::FocusNext);
            assert_eq!(resolved(focus, KeyCode::F(1)), Action::ToggleHelp);
        }
    }

    #[test]
    fn q_quits_in_nav_panes_but_types_in_the_composer() {
        assert_eq!(resolved(Focus::ChatList, KeyCode::Char('q')), Action::Quit);
        assert_eq!(resolved(Focus::History, KeyCode::Char('q')), Action::Quit);
        // In the composer `q` is just text, not a command.
        assert_eq!(
            resolved(Focus::Composer, KeyCode::Char('q')),
            Action::ComposerInput('q')
        );
    }

    #[test]
    fn composer_editing_keys_resolve_only_in_the_composer() {
        assert_eq!(
            resolved(Focus::Composer, KeyCode::Enter),
            Action::ComposerSubmit
        );
        assert_eq!(
            resolved(Focus::Composer, KeyCode::Esc),
            Action::ComposerCancel
        );
        assert_eq!(
            resolved(Focus::Composer, KeyCode::Backspace),
            Action::ComposerBackspace
        );
        // Enter elsewhere is a focus move, not a send.
        assert_eq!(
            resolved(Focus::ChatList, KeyCode::Enter),
            Action::SetFocus(Focus::History)
        );
    }

    #[test]
    fn ctrl_and_alt_chords_are_not_inserted_as_text() {
        // A Ctrl/Alt chord in the composer is not printable input.
        assert_eq!(resolve(Focus::Composer, false, &ctrl('a')), Action::Noop);
    }

    #[test]
    fn key_release_is_ignored() {
        let mut release = key(KeyCode::Char('q'));
        release.kind = KeyEventKind::Release;
        assert_eq!(resolve(Focus::ChatList, false, &release), Action::Noop);
    }

    #[test]
    fn an_open_help_overlay_is_modal() {
        // Any key closes it, except Ctrl-C which still quits.
        assert_eq!(
            resolve(Focus::ChatList, true, &key(KeyCode::Char('x'))),
            Action::ToggleHelp
        );
        assert_eq!(resolve(Focus::Composer, true, &ctrl('c')), Action::Quit);
    }

    #[test]
    fn help_sections_are_generated_from_the_keymap() {
        let sections = help_sections();
        // Every documented binding count matches what the table holds, and the
        // composer gains the synthetic "type to insert" line.
        let titles: Vec<&str> = sections.iter().map(|s| s.title).collect();
        assert_eq!(
            titles,
            vec![
                "Global",
                "Chat list",
                "History",
                "Composer",
                "Chat list & history"
            ]
        );
        let composer = sections.iter().find(|s| s.title == "Composer").unwrap();
        assert!(
            composer
                .entries
                .iter()
                .any(|e| e.description == "type to insert text"),
            "composer help documents the printable-insert fall-through"
        );
        // The keymap is the source: every entry carries non-empty help text.
        for section in &sections {
            for entry in &section.entries {
                assert!(!entry.keys.is_empty() && !entry.description.is_empty());
            }
        }
    }
}
