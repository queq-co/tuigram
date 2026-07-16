//! Secure storage for the high-value asset: the `TDLib` session/database.
//!
//! The `api_id`/`api_hash` ([`crate::credentials`]) are low-value; the `TDLib`
//! database is *live account access*. Per
//! `docs/research/app-registration-security.md` we keep it under the user's data
//! directory with owner-only permissions and encrypt it at rest:
//!
//! * data dir `$XDG_DATA_HOME/tuigram` (else `~/.local/share/tuigram`), `700`;
//! * `TDLib`'s optional **database encryption key** enabled — 32 bytes of CSPRNG
//!   entropy, hex-encoded;
//! * the key stored in the OS keyring (macOS Keychain / Windows Credential
//!   Manager / Linux Secret Service), **falling back to a `600` key file** in the
//!   data dir where no keyring is reachable (headless Linux, CI, minimal hosts).
//!
//! These fill the `database_directory` / `files_directory` /
//! `database_encryption_key` fields of [`crate::ClientParameters`].
//!
//! ## Threat model
//!
//! This protects against **casual disk access** — a stolen laptop, a synced
//! backup, another local user reading `~/.local/share`. The encrypted database is
//! useless without the key, and the key sits in the OS credential store (or a
//! file only the owner can read).
//!
//! It does **not** protect against a **root-level attacker on the same machine**:
//! root can read the keyring, the key file, and this process's memory. Defending
//! against that is out of scope — it requires a hardware token or a passphrase the
//! user types every launch, neither of which this client asks for.
//!
//! The key is never logged: [`EncryptionKey`] redacts itself in `Debug`, and no
//! error in this module embeds it.
//!
//! The storage backends sit behind the `KeyStore` seam so resolution
//! (keyring-then-file, generate-on-first-use) is unit-tested without touching the
//! real OS keyring.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Length of the database encryption key in bytes (256 bits of entropy).
const KEY_BYTES: usize = 32;
/// Keyring service name for the stored key.
const KEYRING_SERVICE: &str = "tuigram";
/// Keyring entry name (the "username" slot) for the stored key.
const KEYRING_ENTRY: &str = "database-encryption-key";
/// Name of the fallback key file inside the data dir.
const KEY_FILE_NAME: &str = "db_encryption_key";

/// The `TDLib` database encryption key: 32 bytes of CSPRNG entropy as a 64-char
/// lowercase hex string.
///
/// Hex (not raw bytes) so it round-trips cleanly through the keyring's string API
/// and a text key file, and maps onto the `String` `TDLib` expects. Treated as a
/// secret throughout: `Debug` is redacted and the value is exposed only via
/// [`EncryptionKey::expose`], at the point it is moved into the `TDLib` request.
#[derive(Clone, PartialEq, Eq)]
pub struct EncryptionKey(String);

impl EncryptionKey {
    /// Generate a fresh key from the operating system's CSPRNG.
    ///
    /// # Errors
    /// Fails if the OS random source is unavailable.
    pub fn generate() -> Result<Self, SessionError> {
        let mut bytes = [0u8; KEY_BYTES];
        getrandom::fill(&mut bytes).map_err(|err| SessionError::Rng(err.to_string()))?;
        Ok(Self(hex_encode(&bytes)))
    }

    /// Reconstruct a key from its stored hex form, rejecting anything that is not
    /// exactly [`KEY_BYTES`] of lowercase hex (a corrupt keyring entry or key
    /// file is surfaced, never silently used).
    fn from_stored(text: &str) -> Result<Self, SessionError> {
        let text = text.trim();
        let valid = text.len() == KEY_BYTES * 2
            && text
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if !valid {
            return Err(SessionError::CorruptKey);
        }
        Ok(Self(text.to_owned()))
    }

    /// The key as the hex string `TDLib`'s `database_encryption_key` expects.
    ///
    /// Call this only when handing the key to `TDLib` — never log the result.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for EncryptionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never render the secret, even in logs/panics that format it.
        f.write_str("EncryptionKey(<redacted>)")
    }
}

/// Resolved, owner-only locations and key for a `TDLib` client's persistent state.
///
/// Build with [`SessionStorage::open`] for real use; `SessionStorage::open_at`
/// takes an explicit data dir and keyring seam for tests.
pub struct SessionStorage {
    data_dir: PathBuf,
    key: EncryptionKey,
}

impl SessionStorage {
    /// Open (creating if needed) the user's session storage and resolve its
    /// encryption key from the OS keyring, falling back to a `600` key file.
    ///
    /// The data dir is created `700`; the key is generated on first use and
    /// persisted so subsequent launches reuse it.
    ///
    /// # Errors
    /// Returns [`SessionError`] if the data dir cannot be located or created, the
    /// keyring is reachable but errors, the key file cannot be read/written, or
    /// the OS random source is unavailable.
    pub fn open() -> Result<Self, SessionError> {
        Self::open_at(default_data_dir()?, &KeyringStore::new(KEYRING_ENTRY))
    }

