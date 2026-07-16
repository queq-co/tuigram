//! User preferences read from `~/.config/tuigram/settings.toml` — the download-cache
//! retention policy (#120) and the terminal-UI toggles (`[interface]`, #161/#162,
//! plus the graphics toggle, #209).
//!
//! Distinct from [`credentials`](crate::credentials): credentials are secrets the
//! onboarding flow writes (mode `600`), whereas settings are plain preferences the
//! user hand-edits. They live in sibling files under the same `tuigram` config dir
//! so both resolve identically. This module owns no secrets, and its only write is a
//! **default template on first run** ([`StorageSettings::ensure_default_file`]) so the
//! file exists to edit — an existing file is never overwritten. Loads are pure reads,
//! and a missing or malformed file falls back to safe defaults rather than failing
//! startup.
//!
//! ## Retention ([`StorageSettings`])
//!
//! Downloaded media accumulates in `TDLib`'s cache with no bound; these settings cap
//! that by expiring files not accessed within a per-kind time-to-live, mirroring the
//! official apps' separate **private chats / groups / channels** "Keep Media"
//! controls. The driver applies them through `TDLib` `optimizeStorage`, whose `ttl` is
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
//! On first run tuigram writes a default `settings.toml` (all keys at their defaults)
//! so there is a real file to edit in place; an annotated `settings.example.toml` also
//! ships at the repo root as a fuller reference. Editing either — by hand, or through
//! the app — changes retention.
//!
//! ## Two complementary policies
//!
//! The per-kind TTLs are applied through `optimizeStorage` **scoped** to chat ids, and
//! tuigram pages chats lazily, so a TTL sweep only reaches chats already loaded into
//! the store; the periodic sweep re-reads the store each pass, so coverage grows as
//! the user browses. To bound the cache regardless of which chats are loaded,
//! [`max_cache`](StorageSettings::max_cache) runs one **unscoped** `optimizeStorage`
//! with a byte cap (and no TTL) over every chat — a safety net that catches media from
//! chats never opened this session (#138). `TDLib` evicts least-recently-used files
//! first, so a generous ceiling rarely touches actively-used media while still
//! bounding the total. Both default off, so retention stays strictly opt-in.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
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

    /// Render this policy as the text of a `settings.toml` — an annotated `[storage]`
    /// table with these values filled in. The single source of truth for the file's
    /// on-disk form: [`ensure_default_file`](Self::ensure_default_file) writes it on
    /// first run, and it round-trips (the output [`load`](Self::load)s back to `self`),
    /// so the same renderer can rewrite the file after an edit without losing shape.
    #[must_use]
    pub fn render(&self) -> String {
        // The comment block mirrors settings.example.toml's grammar so a hand-editor
        // needs no other reference; the values are this policy's, aligned on the keys.
        format!(
            "# tuigram settings — download-cache retention. Safe to hand-edit.\n\
             #\n\
             # Plain preferences, not secrets (credentials live in config.toml). Every\n\
             # key is optional and falls back to its default; the defaults below keep\n\
             # everything, so retention is opt-in. tuigram writes this file once with the\n\
             # defaults and never overwrites your edits.\n\
             #\n\
             # keep_* : \"forever\" (default), or a duration — \"3d\", \"1w\", \"1m\", or\n\
             #          a bare day count like \"14\". Deletes media not accessed within it.\n\
             # max_cache : \"unbounded\" (default), or a total-cache size ceiling across\n\
             #             every chat — \"512MB\", \"2GB\", or a byte count. Binary units.\n\
             \n\
             [storage]\n\
             keep_private  = \"{}\"\n\
             keep_groups   = \"{}\"\n\
             keep_channels = \"{}\"\n\
             max_cache     = \"{}\"\n",
            self.keep_private, self.keep_groups, self.keep_channels, self.max_cache
        )
    }

    /// First run: write a default `settings.toml` to the user's config path so the file
    /// exists to edit, if it is not there already. Best-effort maintenance — an
    /// unresolvable config dir or an unwritable location is logged and ignored (the app
    /// still runs on defaults), and an **existing** file is left exactly as-is.
    pub fn ensure_default_file() {
        let Some(path) = settings_path() else {
            return;
        };
        if let Err(err) = init_default_at(&path) {
            eprintln!("tuigram: could not write {}: {err}", path.display());
        }
    }

    /// Persist this policy to `~/.config/tuigram/settings.toml`, replacing any existing
    /// file with its [`render`](Self::render)ed form (annotations and all). This is the
    /// deliberate save behind the in-app editor (#146): unlike
    /// [`ensure_default_file`](Self::ensure_default_file), it **does** overwrite, so the
    /// on-disk file matches what the running session applies. The error (no config dir,
    /// an unwritable location) is returned rather than logged, so the caller can toast
    /// it while still applying the edit in memory.
    ///
    /// # Errors
    ///
    /// Returns an error if the config directory can't be resolved or the file
    /// can't be written.
    pub fn save(&self) -> io::Result<()> {
        let path = settings_path().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "no config directory (set HOME or XDG_CONFIG_HOME)",
            )
        })?;
        // The save rewrites the whole file, so re-emit the current `[interface]`
        // section too — read from disk — rather than dropping it. Otherwise saving
        // a retention edit through the in-app editor would silently reset a user's
        // `mouse = false` back to the default.
        let contents = format!(
            "{}{}",
            self.render(),
            InterfaceSettings::load().render_section()
        );
        write_settings_file(&path, &contents)
    }
}

