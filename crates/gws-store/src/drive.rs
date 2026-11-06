//! Drive in the store: the files the person has looked at, and each list as
//! Google gave it. A list keeps the place each row arrived in, not a sort of
//! our own: Google's idea of name order may differ from ours, and a row sorted
//! into pages already shown would push one out of them and double another.
//!
//! A view is `my`, `shared`, `recent`, `starred`, `trash` or `folder:<id>`.
//! Only what was visited is here, plus what the changes sync touches of it.

use rusqlite::{Connection, OptionalExtension as _, params};

use crate::{Result, Store};

/// Where the changes sync's token is kept. It goes in with the changes it
/// follows, so a sync cut short starts again from the right place.
pub const CHANGES_TOKEN: &str = "drive_changes";
pub const TRASH: &str = "trash";
pub const RECENT: &str = "recent";
const FOLDER_MIME: &str = "application/vnd.google-apps.folder";

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct File {
    pub id: String,
    pub name: String,
    /// folder, doc, sheet, slides, pdf, image, word, spreadsheet or file.
    pub kind: String,
    pub mime: String,
    /// A display name.
    pub owner: String,
    /// The person owns it.
    pub mine: bool,
    /// RFC 3339.
    pub changed: String,
    /// Unix milliseconds.
    pub changed_ms: i64,
    /// Bytes; Google's own files have none.
    pub size: Option<i64>,
    pub starred: bool,
    /// Shared with anyone at all.
    pub shared: bool,
    /// Someone shared it with the person.
    pub with_me: bool,
    pub trashed: bool,
    pub parents: Vec<String>,
    /// What Google says the person may do with it, in the plugin's words, or
    /// `None` if it didn't say.
    pub can: Option<Vec<String>>,
}

impl File {
    fn folder(&self) -> bool {
        self.mime == FOLDER_MIME
    }
}

fn row(r: &rusqlite::Row) -> rusqlite::Result<File> {
    let parents: String = r.get("parents")?;
    let can: Option<String> = r.get("can")?;
    Ok(File {
        id: r.get("id")?,
        name: r.get("name")?,
        kind: r.get("kind")?,
        mime: r.get("mime")?,
        owner: r.get("owner")?,
        mine: r.get("mine")?,
        changed: r.get("changed")?,
        changed_ms: r.get("changed_ms")?,
        size: r.get("size")?,
        starred: r.get("starred")?,
        shared: r.get("shared")?,
        with_me: r.get("with_me")?,
        trashed: r.get("trashed")?,
        parents: parents.split_whitespace().map(str::to_owned).collect(),
        can: can.map(|c| c.split_whitespace().map(str::to_owned).collect()),
    })
}

fn put(conn: &Connection, account: &str, f: &File) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO drive_files(account, id, name, kind, mime, owner, mine, changed, changed_ms, size, starred, shared, with_me, trashed, parents, can) VALUES(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        params![account, f.id, f.name, f.kind, f.mime, f.owner, f.mine, f.changed, f.changed_ms, f.size, f.starred, f.shared, f.with_me, f.trashed, f.parents.join(" "), f.can.as_ref().map(|c| c.join(" "))],
    )?;
    Ok(())
}

/// Whether `file` is in `view` now. `root` is My Drive's id.
fn belongs(view: &str, file: &File, root: &str) -> bool {
    let inside = |folder: &str| file.parents.iter().any(|p| p == folder);
    match view {
        TRASH => file.trashed,
        _ if file.trashed => false,
        "my" => inside(root),
        "shared" => file.with_me,
        RECENT => true,
        "starred" => file.starred,
        _ => view.strip_prefix("folder:").is_some_and(inside),
    }
}

/// Whether `a` goes ahead of `b` in `view`.
fn ahead(view: &str, a: &File, b: &File) -> bool {
    if view == RECENT {
        return (b.changed_ms, &b.id) < (a.changed_ms, &a.id);
    }
    let key = |f: &File| (!f.folder(), f.name.to_lowercase(), f.id.clone());
    key(a) < key(b)
}

