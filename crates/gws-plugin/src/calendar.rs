//! Calendar: Google's API (`calendar/v3`), a sync into the store that rides on
//! Mail's, and the tools. The screens read what's stored, in UTC millis, so a
//! zone can't change which events a range holds and a lost connection shows
//! old events, not a blank week.
//!
//! Nothing here reaches a guest but `sendUpdates`, and that's only ever set on
//! a person's write. The model reads and fills a draft: a draft calls no
//! Google, and only `gws_event_save`, which is app only, writes.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use chrono::{
    DateTime, FixedOffset, Local, NaiveDate, NaiveDateTime, NaiveTime, SecondsFormat, TimeDelta,
    TimeZone,
};
use chrono_tz::Tz;
use gws_store::calendar::{Calendar, Event, Window};
use serde_json::{Map, Value, json};

use crate::accounts::Bearer;
use crate::gmail::{BODY_MAX, Failure, HEADER_MAX, cap_bytes};
use crate::mail::{Subject, now_ms, signed_in, with_own_store, with_store};
use crate::tools::{
    CAL_CHANGES, CAL_LIST, CAL_NEXT, CAL_PEOPLE, CAL_RANGE, CAL_SHOW, EVENT_DELETE, EVENT_DRAFT,
    EVENT_DRAFT_READ, EVENT_READ, EVENT_RESTORE, EVENT_RSVP, EVENT_SAVE, SEARCH_EVENTS,
    UNTRUSTED_EVENT, fail, failed, ok, plain_error,
};
use crate::{Core, clean, js};

/// The production API base.
pub const CALENDAR_API: &str = "https://www.googleapis.com/calendar/v3";
const TIMEOUT: Duration = Duration::from_secs(30);
/// A page of events, attendees and all.
const PAGE_MAX: u64 = 32 * 1024 * 1024;
/// The most events one page asks for, which is the most Google gives.
const PAGE: &str = "2500";
/// What the first read of a calendar covers.
const DAYS_BACK: i64 = 365;
const DAYS_AHEAD: i64 = 730;
/// The most rows a range answers: a result stops at 1 MiB.
const RANGE_MAX: i64 = 2000;
const SEARCH_MAX: i64 = 25;
/// Rows looked at for the next event: enough for the order to come out right
/// with all-day events mixed in.
const NEXT_LOOK: i64 = 50;
/// How many deletes can still be undone.
const DELETES_MAX: usize = 20;
/// The most pages an event's UID is looked for on.
const UID_PAGES: usize = 20;
const SEQ: &str = "cal_seq";
const STATUS: &str = "cal_status";

/// What Google said to a call a person's press hangs on.
#[derive(Debug, PartialEq)]
pub enum Answer {
    Done(Value),
    NotFound,
    /// 410: a sync token too old, or an event already deleted.
    Gone,
    /// 412: someone else changed it.
    Changed,
    /// 400, with why.
    Invalid(String),
}

/// A write's conditions.
pub struct Write<'a> {
    /// Whether guests hear of it: `sendUpdates`, never left to Google's default.
    pub notify: bool,
    /// What the event must still be, for a write that only stands if it is.
    pub if_match: &'a str,
}

/// The Calendar API at a base URL. Like Gmail's, it follows no redirects, so
/// the token only ever goes to the API base.
#[derive(Clone)]
pub struct CalendarApi {
    agent: ureq::Agent,
    base: String,
}

/// `text` as one path segment. Calendar ids hold `@` and `#`, and a dot stays
/// out too, so `..` can never climb out of the path.
fn segment(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn send_updates(notify: bool) -> &'static str {
    if notify { "all" } else { "none" }
}

impl CalendarApi {
    /// `base` is [`CALENDAR_API`] in production; tests pass a local server.
    pub fn new(base: &str) -> Self {
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .timeout_global(Some(TIMEOUT))
            .user_agent("buwiz-gws")
            .build();
        Self {
            agent: config.new_agent(),
            base: base.trim_end_matches('/').to_owned(),
        }
    }

    fn answer(
        res: Result<ureq::http::Response<ureq::Body>, ureq::Error>,
    ) -> Result<Answer, Failure> {
        let mut res = res.map_err(|e| {
            Failure::Offline(format!(
                "could not reach Calendar: {}",
                js::slice_utf16(&e.to_string(), 200)
            ))
        })?;
        let status = res.status().as_u16();
        let body = res
            .body_mut()
            .with_config()
            .limit(PAGE_MAX)
            .read_to_vec()
            .map_err(|e| {
                Failure::Plain(format!(
                    "Calendar's answer could not be read: {}",
                    js::slice_utf16(&e.to_string(), 200)
                ))
            })?;
        let why = || {
            let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            v.pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("no reason given")
                .to_owned()
        };
        match status {
            // a delete answers with nothing
            200 | 204 if body.is_empty() => Ok(Answer::Done(Value::Null)),
            200 => serde_json::from_slice(&body)
                .map(Answer::Done)
                .map_err(|_| Failure::Plain("Calendar's answer is not JSON".to_owned())),
            400 => Ok(Answer::Invalid(js::slice_utf16(&why(), 300))),
            401 => Err(Failure::AccountRequired),
            404 => Ok(Answer::NotFound),
            410 => Ok(Answer::Gone),
            412 => Ok(Answer::Changed),
            403 if why().contains("insufficient") => Err(Failure::NeedsAccess),
            429 => Err(Failure::Plain(
                "Calendar is busy right now. Try again in a moment.".to_owned(),
            )),
            500.. => Err(Failure::Plain(
                "Calendar had a problem just now. Try again.".to_owned(),
            )),
            _ => Err(Failure::Plain(format!(
                "Calendar didn't accept that: {}",
                js::slice_utf16(&why(), 300)
            ))),
        }
    }

    fn get(&self, auth: &Bearer, path: &str, query: &[(&str, String)]) -> Result<Answer, Failure> {
        let mut req = self
            .agent
            .get(format!("{}/{path}", self.base))
            .header("authorization", auth.header())
            .header("accept", "application/json");
        for (k, v) in query {
            req = req.query(*k, v);
        }
        Self::answer(req.call())
    }

    /// Every calendar on the person's list.
    pub fn calendars(&self, auth: &Bearer) -> Result<Vec<Value>, Failure> {
        let (mut all, mut page) = (Vec::new(), None::<String>);
        loop {
            let query: Vec<(&str, String)> =
                page.take().map(|p| ("pageToken", p)).into_iter().collect();
            let Answer::Done(v) = self.get(auth, "users/me/calendarList", &query)? else {
                return Err(Failure::Plain(
                    "Calendar has no list of calendars.".to_owned(),
                ));
            };
            all.extend(
                v.get("items")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default(),
            );
            match v.get("nextPageToken").and_then(Value::as_str) {
                Some(next) => page = Some(next.to_owned()),
                None => return Ok(all),
            }
        }
    }

    /// A calendar's events, each series laid out into instances: what changed
    /// since `token`, or with none, all of them in the window around now. With
    /// the token to carry on from. `None` when Google has forgotten the token.
    pub fn events(
        &self,
        auth: &Bearer,
        calendar: &str,
        token: Option<&str>,
    ) -> Result<Option<(Vec<Value>, String)>, Failure> {
        let path = format!("calendars/{}/events", segment(calendar));
        let (mut items, mut page) = (Vec::new(), None::<String>);
        loop {
            let mut query = vec![
                ("singleEvents", "true".to_owned()),
                ("maxResults", PAGE.to_owned()),
            ];
            match token {
                // Google refuses a time range beside a sync token
                Some(token) => query.push(("syncToken", token.to_owned())),
                None => {
                    let edge = |days: i64| {
                        let at = DateTime::from_timestamp_millis(now_ms() + days * 86_400_000);
                        at.unwrap_or_default()
                            .to_rfc3339_opts(SecondsFormat::Secs, true)
                    };
                    query.push(("timeMin", edge(-DAYS_BACK)));
                    query.push(("timeMax", edge(DAYS_AHEAD)));
                }
            }
            query.extend(page.take().map(|p| ("pageToken", p)));
            let v = match self.get(auth, &path, &query)? {
                Answer::Done(v) => v,
                Answer::Gone => return Ok(None),
                Answer::NotFound => {
                    return Err(Failure::Plain("Calendar has no such calendar.".to_owned()));
                }
                _ => {
                    return Err(Failure::Plain(
                        "Calendar refused to list events.".to_owned(),
                    ));
                }
            };
            items.extend(
                v.get("items")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default(),
            );
            match v.get("nextPageToken").and_then(Value::as_str) {
                Some(next) => page = Some(next.to_owned()),
                None => {
                    let next = v.get("nextSyncToken").and_then(Value::as_str);
                    return Ok(Some((items, next.unwrap_or_default().to_owned())));
                }
            }
        }
    }

    /// One event as Google holds it now; `None` when it has no such event.
    pub fn event(&self, auth: &Bearer, calendar: &str, id: &str) -> Result<Option<Value>, Failure> {
        let path = format!("calendars/{}/events/{}", segment(calendar), segment(id));
        match self.get(auth, &path, &[])? {
            Answer::Done(v) => Ok(Some(v)),
            _ => Ok(None),
        }
    }

