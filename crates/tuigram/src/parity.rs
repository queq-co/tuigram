//! Command-surface parity guard (#197).
//!
//! #195 shipped because the TUI silently fell behind the REPL's command
//! surface, with nothing to flag it. This module is the guard: every name in
//! [`tuigram_core::command_surface::REPL_COMMANDS`] must either resolve to a
//! real TUI [`Action`](crate::app::Action) (proved by actually constructing
//! that variant, so a rename or removal is a compile error here) or be listed
//! in [`DIVERGENT`] with a reason. [`docs/repl-tui-divergences.md`] mirrors
//! this list for human readers — update both together.
//!
//! [`docs/repl-tui-divergences.md`]: https://github.com/queq-co/tuigram/blob/develop/docs/repl-tui-divergences.md

use crate::app::Action;
use crate::keymap::Focus;

/// REPL commands intentionally without a TUI keybinding, and why. Mirrors
/// `docs/repl-tui-divergences.md`'s "REPL-only commands" table — keep both in
/// sync.
pub const DIVERGENT: &[(&str, &str)] = &[
    (
        "chats",
        "the TUI's chat-list pane is always visible; no toggle needed",
    ),
    (
        "history",
        "the TUI's history pane always shows the open chat; no separate on-demand fetch",
    ),
    (
        "read",
        "the TUI marks the open chat's messages read automatically while it's open; the REPL \
         has no equivalent live loop, so it needs an explicit command",
    ),
    (
        "file",
        "the TUI's conversation pane always renders a downloadable message's transfer state \
         inline; the REPL has no persistent view, so it needs an explicit command",
    ),
    (
        "folder",
        "the TUI cycles chat lists in a fixed order (NextList/PrevList); a REPL-style jump to an \
         arbitrary folder id has no keybinding",
    ),
    (
        "secrets",
        "the TUI already shows each chat row's secret-chat lifecycle state inline; the REPL has \
         no persistent view, so it needs an explicit listing command",
    ),
    (
        "status",
        "the TUI's status bar always shows connection state; no on-demand command needed",
    ),
    (
        "probe",
        "a terminal-injection self-test (#174); a developer/security diagnostic, not a user action",
    ),
    (
        "typing",
        "the TUI sends the typing action automatically while the composer has unsent input \
         (#197); there is no manual one-shot command",
    ),
];

/// The `Action` a REPL command with real TUI parity maps to, or `None` when
/// the name is not dispatched by the TUI at all (in which case it must be in
/// [`DIVERGENT`], checked by the test below). Constructing the real variant
/// here — rather than comparing strings — means a renamed or removed `Action`
/// fails to *compile*, not just fails a run-time assertion.
fn bound_action(repl_command: &str) -> Option<Action> {
    match repl_command {
        "send" => Some(Action::ComposerSubmit),
        "reply" => Some(Action::ReplyMessage),
        "edit" => Some(Action::EditMessage),
        "delete" => Some(Action::DeleteMessage),
        "search" => Some(Action::SearchOpen),
        "forward" => Some(Action::ForwardMessage),
        "download" => Some(Action::SaveMedia),
        "sendmedia" => Some(Action::AttachOpen),
        "archive" | "folders" => Some(Action::NextList),
        "react" | "unreact" => Some(Action::ReactionConfirm),
        "pin" | "unpin" => Some(Action::PinToggle),
        "secret-close" => Some(Action::SecretOpen),
        // "secret-new <user_id>" targets any user id, contact or not; the
        // chat-list-scoped `SecretOpen` can't reach one outside the open chat
        // list, so the real analog is the contact-search picker (#197) — closer,
        // though narrower (contacts only; see docs/repl-tui-divergences.md).
        "secret-new" => Some(Action::ContactSearchOpen),
        "resync" => Some(Action::Resync),
        "logout" => Some(Action::LogoutOpen),
        "help" => Some(Action::ToggleHelp),
        "quit" => Some(Action::Quit),
        // "open" resolves through the chat-list's Enter binding, not a
        // standalone Action constructed here; recorded structurally below.
        "open" => Some(Action::SetFocus(Focus::History)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuigram_core::command_surface::REPL_COMMANDS;

    #[test]
    fn every_repl_command_has_tui_parity_or_is_an_explicit_divergence() {
        for name in REPL_COMMANDS {
            let bound = bound_action(name);
            let divergent = DIVERGENT.iter().find(|(n, _)| n == name);
            match (bound, divergent) {
                (Some(_), None) => {}
                (None, Some(_)) => {}
                (Some(action), Some((_, reason))) => panic!(
                    "REPL command `{name}` is both bound to {action:?} and listed in DIVERGENT \
                     ({reason:?}) — pick one",
                ),
                (None, None) => panic!(
                    "REPL command `{name}` has no TUI Action and is not in DIVERGENT — either \
                     add a keybinding (this would have caught #195) or add it to DIVERGENT (and \
                     docs/repl-tui-divergences.md) with a reason",
                ),
            }
        }
    }

    #[test]
    fn divergent_names_are_real_repl_commands() {
        // Catches a stale DIVERGENT entry for a command that was renamed or
        // removed from the REPL.
        for (name, _) in DIVERGENT {
            assert!(
                REPL_COMMANDS.contains(name),
                "DIVERGENT lists `{name}`, which is not a REPL command",
            );
        }
    }

    #[test]
    fn divergent_names_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for (name, _) in DIVERGENT {
            assert!(seen.insert(*name), "`{name}` is listed in DIVERGENT twice");
        }
    }
}
