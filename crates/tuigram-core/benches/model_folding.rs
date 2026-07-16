//! Baseline for #183: folding throughput on a synthetic "joined a busy group"
//! update burst. Real sessions fold every update through both `ChatStore` and
//! `MessageStore` off the same stream, so this times both together rather
//! than either store in isolation.
//!
//! Run locally with `cargo bench -p tuigram-core`; not wired into default CI
//! (bench noise on shared runners is worse than no gate) — see #183.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use tuigram_core::{ChatStore, MessageStore};

fn busy_group_fold(c: &mut Criterion) {
    let updates = tuigram_fixtures::busy_group_burst(50, 200);
    c.bench_function("busy_group_fold_50_chats_x_200_messages", |b| {
        b.iter(|| {
            let mut chats = ChatStore::new();
            let mut messages = MessageStore::new();
            for update in &updates {
                chats.reduce(black_box(update));
                messages.reduce(black_box(update));
            }
            black_box((chats.len(), messages.is_empty()));
        });
    });
}

criterion_group!(benches, busy_group_fold);
criterion_main!(benches);
