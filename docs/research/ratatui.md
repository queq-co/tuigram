# Research: Ratatui

> **Phase 1 placeholder.** Questions to answer; findings land here.

## Questions

- Rendering model: immediate-mode draw loop, `Frame`, widgets, and `Buffer`.
- Backend choice: `crossterm` (portable) vs alternatives.
- Event handling: input events, resize, and how to keep the draw loop
  non-blocking while `tuigram-core` does async network I/O.
- Layout primitives: `Layout`/`Constraint`, and patterns for a chat-list +
  message-pane + composer view.
- State management patterns for a responsive app (app state vs widget state).
- Testing TUI output: `TestBackend` / buffer assertions.

## Links

- Site: https://ratatui.rs
- Repo: https://github.com/ratatui/ratatui