    /// Open session storage at an explicit data dir, using `keyring` as the
    /// primary key store and a `600` file inside the dir as the fallback.
    fn open_at(data_dir: PathBuf, keyring: &dyn KeyStore) -> Result<Self, SessionError> {
        fs::create_dir_all(&data_dir)?;
        set_dir_private(&data_dir)?;

        let fallback = FileStore::new(data_dir.join(KEY_FILE_NAME));
        let key = resolve_key(keyring, &fallback)?;

        Ok(Self { data_dir, key })
    }

    /// Directory `TDLib` uses for its persistent database.
    #[must_use]
    pub fn database_directory(&self) -> String {
        self.data_dir
            .join("database")
            .to_string_lossy()
            .into_owned()
    }

    /// Directory `TDLib` uses for downloaded files.
    #[must_use]
    pub fn files_directory(&self) -> String {
        self.data_dir.join("files").to_string_lossy().into_owned()
    }

    /// The database encryption key. Hand straight to `TDLib`; never log it.
    #[must_use]
    pub fn encryption_key(&self) -> &EncryptionKey {
        &self.key
    }

    /// The data dir this session is rooted at.
    #[must_use]
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }
}

/// Where a key is persisted between launches: the OS keyring, or a file.
///
/// `load` distinguishes three outcomes: a stored key, *no* key yet
/// ([`Ok(None)`]), and a backend that is *not reachable on this machine*
/// ([`KeyStoreError::Unavailable`]) — the last is what triggers the file
/// fallback, as opposed to a present-but-failing backend which is propagated.
trait KeyStore {
    /// The stored key, `Ok(None)` if none has been written yet.
    fn load(&self) -> Result<Option<EncryptionKey>, KeyStoreError>;
    /// Persist `key`, replacing any existing value.
    fn store(&self, key: &EncryptionKey) -> Result<(), KeyStoreError>;
}

/// Get the key from `primary`, generating and persisting one on first use; if the
/// primary backend is unreachable, do the same against `fallback`.
fn resolve_key(
    primary: &dyn KeyStore,
    fallback: &dyn KeyStore,
) -> Result<EncryptionKey, SessionError> {
    match primary.load() {
        Ok(Some(key)) => Ok(key),
        Ok(None) => get_or_create(primary),
        Err(KeyStoreError::Unavailable) => get_or_create(fallback),
        Err(KeyStoreError::Backend(msg)) => Err(SessionError::Keyring(msg)),
        Err(KeyStoreError::Io(err)) => Err(SessionError::Io(err)),
        Err(KeyStoreError::Corrupt) => Err(SessionError::CorruptKey),
    }
}

/// Return the store's key, or generate one, persist it, and return that.
fn get_or_create(store: &dyn KeyStore) -> Result<EncryptionKey, SessionError> {
    if let Some(key) = store.load().map_err(SessionError::from)? {
        return Ok(key);
    }
    let key = EncryptionKey::generate()?;
    store.store(&key).map_err(SessionError::from)?;
    Ok(key)
}

/// `$XDG_DATA_HOME/tuigram`, falling back to `~/.local/share/tuigram`.
fn default_data_dir() -> Result<PathBuf, SessionError> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|s| !s.is_empty())
                .map(|home| PathBuf::from(home).join(".local").join("share"))
        })
        .ok_or(SessionError::NoDataDir)?;
    Ok(base.join("tuigram"))
}

/// Lowercase-hex encode a byte slice without pulling in a hex crate.
fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// Key store backed by the OS keyring via the `keyring` crate.
struct KeyringStore {
    entry: &'static str,
}

impl KeyringStore {
    fn new(entry: &'static str) -> Self {
        Self { entry }
    }
}

