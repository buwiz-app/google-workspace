//! Mail in the store: labels, thread rows for the lists, and the cleaned views
//! of threads the person opened, within a size cap.

use rusqlite::{OptionalExtension as _, params};

use crate::{Result, Store};

/// The most opened-thread bytes kept per account.
pub const BODIES_MAX: i64 = 200 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Label {
    pub id: String,
    pub name: String,
    /// `system` or `user`.
    pub kind: String,
    pub unread: i64,
    pub total: i64,
}

/// One row of a mail list.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Thread {
    pub id: String,
    pub subject: String,
    pub snippet: String,
    /// The display name of the latest sender, else the address.
    pub sender: String,
    pub address: String,
    /// Unix milliseconds of the latest message.
    pub at: i64,
    pub unread: bool,
    pub starred: bool,
    pub messages: i64,
    pub attachment: bool,
    pub labels: Vec<String>,
}

fn row(r: &rusqlite::Row) -> rusqlite::Result<Thread> {
    Ok(Thread {
        id: r.get("id")?,
        subject: r.get("subject")?,
        snippet: r.get("snippet")?,
        sender: r.get("sender")?,
        address: r.get("address")?,
        at: r.get("at")?,
        unread: r.get("unread")?,
        starred: r.get("starred")?,
        messages: r.get("messages")?,
        attachment: r.get("attachment")?,
        labels: Vec::new(),
    })
}

