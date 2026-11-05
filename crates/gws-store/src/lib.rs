//! The plugin's local store: SQLCipher in `<data dir>/workspace.db`, keyed by
//! a 256-bit raw key in a 0600 file beside it, migrated forward on open.
//! Everything here is the person's Google data, so the store refuses to open
//! without encryption. It holds no tokens: those stay in the Bot's keystore
//! and in the plugin's memory.

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension as _, params};

pub const DATA_DIR_ENV: &str = "GWS_DATA_DIR";
const DB_FILE: &str = "workspace.db";
const KEY_FILE: &str = "workspace.key";
/// Where open Drive files and attachments are kept while the person works on them.
const OPEN_DIR: &str = "open";

/// The schema, oldest first. A released migration never changes.
const MIGRATIONS: &[(i64, &str)] = &[(
    1,
    "CREATE TABLE state(account TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL, PRIMARY KEY(account, key));",
)];

/// Why the store didn't open or answer: a stable `code` and a message for people.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub code: &'static str,
    pub message: String,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for Error {}

fn error(code: &'static str, message: &str) -> Error {
    Error { code, message: message.to_owned() }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        // SQLite's text can quote what it was given, so it stays out of the message
        let _ = e;
        error("store", "The local mail store could not do that.")
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Where the store's files live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub dir: PathBuf,
}

impl Paths {
    pub fn in_dir(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// `GWS_DATA_DIR`, else `~/Library/Application Support/app.buwiz.google-workspace`.
    pub fn from_env() -> Self {
        let dir = std::env::var_os(DATA_DIR_ENV).filter(|d| !d.is_empty()).map(PathBuf::from).unwrap_or_else(|| {
            let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
            home.join("Library/Application Support/app.buwiz.google-workspace")
        });
        Self { dir }
    }

    pub fn db(&self) -> PathBuf {
        self.dir.join(DB_FILE)
    }

    pub fn key_file(&self) -> PathBuf {
        self.dir.join(KEY_FILE)
    }

    pub fn open_dir(&self) -> PathBuf {
        self.dir.join(OPEN_DIR)
    }
}

/// The key file's 64 hex digits, making it (0600) when absent.
fn key(path: &Path) -> Result<String> {
    match fs::read_to_string(path) {
        Ok(text) => {
            let text = text.trim().to_owned();
            if text.len() != 64 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(error("bad_key", "The store's key file is damaged."));
            }
            if fs::metadata(path).map(|m| m.permissions().mode() & 0o077 != 0).unwrap_or(true) {
                return Err(error("key_open", "The store's key file is readable by others, so it was not used."));
            }
            Ok(text)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut bytes = [0u8; 32];
            getrandom::fill(&mut bytes).map_err(|_| error("no_random", "The system gave no random bytes."))?;
            let text: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
            let mut file = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path).map_err(|_| error("key_write", "The store's key file cannot be made."))?;
            file.write_all(text.as_bytes()).map_err(|_| error("key_write", "The store's key file cannot be written."))?;
            Ok(text)
        }
        Err(_) => Err(error("key_read", "The store's key file cannot be read.")),
    }
}

pub struct Store {
    conn: Connection,
}

