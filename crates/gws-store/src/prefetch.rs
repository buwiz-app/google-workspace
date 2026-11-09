//! Recent mail kept ahead of time: which threads still want their view, and the
//! bytes of their small attachments, within a budget of their own. An
//! attachment is found by its message and part: Gmail's attachment id can
//! change from one read to the next. Past the budget the oldest mail's go
//! first, so what's kept is always the newest there's room for.

use rusqlite::{OptionalExtension as _, params};

use crate::mail::BODIES_MAX;
use crate::{Result, Store};

/// The most attachment bytes kept per account.
pub const ATTACHMENTS_MAX: i64 = 500 * 1024 * 1024;
/// How many rounds a thread's attachments are tried before they're left to
/// the press.
pub const TRIES_MAX: i64 = 3;

/// A kept attachment. Its name and type are what Google said when it came, and
/// a stranger wrote both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Saved {
    pub thread: String,
    /// The thread's latest mail, Unix milliseconds: what orders the budget.
    pub at: i64,
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
    /// wait for an open, and so does a thread there was nothing to keep of, or
    /// whose view missed a picture [`TRIES_MAX`] rounds running, until it
    /// changes.
    pub fn unviewed_since(&self, account: &str, since: i64, take: usize) -> Result<Vec<String>> {
        let mut q = self.conn.prepare(
            "SELECT t.id FROM threads t WHERE t.account = ?1 AND t.at >= ?2 \
             AND NOT EXISTS (SELECT 1 FROM bodies b WHERE b.account = t.account AND b.thread = t.id) \
             AND NOT EXISTS (SELECT 1 FROM thread_labels l WHERE l.account = t.account AND l.thread = t.id AND l.label IN ('SPAM', 'TRASH')) \
             AND NOT EXISTS (SELECT 1 FROM ahead_passed p WHERE p.account = t.account AND p.thread = t.id AND p.at = t.at AND p.messages = t.messages AND p.misses >= ?4) \
             ORDER BY t.at DESC, t.id LIMIT ?3",
        )?;
        let rows = q.query_map(params![account, since, take as i64, TRIES_MAX], |r| {
            r.get(0)
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// `thread`'s row as the store has it: `(at, messages)`.
    pub fn thread_row(&self, account: &str, thread: &str) -> Result<Option<(i64, i64)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT at, messages FROM threads WHERE account = ? AND id = ?",
                params![account, thread],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    /// There's nothing to keep of `thread` while its row is `(at, messages)`,
    /// the row it was read by: a sync since then that changed it brings it
    /// back.
    pub fn pass_thread(&self, account: &str, thread: &str, row: (i64, i64)) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO ahead_passed(account, thread, at, messages, misses) VALUES(?, ?, ?, ?, ?)",
            params![account, thread, row.0, row.1, TRIES_MAX],
        )?;
        Ok(())
    }

    /// The view of `thread`, read at `row`, missed a picture or its twin check
    /// for time or a failure, and wasn't kept. After [`TRIES_MAX`] such rounds
    /// with the row unchanged it's passed as [`Store::pass_thread`] passes
    /// one: true then. A change starts the count over.
    pub fn missed_thread(&self, account: &str, thread: &str, row: (i64, i64)) -> Result<bool> {
        Ok(self.conn.query_row(
            "INSERT INTO ahead_passed(account, thread, at, messages, misses) VALUES(?1, ?2, ?3, ?4, 1) \
             ON CONFLICT(account, thread) DO UPDATE SET \
             misses = CASE WHEN at = excluded.at AND messages = excluded.messages THEN misses + 1 ELSE 1 END, \
             at = excluded.at, messages = excluded.messages \
             RETURNING misses >= ?5",
            params![account, thread, row.0, row.1, TRIES_MAX],
            |r| r.get(0),
        )?)
    }

    /// The bytes of the views of threads since `since`.
    pub fn recent_view_bytes(&self, account: &str, since: i64) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT coalesce(sum(b.size), 0) FROM bodies b JOIN threads t ON t.account = b.account AND t.id = b.thread WHERE b.account = ? AND t.at >= ?",
            params![account, since],
            |r| r.get(0),
        )?)
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
        if self.recent_view_bytes(account, since)? + view.len() as i64 > max {
            return Ok(Ahead::Full);
        }
        self.put_view(account, thread, view, now)?;
        Ok(Ahead::Kept)
    }

    /// Recent threads whose view is kept and whose attachments aren't looked
    /// at yet, the least tried first, then the newest: `(thread, at, view)`.
    pub fn views_unfetched_since(
        &self,
        account: &str,
        since: i64,
        take: usize,
    ) -> Result<Vec<(String, i64, String)>> {
        let mut q = self.conn.prepare(
            "SELECT b.thread, t.at, b.view FROM bodies b JOIN threads t ON t.account = b.account AND t.id = b.thread \
             WHERE b.account = ?1 AND b.fetched = 0 AND t.at >= ?2 ORDER BY b.tries, t.at DESC, t.id LIMIT ?3",
        )?;
        let rows = q.query_map(params![account, since, take as i64], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
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

    /// One of `thread`'s attachments didn't come. It's tried again after the
    /// others, and after [`TRIES_MAX`] rounds left to the press.
    pub fn tried(&self, account: &str, thread: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE bodies SET tries = tries + 1, fetched = (tries + 1 >= ?3) WHERE account = ?1 AND thread = ?2",
            params![account, thread, TRIES_MAX],
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

    /// Whether `size` bytes of mail from `at` fit the budget, once older mail's
    /// attachments made way: asked before a download, so none is wasted.
    pub fn attachment_room(&self, account: &str, at: i64, size: i64) -> Result<bool> {
        self.attachment_room_within(account, at, size, ATTACHMENTS_MAX)
    }

    fn attachment_room_within(&self, account: &str, at: i64, size: i64, max: i64) -> Result<bool> {
        let newer: i64 = self.conn.query_row(
            "SELECT coalesce(sum(size), 0) FROM attachment_bytes WHERE account = ? AND at >= ?",
            params![account, at],
            |r| r.get(0),
        )?;
        Ok(newer + size <= max)
    }

    /// Keeps an attachment's bytes. Past [`ATTACHMENTS_MAX`] the oldest mail's
    /// go first, this one among them: false if it was the one that went.
    pub fn put_attachment(&self, account: &str, saved: &Saved, now: i64) -> Result<bool> {
        self.put_attachment_within(account, saved, now, ATTACHMENTS_MAX)
    }

    fn put_attachment_within(&self, account: &str, a: &Saved, now: i64, max: i64) -> Result<bool> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO attachment_bytes(account, message, part, thread, at, name, mime, size, used, bytes) VALUES(?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![account, a.message, a.part, a.thread, a.at, a.name, a.mime, a.bytes.len() as i64, now, a.bytes],
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
            tx.execute(
                "DELETE FROM attachment_bytes WHERE rowid = (SELECT rowid FROM attachment_bytes WHERE account = ? ORDER BY at, used LIMIT 1)",
                [account],
            )?;
        }
        let kept: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM attachment_bytes WHERE account = ? AND message = ? AND part = ?)",
            params![account, a.message, a.part],
            |r| r.get(0),
        )?;
        tx.commit()?;
        Ok(kept)
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
                "SELECT thread, at, name, mime, bytes FROM attachment_bytes WHERE account = ? AND message = ? AND part = ?",
                params![account, message, part],
                |r| {
                    Ok(Saved {
                        thread: r.get(0)?,
                        at: r.get(1)?,
                        message: message.to_owned(),
                        part: part.to_owned(),
                        name: r.get(2)?,
                        mime: r.get(3)?,
                        bytes: r.get(4)?,
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

    fn file(message: &str, at: i64, bytes: &[u8]) -> Saved {
        Saved {
            thread: "t".into(),
            at,
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
    fn a_thread_with_nothing_to_keep_is_passed_until_it_changes() {
        let s = store("passed");
        s.put_thread("me", &thread("a", 10, &["INBOX"])).unwrap();
        s.put_thread("me", &thread("b", 5, &["INBOX"])).unwrap();
        s.pass_thread("me", "a", (10, 1)).unwrap();
        assert_eq!(s.unviewed_since("me", 0, 1).unwrap(), ["b"], "moved past");
        s.put_thread("me", &thread("a", 11, &["INBOX"])).unwrap();
        assert_eq!(s.unviewed_since("me", 0, 1).unwrap(), ["a"], "new mail");
        s.pass_thread("me", "a", (11, 1)).unwrap();
        let mut grown = thread("a", 11, &["INBOX"]);
        grown.messages = 2;
        s.put_thread("me", &grown).unwrap();
        assert_eq!(
            s.unviewed_since("me", 0, 1).unwrap(),
            ["a"],
            "a message more at the same time is a change too"
        );
        s.pass_thread("me", "a", (11, 1)).unwrap();
        assert_eq!(
            s.unviewed_since("me", 0, 1).unwrap(),
            ["a"],
            "passed by the row read, not by the row a sync left since"
        );
        s.remove_thread("me", "a").unwrap();
        s.forget_account("me").unwrap();
        assert!(s.accounts().unwrap().is_empty());
    }

    #[test]
    fn a_thread_whose_view_keeps_missing_is_passed_after_three_rounds_until_it_changes() {
        let s = store("missed");
        s.put_thread("me", &thread("a", 10, &["INBOX"])).unwrap();
        for round in 1..TRIES_MAX {
            assert!(!s.missed_thread("me", "a", (10, 1)).unwrap(), "{round}");
            assert_eq!(s.unviewed_since("me", 0, 5).unwrap(), ["a"], "{round}");
        }
        assert!(s.missed_thread("me", "a", (10, 1)).unwrap());
        assert!(s.unviewed_since("me", 0, 5).unwrap().is_empty(), "passed");
        s.put_thread("me", &thread("a", 11, &["INBOX"])).unwrap();
        assert_eq!(s.unviewed_since("me", 0, 5).unwrap(), ["a"], "new mail");
        assert!(
            !s.missed_thread("me", "a", (11, 1)).unwrap(),
            "the count starts over"
        );
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
        assert_eq!(s.recent_view_bytes("me", 0).unwrap(), 6);
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
            [("a".to_owned(), 10, "v2".to_owned())]
        );
        assert!(s.views_unfetched_since("me", 11, 5).unwrap().is_empty());
    }

    #[test]
    fn a_thread_whose_attachment_didnt_come_goes_last_and_is_left_after_three() {
        let s = store("tries");
        s.put_thread("me", &thread("a", 20, &["INBOX"])).unwrap();
        s.put_thread("me", &thread("b", 10, &["INBOX"])).unwrap();
        s.put_view("me", "a", "va", 1).unwrap();
        s.put_view("me", "b", "vb", 1).unwrap();
        s.tried("me", "a").unwrap();
        let first = |s: &Store| s.views_unfetched_since("me", 0, 1).unwrap()[0].0.clone();
        assert_eq!(first(&s), "b", "the newest no longer holds the line");
        s.tried("me", "a").unwrap();
        s.mark_fetched("me", "b").unwrap();
        assert_eq!(first(&s), "a");
        s.tried("me", "a").unwrap();
        assert!(s.views_unfetched_since("me", 0, 5).unwrap().is_empty());
    }

    #[test]
    fn past_the_budget_the_oldest_mails_attachments_go_this_one_among_them() {
        let s = store("budget");
        assert!(
            s.put_attachment_within("me", &file("old", 1, b"aaaa"), 1, 10)
                .unwrap()
        );
        assert!(
            s.put_attachment_within("me", &file("mid", 2, b"bbbb"), 2, 10)
                .unwrap()
        );
        // opened since, the old one is still the oldest mail
        assert!(s.attachment("me", "old", "1", 3).unwrap().is_some());
        assert!(s.attachment_room_within("me", 3, 4, 10).unwrap());
        assert!(
            s.put_attachment_within("me", &file("new", 3, b"cccc"), 4, 10)
                .unwrap()
        );
        assert!(!s.has_attachment("me", "old", "1").unwrap());
        assert!(s.has_attachment("me", "mid", "1").unwrap());
        assert!(
            !s.attachment_room_within("me", 0, 4, 10).unwrap(),
            "older mail has no room"
        );
        assert!(
            !s.put_attachment_within("me", &file("older", 0, b"dddd"), 5, 10)
                .unwrap(),
            "never pushes out newer mail"
        );
        assert!(
            s.has_attachment("me", "mid", "1").unwrap()
                && s.has_attachment("me", "new", "1").unwrap()
        );
        s.put_attachment_within("other", &file("theirs", 9, b"eeeeeeee"), 6, 10)
            .unwrap();
        assert!(
            s.has_attachment("me", "mid", "1").unwrap(),
            "each account has its own budget"
        );
    }

    #[test]
    fn attachments_go_with_their_thread_and_their_account() {
        let s = store("forget");
        s.put_thread("me", &thread("t", 10, &["INBOX"])).unwrap();
        s.put_attachment("me", &file("m1", 10, b"x"), 1).unwrap();
        assert_eq!(s.accounts().unwrap(), ["me"]);
        s.remove_thread("me", "t").unwrap();
        assert!(!s.has_attachment("me", "m1", "1").unwrap());
        s.put_attachment("me", &file("m2", 10, b"x"), 1).unwrap();
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