    /// The event an invite's `.ics` names, by its UID: a series comes as its
    /// master. `None` when the calendar has no such event, or only a cancelled one.
    pub fn by_ical_uid(
        &self,
        auth: &Bearer,
        calendar: &str,
        uid: &str,
    ) -> Result<Option<Value>, Failure> {
        let path = format!("calendars/{}/events", segment(calendar));
        // a series with many changed instances can run to pages, and the
        // series itself may be on any of them
        let (mut first, mut page) = (None, None::<String>);
        for _ in 0..UID_PAGES {
            let mut query = vec![("iCalUID", uid.to_owned()), ("maxResults", PAGE.to_owned())];
            query.extend(page.take().map(|p| ("pageToken", p)));
            let Answer::Done(v) = self.get(auth, &path, &query)? else {
                return Ok(first);
            };
            let items = v.get("items").and_then(Value::as_array);
            for e in items.into_iter().flatten() {
                if text(e, "status") == "cancelled" {
                    continue;
                }
                if text(e, "recurringEventId").is_empty() {
                    return Ok(Some(e.clone()));
                }
                first = first.or_else(|| Some(e.clone()));
            }
            match v.get("nextPageToken").and_then(Value::as_str) {
                Some(next) => page = Some(next.to_owned()),
                None => break,
            }
        }
        Ok(first)
    }

    pub fn insert(
        &self,
        auth: &Bearer,
        calendar: &str,
        body: &Value,
        notify: bool,
    ) -> Result<Answer, Failure> {
        let mut req = self
            .agent
            .post(format!(
                "{}/calendars/{}/events",
                self.base,
                segment(calendar)
            ))
            .query("sendUpdates", send_updates(notify))
            .header("authorization", auth.header())
            .header("accept", "application/json")
            .header("content-type", "application/json");
        if body.get("conferenceData").is_some() {
            req = req.query("conferenceDataVersion", "1");
        }
        Self::answer(req.send(body.to_string().as_bytes()))
    }

    pub fn patch(
        &self,
        auth: &Bearer,
        calendar: &str,
        id: &str,
        body: &Value,
        write: &Write,
    ) -> Result<Answer, Failure> {
        let url = format!(
            "{}/calendars/{}/events/{}",
            self.base,
            segment(calendar),
            segment(id)
        );
        let mut req = self
            .agent
            .patch(url)
            .query("sendUpdates", send_updates(write.notify))
            .header("authorization", auth.header())
            .header("accept", "application/json")
            .header("content-type", "application/json");
        if body.get("conferenceData").is_some() {
            req = req.query("conferenceDataVersion", "1");
        }
        if !write.if_match.is_empty() {
            req = req.header("if-match", write.if_match);
        }
        Self::answer(req.send(body.to_string().as_bytes()))
    }

    pub fn delete(
        &self,
        auth: &Bearer,
        calendar: &str,
        id: &str,
        notify: bool,
    ) -> Result<Answer, Failure> {
        let url = format!(
            "{}/calendars/{}/events/{}",
            self.base,
            segment(calendar),
            segment(id)
        );
        let req = self
            .agent
            .delete(url)
            .query("sendUpdates", send_updates(notify))
            .header("authorization", auth.header());
        Self::answer(req.call())
    }
}

fn text(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn millis(time: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(time)
        .ok()
        .map(|t| t.timestamp_millis())
}

fn date(text: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(text, "%Y-%m-%d").ok()
}

/// A date's UTC midnight.
fn date_ms(day: NaiveDate) -> i64 {
    day.and_time(NaiveTime::MIN).and_utc().timestamp_millis()
}

/// An event's description as plain text. A description may be HTML or plain:
/// plain keeps its lines, which reading it as HTML would run together. Drive's
/// files have one too.
pub(crate) fn notes_of(event: &Value) -> String {
    let raw = text(event, "description");
    let html = raw.contains('<') && raw.contains('>');
    let notes = if html { clean::visible_text(&raw) } else { raw };
    cap_bytes(&notes, BODY_MAX)
}

fn attendees(event: &Value) -> &[Value] {
    event
        .get("attendees")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

fn is_self(attendee: &Value, account: &str) -> bool {
    attendee.get("self") == Some(&Value::Bool(true))
        || text(attendee, "email").eq_ignore_ascii_case(account)
}

/// A room isn't a guest, and nor is the person.
fn is_guest(attendee: &Value, account: &str) -> bool {
    !is_self(attendee, account) && attendee.get("resource") != Some(&Value::Bool(true))
}

/// Who an event involves besides the person.
fn guests_of<'a>(event: &'a Value, account: &str) -> Vec<&'a Value> {
    attendees(event)
        .iter()
        .filter(|a| is_guest(a, account))
        .collect()
}

/// Those a form names who could be guests of `event`. A form read from the
/// event can hold its room, or the person under another address: they're on
/// the event already as what they are.
fn named<'a>(others: &[&'a String], event: &Value, account: &str) -> Vec<&'a String> {
    let on_it_already = |g: &str| {
        attendees(event)
            .iter()
            .any(|a| !is_guest(a, account) && text(a, "email").eq_ignore_ascii_case(g))
    };
    others
        .iter()
        .filter(|g| !on_it_already(g))
        .copied()
        .collect()
}

/// The video link, only as https: it's a link a person is sent to.
fn meet_url(event: &Value) -> String {
    let entry = event
        .pointer("/conferenceData/entryPoints")
        .and_then(Value::as_array)
        .and_then(|points| points.iter().find(|p| p["entryPointType"] == "video"))
        .map(|p| text(p, "uri"));
    let hangout = Some(text(event, "hangoutLink")).filter(|l| !l.is_empty());
    hangout
        .or(entry)
        .filter(|l| l.starts_with("https://"))
        .unwrap_or_default()
}

/// A stored event from Google's resource; `None` for one with no usable times.
pub fn event_of(calendar: &str, account: &str, v: &Value) -> Option<Event> {
    let (start, end) = (v.get("start")?, v.get("end")?);
    let all_day = start.get("date").is_some();
    let (start_text, end_text, start_ms, end_ms, start_date, end_date) = if all_day {
        let (from, to) = (text(start, "date"), text(end, "date"));
        let (from_ms, to_ms) = (date_ms(date(&from)?), date_ms(date(&to)?));
        (from.clone(), to.clone(), from_ms, to_ms, from, to)
    } else {
        let (from, to) = (text(start, "dateTime"), text(end, "dateTime"));
        let (from_ms, to_ms) = (millis(&from)?, millis(&to)?);
        (from, to, from_ms, to_ms, String::new(), String::new())
    };
    let mine = attendees(v).iter().find(|a| is_self(a, account));
    let organizer = v.get("organizer").unwrap_or(&Value::Null);
    let own = organizer.get("self") == Some(&Value::Bool(true))
        || text(organizer, "email").eq_ignore_ascii_case(calendar);
    let people: Vec<Value> = attendees(v)
        .iter()
        .map(|a| {
            let flag = |k: &str| a.get(k) == Some(&Value::Bool(true));
            json!({"email": text(a, "email"), "name": text(a, "displayName"), "response": text(a, "responseStatus"), "optional": flag("optional"), "organizer": flag("organizer"), "self": is_self(a, account), "resource": flag("resource")})
        })
        .collect();
    let named = Some(text(organizer, "displayName")).filter(|n| !n.is_empty());
    let detail = json!({
        "organizer": named.unwrap_or_else(|| text(organizer, "email")),
        "attendees": people,
        "meet_url": meet_url(v),
        "time_zone": text(start, "timeZone"),
        "editable": own || v.get("guestsCanModify") == Some(&Value::Bool(true)),
    });
    let status = Some(text(v, "status")).filter(|s| !s.is_empty());
    Some(Event {
        calendar: calendar.to_owned(),
        id: text(v, "id"),
        series: text(v, "recurringEventId"),
        title: cap_bytes(&text(v, "summary"), HEADER_MAX),
        place: cap_bytes(&text(v, "location"), HEADER_MAX),
        notes: notes_of(v),
        start: start_text,
        end: end_text,
        all_day,
        start_ms,
        end_ms,
        start_date,
        end_date,
        status: status.unwrap_or_else(|| "confirmed".to_owned()),
        response: mine.map_or_else(|| "none".to_owned(), |a| text(a, "responseStatus")),
        guests: guests_of(v, account).len() as i64,
        meet: !meet_url(v).is_empty(),
        etag: text(v, "etag"),
        detail: detail.to_string(),
    })
}

fn calendar_of(v: &Value) -> Option<Calendar> {
    let role = text(v, "accessRole");
    let name = Some(text(v, "summaryOverride"))
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| text(v, "summary"));
    let primary = v.get("primary") == Some(&Value::Bool(true));
    Some(Calendar {
        id: Some(text(v, "id")).filter(|id| !id.is_empty())?,
        name: cap_bytes(&name, HEADER_MAX),
        primary,
        writable: role == "owner" || role == "writer",
        shown: primary || v.get("selected") == Some(&Value::Bool(true)),
        time_zone: text(v, "timeZone"),
    })
}