impl Store {
    /// Open (creating when absent) the store at `paths`, migrated to the latest schema.
    pub fn open(paths: &Paths) -> Result<Self> {
        fs::DirBuilder::new().recursive(true).mode(0o700).create(&paths.dir).map_err(|_| error("data_dir", "The data folder cannot be made."))?;
        let key = key(&paths.key_file())?;
        let conn = Connection::open(paths.db())?;
        // SQLCipher reads a raw key as x'<64 hex>' (no key derivation)
        conn.execute_batch(&format!("PRAGMA key = \"x'{key}'\";"))?;
        let cipher: Option<String> = conn.query_row("PRAGMA cipher_version", [], |r| r.get(0)).optional()?;
        if cipher.filter(|c| !c.is_empty()).is_none() {
            return Err(error("no_cipher", "This build has no SQLCipher, so it will not keep mail it can't encrypt."));
        }
        if conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get::<_, i64>(0)).is_err() {
            return Err(error("wrong_key", "The store does not open with this key file."));
        }
        conn.execute_batch("PRAGMA foreign_keys = ON; PRAGMA journal_mode = DELETE;")?;
        let mut store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&mut self) -> Result<()> {
        let current: i64 = self.conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        let latest = MIGRATIONS.last().map_or(0, |m| m.0);
        if current > latest {
            return Err(error("schema_newer", "The store is from a newer plugin; update it to open this store."));
        }
        for (version, sql) in MIGRATIONS.iter().filter(|m| m.0 > current) {
            let tx = self.conn.transaction()?;
            tx.execute_batch(sql)?;
            tx.execute_batch(&format!("PRAGMA user_version = {version};"))?;
            tx.commit()?;
        }
        Ok(())
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// A small value kept for `account`: a sync cursor, a setting.
    pub fn state(&self, account: &str, key: &str) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT value FROM state WHERE account = ? AND key = ?", params![account, key], |r| r.get(0)).optional()?)
    }

    pub fn set_state(&self, account: &str, key: &str, value: &str) -> Result<()> {
        self.conn.execute("INSERT INTO state(account, key, value) VALUES(?, ?, ?) ON CONFLICT(account, key) DO UPDATE SET value = excluded.value", params![account, key, value])?;
        Ok(())
    }
}

/// Deletes every file the plugin keeps (`buwiz.uninstall`).
pub fn wipe(paths: &Paths) -> Result<()> {
    match fs::remove_dir_all(&paths.dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(error("wipe", "The data folder could not be removed.")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dir(PathBuf);

    impl Dir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("gws-store-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            Self(dir)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn what_it_keeps_survives_a_reopen_and_is_encrypted_on_disk() {
        let dir = Dir::new("enc");
        let paths = Paths::in_dir(&dir.0);
        let secret = "fake-subject-line-to-find-on-disk";
        Store::open(&paths).unwrap().set_state("me@example.com", "cursor", secret).unwrap();
        assert_eq!(Store::open(&paths).unwrap().state("me@example.com", "cursor").unwrap().as_deref(), Some(secret));
        let bytes = fs::read(paths.db()).unwrap();
        assert!(!bytes.starts_with(b"SQLite format 3"), "an encrypted file has no SQLite header");
        assert!(!bytes.windows(secret.len()).any(|w| w == secret.as_bytes()), "the value is not in the file");
    }

    #[test]
    fn the_key_file_is_private_and_a_loose_one_is_refused() {
        let dir = Dir::new("key");
        let paths = Paths::in_dir(&dir.0);
        drop(Store::open(&paths).unwrap());
        let key_file = paths.key_file();
        assert_eq!(fs::metadata(&key_file).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::metadata(&paths.dir).unwrap().permissions().mode() & 0o777, 0o700);
        fs::set_permissions(&key_file, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(Store::open(&paths).err().unwrap().code, "key_open");
    }

    #[test]
    fn another_key_does_not_open_it() {
        let dir = Dir::new("wrong");
        let paths = Paths::in_dir(&dir.0);
        drop(Store::open(&paths).unwrap());
        fs::write(paths.key_file(), "ab".repeat(32)).unwrap();
        assert_eq!(Store::open(&paths).err().unwrap().code, "wrong_key");
    }

    #[test]
    fn a_newer_store_is_refused_and_wipe_removes_everything() {
        let dir = Dir::new("newer");
        let paths = Paths::in_dir(&dir.0);
        let store = Store::open(&paths).unwrap();
        store.conn().execute_batch("PRAGMA user_version = 9999;").unwrap();
        drop(store);
        assert_eq!(Store::open(&paths).err().unwrap().code, "schema_newer");
        wipe(&paths).unwrap();
        assert!(!paths.dir.exists());
        wipe(&paths).unwrap();
    }
}
