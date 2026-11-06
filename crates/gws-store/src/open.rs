//! What's open on this Mac: one row for each file in the `open/` folder, with
//! what it was when it came. The file itself is plain, as the viewer needs a
//! real file; this row is what tells an edit from what Drive sent, and it
//! belongs in here, not beside the file, where anything could rewrite it.

use rusqlite::{OptionalExtension as _, params};

use crate::{Result, Store};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Opened {
    /// The folder under `open/`: a Drive id, or `att-…` for an attachment.
    pub id: String,
    /// Whose it is: the one folder serves every sign-in on this Mac.
    pub account: String,
    /// The safe file name it has on disk.
    pub name: String,
    /// What's on disk, so a Google Doc's is docx's.
    pub mime: String,
    /// A Google Doc, Sheet or Slides: never written over.
    pub google: bool,
    /// A mail's attachment: nothing to edit back to.
    pub attachment: bool,
    /// SHA-256 of what came, in hex.
    pub hash: String,
    /// Drive's `version` when it came.
    pub version: String,
    /// Drive's `md5Checksum` when it came. A Google file has none.
    pub md5: String,
    /// When it was last opened, in Unix milliseconds.
    pub opened_ms: i64,
}

fn row(r: &rusqlite::Row) -> rusqlite::Result<Opened> {
    Ok(Opened {
        id: r.get("id")?,
        account: r.get("account")?,
        name: r.get("name")?,
        mime: r.get("mime")?,
        google: r.get("google")?,
        attachment: r.get("attachment")?,
        hash: r.get("hash")?,
        version: r.get("version")?,
        md5: r.get("md5")?,
        opened_ms: r.get("opened_ms")?,
    })
}

impl Store {
    pub fn put_opened(&self, o: &Opened) -> Result<()> {
        self.conn().execute(
            "INSERT OR REPLACE INTO opened(id, account, name, mime, google, attachment, hash, version, md5, opened_ms) VALUES(?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![o.id, o.account, o.name, o.mime, o.google, o.attachment, o.hash, o.version, o.md5, o.opened_ms],
        )?;
        Ok(())
    }

    pub fn opened(&self, id: &str) -> Result<Option<Opened>> {
        let found = self
            .conn()
            .query_row("SELECT * FROM opened WHERE id = ?", [id], row)
            .optional()?;
        Ok(found)
    }

    /// Everything open, oldest first.
    pub fn all_opened(&self) -> Result<Vec<Opened>> {
        let mut q = self
            .conn()
            .prepare("SELECT * FROM opened ORDER BY opened_ms, id")?;
        let rows = q.query_map([], row)?.collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    pub fn forget_opened(&self, id: &str) -> Result<()> {
        self.conn()
            .execute("DELETE FROM opened WHERE id = ?", [id])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Paths;

    #[test]
    fn a_row_comes_back_whole_and_outlasts_its_account() {
        let dir = std::env::temp_dir().join(format!("gws-opened-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&Paths::in_dir(&dir), &crate::key_from_seed("seed")).unwrap();
        let file = Opened {
            id: "a1".to_owned(),
            account: "owner@example.com".to_owned(),
            name: "Plan.docx".to_owned(),
            mime: "application/pdf".to_owned(),
            google: true,
            attachment: false,
            hash: "abc".to_owned(),
            version: "7".to_owned(),
            md5: "d41d".to_owned(),
            opened_ms: 42,
        };
        store.put_opened(&file).unwrap();
        assert_eq!(store.opened("a1").unwrap().as_ref(), Some(&file));
        assert_eq!(store.all_opened().unwrap(), [file]);
        assert_eq!(store.accounts().unwrap(), ["owner@example.com"]);
        store.forget_account("owner@example.com").unwrap();
        // what's edited here can't be fetched again, so the plugin decides
        assert!(store.opened("a1").unwrap().is_some());
        store.forget_opened("a1").unwrap();
        assert_eq!(store.opened("a1").unwrap(), None);
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
