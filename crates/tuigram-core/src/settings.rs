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
//! defaults to keeping everything. The per-kind values are `"forever"` or a duration
//! — `"3d"`, `"1w"`, `"2w"`, `"1m"`, or a bare day count like `"14"` — and `max_cache`
//! is a global byte ceiling (`"unbounded"`, or a size like `"512MB"`/`"2GB"`):
//!
//! ```toml
//! [storage]
//! keep_private  = "forever"   # one-to-one (and secret) chats
//! keep_groups   = "1w"        # basic groups + supergroups
//! keep_channels = "3d"        # broadcast channels
//! max_cache     = "2GB"       # global size backstop across every chat
//! ```
//!
//! ## Two complementary policies
//!
//! The per-kind TTLs are applied through `optimizeStorage` **scoped** to chat ids, and
//! tuigram pages chats lazily, so a TTL sweep only reaches chats already loaded into
//! the store; the periodic sweep re-reads the store each pass, so coverage grows as
//! the user browses. To bound the cache regardless of which chats are loaded,
//! [`max_cache`](StorageSettings::max_cache) runs one **unscoped** `optimizeStorage`
//! with a byte cap (and no TTL) over every chat — a safety net that catches media from
//! chats never opened this session (#138). TDLib evicts least-recently-used files
//! first, so a generous ceiling rarely touches actively-used media while still
//! bounding the total. Both default off, so retention stays strictly opt-in.

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

/// The bytes in a kilobyte/megabyte/gigabyte — binary units, the sizes users expect
/// of a disk cache (`"1GB"` ≈ what the OS reports for a 1 GiB file).
const BYTES_PER_KB: u64 = 1024;
const BYTES_PER_MB: u64 = 1024 * BYTES_PER_KB;
const BYTES_PER_GB: u64 = 1024 * BYTES_PER_MB;

/// A global ceiling on the total downloaded-media cache: unbounded (grow freely), or a
/// size in bytes above which the least-recently-used files are swept.
///
/// Parsed from and written back as a short string — `"unbounded"`, or a count with a
/// `KB`/`MB`/`GB` suffix (`"512MB"`, `"2GB"`; the bare letter `K`/`M`/`G` also works)
/// or a plain byte count (`"1048576"` or `"1048576B"`). It round-trips by *meaning*:
/// a value that divides evenly serialises in the largest whole unit, so `"2048MB"`
/// comes back as `"2GB"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheCap {
    /// No size limit — the cache grows freely, the safe default so an unconfigured
    /// install deletes nothing.
    #[default]
    Unbounded,
    /// Cap the cache at this many bytes. Always ≥ 1 (a zero cap, which would wipe
    /// media the instant it is fetched, is rejected at parse time).
    Bytes(u64),
}

impl CacheCap {
    /// The `optimizeStorage` size limit in bytes, or `None` when the cache is
    /// unbounded (the driver then skips the global backstop rather than passing an
    /// unlimited cap). Saturates rather than overflowing on absurd byte counts.
    #[must_use]
    pub fn to_size_bytes(self) -> Option<i64> {
        match self {
            Self::Unbounded => None,
            Self::Bytes(bytes) => Some(i64::try_from(bytes).unwrap_or(i64::MAX)),
        }
    }
}

impl FromStr for CacheCap {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let value = raw.trim().to_ascii_lowercase();
        if value == "unbounded" {
            return Ok(Self::Unbounded);
        }
        // Strip an optional unit suffix (longest first so "gb" wins over "g"/"b"),
        // leaving the digits and the multiplier that turns them into bytes.
        let units = [
            ("gb", BYTES_PER_GB),
            ("mb", BYTES_PER_MB),
            ("kb", BYTES_PER_KB),
            ("g", BYTES_PER_GB),
            ("m", BYTES_PER_MB),
            ("k", BYTES_PER_KB),
            ("b", 1),
        ];
        let (digits, multiplier) = units
            .into_iter()
            .find_map(|(suffix, mult)| value.strip_suffix(suffix).map(|rest| (rest, mult)))
            .unwrap_or((value.as_str(), 1));
        let count: u64 = digits.trim().parse().map_err(|_| {
            format!("expected \"unbounded\" or a size like \"512MB\"/\"2GB\", got {raw:?}")
        })?;
        let bytes = count
            .checked_mul(multiplier)
            .ok_or_else(|| format!("cache size {raw:?} is too large"))?;
        if bytes == 0 {
            return Err(format!(
                "cache size must be at least one byte (or \"unbounded\"), got {raw:?}"
            ));
        }
        Ok(Self::Bytes(bytes))
    }
}