impl KeyStore for KeyringStore {
    fn load(&self) -> Result<Option<EncryptionKey>, KeyStoreError> {
        let entry = keyring::Entry::new(KEYRING_SERVICE, self.entry).map_err(map_keyring_err)?;
        match entry.get_password() {
            Ok(text) => Ok(Some(EncryptionKey::from_stored(&text)?)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(err) => Err(map_keyring_err(err)),
        }
    }

    fn store(&self, key: &EncryptionKey) -> Result<(), KeyStoreError> {
        let entry = keyring::Entry::new(KEYRING_SERVICE, self.entry).map_err(map_keyring_err)?;
        entry.set_password(key.expose()).map_err(map_keyring_err)
    }
}

/// Map a `keyring` error to our coarser outcome. A backend that is simply *not
/// present* on this host (no Secret Service daemon, no platform support) becomes
/// [`KeyStoreError::Unavailable`] so the file fallback kicks in; a backend that is
/// present but failing is propagated.
fn map_keyring_err(err: keyring::Error) -> KeyStoreError {
    match err {
        keyring::Error::NoEntry
        | keyring::Error::NoStorageAccess(_)
        | keyring::Error::PlatformFailure(_)
        | keyring::Error::NotSupportedByStore(_) => KeyStoreError::Unavailable,
        other => KeyStoreError::Backend(other.to_string()),
    }
}

/// Key store backed by a `600` file holding the hex key.
struct FileStore {
    path: PathBuf,
}

impl FileStore {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl KeyStore for FileStore {
    fn load(&self) -> Result<Option<EncryptionKey>, KeyStoreError> {
        match fs::read_to_string(&self.path) {
            Ok(text) => Ok(Some(EncryptionKey::from_stored(&text)?)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(KeyStoreError::Io(err)),
        }
    }

    fn store(&self, key: &EncryptionKey) -> Result<(), KeyStoreError> {
        write_private(&self.path, key.expose()).map_err(KeyStoreError::Io)
    }
}

/// Outcome of a [`KeyStore`] operation, distinguishing an unreachable backend
/// (triggers fallback) from a real failure (propagated).
enum KeyStoreError {
    /// The backend is not reachable on this host — fall back to the next store.
    Unavailable,
    /// The backend is present but the operation failed.
    Backend(String),
    /// Filesystem error reading/writing a key file.
    Io(io::Error),
    /// A stored key was malformed.
    Corrupt,
}

impl From<SessionError> for KeyStoreError {
    fn from(_: SessionError) -> Self {
        // Only `from_stored` produces a SessionError inside a KeyStore, and only
        // the corrupt-key case.
        Self::Corrupt
    }
}

impl From<KeyStoreError> for SessionError {
    fn from(err: KeyStoreError) -> Self {
        match err {
            KeyStoreError::Unavailable => {
                Self::Keyring("key store unexpectedly unavailable".to_owned())
            }
            KeyStoreError::Backend(msg) => Self::Keyring(msg),
            KeyStoreError::Io(err) => Self::Io(err),
            KeyStoreError::Corrupt => Self::CorruptKey,
        }
    }
}

/// Write a file readable only by its owner (`600`). On unix the mode is applied
/// at creation (no world-readable window) and re-enforced afterwards in case the
/// file pre-existed with looser perms.
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

// Must return `io::Result<()>` to match the `unix` sibling above: callers are
// platform-agnostic and use `?` regardless of which cfg arm compiles.
#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)]
fn set_dir_private(_dir: &Path) -> io::Result<()> {
    Ok(())
}

/// Failure opening session storage or resolving its encryption key.
#[derive(Debug)]
pub enum SessionError {
    /// Neither `XDG_DATA_HOME` nor `HOME` is set, so no data dir can be located.
    NoDataDir,
    /// Creating the data dir or reading/writing the key file failed.
    Io(io::Error),
    /// The OS keyring was reachable but returned an error.
    Keyring(String),
    /// A persisted key was not valid hex of the expected length.
    CorruptKey,
    /// The OS random source was unavailable when generating a key.
    Rng(String),
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoDataDir => write!(
                f,
                "cannot locate session storage: neither XDG_DATA_HOME nor HOME is set"
            ),
            Self::Io(err) => write!(f, "session storage I/O error: {err}"),
            Self::Keyring(msg) => write!(f, "OS keyring error: {msg}"),
            Self::CorruptKey => write!(
                f,
                "stored database encryption key is malformed; remove it to regenerate \
                 (this re-encrypts the local database from scratch)"
            ),
            Self::Rng(msg) => write!(f, "could not generate encryption key: {msg}"),
        }
    }
}

impl std::error::Error for SessionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for SessionError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// In-memory [`KeyStore`] standing in for the OS keyring: holds a key, can be
    /// toggled "unavailable" to drive the file-fallback path, and counts writes so
    /// tests can assert the key was generated exactly once.
    struct MockKeyStore {
        available: bool,
        slot: RefCell<Option<EncryptionKey>>,
        stores: RefCell<usize>,
    }

    impl MockKeyStore {
        fn available() -> Self {
            Self {
                available: true,
                slot: RefCell::new(None),
                stores: RefCell::new(0),
            }
        }

        fn unavailable() -> Self {
            Self {
                available: false,
                slot: RefCell::new(None),
                stores: RefCell::new(0),
            }
        }

        fn with_key(key: EncryptionKey) -> Self {
            Self {
                available: true,
                slot: RefCell::new(Some(key)),
                stores: RefCell::new(0),
            }
        }
    }

