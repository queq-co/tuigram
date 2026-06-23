//! `tuigram` — a Ratatui Telegram client.
//!
//! This is the Phase 5 spine: an RAII terminal guard, a panic hook that restores
//! the terminal, and the single `tokio::select!` loop that races terminal input,
//! a render tick, and core events into [`Action`]s applied to one [`App`]. The
//! draw call stays on the main task and is never awaited inside. Real widgets and
//! live Telegram data arrive in later Phase 5/6 issues; the loop's shape does not
//! change when they do.
//!
//! Phase 6 (#109) bootstraps a real [`tuigram_core::Client`] before the loop: the
//! [`bootstrap`] module resolves credentials, drives login to `Ready` on the
//! plain terminal, and starts the update router. `main` holds that one handle
//! across the TUI run and closes TDLib cleanly on exit. The loop itself is still
//! fed by the temporary fake source — #110 swaps that for the client's update
//! stream without changing the loop's shape.

mod app;
mod bootstrap;
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
use std::process::ExitCode;
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
async fn main() -> ExitCode {
    // Phase 1 — bootstrap a live, authenticated `Client` on the plain terminal
    // (interactive login), before raw mode. A failure here prints and exits
    // without ever touching the TUI.
    let client = match bootstrap::bootstrap().await {
        Ok(client) => client,
        Err(err) => {
            eprintln!("tuigram: {err}");
            return ExitCode::FAILURE;
        }
    };

    // Phase 2 — run the TUI over the held client handle.
    install_panic_hook();
    let mut guard = match TerminalGuard::new() {
        Ok(guard) => guard,
        Err(err) => {
            eprintln!("tuigram: could not initialize the terminal: {err}");
            bootstrap::shutdown(&client).await;
            return ExitCode::FAILURE;
        }
    };
    let result = run(&mut guard).await;
    // Restore explicitly before any error reaches the user's normal screen.
    // (`guard`'s Drop would also restore, but make the ordering obvious.)
    drop(guard);

    // Phase 3 — close TDLib cleanly so its database is flushed, not left
    // mid-write for the next run. Runs on every exit path.
    bootstrap::shutdown(&client).await;

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("tuigram: {err}");
            ExitCode::FAILURE
        }
    }
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
