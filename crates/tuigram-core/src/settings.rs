//! User preferences read from `~/.config/tuigram/settings.toml` — currently the
//! download-cache retention policy (#120).
//!
//! Distinct from [`credentials`](crate::credentials): credentials are secrets the
//! onboarding flow writes (mode `600`), whereas settings are plain preferences the
//! user hand-edits. They live in sibling files under the same `tuigram` config dir
//! so both resolve identically, but this module owns no secrets and never writes —
//! it only reads, and a missing or malformed file falls back to safe defaults rather
//! than failing startup.
//!
//! ## Retention ([`StorageSettings`])
//!
//! Downloaded media accumulates in TDLib's cache with no bound; these settings cap
//! that by expiring files not accessed within a per-kind time-to-live, mirroring the
//! official apps' separate **private chats / groups / channels** "Keep Media"
//! controls. The driver applies them through TDLib `optimizeStorage`, whose `ttl` is
//! measured since a file was last *accessed* — so viewing a file resets its clock.
//!
//! Configured by hand in `~/.config/tuigram/settings.toml`; every key is optional and
//! defaults to `"forever"` (keep, never delete). Values are `"forever"` or a duration
//! — `"3d"`, `"1w"`, `"2w"`, `"1m"`, or a bare day count like `"14"`:
//!
//! ```toml
//! [storage]
//! keep_private  = "forever"   # one-to-one (and secret) chats
//! keep_groups   = "1w"        # basic groups + supergroups
//! keep_channels = "3d"        # broadcast channels
//! ```
//!
//! **Coverage caveat:** `optimizeStorage` scopes deletion to the chat ids it is
//! given, and tuigram pages chats lazily, so a sweep only reaches chats already
//! loaded into the store. Files from chats the user has not opened this session are
//! not expired until those chats load; the periodic sweep re-reads the store each
//! pass, so coverage grows as the user browses. Broadening this (a global size
//! backstop, or a full chat enumeration before a sweep) is a follow-up.

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Days per week and per month, the coarse units the config strings accept. A month
/// is a flat 30 days — this is a retention knob, not a calendar.
const DAYS_PER_WEEK: u32 = 7;
const DAYS_PER_MONTH: u32 = 30;
const SECONDS_PER_DAY: i64 = 86_400;

/// How long to keep downloaded media before it may be swept: forever (never
/// deleted), or a number of days since last access.
///
/// Parsed from and written back as a short string — `"forever"`, or a count with a
/// `d`/`w`/`m` unit suffix (`"3d"`, `"1w"`, `"1m"`) or a bare day count (`"14"`).
/// Weeks and months normalise to days (`"1w"` == `"7d"`), so the value round-trips
/// by meaning, not by spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeepMedia {
    /// Never auto-delete — the safe default, so an unconfigured install deletes
    /// nothing.
    #[default]
    Forever,
    /// Delete after this many days without access. Always ≥ 1 (a zero TTL, which
    /// would wipe media the instant it is fetched, is rejected at parse time).
    Days(u32),
}

impl KeepMedia {
    /// The `optimizeStorage` TTL in seconds, or `None` when media is kept forever
    /// (the driver then skips this kind entirely rather than passing an unbounded
    /// TTL). Saturates rather than overflowing on absurd day counts.
    #[must_use]
    pub fn to_ttl_seconds(self) -> Option<i32> {
        match self {
            Self::Forever => None,
            Self::Days(days) => {
                let seconds = i64::from(days) * SECONDS_PER_DAY;
                Some(i32::try_from(seconds).unwrap_or(i32::MAX))
            }
        }
    }
}

impl FromStr for KeepMedia {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let value = raw.trim().to_ascii_lowercase();
        if value == "forever" {
            return Ok(Self::Forever);
        }
        // A trailing d/w/m picks the unit; a bare number is days. Split the digits
        // from an optional single-letter suffix.
        let (digits, multiplier) = match value.strip_suffix(['d', 'w', 'm']) {
            Some(rest) if value.ends_with('w') => (rest, DAYS_PER_WEEK),
            Some(rest) if value.ends_with('m') => (rest, DAYS_PER_MONTH),
            Some(rest) => (rest, 1), // 'd'
            None => (value.as_str(), 1),
        };
        let count: u32 = digits.trim().parse().map_err(|_| {
            format!("expected \"forever\" or a duration like \"3d\"/\"1w\"/\"1m\", got {raw:?}")
        })?;
        let days = count
            .checked_mul(multiplier)
            .ok_or_else(|| format!("retention {raw:?} is too large"))?;
        if days == 0 {
            return Err(format!(
                "retention must be at least one day (or \"forever\"), got {raw:?}"
            ));
        }
        Ok(Self::Days(days))
    }
}

impl fmt::Display for KeepMedia {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Forever => f.write_str("forever"),
            Self::Days(days) => write!(f, "{days}d"),
        }
    }
}

impl Serialize for KeepMedia {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for KeepMedia {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        raw.parse().map_err(serde::de::Error::custom)
    }
}

/// The download-cache retention policy: one [`KeepMedia`] per chat kind. Grouped as
/// the official apps group them — one-to-one chats, groups, and broadcast channels —
/// so each can expire on its own schedule. All [`KeepMedia::Forever`] by default, so
/// retention is strictly opt-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StorageSettings {
    /// Retention for one-to-one (private, and secret) chats.
    pub keep_private: KeepMedia,
    /// Retention for basic groups and supergroups.
    pub keep_groups: KeepMedia,
    /// Retention for broadcast channels.
    pub keep_channels: KeepMedia,
}

