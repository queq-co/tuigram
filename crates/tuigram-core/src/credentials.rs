//! Resolves the Telegram `api_id` / `api_hash` a `TDLib` login requires.
//!
//! Telegram ties these to a developer's own app registration and rate-limits the
//! published *sample* id (`API_ID_PUBLISHED_FLOOD`), so a FOSS client must never
//! ship a shared credential — see `docs/research/app-registration-security.md`.
//! Each user supplies their own, resolved here in precedence order (first hit
//! wins):
//!
//! 1. the `TUIGRAM_API_ID` / `TUIGRAM_API_HASH` environment variables;
//! 2. `~/.config/tuigram/config.toml` (written `600`);
//! 3. first-run interactive onboarding — capture the two values and persist them
//!    to that config, once.
//!
//! The interactive step is the [`Onboarding`] seam: the prompt copy (the "why"
//! and the my.telegram.org walkthrough) lives in the login harness (#9), while
//! the precedence and on-disk handling here stay unit-testable. The resolver
//! takes its config path and env values by value, so tests drive every branch
//! without mutating process-global env.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tdlib_rs::types::Error as TdError;

/// Environment variable holding the Telegram `api_id`.
pub const ENV_API_ID: &str = "TUIGRAM_API_ID";
/// Environment variable holding the Telegram `api_hash`.
pub const ENV_API_HASH: &str = "TUIGRAM_API_HASH";

/// `TDLib` error message when the `api_id` in use is the rate-limited public
/// sample — the failure mode the per-user-credentials policy exists to avoid.
pub const API_ID_PUBLISHED_FLOOD: &str = "API_ID_PUBLISHED_FLOOD";

/// A user's Telegram application credentials, from <https://my.telegram.org>.
///
/// Low-value relative to the `TDLib` session (which is live account access), but
/// still never committed to git. These map straight onto the `api_id`/`api_hash`
/// fields of [`crate::ClientParameters`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiCredentials {
    /// Telegram API id (a positive integer).
    pub api_id: i32,
    /// Telegram API hash (a hex string).
    pub api_hash: String,
}

/// Interactive first-run capture of freshly-registered credentials.
///
/// The only side of resolution that talks to the user; implemented by the login
/// harness (#9), which explains *why* and links to my.telegram.org. Kept as a
/// trait so [`CredentialResolver::resolve`] is testable with a scripted stand-in.
pub trait Onboarding {
    /// Prompt for and return the user's `api_id` / `api_hash`.
    ///
    /// # Errors
    ///
    /// Returns an error if the prompt is cancelled, fails to read input, or the
    /// captured values don't parse into valid credentials.
    fn capture(&self) -> Result<ApiCredentials, CredentialError>;
}

/// Resolves credentials from env, then config, then onboarding.
///
/// Construct with [`CredentialResolver::from_environment`] for real use, or
/// [`CredentialResolver::new`] to supply an explicit config path and env values
/// (tests, or a caller managing its own configuration roots).
pub struct CredentialResolver {
    config_path: PathBuf,
    api_id_env: Option<String>,
    api_hash_env: Option<String>,
}

impl CredentialResolver {
    /// Build a resolver from the process environment: the `TUIGRAM_*` variables
    /// and the XDG config path `~/.config/tuigram/config.toml`.
    ///
    /// # Errors
    /// Fails only if neither `XDG_CONFIG_HOME` nor `HOME` is set, so no config
    /// location can be determined.
    pub fn from_environment() -> Result<Self, CredentialError> {
        Ok(Self {
            config_path: default_config_path()?,
            api_id_env: env_var(ENV_API_ID),
            api_hash_env: env_var(ENV_API_HASH),
        })
    }

    /// Construct with an explicit config path and pre-read env values.
    #[must_use]
    pub fn new(
        config_path: PathBuf,
        api_id_env: Option<String>,
        api_hash_env: Option<String>,
    ) -> Self {
        Self {
            config_path,
            api_id_env,
            api_hash_env,
        }
    }

    /// The config file this resolver reads from and writes onboarding results to.
    #[must_use]
    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    /// Resolve credentials, falling through env → config → onboarding.
    ///
    /// Onboarding runs only when neither env nor config supplies a value, and its
    /// result is persisted to the config (perms `600`) so it is captured once.
    ///
    /// # Errors
    /// Returns [`CredentialError`] if env/config values are present but malformed,
    /// if the config cannot be read or written, or if onboarding fails.
    pub fn resolve<O: Onboarding>(
        &self,
        onboarding: &O,
    ) -> Result<ApiCredentials, CredentialError> {
        if let Some(creds) = self.env_credentials()? {
            return Ok(creds);
        }
        if let Some(creds) = self.config_credentials()? {
            return Ok(creds);
        }
        let creds = onboarding.capture()?;
        self.persist(&creds)?;
        Ok(creds)
    }