    impl KeyStore for MockKeyStore {
        fn load(&self) -> Result<Option<EncryptionKey>, KeyStoreError> {
            if !self.available {
                return Err(KeyStoreError::Unavailable);
            }
            Ok(self.slot.borrow().clone())
        }

        fn store(&self, key: &EncryptionKey) -> Result<(), KeyStoreError> {
            if !self.available {
                return Err(KeyStoreError::Unavailable);
            }
            *self.slot.borrow_mut() = Some(key.clone());
            *self.stores.borrow_mut() += 1;
            Ok(())
        }
    }

    #[test]
    fn generated_key_is_64_hex_chars() {
        let key = EncryptionKey::generate().unwrap();
        assert_eq!(key.expose().len(), KEY_BYTES * 2);
        assert!(key.expose().bytes().all(|b| b.is_ascii_hexdigit()));
    }

    #[test]
    fn debug_redacts_the_key() {
        let key = EncryptionKey::generate().unwrap();
        let shown = format!("{key:?}");
        assert!(!shown.contains(key.expose()), "Debug must not leak the key");
        assert!(shown.contains("redacted"));
    }

    #[test]
    fn corrupt_stored_key_is_rejected() {
        assert!(EncryptionKey::from_stored("not-hex").is_err());
        assert!(EncryptionKey::from_stored("abcd").is_err()); // too short
        assert!(EncryptionKey::from_stored(&"g".repeat(64)).is_err()); // non-hex
        assert!(EncryptionKey::from_stored(&"a".repeat(64)).is_ok());
    }

    #[test]
    fn key_generated_and_stored_in_keyring_on_first_use() {
        let dir = tempfile::tempdir().unwrap();
        let keyring = MockKeyStore::available();
        let session = SessionStorage::open_at(dir.path().to_path_buf(), &keyring).unwrap();

        // Stored in the keyring exactly once...
        assert_eq!(*keyring.stores.borrow(), 1);
        assert_eq!(
            keyring.slot.borrow().as_ref().unwrap().expose(),
            session.encryption_key().expose()
        );
        // ...and the file fallback was never touched.
        assert!(!dir.path().join(KEY_FILE_NAME).exists());
    }

    #[test]
    fn existing_keyring_key_is_reused() {
        let dir = tempfile::tempdir().unwrap();
        let existing = EncryptionKey::generate().unwrap();
        let keyring = MockKeyStore::with_key(existing.clone());

        let session = SessionStorage::open_at(dir.path().to_path_buf(), &keyring).unwrap();

        assert_eq!(session.encryption_key().expose(), existing.expose());
        assert_eq!(
            *keyring.stores.borrow(),
            0,
            "must not overwrite an existing key"
        );
    }

    #[test]
    fn falls_back_to_key_file_when_keyring_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let keyring = MockKeyStore::unavailable();

        let session = SessionStorage::open_at(dir.path().to_path_buf(), &keyring).unwrap();
        let key_file = dir.path().join(KEY_FILE_NAME);
        assert!(
            key_file.exists(),
            "key must be written to the fallback file"
        );

        // The file holds exactly the resolved key, and a second open reuses it.
        let second =
            SessionStorage::open_at(dir.path().to_path_buf(), &MockKeyStore::unavailable())
                .unwrap();
        assert_eq!(
            session.encryption_key().expose(),
            second.encryption_key().expose()
        );
    }

    #[cfg(unix)]
    #[test]
    fn data_dir_is_700_and_key_file_is_600() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        // Nest one level so the dir we manage is created (not the pre-made tempdir).
        let data_dir = dir.path().join("tuigram");
        let keyring = MockKeyStore::unavailable(); // force the file path to exist
        SessionStorage::open_at(data_dir.clone(), &keyring).unwrap();

        let dir_mode = fs::metadata(&data_dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "data dir must be 700");

        let file_mode = fs::metadata(data_dir.join(KEY_FILE_NAME))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(file_mode, 0o600, "key file must be 600");
    }

    #[test]
    fn corrupt_key_file_is_surfaced_not_used() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::new(dir.path().join("k"));
        fs::write(dir.path().join("k"), "garbage").unwrap();
        assert!(matches!(store.load(), Err(KeyStoreError::Corrupt)));
    }

    #[test]
    fn directories_sit_under_the_data_dir() {
        let dir = tempfile::tempdir().unwrap();
        let session =
            SessionStorage::open_at(dir.path().to_path_buf(), &MockKeyStore::available()).unwrap();
        assert!(
            session
                .database_directory()
                .starts_with(dir.path().to_str().unwrap())
        );
        assert!(
            session
                .files_directory()
                .starts_with(dir.path().to_str().unwrap())
        );
        assert_ne!(session.database_directory(), session.files_directory());
    }
}
