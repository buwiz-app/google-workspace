//! Recent mail kept ahead of time: which threads still want their view, and the
//! bytes of their small attachments, within a budget of their own. An
//! attachment is found by its message and part: Gmail's attachment id can
//! change from one read to the next.

use rusqlite::{OptionalExtension as _, params};

use crate::mail::BODIES_MAX;
use crate::{Result, Store};

/// The most attachment bytes kept per account.
pub const ATTACHMENTS_MAX: i64 = 500 * 1024 * 1024;

/// A kept attachment. Its name and type are what Google said when it came, and
/// a stranger wrote both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Saved {
    pub thread: String,
    pub message: String,
    pub part: String,
    pub name: String,
    pub mime: String,
    pub bytes: Vec<u8>,
}

/// What became of a view kept ahead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ahead {
    Kept,
    /// The thread has a view already, or changed or went since it was read.
    Passed,
    /// The recent views fill the budget: one more would push out another.
    Full,
}

impl Store {
    /// Threads since `since` that have no view, newest first. Spam and Trash
    /// wait for an open.
    pub fn unviewed_since(&self, account: &str, since: i64, take: usize) -> Result<Vec<String>> {
        let mut q = self.conn.prepare(
            "SELECT t.id FROM threads t WHERE t.account = ?1 AND t.at >= ?2 \
             AND NOT EXISTS (SELECT 1 FROM bodies b WHERE b.account = t.account AND b.thread = t.id) \
             AND NOT EXISTS (SELECT 1 FROM thread_labels l WHERE l.account = t.account AND l.thread = t.id AND l.label IN ('SPAM', 'TRASH')) \
             ORDER BY t.at DESC, t.id LIMIT ?3",
        )?;
        let rows = q.query_map(params![account, since, take as i64], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Keeps a view read ahead of an open, as [`Store::put_view`] does, only
    /// while the thread's row is still `(at, messages)` and nothing kept one
    /// first. Not past the budget for recent threads: the next would push one
    /// out, and that one would come back on the next round.
    pub fn put_view_ahead(
        &self,
        account: &str,
        thread: &str,
        row: (i64, i64),
        view: &str,
        now: i64,
        since: i64,
    ) -> Result<Ahead> {
        self.put_view_ahead_within(account, thread, row, view, now, since, BODIES_MAX)
    }

    #[allow(clippy::too_many_arguments)]
    fn put_view_ahead_within(
        &self,
        account: &str,
        thread: &str,
        row: (i64, i64),
        view: &str,
        now: i64,
        since: i64,
        max: i64,
    ) -> Result<Ahead> {
        let now_row: Option<(i64, i64)> = self
            .conn
            .query_row(
                "SELECT at, messages FROM threads WHERE account = ? AND id = ?",
                params![account, thread],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let viewed: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM bodies WHERE account = ? AND thread = ?)",
            params![account, thread],
            |r| r.get(0),
        )?;
        if now_row != Some(row) || viewed {
            return Ok(Ahead::Passed);
        }
        let recent: i64 = self.conn.query_row(
            "SELECT coalesce(sum(b.size), 0) FROM bodies b JOIN threads t ON t.account = b.account AND t.id = b.thread WHERE b.account = ? AND t.at >= ?",
            params![account, since],
            |r| r.get(0),
        )?;
        if recent + view.len() as i64 > max {
            return Ok(Ahead::Full);
        }
        self.put_view(account, thread, view, now)?;
        Ok(Ahead::Kept)
    }

    /// Recent threads whose view is kept and whose attachments aren't looked
    /// at yet, newest first: `(thread, view)`.
    pub fn views_unfetched_since(
        &self,
        account: &str,
        since: i64,
        take: usize,
    ) -> Result<Vec<(String, String)>> {
        let mut q = self.conn.prepare(
            "SELECT b.thread, b.view FROM bodies b JOIN threads t ON t.account = b.account AND t.id = b.thread \
             WHERE b.account = ?1 AND b.fetched = 0 AND t.at >= ?2 ORDER BY t.at DESC, t.id LIMIT ?3",
        )?;
        let rows = q.query_map(params![account, since, take as i64], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Every attachment of the view of `thread` is kept or left to its open.
    /// A new view of the thread starts over.
    pub fn mark_fetched(&self, account: &str, thread: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE bodies SET fetched = 1 WHERE account = ? AND thread = ?",
            params![account, thread],
        )?;
        Ok(())
    }

    pub fn has_attachment(&self, account: &str, message: &str, part: &str) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM attachment_bytes WHERE account = ? AND message = ? AND part = ?)",
            params![account, message, part],
            |r| r.get(0),
        )?)
    }

    /// Keeps an attachment's bytes, dropping the least recently used past
    /// [`ATTACHMENTS_MAX`], never the one just kept.
    pub fn put_attachment(&self, account: &str, saved: &Saved, now: i64) -> Result<()> {
        self.put_attachment_within(account, saved, now, ATTACHMENTS_MAX)
    }

    fn put_attachment_within(&self, account: &str, a: &Saved, now: i64, max: i64) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO attachment_bytes(account, message, part, thread, name, mime, size, used, bytes) VALUES(?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![account, a.message, a.part, a.thread, a.name, a.mime, a.bytes.len() as i64, now, a.bytes],
        )?;
        loop {
            let total: i64 = tx.query_row(
                "SELECT coalesce(sum(size), 0) FROM attachment_bytes WHERE account = ?",
                [account],
                |r| r.get(0),
            )?;
            if total <= max {
                break;
            }
            let gone = tx.execute(
                "DELETE FROM attachment_bytes WHERE rowid = (SELECT rowid FROM attachment_bytes WHERE account = ?1 AND NOT (message = ?2 AND part = ?3) ORDER BY used LIMIT 1)",
                params![account, a.message, a.part],
            )?;
            if gone == 0 {
                break;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// A kept attachment, which counts as used now.
    pub fn attachment(
        &self,
        account: &str,
        message: &str,
        part: &str,
        now: i64,
    ) -> Result<Option<Saved>> {
        let saved = self
            .conn
            .query_row(
                "SELECT thread, name, mime, bytes FROM attachment_bytes WHERE account = ? AND message = ? AND part = ?",
                params![account, message, part],
                |r| {
                    Ok(Saved {
                        thread: r.get(0)?,
                        message: message.to_owned(),
                        part: part.to_owned(),
                        name: r.get(1)?,
                        mime: r.get(2)?,
                        bytes: r.get(3)?,
                    })
                },
            )
            .optional()?;
        if saved.is_some() {
            self.conn.execute(
                "UPDATE attachment_bytes SET used = ? WHERE account = ? AND message = ? AND part = ?",
                params![now, account, message, part],
            )?;
        }
        Ok(saved)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail::Thread;
    use crate::{Paths, key_from_seed};

    fn store(name: &str) -> Store {
        let dir = std::env::temp_dir().join(format!("gws-ahead-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Store::open(&Paths::in_dir(dir), &key_from_seed("s")).unwrap()
    }

    fn thread(id: &str, at: i64, labels: &[&str]) -> Thread {
        Thread {
            id: id.into(),
            at,
            messages: 1,
            labels: labels.iter().map(|l| (*l).to_owned()).collect(),
            ..Thread::default()
        }
    }

    fn file(message: &str, bytes: &[u8]) -> Saved {
        Saved {
            thread: "t".into(),
            message: message.into(),
            part: "1".into(),
            name: "a.pdf".into(),
            mime: "application/pdf".into(),
            bytes: bytes.to_vec(),
        }
    }

    #[test]
    fn only_recent_threads_without_a_view_are_due_newest_first() {
        let s = store("due");
        for (id, at) in [("old", 1), ("a", 10), ("b", 30), ("c", 20)] {
            s.put_thread("me", &thread(id, at, &["INBOX"])).unwrap();
        }
        s.put_thread("me", &thread("junk", 40, &["SPAM"])).unwrap();
        s.put_thread("other", &thread("theirs", 50, &["INBOX"]))
            .unwrap();
        s.put_view("me", "c", "seen", 1).unwrap();
        assert_eq!(s.unviewed_since("me", 5, 20).unwrap(), ["b", "a"]);
        assert_eq!(s.unviewed_since("me", 5, 1).unwrap().len(), 1);
    }

    #[test]
    fn a_view_read_ahead_never_replaces_one_or_lands_on_a_changed_thread() {
        let s = store("ahead");
        s.put_thread("me", &thread("a", 10, &["INBOX"])).unwrap();
        assert_eq!(
            s.put_view_ahead("me", "a", (9, 1), "stale", 1, 0).unwrap(),
            Ahead::Passed,
            "the row moved on since it was read"
        );
        s.put_view("me", "a", "opened", 1).unwrap();
        assert_eq!(
            s.put_view_ahead("me", "a", (10, 1), "ahead", 2, 0).unwrap(),
            Ahead::Passed
        );
        assert_eq!(s.view("me", "a", 3).unwrap().as_deref(), Some("opened"));
        s.put_thread("me", &thread("b", 10, &["INBOX"])).unwrap();
        assert_eq!(
            s.put_view_ahead_within("me", "b", (10, 1), "0123456789", 4, 0, 12)
                .unwrap(),
            Ahead::Full,
            "past the budget with the recent views"
        );
        assert_eq!(
            s.put_view_ahead("me", "b", (10, 1), "ahead", 4, 0).unwrap(),
            Ahead::Kept
        );
    }

    #[test]
    fn a_new_view_has_its_attachments_looked_at_again() {
        let s = store("fetched");
        s.put_thread("me", &thread("a", 10, &["INBOX"])).unwrap();
        s.put_view("me", "a", "v1", 1).unwrap();
        assert_eq!(s.views_unfetched_since("me", 0, 5).unwrap().len(), 1);
        s.mark_fetched("me", "a").unwrap();
        assert!(s.views_unfetched_since("me", 0, 5).unwrap().is_empty());
        s.put_view("me", "a", "v2", 2).unwrap();
        assert_eq!(
            s.views_unfetched_since("me", 0, 5).unwrap(),
            [("a".to_owned(), "v2".to_owned())]
        );
        assert!(s.views_unfetched_since("me", 11, 5).unwrap().is_empty());
    }

    #[test]
    fn past_the_budget_the_least_recently_used_attachment_goes_never_the_new_one() {
        let s = store("budget");
        s.put_attachment_within("me", &file("old", b"aaaa"), 1, 10)
            .unwrap();
        s.put_attachment_within("me", &file("mid", b"bbbb"), 2, 10)
            .unwrap();
        assert!(
            s.attachment("me", "old", "1", 3).unwrap().is_some(),
            "and opening it makes it recent"
        );
        s.put_attachment_within("me", &file("new", b"cccc"), 4, 10)
            .unwrap();
        assert!(!s.has_attachment("me", "mid", "1").unwrap());
        assert!(s.has_attachment("me", "old", "1").unwrap());
        assert_eq!(
            s.attachment("me", "new", "1", 5).unwrap().unwrap().bytes,
            b"cccc"
        );
        s.put_attachment_within("other", &file("theirs", b"dddddddd"), 6, 10)
            .unwrap();
        assert!(
            s.has_attachment("me", "old", "1").unwrap(),
            "each account has its own budget"
        );
        s.put_attachment_within("me", &file("huge", &[7; 50]), 7, 10)
            .unwrap();
        assert!(s.has_attachment("me", "huge", "1").unwrap());
    }

    #[test]
    fn attachments_go_with_their_thread_and_their_account() {
        let s = store("forget");
        s.put_thread("me", &thread("t", 10, &["INBOX"])).unwrap();
        s.put_attachment("me", &file("m1", b"x"), 1).unwrap();
        assert_eq!(s.accounts().unwrap(), ["me"]);
        s.remove_thread("me", "t").unwrap();
        assert!(!s.has_attachment("me", "m1", "1").unwrap());
        s.put_attachment("me", &file("m2", b"x"), 1).unwrap();
        assert_eq!(
            s.accounts().unwrap(),
            ["me"],
            "a store with only an attachment still knows whose"
        );
        s.forget_account("me").unwrap();
        assert!(!s.has_attachment("me", "m2", "1").unwrap());
        assert!(s.accounts().unwrap().is_empty());
    }
}