fn bump(core: &Core, account: &str) -> Result<(), Failure> {
    with_own_store(core, account, |store| {
        let next = store
            .state(account, SEQ)?
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0)
            + 1;
        store.set_state(account, SEQ, &next.to_string())
    })
}

/// Brings one calendar up to date; true if anything changed. A 410 reads it
/// whole again. Everything is fetched first and goes in at one step, so a
/// connection lost halfway leaves the old events, not half of the new ones.
fn sync_calendar(core: &Core, auth: &Bearer, account: &str, id: &str) -> Result<bool, Failure> {
    let token = with_store(core, |store| store.sync_token(account, id))?;
    let held = Some(token.as_str()).filter(|t| !t.is_empty());
    let delta = match held {
        Some(token) => core.calendar.events(auth, id, Some(token))?,
        None => None,
    };
    let whole = delta.is_none();
    let (items, next) = match delta {
        Some(delta) => delta,
        None => {
            if held.is_some() {
                with_own_store(core, account, |store| store.clear_sync_token(account, id))?;
            }
            core.calendar
                .events(auth, id, None)?
                .ok_or_else(|| Failure::Plain("Calendar wouldn't read that calendar.".to_owned()))?
        }
    };
    let (mut events, mut gone) = (Vec::with_capacity(items.len()), Vec::new());
    for item in &items {
        if text(item, "status") == "cancelled" {
            gone.push(text(item, "id"));
        } else if let Some(event) = event_of(id, account, item) {
            events.push(event);
        }
    }
    with_own_store(core, account, |store| {
        store.apply_events(account, id, whole, &events, &gone, &next)
    })?;
    Ok(whole || !items.is_empty())
}

fn sync_once(core: &Core, auth: &Bearer, account: &str) -> Result<bool, Failure> {
    let listed = core.calendar.calendars(auth)?;
    let calendars: Vec<Calendar> = listed.iter().filter_map(calendar_of).collect();
    let mut changed = with_own_store(core, account, |store| {
        store.put_calendars(account, &calendars)
    })?;
    // one calendar that won't read must not stop the others
    let mut failure = None;
    for c in &calendars {
        match sync_calendar(core, auth, account, &c.id) {
            Ok(moved) => changed |= moved,
            Err(e @ (Failure::Offline(_) | Failure::AccountRequired)) => return Err(e),
            Err(e) => failure = Some(e),
        }
    }
    match failure {
        // what did come in still has to show
        Some(e) if changed => {
            bump(core, account)?;
            Err(e)
        }
        Some(e) => Err(e),
        None => Ok(changed),
    }
}

/// The calendar side of a sync, its outcome kept apart from Mail's: a sign-in
/// that doesn't cover Calendar leaves mail as it was.
pub fn sync(core: &Core, auth: &Bearer, account: &str) -> Result<(), Failure> {
    let _one_at_a_time = core.cal_sync.lock().unwrap_or_else(|e| e.into_inner());
    let result = sync_once(core, auth, account);
    let status = match &result {
        Ok(_) => "ok",
        Err(Failure::Offline(_)) => "offline",
        Err(Failure::NeedsAccess) => "needs_access",
        Err(Failure::AccountRequired) => return Err(Failure::AccountRequired),
        Err(Failure::Plain(_)) => "failed",
    };
    with_own_store(core, account, |store| {
        store.set_state(account, STATUS, status)
    })?;
    if matches!(result, Ok(true)) {
        bump(core, account)?;
    }
    result.map(drop)
}

/// Whether the screens are looking at saved events: true when the last check
/// found no connection. The very first read does its own sync, so a person
/// opening Calendar straight after signing in isn't shown an empty week.
fn saved(core: &Core, auth: &Bearer, account: &str) -> Result<bool, Failure> {
    let status = || with_store(core, |store| store.state(account, STATUS));
    if status()?.is_none()
        && let Err(Failure::AccountRequired) = sync(core, auth, account)
    {
        return Err(Failure::AccountRequired);
    }
    match status()?.as_deref() {
        Some("offline") => Ok(true),
        Some("needs_access") => Err(Failure::NeedsAccess),
        _ => Ok(false),
    }
}

fn row_json(e: &Event) -> Value {
    json!({"id": e.id, "calendar": e.calendar, "title": e.title, "start": e.start, "end": e.end, "all_day": e.all_day, "where": e.place, "status": e.status, "response": e.response, "guests": e.guests, "meet": e.meet, "recurring": !e.series.is_empty()})
}

fn rows(events: &[Event]) -> Vec<Value> {
    events.iter().map(row_json).collect()
}

/// Puts events in order of start, for someone `offset` milliseconds from UTC:
/// an all-day event starts at that person's midnight, ahead of what starts then.
fn in_order(events: &mut [Event], offset: i64) {
    events.sort_by_key(|e| {
        let start = if e.all_day {
            e.start_ms - offset
        } else {
            e.start_ms
        };
        (start, !e.all_day, e.id.clone())
    });
}

fn time_in(args: &Value, key: &str) -> Result<Option<DateTime<FixedOffset>>, ()> {
    match js::trim(&js::string_or_empty(args.get(key))) {
        "" => Ok(None),
        t => DateTime::parse_from_rfc3339(t).map(Some).map_err(drop),
    }
}

/// The window from `from` up to `to`. An all-day event belongs to dates, so
/// its dates here are the ones those times fall on where they were written:
/// a day asked for in Manila and in New York holds the same all-day events.
/// `end` is the first date left out, from a caller who asked for whole days.
/// Only that caller knows: where the clocks skip midnight, a day starts at
/// 01:00, and from the time alone that's a day partly in the window.
fn window_of(
    from: DateTime<FixedOffset>,
    to: DateTime<FixedOffset>,
    end: Option<NaiveDate>,
) -> Option<Window> {
    let last = to.date_naive();
    let to_date = match end {
        Some(end) => end,
        None if to.time() == NaiveTime::MIN => last,
        None => last.succ_opt()?,
    };
    (from < to).then(|| Window {
        from_ms: from.timestamp_millis(),
        to_ms: to.timestamp_millis(),
        from_date: from.date_naive().to_string(),
        to_date: to_date.to_string(),
    })
}

fn refusal(code: &str, why: &str) -> Value {
    ok(json!({"refused": code, "message": why}))
}

fn bad_args(why: &str) -> Value {
    refusal("invalid_args", why)
}

fn gone() -> Value {
    refusal("not_found", "That event is gone.")
}

fn changed() -> Value {
    refusal(
        "changed",
        "Someone else changed this event. Open it again to see what they did.",
    )
}

/// What a write's refusal says to the screen; `None` if it went through.
fn refused(answer: &Answer) -> Option<Value> {
    match answer {
        Answer::Done(_) => None,
        Answer::NotFound | Answer::Gone => Some(gone()),
        Answer::Changed => Some(changed()),
        Answer::Invalid(why) => Some(bad_args(why)),
    }
}

fn range(core: &Core, args: &Value) -> Result<Value, Failure> {
    let (auth, account) = signed_in(core)?;
    let (Ok(Some(from)), Ok(Some(to))) = (time_in(args, "from"), time_in(args, "to")) else {
        return Ok(bad_args(
            "A range needs a start and an end, with their offsets.",
        ));
    };
    let end = match js::trim(&js::string_or_empty(args.get("to_date"))) {
        "" => None,
        day => match date(day) {
            Some(day) => Some(day),
            None => return Ok(bad_args("A range's to_date is YYYY-MM-DD.")),
        },
    };
    let Some(window) = window_of(from, to, end) else {
        return Ok(bad_args("A range has to end after it starts."));
    };
    let saved = saved(core, &auth, &account)?;
    let mut events = with_store(core, |store| store.events_in(&account, &window, RANGE_MAX))?;
    in_order(
        &mut events,
        i64::from(from.offset().local_minus_utc()) * 1000,
    );
    Ok(ok(json!({"events": rows(&events), "saved": saved})))
}

fn list(core: &Core) -> Result<Value, Failure> {
    let (auth, account) = signed_in(core)?;
    saved(core, &auth, &account)?;
    let calendars = with_store(core, |store| store.calendars(&account))?;
    let list: Vec<Value> = calendars
        .iter()
        .map(|c| json!({"id": c.id, "name": c.name, "primary": c.primary, "writable": c.writable, "shown": c.shown, "time_zone": c.time_zone}))
        .collect();
    Ok(ok(json!({"calendars": list})))
}

fn show(core: &Core, args: &Value) -> Result<Value, Failure> {
    let (_, account) = signed_in(core)?;
    let calendar = js::string_or_empty(args.get("calendar"));
    let Some(shown) = args.get("shown").and_then(Value::as_bool) else {
        return Ok(bad_args("Say whether to show it."));
    };
    let found = with_own_store(core, &account, |store| {
        store.set_shown(&account, &calendar, shown)
    })?;
    if !found {
        return Ok(refusal("not_found", "That calendar is gone."));
    }
    // what a range holds has changed, though no event has
    bump(core, &account)?;
    Ok(ok(json!({"ok": true})))
}

