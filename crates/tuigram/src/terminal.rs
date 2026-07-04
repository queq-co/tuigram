//! RAII terminal lifecycle: raw mode + the alternate screen, entered on
//! construction and restored on drop — and, via [`install_panic_hook`], on a
//! panic from any task. A crash must never leave the user's terminal wedged in
//! raw mode or stuck on the alternate screen.

use std::io::{self, Stdout};

use crossterm::cursor::Show;
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui_image::picker::{Picker, ProtocolType};

/// The concrete terminal type the app draws onto: Ratatui over crossterm on the
/// process's stdout.
pub type Tui = Terminal<CrosstermBackend<Stdout>>;

/// What [`Picker::from_query_stdio`] detected at startup for the raster
/// mini-avatar (#201). Only real graphics protocols (Sixel/Kitty/iTerm2) count
/// as supported; halfblocks and any detection failure both fall back to
/// `None`, rendering today's #194 plain colored-name header with no avatar
/// gutter — this plan deliberately skips building and hand-verifying a second
/// (halfblocks) visual path.
#[derive(Debug, Clone, Default)]
pub enum AvatarSupport {
    /// A terminal that speaks Sixel, Kitty, or iTerm2 graphics. The `Picker` is
    /// unused until Stage 3's render path calls `new_protocol` on it.
    #[allow(dead_code)]
    Graphics(Picker),
    /// Halfblocks-only, or capability detection failed outright.
    #[default]
    None,
}

impl AvatarSupport {
    fn detect(picker: Picker) -> Self {
        match picker.protocol_type() {
            ProtocolType::Sixel | ProtocolType::Kitty | ProtocolType::Iterm2 => {
                AvatarSupport::Graphics(picker)
            }
            ProtocolType::Halfblocks => AvatarSupport::None,
        }
    }
}

/// Owns raw mode + the alternate screen for the lifetime of the app. Building it
/// enters them; dropping it (a normal return *or* an unwinding panic) leaves
/// them again. The companion [`install_panic_hook`] runs the same teardown so
/// the terminal is restored even before a panic message prints.
pub struct TerminalGuard {
    terminal: Tui,
    avatar_support: AvatarSupport,
}

impl TerminalGuard {
    /// Enter raw mode + the alternate screen and wrap stdout in a Ratatui
    /// terminal. The screen is restored when the returned guard is dropped.
    pub fn new() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
        // Must run after entering the alternate screen but before any terminal
        // events are read (`Picker`'s own contract) — right here, before the
        // caller starts its event loop. A query failure (no real TTY: CI, piped
        // output) falls back to `AvatarSupport::None` rather than propagating,
        // since a missing avatar is not a fatal condition for the rest of the app.
        let avatar_support = Picker::from_query_stdio()
            .map(AvatarSupport::detect)
            .unwrap_or(AvatarSupport::None);
        Ok(Self {
            terminal,
            avatar_support,
        })
    }

    /// The underlying Ratatui terminal, for `draw` on the main task.
    pub fn terminal_mut(&mut self) -> &mut Tui {
        &mut self.terminal
    }

    /// The graphics-protocol capability detected at construction time, for the
    /// bootstrap path to seed onto `App` (#201).
    pub fn avatar_support(&self) -> &AvatarSupport {
        &self.avatar_support
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Best-effort: there is nothing useful to do with an error while we are
        // already tearing the terminal down.
        let _ = restore();
    }
}

/// Tear down raw mode + the alternate screen on the process's stdout and make
/// the cursor visible again. Idempotent and best-effort, so it is safe to call
/// from both [`TerminalGuard`]'s `Drop` and the panic hook even if the terminal
/// was already (partly) restored.
pub fn restore() -> io::Result<()> {
    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen, Show)?;
    Ok(())
}

/// Chain terminal restoration in front of the existing panic hook so a panic in
/// any task leaves the terminal usable *before* the (often multi-line) panic
/// message is printed onto the user's normal screen.
pub fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore();
        original(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    // `Picker::from_query_stdio` manages its own raw mode and genuinely queries
    // real stdio (see its `query_with_timeout`), so this test's outcome is
    // environment-dependent: on CI (no TTY, or piped stdio) it falls back to
    // `None`, but run inside a real Sixel/Kitty/iTerm2 terminal it can validly
    // detect `Graphics`. Only assert what's actually guaranteed everywhere —
    // that detection completes without panicking — not a specific variant.
    #[test]
    fn avatar_support_detects_without_panicking() {
        let _support = Picker::from_query_stdio()
            .map(AvatarSupport::detect)
            .unwrap_or(AvatarSupport::None);
    }
}
