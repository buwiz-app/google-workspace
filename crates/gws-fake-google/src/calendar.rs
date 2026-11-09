//! Google Calendar, faked: `calendarList`, and `events` with sync tokens,
//! ETags, `sendUpdates` and Meet links. A recurring event is kept as one master
//! and laid out into instances when read, as `singleEvents=true` does, in its
//! own zone, so an instance keeps its local time across daylight saving.
//!
//! Only what the plugin needs is here. It's written from what's known of
//! Google's API: passing against it proves the plugin against this stub, not
//! against Google.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use crate::{FAKE_EMAIL, Reply, Request, civil, decode};

const DAY: i64 = 86_400_000;
const HOUR: i64 = 3_600_000;
const MINUTE: i64 = 60_000;
/// The stub's Meet links. Fake data stays off Google's domains.
const MEET: &str = "https://meet.example.com";
/// Where a list with no `timeMax` stops laying out a series that never ends.
const FOREVER: i64 = 4_102_444_800_000;

/// A calendar a fake account starts with.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FakeCalendar {
    pub id: String,
    pub name: String,
    pub primary: bool,
    /// `owner`, `writer` or `reader`.
    pub role: String,
    pub time_zone: String,
    /// Event resources as Google holds them; a series is its master.
    pub events: Vec<Value>,
}

impl FakeCalendar {
    /// An empty calendar the person owns.
    pub fn new(id: &str, name: &str, primary: bool, time_zone: &str) -> Self {
        Self {
            id: id.to_owned(),
            name: name.to_owned(),
            primary,
            role: "owner".to_owned(),
            time_zone: time_zone.to_owned(),
            events: Vec::new(),
        }
    }
}

/// A write the stub accepted, as it arrived.
#[derive(Debug, Clone, PartialEq)]
pub struct CalendarWrite {
    pub method: String,
    pub calendar: String,
    pub event: String,
    /// The `sendUpdates` it carried: who would have heard of it.
    pub send_updates: Option<String>,
    pub if_match: String,
    pub body: Value,
}

/// What a sync token stands for: the events as they were, by id with their
/// ETags, so the next read can say what's different.
#[derive(Clone)]
struct Snapshot {
    from: i64,
    to: i64,
    seen: BTreeMap<String, String>,
}

#[derive(Default)]
struct Held {
    about: FakeCalendar,
    /// A single-instance edit, by the instance's id: only what it changed.
    overrides: BTreeMap<String, Value>,
    /// Every token given out; `None` once expired.
    tokens: Vec<Option<Snapshot>>,
}

/// Every calendar of a fake account.
#[derive(Default)]
pub(crate) struct Calendars {
    list: Vec<Held>,
    next: u64,
    /// Events a page holds, whatever was asked for.
    pub(crate) page: Option<usize>,
    /// A read of a whole calendar fails, as a dropped connection would.
    pub(crate) whole_reads_fail: bool,
    pub(crate) writes: Vec<CalendarWrite>,
}

fn error(code: u16, message: &str) -> Reply {
    Reply::json(code, json!({"error": {"code": code, "message": message}}))
}

fn not_found() -> Reply {
    error(404, "Not Found")
}