fn changes(core: &Core) -> Result<Value, Failure> {
    let (_, account) = signed_in(core)?;
    let (seq, status) = with_store(core, |store| {
        Ok((
            store
                .state(&account, SEQ)?
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(0),
            store.state(&account, STATUS)?,
        ))
    })?;
    Ok(ok(
        json!({"seq": seq, "status": status.unwrap_or_else(|| "syncing".to_owned())}),
    ))
}

fn next(core: &Core) -> Result<Value, Failure> {
    let (auth, account) = signed_in(core)?;
    saved(core, &auth, &account)?;
    let now = Local::now();
    let today = now.date_naive().to_string();
    let mut events = with_store(core, |store| {
        store.upcoming(&account, now_ms(), &today, NEXT_LOOK)
    })?;
    in_order(
        &mut events,
        i64::from(now.offset().local_minus_utc()) * 1000,
    );
    let mut soonest = events.iter().map(row_json);
    Ok(ok(
        json!({"event": soonest.next(), "after": soonest.take(2).collect::<Vec<_>>()}),
    ))
}

fn search(core: &Core, args: &Value) -> Result<Value, Failure> {
    let query = js::trim(&js::string_or_empty(args.get("query"))).to_owned();
    if query.is_empty() {
        return Ok(plain_error("gws_search_events needs a query"));
    }
    let (Ok(from), Ok(to)) = (time_in(args, "from"), time_in(args, "to")) else {
        return Ok(plain_error("from and to are RFC 3339 times with an offset"));
    };
    let window = match (from, to) {
        (None, None) => None,
        (from, to) => {
            let open = DateTime::<FixedOffset>::from(DateTime::UNIX_EPOCH);
            let far = open + TimeDelta::days(100_000);
            let Some(window) = window_of(from.unwrap_or(open), to.unwrap_or(far), None) else {
                return Ok(plain_error("to has to come after from"));
            };
            Some(window)
        }
    };
    let (auth, account) = signed_in(core)?;
    saved(core, &auth, &account)?;
    let found = with_store(core, |store| {
        store.search_events(&account, &query, window.as_ref(), SEARCH_MAX)
    })?;
    Ok(ok(
        json!({"items": rows(&found), "untrusted": UNTRUSTED_EVENT}),
    ))
}

/// The `RRULE` line of an event's recurrence, or "".
fn rrule(event: &Value) -> String {
    event
        .get("recurrence")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .find(|l| l.starts_with("RRULE:"))
        .unwrap_or_default()
        .to_owned()
}

fn ids(args: &Value) -> Option<(String, String)> {
    let calendar = js::trim(&js::string_or_empty(args.get("calendar"))).to_owned();
    let id = js::trim(&js::string_or_empty(args.get("id"))).to_owned();
    (!calendar.is_empty() && !id.is_empty()).then_some((calendar, id))
}

fn read(core: &Core, args: &Value) -> Result<Value, Failure> {
    let (auth, account) = signed_in(core)?;
    let Some((calendar, id)) = ids(args) else {
        return Ok(bad_args("That isn't an event."));
    };
    saved(core, &auth, &account)?;
    let stored = with_store(core, |store| store.event(&account, &calendar, &id))?;
    let mut repeat = String::new();
    let event = match stored {
        Some(event) => event,
        // a series itself is never stored, nor what the sync hasn't reached
        None => {
            let live = match core.calendar.event(&auth, &calendar, &id) {
                Ok(live) => live.filter(|v| text(v, "status") != "cancelled"),
                Err(Failure::Offline(_)) => None,
                Err(e) => return Err(e),
            };
            let Some(event) = live.as_ref().and_then(|v| event_of(&calendar, &account, v)) else {
                return Ok(gone());
            };
            repeat = live.map(|v| rrule(&v)).unwrap_or_default();
            event
        }
    };
    if !event.series.is_empty() {
        // the rule lives on the series. With no connection it's only left blank:
        // writes need one, and a blank never ends a series
        repeat = match core.calendar.event(&auth, &calendar, &event.series) {
            Ok(master) => master.map(|m| rrule(&m)).unwrap_or_default(),
            Err(Failure::Offline(_)) => String::new(),
            Err(e) => return Err(e),
        };
    }
    let writable = with_store(core, |store| store.calendars(&account))?
        .iter()
        .any(|c| c.id == calendar && c.writable);
    let detail: Value = serde_json::from_str(&event.detail).unwrap_or(Value::Null);
    let mut view = row_json(&event);
    view["notes"] = json!(event.notes);
    view["organizer"] = detail["organizer"].clone();
    view["attendees"] = detail["attendees"].clone();
    view["meet_url"] = detail["meet_url"].clone();
    view["repeat"] = json!(repeat);
    view["series"] = json!(event.series);
    view["time_zone"] = detail["time_zone"].clone();
    view["can_edit"] = json!(writable && detail["editable"] == true);
    view["etag"] = json!(event.etag);
    Ok(ok(view))
}

/// What the person filled in, on the editor or on a card. A field left out
/// stays as it is: only a save that says so changes it.
struct Form {
    calendar: String,
    title: Option<String>,
    all_day: bool,
    start: String,
    end: String,
    time_zone: String,
    repeat: String,
    guests: Option<Vec<String>>,
    place: Option<String>,
    notes: Option<String>,
    meet: bool,
}

fn email_ok(address: &str) -> bool {
    let parts = address.split_once('@');
    address.len() <= 254
        && !address.chars().any(|c| c.is_whitespace() || c.is_control())
        && parts
            .is_some_and(|(name, host)| !name.is_empty() && !host.is_empty() && !host.contains('@'))
}

fn zone_ok(zone: &str) -> bool {
    zone.len() <= 64
        && zone
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'_' | b'-' | b'+'))
}

fn repeat_ok(repeat: &str) -> bool {
    repeat.is_empty()
        || (repeat.len() <= 1024
            && repeat.starts_with("RRULE:")
            && !repeat.chars().any(char::is_control))
}

impl Form {
    fn of(args: &Value) -> Result<Self, &'static str> {
        let word = |k: &str| js::trim(&js::string_or_empty(args.get(k))).to_owned();
        let said = |k: &str| args.get(k).and_then(Value::as_str).map(str::to_owned);
        let all_day = args.get("all_day") == Some(&Value::Bool(true));
        let (start, end) = (word("start"), word("end"));
        let in_order = if all_day {
            date(&start).zip(date(&end)).map(|(s, e)| s < e)
        } else {
            millis(&start).zip(millis(&end)).map(|(s, e)| s < e)
        };
        match in_order {
            None if all_day => return Err("An all-day event's dates are YYYY-MM-DD."),
            None => return Err("An event's times are RFC 3339, with an offset."),
            Some(false) => return Err("An event has to end after it starts."),
            Some(true) => {}
        }
        let (time_zone, repeat) = (word("time_zone"), word("repeat"));
        if !zone_ok(&time_zone) {
            return Err("That isn't a time zone.");
        }
        if !repeat_ok(&repeat) {
            return Err("A repeat is one RRULE line.");
        }
        let guests = args.get("guests").and_then(Value::as_array).map(|list| {
            let mut emails: Vec<String> = Vec::with_capacity(list.len());
            for g in list.iter().filter_map(Value::as_str).map(js::trim) {
                if !emails.iter().any(|e| e.eq_ignore_ascii_case(g)) {
                    emails.push(g.to_owned());
                }
            }
            emails
        });
        if guests.iter().flatten().any(|g| !email_ok(g)) {
            return Err("One of the guests isn't an email address.");
        }
        Ok(Self {
            calendar: word("calendar"),
            title: said("title").map(|t| cap_bytes(&t, HEADER_MAX)),
            all_day,
            start,
            end,
            time_zone,
            repeat,
            guests,
            place: said("where").map(|t| cap_bytes(&t, HEADER_MAX)),
            notes: said("notes").map(|t| cap_bytes(&t, BODY_MAX)),
            meet: args.get("meet") == Some(&Value::Bool(true)),
        })
    }

    /// The guests other than the person; `None` if the form doesn't say.
    fn others(&self, account: &str) -> Option<Vec<&String>> {
        let guests = self.guests.as_ref()?;
        Some(
            guests
                .iter()
                .filter(|g| !g.eq_ignore_ascii_case(account))
                .collect(),
        )
    }

    /// Start and end as a new event sends them.
    fn times(&self, zone: &str) -> (Value, Value) {
        let side = |value: &str| {
            if self.all_day {
                json!({"date": value})
            } else if zone.is_empty() {
                json!({"dateTime": value})
            } else {
                json!({"dateTime": value, "timeZone": zone})
            }
        };
        (side(&self.start), side(&self.end))
    }

    /// Whether it says the same times as `event`.
    fn same_times(&self, event: &Value) -> bool {
        let (start, end) = (&event["start"], &event["end"]);
        let same_kind = self.all_day == start.get("date").is_some();
        let same = if self.all_day {
            text(start, "date") == self.start && text(end, "date") == self.end
        } else {
            millis(&self.start) == millis(&text(start, "dateTime"))
                && millis(&self.end) == millis(&text(end, "dateTime"))
        };
        same_kind && same && !self.zone_differs(event)
    }

    fn zone_differs(&self, event: &Value) -> bool {
        !self.time_zone.is_empty() && text(&event["start"], "timeZone") != self.time_zone
    }
}

