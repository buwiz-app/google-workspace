//! Calendar in the store: the person's calendars, and each one's events as the
//! sync last saw them. A timed event is matched by its UTC millis, so a time
//! zone can't change which events a range holds. An all-day event belongs to
//! its dates and to no instant, so it's matched by those.

use rusqlite::{OptionalExtension as _, params};

use crate::{Result, Store};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Calendar {
    pub id: String,
    pub name: String,
    pub primary: bool,
    pub writable: bool,
    /// The person's choice, kept across syncs. A new calendar starts with what
    /// Google says.
    pub shown: bool,
    pub time_zone: String,
}

/// One event, or one instance of a series.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Event {
    pub calendar: String,
    pub id: String,
    /// The recurring event this is an instance of; empty if it's none.
    pub series: String,
    pub title: String,
    pub place: String,
    /// Plain text, cleaned.
    pub notes: String,
    /// RFC 3339, or `YYYY-MM-DD` for an all-day event.
    pub start: String,
    /// The same; an all-day event's is Google's exclusive end date.
    pub end: String,
    pub all_day: bool,
    /// Unix milliseconds. An all-day event's are UTC midnights, for order only.
    pub start_ms: i64,
    pub end_ms: i64,
    /// `YYYY-MM-DD` for an all-day event, else empty.
    pub start_date: String,
    pub end_date: String,
    pub status: String,
    pub response: String,
    pub guests: i64,
    pub meet: bool,
    pub etag: String,
    /// What only the event's own screen shows, as JSON.
    pub detail: String,
}

/// A span of time: the instants for timed events, the dates for all-day ones.
/// `to_date` is exclusive, as an all-day event's end is.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Window {
    pub from_ms: i64,
    pub to_ms: i64,
    pub from_date: String,
    pub to_date: String,
}

fn row(r: &rusqlite::Row) -> rusqlite::Result<Event> {
    Ok(Event {
        calendar: r.get("calendar")?,
        id: r.get("id")?,
        series: r.get("series")?,
        title: r.get("title")?,
        place: r.get("place")?,
        notes: r.get("notes")?,
        start: r.get("start_text")?,
        end: r.get("end_text")?,
        all_day: r.get("all_day")?,
        start_ms: r.get("start_ms")?,
        end_ms: r.get("end_ms")?,
        start_date: r.get("start_date")?,
        end_date: r.get("end_date")?,
        status: r.get("status")?,
        response: r.get("response")?,
        guests: r.get("guests")?,
        meet: r.get("meet")?,
        etag: r.get("etag")?,
        detail: r.get("detail")?,
    })
}

/// Over the calendars the person shows.
const SHOWN: &str = "SELECT e.* FROM events e JOIN calendars c ON c.account = e.account AND c.id = e.calendar AND c.shown = 1 WHERE e.account = ?1";
/// In the window of `?2` to `?5`. An event of no length still counts at its
/// start, though nothing of it comes after the window opens.
const IN_WINDOW: &str = "((e.all_day = 0 AND e.start_ms < ?3 AND (e.end_ms > ?2 OR e.start_ms >= ?2)) OR (e.all_day = 1 AND e.start_date < ?5 AND e.end_date > ?4))";

fn put(tx: &rusqlite::Transaction, account: &str, e: &Event) -> Result<()> {
    tx.execute(
        "INSERT OR REPLACE INTO events(account, calendar, id, series, title, place, notes, start_text, end_text, all_day, start_ms, end_ms, start_date, end_date, status, response, guests, meet, etag, detail) VALUES(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        params![account, e.calendar, e.id, e.series, e.title, e.place, e.notes, e.start, e.end, e.all_day, e.start_ms, e.end_ms, e.start_date, e.end_date, e.status, e.response, e.guests, e.meet, e.etag, e.detail],
    )?;
    Ok(())
}