impl Store {
    pub fn put_labels(&self, account: &str, labels: &[Label]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM labels WHERE account = ?", [account])?;
        for l in labels {
            tx.execute("INSERT INTO labels(account, id, name, kind, unread, total) VALUES(?, ?, ?, ?, ?, ?)", params![account, l.id, l.name, l.kind, l.unread, l.total])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn labels(&self, account: &str) -> Result<Vec<Label>> {
        let mut q = self.conn.prepare("SELECT id, name, kind, unread, total FROM labels WHERE account = ? ORDER BY kind, name")?;
        let rows = q.query_map([account], |r| {
            Ok(Label {
                id: r.get(0)?,
                name: r.get(1)?,
                kind: r.get(2)?,
                unread: r.get(3)?,
                total: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Adds or replaces a thread, its labels with it, and drops its opened view:
    /// a thread that changed may have a new message.
    pub fn put_thread(&self, account: &str, t: &Thread) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO threads(account, id, subject, snippet, sender, address, at, unread, starred, messages, attachment) VALUES(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![account, t.id, t.subject, t.snippet, t.sender, t.address, t.at, t.unread, t.starred, t.messages, t.attachment],
        )?;
        tx.execute(
            "DELETE FROM thread_labels WHERE account = ? AND thread = ?",
            params![account, t.id],
        )?;
        for l in &t.labels {
            tx.execute(
                "INSERT OR IGNORE INTO thread_labels(account, thread, label) VALUES(?, ?, ?)",
                params![account, t.id, l],
            )?;
        }
        tx.execute(
            "DELETE FROM bodies WHERE account = ? AND thread = ?",
            params![account, t.id],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn remove_thread(&self, account: &str, id: &str) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        for table in [
            "threads WHERE account = ? AND id = ?",
            "thread_labels WHERE account = ? AND thread = ?",
            "bodies WHERE account = ? AND thread = ?",
        ] {
            tx.execute(&format!("DELETE FROM {table}"), params![account, id])?;
        }
        tx.commit()?;
        Ok(())
    }

    /// A label's threads, newest first.
    pub fn threads(&self, account: &str, label: &str, skip: i64, take: i64) -> Result<Vec<Thread>> {
        let mut q = self.conn.prepare("SELECT t.* FROM threads t JOIN thread_labels l ON l.account = t.account AND l.thread = t.id WHERE t.account = ? AND l.label = ? ORDER BY t.at DESC, t.id LIMIT ? OFFSET ?")?;
        let rows = q.query_map(params![account, label, take, skip], row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn thread_count(&self, account: &str, label: &str) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT count(*) FROM thread_labels WHERE account = ? AND label = ?",
            params![account, label],
            |r| r.get(0),
        )?)
    }

    /// What's stored that matches `text`, for searching with no connection.
    pub fn search_threads(&self, account: &str, text: &str, take: i64) -> Result<Vec<Thread>> {
        let like = format!(
            "%{}%",
            text.replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        );
        let mut q = self.conn.prepare("SELECT * FROM threads WHERE account = ?1 AND (subject LIKE ?2 ESCAPE '\\' OR sender LIKE ?2 ESCAPE '\\' OR address LIKE ?2 ESCAPE '\\' OR snippet LIKE ?2 ESCAPE '\\') ORDER BY at DESC LIMIT ?3")?;
        let rows = q.query_map(params![account, like, take], row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Keeps a thread's cleaned view, dropping the least recently opened past
    /// [`BODIES_MAX`].
    pub fn put_view(&self, account: &str, thread: &str, view: &str, now: i64) -> Result<()> {
        self.put_view_within(account, thread, view, now, BODIES_MAX)
    }

    fn put_view_within(
        &self,
        account: &str,
        thread: &str,
        view: &str,
        now: i64,
        max: i64,
    ) -> Result<()> {
        self.conn.execute("INSERT OR REPLACE INTO bodies(account, thread, view, size, used) VALUES(?, ?, ?, ?, ?)", params![account, thread, view, view.len() as i64, now])?;
        loop {
            let total: i64 = self.conn.query_row(
                "SELECT coalesce(sum(size), 0) FROM bodies WHERE account = ?",
                [account],
                |r| r.get(0),
            )?;
            if total <= max {
                return Ok(());
            }
            let gone = self.conn.execute("DELETE FROM bodies WHERE account = ?1 AND thread = (SELECT thread FROM bodies WHERE account = ?1 AND thread != ?2 ORDER BY used LIMIT 1)", params![account, thread])?;
            if gone == 0 {
                return Ok(());
            }
        }
    }

    pub fn view(&self, account: &str, thread: &str, now: i64) -> Result<Option<String>> {
        let view = self
            .conn
            .query_row(
                "SELECT view FROM bodies WHERE account = ? AND thread = ?",
                params![account, thread],
                |r| r.get(0),
            )
            .optional()?;
        if view.is_some() {
            self.conn.execute(
                "UPDATE bodies SET used = ? WHERE account = ? AND thread = ?",
                params![now, account, thread],
            )?;
        }
        Ok(view)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Paths, key_from_seed};

    fn store(name: &str) -> Store {
        let dir = std::env::temp_dir().join(format!("gws-mail-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Store::open(&Paths::in_dir(dir), &key_from_seed("s")).unwrap()
    }

    fn thread(id: &str, at: i64, labels: &[&str]) -> Thread {
        Thread {
            id: id.into(),
            subject: format!("Subject {id}"),
            sender: "Ana".into(),
            address: "ana@example.com".into(),
            at,
            messages: 1,
            labels: labels.iter().map(|l| (*l).to_owned()).collect(),
            ..Thread::default()
        }
    }

    #[test]
    fn a_label_lists_its_threads_newest_first_and_pages() {
        let s = store("list");
        for (id, at) in [("a", 1), ("b", 3), ("c", 2)] {
            s.put_thread("me", &thread(id, at, &["INBOX"])).unwrap();
        }
        s.put_thread("me", &thread("d", 4, &["SENT"])).unwrap();
        s.put_thread("other", &thread("e", 9, &["INBOX"])).unwrap();
        let ids = |skip, take| {
            s.threads("me", "INBOX", skip, take)
                .unwrap()
                .into_iter()
                .map(|t| t.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(0, 10), ["b", "c", "a"]);
        assert_eq!(ids(1, 1), ["c"]);
        assert_eq!(s.thread_count("me", "INBOX").unwrap(), 3);
    }

    #[test]
    fn a_thread_that_changes_labels_moves_and_loses_its_old_view() {
        let s = store("move");
        s.put_thread("me", &thread("a", 1, &["INBOX"])).unwrap();
        s.put_view("me", "a", "old view", 1).unwrap();
        s.put_thread("me", &thread("a", 2, &["TRASH"])).unwrap();
        assert_eq!(s.thread_count("me", "INBOX").unwrap(), 0);
        assert_eq!(s.thread_count("me", "TRASH").unwrap(), 1);
        assert_eq!(s.view("me", "a", 3).unwrap(), None);
        s.remove_thread("me", "a").unwrap();
        assert_eq!(s.thread_count("me", "TRASH").unwrap(), 0);
    }

    #[test]
    fn search_treats_its_text_as_text() {
        let s = store("search");
        s.put_thread("me", &thread("a", 1, &["INBOX"])).unwrap();
        assert_eq!(s.search_threads("me", "subject a", 5).unwrap().len(), 1);
        assert!(s.search_threads("me", "%", 5).unwrap().is_empty());
        assert!(s.search_threads("me", "_", 5).unwrap().is_empty());
    }

    #[test]
    fn past_the_cap_the_least_recently_opened_view_goes_never_the_new_one() {
        let s = store("cap");
        s.put_view_within("me", "old", "aaaa", 1, 10).unwrap();
        s.put_view_within("me", "mid", "bbbb", 2, 10).unwrap();
        assert!(
            s.view("me", "old", 3).unwrap().is_some(),
            "and opening it makes it recent"
        );
        s.put_view_within("me", "new", "cccc", 4, 10).unwrap();
        assert_eq!(s.view("me", "mid", 5).unwrap(), None);
        assert!(
            s.view("me", "old", 5).unwrap().is_some() && s.view("me", "new", 5).unwrap().is_some()
        );
        s.put_view_within("me", "huge", &"x".repeat(50), 6, 10)
            .unwrap();
        assert!(s.view("me", "huge", 7).unwrap().is_some());
    }

    #[test]
    fn labels_replace_what_was_there() {
        let s = store("labels");
        s.put_labels(
            "me",
            &[Label {
                id: "INBOX".into(),
                name: "INBOX".into(),
                kind: "system".into(),
                unread: 2,
                total: 9,
            }],
        )
        .unwrap();
        s.put_labels(
            "me",
            &[Label {
                id: "Label_1".into(),
                name: "Work".into(),
                kind: "user".into(),
                unread: 0,
                total: 1,
            }],
        )
        .unwrap();
        assert_eq!(
            s.labels("me")
                .unwrap()
                .iter()
                .map(|l| l.name.as_str())
                .collect::<Vec<_>>(),
            ["Work"]
        );
    }
}
