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

/// The concrete terminal type the app draws onto: Ratatui over crossterm on the
/// process's stdout.
pub type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Owns raw mode + the alternate screen for the lifetime of the app. Building it
/// enters them; dropping it (a normal return *or* an unwinding panic) leaves
/// them again. The companion [`install_panic_hook`] runs the same teardown so
/// the terminal is restored even before a panic message prints.
pub struct TerminalGuard {
    terminal: Tui,
}

impl TerminalGuard {
    /// Enter raw mode + the alternate screen and wrap stdout in a Ratatui
    /// terminal. The screen is restored when the returned guard is dropped.
    pub fn new() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
        Ok(Self { terminal })
    }

    /// The underlying Ratatui terminal, for `draw` on the main task.
    pub fn terminal_mut(&mut self) -> &mut Tui {
        &mut self.terminal
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