impl Store {
    /// Brings the list up to Google's. A calendar no longer listed goes, with
    /// its events. True if anything about the list changed.
    pub fn put_calendars(&self, account: &str, calendars: &[Calendar]) -> Result<bool> {
        let before = self.calendars(account)?;
        let tx = self.conn.unchecked_transaction()?;
        let mut changed = false;
        for old in &before {
            if !calendars.iter().any(|c| c.id == old.id) {
                changed = true;
                for table in [
                    "events WHERE account = ? AND calendar = ?",
                    "calendars WHERE account = ? AND id = ?",
                ] {
                    tx.execute(&format!("DELETE FROM {table}"), params![account, old.id])?;
                }
            }
        }
        for c in calendars {
            let old = before.iter().find(|old| old.id == c.id);
            let shown = old.map_or(c.shown, |old| old.shown);
            changed |= old != Some(&Calendar { shown, ..c.clone() });
            tx.execute(
                "INSERT INTO calendars(account, id, name, is_primary, writable, shown, time_zone) VALUES(?, ?, ?, ?, ?, ?, ?) ON CONFLICT(account, id) DO UPDATE SET name = excluded.name, is_primary = excluded.is_primary, writable = excluded.writable, time_zone = excluded.time_zone",
                params![account, c.id, c.name, c.primary, c.writable, shown, c.time_zone],
            )?;
        }
        tx.commit()?;
        Ok(changed)
    }

