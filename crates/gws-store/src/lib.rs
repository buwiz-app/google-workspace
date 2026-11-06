//! The plugin's local store: SQLCipher in `<data dir>/workspace-<id>.db`,
//! migrated forward on open. Everything here is the person's Google data, so the store
//! refuses to open without encryption. It holds no tokens: those stay in the
//! Bot's keystore and in the plugin's memory.
//!
//! The key is on no disk of ours. [`key_from_seed`] derives it from the plugin
//! key the Bot's keystore holds and provisions at every start, so a backup or
//! a copy of the data folder carries the lock without the key. The file's name
//! comes from the key too, so each install has a store of its own and a new
//! plugin key finds a new file: nothing ever has to delete a store to go on.

use std::fs;
use std::os::unix::fs::DirBuilderExt as _;
use std::path::PathBuf;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension as _, params};
use sha2::{Digest as _, Sha256};

pub mod calendar;
pub mod drive;
pub mod mail;
pub mod open;

pub const DATA_DIR_ENV: &str = "GWS_DATA_DIR";
/// How long a locked store is waited for.
const BUSY_WAIT: Duration = Duration::from_secs(5);

/// The schema, oldest first. A released migration never changes.
const MIGRATIONS: &[(i64, &str)] = &[
    (1, "CREATE TABLE state(account TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL, PRIMARY KEY(account, key));"),
    (
        2,
        "CREATE TABLE labels(account TEXT NOT NULL, id TEXT NOT NULL, name TEXT NOT NULL, kind TEXT NOT NULL, unread INTEGER NOT NULL, total INTEGER NOT NULL, PRIMARY KEY(account, id));
         CREATE TABLE threads(account TEXT NOT NULL, id TEXT NOT NULL, subject TEXT NOT NULL, snippet TEXT NOT NULL, sender TEXT NOT NULL, address TEXT NOT NULL, at INTEGER NOT NULL, unread INTEGER NOT NULL, starred INTEGER NOT NULL, messages INTEGER NOT NULL, attachment INTEGER NOT NULL, PRIMARY KEY(account, id));
         CREATE INDEX threads_by_time ON threads(account, at DESC);
         CREATE TABLE thread_labels(account TEXT NOT NULL, thread TEXT NOT NULL, label TEXT NOT NULL, PRIMARY KEY(account, thread, label));
         CREATE INDEX threads_of_label ON thread_labels(account, label);
         CREATE TABLE bodies(account TEXT NOT NULL, thread TEXT NOT NULL, view TEXT NOT NULL, size INTEGER NOT NULL, used INTEGER NOT NULL, PRIMARY KEY(account, thread));",
    ),
    (
        3,
        "ALTER TABLE threads ADD COLUMN recipient TEXT NOT NULL DEFAULT '';",
    ),
    (
        4,
        "CREATE TABLE calendars(account TEXT NOT NULL, id TEXT NOT NULL, name TEXT NOT NULL, is_primary INTEGER NOT NULL, writable INTEGER NOT NULL, shown INTEGER NOT NULL, time_zone TEXT NOT NULL, sync_token TEXT NOT NULL DEFAULT '', PRIMARY KEY(account, id));
         CREATE TABLE events(account TEXT NOT NULL, calendar TEXT NOT NULL, id TEXT NOT NULL, series TEXT NOT NULL, title TEXT NOT NULL, place TEXT NOT NULL, notes TEXT NOT NULL, start_text TEXT NOT NULL, end_text TEXT NOT NULL, all_day INTEGER NOT NULL, start_ms INTEGER NOT NULL, end_ms INTEGER NOT NULL, start_date TEXT NOT NULL, end_date TEXT NOT NULL, status TEXT NOT NULL, response TEXT NOT NULL, guests INTEGER NOT NULL, meet INTEGER NOT NULL, etag TEXT NOT NULL, detail TEXT NOT NULL, PRIMARY KEY(account, calendar, id));
         CREATE INDEX events_by_time ON events(account, start_ms);
         CREATE INDEX events_of_series ON events(account, calendar, series);",
    ),
    (
        5,
        "CREATE TABLE drive_files(account TEXT NOT NULL, id TEXT NOT NULL, name TEXT NOT NULL, kind TEXT NOT NULL, mime TEXT NOT NULL, owner TEXT NOT NULL, mine INTEGER NOT NULL, changed TEXT NOT NULL, changed_ms INTEGER NOT NULL, size INTEGER, starred INTEGER NOT NULL, shared INTEGER NOT NULL, with_me INTEGER NOT NULL, trashed INTEGER NOT NULL, parents TEXT NOT NULL, PRIMARY KEY(account, id));
         CREATE TABLE drive_views(account TEXT NOT NULL, view TEXT NOT NULL, next TEXT NOT NULL, PRIMARY KEY(account, view));
         CREATE TABLE drive_listed(account TEXT NOT NULL, view TEXT NOT NULL, file TEXT NOT NULL, at INTEGER NOT NULL, PRIMARY KEY(account, view, file));
         CREATE INDEX drive_listed_in_order ON drive_listed(account, view, at);
         CREATE INDEX drive_listed_by_file ON drive_listed(account, file);",
    ),
    // a stored mail view has no message id on its attachments: fetched again, it does
    (
        6,
        "CREATE TABLE opened(id TEXT PRIMARY KEY, account TEXT NOT NULL, name TEXT NOT NULL, mime TEXT NOT NULL, google INTEGER NOT NULL, attachment INTEGER NOT NULL, hash TEXT NOT NULL, version TEXT NOT NULL, md5 TEXT NOT NULL, opened_ms INTEGER NOT NULL);
         DELETE FROM bodies;",
    ),
    // nor has it the part of each attachment, which is what opens one now
    (7, "DELETE FROM bodies;"),
];

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
    Error {
        code,
        message: message.to_owned(),
    }
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
    /// A folder that isn't absolute is refused: the working folder is nobody's store.
    pub fn from_env() -> Result<Self> {
        let data = std::env::var_os(DATA_DIR_ENV);
        // cargo leaves this on all a test starts, so a test that names no folder
        // stops here, not in the person's. A release never has this check.
        let tested = cfg!(debug_assertions) && std::env::var_os("CARGO_MANIFEST_DIR").is_some();
        if tested && data.as_ref().is_none_or(|d| d.is_empty()) {
            return Err(error(
                "data_dir",
                "Under cargo, GWS_DATA_DIR must name a folder.",
            ));
        }
        Self::from_vars(data, std::env::var_os("HOME"))
    }

    fn from_vars(
        data: Option<std::ffi::OsString>,
        home: Option<std::ffi::OsString>,
    ) -> Result<Self> {
        let dir = match (
            data.filter(|d| !d.is_empty()),
            home.filter(|h| !h.is_empty()),
        ) {
            (Some(data), _) => PathBuf::from(data),
            (None, Some(home)) => {
                PathBuf::from(home).join("Library/Application Support/app.buwiz.google-workspace")
            }
            (None, None) => {
                return Err(error(
                    "no_home",
                    "HOME is not set, so the plugin has no folder for its store.",
                ));
            }
        };
        if dir.is_absolute() {
            Ok(Self { dir })
        } else {
            Err(error(
                "data_dir",
                "The data folder must be an absolute path.",
            ))
        }
    }

    /// The store of the install holding `key`.
    pub fn db(&self, key: &str) -> PathBuf {
        self.dir.join(format!("workspace-{}.db", install_id(key)))
    }

    /// There when a signed-out account's mail couldn't be cleared yet. It says
    /// no more than that, so it can be written when the store can't.
    pub fn wipe_pending(&self) -> PathBuf {
        self.dir.join("wipe-pending")
    }

    /// Where this install keeps open Drive files and attachments, as
    /// `<id>/<name>` below it. An edit here can't be fetched again, so a sign-out
    /// leaves one, and only the person's own press or an uninstall removes it.
    pub fn open_dir(&self, key: &str) -> PathBuf {
        self.dir.join(format!("open-{}", install_id(key)))
    }
}