/// One side of a write's time. A PATCH merges into what's there, so a change
/// between timed and all-day has to take the other kind away.
fn patched(side: &Value) -> Value {
    let mut side = side.clone();
    if side.get("date").is_some() {
        side["dateTime"] = Value::Null;
        side["timeZone"] = Value::Null;
    } else {
        side["date"] = Value::Null;
    }
    side
}

static REQUESTS: AtomicU64 = AtomicU64::new(0);

/// Asks Google for a Meet link. The id only has to be new for each request.
fn meet_request() -> Value {
    let n = REQUESTS.fetch_add(1, Ordering::Relaxed);
    json!({"createRequest": {"requestId": format!("gws-{}-{n}", now_ms()), "conferenceSolutionKey": {"type": "hangoutsMeet"}}})
}

/// The attendee list a save leaves, when it's different. Whoever stays keeps
/// their whole entry, or their answer would be lost.
fn attendees_after(others: &[&String], event: &Value, account: &str) -> Option<Value> {
    let others = named(others, event, account);
    let now = guests_of(event, account);
    let held = |g: &str| {
        now.iter()
            .find(|a| text(a, "email").eq_ignore_ascii_case(g))
    };
    if others.len() == now.len() && others.iter().all(|g| held(g).is_some()) {
        return None;
    }
    let mut kept: Vec<Value> = attendees(event)
        .iter()
        .filter(|a| !is_guest(a, account))
        .cloned()
        .collect();
    kept.extend(
        others
            .iter()
            .map(|g| held(g).map_or_else(|| json!({"email": g}), |&a| a.clone())),
    );
    Some(json!(kept))
}

/// Everything but the times that the form changes in `event`.
fn field_changes(form: &Form, event: &Value, account: &str) -> Map<String, Value> {
    let mut body = Map::new();
    let mut set = |key: &str, new: &Option<String>, now: String| {
        if let Some(new) = new.as_ref().filter(|n| **n != now) {
            body.insert(key.to_owned(), json!(new));
        }
    };
    set("summary", &form.title, text(event, "summary"));
    set("location", &form.place, text(event, "location"));
    // compared as the person read it, so an HTML description isn't flattened
    // by a save that never touched it
    set("description", &form.notes, notes_of(event));
    if let Some(list) = form
        .others(account)
        .and_then(|others| attendees_after(&others, event, account))
    {
        body.insert("attendees".to_owned(), list);
    }
    if form.meet && meet_url(event).is_empty() {
        body.insert("conferenceData".to_owned(), meet_request());
    }
    body
}

const SERIES_UNREADABLE: &str = "That series' times can't be read.";
const SERIES_SKIPPED: &str = "The clocks skip that time on the series' first day. Choose another.";

/// `time` on the clock of `zone`. The offset in the text won't do: Google
/// writes a time in the calendar's zone, which needn't be the event's. With no
/// zone there's no clock to read it on, and an offset alone can't say where a
/// clock change falls, so a move would land an hour off across one.
fn wall(time: &str, zone: &str) -> Option<NaiveDateTime> {
    let at = DateTime::parse_from_rfc3339(time).ok()?;
    Some(at.with_timezone(&zone.parse::<Tz>().ok()?).naive_local())
}

/// `at` on the clock of `zone`, as a time to send. A time the clocks repeat
/// means the first one, as in the editor.
fn moment_of(at: NaiveDateTime, zone: &str) -> Result<String, &'static str> {
    let zone = zone.parse::<Tz>().map_err(|_| SERIES_UNREADABLE)?;
    let on_clock = zone.from_local_datetime(&at).earliest();
    let moment = on_clock.map(|t| t.to_rfc3339_opts(SecondsFormat::Secs, false));
    moment.ok_or(SERIES_SKIPPED)
}

/// The series' `key` side, moved as the person moved it on `instance` to
/// `typed`. The move is measured on wall clocks, not in hours: across a clock
/// change the same time of day is an hour more or less away, and a series
/// keeps its time of day. `home` is the calendar's zone, for a side naming none.
fn shifted(
    form: &Form,
    key: &str,
    typed: &str,
    instance: &Value,
    master: &Value,
    home: &str,
) -> Result<Value, &'static str> {
    let (was, side) = (&instance[key], &master[key]);
    let mut moved = side.clone();
    if form.all_day {
        let by = date(typed)
            .zip(date(&text(was, "date")))
            .map(|(to, from)| to - from);
        let day = date(&text(side, "date")).zip(by).map(|(day, by)| day + by);
        moved["date"] = json!(day.ok_or(SERIES_UNREADABLE)?.to_string());
        return Ok(moved);
    }
    let or = |zone: String, other: &str| {
        if zone.is_empty() {
            other.to_owned()
        } else {
            zone
        }
    };
    let series_zone = or(text(side, "timeZone"), home);
    let was_zone = or(text(was, "timeZone"), &series_zone);
    // an instance can have a zone of its own, and that's no change to the series
    let (typed_zone, zone) = if form.time_zone.is_empty() || form.time_zone == was_zone {
        (&was_zone, &series_zone)
    } else {
        (&form.time_zone, &form.time_zone)
    };
    let by = wall(typed, typed_zone)
        .zip(wall(&text(was, "dateTime"), &was_zone))
        .map(|(to, from)| to - from);
    let at = wall(&text(side, "dateTime"), &series_zone).zip(by);
    let at = at.map(|(at, by)| at + by).ok_or(SERIES_UNREADABLE)?;
    moved["dateTime"] = json!(moment_of(at, zone)?);
    moved["timeZone"] = json!(zone);
    Ok(moved)
}

/// A save of `all`, from the instance the person edited. The series keeps its
/// first date: the instance's new time is a shift to apply, or the series
/// would start over at that instance and lose everything before it.
fn series_changes(
    form: &Form,
    instance: &Value,
    master: &Value,
    account: &str,
    home: &str,
) -> Result<Map<String, Value>, &'static str> {
    let mut body = field_changes(form, instance, account);
    if !form.same_times(instance) {
        if form.all_day != instance["start"].get("date").is_some() {
            return Err("A series can't change between timed and all-day here.");
        }
        let start = shifted(form, "start", &form.start, instance, master, home)?;
        let end = shifted(form, "end", &form.end, instance, master, home)?;
        body.insert("start".to_owned(), patched(&start));
        body.insert("end".to_owned(), patched(&end));
    }
    if !form.repeat.is_empty() && form.repeat != rrule(master) {
        body.insert("recurrence".to_owned(), recurrence(form, master));
    }
    Ok(body)
}

/// The series' lines with its rule replaced: exceptions stay.
fn recurrence(form: &Form, event: &Value) -> Value {
    let mut lines: Vec<Value> = event
        .get("recurrence")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|l| !l.as_str().is_some_and(|l| l.starts_with("RRULE:")))
        .cloned()
        .collect();
    lines.insert(0, json!(form.repeat));
    json!(lines)
}

/// The primary calendar, for a save that doesn't name one.
pub(crate) fn calendar_or_primary(
    core: &Core,
    account: &str,
    named: &str,
) -> Result<String, Failure> {
    if !named.is_empty() {
        return Ok(named.to_owned());
    }
    let calendars = with_store(core, |store| store.calendars(account))?;
    let primary = calendars.into_iter().find(|c| c.primary).map(|c| c.id);
    Ok(primary.unwrap_or_else(|| "primary".to_owned()))
}

/// Which event a write goes to: the id itself, or for `all` its series. An
/// instance has to say which it means.
fn target(args: &Value, live: &Value, id: &str) -> Result<(String, bool), Value> {
    let series = text(live, "recurringEventId");
    if series.is_empty() {
        return Ok((id.to_owned(), false));
    }
    match js::string_or_empty(args.get("scope")).as_str() {
        "this" => Ok((id.to_owned(), false)),
        "all" => Ok((series, true)),
        _ => Err(bad_args("Say whether this is for one event or the series.")),
    }
}

/// After a write: what Google answered goes in at once, the sync of that
/// calendar brings the rest, such as a series' instances, and the row is what
/// the store now holds under `id`, else what Google answered.
fn finish(
    core: &Core,
    auth: &Bearer,
    account: &str,
    calendar: &str,
    id: &str,
    written: &Value,
) -> Result<Value, Failure> {
    let is_series = written.get("recurrence").is_some();
    let mut event = event_of(calendar, account, written);
    if let Some(event) = event.as_mut() {
        if is_series {
            event.series = event.id.clone();
        } else {
            with_own_store(core, account, |store| store.put_event(account, event))?;
        }
    }
    {
        let _one_at_a_time = core.cal_sync.lock().unwrap_or_else(|e| e.into_inner());
        // the write stands whatever this finds: the next check catches up
        let _ = sync_calendar(core, auth, account, calendar);
    }
    bump(core, account)?;
    core.sync.wake();
    let stored = with_store(core, |store| store.event(account, calendar, id))?;
    Ok(ok(
        json!({"event": stored.or(event).as_ref().map(row_json)}),
    ))
}