impl StorageSettings {
    /// Whether any kind has a finite retention — if not, the sweep has nothing to do
    /// and the driver can skip scheduling it.
    #[must_use]
    pub fn sweeps_anything(&self) -> bool {
        [self.keep_private, self.keep_groups, self.keep_channels]
            .into_iter()
            .any(|k| k.to_ttl_seconds().is_some())
    }

    /// Load the retention policy from `~/.config/tuigram/settings.toml`'s `[storage]`
    /// table. A missing file, or one without a `[storage]` table, yields the
    /// all-`Forever` default; a malformed file logs a warning and also falls back to
    /// the default, so a typo disables retention rather than blocking startup.
    #[must_use]
    pub fn load() -> Self {
        let Some(path) = settings_path() else {
            return Self::default();
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            // No settings file is the common case: nothing configured, keep forever.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(err) => {
                eprintln!("tuigram: cannot read {}: {err}", path.display());
                return Self::default();
            }
        };
        match toml::from_str::<SettingsFile>(&text) {
            Ok(file) => file.storage,
            Err(err) => {
                eprintln!(
                    "tuigram: ignoring {} (retention disabled): {err}",
                    path.display()
                );
                Self::default()
            }
        }
    }
}

/// The whole settings file. Only `[storage]` today; a table keeps room for future
/// sections without a breaking reshape, and `#[serde(default)]` lets the file omit
/// it entirely.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SettingsFile {
    storage: StorageSettings,
}

/// `$XDG_CONFIG_HOME/tuigram/settings.toml`, falling back to `~/.config/...` — the
/// sibling of `credentials`' `config.toml`, resolved the same way. `None` when
/// neither `XDG_CONFIG_HOME` nor `HOME` is set (retention then simply stays at its
/// default rather than erroring).
fn settings_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|s| !s.is_empty())
                .map(|home| PathBuf::from(home).join(".config"))
        })?;
    Some(base.join("tuigram").join("settings.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_forever_case_insensitively() {
        assert_eq!("forever".parse::<KeepMedia>().unwrap(), KeepMedia::Forever);
        assert_eq!(
            "  Forever ".parse::<KeepMedia>().unwrap(),
            KeepMedia::Forever
        );
        assert_eq!(KeepMedia::Forever.to_ttl_seconds(), None);
    }

    #[test]
    fn parses_day_week_month_and_bare_counts() {
        assert_eq!("3d".parse::<KeepMedia>().unwrap(), KeepMedia::Days(3));
        assert_eq!("1w".parse::<KeepMedia>().unwrap(), KeepMedia::Days(7));
        assert_eq!("1m".parse::<KeepMedia>().unwrap(), KeepMedia::Days(30));
        assert_eq!("14".parse::<KeepMedia>().unwrap(), KeepMedia::Days(14));
        // TTL is the day count in seconds.
        assert_eq!(KeepMedia::Days(3).to_ttl_seconds(), Some(3 * 86_400));
    }

    #[test]
    fn rejects_zero_and_garbage() {
        assert!(
            "0d".parse::<KeepMedia>().is_err(),
            "zero would wipe on fetch"
        );
        assert!("".parse::<KeepMedia>().is_err());
        assert!("soon".parse::<KeepMedia>().is_err());
        assert!("1y".parse::<KeepMedia>().is_err(), "years unsupported");
    }

    #[test]
    fn keep_media_round_trips_by_meaning() {
        // A week serialises as its day-equivalent and parses back to the same value.
        for original in [KeepMedia::Forever, KeepMedia::Days(7), KeepMedia::Days(30)] {
            let text = original.to_string();
            assert_eq!(text.parse::<KeepMedia>().unwrap(), original);
        }
        assert_eq!(KeepMedia::Days(7).to_string(), "7d");
    }

    #[test]
    fn storage_settings_default_to_forever_and_sweep_nothing() {
        let settings = StorageSettings::default();
        assert_eq!(settings.keep_private, KeepMedia::Forever);
        assert_eq!(settings.keep_groups, KeepMedia::Forever);
        assert_eq!(settings.keep_channels, KeepMedia::Forever);
        assert!(!settings.sweeps_anything());
    }

    #[test]
    fn a_partial_storage_table_fills_the_rest_with_forever() {
        // Only channels configured: the others stay forever, and something sweeps.
        let file: SettingsFile = toml::from_str("[storage]\nkeep_channels = \"3d\"\n").unwrap();
        assert_eq!(file.storage.keep_channels, KeepMedia::Days(3));
        assert_eq!(file.storage.keep_private, KeepMedia::Forever);
        assert_eq!(file.storage.keep_groups, KeepMedia::Forever);
        assert!(file.storage.sweeps_anything());
    }

    #[test]
    fn an_empty_file_is_all_forever() {
        let file: SettingsFile = toml::from_str("").unwrap();
        assert_eq!(file.storage, StorageSettings::default());
    }

    #[test]
    fn a_bad_retention_value_fails_the_whole_parse() {
        // A malformed value surfaces as a parse error here; `load` turns that into a
        // warning + default, but the table itself must not silently coerce.
        assert!(toml::from_str::<SettingsFile>("[storage]\nkeep_groups = \"nope\"\n").is_err());
    }
}
