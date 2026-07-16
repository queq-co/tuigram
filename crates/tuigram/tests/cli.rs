//! Process-boundary coverage for the argv check (#166): what an actual caller
//! observes (stdout/stderr/exit code) invoking the real binary, complementing
//! `cli::parse`'s unit tests in `src/cli.rs`. Headless-safe by construction —
//! `--version`/`--help`/an unknown argument all return before any
//! terminal-mode or `TDLib` work, so this needs no TTY and holds in CI.

use std::process::Command;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tuigram"))
}

#[test]
fn version_flag_prints_and_exits_zero() {
    let out = bin().arg("--version").output().expect("run tuigram");
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");
    assert_eq!(
        stdout.trim(),
        format!("tuigram {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn short_version_flag_matches_long_form() {
    let long = bin().arg("--version").output().expect("run tuigram");
    let short = bin().arg("-V").output().expect("run tuigram");
    assert_eq!(long.stdout, short.stdout);
    assert!(short.status.success());
}

#[test]
fn help_flag_prints_usage_and_exits_zero() {
    let out = bin().arg("--help").output().expect("run tuigram");
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");
    assert!(stdout.starts_with("Usage: tuigram"));
    assert!(stdout.contains("config.toml"));
    assert!(stdout.contains("settings.toml"));
    assert!(stdout.contains("TUIGRAM_API_ID"));
    assert!(stdout.contains("TUIGRAM_API_HASH"));
}

#[test]
fn unknown_argument_prints_usage_to_stderr_and_exits_2() {
    let out = bin().arg("--bogus").output().expect("run tuigram");
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    let stderr = String::from_utf8(out.stderr).expect("utf8 stderr");
    assert!(stderr.starts_with("Usage: tuigram"));
    assert!(stderr.contains("unrecognized argument '--bogus'"));
}