fn save(core: &Core, args: &Value) -> Result<Value, Failure> {
    let (auth, account) = signed_in(core)?;
    let form = match Form::of(args) {
        Ok(form) => form,
        Err(why) => return Ok(bad_args(why)),
    };
    let calendar = calendar_or_primary(core, &account, &form.calendar)?;
    let id = js::trim(&js::string_or_empty(args.get("id"))).to_owned();
    if id.is_empty() {
        return insert(core, &auth, &account, &calendar, &form);
    }
    let Some(live) = core
        .calendar
        .event(&auth, &calendar, &id)?
        .filter(|v| text(v, "status") != "cancelled")
    else {
        return Ok(gone());
    };
    let (write_id, whole) = match target(args, &live, &id) {
        Ok(target) => target,
        Err(refusal) => return Ok(refusal),
    };
    let etag = js::string_or_empty(args.get("etag"));
    let (body, if_match) = if whole {
        // the person read an instance, and Google's If-Match on the series
        // can't see that, so their ETag is held against the instance here
        if !etag.is_empty() && etag != text(&live, "etag") {
            core.sync.wake();
            return Ok(changed());
        }
        let Some(master) = core.calendar.event(&auth, &calendar, &write_id)? else {
            return Ok(gone());
        };
        let home = calendar_zone(core, &account, &calendar)?;
        match series_changes(&form, &live, &master, &account, &home) {
            Ok(body) => (body, text(&master, "etag")),
            Err(why) => return Ok(bad_args(why)),
        }
    } else {
        let mut body = field_changes(&form, &live, &account);
        let new_repeat = !form.repeat.is_empty()
            && form.repeat != rrule(&live)
            && text(&live, "recurringEventId").is_empty();
        if !form.same_times(&live) || new_repeat {
            let zone = zone_for(core, &account, &calendar, &form)?;
            let (start, end) = form.times(&zone);
            body.insert("start".to_owned(), patched(&start));
            body.insert("end".to_owned(), patched(&end));
        }
        if new_repeat {
            body.insert("recurrence".to_owned(), recurrence(&form, &live));
        }
        (body, etag)
    };
    if body.is_empty() {
        // nothing to write, so nobody to tell
        let event = event_of(&calendar, &account, &live);
        return Ok(ok(json!({"event": event.as_ref().map(row_json)})));
    }
    let guests = form.others(&account).map_or_else(
        || guests_of(&live, &account).len(),
        |g| named(&g, &live, &account).len(),
    );
    let write = Write {
        // a guest just taken off has to hear of it too
        notify: guests > 0 || !guests_of(&live, &account).is_empty(),
        if_match: &if_match,
    };
    let answer = core
        .calendar
        .patch(&auth, &calendar, &write_id, &Value::Object(body), &write)?;
    if let Some(no) = refused(&answer) {
        core.sync.wake();
        return Ok(no);
    }
    let Answer::Done(written) = answer else {
        return Ok(gone());
    };
    finish(core, &auth, &account, &calendar, &id, &written)
}

/// The zone a form's times go out in: its own, else its calendar's. A series
/// needs one, since that's the clock its instances keep to.
fn zone_for(core: &Core, account: &str, calendar: &str, form: &Form) -> Result<String, Failure> {
    if !form.time_zone.is_empty() {
        return Ok(form.time_zone.clone());
    }
    calendar_zone(core, account, calendar)
}

fn calendar_zone(core: &Core, account: &str, calendar: &str) -> Result<String, Failure> {
    let calendars = with_store(core, |store| store.calendars(account))?;
    let held = calendars.into_iter().find(|c| c.id == calendar);
    Ok(held.map(|c| c.time_zone).unwrap_or_default())
}

fn insert(
    core: &Core,
    auth: &Bearer,
    account: &str,
    calendar: &str,
    form: &Form,
) -> Result<Value, Failure> {
    let zone = zone_for(core, account, calendar, form)?;
    let (start, end) = form.times(&zone);
    let mut body =
        json!({"summary": form.title.as_deref().unwrap_or_default(), "start": start, "end": end});
    if let Some(place) = form.place.as_ref().filter(|p| !p.is_empty()) {
        body["location"] = json!(place);
    }
    if let Some(notes) = form.notes.as_ref().filter(|n| !n.is_empty()) {
        body["description"] = json!(notes);
    }
    if !form.repeat.is_empty() {
        body["recurrence"] = json!([form.repeat]);
    }
    let others = form.others(account).unwrap_or_default();
    if !others.is_empty() {
        let people: Vec<Value> = others.iter().map(|g| json!({"email": g})).collect();
        body["attendees"] = json!(people);
    }
    if form.meet {
        body["conferenceData"] = meet_request();
    }
    let answer = core
        .calendar
        .insert(auth, calendar, &body, !others.is_empty())?;
    if let Some(no) = refused(&answer) {
        return Ok(no);
    }
    let Answer::Done(written) = answer else {
        return Ok(gone());
    };
    finish(
        core,
        auth,
        account,
        calendar,
        &text(&written, "id"),
        &written,
    )
}

/// Deletes that can still be undone, in this run, by the person's own screen.
#[derive(Default)]
pub struct Deleted(VecDeque<Delete>);

struct Delete {
    account: String,
    calendar: String,
    /// What the screen named, and what was written to: a series, for `all`.
    id: String,
    target: String,
    guests: bool,
}

impl Deleted {
    fn keep(&mut self, delete: Delete) {
        self.0.push_back(delete);
        if self.0.len() > DELETES_MAX {
            self.0.pop_front();
        }
    }

    fn find(&self, account: &str, calendar: &str, id: &str) -> Option<usize> {
        self.0
            .iter()
            .rposition(|d| d.account == account && d.calendar == calendar && d.id == id)
    }
}

fn delete(core: &Core, args: &Value) -> Result<Value, Failure> {
    let (auth, account) = signed_in(core)?;
    let Some((calendar, id)) = ids(args) else {
        return Ok(bad_args("That isn't an event."));
    };
    let Some(live) = core
        .calendar
        .event(&auth, &calendar, &id)?
        .filter(|v| text(v, "status") != "cancelled")
    else {
        return Ok(gone());
    };
    let (write_id, _) = match target(args, &live, &id) {
        Ok(target) => target,
        Err(refusal) => return Ok(refusal),
    };
    let guests = !guests_of(&live, &account).is_empty();
    let answer = core.calendar.delete(&auth, &calendar, &write_id, guests)?;
    if let Some(no) = refused(&answer) {
        return Ok(no);
    }
    with_own_store(core, &account, |store| {
        store.remove_event(&account, &calendar, &write_id)
    })?;
    core.cal_deleted
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .keep(Delete {
            account: account.clone(),
            calendar: calendar.clone(),
            id,
            target: write_id,
            guests,
        });
    {
        let _one_at_a_time = core.cal_sync.lock().unwrap_or_else(|e| e.into_inner());
        let _ = sync_calendar(core, &auth, &account, &calendar);
    }
    bump(core, &account)?;
    core.sync.wake();
    Ok(ok(json!({"ok": true, "undo": !guests})))
}

/// Undo of a delete, which only stands for one nobody else heard of: guests
/// have been told it's off, and bringing it back quietly would tell them wrong.
fn restore(core: &Core, args: &Value) -> Result<Value, Failure> {
    let (auth, account) = signed_in(core)?;
    let Some((calendar, id)) = ids(args) else {
        return Ok(bad_args("That isn't an event."));
    };
    let (write_id, guests) = {
        let held = core.cal_deleted.lock().unwrap_or_else(|e| e.into_inner());
        match held.find(&account, &calendar, &id) {
            Some(at) => (held.0[at].target.clone(), held.0[at].guests),
            None => return Ok(refusal("not_found", "That can't be undone any more.")),
        }
    };
    if guests {
        return Ok(refusal(
            "has_guests",
            "Its guests were told it's cancelled, so it can't be undone. Make it again instead.",
        ));
    }
    let write = Write {
        notify: false,
        if_match: "",
    };
    let body = json!({"status": "confirmed"});
    let answer = core
        .calendar
        .patch(&auth, &calendar, &write_id, &body, &write)?;
    if let Some(no) = refused(&answer) {
        return Ok(no);
    }
    let Answer::Done(written) = answer else {
        return Ok(gone());
    };
    {
        let mut held = core.cal_deleted.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(at) = held.find(&account, &calendar, &id) {
            held.0.remove(at);
        }
    }
    finish(core, &auth, &account, &calendar, &id, &written)
}

/// A guest's answer: everyone else on the event hears of it.
fn rsvp(core: &Core, args: &Value) -> Result<Value, Failure> {
    rsvp_with(core, args, None, false)
}

