# Profiling a real session (#185)

Synthetic fixtures (benches, #183) only exercise hot paths in isolation. This
is the step-by-step guide for the real-account exercise: three capture tools,
run one at a time against a live `tuigram` session, over the fallback
scenarios from #185 (cold start on a large account, scrolling a busy group,
a sustained update storm).

Run these **one tool at a time** — stacking dhat/console instrumentation on
top of each other skews both readings, and samply samples the process from
outside so it wants an unencumbered binary.

## 0. One-time setup

```sh
# samply: samples the running process from outside — no code change, no
# feature flag, just the tool. Preferred over cargo-flamegraph on macOS: it
# doesn't go through Instruments/xctrace (see the gotcha below and the
# Troubleshooting section), and it opens an interactive view (zoom, search
# by symbol) instead of one static SVG.
cargo install samply

# tokio-console: the CLI that connects to the running binary's diagnostic port.
cargo install tokio-console
```

dhat needs no separate install — it's a regular (optional) dependency,
already wired into `tuigram-client` behind the `profile-dhat` feature.

### Gotcha: build with `--features tuigram-client/static` for anything that runs the binary directly

`samply` (and `cargo-flamegraph`, if you use that instead — see
Troubleshooting) exec the compiled `tuigram` binary directly, not through
`cargo run`. A plain `cargo build --release` links against tdjson as a
dylib whose install name is `@rpath/libtdjson.<ver>.dylib`, resolved via an
rpath that only `cargo run`/`cargo test` supply (they set `DYLD_LIBRARY_PATH`
for you); the standalone binary itself carries no working rpath and fails
immediately with:

```
dyld[...]: Library not loaded: @rpath/libtdjson.1.8.61.dylib
Reason: no LC_RPATH's found
```

Sidestep it entirely by building the profiling binary statically linked
(same feature the release CI job uses, #167) — no dylib, no rpath, runs
standalone:

```sh
cargo build --release -p tuigram-client --bin tuigram --features tuigram-client/static
```

Do this once before a `samply`/`flamegraph` session; `cargo run`-based tools
(dhat, tokio-console below) don't need it.

## 1. samply (CPU)

```sh
samply record ./target/release/tuigram
```

Use the TUI normally for the scenario you're capturing (see §4), then quit
(`q`) — samply opens the recorded profile in the Firefox Profiler UI
(locally, in your browser) as soon as the process exits. Use its "Save"
button to keep a `.json.gz` per scenario before the next run.

## 2. dhat (heap)

```sh
cargo run -p tuigram-client --release --features profile-dhat
```

Run the scenario, quit normally (`q`) — the profiler guard in `main.rs` is
dropped on return, which flushes `dhat-heap.json` to the working directory.
Open it at <https://nnethercote.github.io/dh_view/dh_view.html> (drag the file
in, nothing is uploaded — it's a client-side page). Rename the json per
scenario before the next run, same as the samply profile.

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
samply's self-time for that function are the two views that would confirm or
clear it.

## 5. Reporting

File each finding as its own issue (per #185's deliverables), and paste the
startup-time/idle-RSS numbers plus links/attachments to the samply profiles
and dhat json (or screenshots of the tokio-console tables) into #185's closing
comment as the baseline. If the render-path allocation suspicion is confirmed
or cleared, say so explicitly — that's the evidence #186 is gated on.

## Troubleshooting

**`cargo-flamegraph` instead of samply:** if you'd rather use
`cargo-flamegraph` (one static SVG instead of samply's interactive view), the
same static-build gotcha above applies, plus on macOS it shells out to
`xctrace` (Instruments' CLI), not `dtrace` — which only exists inside a full
Xcode.app, not the standalone Command Line Tools:

```
xcode-select: error: tool 'xctrace' requires Xcode, but active developer
directory '/Library/Developer/CommandLineTools' is a command line tools instance
```

Fix by pointing `xcode-select` at a full (or beta) Xcode install for the
session, then **switching back afterwards** — leaving it pointed at a beta
Xcode affects every other `cargo build`'s clang/linker on the machine, not
just this one:

```sh
# 1. Find your installed Xcode(s):
ls /Applications | grep -i xcode

# 2. Point at it (adjust the app name to what step 1 found):
sudo xcode-select -s /Applications/Xcode-beta.app/Contents/Developer

# 3. Accept its license once:
sudo xcodebuild -license accept

# 4. Verify:
xcrun xctrace version

# 5. Run cargo-flamegraph, then switch back to Command Line Tools:
CARGO_PROFILE_RELEASE_DEBUG=true cargo flamegraph -p tuigram-client --bin tuigram --features tuigram-client/static --
sudo xcode-select -s /Library/Developer/CommandLineTools
```

`samply` needs none of this — it's the reason it's the default recommendation
above.
