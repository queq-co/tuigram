//! Baseline for #183: `wrap::layout_rows` over long/emoji-heavy/CJK text at a
//! few pane widths — the composer and history pane's shared wrapping seam
//! (#214, #215).
//!
//! Run locally with `cargo bench -p tuigram-client`; not wired into default
//! CI (bench noise on shared runners is worse than no gate) — see #183.

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use tuigram_client::layout_rows;

fn wrap_bench(c: &mut Criterion) {
    let long = "This is an ordinary line of chat text, long enough to wrap \
                across several columns in a narrow pane. "
        .repeat(5);
    let emoji = "🎉🚀😄🔥✨🌟💯🙌👏🥳 ".repeat(20);
    let cjk = "这是一段中日韩宽字符组成的示例文本，用来撑满换行逻辑。".repeat(10);

    let mut group = c.benchmark_group("layout_rows");
    for width in [40_usize, 80, 120] {
        for (name, text) in [("long", &long), ("emoji", &emoji), ("cjk", &cjk)] {
            group.bench_with_input(BenchmarkId::new(name, width), &width, |b, &width| {
                b.iter(|| black_box(layout_rows(black_box(text), black_box(width))));
            });
        }
    }
    group.finish();
}

criterion_group!(benches, wrap_bench);
criterion_main!(benches);