/// [`rsvp`], from `known` when the caller has just read the event, and with
/// `sendUpdates=all` whoever else the person's copy lists when `always`: an
/// invite answered from Mail ([`crate::invite`]) is for its organiser.
pub(crate) fn rsvp_with(
    core: &Core,
    args: &Value,
    known: Option<Value>,
    always: bool,
) -> Result<Value, Failure> {
    let (auth, account) = signed_in(core)?;
    let Some((calendar, id)) = ids(args) else {
        return Ok(bad_args("That isn't an event."));
    };
    let response = js::string_or_empty(args.get("response"));
    if !["accepted", "declined", "tentative"].contains(&response.as_str()) {
        return Ok(bad_args("A reply is accepted, declined or tentative."));
    }
    let live = match known {
        Some(event) => Some(event),
        None => core.calendar.event(&auth, &calendar, &id)?,
    };
    let Some(live) = live.filter(|v| text(v, "status") != "cancelled") else {
        return Ok(gone());
    };
    // a reply to one instance is the plain reading: `all` has to be said
    let all = js::string_or_empty(args.get("scope")) == "all";
    let series = text(&live, "recurringEventId");
    let (write_id, event) = if all && !series.is_empty() {
        let Some(master) = core.calendar.event(&auth, &calendar, &series)? else {
            return Ok(gone());
        };
        (series, master)
    } else {
        (id.clone(), live)
    };
    if !attendees(&event).iter().any(|a| is_self(a, &account)) {
        return Ok(bad_args("You aren't a guest of this event."));
    }
    // a PATCH replaces the list, so everyone else goes back as they were
    let people: Vec<Value> = attendees(&event)
        .iter()
        .map(|a| {
            let mut a = a.clone();
            if is_self(&a, &account) {
                a["responseStatus"] = json!(response);
            }
            a
        })
        .collect();
    let mut body = json!({"attendees": people});
    if event.get("attendeesOmitted") == Some(&Value::Bool(true)) {
        body["attendeesOmitted"] = json!(true);
    }
    let etag = text(&event, "etag");
    let write = Write {
        notify: always || !guests_of(&event, &account).is_empty(),
        if_match: &etag,
    };
    let answer = core
        .calendar
        .patch(&auth, &calendar, &write_id, &body, &write)?;
    if let Some(no) = refused(&answer) {
        core.sync.wake();
        return Ok(no);
    }
    let Answer::Done(written) = answer else {
        return Ok(gone());
    };
    finish(core, &auth, &account, &calendar, &id, &written)
}

/// `gws_event_draft`: a ticket and nothing else. No Google, no store: the
/// fields live behind the ticket until the person presses Save on the card.
fn draft(core: &Core, args: &Value) -> Value {
    let form = match Form::of(args) {
        Ok(form) => form,
        Err(why) => return fail("invalid_args", why),
    };
    let fields = json!({
        "title": form.title.unwrap_or_default(),
        "start": form.start,
        "end": form.end,
        "all_day": form.all_day,
        "time_zone": form.time_zone,
        "calendar": form.calendar,
        "guests": form.guests.unwrap_or_default(),
        "where": form.place.unwrap_or_default(),
        "notes": form.notes.unwrap_or_default(),
        "repeat": form.repeat,
        "meet": form.meet,
        // a card saves to the account it was drafted on, or not at all
        "as_account": core.accounts.signed_in().map(|(who, _)| who),
    });
    let ticket = core
        .tickets
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .issue(&Subject::EventDraft(fields.to_string()));
    ok(json!({"ticket": ticket}))
}

fn draft_read(core: &Core, args: &Value) -> Value {
    let tickets = core.tickets.lock().unwrap_or_else(|e| e.into_inner());
    let held = tickets.subject(&js::string_or_empty(args.get("ticket")));
    let fields = match held {
        Some(Subject::EventDraft(fields)) => serde_json::from_str::<Value>(fields).ok(),
        _ => None,
    };
    let now = core.accounts.signed_in().map(|(who, _)| who);
    let fields = fields.filter(|f| {
        let drafted = f["as_account"].as_str();
        drafted.is_none() || drafted == now.as_deref()
    });
    fields.map_or_else(|| refusal("not_found", "This card is out of date."), ok)
}

/// Offers the `decide` card with a chat's event draft. Nothing of the event
/// goes in it: props hold a ticket, and the card reads the fields itself.
pub fn offer_card(tool: &str, result: &mut Value) {
    if tool != EVENT_DRAFT {
        return;
    }
    let ticket = result.pointer("/structuredContent/ticket").cloned();
    if let Some(ticket) = ticket {
        result["_meta"] = json!({"buwiz/open": {"app": "calendar", "props": {"draft": ticket}, "title": "Calendar", "placement": "inline"}});
    }
}