    /// Read credentials from the `TUIGRAM_*` env vars, if present.
    ///
    /// Both or neither: one without the other is a misconfiguration we surface
    /// rather than silently fall through, since the user clearly intended env.
    fn env_credentials(&self) -> Result<Option<ApiCredentials>, CredentialError> {
        match (self.api_id_env.as_deref(), self.api_hash_env.as_deref()) {
            (None, None) => Ok(None),
            (Some(id), Some(hash)) => Ok(Some(validate_str(id, hash, "environment")?)),
            (Some(_), None) => Err(CredentialError::Malformed(format!(
                "{ENV_API_ID} is set but {ENV_API_HASH} is not; set both or neither"
            ))),
            (None, Some(_)) => Err(CredentialError::Malformed(format!(
                "{ENV_API_HASH} is set but {ENV_API_ID} is not; set both or neither"
            ))),
        }
    }

    /// Read credentials from the config file, if it exists and has them.
    fn config_credentials(&self) -> Result<Option<ApiCredentials>, CredentialError> {
        let text = match fs::read_to_string(&self.config_path) {
            Ok(text) => text,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(CredentialError::Io(err)),
        };
        let config: ConfigFile = toml::from_str(&text).map_err(|err| {
            CredentialError::Malformed(format!("{}: {err}", self.config_path.display()))
        })?;
        match config.telegram {
            None => Ok(None),
            Some(t) => Ok(Some(validate(t.api_id, &t.api_hash, "config.toml")?)),
        }
    }

    /// Write credentials to the config, creating the directory (`700`) and file
    /// (`600`) with owner-only permissions.
    fn persist(&self, creds: &ApiCredentials) -> Result<(), CredentialError> {
        if let Some(parent) = self.config_path.parent() {
            fs::create_dir_all(parent)?;
            set_dir_private(parent)?;
        }
        let config = ConfigFile {
            telegram: Some(TelegramSection {
                api_id: creds.api_id,
                api_hash: creds.api_hash.clone(),
            }),
        };
        let text =
            toml::to_string(&config).map_err(|err| CredentialError::Malformed(err.to_string()))?;
        write_private(&self.config_path, &text)?;
        Ok(())
    }
}

/// Whether a `TDLib` error is the published-sample-id flood condition.
#[must_use]
pub fn is_api_id_published_flood(error: &TdError) -> bool {
    error.message.contains(API_ID_PUBLISHED_FLOOD)
}

/// On-disk config schema. Credentials sit under `[telegram]`, leaving room for
/// other sections later without disturbing them.
#[derive(Debug, Default, Serialize, Deserialize)]
struct ConfigFile {
    #[serde(default)]
    telegram: Option<TelegramSection>,
}

#[derive(Debug, Serialize, Deserialize)]
struct TelegramSection {
    api_id: i32,
    api_hash: String,
}

/// Validate already-typed config values into [`ApiCredentials`].
fn validate(api_id: i32, api_hash: &str, source: &str) -> Result<ApiCredentials, CredentialError> {
    if api_id <= 0 {
        return Err(CredentialError::Malformed(format!(
            "{source} api_id must be a positive integer, got {api_id}"
        )));
    }
    let api_hash = api_hash.trim().to_owned();
    if api_hash.is_empty() {
        return Err(CredentialError::Malformed(format!(
            "{source} api_hash is empty"
        )));
    }
    Ok(ApiCredentials { api_id, api_hash })
}

/// Validate string-form values (the env path; `api_id` arrives as text).
fn validate_str(
    api_id: &str,
    api_hash: &str,
    source: &str,
) -> Result<ApiCredentials, CredentialError> {
    let api_id = api_id.trim().parse::<i32>().map_err(|_| {
        CredentialError::Malformed(format!(
            "{source} api_id must be a positive integer, got {api_id:?}"
        ))
    })?;
    validate(api_id, api_hash, source)
}

/// `$XDG_CONFIG_HOME/tuigram/config.toml`, falling back to `~/.config/...`.
fn default_config_path() -> Result<PathBuf, CredentialError> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|s| !s.is_empty())
                .map(|home| PathBuf::from(home).join(".config"))
        })
        .ok_or_else(|| {
            CredentialError::Malformed(
                "cannot locate config: neither XDG_CONFIG_HOME nor HOME is set".to_owned(),
            )
        })?;
    Ok(base.join("tuigram").join("config.toml"))
}

