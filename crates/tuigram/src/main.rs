//! `tuigram` — a Ratatui Telegram client.
//!
//! This is the Phase 5 spine: an RAII terminal guard, a panic hook that restores
//! the terminal, and the single `tokio::select!` loop that races terminal input,
//! a render tick, and core events into [`Action`]s applied to one [`App`]. The
//! draw call stays on the main task and is never awaited inside. Real widgets and
//! live Telegram data arrive in later Phase 5/6 issues; the loop's shape does not
//! change when they do.

mod app;
mod chat_list;
mod composer;
mod conversation;
mod event;
mod forward;
mod keymap;
mod login;
mod mediaform;
mod reactions;
mod search;
mod secret;
mod status;
mod terminal;
mod textinput;
mod ui;

use std::io;
use std::time::Duration;

use crossterm::event::EventStream;
use tokio_stream::StreamExt;

use crate::app::{Action, App};
use crate::event::spawn_fake_source;
use crate::terminal::{TerminalGuard, install_panic_hook};

/// Render cadence cap (~30 FPS). Bounds repaint rate independently of network
/// latency, so the UI stays smooth while core is mid-request.
const FRAME: Duration = Duration::from_millis(33);

/// Fake core heartbeat period (placeholder until Phase 6's real update stream).
const HEARTBEAT: Duration = Duration::from_secs(1);

#[tokio::main]
async fn main() -> io::Result<()> {
    install_panic_hook();
    let mut guard = TerminalGuard::new()?;
    let result = run(&mut guard).await;
    // Restore explicitly before any error reaches the user's normal screen.
    // (`guard`'s Drop would also restore, but make the ordering obvious.)
    drop(guard);
    result
}

/// The central event loop. Owns no terminal lifecycle (that is `guard`'s job) and
/// awaits only the `select!` sources — never the `draw`.
async fn run(guard: &mut TerminalGuard) -> io::Result<()> {
    let mut app = App::new();
    let mut input = EventStream::new();
    let mut tick = tokio::time::interval(FRAME);
    let mut core_rx = spawn_fake_source(HEARTBEAT);

    while !app.should_quit() {
        if app.is_dirty() {
            guard.terminal_mut().draw(|frame| ui::ui(frame, &app))?;
            app.clear_dirty();
        }

        tokio::select! {
            // Terminal input. `None` => stdin closed; treat as a quit so the
            // app never spins on a dead stream.
            maybe_event = input.next() => match maybe_event {
                Some(Ok(event)) => {
                    let action = app.on_terminal_event(event);
                    app.dispatch(action);
                }
                // Transient read error: ignore and re-enter the loop.
                Some(Err(_)) => {}
                None => app.dispatch(Action::Quit),
            },
            // Render tick: mark dirty so clocks/animations repaint on cadence.
            _ = tick.tick() => app.dispatch(Action::Render),
            // Core events (fake for now). `None` => the source ended; keep
            // running so the UI stays usable without it.
            maybe_app_event = core_rx.recv() => {
                if let Some(app_event) = maybe_app_event {
                    let action = app.on_app_event(app_event);
                    app.dispatch(action);
                }
            }
        }
    }

    Ok(())
}
