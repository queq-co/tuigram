//! Baseline for #183: the chat-list pane's projection over 1k+ chats —
//! folding chats into a `ChatStore` (the same `updateNewChat`/
//! `updateChatPosition` fold a real session does) then reading `project_lists`
//! back out into the switchable-list view (#113).
//!
//! Run locally with `cargo bench -p tuigram-client`; not wired into default
//! CI (bench noise on shared runners is worse than no gate) — see #183.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use tuigram_client::project_lists;

const CHATS: usize = 2_000;

fn project_lists_bench(c: &mut Criterion) {
    let store = tuigram_fixtures::chat_store(CHATS);
    c.bench_function("project_lists_2000_chats", |b| {
        b.iter(|| black_box(project_lists(black_box(&store))));
    });
}

criterion_group!(benches, project_lists_bench);
criterion_main!(benches);