fn env_var(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

/// Write a file readable only by its owner. On unix the mode is applied both at
/// creation (no world-readable window) and afterwards (enforced even if the file
/// pre-existed with looser perms).
#[cfg(unix)]
fn write_private(path: &Path, contents: &str) -> io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(contents.as_bytes())?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private(path: &Path, contents: &str) -> io::Result<()> {
    fs::write(path, contents)
}

#[cfg(unix)]
fn set_dir_private(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_dir_private(_dir: &Path) -> io::Result<()> {
    Ok(())
}

/// Failure resolving credentials.
#[derive(Debug)]
pub enum CredentialError {
    /// Reading or writing the config file failed.
    Io(io::Error),
    /// A configured value was present but invalid (bad `api_id`, empty hash,
    /// partial env pair, unparseable TOML).
    Malformed(String),
    /// The interactive onboarding step failed (e.g. aborted, or EOF on input).
    Onboarding(String),
    /// Telegram rejected the `api_id` with `API_ID_PUBLISHED_FLOOD`: it is the
    /// rate-limited public sample and the user must register their own.
    PublishedApiIdFlood,
}

impl std::fmt::Display for CredentialError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(err) => write!(f, "credential config I/O error: {err}"),
            Self::Malformed(msg) => write!(f, "invalid credentials: {msg}"),
            Self::Onboarding(msg) => write!(f, "credential onboarding failed: {msg}"),
            Self::PublishedApiIdFlood => write!(
                f,
                "Telegram rejected the api_id with {API_ID_PUBLISHED_FLOOD}: this is the \
                 rate-limited public sample id and cannot be used by a released app. Register \
                 your own at https://my.telegram.org (API development tools) and set {ENV_API_ID} \
                 / {ENV_API_HASH} or add them to your config.toml."
            ),
        }
    }
}

impl std::error::Error for CredentialError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for CredentialError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ApiCredentials {
        ApiCredentials {
            api_id: 1_234_567,
            api_hash: "0123456789abcdef0123456789abcdef".to_owned(),
        }
    }

    /// Onboarding that yields scripted credentials.
    struct Scripted(ApiCredentials);
    impl Onboarding for Scripted {
        fn capture(&self) -> Result<ApiCredentials, CredentialError> {
            Ok(self.0.clone())
        }
    }

    /// Onboarding that must never be reached — fails the test loudly if it is.
    struct NeverPrompts;
    impl Onboarding for NeverPrompts {
        fn capture(&self) -> Result<ApiCredentials, CredentialError> {
            panic!("onboarding ran even though credentials were already available");
        }
    }

    fn config_path(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("tuigram").join("config.toml")
    }

    #[test]
    fn env_takes_precedence_over_config_and_onboarding() {
        let dir = tempfile::tempdir().unwrap();
        let path = config_path(&dir);
        // Seed a *different* config to prove env wins over it.
        CredentialResolver::new(path.clone(), None, None)
            .persist(&ApiCredentials {
                api_id: 999,
                api_hash: "fromconfig".to_owned(),
            })
            .unwrap();

        let resolver = CredentialResolver::new(
            path,
            Some("1234567".to_owned()),
            Some("0123456789abcdef0123456789abcdef".to_owned()),
        );
        assert_eq!(resolver.resolve(&NeverPrompts).unwrap(), sample());
    }

    #[test]
    fn config_used_when_env_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = config_path(&dir);
        CredentialResolver::new(path.clone(), None, None)
            .persist(&sample())
            .unwrap();

        let resolver = CredentialResolver::new(path, None, None);
        assert_eq!(resolver.resolve(&NeverPrompts).unwrap(), sample());
    }

    #[test]
    fn onboarding_runs_then_persists_when_nothing_configured() {
        let dir = tempfile::tempdir().unwrap();
        let path = config_path(&dir);

        let first = CredentialResolver::new(path.clone(), None, None);
        assert_eq!(first.resolve(&Scripted(sample())).unwrap(), sample());

        // The captured values are now on disk: a second resolve reads them from
        // config and never touches onboarding again.
        let second = CredentialResolver::new(path, None, None);
        assert_eq!(second.resolve(&NeverPrompts).unwrap(), sample());
    }

    #[cfg(unix)]
    #[test]
    fn persisted_config_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = config_path(&dir);
        let resolver = CredentialResolver::new(path.clone(), None, None);
        resolver.resolve(&Scripted(sample())).unwrap();

        let file_mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(file_mode, 0o600, "config file must be 600");

        let dir_mode = fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700, "config directory must be 700");
    }

    #[test]
    fn partial_env_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = config_path(&dir);
        let resolver = CredentialResolver::new(path, Some("1234567".to_owned()), None);
        assert!(matches!(
            resolver.resolve(&NeverPrompts),
            Err(CredentialError::Malformed(_))
        ));
    }

    #[test]
    fn non_numeric_env_api_id_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = config_path(&dir);
        let resolver = CredentialResolver::new(
            path,
            Some("not-a-number".to_owned()),
            Some("hash".to_owned()),
        );
        assert!(matches!(
            resolver.resolve(&NeverPrompts),
            Err(CredentialError::Malformed(_))
        ));
    }

    #[test]
    fn published_api_id_flood_is_detected_and_explained() {
        let flood = TdError {
            code: 406,
            message: "API_ID_PUBLISHED_FLOOD".to_owned(),
        };
        assert!(is_api_id_published_flood(&flood));

        let other = TdError {
            code: 400,
            message: "PHONE_NUMBER_INVALID".to_owned(),
        };
        assert!(!is_api_id_published_flood(&other));

        // The actionable error tells the user exactly what to do.
        let rendered = CredentialError::PublishedApiIdFlood.to_string();
        assert!(rendered.contains("my.telegram.org"));
        assert!(rendered.contains(API_ID_PUBLISHED_FLOOD));
    }
}