/// Days since 1970-01-01 of a date (the inverse of [`civil`]).
fn days_of(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

fn date_of(s: &str) -> Option<i64> {
    let mut parts = s.get(..10)?.split('-');
    let (y, m, d) = (parts.next()?, parts.next()?, parts.next()?);
    Some(days_of(y.parse().ok()?, m.parse().ok()?, d.parse().ok()?))
}

fn date_text(days: i64) -> String {
    let (y, m, d) = civil(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// An RFC 3339 time as Unix milliseconds.
fn instant_of(s: &str) -> Option<i64> {
    let days = date_of(s)?;
    let number = |at: std::ops::Range<usize>| s.get(at)?.parse::<i64>().ok();
    let (h, m, sec) = (number(11..13)?, number(14..16)?, number(17..19)?);
    let zone = s
        .get(19..)?
        .trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
    let offset = match zone {
        "Z" => 0,
        _ => {
            let sign = if zone.starts_with('-') { -1 } else { 1 };
            let (zh, zm) = zone.get(1..)?.split_once(':')?;
            sign * (zh.parse::<i64>().ok()? * HOUR + zm.parse::<i64>().ok()? * MINUTE)
        }
    };
    Some(days * DAY + h * HOUR + m * MINUTE + sec * 1000 - offset)
}

/// `ms` as RFC 3339 at `offset` from UTC.
fn instant_text(ms: i64, offset: i64) -> String {
    let wall = ms + offset;
    let t = wall.rem_euclid(DAY);
    let (sign, off) = if offset < 0 {
        ('-', -offset)
    } else {
        ('+', offset)
    };
    format!(
        "{}T{:02}:{:02}:{:02}{sign}{:02}:{:02}",
        date_text(wall.div_euclid(DAY)),
        t / HOUR,
        t / MINUTE % 60,
        t / 1000 % 60,
        off / HOUR,
        off / MINUTE % 60
    )
}

/// The days the US changes its clocks in `year`: the second Sunday of March,
/// the first of November.
fn us_changes(year: i64) -> (i64, i64) {
    let sunday_on_or_after = |day: i64| day + (3 - day).rem_euclid(7);
    (
        sunday_on_or_after(days_of(year, 3, 8)),
        sunday_on_or_after(days_of(year, 11, 1)),
    )
}

/// A zone's offset from UTC at `ms`; `None` for a zone the stub doesn't know.
fn offset_at(zone: &str, ms: i64) -> Option<i64> {
    let us = |standard: i64| {
        let (spring, autumn) = us_changes(civil(ms.div_euclid(DAY)).0);
        // the clocks change at 02:00 local
        let (on, off) = (
            spring * DAY + 2 * HOUR - standard,
            autumn * DAY + 2 * HOUR - (standard + HOUR),
        );
        standard + if (on..off).contains(&ms) { HOUR } else { 0 }
    };
    match zone {
        "UTC" => Some(0),
        "America/New_York" => Some(us(-5 * HOUR)),
        "America/Los_Angeles" => Some(us(-8 * HOUR)),
        "Asia/Manila" => Some(8 * HOUR),
        "Asia/Tokyo" => Some(9 * HOUR),
        "Pacific/Kiritimati" => Some(14 * HOUR),
        _ => None,
    }
}

/// A local time in `zone` as Unix milliseconds.
fn utc_of(zone: &str, wall: i64) -> Option<i64> {
    let guess = wall - offset_at(zone, wall)?;
    Some(wall - offset_at(zone, guess)?)
}

/// One side of an event's time.
fn side(event: &Value, key: &str) -> Option<i64> {
    let at = event.get(key)?;
    match at.get("dateTime").and_then(Value::as_str) {
        Some(t) => instant_of(t),
        None => Some(date_of(at.get("date")?.as_str()?)? * DAY),
    }
}

fn is_cancelled(event: &Value) -> bool {
    event["status"] == "cancelled"
}

/// A PATCH: objects merge, a null takes a key away, anything else replaces.
fn merge(into: &mut Map<String, Value>, change: &Map<String, Value>) {
    for (k, v) in change {
        match (into.get_mut(k), v) {
            (_, Value::Null) => {
                into.remove(k);
            }
            (Some(Value::Object(held)), Value::Object(new)) => merge(held, new),
            _ => {
                into.insert(k.clone(), v.clone());
            }
        }
    }
}

/// The parts of an `RRULE` the stub lays out.
struct Rule {
    freq: String,
    interval: i64,
    count: Option<usize>,
    until: Option<i64>,
    /// 0 is Monday.
    days: Vec<i64>,
}

fn weekday(day: i64) -> i64 {
    (day + 3).rem_euclid(7)
}

impl Rule {
    fn of(event: &Value) -> Option<Self> {
        let line = event["recurrence"]
            .as_array()?
            .iter()
            .filter_map(Value::as_str)
            .find_map(|l| l.strip_prefix("RRULE:"))?;
        let mut rule = Self {
            freq: String::new(),
            interval: 1,
            count: None,
            until: None,
            days: Vec::new(),
        };
        for part in line.split(';') {
            let (k, v) = part.split_once('=')?;
            match k {
                "FREQ" => rule.freq = v.to_owned(),
                "INTERVAL" => rule.interval = v.parse().ok()?,
                "COUNT" => rule.count = v.parse().ok(),
                "UNTIL" => {
                    let date = format!("{}-{}-{}", v.get(..4)?, v.get(4..6)?, v.get(6..8)?);
                    rule.until = Some(match v.get(9..15) {
                        Some(t) => {
                            let time = format!("{}:{}:{}Z", &t[..2], &t[2..4], &t[4..6]);
                            instant_of(&format!("{date}T{time}"))?
                        }
                        None => (date_of(&date)? + 1) * DAY - 1,
                    });
                }
                "BYDAY" => {
                    for d in v.split(',') {
                        let at = ["MO", "TU", "WE", "TH", "FR", "SA", "SU"]
                            .iter()
                            .position(|n| *n == d)?;
                        rule.days.push(at as i64);
                    }
                }
                _ => return None,
            }
        }
        ["DAILY", "WEEKLY", "MONTHLY", "YEARLY"]
            .contains(&rule.freq.as_str())
            .then_some(rule)
    }

    /// Whether `day` is one of the series, which began on `first`.
    fn hits(&self, day: i64, first: i64) -> bool {
        let (y0, m0, d0) = civil(first);
        let (y, m, d) = civil(day);
        match self.freq.as_str() {
            "DAILY" => (day - first) % self.interval == 0,
            "WEEKLY" => {
                let monday = first - weekday(first);
                let on_day = if self.days.is_empty() {
                    weekday(day) == weekday(first)
                } else {
                    self.days.contains(&weekday(day))
                };
                on_day && ((day - monday) / 7) % self.interval == 0
            }
            "MONTHLY" => d == d0 && ((y - y0) * 12 + m - m0) % self.interval == 0,
            _ => d == d0 && m == m0 && (y - y0) % self.interval == 0,
        }
    }
}

/// A series laid out as instances that start before `to`, edits applied.
/// Cancelled ones are kept: a read of one by id still finds it.
fn lay_out(master: &Value, overrides: &BTreeMap<String, Value>, zone: &str, to: i64) -> Vec<Value> {
    let (Some(rule), Some(start), Some(end)) =
        (Rule::of(master), side(master, "start"), side(master, "end"))
    else {
        return Vec::new();
    };
    let id = master["id"].as_str().unwrap_or_default();
    let all_day = master["start"].get("date").is_some();
    let zone = master["start"]["timeZone"].as_str().unwrap_or(zone);
    let Some(wall) = offset_at(zone, start).map(|o| start + if all_day { 0 } else { o }) else {
        return Vec::new();
    };
    let (first, clock) = (wall.div_euclid(DAY), wall.rem_euclid(DAY));
    let (mut out, mut n, mut day) = (Vec::new(), 0, first);
    while day * DAY <= to + DAY {
        let this_day = day;
        day += 1;
        if !rule.hits(this_day, first) {
            continue;
        }
        let at = if all_day {
            this_day * DAY
        } else {
            match utc_of(zone, this_day * DAY + clock) {
                Some(at) => at,
                None => break,
            }
        };
        if rule.count.is_some_and(|c| n >= c) || rule.until.is_some_and(|u| at > u) {
            break;
        }
        n += 1;
        let until = at + (end - start);
        let time = |ms: i64| match all_day {
            true => json!({"date": date_text(ms.div_euclid(DAY))}),
            false => {
                let offset = offset_at(zone, ms).unwrap_or(0);
                json!({"dateTime": instant_text(ms, offset), "timeZone": zone})
            }
        };
        let stamp = {
            let (y, m, d) = civil(at.div_euclid(DAY));
            let t = at.rem_euclid(DAY);
            match all_day {
                true => format!("{y:04}{m:02}{d:02}"),
                false => format!(
                    "{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z",
                    t / HOUR,
                    t / MINUTE % 60,
                    t / 1000 % 60
                ),
            }
        };
        let mut instance = master.clone();
        instance["id"] = json!(format!("{id}_{stamp}"));
        instance["recurringEventId"] = json!(id);
        instance["originalStartTime"] = time(at);
        instance["start"] = time(at);
        instance["end"] = time(until);
        let master_tag = master["etag"]
            .as_str()
            .unwrap_or_default()
            .trim_matches('"');
        let mut tag = master_tag.to_owned();
        if let Some(obj) = instance.as_object_mut() {
            obj.remove("recurrence");
            if let Some(edit) = overrides.get(&format!("{id}_{stamp}")) {
                tag = format!("{master_tag}-{}", edit["etag"].as_str().unwrap_or_default());
                if let Some(edit) = edit.as_object() {
                    merge(obj, edit);
                }
            }
        }
        instance["etag"] = json!(format!("\"{tag}\""));
        out.push(instance);
    }
    out
}

impl Held {
    /// Every event and instance touching `from..to`, as a list shows them.
    fn listed(&self, from: i64, to: i64) -> Vec<Value> {
        let mut out = Vec::new();
        for event in self.about.events.iter().filter(|e| !is_cancelled(e)) {
            if event.get("recurrence").is_some() {
                out.extend(lay_out(event, &self.overrides, &self.about.time_zone, to));
            } else {
                out.push(event.clone());
            }
        }
        out.retain(|e| {
            let (start, end) = (side(e, "start").unwrap_or(0), side(e, "end").unwrap_or(0));
            !is_cancelled(e) && start < to && end > from
        });
        out.sort_by_key(|e| side(e, "start").unwrap_or(0));
        out
    }

    /// An instance by id, cancelled or not.
    fn instance(&self, id: &str) -> Option<Value> {
        let (series, stamp) = id.rsplit_once('_')?;
        let master = self.about.events.iter().find(|e| e["id"] == series)?;
        if is_cancelled(master) {
            return None;
        }
        let year: i64 = stamp.get(..4)?.parse().ok()?;
        lay_out(
            master,
            &self.overrides,
            &self.about.time_zone,
            days_of(year + 1, 1, 1) * DAY,
        )
        .into_iter()
        .find(|e| e["id"] == id)
    }

    fn event(&self, id: &str) -> Option<Value> {
        let held = self.about.events.iter().find(|e| e["id"] == id);
        held.cloned().or_else(|| self.instance(id))
    }

    fn snapshot(&mut self, from: i64, to: i64) -> String {
        let seen = self
            .listed(from, to)
            .iter()
            .map(|e| (text(e, "id"), text(e, "etag")))
            .collect();
        self.tokens.push(Some(Snapshot { from, to, seen }));
        format!("sync-{}", self.tokens.len() - 1)
    }
}

fn text(v: &Value, key: &str) -> String {
    v[key].as_str().unwrap_or_default().to_owned()
}

/// An event's `iCalUID`: its own, else what Google makes of its id.
fn ical_uid(event: &Value) -> String {
    match event["iCalUID"].as_str() {
        Some(uid) => uid.to_owned(),
        None => format!("{}@google.com", text(event, "id")),
    }
}

/// What Google adds to an attendee list: a response, and who `self` is.
fn tidy_attendees(event: &mut Value) {
    for a in event["attendees"].as_array_mut().into_iter().flatten() {
        if a.get("responseStatus").is_none() {
            a["responseStatus"] = json!("needsAction");
        }
        if a["email"] == FAKE_EMAIL {
            a["self"] = json!(true);
        }
    }
}

/// Whether an event's times would stand at Google.
fn times_ok(event: &Value, zone: &str) -> bool {
    let sides = ["start", "end"].iter().all(|k| {
        let at = &event[*k];
        at.get("date").is_some() != at.get("dateTime").is_some()
    });
    let timed_series =
        event.get("recurrence").is_some() && event["start"].get("dateTime").is_some();
    let named = event["start"]["timeZone"].as_str();
    sides
        && side(event, "start") < side(event, "end")
        && (!timed_series || named.is_some())
        && offset_at(named.unwrap_or(zone), 0).is_some()
        && (event.get("recurrence").is_none() || Rule::of(event).is_some())
}

impl Calendars {
    pub(crate) fn new(seed: Vec<FakeCalendar>) -> Self {
        let mut all = Self::default();
        seed.into_iter().for_each(|c| all.add_calendar(c));
        all
    }

    pub(crate) fn add_calendar(&mut self, mut about: FakeCalendar) {
        let events = std::mem::take(&mut about.events);
        self.list.push(Held {
            about,
            ..Held::default()
        });
        for event in events {
            self.add(self.list.len() - 1, event);
        }
    }

    fn tag(&mut self) -> u64 {
        self.next += 1;
        self.next
    }

    /// An event someone else made: it's there, in whatever state it was given.
    fn add(&mut self, at: usize, mut event: Value) {
        let n = self.tag();
        event["etag"] = json!(format!("\"{n}\""));
        if event.get("id").is_none() {
            event["id"] = json!(format!("evt{n}"));
        }
        if event.get("status").is_none() {
            event["status"] = json!("confirmed");
        }
        if event.get("organizer").is_none() {
            event["organizer"] = json!({"email": FAKE_EMAIL, "self": true});
        }
        tidy_attendees(&mut event);
        self.list[at].about.events.push(event);
    }

    pub(crate) fn add_event(&mut self, calendar: &str, event: Value) {
        if let Some(at) = self.at(calendar) {
            self.add(at, event);
        }
    }

    fn at(&self, calendar: &str) -> Option<usize> {
        self.list
            .iter()
            .position(|c| c.about.id == calendar || (calendar == "primary" && c.about.primary))
    }

    pub(crate) fn expire(&mut self, calendar: &str) {
        if let Some(at) = self.at(calendar) {
            self.list[at].tokens.iter_mut().for_each(|t| *t = None);
        }
    }

    /// Someone else changes an event: it gets a new ETag.
    pub(crate) fn edit(&mut self, calendar: &str, id: &str, change: &Value) -> bool {
        let Some(at) = self.at(calendar) else {
            return false;
        };
        self.apply(at, id, change)
    }

    pub(crate) fn read(&self, calendar: &str, id: &str) -> Option<Value> {
        self.list[self.at(calendar)?].event(id)
    }

    /// Writes `change` to an event, or to an instance as an edit of that one.
    fn apply(&mut self, at: usize, id: &str, change: &Value) -> bool {
        let n = self.tag();
        let held = &mut self.list[at];
        let change = change.as_object().cloned().unwrap_or_default();
        if let Some(event) = held.about.events.iter_mut().find(|e| e["id"] == id) {
            if let Some(obj) = event.as_object_mut() {
                merge(obj, &change);
            }
            event["etag"] = json!(format!("\"{n}\""));
            tidy_attendees(event);
            return true;
        }
        if held.instance(id).is_none() {
            return false;
        }
        let edit = held
            .overrides
            .entry(id.to_owned())
            .or_insert_with(|| json!({}));
        if let Some(obj) = edit.as_object_mut() {
            merge(obj, &change);
        }
        edit["etag"] = json!(n.to_string());
        tidy_attendees(edit);
        true
    }

    pub(crate) fn handle(&mut self, r: &Request, rest: &str) -> Reply {
        let segs: Vec<String> = rest
            .split('/')
            .filter(|s| !s.is_empty())
            .map(decode)
            .collect();
        let segs: Vec<&str> = segs.iter().map(String::as_str).collect();
        match (r.method.as_str(), segs.as_slice()) {
            ("GET", ["users", "me", "calendarList"]) => self.calendar_list(r),
            ("GET", ["calendars", c, "events"]) => self.events(r, c),
            ("POST", ["calendars", c, "events"]) => self.insert(r, c),
            ("GET", ["calendars", c, "events", id]) => match self.read(c, id) {
                Some(event) => Reply::json(200, event),
                None => not_found(),
            },
            ("PATCH", ["calendars", c, "events", id]) => self.patch(r, c, id),
            ("DELETE", ["calendars", c, "events", id]) => self.delete(r, c, id),
            _ => not_found(),
        }
    }

    fn calendar_list(&self, r: &Request) -> Reply {
        let start: usize = r
            .param("pageToken")
            .and_then(|t| t.parse().ok())
            .unwrap_or(0);
        let items: Vec<Value> = self
            .list
            .iter()
            .map(|c| {
                let a = &c.about;
                let mut item = json!({"id": a.id, "summary": a.name, "accessRole": a.role, "timeZone": a.time_zone, "selected": true});
                if a.primary {
                    item["primary"] = json!(true);
                }
                item
            })
            .collect();
        let max = self.page.unwrap_or(items.len().max(1));
        let page: Vec<&Value> = items.iter().skip(start).take(max).collect();
        let mut v = json!({"items": page});
        if start + max < items.len() {
            v["nextPageToken"] = json!((start + max).to_string());
        }
        Reply::json(200, v)
    }

    fn events(&mut self, r: &Request, calendar: &str) -> Reply {
        let Some(at) = self.at(calendar) else {
            return not_found();
        };
        // how an invite finds its event: by the UID its .ics carries, a series
        // as its master
        if let Some(uid) = r.param("iCalUID") {
            let held = &self.list[at].about;
            let items: Vec<&Value> = held
                .events
                .iter()
                .filter(|e| !is_cancelled(e) && ical_uid(e) == uid)
                .collect();
            return Reply::json(200, json!({"items": items, "timeZone": held.time_zone}));
        }
        if r.param("singleEvents").as_deref() != Some("true") {
            return error(400, "The stub lists events with singleEvents=true only.");
        }
        let token = r.param("syncToken");
        let bound = |k: &str| r.param(k).as_deref().and_then(instant_of);
        if token.is_some() && (r.param("timeMin").is_some() || r.param("timeMax").is_some()) {
            return error(400, "syncToken can't be used with timeMin or timeMax.");
        }
        if token.is_none() && self.whole_reads_fail {
            return error(500, "Backend error");
        }
        let held = &mut self.list[at];
        let (items, from, to) = match token {
            Some(token) => {
                let index = token
                    .strip_prefix("sync-")
                    .and_then(|n| n.parse::<usize>().ok());
                let snapshot = index.and_then(|n| held.tokens.get(n).cloned().flatten());
                let Some(was) = snapshot else {
                    return error(
                        410,
                        "Sync token is no longer valid, a full sync is required.",
                    );
                };
                let now = held.listed(was.from, was.to);
                let mut items: Vec<Value> = now
                    .iter()
                    .filter(|e| was.seen.get(&text(e, "id")) != Some(&text(e, "etag")))
                    .cloned()
                    .collect();
                for id in was.seen.keys() {
                    if !now.iter().any(|e| e["id"] == id.as_str()) {
                        items.push(json!({"id": id, "status": "cancelled"}));
                    }
                }
                (items, was.from, was.to)
            }
            None => {
                let (from, to) = (
                    bound("timeMin").unwrap_or(i64::MIN),
                    bound("timeMax").unwrap_or(FOREVER),
                );
                (held.listed(from, to), from, to)
            }
        };
        let start: usize = r
            .param("pageToken")
            .and_then(|t| t.parse().ok())
            .unwrap_or(0);
        let asked = r
            .param("maxResults")
            .and_then(|m| m.parse().ok())
            .unwrap_or(250usize);
        let max = self.page.unwrap_or(asked.min(2500));
        let page: Vec<&Value> = items.iter().skip(start).take(max).collect();
        let mut v = json!({"items": page, "timeZone": held.about.time_zone});
        if start + max < items.len() {
            v["nextPageToken"] = json!((start + max).to_string());
        } else {
            v["nextSyncToken"] = json!(held.snapshot(from, to));
        }
        Reply::json(200, v)
    }

    /// What a write's `conferenceData` becomes: only with
    /// `conferenceDataVersion=1`, as Google drops it otherwise.
    fn conference(&mut self, r: &Request, body: &mut Value) -> Result<(), Reply> {
        let asked = body.pointer("/conferenceData/createRequest").cloned();
        if let Some(obj) = body.as_object_mut() {
            obj.remove("conferenceData");
        }
        let Some(asked) =
            asked.filter(|_| r.param("conferenceDataVersion").as_deref() == Some("1"))
        else {
            return Ok(());
        };
        if asked
            .pointer("/conferenceSolutionKey/type")
            .and_then(Value::as_str)
            != Some("hangoutsMeet")
            || text(&asked, "requestId").is_empty()
        {
            return Err(error(400, "Invalid conference type value."));
        }
        let code = format!("stub-{}", self.tag());
        let uri = format!("{MEET}/{code}");
        body["hangoutLink"] = json!(uri);
        body["conferenceData"] = json!({"conferenceId": code, "conferenceSolution": {"key": {"type": "hangoutsMeet"}}, "entryPoints": [{"entryPointType": "video", "uri": uri}], "createRequest": {"requestId": asked["requestId"], "conferenceSolutionKey": asked["conferenceSolutionKey"], "status": {"statusCode": "success"}}});
        Ok(())
    }

    fn record(&mut self, r: &Request, calendar: &str, event: &str, body: Value) {
        self.writes.push(CalendarWrite {
            method: r.method.clone(),
            calendar: calendar.to_owned(),
            event: event.to_owned(),
            send_updates: r.param("sendUpdates"),
            if_match: r.if_match.clone(),
            body,
        });
    }

    fn writable(&self, at: usize) -> Result<(), Reply> {
        if self.list[at].about.role == "reader" {
            return Err(error(
                403,
                "You need to have writer access to this calendar.",
            ));
        }
        Ok(())
    }

    fn insert(&mut self, r: &Request, calendar: &str) -> Reply {
        let Some(at) = self.at(calendar) else {
            return not_found();
        };
        if let Err(e) = self.writable(at) {
            return e;
        }
        let sent: Value = serde_json::from_slice(&r.body).unwrap_or(Value::Null);
        let mut event = sent.clone();
        if let Err(e) = self.conference(r, &mut event) {
            return e;
        }
        if !times_ok(&event, &self.list[at].about.time_zone) {
            return error(400, "Bad Request");
        }
        let n = self.tag();
        let id = format!("evt{n}");
        event["id"] = json!(id);
        self.add(at, event);
        self.record(r, calendar, &id, sent);
        match self.list[at].event(&id) {
            Some(event) => Reply::json(200, event),
            None => not_found(),
        }
    }

    fn patch(&mut self, r: &Request, calendar: &str, id: &str) -> Reply {
        let Some(at) = self.at(calendar) else {
            return not_found();
        };
        if let Err(e) = self.writable(at) {
            return e;
        }
        let Some(mut held) = self.list[at].event(id) else {
            return not_found();
        };
        if !r.if_match.is_empty() && r.if_match != text(&held, "etag") {
            return error(412, "Precondition Failed");
        }
        let sent: Value = serde_json::from_slice(&r.body).unwrap_or(Value::Null);
        let mut change = sent.clone();
        if let Err(e) = self.conference(r, &mut change) {
            return e;
        }
        let instance = held.get("recurringEventId").is_some();
        if instance && change.get("recurrence").is_some() {
            return error(400, "An instance can't have a recurrence.");
        }
        if let (Some(obj), Some(new)) = (held.as_object_mut(), change.as_object()) {
            merge(obj, new);
        }
        if !times_ok(&held, &self.list[at].about.time_zone) {
            return error(400, "Bad Request");
        }
        self.apply(at, id, &change);
        self.record(r, calendar, id, sent);
        match self.list[at].event(id) {
            Some(event) => Reply::json(200, event),
            None => not_found(),
        }
    }

    fn delete(&mut self, r: &Request, calendar: &str, id: &str) -> Reply {
        let Some(at) = self.at(calendar) else {
            return not_found();
        };
        if let Err(e) = self.writable(at) {
            return e;
        }
        let Some(held) = self.list[at].event(id) else {
            return not_found();
        };
        if is_cancelled(&held) {
            return error(410, "Resource has been deleted");
        }
        if !r.if_match.is_empty() && r.if_match != text(&held, "etag") {
            return error(412, "Precondition Failed");
        }
        // it stays, cancelled: that's what a restore brings back
        self.apply(at, id, &json!({"status": "cancelled"}));
        self.record(r, calendar, id, Value::Null);
        Reply(204, "text/plain", Vec::new(), Vec::new())
    }
}

/// The person's zone in a live run's seed.
const SEED_ZONE: &str = "Asia/Manila";

/// The two calendars of a live run, with events around `now`.
pub(crate) fn around(now: i64) -> Vec<FakeCalendar> {
    let offset = 8 * HOUR;
    let today = (now + offset).div_euclid(DAY);
    // a time on `day`, counted from today, as Manila's clock shows it
    let at = |day: i64, hour: i64, minute: i64| {
        let ms = (today + day) * DAY + hour * HOUR + minute * MINUTE - offset;
        json!({"dateTime": instant_text(ms, offset), "timeZone": SEED_ZONE})
    };
    let soon = (now / (30 * MINUTE) + 3) * 30 * MINUTE;
    let time = |ms: i64| json!({"dateTime": instant_text(ms, offset), "timeZone": SEED_ZONE});
    let mut primary = FakeCalendar::new(FAKE_EMAIL, "Owner", true, SEED_ZONE);
    primary.events = vec![
        json!({"id": "seednext", "summary": "Call the roofer", "start": time(soon), "end": time(soon + HOUR), "location": "Phone"}),
        json!({"id": "seedreview", "summary": "Contract review", "description": "<p>Bring the <b>signed</b> copy.</p>", "location": "Room 4", "start": at(1, 14, 0), "end": at(1, 15, 0),
            "attendees": [{"email": FAKE_EMAIL, "responseStatus": "accepted", "organizer": true}, {"email": "ana@example.com", "displayName": "Ana Reyes", "responseStatus": "accepted"}, {"email": "ben@example.com", "displayName": "Ben Cruz", "responseStatus": "tentative"}],
            "hangoutLink": format!("{MEET}/seed-review"),
            "conferenceData": {"conferenceId": "seed-review", "entryPoints": [{"entryPointType": "video", "uri": format!("{MEET}/seed-review")}]}}),
        json!({"id": "seedclosed", "summary": "Office closed", "start": {"date": date_text(today + 2)}, "end": {"date": date_text(today + 3)}}),
        json!({"id": "seedinvite", "summary": "Quarter planning", "start": at(3, 15, 0), "end": at(3, 16, 30),
            "organizer": {"email": "cara@example.com", "displayName": "Cara Lim"},
            "attendees": [{"email": "cara@example.com", "displayName": "Cara Lim", "organizer": true, "responseStatus": "accepted"}, {"email": FAKE_EMAIL, "responseStatus": "needsAction"}]}),
    ];
    let mut team = FakeCalendar::new("team@group.example.com", "Team", false, SEED_ZONE);
    team.events = vec![
        json!({"id": "seedstandup", "summary": "Team standup", "start": at(-14, 9, 30), "end": at(-14, 9, 45), "recurrence": ["RRULE:FREQ=WEEKLY"]}),
    ];
    vec![primary, team]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn instants(master: &Value, to: i64) -> Vec<String> {
        lay_out(master, &BTreeMap::new(), "UTC", to)
            .iter()
            .map(|e| e["start"]["dateTime"].as_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn dates_and_times_read_back_as_they_were_written() {
        for day in [-1, 0, 1, 20_000] {
            let (y, m, d) = civil(day);
            assert_eq!(days_of(y, m, d), day);
        }
        let t = "2026-11-02T09:00:00-05:00";
        assert_eq!(instant_of(t), instant_of("2026-11-02T14:00:00Z"));
        assert_eq!(instant_text(instant_of(t).unwrap(), -5 * HOUR), t);
    }

    #[test]
    fn new_york_changes_its_clocks_when_it_does() {
        let at = |t: &str| offset_at("America/New_York", instant_of(t).unwrap()).unwrap();
        assert_eq!(at("2026-11-01T05:59:59Z"), -4 * HOUR);
        assert_eq!(at("2026-11-01T06:00:00Z"), -5 * HOUR);
        assert_eq!(at("2026-03-08T06:59:59Z"), -5 * HOUR);
        assert_eq!(at("2026-03-08T07:00:00Z"), -4 * HOUR);
    }

    #[test]
    fn a_weekly_series_keeps_its_local_time_and_its_count() {
        let master = json!({"id": "s", "start": {"dateTime": "2026-10-19T09:00:00-04:00", "timeZone": "America/New_York"}, "end": {"dateTime": "2026-10-19T10:00:00-04:00", "timeZone": "America/New_York"}, "recurrence": ["RRULE:FREQ=WEEKLY;COUNT=3"]});
        assert_eq!(
            instants(&master, FOREVER),
            [
                "2026-10-19T09:00:00-04:00",
                "2026-10-26T09:00:00-04:00",
                "2026-11-02T09:00:00-05:00"
            ]
        );
    }

    #[test]
    fn byday_interval_and_until_are_kept() {
        let mut master = json!({"id": "s", "start": {"dateTime": "2026-11-02T09:00:00+00:00", "timeZone": "UTC"}, "end": {"dateTime": "2026-11-02T09:30:00+00:00", "timeZone": "UTC"}, "recurrence": ["RRULE:FREQ=WEEKLY;INTERVAL=2;BYDAY=MO,WE;UNTIL=20261118T235959Z"]});
        let want = [
            "2026-11-02T09:00:00+00:00",
            "2026-11-04T09:00:00+00:00",
            "2026-11-16T09:00:00+00:00",
            "2026-11-18T09:00:00+00:00",
        ];
        assert_eq!(instants(&master, FOREVER), want);
        master["recurrence"] = json!(["EXRULE:x"]);
        assert!(instants(&master, FOREVER).is_empty());
    }

    #[test]
    fn a_patch_merges_and_a_null_takes_away() {
        let mut held = json!({"start": {"dateTime": "x", "timeZone": "UTC"}, "summary": "a"});
        let change = json!({"start": {"dateTime": null, "date": "2026-11-02"}, "summary": "b"});
        merge(held.as_object_mut().unwrap(), change.as_object().unwrap());
        assert_eq!(
            held,
            json!({"start": {"date": "2026-11-02", "timeZone": "UTC"}, "summary": "b"})
        );
    }

    #[test]
    fn the_live_seed_is_the_same_for_the_same_now_and_has_what_a_run_needs() {
        let now = 1_793_955_600_000;
        assert_eq!(around(now), around(now));
        let all: Vec<Value> = around(now).into_iter().flat_map(|c| c.events).collect();
        assert!(all.iter().any(|e| e["start"].get("date").is_some()));
        assert!(all.iter().any(|e| e.get("recurrence").is_some()));
        assert!(all.iter().any(|e| e.get("hangoutLink").is_some()));
        assert!(all.iter().any(|e| {
            e["attendees"]
                .as_array()
                .is_some_and(|a| a.iter().any(|p| p["responseStatus"] == "needsAction"))
        }));
        let text = json!(all).to_string();
        assert!(text.contains("example.com") && !text.contains("google.com"));
    }
}
