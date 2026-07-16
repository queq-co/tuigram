//! Baseline for #183: `ConversationView::project` over a 10k-message
//! open-chat history (#114) — both the fresh-open fold and, separately, a
//! same-chat *refresh*, which is what actually exercises the anchor recompute
//! (`following`/`newest_anchor`, or the by-id cursor search) #183 names:
//! `project`'s same-chat branch only runs when `chat_id` matches the view's
//! existing one, which a fresh-open bench alone never reaches.
//!
//! Run locally with `cargo bench -p tuigram-client`; not wired into default
//! CI (bench noise on shared runners is worse than no gate) — see #183.

use std::collections::{HashMap, HashSet};
use std::hint::black_box;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use tuigram_client::ConversationView;

const CHAT_ID: i64 = 1;
const MESSAGES: usize = 10_000;

fn project_fresh_open_bench(c: &mut Criterion) {
    let messages = tuigram_fixtures::fake_messages(MESSAGES, CHAT_ID);
    c.bench_function("conversation_project_10000_messages_fresh_open", |b| {
        b.iter_batched(
            || (ConversationView::default(), messages.clone()),
            |(mut view, messages)| {
                view.project(
                    black_box(CHAT_ID),
                    messages,
                    HashSet::new(),
                    HashMap::new(),
                    0,
                    0,
                    true,
                );
                black_box(view);
            },
            BatchSize::LargeInput,
        );
    });
}

fn project_refresh_bench(c: &mut Criterion) {
    let messages = tuigram_fixtures::fake_messages(MESSAGES, CHAT_ID);
    c.bench_function("conversation_project_10000_messages_refresh", |b| {
        b.iter_batched(
            || {
                let mut view = ConversationView::default();
                // Untimed warm-up: the fresh-open fold, so the timed iteration
                // measures a same-chat *refresh* — `project`'s anchor-recompute
                // branch (following the tail, or re-finding the cursor by id).
                view.project(
                    CHAT_ID,
                    messages.clone(),
                    HashSet::new(),
                    HashMap::new(),
                    0,
                    0,
                    true,
                );
                (view, messages.clone())
            },
            |(mut view, messages)| {
                view.project(
                    black_box(CHAT_ID),
                    messages,
                    HashSet::new(),
                    HashMap::new(),
                    0,
                    0,
                    false,
                );
                black_box(view);
            },
            BatchSize::LargeInput,
        );
    });
}

criterion_group!(benches, project_fresh_open_bench, project_refresh_bench);
criterion_main!(benches);