/// Write the default template to `path` unless a settings file is already there.
/// Returns whether it wrote. Split from [`StorageSettings::ensure_default_file`] so the
/// first-run behaviour is testable against an explicit path with no `HOME`.
fn init_default_at(path: &Path) -> io::Result<bool> {
    if path.exists() {
        return Ok(false);
    }
    let contents = format!(
        "{}{}",
        StorageSettings::default().render(),
        InterfaceSettings::default().render_section()
    );
    write_settings_file(path, &contents)?;
    Ok(true)
}

/// Write `contents` to `path`, creating the parent dir. Unlike credentials' owner-only
/// `600`, settings are non-secret preferences, so the file is the usual world-readable
/// `644`. Always truncates — callers decide whether to overwrite before calling.
#[cfg(unix)]
fn write_settings_file(path: &Path, contents: &str) -> io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o644)
        .open(path)?;
    file.write_all(contents.as_bytes())?;
    file.set_permissions(std::fs::Permissions::from_mode(0o644))?;
    Ok(())
}

#[cfg(not(unix))]
fn write_settings_file(path: &Path, contents: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, contents)
}

/// Terminal-UI behaviour toggles (the `[interface]` table). Distinct from the
/// retention policy: these shape how the TUI reacts to input rather than what it
/// keeps on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct InterfaceSettings {
    /// Whether mouse reporting is enabled (#161/#162): clicking a pane focuses it
    /// and the wheel scrolls the pane under the pointer. On by default. Turning it
    /// off hands the terminal's native text selection back to the user, since
    /// mouse capture otherwise intercepts drag-to-select in most emulators.
    pub mouse: bool,
    /// Whether graphics rendering is enabled (#209): sender avatars (#201) and
    /// inline media (#208) draw as images on a graphics-capable terminal. On by
    /// default. Turning it off forces the plain-text layout that predates both —
    /// no avatar gutter (not even the generated fallback bubble) and text
    /// placeholders for photos/stickers/GIFs/videos — even though the terminal
    /// itself can still do graphics. Unlike `mouse`, this takes effect live: the
    /// in-app editor (#146 pattern) can flip it without a restart.
    pub graphics: bool,
}

impl Default for InterfaceSettings {
    /// Mouse and graphics both on — the common case. (Serde's `#[serde(default)]`
    /// uses this, so a file that omits `[interface]` or either key lands here,
    /// not on `bool`'s `false`.)
    fn default() -> Self {
        Self {
            mouse: true,
            graphics: true,
        }
    }
}

impl InterfaceSettings {
    /// Load the interface toggles from `settings.toml`'s `[interface]` table. A
    /// missing file, a missing section, or a malformed file all fall back to the
    /// defaults (mouse on); a malformed file is already warned about by
    /// [`StorageSettings::load`] at startup, so this stays quiet to avoid a second
    /// warning for the same file.
    #[must_use]
    pub fn load() -> Self {
        let Some(path) = settings_path() else {
            return Self::default();
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        toml::from_str::<SettingsFile>(&text)
            .map(|file| file.interface)
            .unwrap_or_default()
    }

    /// Render this section as the `[interface]` block of a `settings.toml`,
    /// annotated like the `[storage]` block. Appended after
    /// [`StorageSettings::render`] to make the whole-file text (see
    /// [`StorageSettings::save`]), so the two sections round-trip together.
    #[must_use]
    pub fn render_section(&self) -> String {
        format!(
            "\n\
             # [interface] — terminal UI behaviour.\n\
             #\n\
             # mouse : true (default) or false. When on, a click focuses the pane under\n\
             #         the pointer and the wheel scrolls the hovered pane. Mouse capture\n\
             #         takes over the terminal's native text selection, so set false to\n\
             #         keep drag-to-select (Shift/Option-drag also bypasses it on most\n\
             #         emulators).\n\
             # graphics : true (default) or false. When on, sender avatars and inline\n\
             #            media (photos/stickers/GIF+video stills) render as images on a\n\
             #            graphics-capable terminal. false forces the plain-text layout\n\
             #            from before that, with no avatar gutter and text placeholders\n\
             #            for media, even on a graphics-capable terminal.\n\
             \n\
             [interface]\n\
             mouse = {}\n\
             graphics = {}\n",
            self.mouse, self.graphics
        )
    }

    /// Persist this section to `~/.config/tuigram/settings.toml`, replacing any
    /// existing file. The deliberate save behind the in-app graphics toggle
    /// (#209) — mirrors [`StorageSettings::save`] but in the other direction: it
    /// re-reads the current `[storage]` table from disk and re-emits it
    /// unchanged, so persisting an interface edit never clobbers a retention
    /// policy the user (or `StorageSettings::save`) already wrote.
    ///
    /// # Errors
    ///
    /// Returns an error if the config directory can't be resolved or the file
    /// can't be written.
    pub fn save(&self) -> io::Result<()> {
        let path = settings_path().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "no config directory (set HOME or XDG_CONFIG_HOME)",
            )
        })?;
        let contents = format!(
            "{}{}",
            StorageSettings::load().render(),
            self.render_section()
        );
        write_settings_file(&path, &contents)
    }
}