    /// The primary calendar first, then by name.
    pub fn calendars(&self, account: &str) -> Result<Vec<Calendar>> {
        let mut q = self.conn.prepare("SELECT id, name, is_primary, writable, shown, time_zone FROM calendars WHERE account = ? ORDER BY is_primary DESC, name, id")?;
        let rows = q.query_map([account], |r| {
            Ok(Calendar {
                id: r.get(0)?,
                name: r.get(1)?,
                primary: r.get(2)?,
                writable: r.get(3)?,
                shown: r.get(4)?,
                time_zone: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// False when there's no such calendar.
    pub fn set_shown(&self, account: &str, calendar: &str, shown: bool) -> Result<bool> {
        let done = self.conn.execute(
            "UPDATE calendars SET shown = ? WHERE account = ? AND id = ?",
            params![shown, account, calendar],
        )?;
        Ok(done > 0)
    }

    /// Where a calendar's sync left off; empty means read it whole.
    pub fn sync_token(&self, account: &str, calendar: &str) -> Result<String> {
        Ok(self
            .conn
            .query_row(
                "SELECT sync_token FROM calendars WHERE account = ? AND id = ?",
                params![account, calendar],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or_default())
    }

    pub fn clear_sync_token(&self, account: &str, calendar: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE calendars SET sync_token = '' WHERE account = ? AND id = ?",
            params![account, calendar],
        )?;
        Ok(())
    }

    /// What a sync read, in one step with where it got to, so neither is ever
    /// kept without the other. `whole` is a full read: it replaces every event
    /// of this calendar and no other's. A series in `gone` takes its instances.
    pub fn apply_events(
        &self,
        account: &str,
        calendar: &str,
        whole: bool,
        events: &[Event],
        gone: &[String],
        token: &str,
    ) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        if whole {
            tx.execute(
                "DELETE FROM events WHERE account = ? AND calendar = ?",
                params![account, calendar],
            )?;
        }
        for id in gone {
            tx.execute(
                "DELETE FROM events WHERE account = ?1 AND calendar = ?2 AND (id = ?3 OR series = ?3)",
                params![account, calendar, id],
            )?;
        }
        for e in events {
            put(&tx, account, e)?;
        }
        tx.execute(
            "UPDATE calendars SET sync_token = ? WHERE account = ? AND id = ?",
            params![token, account, calendar],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// One event a write just got back from Google.
    pub fn put_event(&self, account: &str, event: &Event) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        put(&tx, account, event)?;
        tx.commit()?;
        Ok(())
    }

    /// An event, and every instance of it if it's a series.
    pub fn remove_event(&self, account: &str, calendar: &str, id: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM events WHERE account = ?1 AND calendar = ?2 AND (id = ?3 OR series = ?3)",
            params![account, calendar, id],
        )?;
        Ok(())
    }

    pub fn event(&self, account: &str, calendar: &str, id: &str) -> Result<Option<Event>> {
        Ok(self
            .conn
            .query_row(
                "SELECT * FROM events WHERE account = ? AND calendar = ? AND id = ?",
                params![account, calendar, id],
                row,
            )
            .optional()?)
    }

    /// The events in `window`, on the calendars shown, earliest first.
    pub fn events_in(&self, account: &str, window: &Window, take: i64) -> Result<Vec<Event>> {
        let sql = format!("{SHOWN} AND {IN_WINDOW} ORDER BY e.start_ms, e.id LIMIT ?6");
        let mut q = self.conn.prepare(&sql)?;
        let rows = q.query_map(
            params![
                account,
                window.from_ms,
                window.to_ms,
                window.from_date,
                window.to_date,
                take
            ],
            row,
        )?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// What's still to come on the calendars shown: a timed event until it
    /// ends, an all-day event from the day after `today`. The person's
    /// declined events don't count.
    pub fn upcoming(
        &self,
        account: &str,
        now_ms: i64,
        today: &str,
        take: i64,
    ) -> Result<Vec<Event>> {
        let sql = format!(
            "{SHOWN} AND e.response != 'declined' AND ((e.all_day = 0 AND e.end_ms > ?2) OR (e.all_day = 1 AND e.start_date > ?3)) ORDER BY e.start_ms, e.id LIMIT ?4"
        );
        let mut q = self.conn.prepare(&sql)?;
        let rows = q.query_map(params![account, now_ms, today, take], row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// What's stored that matches `text`, on the calendars shown, in `window`
    /// if there is one.
    pub fn search_events(
        &self,
        account: &str,
        text: &str,
        window: Option<&Window>,
        take: i64,
    ) -> Result<Vec<Event>> {
        let like = format!(
            "%{}%",
            text.replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        );
        let every = Window {
            from_ms: i64::MIN,
            to_ms: i64::MAX,
            from_date: String::new(),
            to_date: "9999-99-99".to_owned(),
        };
        let w = window.unwrap_or(&every);
        let sql = format!(
            "{SHOWN} AND {IN_WINDOW} AND (e.title LIKE ?6 ESCAPE '\\' OR e.place LIKE ?6 ESCAPE '\\' OR e.notes LIKE ?6 ESCAPE '\\') ORDER BY e.start_ms, e.id LIMIT ?7"
        );
        let mut q = self.conn.prepare(&sql)?;
        let rows = q.query_map(
            params![
                account,
                w.from_ms,
                w.to_ms,
                w.from_date,
                w.to_date,
                like,
                take
            ],
            row,
        )?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Paths, key_from_seed};

    fn store(name: &str) -> Store {
        let dir = std::env::temp_dir().join(format!("gws-cal-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Store::open(&Paths::in_dir(dir), &key_from_seed("s")).unwrap()
    }

    fn calendar(id: &str) -> Calendar {
        Calendar {
            id: id.into(),
            name: format!("Calendar {id}"),
            writable: true,
            shown: true,
            time_zone: "UTC".into(),
            ..Calendar::default()
        }
    }

    fn timed(calendar: &str, id: &str, start_ms: i64, end_ms: i64) -> Event {
        Event {
            calendar: calendar.into(),
            id: id.into(),
            title: format!("Event {id}"),
            start_ms,
            end_ms,
            status: "confirmed".into(),
            response: "none".into(),
            ..Event::default()
        }
    }

    fn all_day(calendar: &str, id: &str, from: &str, to: &str) -> Event {
        Event {
            all_day: true,
            start_date: from.into(),
            end_date: to.into(),
            ..timed(calendar, id, 0, 0)
        }
    }

    fn window(from_ms: i64, to_ms: i64, from_date: &str, to_date: &str) -> Window {
        Window {
            from_ms,
            to_ms,
            from_date: from_date.into(),
            to_date: to_date.into(),
        }
    }

    fn ids(events: Vec<Event>) -> Vec<String> {
        events.into_iter().map(|e| e.id).collect()
    }

    #[test]
    fn a_range_holds_what_overlaps_it_and_an_end_is_exclusive() {
        let s = store("range");
        s.put_calendars("me", &[calendar("a")]).unwrap();
        let events = [
            timed("a", "before", 0, 10),
            timed("a", "across", 5, 15),
            timed("a", "inside", 12, 13),
            timed("a", "instant", 10, 10),
            timed("a", "after", 20, 30),
        ];
        s.apply_events("me", "a", true, &events, &[], "t").unwrap();
        let held = s
            .events_in("me", &window(10, 20, "2026-01-01", "2026-01-02"), 10)
            .unwrap();
        assert_eq!(ids(held), ["across", "instant", "inside"]);
    }

    #[test]
    fn an_all_day_event_is_matched_by_its_dates_and_its_end_is_exclusive() {
        let s = store("dates");
        s.put_calendars("me", &[calendar("a")]).unwrap();
        let events = [
            all_day("a", "yesterday", "2026-11-09", "2026-11-10"),
            all_day("a", "today", "2026-11-10", "2026-11-11"),
            all_day("a", "tomorrow", "2026-11-11", "2026-11-12"),
            all_day("a", "trip", "2026-11-08", "2026-11-13"),
        ];
        s.apply_events("me", "a", true, &events, &[], "t").unwrap();
        // the instants are ones no all-day event's UTC midnight falls in
        let held = s
            .events_in("me", &window(-5, -1, "2026-11-10", "2026-11-11"), 10)
            .unwrap();
        let mut held = ids(held);
        held.sort();
        assert_eq!(held, ["today", "trip"]);
    }

    #[test]
    fn only_the_calendars_shown_are_read_and_the_choice_outlives_a_sync() {
        let s = store("shown");
        s.put_calendars("me", &[calendar("a"), calendar("b")])
            .unwrap();
        s.apply_events("me", "a", true, &[timed("a", "1", 1, 2)], &[], "t")
            .unwrap();
        s.apply_events("me", "b", true, &[timed("b", "2", 1, 2)], &[], "t")
            .unwrap();
        assert!(s.set_shown("me", "b", false).unwrap());
        assert!(!s.set_shown("me", "nope", false).unwrap());
        let all = window(0, 9, "", "");
        assert_eq!(ids(s.events_in("me", &all, 10).unwrap()), ["1"]);
        let renamed = Calendar {
            name: "New name".into(),
            ..calendar("b")
        };
        assert!(s.put_calendars("me", &[calendar("a"), renamed]).unwrap());
        assert!(!s.calendars("me").unwrap()[1].shown, "still hidden");
        assert_eq!(s.sync_token("me", "b").unwrap(), "t", "and still synced");
    }

    #[test]
    fn a_calendar_no_longer_listed_goes_with_its_events() {
        let s = store("gone");
        s.put_calendars("me", &[calendar("a"), calendar("b")])
            .unwrap();
        s.apply_events("me", "b", true, &[timed("b", "2", 1, 2)], &[], "t")
            .unwrap();
        assert!(s.put_calendars("me", &[calendar("a")]).unwrap());
        assert_eq!(s.event("me", "b", "2").unwrap(), None);
        assert!(!s.put_calendars("me", &[calendar("a")]).unwrap());
    }

    #[test]
    fn a_whole_read_replaces_one_calendar_and_leaves_the_others() {
        let s = store("whole");
        s.put_calendars("me", &[calendar("a"), calendar("b")])
            .unwrap();
        s.apply_events("me", "a", true, &[timed("a", "old", 1, 2)], &[], "t1")
            .unwrap();
        s.apply_events("me", "b", true, &[timed("b", "kept", 1, 2)], &[], "t2")
            .unwrap();
        s.apply_events("me", "a", true, &[timed("a", "new", 1, 2)], &[], "t3")
            .unwrap();
        assert_eq!(s.event("me", "a", "old").unwrap(), None);
        assert!(s.event("me", "a", "new").unwrap().is_some());
        assert!(s.event("me", "b", "kept").unwrap().is_some());
        assert_eq!(s.sync_token("me", "b").unwrap(), "t2");
    }

    #[test]
    fn removing_a_series_takes_its_instances() {
        let s = store("series");
        s.put_calendars("me", &[calendar("a")]).unwrap();
        let instance = |id: &str| Event {
            series: "s".into(),
            ..timed("a", id, 1, 2)
        };
        let events = [instance("s_1"), instance("s_2"), timed("a", "other", 1, 2)];
        s.apply_events("me", "a", true, &events, &[], "t").unwrap();
        s.remove_event("me", "a", "s_1").unwrap();
        assert_eq!(s.event("me", "a", "s_1").unwrap(), None);
        assert!(s.event("me", "a", "s_2").unwrap().is_some());
        s.remove_event("me", "a", "s").unwrap();
        assert_eq!(s.event("me", "a", "s_2").unwrap(), None);
        assert!(s.event("me", "a", "other").unwrap().is_some());
    }

    #[test]
    fn upcoming_skips_what_ended_what_was_declined_and_todays_all_day() {
        let s = store("next");
        s.put_calendars("me", &[calendar("a")]).unwrap();
        let declined = Event {
            response: "declined".into(),
            ..timed("a", "declined", 150, 160)
        };
        let events = [
            timed("a", "ended", 10, 20),
            timed("a", "running", 90, 120),
            declined,
            timed("a", "later", 200, 210),
            all_day("a", "today", "2026-11-10", "2026-11-11"),
            all_day("a", "tomorrow", "2026-11-11", "2026-11-12"),
        ];
        s.apply_events("me", "a", true, &events, &[], "t").unwrap();
        let next = s.upcoming("me", 100, "2026-11-10", 10).unwrap();
        let mut next = ids(next);
        next.sort();
        assert_eq!(next, ["later", "running", "tomorrow"]);
    }

    #[test]
    fn search_treats_its_text_as_text_and_keeps_to_its_window() {
        let s = store("search");
        s.put_calendars("me", &[calendar("a")]).unwrap();
        let event = Event {
            place: "Room 4".into(),
            notes: "Bring the roof quote".into(),
            ..timed("a", "1", 100, 200)
        };
        s.apply_events("me", "a", true, &[event], &[], "t").unwrap();
        for text in ["Event 1", "room", "ROOF"] {
            assert_eq!(s.search_events("me", text, None, 5).unwrap().len(), 1);
        }
        assert!(s.search_events("me", "%", None, 5).unwrap().is_empty());
        let later = window(500, 600, "", "");
        assert!(
            s.search_events("me", "room", Some(&later), 5)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn an_account_is_forgotten_whole_and_the_others_keep_theirs() {
        let s = store("forget");
        s.put_calendars("me", &[calendar("a")]).unwrap();
        s.put_calendars("you", &[calendar("a")]).unwrap();
        s.apply_events("me", "a", true, &[timed("a", "1", 1, 2)], &[], "t")
            .unwrap();
        s.apply_events("you", "a", true, &[timed("a", "1", 1, 2)], &[], "t")
            .unwrap();
        let mut accounts = s.accounts().unwrap();
        accounts.sort();
        assert_eq!(accounts, ["me", "you"]);
        s.forget_account("me").unwrap();
        assert!(s.calendars("me").unwrap().is_empty());
        assert_eq!(s.event("me", "a", "1").unwrap(), None);
        assert!(s.event("you", "a", "1").unwrap().is_some());
    }
}
