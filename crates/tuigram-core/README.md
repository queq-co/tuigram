# tuigram-core

Headless core for [tuigram](https://github.com/queq-co/tuigram): Telegram
(TDLib) client logic — auth, chats, messages, and the update-folding model —
with no terminal/UI dependency, so it can be unit-tested without a TTY.

This crate is the backend the `tuigram` binary crate builds its Ratatui
front-end on top of. See the [repository README](https://github.com/queq-co/tuigram#readme)
and [`docs/`](https://github.com/queq-co/tuigram/tree/main/docs) for the full
picture (architecture, TDLib native-dependency notes, releasing).

License: MIT