/// The whole settings file: the `[storage]` retention policy and the `[interface]`
/// UI toggles. A table per concern keeps room for future sections without a
/// breaking reshape, and `#[serde(default)]` lets the file omit either entirely.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SettingsFile {
    storage: StorageSettings,
    interface: InterfaceSettings,
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
    fn the_shipped_example_parses_to_the_all_default_policy() {
        // `settings.example.toml` at the repo root is the user-facing template; keep it
        // in lockstep with the parser. Every key it shows is the default, so a fresh
        // copy is a valid no-op config (all-forever, unbounded, sweeps nothing).
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../settings.example.toml");
        let text = std::fs::read_to_string(path).expect("settings.example.toml must exist");
        let file: SettingsFile = toml::from_str(&text).expect("example must parse");
        assert_eq!(file.storage, StorageSettings::default());
        assert_eq!(file.interface, InterfaceSettings::default());
        assert!(!file.storage.sweeps_anything());
    }

    #[test]
    fn interface_defaults_to_mouse_and_graphics_on() {
        // `bool`'s own default is `false`; the section's is `true` for both, so an
        // unconfigured install gets mouse support and graphics rendering.
        assert!(InterfaceSettings::default().mouse);
        assert!(InterfaceSettings::default().graphics);
    }

    #[test]
    fn a_file_without_an_interface_section_keeps_mouse_and_graphics_on() {
        // The common case: a storage-only file (or the pre-#161 default template)
        // still yields both on because `#[serde(default)]` fills the missing table.
        let file: SettingsFile = toml::from_str("[storage]\n").unwrap();
        assert!(file.interface.mouse);
        assert!(file.interface.graphics);
    }

    #[test]
    fn interface_mouse_can_be_disabled() {
        let file: SettingsFile = toml::from_str("[interface]\nmouse = false\n").unwrap();
        assert!(!file.interface.mouse);
        // Unset in this partial table, so it still falls back to on.
        assert!(file.interface.graphics);
    }

    #[test]
    fn interface_graphics_can_be_disabled() {
        let file: SettingsFile = toml::from_str("[interface]\ngraphics = false\n").unwrap();
        assert!(!file.interface.graphics);
        assert!(file.interface.mouse, "unset in this partial table");
    }

    #[test]
    fn interface_section_round_trips_through_load() {
        // The rendered `[interface]` block, appended after `[storage]`, parses back
        // to the exact toggles — the default (both on), and each knob disabled on
        // its own — so a save is lossless.
        for original in [
            InterfaceSettings::default(),
            InterfaceSettings {
                mouse: false,
                graphics: true,
            },
            InterfaceSettings {
                mouse: true,
                graphics: false,
            },
            InterfaceSettings {
                mouse: false,
                graphics: false,
            },
        ] {
            let text = format!(
                "{}{}",
                StorageSettings::default().render(),
                original.render_section()
            );
            let parsed: SettingsFile = toml::from_str(&text).expect("rendered file must parse");
            assert_eq!(parsed.interface, original);
            // The storage half is untouched by the appended section.
            assert_eq!(parsed.storage, StorageSettings::default());
        }
    }

    #[test]
    fn a_saved_storage_edit_preserves_disabled_interface_toggles() {
        // Saving a retention edit rewrites the whole file; on-disk `mouse = false`
        // and `graphics = false` must both survive rather than reset to defaults
        // (#161, #209).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tuigram").join("settings.toml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let contents = format!(
            "{}{}",
            StorageSettings::default().render(),
            InterfaceSettings {
                mouse: false,
                graphics: false,
            }
            .render_section()
        );
        write_settings_file(&path, &contents).unwrap();

        // Emulate `save`'s composition (its path resolution is exercised elsewhere):
        // re-read the interface section and re-emit it after the edited storage.
        let interface: InterfaceSettings = {
            let text = std::fs::read_to_string(&path).unwrap();
            toml::from_str::<SettingsFile>(&text).unwrap().interface
        };
        assert!(!interface.mouse, "the disabled toggle must be read back");
        assert!(!interface.graphics, "the disabled toggle must be read back");
        let edited = StorageSettings {
            keep_groups: KeepMedia::Days(7),
            ..StorageSettings::default()
        };
        let rewritten = format!("{}{}", edited.render(), interface.render_section());
        write_settings_file(&path, &rewritten).unwrap();

        let file: SettingsFile = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(file.storage, edited);
        assert!(
            !file.interface.mouse,
            "mouse=false survived the storage save"
        );
        assert!(
            !file.interface.graphics,
            "graphics=false survived the storage save"
        );
    }

    #[test]
    fn a_saved_interface_edit_preserves_the_storage_policy_on_disk() {
        // The inverse direction (#209): confirming the graphics toggle rewrites the
        // whole file through `InterfaceSettings::save`'s composition, which must
        // not clobber a non-default retention policy already on disk.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tuigram").join("settings.toml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let storage = StorageSettings {
            keep_channels: KeepMedia::Days(3),
            ..StorageSettings::default()
        };
        let contents = format!(
            "{}{}",
            storage.render(),
            InterfaceSettings::default().render_section()
        );
        write_settings_file(&path, &contents).unwrap();

        // Emulate `InterfaceSettings::save`'s composition: re-read the current
        // storage section and re-emit it after the edited interface toggles.
        let on_disk_storage: StorageSettings = {
            let text = std::fs::read_to_string(&path).unwrap();
            toml::from_str::<SettingsFile>(&text).unwrap().storage
        };
        let edited_interface = InterfaceSettings {
            mouse: true,
            graphics: false,
        };
        let rewritten = format!(
            "{}{}",
            on_disk_storage.render(),
            edited_interface.render_section()
        );
        write_settings_file(&path, &rewritten).unwrap();

        let file: SettingsFile = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(file.storage, storage, "the retention policy survived");
        assert_eq!(file.interface, edited_interface);
    }

    #[test]
    fn render_round_trips_through_load() {
        // The rendered file must parse back to the exact policy it came from — both the
        // default and a fully non-default one — so first-run and later rewrites are
        // lossless. Round-trip is by meaning: a week reads back as its day-equivalent.
        for original in [
            StorageSettings::default(),
            StorageSettings {
                keep_private: KeepMedia::Days(3),
                keep_groups: KeepMedia::Days(7),
                keep_channels: KeepMedia::Forever,
                max_cache: CacheCap::Bytes(2 * BYTES_PER_GB),
            },
        ] {
            let parsed: SettingsFile =
                toml::from_str(&original.render()).expect("rendered settings must parse");
            assert_eq!(parsed.storage, original);
        }
    }

    #[test]
    fn init_default_writes_the_default_when_absent_and_creates_the_dir() {
        // First run: no file yet, so a default one is written under a freshly created
        // config dir and parses back to the default policy.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tuigram").join("settings.toml");
        assert!(init_default_at(&path).unwrap(), "should report it wrote");
        assert!(path.exists());
        let text = std::fs::read_to_string(&path).unwrap();
        let file: SettingsFile = toml::from_str(&text).unwrap();
        assert_eq!(file.storage, StorageSettings::default());
    }

    #[test]
    fn init_default_never_overwrites_an_existing_file() {
        // A settings file already there — even a hand-edited or malformed one — is left
        // byte-for-byte untouched, and the call reports it did not write.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        let existing = "[storage]\nkeep_channels = \"3d\"\n# my notes\n";
        std::fs::write(&path, existing).unwrap();
        assert!(
            !init_default_at(&path).unwrap(),
            "should not write over a file"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), existing);
    }

    #[test]
    fn a_saved_edit_overwrites_the_prior_file_through_render() {
        // The in-app editor's save (#146) replaces the file wholesale with the edited
        // policy's rendered form — comments and all — over whatever was there, and it
        // reads back to exactly the edit. (`save` itself resolves the config path and
        // delegates here; this exercises the overwrite-and-round-trip that matters.)
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        write_settings_file(&path, &StorageSettings::default().render()).unwrap();
        let edited = StorageSettings {
            keep_private: KeepMedia::Forever,
            keep_groups: KeepMedia::Days(7),
            keep_channels: KeepMedia::Days(3),
            max_cache: CacheCap::Bytes(2 * BYTES_PER_GB),
        };
        write_settings_file(&path, &edited.render()).unwrap();
        let file: SettingsFile = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(file.storage, edited);
    }

    #[cfg(unix)]
    #[test]
    fn a_written_settings_file_is_world_readable_644() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        write_settings_file(&path, "[storage]\n").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644, "settings are non-secret preferences, not 600");
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