/// What names an install's files: says nothing about the key.
fn install_id(key: &str) -> String {
    let digest = Sha256::new()
        .chain_update(b"app.buwiz.google-workspace install id v1\0")
        .chain_update(key.as_bytes())
        .finalize();
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// The store's key: 64 hex digits from the provisioned plugin key, under a
/// label of its own so it is never the signing key itself.
pub fn key_from_seed(seed: &str) -> String {
    let digest = Sha256::new()
        .chain_update(b"app.buwiz.google-workspace store key v1\0")
        .chain_update(seed.as_bytes())
        .finalize();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

pub struct Store {
    conn: Connection,
}

impl Store {
    /// Open (creating when absent) this install's store, migrated to the
    /// latest schema. Nothing here deletes anything: a store that won't open
    /// stays as it is and the error says why.
    pub fn open(paths: &Paths, key: &str) -> Result<Self> {
        Self::open_waiting(paths, key, BUSY_WAIT)
    }

    fn open_waiting(paths: &Paths, key: &str, wait: Duration) -> Result<Self> {
        if key.len() != 64 || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(error("bad_key", "That is not a store key."));
        }
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&paths.dir)
            .map_err(|_| error("data_dir", "The data folder cannot be made."))?;
        let conn = Connection::open(paths.db(key))?;
        conn.busy_timeout(wait)?;
        // SQLCipher reads a raw key as x'<64 hex>' (no key derivation)
        conn.execute_batch(&format!("PRAGMA key = \"x'{key}'\";"))?;
        let cipher: Option<String> = conn
            .query_row("PRAGMA cipher_version", [], |r| r.get(0))
            .optional()?;
        if cipher.filter(|c| !c.is_empty()).is_none() {
            return Err(error(
                "no_cipher",
                "This build has no SQLCipher, so it will not keep mail it can't encrypt.",
            ));
        }
        match conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| {
            r.get::<_, i64>(0)
        }) {
            Ok(_) => {}
            // the one answer that means this key doesn't fit; a busy or full
            // disk is only that, whatever it might look like
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::NotADatabase =>
            {
                return Err(error("wrong_key", "The store does not open with this key."));
            }
            Err(e) => return Err(e.into()),
        }
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        conn.query_row("PRAGMA journal_mode = DELETE", [], |_| Ok(()))?;
        let mut store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&mut self) -> Result<()> {
        let current: i64 = self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))?;
        let latest = MIGRATIONS.last().map_or(0, |m| m.0);
        if current > latest {
            return Err(error(
                "schema_newer",
                "The store is from a newer plugin; update it to open this store.",
            ));
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
        Ok(self
            .conn
            .query_row(
                "SELECT value FROM state WHERE account = ? AND key = ?",
                params![account, key],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn set_state(&self, account: &str, key: &str, value: &str) -> Result<()> {
        self.conn.execute("INSERT INTO state(account, key, value) VALUES(?, ?, ?) ON CONFLICT(account, key) DO UPDATE SET value = excluded.value", params![account, key, value])?;
        Ok(())
    }
}

/// Deletes this install's files (`buwiz.uninstall`, which the person
/// confirmed), the store's side files with it, and the folder once it's empty.
/// Nothing else calls this, and another install's files stay.
pub fn wipe(paths: &Paths, key: &str) -> Result<()> {
    let gone = |r: std::io::Result<()>| match r {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(error("wipe", "The plugin's files could not be removed.")),
    };
    let db = paths.db(key);
    for suffix in ["-journal", "-wal", "-shm"] {
        let mut side = db.clone().into_os_string();
        side.push(suffix);
        gone(fs::remove_file(side))?;
    }
    gone(fs::remove_file(&db))?;
    gone(fs::remove_dir_all(paths.open_dir(key)))?;
    let _ = fs::remove_dir(&paths.dir);
    Ok(())
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

    fn key() -> String {
        key_from_seed("a-fake-provisioned-seed")
    }

    #[test]
    fn what_it_keeps_survives_a_reopen_and_is_encrypted_on_disk() {
        let dir = Dir::new("enc");
        let paths = Paths::in_dir(&dir.0);
        let secret = "fake-subject-line-to-find-on-disk";
        Store::open(&paths, &key())
            .unwrap()
            .set_state("me@example.com", "cursor", secret)
            .unwrap();
        assert_eq!(
            Store::open(&paths, &key())
                .unwrap()
                .state("me@example.com", "cursor")
                .unwrap()
                .as_deref(),
            Some(secret)
        );
        let bytes = fs::read(paths.db(&key())).unwrap();
        assert!(
            !bytes.starts_with(b"SQLite format 3"),
            "an encrypted file has no SQLite header"
        );
        assert!(
            !bytes.windows(secret.len()).any(|w| w == secret.as_bytes()),
            "the value is not in the file"
        );
    }

    /// A backup or a copy of the folder holds the lock, never the key.
    #[test]
    fn nothing_in_the_data_folder_opens_the_store() {
        let dir = Dir::new("copy");
        let paths = Paths::in_dir(&dir.0);
        let key = key();
        drop(Store::open(&paths, &key).unwrap());
        let files: Vec<String> = fs::read_dir(&paths.dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(files.len(), 1, "no key file beside it: {files:?}");
        assert!(
            files[0].starts_with("workspace-") && !files[0].contains(&key[..16]),
            "{files:?}"
        );
        let bytes = fs::read(paths.db(&key)).unwrap();
        assert!(!bytes.windows(key.len()).any(|w| w == key.as_bytes()));
    }

    /// Two installs on one folder, or a new plugin key: neither touches the other.
    #[test]
    fn each_install_has_its_own_store() {
        let dir = Dir::new("two");
        let paths = Paths::in_dir(&dir.0);
        let (one, two) = (key_from_seed("one"), key_from_seed("two"));
        Store::open(&paths, &one)
            .unwrap()
            .set_state("a", "k", "first")
            .unwrap();
        Store::open(&paths, &two)
            .unwrap()
            .set_state("a", "k", "second")
            .unwrap();
        assert_eq!(
            Store::open(&paths, &one)
                .unwrap()
                .state("a", "k")
                .unwrap()
                .as_deref(),
            Some("first")
        );
        assert_eq!(
            Store::open(&paths, &two)
                .unwrap()
                .state("a", "k")
                .unwrap()
                .as_deref(),
            Some("second")
        );
        wipe(&paths, &one).unwrap();
        assert!(
            !paths.db(&one).exists() && paths.db(&two).exists(),
            "an uninstall takes its own files only"
        );
    }

    #[test]
    fn the_key_is_the_seeds_and_not_the_seed() {
        assert_eq!(key_from_seed("s"), key_from_seed("s"));
        assert_ne!(key_from_seed("s"), key_from_seed("t"));
        assert_eq!(key_from_seed("s").len(), 64);
        assert!(!key_from_seed("seed-text").contains("seed-text"));
    }

    #[test]
    fn a_damaged_store_is_wrong_key_and_stays_on_disk_and_a_malformed_key_is_refused() {
        let dir = Dir::new("wrong");
        let paths = Paths::in_dir(&dir.0);
        let key = key();
        drop(Store::open(&paths, &key).unwrap());
        let bytes = vec![7u8; 8192];
        fs::write(paths.db(&key), &bytes).unwrap();
        assert_eq!(Store::open(&paths, &key).err().unwrap().code, "wrong_key");
        assert_eq!(fs::read(paths.db(&key)).unwrap(), bytes, "left as it was");
        assert_eq!(Store::open(&paths, "short").err().unwrap().code, "bad_key");
    }

    /// A store another connection holds is busy, not the wrong key.
    #[test]
    fn a_busy_store_is_not_a_wrong_key() {
        let dir = Dir::new("busy");
        let paths = Paths::in_dir(&dir.0);
        let key = key();
        let holder = Store::open(&paths, &key).unwrap();
        holder.conn().execute_batch("BEGIN EXCLUSIVE;").unwrap();
        let err = Store::open_waiting(&paths, &key, Duration::from_millis(50))
            .err()
            .unwrap();
        assert_eq!(err.code, "store");
        holder.conn().execute_batch("COMMIT;").unwrap();
        assert!(Store::open(&paths, &key).is_ok(), "and nothing was lost");
    }

    #[test]
    fn a_store_it_cant_reach_is_not_a_wrong_key() {
        let dir = Dir::new("io");
        let paths = Paths::in_dir(&dir.0);
        let key = key();
        fs::create_dir_all(paths.db(&key)).unwrap();
        assert_eq!(Store::open(&paths, &key).err().unwrap().code, "store");
    }

    #[test]
    fn with_no_home_or_a_relative_folder_there_is_no_store() {
        assert_eq!(Paths::from_vars(None, None).err().unwrap().code, "no_home");
        assert_eq!(
            Paths::from_vars(None, Some("".into())).err().unwrap().code,
            "no_home"
        );
        assert_eq!(
            Paths::from_vars(Some("relative/dir".into()), Some("/Users/u".into()))
                .err()
                .unwrap()
                .code,
            "data_dir"
        );
        assert_eq!(
            Paths::from_vars(Some("/d".into()), None).unwrap().dir,
            PathBuf::from("/d")
        );
        assert!(
            Paths::from_vars(None, Some("/Users/u".into()))
                .unwrap()
                .dir
                .ends_with("app.buwiz.google-workspace")
        );
    }

    #[test]
    fn wipe_takes_this_installs_files_and_leaves_the_rest() {
        let dir = Dir::new("shared");
        let paths = Paths::in_dir(&dir.0);
        let key = key();
        drop(Store::open(&paths, &key).unwrap());
        fs::create_dir_all(paths.open_dir(&key)).unwrap();
        fs::write(paths.open_dir(&key).join("a.docx"), b"x").unwrap();
        let sides: Vec<String> = ["-journal", "-wal", "-shm"]
            .iter()
            .map(|s| format!("{}{s}", paths.db(&key).display()))
            .collect();
        for side in &sides {
            fs::write(side, b"stale").unwrap();
        }
        let other = paths.dir.join("someone-elses.txt");
        fs::write(&other, b"keep me").unwrap();
        wipe(&paths, &key).unwrap();
        assert!(other.exists() && !paths.db(&key).exists() && !paths.open_dir(&key).exists());
        assert!(
            sides.iter().all(|s| !std::path::Path::new(s).exists()),
            "no stale journal for a later store"
        );
    }

    /// One kept from before has no part on its attachments, and none would open.
    #[test]
    fn a_stored_mail_view_from_before_parts_goes() {
        let dir = Dir::new("parts");
        let paths = Paths::in_dir(&dir.0);
        let store = Store::open(&paths, &key()).unwrap();
        store
            .conn()
            .execute_batch(
                "INSERT INTO bodies(account, thread, view, size, used) VALUES('a', 't', '{}', 2, 1);
                 PRAGMA user_version = 6;",
            )
            .unwrap();
        drop(store);
        let store = Store::open(&paths, &key()).unwrap();
        assert_eq!(store.view("a", "t", 2).unwrap(), None);
    }

    #[test]
    fn a_newer_store_is_refused_and_wipe_removes_everything() {
        let dir = Dir::new("newer");
        let paths = Paths::in_dir(&dir.0);
        let store = Store::open(&paths, &key()).unwrap();
        store
            .conn()
            .execute_batch("PRAGMA user_version = 9999;")
            .unwrap();
        drop(store);
        assert_eq!(
            Store::open(&paths, &key()).err().unwrap().code,
            "schema_newer"
        );
        wipe(&paths, &key()).unwrap();
        assert!(!paths.dir.exists());
        wipe(&paths, &key()).unwrap();
    }
}