/// A list's rows in order, each with its place.
fn listed(conn: &Connection, account: &str, view: &str) -> Result<Vec<(i64, File)>> {
    let mut q = conn.prepare(
        "SELECT l.at AS at, f.* FROM drive_listed l JOIN drive_files f ON f.account = l.account AND f.id = l.file WHERE l.account = ? AND l.view = ? ORDER BY l.at",
    )?;
    let rows = q
        .query_map(params![account, view], |r| Ok((r.get("at")?, row(r)?)))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// Puts a changed file where it goes in a list. Past the last row of a list
/// with pages still to come, it's left out: one of those pages brings it.
fn place(conn: &Connection, account: &str, view: &str, next: &str, f: &File) -> Result<bool> {
    let rows = listed(conn, account, view)?;
    let at = match rows.iter().find(|(_, other)| ahead(view, f, other)) {
        Some((at, _)) => {
            conn.execute(
                "UPDATE drive_listed SET at = at + 1 WHERE account = ? AND view = ? AND at >= ?",
                params![account, view, at],
            )?;
            *at
        }
        None if next.is_empty() => rows.last().map_or(0, |(at, _)| at + 1),
        None => return Ok(false),
    };
    conn.execute(
        "INSERT INTO drive_listed(account, view, file, at) VALUES(?, ?, ?, ?)",
        params![account, view, f.id, at],
    )?;
    Ok(true)
}

fn unlist(conn: &Connection, account: &str, view: &str, file: &str) -> Result<bool> {
    let n = conn.execute(
        "DELETE FROM drive_listed WHERE account = ? AND view = ? AND file = ?",
        params![account, view, file],
    )?;
    Ok(n > 0)
}

/// Changed files and ones that went, as the lists should now hold them. True if
/// anything here changed.
fn apply(
    conn: &Connection,
    account: &str,
    root: &str,
    changed: &[File],
    gone: &[String],
) -> Result<bool> {
    let views: Vec<(String, String)> = {
        let mut q = conn.prepare("SELECT view, next FROM drive_views WHERE account = ?")?;
        q.query_map([account], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?
    };
    let mut moved = false;
    for f in changed {
        let known = conn
            .query_row(
                "SELECT 1 FROM drive_files WHERE account = ? AND id = ?",
                params![account, f.id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        // the row goes in first: a list's own rows are read with it
        put(conn, account, f)?;
        let mut wanted = false;
        for (view, next) in &views {
            let was = unlist(conn, account, view, &f.id)?;
            let now = belongs(view, f, root) && place(conn, account, view, next, f)?;
            wanted |= was || now;
        }
        if !known && !wanted {
            conn.execute(
                "DELETE FROM drive_files WHERE account = ? AND id = ?",
                params![account, f.id],
            )?;
        }
        moved |= known || wanted;
    }
    for id in gone {
        let n = conn.execute(
            "DELETE FROM drive_files WHERE account = ? AND id = ?",
            params![account, id],
        )?;
        let listed = conn.execute(
            "DELETE FROM drive_listed WHERE account = ? AND file = ?",
            params![account, id],
        )?;
        // what a folder that's gone held can't be reached any more
        drop_view(conn, account, &format!("folder:{id}"))?;
        moved |= n + listed > 0;
    }
    Ok(moved)
}

fn drop_view(conn: &Connection, account: &str, view: &str) -> Result<()> {
    for table in ["drive_listed", "drive_views"] {
        conn.execute(
            &format!("DELETE FROM {table} WHERE account = ? AND view = ?"),
            params![account, view],
        )?;
    }
    Ok(())
}

impl Store {
    /// One page of a list from Google, and the token of the page after it,
    /// empty at the end. It all goes in at one step, so a list can't get ahead
    /// of its token. A row already in the list stays where it is.
    pub fn put_page(&self, account: &str, view: &str, files: &[File], next: &str) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        let mut at: i64 = tx.query_row(
            "SELECT coalesce(max(at), -1) + 1 FROM drive_listed WHERE account = ? AND view = ?",
            params![account, view],
            |r| r.get(0),
        )?;
        for f in files {
            put(&tx, account, f)?;
            at += tx.execute(
                "INSERT OR IGNORE INTO drive_listed(account, view, file, at) VALUES(?, ?, ?, ?)",
                params![account, view, f.id, at],
            )? as i64;
        }
        tx.execute(
            "INSERT OR REPLACE INTO drive_views(account, view, next) VALUES(?, ?, ?)",
            params![account, view, next],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The token of a list's next page: empty at its end, `None` if it was
    /// never read.
    pub fn view_next(&self, account: &str, view: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT next FROM drive_views WHERE account = ? AND view = ?",
                params![account, view],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Up to `limit` rows of a list from `skip` on. Trash shows only what's
    /// trashed and every other list leaves it out, whatever arrived.
    pub fn view_files(
        &self,
        account: &str,
        view: &str,
        skip: i64,
        limit: i64,
    ) -> Result<Vec<File>> {
        let mut q = self.conn.prepare(
            "SELECT f.* FROM drive_listed l JOIN drive_files f ON f.account = l.account AND f.id = l.file WHERE l.account = ? AND l.view = ? AND f.trashed = ? ORDER BY l.at LIMIT ? OFFSET ?",
        )?;
        let rows = q
            .query_map(params![account, view, view == TRASH, limit, skip], row)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    pub fn view_total(&self, account: &str, view: &str) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT count(*) FROM drive_listed l JOIN drive_files f ON f.account = l.account AND f.id = l.file WHERE l.account = ? AND l.view = ? AND f.trashed = ?",
            params![account, view, view == TRASH],
            |r| r.get(0),
        )?)
    }

    /// Forgets a list, so the next read starts it again.
    pub fn forget_view(&self, account: &str, view: &str) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        drop_view(&tx, account, view)?;
        tx.commit()?;
        Ok(())
    }

    /// Rows that belong to no list: a search's, or a folder on a path.
    pub fn put_files(&self, account: &str, files: &[File]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        for f in files {
            put(&tx, account, f)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn drive_file(&self, account: &str, id: &str) -> Result<Option<File>> {
        Ok(self
            .conn
            .query_row(
                "SELECT * FROM drive_files WHERE account = ? AND id = ?",
                params![account, id],
                row,
            )
            .optional()?)
    }

    /// Stored files whose name holds `words`, newest first, none of them trashed.
    pub fn search_files(&self, account: &str, words: &str, limit: i64) -> Result<Vec<File>> {
        let like = format!(
            "%{}%",
            words
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        );
        let mut q = self.conn.prepare(
            "SELECT * FROM drive_files WHERE account = ? AND trashed = 0 AND name LIKE ? ESCAPE '\\' ORDER BY changed_ms DESC, id LIMIT ?",
        )?;
        let rows = q
            .query_map(params![account, like, limit], row)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// One page of the changes sync, and the token after it, at one step. A
    /// file already stored is updated, and moved to the lists it now belongs
    /// in; one nobody has looked at and no list wants stays out. True if
    /// anything here changed.
    pub fn apply_changes(
        &self,
        account: &str,
        root: &str,
        changed: &[File],
        gone: &[String],
        token: &str,
    ) -> Result<bool> {
        let tx = self.conn.unchecked_transaction()?;
        let moved = apply(&tx, account, root, changed, gone)?;
        tx.execute("INSERT INTO state(account, key, value) VALUES(?, ?, ?) ON CONFLICT(account, key) DO UPDATE SET value = excluded.value", params![account, CHANGES_TOKEN, token])?;
        tx.commit()?;
        Ok(moved)
    }

    /// Files as a write of the person's just left them, in every list they
    /// belong to, so the list is right before the next sync. The changes token
    /// stays: the sync will bring these again, which changes nothing.
    pub fn apply_write(&self, account: &str, root: &str, files: &[File]) -> Result<bool> {
        let tx = self.conn.unchecked_transaction()?;
        let moved = apply(&tx, account, root, files, &[])?;
        tx.commit()?;
        Ok(moved)
    }
}

#[cfg(test)]
mod tests {
    use crate::{Paths, Store, key_from_seed};

    use super::*;

    const ME: &str = "me@example.com";
    const ROOT: &str = "root1";

    fn store(name: &str) -> Store {
        let dir = std::env::temp_dir().join(format!("gws-drive-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Store::open(&Paths::in_dir(dir), &key_from_seed("seed")).unwrap()
    }

    fn file(id: &str, name: &str) -> File {
        File {
            id: id.to_owned(),
            name: name.to_owned(),
            kind: "file".to_owned(),
            mime: "text/plain".to_owned(),
            parents: vec![ROOT.to_owned()],
            ..File::default()
        }
    }

    fn folder(id: &str, name: &str) -> File {
        File {
            kind: "folder".to_owned(),
            mime: FOLDER_MIME.to_owned(),
            ..file(id, name)
        }
    }

    fn ids(store: &Store, view: &str) -> Vec<String> {
        let rows = store.view_files(ME, view, 0, 100).unwrap();
        rows.into_iter().map(|f| f.id).collect()
    }

    #[test]
    fn a_list_keeps_googles_order_across_pages_and_no_row_twice() {
        let s = store("order");
        s.put_page(ME, "my", &[file("b", "zebra"), file("a", "apple")], "p2")
            .unwrap();
        // this name sorts ahead of both here, and came later from Google
        s.put_page(ME, "my", &[file("a", "apple"), file("c", "Aardvark")], "")
            .unwrap();
        assert_eq!(ids(&s, "my"), ["b", "a", "c"]);
        assert_eq!(s.view_total(ME, "my").unwrap(), 3);
        assert_eq!(s.view_next(ME, "my").unwrap().as_deref(), Some(""));
        assert_eq!(s.view_next(ME, "starred").unwrap(), None);
    }

    #[test]
    fn a_change_updates_a_row_moves_it_and_a_removal_takes_it_out() {
        let s = store("change");
        let rows = [
            folder("f", "Folder"),
            file("a", "apple"),
            file("c", "cherry"),
        ];
        s.put_page(ME, "my", &rows, "").unwrap();
        let mut renamed = file("a", "date");
        renamed.starred = true;
        assert!(
            s.apply_changes(ME, ROOT, &[renamed], &["c".to_owned()], "t2")
                .unwrap()
        );
        assert_eq!(ids(&s, "my"), ["f", "a"]);
        assert_eq!(s.drive_file(ME, "a").unwrap().unwrap().name, "date");
        assert_eq!(s.drive_file(ME, "c").unwrap(), None);
        assert_eq!(
            s.state(ME, CHANGES_TOKEN).unwrap().as_deref(),
            Some("t2"),
            "the token goes in with what it follows"
        );
    }

    #[test]
    fn a_new_file_goes_in_its_place_and_past_a_part_list_waits_for_its_page() {
        let s = store("place");
        s.put_page(ME, "my", &[folder("f", "Z"), file("a", "apple")], "")
            .unwrap();
        s.put_page(ME, "folder:x", &[file("m", "melon")], "more")
            .unwrap();
        let mut late = file("z", "zebra");
        late.parents = vec!["x".to_owned()];
        let mut early = file("b", "banana");
        early.parents = vec!["x".to_owned(), ROOT.to_owned()];
        s.apply_changes(ME, ROOT, &[early, late, folder("g", "A")], &[], "t")
            .unwrap();
        assert_eq!(ids(&s, "my"), ["g", "f", "a", "b"], "folders first");
        assert_eq!(ids(&s, "folder:x"), ["b", "m"]);
        assert_eq!(
            s.drive_file(ME, "z").unwrap(),
            None,
            "a row no list holds isn't kept"
        );
    }

    #[test]
    fn a_trashed_file_leaves_every_list_for_trash() {
        let s = store("trash");
        s.put_page(ME, "my", &[file("a", "apple")], "").unwrap();
        s.put_page(ME, RECENT, &[file("a", "apple")], "").unwrap();
        s.put_page(ME, TRASH, &[], "").unwrap();
        let mut gone = file("a", "apple");
        gone.trashed = true;
        s.apply_changes(ME, ROOT, &[gone], &[], "t").unwrap();
        assert!(ids(&s, "my").is_empty() && ids(&s, RECENT).is_empty());
        assert_eq!(ids(&s, TRASH), ["a"]);
        // and whatever a page brings, a list's filter holds
        let mut stray = file("s", "stray");
        stray.trashed = true;
        s.put_page(ME, "my", std::slice::from_ref(&stray), "")
            .unwrap();
        assert!(ids(&s, "my").is_empty());
        assert_eq!(s.view_total(ME, "my").unwrap(), 0);
    }

    #[test]
    fn a_write_moves_rows_between_lists_and_leaves_the_sync_where_it_was() {
        let s = store("write");
        s.put_page(ME, "my", &[file("a", "apple")], "").unwrap();
        s.put_page(ME, "folder:f", &[file("b", "banana")], "")
            .unwrap();
        s.put_page(ME, TRASH, &[], "").unwrap();
        s.set_state(ME, CHANGES_TOKEN, "t1").unwrap();

        let mut moved = file("a", "cherry");
        moved.parents = vec!["f".to_owned()];
        assert!(s.apply_write(ME, ROOT, &[moved.clone()]).unwrap());
        assert!(ids(&s, "my").is_empty());
        assert_eq!(ids(&s, "folder:f"), ["b", "a"], "in its place by name");

        moved.trashed = true;
        s.apply_write(ME, ROOT, &[moved]).unwrap();
        assert_eq!(ids(&s, "folder:f"), ["b"]);
        assert_eq!(ids(&s, TRASH), ["a"]);
        assert_eq!(
            s.state(ME, CHANGES_TOKEN).unwrap().as_deref(),
            Some("t1"),
            "a write says nothing about what the sync has seen"
        );
    }

    /// A list read before the store knew what a file allows has to go, or its
    /// rows would offer every press for as long as nothing changes them.
    #[test]
    fn a_store_from_before_capabilities_reads_its_lists_again() {
        let dir = std::env::temp_dir().join(format!("gws-drive-old-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let paths = Paths::in_dir(dir);
        let s = Store::open(&paths, &key_from_seed("seed")).unwrap();
        s.put_page(ME, "my", &[file("a", "apple")], "").unwrap();
        s.set_state(ME, CHANGES_TOKEN, "t1").unwrap();
        s.conn()
            .execute_batch("ALTER TABLE drive_files DROP COLUMN can; PRAGMA user_version = 7;")
            .unwrap();
        drop(s);
        let s = Store::open(&paths, &key_from_seed("seed")).unwrap();
        assert_eq!(s.view_next(ME, "my").unwrap(), None, "read again");
        assert_eq!(s.view_total(ME, "my").unwrap(), 0);
        assert_eq!(
            s.state(ME, CHANGES_TOKEN).unwrap().as_deref(),
            Some("t1"),
            "the sync goes on from where it was"
        );
    }

    #[test]
    fn what_google_lets_the_person_do_stays_with_the_row() {
        let s = store("can");
        let can = |id: &str, can: Option<Vec<String>>| File {
            can,
            ..file(id, id)
        };
        let some = Some(vec!["rename".to_owned(), "trash".to_owned()]);
        let rows = [
            can("some", some.clone()),
            can("none", Some(Vec::new())),
            can("unsaid", None),
        ];
        s.put_page(ME, "my", &rows, "").unwrap();
        let held = |id: &str| s.drive_file(ME, id).unwrap().unwrap().can;
        assert_eq!(held("some"), some);
        assert_eq!(
            held("none"),
            Some(Vec::new()),
            "nothing allowed isn't unknown"
        );
        assert_eq!(held("unsaid"), None);
    }

    #[test]
    fn recent_puts_what_changed_on_top() {
        let s = store("recent");
        let at = |id: &str, ms: i64| File {
            changed_ms: ms,
            ..file(id, id)
        };
        s.put_page(ME, RECENT, &[at("a", 20), at("b", 10)], "")
            .unwrap();
        s.apply_changes(ME, ROOT, &[at("b", 30)], &[], "t").unwrap();
        assert_eq!(ids(&s, RECENT), ["b", "a"]);
    }

    #[test]
    fn a_folder_that_goes_takes_its_list_with_it() {
        let s = store("gone");
        s.put_page(ME, "folder:f", &[file("a", "apple")], "")
            .unwrap();
        s.apply_changes(ME, ROOT, &[], &["f".to_owned()], "t")
            .unwrap();
        assert_eq!(s.view_next(ME, "folder:f").unwrap(), None);
    }

    #[test]
    fn a_search_by_name_is_literal_and_leaves_out_the_trash() {
        let s = store("search");
        let mut trashed = file("t", "report old");
        trashed.trashed = true;
        s.put_files(
            ME,
            &[file("a", "Report 100%"), file("b", "reports"), trashed],
        )
        .unwrap();
        let found = |words: &str| -> Vec<String> {
            let rows = s.search_files(ME, words, 10).unwrap();
            rows.into_iter().map(|f| f.id).collect()
        };
        assert_eq!(found("report").len(), 2);
        assert_eq!(found("100%"), ["a"]);
        assert!(found("%").len() == 1 && found("_").is_empty());
    }

    #[test]
    fn signing_out_takes_all_of_drive_and_another_account_keeps_its_own() {
        let s = store("forget");
        s.put_page(ME, "my", &[file("a", "apple")], "").unwrap();
        s.put_page("you@example.com", "my", &[file("a", "apple")], "")
            .unwrap();
        s.apply_changes(ME, ROOT, &[], &[], "t").unwrap();
        assert!(s.accounts().unwrap().contains(&ME.to_owned()));
        s.forget_account(ME).unwrap();
        assert_eq!(s.view_next(ME, "my").unwrap(), None);
        assert_eq!(s.drive_file(ME, "a").unwrap(), None);
        assert_eq!(s.state(ME, CHANGES_TOKEN).unwrap(), None);
        assert_eq!(s.view_total(ME, "my").unwrap(), 0);
        assert_eq!(s.view_total("you@example.com", "my").unwrap(), 1);
    }
}