/// Runs a calendar tool; `None` for any other tool. Every call says a screen
/// is open, so the sync keeps up with it.
pub fn run(core: &Core, tool: &str, args: &Value) -> Option<Value> {
    let result = match tool {
        CAL_LIST => list(core),
        CAL_SHOW => show(core, args),
        CAL_RANGE => range(core, args),
        CAL_CHANGES => changes(core),
        CAL_NEXT => next(core),
        CAL_PEOPLE => crate::compose::contacts(core, args),
        EVENT_READ => read(core, args),
        SEARCH_EVENTS => search(core, args),
        EVENT_SAVE => save(core, args),
        EVENT_DELETE => delete(core, args),
        EVENT_RESTORE => restore(core, args),
        EVENT_RSVP => rsvp(core, args),
        EVENT_DRAFT => Ok(draft(core, args)),
        EVENT_DRAFT_READ => Ok(draft_read(core, args)),
        _ => return None,
    };
    core.sync.seen();
    Some(result.unwrap_or_else(failed))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: &str = "owner@example.com";

    #[test]
    fn a_segment_never_leaves_its_place_in_the_path() {
        assert_eq!(segment("abc_20261102T140000Z"), "abc_20261102T140000Z");
        assert_eq!(
            segment("en.usa#holiday@group"),
            "en%2Eusa%23holiday%40group"
        );
        assert_eq!(segment("../x"), "%2E%2E%2Fx");
    }

    #[test]
    fn a_range_of_one_day_is_that_day_wherever_it_was_asked() {
        let at = |t: &str| DateTime::parse_from_rfc3339(t).unwrap();
        for zone in ["+13:00", "+00:00", "-08:00"] {
            let w = window_of(
                at(&format!("2026-11-10T00:00:00{zone}")),
                at(&format!("2026-11-11T00:00:00{zone}")),
                None,
            )
            .unwrap();
            assert_eq!(
                (w.from_date.as_str(), w.to_date.as_str()),
                ("2026-11-10", "2026-11-11")
            );
        }
        let w = window_of(
            at("2026-11-10T09:00:00+08:00"),
            at("2026-11-10T10:00:00+08:00"),
            None,
        )
        .unwrap();
        assert_eq!(w.to_date, "2026-11-11", "the day it ends on still counts");
        assert_eq!(
            window_of(at("2026-11-11T00:00:00Z"), at("2026-11-10T00:00:00Z"), None),
            None
        );
    }

    /// Havana's clocks change at midnight, so March 8 starts at 01:00, and a
    /// range of March 7 ends there. March 8 isn't in it.
    #[test]
    fn a_range_of_whole_days_ends_on_the_date_it_says() {
        let at = |t: &str| DateTime::parse_from_rfc3339(t).unwrap();
        let w = window_of(
            at("2026-03-07T00:00:00-05:00"),
            at("2026-03-08T01:00:00-04:00"),
            date("2026-03-08"),
        )
        .unwrap();
        assert_eq!(
            (w.from_date.as_str(), w.to_date.as_str()),
            ("2026-03-07", "2026-03-08")
        );
    }

    #[test]
    fn notes_are_plain_text_with_their_lines_and_without_the_markup() {
        let html = json!({"description": "<p>Bring <b>this</b></p><script>evil()</script>"});
        assert_eq!(notes_of(&html), "Bring this");
        let plain = json!({"description": "one\ntwo"});
        assert_eq!(notes_of(&plain), "one\ntwo");
    }

    #[test]
    fn only_an_https_link_is_a_meet_link() {
        assert_eq!(meet_url(&json!({"hangoutLink": "javascript:evil()"})), "");
        let v = json!({"conferenceData": {"entryPoints": [{"entryPointType": "phone", "uri": "tel:+1"}, {"entryPointType": "video", "uri": "https://meet.example.com/a"}]}});
        assert_eq!(meet_url(&v), "https://meet.example.com/a");
    }

    #[test]
    fn a_row_says_how_the_person_stands() {
        let v = json!({"id": "e", "summary": "s", "start": {"dateTime": "2026-11-02T09:00:00-05:00"}, "end": {"dateTime": "2026-11-02T10:00:00-05:00"}, "attendees": [{"email": ME, "self": true, "responseStatus": "needsAction"}, {"email": "ana@example.com"}, {"email": "room@example.com", "resource": true}]});
        let e = event_of("c", ME, &v).unwrap();
        assert_eq!((e.response.as_str(), e.guests), ("needsAction", 1));
        assert_eq!(e.start_ms, millis("2026-11-02T14:00:00Z").unwrap());
        let none =
            json!({"id": "e", "start": {"date": "2026-11-02"}, "end": {"date": "2026-11-03"}});
        let e = event_of("c", ME, &none).unwrap();
        assert_eq!(
            (e.response.as_str(), e.all_day, e.status.as_str()),
            ("none", true, "confirmed")
        );
        assert_eq!(event_of("c", ME, &json!({"id": "x"})), None);
    }

    #[test]
    fn whoever_stays_on_a_save_keeps_their_answer() {
        let event = json!({"attendees": [{"email": ME, "self": true, "responseStatus": "accepted"}, {"email": "ana@example.com", "responseStatus": "declined"}]});
        let (ana, ben) = ("Ana@example.com".to_owned(), "ben@example.com".to_owned());
        assert_eq!(
            attendees_after(&[&ana], &event, ME),
            None,
            "the same people"
        );
        let list = attendees_after(&[&ana, &ben], &event, ME).unwrap();
        assert_eq!(
            list,
            json!([{"email": ME, "self": true, "responseStatus": "accepted"}, {"email": "ana@example.com", "responseStatus": "declined"}, {"email": "ben@example.com"}])
        );
    }

    #[test]
    fn a_room_stays_a_room_and_is_never_invited() {
        let room =
            json!({"email": "room@example.com", "resource": true, "responseStatus": "accepted"});
        let event =
            json!({"attendees": [{"email": ME, "self": true}, {"email": "ana@example.com"}, room]});
        let (ana, ben) = ("ana@example.com".to_owned(), "ben@example.com".to_owned());
        let named_room = "Room@example.com".to_owned();
        assert_eq!(
            attendees_after(&[&ana, &named_room], &event, ME),
            None,
            "the same people, with the room as the form read it"
        );
        assert_eq!(
            attendees_after(&[&ana, &named_room, &ben], &event, ME).unwrap(),
            json!([{"email": ME, "self": true}, room, {"email": "ana@example.com"}, {"email": "ben@example.com"}])
        );
    }

    #[test]
    fn the_person_under_another_address_is_not_invited_to_their_own_event() {
        let event = json!({"attendees": [{"email": "alias@example.com", "self": true}, {"email": "ana@example.com"}]});
        let (ana, alias) = ("ana@example.com".to_owned(), "alias@example.com".to_owned());
        assert_eq!(attendees_after(&[&alias, &ana], &event, ME), None);
    }

    const NY: &str = "America/New_York";

    fn form(start: &str, end: &str, zone: &str) -> Form {
        Form::of(&json!({"start": start, "end": end, "time_zone": zone})).unwrap()
    }

    fn timed(start: &str, end: &str, zone: &str) -> Value {
        json!({"start": {"dateTime": start, "timeZone": zone}, "end": {"dateTime": end, "timeZone": zone}})
    }

    /// Google writes times in the calendar's zone: here Manila's, on an event
    /// that keeps New York's clock. Its 09:00 either side of the change is 21:00
    /// and 22:00 in Manila, and moved an hour it's still an hour.
    #[test]
    fn a_series_is_read_on_its_own_clock_whatever_offset_it_is_written_with() {
        let master = timed("2026-10-27T21:00:00+08:00", "2026-10-27T22:00:00+08:00", NY);
        let instance = timed("2026-11-03T22:00:00+08:00", "2026-11-03T23:00:00+08:00", NY);
        let moved = form("2026-11-03T10:00:00-05:00", "2026-11-03T11:00:00-05:00", NY);
        let body = series_changes(&moved, &instance, &master, ME, "Asia/Manila").unwrap();
        assert_eq!(body["start"]["dateTime"], "2026-10-27T10:00:00-04:00");
        assert_eq!(body["end"]["dateTime"], "2026-10-27T11:00:00-04:00");
        assert_eq!(body["start"]["timeZone"], NY);
    }

    #[test]
    fn a_series_moved_to_another_zone_lands_on_the_clock_typed_there() {
        let master = timed("2026-10-27T09:00:00-04:00", "2026-10-27T10:00:00-04:00", NY);
        let instance = timed("2026-11-03T09:00:00-05:00", "2026-11-03T10:00:00-05:00", NY);
        let moved = form(
            "2026-11-03T09:00:00+08:00",
            "2026-11-03T10:00:00+08:00",
            "Asia/Manila",
        );
        let body = series_changes(&moved, &instance, &master, ME, NY).unwrap();
        assert_eq!(body["start"]["dateTime"], "2026-10-27T09:00:00+08:00");
        assert_eq!(body["start"]["timeZone"], "Asia/Manila");
    }

    /// One instance moved to Manila says nothing of the series, which stays
    /// on New York's clock when that instance moves it.
    #[test]
    fn an_instance_with_a_zone_of_its_own_moves_the_series_on_the_series_clock() {
        let master = timed("2026-10-27T09:00:00-04:00", "2026-10-27T10:00:00-04:00", NY);
        let zone = "Asia/Manila";
        let instance = timed(
            "2026-11-03T09:00:00+08:00",
            "2026-11-03T10:00:00+08:00",
            zone,
        );
        let moved = form(
            "2026-11-03T10:00:00+08:00",
            "2026-11-03T11:00:00+08:00",
            zone,
        );
        let body = series_changes(&moved, &instance, &master, ME, NY).unwrap();
        assert_eq!(body["start"]["dateTime"], "2026-10-27T10:00:00-04:00");
        assert_eq!(body["start"]["timeZone"], NY);
    }

    #[test]
    fn a_series_with_no_zone_named_keeps_to_its_calendars() {
        let side = |t: &str| json!({"dateTime": t});
        let master = json!({"start": side("2026-10-27T21:00:00+08:00"), "end": side("2026-10-27T22:00:00+08:00")});
        let instance = json!({"start": side("2026-11-03T22:00:00+08:00"), "end": side("2026-11-03T23:00:00+08:00")});
        let moved = form("2026-11-03T10:00:00-05:00", "2026-11-03T11:00:00-05:00", "");
        let body = series_changes(&moved, &instance, &master, ME, NY).unwrap();
        assert_eq!(body["start"]["dateTime"], "2026-10-27T10:00:00-04:00");
    }

    #[test]
    fn a_series_moved_onto_a_time_the_clocks_skip_is_refused() {
        // its first day is the spring change, when 02:30 never happens
        let master = timed("2026-03-08T01:30:00-05:00", "2026-03-08T03:30:00-04:00", NY);
        let instance = timed("2026-03-15T01:30:00-04:00", "2026-03-15T02:30:00-04:00", NY);
        let moved = form("2026-03-15T02:30:00-04:00", "2026-03-15T03:30:00-04:00", NY);
        assert_eq!(
            series_changes(&moved, &instance, &master, ME, NY),
            Err(SERIES_SKIPPED)
        );
    }

    #[test]
    fn a_series_in_a_zone_nobody_can_read_is_refused_not_guessed() {
        let zone = "Mars/Olympus";
        let master = timed(
            "2026-10-27T09:00:00-04:00",
            "2026-10-27T10:00:00-04:00",
            zone,
        );
        let instance = timed(
            "2026-11-03T09:00:00-05:00",
            "2026-11-03T10:00:00-05:00",
            zone,
        );
        let moved = form(
            "2026-11-03T10:00:00-05:00",
            "2026-11-03T11:00:00-05:00",
            zone,
        );
        assert_eq!(
            series_changes(&moved, &instance, &master, ME, NY),
            Err(SERIES_UNREADABLE)
        );
    }

    /// Its calendar isn't held and it names no zone. New York's clocks change
    /// between these two: kept on -04:00 it would land an hour off.
    #[test]
    fn a_series_with_no_zone_anywhere_is_refused_not_moved_on_an_old_offset() {
        let side = |t: &str| json!({"dateTime": t});
        let master = json!({"start": side("2026-10-27T09:00:00-04:00"), "end": side("2026-10-27T10:00:00-04:00")});
        let instance = json!({"start": side("2026-11-03T09:00:00-05:00"), "end": side("2026-11-03T10:00:00-05:00")});
        let moved = form("2026-11-03T10:00:00-05:00", "2026-11-03T11:00:00-05:00", "");
        assert_eq!(
            series_changes(&moved, &instance, &master, ME, ""),
            Err(SERIES_UNREADABLE)
        );
        // its times are all it can't do
        let renamed = Form::of(&json!({"title": "Renamed", "start": "2026-11-03T09:00:00-05:00", "end": "2026-11-03T10:00:00-05:00"})).unwrap();
        let body = series_changes(&renamed, &instance, &master, ME, "").unwrap();
        assert_eq!(body["summary"], "Renamed");
        assert!(body.get("start").is_none());
    }

    #[test]
    fn a_form_refuses_what_could_only_go_wrong() {
        let good = json!({"title": "t", "start": "2026-11-02T09:00:00-05:00", "end": "2026-11-02T10:00:00-05:00"});
        assert!(Form::of(&good).is_ok());
        let with = |key: &str, value: Value| {
            let mut args = good.clone();
            args[key] = value;
            args
        };
        for bad in [
            with("start", json!("2026-11-02 09:00")),
            with("end", json!("2026-11-02T08:00:00-05:00")),
            with("repeat", json!("FREQ=WEEKLY")),
            with("repeat", json!("RRULE:FREQ=WEEKLY\r\nX-EVIL:1")),
            with("guests", json!(["not an address"])),
            with("time_zone", json!("../etc")),
            with("all_day", json!(true)),
        ] {
            assert!(Form::of(&bad).is_err(), "{bad}");
        }
    }
}
