//! RAII terminal lifecycle: raw mode + the alternate screen, entered on
//! construction and restored on drop — and, via [`install_panic_hook`], on a
//! panic from any task. A crash must never leave the user's terminal wedged in
//! raw mode or stuck on the alternate screen.

use std::io::{self, Stdout};

use crossterm::cursor::Show;
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
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

/// Floor on the avatar gutter's width in columns (#201), so a very wide
/// terminal font still leaves a bubble wide enough to read as an image rather
/// than a sliver.
const GUTTER_MIN_COLS: usize = 2;

/// What [`Picker::from_query_stdio`] detected at startup for the raster
/// mini-avatar (#201). Only real graphics protocols (Sixel/Kitty/iTerm2) count
/// as supported; halfblocks and any detection failure both fall back to
/// `None`, rendering today's #194 plain colored-name header with no avatar
/// gutter — this plan deliberately skips building and hand-verifying a second
/// (halfblocks) visual path.
#[derive(Debug, Clone, Default)]
pub enum AvatarSupport {
    /// A terminal that speaks Sixel, Kitty, or iTerm2 graphics.
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

    /// Width, in terminal columns, of the avatar gutter reserved to the left
    /// of each message (#201): `0` when this is `None`, so every line's
    /// leading span collapses to nothing and the pane renders byte-identical
    /// to pre-#201 output; otherwise sized from the terminal's own cell aspect
    /// ratio (`round(2 rows tall / cell aspect)`) so the 2-row bubble reads as
    /// roughly square, clamped to [`GUTTER_MIN_COLS`]. Shared by the render
    /// path (the gutter span's width) and `drive_avatars` (the target size
    /// handed to `Picker::new_protocol`), so an avatar is always encoded to
    /// fill exactly the space reserved for it.
    pub fn gutter_cols(&self) -> usize {
        let Self::Graphics(picker) = self else {
            return 0;
        };
        let font = picker.font_size();
        if font.width == 0 {
            return GUTTER_MIN_COLS;
        }
        let cols = (2.0 * f64::from(font.height) / f64::from(font.width)).round() as usize;
        cols.max(GUTTER_MIN_COLS)
    }

    /// Whether this terminal speaks a real graphics protocol — the single
    /// capability check shared by both the avatar gutter and inline media
    /// (#208), so a future settings toggle (#209) can gate both from one
    /// place.
    #[must_use]
    pub fn is_graphics(&self) -> bool {
        matches!(self, Self::Graphics(_))
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
    ///
    /// `mouse` turns on crossterm mouse reporting (#161/#162) so the loop receives
    /// click and wheel events; it is off when the `[interface] mouse` setting is
    /// `false`, leaving the terminal's native text selection intact. Teardown
    /// ([`restore`]) disables mouse capture unconditionally, so leaving it off here
    /// is always safe.
    pub fn new(mouse: bool) -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        if mouse {
            execute!(stdout, EnableMouseCapture)?;
        }
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

/// Tear down raw mode + the alternate screen on the process's stdout, disable
/// mouse capture, and make the cursor visible again. Idempotent and best-effort,
/// so it is safe to call from both [`TerminalGuard`]'s `Drop` and the panic hook
/// even if the terminal was already (partly) restored. Mouse capture is disabled
/// unconditionally — the escape is a no-op when it was never enabled — so an
/// unrestored terminal never leaks capture (and stray mouse-escape garbage on
/// scroll) regardless of the `[interface] mouse` setting.
pub fn restore() -> io::Result<()> {
    disable_raw_mode()?;
    execute!(
        io::stdout(),
        DisableMouseCapture,
        LeaveAlternateScreen,
        Show
    )?;
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
    use std::io::IsTerminal;

    // `Picker::from_query_stdio` manages its own raw mode and genuinely queries
    // real stdio (see its `query_with_timeout`), so this test's outcome is
    // environment-dependent: on CI (no TTY, or piped stdio) it falls back to
    // `None`, but run inside a real Sixel/Kitty/iTerm2 terminal it can validly
    // detect `Graphics`. Only assert what's actually guaranteed everywhere —
    // that detection completes without panicking — not a specific variant.
    //
    // Skipped outright when stdin is not a real terminal (#223): on
    // Windows, `enable_raw_mode`'s query path opens the system console
    // (`CONIN$`) directly rather than the process's actual stdin handle, so
    // it succeeds even when stdin itself is redirected (as it always is in
    // CI) — unlike Unix, where the analogous `tcgetattr` call fails fast on
    // a non-tty stdin. That leaves the query reading from a stdin no real
    // terminal will ever answer: each zero-byte read resends a "busy" signal
    // that resets `query_with_timeout`'s own deadline, so it never times out
    // on its own — a live lock, not a hang that self-resolves. This is the
    // suspected cause of the windows-x86_64 CI hang.
    #[test]
    fn avatar_support_detects_without_panicking() {
        if !std::io::stdin().is_terminal() {
            return;
        }
        let _support = Picker::from_query_stdio()
            .map(AvatarSupport::detect)
            .unwrap_or(AvatarSupport::None);
    }

    #[test]
    fn gutter_cols_is_zero_without_graphics_support() {
        // The `None` capability (halfblocks or no real terminal, #201) reserves
        // no gutter at all — this is what keeps that path byte-identical to
        // pre-#201 rendering.
        assert_eq!(AvatarSupport::None.gutter_cols(), 0);
    }

    #[test]
    fn gutter_cols_is_derived_from_the_pickers_font_aspect_ratio() {
        // `Picker::halfblocks()` reports a 10×20px cell; a real
        // graphics-capable `Picker` (any protocol) reports whatever the
        // terminal answered. round(2 rows tall / (20/10) cell aspect) = 4.
        let mut picker = Picker::halfblocks();
        picker.set_protocol_type(ProtocolType::Kitty);
        assert_eq!(AvatarSupport::Graphics(picker).gutter_cols(), 4);
    }
}
