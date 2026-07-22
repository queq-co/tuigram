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
    /// The next pane in the cycle (`ChatList` → History → Composer → `ChatList`).
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

/// A modal layer drawn over the three panes. While one is active it **captures**
/// input — key resolution routes to that overlay instead of the focused pane — so
/// at most one is open at a time. [`None`](Self::None) is the normal browsing
/// state, where [`Focus`] drives resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Overlay {
    /// No overlay: normal three-pane browsing, resolved against [`Focus`].
    #[default]
    None,
    /// The help cheatsheet (#83): modal and scrollable, closed only by `?`/`q`/`Esc`.
    Help,
    /// The search query line: typing builds the query, Enter runs it.
    SearchInput,
    /// The search results list: navigate hits, forward one, or close.
    SearchResults,
    /// The forward target picker: choose a destination chat and confirm.
    Forward,
    /// The reaction picker: choose an emoji to toggle on the selected message.
    Reaction,
    /// The send-media prompt: type a local path and an optional caption.
    SendMedia,
    /// The secret-chat lifecycle confirm: start or close a secret chat (#87).
    SecretChat,
    /// The contact-search query line (#197): typing builds the query, Enter runs
    /// it against `search_contacts`.
    ContactSearchInput,
    /// The contact-search results list (#197): navigate hits, Enter opens the
    /// secret-chat confirm for the selected contact, or close.
    ContactSearchResults,
    /// The settings editor (#146): edit the four download-cache knobs plus the
    /// graphics toggle (#209), applied live and written back to `settings.toml`
    /// on confirm.
    Settings,
    /// The delete-message confirm (#195): pick the scope (for me / for everyone)
    /// and confirm, or cancel. Gates the destructive delete behind an explicit step.
    DeleteConfirm,
    /// The logout confirm (#195): confirm ends the session and wipes the local
    /// data, or cancel. Gates the destructive logout behind an explicit step.
    LogoutConfirm,
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
    /// A specific key code with Alt held (and not Ctrl) — e.g. Alt-Enter to
    /// insert a composer newline (#215) without also matching a Ctrl chord.
    Alt(KeyCode),
    /// A specific key code with Shift held — best-effort: crossterm only
    /// reports Shift on a key like Enter when the terminal supports and the
    /// app has enabled the Kitty keyboard-enhancement protocol, which this
    /// crate does not enable, so this fires only on terminals that report it
    /// unprompted (#215).
    Shift(KeyCode),
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
            Self::Alt(code) => {
                key.modifiers.contains(KeyModifiers::ALT)
                    && !key.modifiers.contains(KeyModifiers::CONTROL)
                    && key.code == *code
            }
            Self::Shift(code) => key.modifiers.contains(KeyModifiers::SHIFT) && key.code == *code,
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
    Binding {
        context: Context::Global,
        trigger: Trigger::Ctrl(KeyCode::Char('g')),
        action: Action::NoticeDismiss,
        keys: "Ctrl-G",
        description: "dismiss notification",
    },
    Binding {
        context: Context::Global,
        trigger: Trigger::Ctrl(KeyCode::Char('r')),
        action: Action::Resync,
        keys: "Ctrl-R",
        description: "resync after a dropped-update gap",
    },
    Binding {
        context: Context::Global,
        trigger: Trigger::Ctrl(KeyCode::Char('q')),
        action: Action::LogoutOpen,
        keys: "Ctrl-Q",
        description: "log out (confirm first)",
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
    Binding {
        context: Context::ChatList,
        trigger: Trigger::Plain(&[KeyCode::Char('s')]),
        action: Action::SecretOpen,
        keys: "s",
        description: "secret chat: start / close",
    },
    Binding {
        context: Context::ChatList,
        trigger: Trigger::Plain(&[KeyCode::Char('n')]),
        action: Action::ContactSearchOpen,
        keys: "n",
        description: "new secret chat with a contact (search by name)",
    },
    // History.
    Binding {
        context: Context::History,
        trigger: Trigger::Plain(&[KeyCode::Char('j'), KeyCode::Down]),
        action: Action::ScrollDown,
        keys: "j / ↓",
        description: "scroll down one row",
    },
    Binding {
        context: Context::History,
        trigger: Trigger::Plain(&[KeyCode::Char('k'), KeyCode::Up]),
        action: Action::ScrollUp,
        keys: "k / ↑",
        description: "scroll up one row",
    },
    Binding {
        context: Context::History,
        trigger: Trigger::Plain(&[KeyCode::PageDown]),
        action: Action::PageDown,
        keys: "PgDn",
        description: "scroll down a full page",
    },
    Binding {
        context: Context::History,
        trigger: Trigger::Plain(&[KeyCode::PageUp]),
        action: Action::PageUp,
        keys: "PgUp",
        description: "scroll up a full page",
    },
    Binding {
        context: Context::History,
        trigger: Trigger::Plain(&[KeyCode::Char('G'), KeyCode::End]),
        action: Action::JumpToNewest,
        keys: "G / End",
        description: "jump to the newest message (top of the last screenful)",
    },
    Binding {
        context: Context::History,
        trigger: Trigger::Plain(&[KeyCode::Char('i')]),
        action: Action::SetFocus(Focus::Composer),
        keys: "i",
        description: "write a message",
    },
    Binding {
        context: Context::History,
        trigger: Trigger::Plain(&[KeyCode::Char('r')]),
        action: Action::ReplyMessage,
        keys: "r",
        description: "reply to the selected message",
    },
    Binding {
        context: Context::History,
        trigger: Trigger::Plain(&[KeyCode::Char('e')]),
        action: Action::EditMessage,
        keys: "e",
        description: "edit the selected message (your own)",
    },
    Binding {
        context: Context::History,
        trigger: Trigger::Plain(&[KeyCode::Char('g')]),
        action: Action::JumpToQuoted,
        keys: "g",
        description: "jump to the message the selected reply quotes",
    },
    Binding {
        context: Context::History,
        trigger: Trigger::Plain(&[KeyCode::Char('d')]),
        action: Action::DeleteMessage,
        keys: "d",
        description: "delete the selected message",
    },
    Binding {
        context: Context::History,
        trigger: Trigger::Plain(&[KeyCode::Char('R')]),
        action: Action::ReactionOpen,
        keys: "R",
        description: "react to the selected message",
    },
    Binding {
        context: Context::History,
        trigger: Trigger::Plain(&[KeyCode::Char('p')]),
        action: Action::PinToggle,
        keys: "p",
        description: "pin / unpin the selected message",
    },
    Binding {
        context: Context::History,
        trigger: Trigger::Plain(&[KeyCode::Char('f')]),
        action: Action::ForwardMessage,
        keys: "f",
        description: "forward the selected message",
    },
    Binding {
        context: Context::History,
        trigger: Trigger::Plain(&[KeyCode::Char('a')]),
        action: Action::AttachOpen,
        keys: "a",
        description: "attach / send media",
    },
    Binding {
        context: Context::History,
        trigger: Trigger::Plain(&[KeyCode::Char('S')]),
        action: Action::SaveMedia,
        keys: "S",
        description: "save / download the selected message's media",
    },
    Binding {
        context: Context::History,
        trigger: Trigger::Plain(&[KeyCode::Char('y')]),
        action: Action::CopyMessage,
        keys: "y",
        description: "copy the selected message's text",
    },
    // Composer — editing keys; any other printable character inserts (see resolve).
    //
    // Alt/Shift-Enter must be checked *before* the plain Enter binding below:
    // `Trigger::Plain` is deliberately Shift-agnostic (so shifted glyphs and
    // capital letters still match plain bindings elsewhere), which means a
    // Shift-Enter also satisfies `Trigger::Plain(&[KeyCode::Enter])` — were
    // that binding listed first, `resolve_panes`'s first-match walk would
    // resolve Shift-Enter to `ComposerSubmit` and this line-break binding
    // would never fire (#215).
    Binding {
        context: Context::Composer,
        trigger: Trigger::Alt(KeyCode::Enter),
        action: Action::ComposerNewline,
        keys: "Alt-Enter",
        description: "insert a line break",
    },
    Binding {
        context: Context::Composer,
        trigger: Trigger::Shift(KeyCode::Enter),
        action: Action::ComposerNewline,
        keys: "Shift-Enter",
        description: "insert a line break (where the terminal reports it)",
    },
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
    Binding {
        context: Context::Composer,
        trigger: Trigger::Plain(&[KeyCode::Up]),
        action: Action::ComposerUp,
        keys: "↑",
        description: "cursor up a wrapped row",
    },
    Binding {
        context: Context::Composer,
        trigger: Trigger::Plain(&[KeyCode::Down]),
        action: Action::ComposerDown,
        keys: "↓",
        description: "cursor down a wrapped row",
    },
    // Navigation panes (chat list + history): commands that read, not type.
    Binding {
        context: Context::Nav,
        trigger: Trigger::Plain(&[KeyCode::Char('/')]),
        action: Action::SearchOpen,
        keys: "/",
        description: "search messages",
    },
    Binding {
        context: Context::Nav,
        trigger: Trigger::Plain(&[KeyCode::Char(',')]),
        action: Action::SettingsOpen,
        keys: ",",
        description: "settings (cache retention, graphics)",
    },
    Binding {
        context: Context::Nav,
        trigger: Trigger::Plain(&[KeyCode::Char('b')]),
        action: Action::ToggleChatListCollapse,
        keys: "b",
        description: "collapse / expand the chat-list pane",
    },
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

/// Resolve a key event to an [`Action`] under the current `focus` and `overlay`.
///
/// Key releases are ignored (crossterm reports them on Windows). An open `overlay`
/// **captures** input: the key is resolved against that overlay's own keys, not the
/// focused pane. With no overlay, the first [`BINDINGS`] entry that applies under
/// `focus` and matches the key wins; an unmatched printable key in the composer
/// inserts that character.
///
/// `Ctrl-C` quits from every overlay, so the app is never trapped in a modal.
/// `Ctrl-G` dismisses the showing toast from everywhere too (#139): it is handled
/// centrally, before the overlay match, rather than re-admitted per overlay like the
/// `Ctrl-C` quit — a toast can surface while any overlay is open, so its dismiss must
/// not depend on each resolver remembering to allow it.
#[must_use]
pub fn resolve(focus: Focus, overlay: Overlay, key: &KeyEvent) -> Action {
    if key.kind == KeyEventKind::Release {
        return Action::Noop;
    }
    // The toast-dismiss chord is always available, even inside a capturing overlay;
    // it is a Ctrl chord, so it never collides with an overlay's typed input.
    if is_dismiss(key) {
        return Action::NoticeDismiss;
    }
    match overlay {
        Overlay::None => resolve_panes(focus, key),
        Overlay::Help => resolve_help(key),
        Overlay::SearchInput => resolve_search_input(key),
        Overlay::SearchResults => resolve_search_results(key),
        Overlay::Forward => resolve_forward(key),
        Overlay::Reaction => resolve_reaction(key),
        Overlay::SendMedia => resolve_send_media(key),
        Overlay::SecretChat => resolve_secret_chat(key),
        Overlay::ContactSearchInput => resolve_contact_search_input(key),
        Overlay::ContactSearchResults => resolve_contact_search_results(key),
        Overlay::Settings => resolve_settings(key),
        Overlay::DeleteConfirm => resolve_delete_confirm(key),
        Overlay::LogoutConfirm => resolve_logout_confirm(key),
    }
}

/// Whether the key is `Ctrl-C` — the always-available quit, even inside a modal.
fn is_quit(key: &KeyEvent) -> bool {
    Trigger::Ctrl(KeyCode::Char('c')).matches(key)
}

/// Whether the key is `Ctrl-G` — the always-available toast dismiss (#139), even
/// inside a capturing overlay.
fn is_dismiss(key: &KeyEvent) -> bool {
    Trigger::Ctrl(KeyCode::Char('g')).matches(key)
}

/// Normal browsing: walk the keymap under `focus`, then fall through to composer
/// text insertion for an unmatched printable key.
fn resolve_panes(focus: Focus, key: &KeyEvent) -> Action {
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

/// The help overlay (#83): scrollable and explicitly closed. `j`/`k` (and the
/// arrows) scroll the cheatsheet, which can run taller than a short terminal; `?`,
/// `q`, and `Esc` close it, and `Ctrl-C` still quits. Every other key is ignored, so
/// a stray press no longer dismisses a half-read page.
fn resolve_help(key: &KeyEvent) -> Action {
    if is_quit(key) {
        return Action::Quit;
    }
    match key.code {
        KeyCode::Char('?' | 'q') | KeyCode::Esc => Action::ToggleHelp,
        KeyCode::Char('j') | KeyCode::Down => Action::HelpScrollDown,
        KeyCode::Char('k') | KeyCode::Up => Action::HelpScrollUp,
        _ => Action::Noop,
    }
}

/// The search query line: typing edits the query, Enter runs it, Esc cancels.
fn resolve_search_input(key: &KeyEvent) -> Action {
    if is_quit(key) {
        return Action::Quit;
    }
    match key.code {
        KeyCode::Esc => Action::SearchCancel,
        KeyCode::Enter => Action::SearchSubmit,
        KeyCode::Backspace => Action::SearchBackspace,
        KeyCode::Left => Action::SearchLeft,
        KeyCode::Right => Action::SearchRight,
        KeyCode::Home => Action::SearchHome,
        KeyCode::End => Action::SearchEnd,
        _ => match printable(key) {
            Some(c) => Action::SearchInput(c),
            None => Action::Noop,
        },
    }
}

/// The search results list: navigate hits, open the selected one, forward it, or
/// close.
fn resolve_search_results(key: &KeyEvent) -> Action {
    if is_quit(key) {
        return Action::Quit;
    }
    match key.code {
        KeyCode::Esc => Action::SearchCancel,
        KeyCode::Char('j') | KeyCode::Down => Action::ResultNext,
        KeyCode::Char('k') | KeyCode::Up => Action::ResultPrev,
        KeyCode::Enter => Action::ResultOpen,
        KeyCode::Char('f') => Action::ForwardOpen,
        _ => Action::Noop,
    }
}

/// The forward target picker: navigate target chats, Enter confirms, Esc cancels.
fn resolve_forward(key: &KeyEvent) -> Action {
    if is_quit(key) {
        return Action::Quit;
    }
    match key.code {
        KeyCode::Esc => Action::ForwardCancel,
        KeyCode::Char('j') | KeyCode::Down => Action::ForwardNext,
        KeyCode::Char('k') | KeyCode::Up => Action::ForwardPrev,
        KeyCode::Enter => Action::ForwardConfirm,
        _ => Action::Noop,
    }
}

/// The reaction picker: arrow keys navigate the palette, Enter toggles the chosen
/// reaction on the selected message, Esc cancels. Character keys become
/// [`Action::ReactionKey`] and Backspace [`Action::ReactionBackspace`] — the reducer
/// interprets them by the picker's mode (a palette shortcut like `j`/`k`/`c`, or
/// input for the custom-emoji line), so the keymap stays a pure function of the key.
fn resolve_reaction(key: &KeyEvent) -> Action {
    if is_quit(key) {
        return Action::Quit;
    }
    match key.code {
        KeyCode::Esc => Action::ReactionCancel,
        KeyCode::Enter => Action::ReactionConfirm,
        KeyCode::Backspace => Action::ReactionBackspace,
        KeyCode::Down => Action::ReactionNext,
        KeyCode::Up => Action::ReactionPrev,
        _ => match printable(key) {
            Some(c) => Action::ReactionKey(c),
            None => Action::Noop,
        },
    }
}

/// The send-media prompt: typing edits the focused field, Tab switches fields,
/// Enter confirms the send, Esc cancels.
fn resolve_send_media(key: &KeyEvent) -> Action {
    if is_quit(key) {
        return Action::Quit;
    }
    match key.code {
        KeyCode::Esc => Action::AttachCancel,
        KeyCode::Enter => Action::AttachConfirm,
        KeyCode::Tab => Action::AttachToggleField,
        KeyCode::Backspace => Action::AttachBackspace,
        KeyCode::Left => Action::AttachLeft,
        KeyCode::Right => Action::AttachRight,
        KeyCode::Home => Action::AttachHome,
        KeyCode::End => Action::AttachEnd,
        _ => match printable(key) {
            Some(c) => Action::AttachInput(c),
            None => Action::Noop,
        },
    }
}

/// The settings editor (#146, plus the graphics toggle, #209): typing edits the
/// focused knob, Tab moves between the five fields, Enter validates and saves (a
/// bad value is rejected in place, keeping the overlay open), Esc cancels.
/// Mirrors the send-media prompt's multi-field editing.
fn resolve_settings(key: &KeyEvent) -> Action {
    if is_quit(key) {
        return Action::Quit;
    }
    match key.code {
        KeyCode::Esc => Action::SettingsCancel,
        KeyCode::Enter => Action::SettingsConfirm,
        KeyCode::Tab => Action::SettingsToggleField,
        KeyCode::BackTab => Action::SettingsToggleFieldPrev,
        KeyCode::Backspace => Action::SettingsBackspace,
        KeyCode::Left => Action::SettingsLeft,
        KeyCode::Right => Action::SettingsRight,
        KeyCode::Home => Action::SettingsHome,
        KeyCode::End => Action::SettingsEnd,
        _ => match printable(key) {
            Some(c) => Action::SettingsInput(c),
            None => Action::Noop,
        },
    }
}

/// The secret-chat lifecycle confirm (#87): Enter runs the start/close, Esc cancels.
fn resolve_secret_chat(key: &KeyEvent) -> Action {
    if is_quit(key) {
        return Action::Quit;
    }
    match key.code {
        KeyCode::Esc => Action::SecretCancel,
        KeyCode::Enter => Action::SecretConfirm,
        _ => Action::Noop,
    }
}

/// The contact-search query line (#197): typing edits the query, Enter runs it,
/// Esc cancels. Mirrors [`resolve_search_input`].
fn resolve_contact_search_input(key: &KeyEvent) -> Action {
    if is_quit(key) {
        return Action::Quit;
    }
    match key.code {
        KeyCode::Esc => Action::ContactSearchCancel,
        KeyCode::Enter => Action::ContactSearchSubmit,
        KeyCode::Backspace => Action::ContactSearchBackspace,
        KeyCode::Left => Action::ContactSearchLeft,
        KeyCode::Right => Action::ContactSearchRight,
        KeyCode::Home => Action::ContactSearchHome,
        KeyCode::End => Action::ContactSearchEnd,
        _ => match printable(key) {
            Some(c) => Action::ContactSearchInput(c),
            None => Action::Noop,
        },
    }
}

/// The contact-search results list (#197): navigate hits, Enter opens the
/// secret-chat confirm for the selected one, Esc closes. Mirrors
/// [`resolve_search_results`] minus the forward shortcut (not meaningful here).
fn resolve_contact_search_results(key: &KeyEvent) -> Action {
    if is_quit(key) {
        return Action::Quit;
    }
    match key.code {
        KeyCode::Esc => Action::ContactSearchCancel,
        KeyCode::Char('j') | KeyCode::Down => Action::ContactResultNext,
        KeyCode::Char('k') | KeyCode::Up => Action::ContactResultPrev,
        KeyCode::Enter => Action::ContactResultConfirm,
        _ => Action::Noop,
    }
}

/// The delete-message confirm (#195): Enter runs the delete at the chosen scope,
/// Tab flips between "for me" and "for everyone", Esc cancels. Ctrl-C still quits.
fn resolve_delete_confirm(key: &KeyEvent) -> Action {
    if is_quit(key) {
        return Action::Quit;
    }
    match key.code {
        KeyCode::Esc => Action::DeleteCancel,
        KeyCode::Enter => Action::DeleteConfirm,
        KeyCode::Tab => Action::DeleteToggleScope,
        _ => Action::Noop,
    }
}

/// The logout confirm (#195): Enter ends the session and wipes local data, Esc
/// cancels. A deliberately spare confirm — a stray key does nothing — since the
/// action is destructive. Ctrl-C still quits the app (without logging out).
fn resolve_logout_confirm(key: &KeyEvent) -> Action {
    if is_quit(key) {
        return Action::Quit;
    }
    match key.code {
        KeyCode::Esc => Action::LogoutCancel,
        KeyCode::Enter => Action::LogoutConfirm,
        _ => Action::Noop,
    }
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

/// The number of lines the help overlay renders, mirroring the layout `render_help`
/// builds from [`help_sections`]: a heading plus one line per entry for each section,
/// with a blank separator between sections. The help scroll offset clamps against
/// this so a scroll can never run past the last line.
#[must_use]
pub fn help_line_count() -> usize {
    help_sections()
        .iter()
        .enumerate()
        .map(|(i, section)| usize::from(i > 0) + 1 + section.entries.len())
        .sum()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // tests: panicking on a broken assumption is the point
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn resolved(focus: Focus, code: KeyCode) -> Action {
        resolve(focus, Overlay::None, &key(code))
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
            assert_eq!(resolve(focus, Overlay::None, &ctrl('c')), Action::Quit);
            assert_eq!(resolved(focus, KeyCode::Tab), Action::FocusNext);
            assert_eq!(resolved(focus, KeyCode::F(1)), Action::ToggleHelp);
            // Ctrl-G dismisses a toast everywhere — even in the composer, where a
            // plain letter would type (it is a Ctrl chord, so it never inserts).
            assert_eq!(
                resolve(focus, Overlay::None, &ctrl('g')),
                Action::NoticeDismiss
            );
        }
    }

    #[test]
    fn ctrl_g_dismisses_a_toast_from_inside_every_overlay() {
        // A toast can surface while any overlay is open, so its dismiss must reach
        // through the capture — unlike other globals, which an overlay swallows (#139).
        for overlay in [
            Overlay::Help,
            Overlay::SearchInput,
            Overlay::SearchResults,
            Overlay::Forward,
            Overlay::Reaction,
            Overlay::SendMedia,
            Overlay::SecretChat,
            Overlay::Settings,
            Overlay::DeleteConfirm,
            Overlay::LogoutConfirm,
        ] {
            assert_eq!(
                resolve(Focus::History, overlay, &ctrl('g')),
                Action::NoticeDismiss,
                "Ctrl-G must dismiss from {overlay:?}"
            );
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
    fn comma_opens_settings_in_nav_panes_but_types_in_the_composer() {
        // The settings binding lives in Nav, so it opens from the browsing panes but
        // never steals the comma key from a message being composed (#146).
        assert_eq!(
            resolved(Focus::ChatList, KeyCode::Char(',')),
            Action::SettingsOpen
        );
        assert_eq!(
            resolved(Focus::History, KeyCode::Char(',')),
            Action::SettingsOpen
        );
        assert_eq!(
            resolved(Focus::Composer, KeyCode::Char(',')),
            Action::ComposerInput(',')
        );
    }

    #[test]
    fn b_toggles_the_chat_list_collapse_in_nav_panes_but_types_in_the_composer() {
        // Nav, like `,`/`/`/`q`: works from either browsing pane, never steals
        // the letter from a message being composed (#213).
        assert_eq!(
            resolved(Focus::ChatList, KeyCode::Char('b')),
            Action::ToggleChatListCollapse
        );
        assert_eq!(
            resolved(Focus::History, KeyCode::Char('b')),
            Action::ToggleChatListCollapse
        );
        assert_eq!(
            resolved(Focus::Composer, KeyCode::Char('b')),
            Action::ComposerInput('b')
        );
    }

    #[test]
    fn the_settings_overlay_captures_editing_keys() {
        // While open, the editor captures typing, Tab field-switch, confirm/cancel —
        // resolved against the overlay, not the pane behind it (#146).
        let at = |code| resolve(Focus::History, Overlay::Settings, &key(code));
        assert_eq!(at(KeyCode::Esc), Action::SettingsCancel);
        assert_eq!(at(KeyCode::Enter), Action::SettingsConfirm);
        assert_eq!(at(KeyCode::Tab), Action::SettingsToggleField);
        assert_eq!(at(KeyCode::Backspace), Action::SettingsBackspace);
        assert_eq!(at(KeyCode::Char('3')), Action::SettingsInput('3'));
        // Ctrl-C still escapes the modal.
        assert_eq!(
            resolve(Focus::History, Overlay::Settings, &ctrl('c')),
            Action::Quit
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
        assert_eq!(
            resolve(Focus::Composer, Overlay::None, &ctrl('a')),
            Action::Noop
        );
    }

    #[test]
    fn key_release_is_ignored() {
        let mut release = key(KeyCode::Char('q'));
        release.kind = KeyEventKind::Release;
        assert_eq!(
            resolve(Focus::ChatList, Overlay::None, &release),
            Action::Noop
        );
    }

    #[test]
    fn an_open_help_overlay_scrolls_and_closes_explicitly() {
        // Focus is irrelevant while a modal captures input. `j`/`k` (and the arrows)
        // scroll rather than dismiss; only `?`/`q`/`Esc` close it, and Ctrl-C quits.
        assert_eq!(
            resolve(Focus::ChatList, Overlay::Help, &key(KeyCode::Char('j'))),
            Action::HelpScrollDown
        );
        assert_eq!(
            resolve(Focus::ChatList, Overlay::Help, &key(KeyCode::Up)),
            Action::HelpScrollUp
        );
        for close in [KeyCode::Char('?'), KeyCode::Char('q'), KeyCode::Esc] {
            assert_eq!(
                resolve(Focus::ChatList, Overlay::Help, &key(close)),
                Action::ToggleHelp,
                "{close:?} closes the help overlay"
            );
        }
        // A stray key no longer dismisses a half-read page.
        assert_eq!(
            resolve(Focus::ChatList, Overlay::Help, &key(KeyCode::Char('x'))),
            Action::Noop
        );
        assert_eq!(
            resolve(Focus::Composer, Overlay::Help, &ctrl('c')),
            Action::Quit
        );
    }

    #[test]
    fn slash_opens_search_from_a_nav_pane_but_types_in_the_composer() {
        assert_eq!(
            resolved(Focus::ChatList, KeyCode::Char('/')),
            Action::SearchOpen
        );
        assert_eq!(
            resolved(Focus::History, KeyCode::Char('/')),
            Action::SearchOpen
        );
        // In the composer `/` is just text.
        assert_eq!(
            resolved(Focus::Composer, KeyCode::Char('/')),
            Action::ComposerInput('/')
        );
    }

    #[test]
    fn the_search_input_overlay_edits_the_query_and_runs_on_enter() {
        let at = |code| resolve(Focus::ChatList, Overlay::SearchInput, &key(code));
        // Focus is ignored — the overlay owns the keys. Printables build the query.
        assert_eq!(at(KeyCode::Char('k')), Action::SearchInput('k'));
        assert_eq!(at(KeyCode::Backspace), Action::SearchBackspace);
        assert_eq!(at(KeyCode::Enter), Action::SearchSubmit);
        assert_eq!(at(KeyCode::Esc), Action::SearchCancel);
        assert_eq!(at(KeyCode::Left), Action::SearchLeft);
    }

    #[test]
    fn the_results_overlay_navigates_hits_and_starts_a_forward() {
        let at = |code| resolve(Focus::ChatList, Overlay::SearchResults, &key(code));
        assert_eq!(at(KeyCode::Char('j')), Action::ResultNext);
        assert_eq!(at(KeyCode::Up), Action::ResultPrev);
        assert_eq!(at(KeyCode::Enter), Action::ResultOpen);
        assert_eq!(at(KeyCode::Char('f')), Action::ForwardOpen);
        assert_eq!(at(KeyCode::Esc), Action::SearchCancel);
    }

    #[test]
    fn the_forward_overlay_picks_a_target_and_confirms() {
        let at = |code| resolve(Focus::ChatList, Overlay::Forward, &key(code));
        assert_eq!(at(KeyCode::Char('j')), Action::ForwardNext);
        assert_eq!(at(KeyCode::Char('k')), Action::ForwardPrev);
        assert_eq!(at(KeyCode::Enter), Action::ForwardConfirm);
        assert_eq!(at(KeyCode::Esc), Action::ForwardCancel);
    }

    #[test]
    fn g_and_end_jump_to_the_newest_message_in_the_history() {
        // `G` (Shift-g) and End jump to the bottom-anchored newest message (#158),
        // only in the history pane — End still means end-of-line in the composer.
        let shift_g = KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT);
        assert_eq!(
            resolve(Focus::History, Overlay::None, &shift_g),
            Action::JumpToNewest
        );
        assert_eq!(resolved(Focus::History, KeyCode::End), Action::JumpToNewest);
        assert_eq!(resolved(Focus::Composer, KeyCode::End), Action::ComposerEnd);
        // Unbound in the chat list (history-only context).
        assert_eq!(resolved(Focus::ChatList, KeyCode::End), Action::Noop);
    }

    #[test]
    fn history_keys_act_on_the_selected_message() {
        // r / e / d / R / p / f / a / S operate on the selected message in the
        // history pane. Reply took `r` (the vim/mutt/Telegram convention) and react
        // moved to `R` (#195).
        assert_eq!(
            resolved(Focus::History, KeyCode::Char('r')),
            Action::ReplyMessage
        );
        assert_eq!(
            resolved(Focus::History, KeyCode::Char('e')),
            Action::EditMessage
        );
        assert_eq!(
            resolved(Focus::History, KeyCode::Char('d')),
            Action::DeleteMessage
        );
        // React is now the shifted `R`; the lowercase `r` replies.
        let shift_r = KeyEvent::new(KeyCode::Char('R'), KeyModifiers::SHIFT);
        assert_eq!(
            resolve(Focus::History, Overlay::None, &shift_r),
            Action::ReactionOpen
        );
        assert_eq!(
            resolved(Focus::History, KeyCode::Char('p')),
            Action::PinToggle
        );
        assert_eq!(
            resolved(Focus::History, KeyCode::Char('f')),
            Action::ForwardMessage
        );
        assert_eq!(
            resolved(Focus::History, KeyCode::Char('a')),
            Action::AttachOpen
        );
        assert_eq!(
            resolved(Focus::History, KeyCode::Char('S')),
            Action::SaveMedia
        );
        assert_eq!(
            resolved(Focus::History, KeyCode::Char('y')),
            Action::CopyMessage
        );
        // The same letters are plain text in the composer, not history commands.
        assert_eq!(
            resolved(Focus::Composer, KeyCode::Char('r')),
            Action::ComposerInput('r')
        );
        assert_eq!(
            resolved(Focus::Composer, KeyCode::Char('e')),
            Action::ComposerInput('e')
        );
        // …and unbound in the chat list (history-only context).
        assert_eq!(resolved(Focus::ChatList, KeyCode::Char('p')), Action::Noop);
    }

    #[test]
    fn ctrl_r_resyncs_and_ctrl_q_opens_logout_from_every_focus() {
        // Both are global Ctrl chords, so they fire from any pane — including the
        // composer, where a bare letter would type (a Ctrl chord never inserts).
        for focus in [Focus::ChatList, Focus::History, Focus::Composer] {
            assert_eq!(resolve(focus, Overlay::None, &ctrl('r')), Action::Resync);
            assert_eq!(
                resolve(focus, Overlay::None, &ctrl('q')),
                Action::LogoutOpen
            );
        }
    }

    #[test]
    fn the_delete_confirm_overlay_picks_a_scope_and_confirms() {
        let at = |code| resolve(Focus::History, Overlay::DeleteConfirm, &key(code));
        assert_eq!(at(KeyCode::Enter), Action::DeleteConfirm);
        assert_eq!(at(KeyCode::Tab), Action::DeleteToggleScope);
        assert_eq!(at(KeyCode::Esc), Action::DeleteCancel);
        // A stray key does nothing while the destructive confirm is up.
        assert_eq!(at(KeyCode::Char('x')), Action::Noop);
        // Ctrl-C still escapes the modal.
        assert_eq!(
            resolve(Focus::History, Overlay::DeleteConfirm, &ctrl('c')),
            Action::Quit
        );
    }

    #[test]
    fn the_logout_confirm_overlay_confirms_or_cancels() {
        let at = |code| resolve(Focus::ChatList, Overlay::LogoutConfirm, &key(code));
        assert_eq!(at(KeyCode::Enter), Action::LogoutConfirm);
        assert_eq!(at(KeyCode::Esc), Action::LogoutCancel);
        assert_eq!(at(KeyCode::Char('x')), Action::Noop);
        // Ctrl-C quits the app without logging out.
        assert_eq!(
            resolve(Focus::ChatList, Overlay::LogoutConfirm, &ctrl('c')),
            Action::Quit
        );
    }

    #[test]
    fn the_reaction_overlay_routes_keys_for_the_reducer_to_interpret() {
        let at = |code| resolve(Focus::History, Overlay::Reaction, &key(code));
        // Arrows navigate the palette outright; character keys are routed generically
        // (the reducer decides palette shortcut vs custom-emoji input by mode).
        assert_eq!(at(KeyCode::Down), Action::ReactionNext);
        assert_eq!(at(KeyCode::Up), Action::ReactionPrev);
        assert_eq!(at(KeyCode::Char('j')), Action::ReactionKey('j'));
        assert_eq!(at(KeyCode::Char('c')), Action::ReactionKey('c'));
        assert_eq!(at(KeyCode::Char('🔥')), Action::ReactionKey('🔥'));
        assert_eq!(at(KeyCode::Backspace), Action::ReactionBackspace);
        assert_eq!(at(KeyCode::Enter), Action::ReactionConfirm);
        assert_eq!(at(KeyCode::Esc), Action::ReactionCancel);
    }

    #[test]
    fn the_send_media_overlay_edits_fields_and_confirms() {
        let at = |code| resolve(Focus::History, Overlay::SendMedia, &key(code));
        // Focus is ignored — the overlay owns the keys. Printables edit the field.
        assert_eq!(at(KeyCode::Char('a')), Action::AttachInput('a'));
        assert_eq!(at(KeyCode::Tab), Action::AttachToggleField);
        assert_eq!(at(KeyCode::Backspace), Action::AttachBackspace);
        assert_eq!(at(KeyCode::Enter), Action::AttachConfirm);
        assert_eq!(at(KeyCode::Esc), Action::AttachCancel);
    }

    #[test]
    fn s_opens_the_secret_chat_lifecycle_from_the_chat_list() {
        assert_eq!(
            resolved(Focus::ChatList, KeyCode::Char('s')),
            Action::SecretOpen
        );
        // The same letter is plain text in the composer, not a command.
        assert_eq!(
            resolved(Focus::Composer, KeyCode::Char('s')),
            Action::ComposerInput('s')
        );
        // …and unbound in the history (chat-list-only context).
        assert_eq!(resolved(Focus::History, KeyCode::Char('s')), Action::Noop);
    }

    #[test]
    fn the_secret_chat_overlay_confirms_and_cancels() {
        let at = |code| resolve(Focus::ChatList, Overlay::SecretChat, &key(code));
        assert_eq!(at(KeyCode::Enter), Action::SecretConfirm);
        assert_eq!(at(KeyCode::Esc), Action::SecretCancel);
        // Other keys do nothing while the modal is up.
        assert_eq!(at(KeyCode::Char('x')), Action::Noop);
    }

    #[test]
    fn ctrl_c_quits_from_every_overlay() {
        for overlay in [
            Overlay::SearchInput,
            Overlay::SearchResults,
            Overlay::Forward,
            Overlay::Reaction,
            Overlay::SendMedia,
            Overlay::SecretChat,
            Overlay::DeleteConfirm,
            Overlay::LogoutConfirm,
        ] {
            assert_eq!(resolve(Focus::ChatList, overlay, &ctrl('c')), Action::Quit);
        }
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
