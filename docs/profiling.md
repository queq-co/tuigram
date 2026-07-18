# Profiling a real session (#185)

Synthetic fixtures (benches, #183) only exercise hot paths in isolation. This
is the step-by-step guide for the real-account exercise: three capture tools,
run one at a time against a live `tuigram` session, over the fallback
scenarios from #185 (cold start on a large account, scrolling a busy group,
a sustained update storm).

Run these **one tool at a time** — stacking dhat/console instrumentation on
top of each other skews both readings, and a flamegraph samples the process
from outside so it wants an unencumbered binary.

## 0. One-time setup

```sh
# Flamegraph: samples the running process from outside — no code change, no
# feature flag, just the tool.
cargo install flamegraph

# macOS only: flamegraph shells out to `dtrace`, which needs one-time sudo
# grants. Linux uses `perf` instead — install via your package manager
# (e.g. `sudo apt install linux-tools-common linux-tools-generic`) and see
# https://github.com/flamegraph-rs/flamegraph#perf for the paranoid/kptr sysctls.
sudo dtrace -l >/dev/null   # macOS: triggers the permission prompt once

# tokio-console: the CLI that connects to the running binary's diagnostic port.
cargo install tokio-console
```

dhat needs no separate install — it's a regular (optional) dependency,
already wired into `tuigram-client` behind the `profile-dhat` feature.

## 1. Flamegraph (CPU)

Release-profile symbols are stripped (`strip = "symbols"`, #184), which would
leave a flamegraph full of `??`. Override just the debug-info bit for this run
— `CARGO_PROFILE_RELEASE_DEBUG=true` keeps everything else in `[profile.release]`
(lto, codegen-units) intact so the perf characteristics still match a real
release build:

```sh
CARGO_PROFILE_RELEASE_DEBUG=true cargo flamegraph -p tuigram-client --bin tuigram -- 
```

Use the TUI normally for the scenario you're capturing (see §4), then quit
(`q`) — flamegraph stops sampling when the process exits and writes
`flamegraph.svg` in the current directory. Rename it per scenario before the
next run, e.g. `mv flamegraph.svg flamegraph-cold-start.svg`.

## 2. dhat (heap)

```sh
cargo run -p tuigram-client --release --features profile-dhat
```

Run the scenario, quit normally (`q`) — the profiler guard in `main.rs` is
dropped on return, which flushes `dhat-heap.json` to the working directory.
Open it at <https://nnethercote.github.io/dh_view/dh_view.html> (drag the file
in, nothing is uploaded — it's a client-side page). Rename the json per
scenario before the next run, same as the flamegraph.

## 3. tokio-console (tasks)

Needs a separate `rustflags` cfg (`tokio_unstable`) that a Cargo feature can't
set on its own, and the aggregator needs a second terminal to watch it in:

```sh
# Terminal 1 — run tuigram with the console server enabled:
RUSTFLAGS="--cfg tokio_unstable" cargo run -p tuigram-client --release --features profile-console

# Terminal 2 — attach the viewer (connects to 127.0.0.1:6669 by default):
tokio-console
```

`tokio-console` shows live task/resource tables while terminal 1's TUI is in
the scenario; there's nothing to save automatically — take a screenshot of
the task list at whatever point looks interesting (a spike in busy tasks, a
task with abnormal poll duration), same as you would for a one-off `top`.

## 4. Scenarios (the fallback set from #185)

Run all three tools through each scenario before moving to the next scenario
(9 runs total: 3 tools × 3 scenarios). For each, note the wall-clock time from
launch to the TUI's first fully-rendered frame (cold start) or to steady state
(the other two), plus `ps`/Activity Monitor RSS at idle after the scenario
settles — these are the "startup-time and idle-RSS figures" #185 asks to be
recorded as the baseline budget.

1. **Cold start, large account** — log into an account with a large chat
   list / history, from a fresh (or cleared) local session, and time to first
   render.
2. **Scrolling a busy group** — open the highest-volume group chat available
   and scroll through several screens of history continuously for ~30s.
3. **Sustained update storm** — sit in an active chat (or one with a bot/bulk
   sender posting continuously) for a couple of minutes so a steady stream of
   `update*` events keeps arriving while idle-scrolled.

While in each scenario, keep an eye on the prime suspect named in #185: the
per-repaint rebuild of owned `Line<'static>` allocations in the render path
(`tuigram/src/ui/render/conversation.rs`) — dhat's allocation-site view and
the flamegraph's self-time for that path are the two views that would
confirm or clear it.

## 5. Reporting

File each finding as its own issue (per #185's deliverables), and paste the
startup-time/idle-RSS numbers plus links/attachments to the flamegraphs and
dhat json (or screenshots of the tokio-console tables) into #185's closing
comment as the baseline. If the render-path allocation suspicion is confirmed
or cleared, say so explicitly — that's the evidence #186 is gated on.