impl fmt::Display for CacheCap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unbounded => f.write_str("unbounded"),
            // Render in the largest unit that divides evenly, so the value round-trips
            // by meaning; fall back to a bare byte count when nothing divides cleanly.
            Self::Bytes(bytes) => {
                let bytes = *bytes;
                for (unit, size) in [
                    ("GB", BYTES_PER_GB),
                    ("MB", BYTES_PER_MB),
                    ("KB", BYTES_PER_KB),
                ] {
                    if bytes % size == 0 {
                        return write!(f, "{}{unit}", bytes / size);
                    }
                }
                write!(f, "{bytes}B")
            }
        }
    }
}

impl Serialize for CacheCap {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for CacheCap {
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
    /// Global size ceiling across every chat — the backstop that bounds the cache
    /// regardless of which chats have paged in (#138). Independent of the per-kind
    /// TTLs above.
    pub max_cache: CacheCap,
}

impl StorageSettings {
    /// Whether anything needs sweeping — any kind with a finite retention, or a finite
    /// cache ceiling. If not, the sweep has nothing to do and the driver can skip
    /// scheduling it.
    #[must_use]
    pub fn sweeps_anything(&self) -> bool {
        [self.keep_private, self.keep_groups, self.keep_channels]
            .into_iter()
            .any(|k| k.to_ttl_seconds().is_some())
            || self.max_cache.to_size_bytes().is_some()
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
        assert_eq!(settings.max_cache, CacheCap::Unbounded);
        assert!(!settings.sweeps_anything());
    }

    #[test]
    fn a_cache_cap_alone_makes_something_sweep() {
        // Every kind kept forever, but a byte ceiling is set — the driver must still
        // schedule the sweep so the global backstop runs (#138).
        let file: SettingsFile = toml::from_str("[storage]\nmax_cache = \"2GB\"\n").unwrap();
        assert_eq!(file.storage.keep_private, KeepMedia::Forever);
        assert_eq!(file.storage.max_cache, CacheCap::Bytes(2 * BYTES_PER_GB));
        assert!(file.storage.sweeps_anything());
    }

    #[test]
    fn parses_unbounded_case_insensitively() {
        assert_eq!(
            "unbounded".parse::<CacheCap>().unwrap(),
            CacheCap::Unbounded
        );
        assert_eq!(
            "  Unbounded ".parse::<CacheCap>().unwrap(),
            CacheCap::Unbounded
        );
        assert_eq!(CacheCap::Unbounded.to_size_bytes(), None);
    }

    #[test]
    fn parses_size_units_and_bare_bytes() {
        assert_eq!(
            "512mb".parse::<CacheCap>().unwrap(),
            CacheCap::Bytes(512 * BYTES_PER_MB)
        );
        assert_eq!(
            "2GB".parse::<CacheCap>().unwrap(),
            CacheCap::Bytes(2 * BYTES_PER_GB)
        );
        assert_eq!(
            "4k".parse::<CacheCap>().unwrap(),
            CacheCap::Bytes(4 * BYTES_PER_KB)
        );
        assert_eq!(
            "1048576".parse::<CacheCap>().unwrap(),
            CacheCap::Bytes(1_048_576)
        );
        assert_eq!("2048b".parse::<CacheCap>().unwrap(), CacheCap::Bytes(2048));
        assert_eq!(
            CacheCap::Bytes(2 * BYTES_PER_GB).to_size_bytes(),
            Some(2 * BYTES_PER_GB as i64)
        );
    }

    #[test]
    fn rejects_zero_and_garbage_sizes() {
        assert!(
            "0GB".parse::<CacheCap>().is_err(),
            "zero would wipe on fetch"
        );
        assert!("0".parse::<CacheCap>().is_err());
        assert!("".parse::<CacheCap>().is_err());
        assert!("lots".parse::<CacheCap>().is_err());
        assert!("2TB".parse::<CacheCap>().is_err(), "terabytes unsupported");
    }

    #[test]
    fn cache_cap_round_trips_in_the_largest_whole_unit() {
        // 2 GiB prints as "2GB", not "2048MB"; a size with no clean unit falls back to
        // bytes — either way it parses back to the same value.
        assert_eq!(CacheCap::Bytes(2 * BYTES_PER_GB).to_string(), "2GB");
        assert_eq!(CacheCap::Bytes(512 * BYTES_PER_MB).to_string(), "512MB");
        assert_eq!(CacheCap::Bytes(1500).to_string(), "1500B");
        for original in [
            CacheCap::Unbounded,
            CacheCap::Bytes(3 * BYTES_PER_GB),
            CacheCap::Bytes(1500),
        ] {
            let text = original.to_string();
            assert_eq!(text.parse::<CacheCap>().unwrap(), original);
        }
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
