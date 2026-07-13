//! Argv handling for the two flags `tuigram` supports (#166): `--version`/`-V`
//! and `--help`/`-h`. Packaging smoke tests, the Homebrew formula's `test do`
//! block, and plain user sanity all need `tuigram --version` to print and exit
//! — today `main` ignores argv entirely and starts the TUI regardless. This is
//! a hand-rolled check, not a general parser (clap would be overkill for two
//! flags): [`parse`] must run and return before any terminal-mode or TDLib work
//! so these paths exit cleanly with no TTY and no `~/.config/tuigram/` access.

use std::process::ExitCode;

/// The result of inspecting argv: either nothing matched and the TUI should
/// start normally, or a flag (or an unrecognized argument) already produced
/// its output and the process is done.
pub enum Action {
    Run,
    Exit(ExitCode),
}

/// Usage text, also used as the body of `--help`. Names the config/settings
/// file locations and the credential-override env vars
/// ([`tuigram_core::credentials::ENV_API_ID`]/[`ENV_API_HASH`](tuigram_core::credentials::ENV_API_HASH))
/// directly rather than hardcoding their string values, so this can't drift if
/// they're renamed.
pub fn usage() -> String {
    format!(
        "Usage: tuigram [OPTIONS]\n\
         \n\
         A terminal UI (Ratatui) Telegram client.\n\
         \n\
         Options:\n\
         \x20\x20-V, --version    Print version and exit\n\
         \x20\x20-h, --help       Print this help and exit\n\
         \n\
         Files:\n\
         \x20\x20$XDG_CONFIG_HOME/tuigram/config.toml    Telegram API credentials\n\
         \x20\x20$XDG_CONFIG_HOME/tuigram/settings.toml  Storage/interface settings\n\
         \x20\x20(both fall back to ~/.config/tuigram/ when XDG_CONFIG_HOME is unset)\n\
         \n\
         Environment:\n\
         \x20\x20XDG_CONFIG_HOME    Overrides the config directory above\n\
         \x20\x20HOME               Fallback base for the config directory\n\
         \x20\x20{api_id}    Telegram api_id (overrides config.toml)\n\
         \x20\x20{api_hash}  Telegram api_hash (overrides config.toml)\n",
        api_id = tuigram_core::credentials::ENV_API_ID,
        api_hash = tuigram_core::credentials::ENV_API_HASH,
    )
}

/// Inspect argv (excluding argv[0]). Only the first argument matters — the
/// surface is two flags, not a general parser — so `tuigram --version extra`
/// still prints the version; an unrecognized *first* argument is the only
/// rejection case.
pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Action {
    match args.into_iter().next().as_deref() {
        None => Action::Run,
        Some("--version" | "-V") => {
            println!("tuigram {}", tuigram_core::version());
            Action::Exit(ExitCode::SUCCESS)
        }
        Some("--help" | "-h") => {
            print!("{}", usage());
            Action::Exit(ExitCode::SUCCESS)
        }
        Some(other) => {
            eprint!("{}", usage());
            eprintln!("\nerror: unrecognized argument '{other}'");
            Action::Exit(ExitCode::from(2))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn no_args_runs() {
        assert!(matches!(parse(args(&[])), Action::Run));
    }

    // `ExitCode` has no `PartialEq`, so these only assert the variant; the
    // actual exit code observed by a caller is covered end-to-end by the
    // process-boundary test in `tests/cli.rs`.
    #[test]
    fn version_and_help_exit() {
        assert!(matches!(parse(args(&["--version"])), Action::Exit(_)));
        assert!(matches!(parse(args(&["-V"])), Action::Exit(_)));
        assert!(matches!(parse(args(&["--help"])), Action::Exit(_)));
        assert!(matches!(parse(args(&["-h"])), Action::Exit(_)));
    }

    #[test]
    fn unknown_argument_exits() {
        assert!(matches!(parse(args(&["--bogus"])), Action::Exit(_)));
    }

    #[test]
    fn usage_names_config_files_and_env_vars() {
        let text = usage();
        assert!(text.contains("config.toml"));
        assert!(text.contains("settings.toml"));
        assert!(text.contains(tuigram_core::credentials::ENV_API_ID));
        assert!(text.contains(tuigram_core::credentials::ENV_API_HASH));
    }
}
