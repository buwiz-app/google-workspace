//! What the tools answer, as the screen holds it.

use chrono::{Local, NaiveDateTime, NaiveTime, TimeZone};
use chrono_tz::Tz;
use serde_json::{Value, json};

use super::time::{self, When};

pub(crate) fn text(v: &Value, k: &str) -> String {
    v.get(k)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn flag(v: &Value, k: &str) -> bool {
    v.get(k).and_then(Value::as_bool).unwrap_or(false)
}

/// Which event a press means: rows move as the store changes.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Key {
    pub calendar: String,
    pub id: String,
}

impl Key {
    pub fn args(&self) -> Value {
        json!({"calendar": self.calendar, "id": self.id})
    }
}

#[derive(Clone, PartialEq, Debug)]
pub struct Event {
    pub id: String,
    pub calendar: String,
    pub title: String,
    pub when: When,
    pub place: String,
    pub status: String,
    pub response: String,
    pub guests: i64,
    pub meet: bool,
    pub recurring: bool,
}

impl Event {
    pub fn of(v: &Value) -> Option<Event> {
        Some(Event {
            id: v.get("id")?.as_str()?.to_owned(),
            calendar: text(v, "calendar"),
            title: text(v, "title"),
            when: When::of(
                v.get("start")?.as_str()?,
                v.get("end").and_then(Value::as_str).unwrap_or_default(),
                flag(v, "all_day"),
            )?,
            place: text(v, "where"),
            status: text(v, "status"),
            response: text(v, "response"),
            guests: v.get("guests").and_then(Value::as_i64).unwrap_or(0),
            meet: flag(v, "meet"),
            recurring: flag(v, "recurring"),
        })
    }

    pub fn key(&self) -> Key {
        Key {
            calendar: self.calendar.clone(),
            id: self.id.clone(),
        }
    }

    pub fn name(&self) -> String {
        if self.title.trim().is_empty() {
            "(No title)".to_owned()
        } else {
            self.title.clone()
        }
    }

    /// The person said no, or hasn't said yes for sure: it reads quieter.
    pub fn faint(&self) -> bool {
        matches!(self.response.as_str(), "declined" | "needsAction") || self.status == "tentative"
    }
}

/// The rows of a list answer, without what was called off.
pub fn rows_in(v: &Value, key: &str) -> Vec<Event> {
    v.get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Event::of)
        .filter(|e| e.status != "cancelled")
        .collect()
}

#[derive(Clone, PartialEq, Debug)]
pub struct Calendar {
    pub id: String,
    pub name: String,
    pub primary: bool,
    pub writable: bool,
    pub shown: bool,
    pub time_zone: String,
}

pub fn calendars_in(v: &Value) -> Vec<Calendar> {
    v.get("calendars")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|c| {
            Some(Calendar {
                id: c.get("id")?.as_str()?.to_owned(),
                name: text(c, "name"),
                primary: flag(c, "primary"),
                writable: flag(c, "writable"),
                shown: flag(c, "shown"),
                time_zone: text(c, "time_zone"),
            })
        })
        .collect()
}

/// Where a new event goes: the person's main calendar, else one they can write.
pub fn home(calendars: &[Calendar]) -> Option<&Calendar> {
    calendars
        .iter()
        .find(|c| c.primary && c.writable)
        .or_else(|| calendars.iter().find(|c| c.writable))
}

#[derive(Clone, PartialEq, Debug)]
pub struct Guest {
    pub email: String,
    pub name: String,
    pub response: String,
    pub organizer: bool,
    pub me: bool,
    /// A room: on the event, but nobody to invite.
    pub room: bool,
}

impl Guest {
    pub fn reply(&self) -> &'static str {
        match self.response.as_str() {
            "accepted" => "Yes",
            "declined" => "No",
            "tentative" => "Maybe",
            _ => "No reply yet",
        }
    }
}

/// An event as `gws_event_read` answers it.
#[derive(Clone, PartialEq, Debug)]
pub struct Detail {
    pub row: Event,
    pub notes: String,
    pub organizer: String,
    pub attendees: Vec<Guest>,
    pub meet_url: String,
    pub repeat: String,
    pub series: String,
    pub time_zone: String,
    pub can_edit: bool,
    pub etag: String,
}

impl Detail {
    pub fn of(v: &Value) -> Option<Detail> {
        let attendees = v
            .get("attendees")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|a| Guest {
                email: text(a, "email"),
                name: text(a, "name"),
                response: text(a, "response"),
                organizer: flag(a, "organizer"),
                me: flag(a, "self"),
                room: flag(a, "resource"),
            })
            .collect();
        Some(Detail {
            row: Event::of(v)?,
            notes: text(v, "notes"),
            organizer: text(v, "organizer"),
            attendees,
            meet_url: text(v, "meet_url"),
            repeat: text(v, "repeat"),
            series: text(v, "series"),
            time_zone: text(v, "time_zone"),
            can_edit: flag(v, "can_edit"),
            etag: text(v, "etag"),
        })
    }

    /// Someone asked this person, so Yes, Maybe and No apply. An organizer's
    /// own event isn't an invite.
    pub fn invite(&self) -> bool {
        self.row.response != "none" && !self.attendees.iter().any(|a| a.me && a.organizer)
    }

    /// A change here is a choice between this one and the whole series.
    pub fn in_series(&self) -> bool {
        self.row.recurring || !self.series.is_empty()
    }
}

/// The repeats the editor offers, with their rules.
pub const REPEATS: [(&str, &str); 5] = [
    ("Does not repeat", ""),
    ("Every day", "RRULE:FREQ=DAILY"),
    ("Every week", "RRULE:FREQ=WEEKLY"),
    ("Every month", "RRULE:FREQ=MONTHLY"),
    ("Every year", "RRULE:FREQ=YEARLY"),
];

/// A rule in words. One the editor doesn't offer is shown as it is, as a card
/// must show everything Save would write.
pub fn repeat_line(rule: &str) -> String {
    REPEATS
        .iter()
        .find(|(_, r)| *r == rule)
        .map_or_else(|| rule.to_owned(), |(name, _)| (*name).to_owned())
}

/// What an address needs to be sent an invite.
pub fn is_address(word: &str) -> bool {
    match word.split_once('@') {
        Some((who, place)) => {
            !who.is_empty()
                && place.contains('.')
                && !place.starts_with('.')
                && !place.ends_with('.')
                && !word.contains(char::is_whitespace)
        }
        None => false,
    }
}

/// The event being written. Dates and times are what the person sees, in their
/// own zone, as text: a half typed time mustn't be lost or corrected.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct Form {
    /// The account a draft was made on: its save goes there or nowhere.
    pub as_account: String,
    pub event: Option<Key>,
    pub in_series: bool,
    pub etag: String,
    pub title: String,
    pub all_day: bool,
    pub start_date: String,
    pub start_time: String,
    pub end_date: String,
    pub end_time: String,
    pub time_zone: String,
    pub repeat: String,
    /// The guests, as chips.
    pub guests: Vec<String>,
    /// The person's own place on the guest list: kept, shown as nobody to invite.
    pub own: Vec<String>,
    /// The guests when the event was opened: they hear of a change even if
    /// they're taken off.
    pub before: Vec<String>,
    /// The address being typed, which counts once it's a whole one.
    pub typing: String,
    pub place: String,
    pub notes: String,
    pub meet: bool,
    /// The event already has a link, so there's none to add.
    pub has_meet: bool,
    pub calendar: String,
}

fn set_when<Z: TimeZone>(form: &mut Form, when: &When, zone: &Z) {
    match when {
        When::AllDay { first, end } => {
            form.all_day = true;
            form.start_date = first.to_string();
            form.end_date = time::last_day(*end).to_string();
            form.start_time = "09:00".to_owned();
            form.end_time = "10:00".to_owned();
        }
        When::Timed { start, end } => {
            let (from, to) = (
                start.with_timezone(zone).naive_local(),
                end.with_timezone(zone).naive_local(),
            );
            form.start_date = from.date().to_string();
            form.end_date = to.date().to_string();
            form.start_time = clock(from.time());
            form.end_time = clock(to.time());
        }
    }
}

/// `at` on the clock of `from`, as the clock of `to` reads it.
fn same_moment<Z: TimeZone>(at: NaiveDateTime, from: &Z, to: Option<Tz>) -> NaiveDateTime {
    let moment = from.from_local_datetime(&at).earliest();
    match (moment, to) {
        (Some(moment), Some(to)) => moment.with_timezone(&to).naive_local(),
        _ => at,
    }
}

fn clock(at: NaiveTime) -> String {
    at.format("%H:%M").to_string()
}

impl Form {
    /// A new event in the slot `start` to `end`, the person's own clock. The
    /// fields are read in the calendar's zone, so they show that same moment.
    pub fn new(calendar: Option<&Calendar>, start: NaiveDateTime, end: NaiveDateTime) -> Form {
        let zone = calendar.and_then(|c| c.time_zone.parse::<Tz>().ok());
        let (start, end) = (
            same_moment(start, &Local, zone),
            same_moment(end, &Local, zone),
        );
        Form {
            start_date: start.date().to_string(),
            end_date: end.date().to_string(),
            start_time: clock(start.time()),
            end_time: clock(end.time()),
            time_zone: calendar.map(|c| c.time_zone.clone()).unwrap_or_default(),
            calendar: calendar.map(|c| c.id.clone()).unwrap_or_default(),
            ..Form::default()
        }
    }

    pub fn of_detail<Z: TimeZone>(d: &Detail, zone: &Z) -> Form {
        let people = d.attendees.iter().filter(|a| !a.room);
        let (own, guests): (Vec<_>, Vec<_>) = people.partition(|a| a.me);
        let address = |a: &&Guest| a.email.clone();
        let mut form = Form {
            event: Some(d.row.key()),
            in_series: d.in_series(),
            etag: d.etag.clone(),
            title: d.row.title.clone(),
            time_zone: d.time_zone.clone(),
            repeat: d.repeat.clone(),
            guests: guests.iter().map(address).collect(),
            before: guests.iter().map(address).collect(),
            own: own.iter().map(address).collect(),
            place: d.row.place.clone(),
            notes: d.notes.clone(),
            has_meet: d.row.meet || !d.meet_url.is_empty(),
            calendar: d.row.calendar.clone(),
            ..Form::default()
        };
        form.set_when_in_its_zone(&d.row.when, zone);
        form
    }

    /// The fields of `gws_event_draft_read`. What a draft leaves out comes from
    /// `base`, the form a new event would start with.
    pub fn of_draft<Z: TimeZone>(v: &Value, base: Form, zone: &Z) -> Form {
        let some = |k: &str, or: String| Some(text(v, k)).filter(|t| !t.is_empty()).unwrap_or(or);
        let guests = v.get("guests").and_then(Value::as_array);
        let mut form = Form {
            as_account: text(v, "as_account"),
            title: text(v, "title"),
            time_zone: some("time_zone", base.time_zone.clone()),
            calendar: some("calendar", base.calendar.clone()),
            repeat: text(v, "repeat"),
            guests: guests
                .into_iter()
                .flatten()
                .filter_map(|g| g.as_str().map(str::to_owned))
                .collect(),
            place: text(v, "where"),
            notes: text(v, "notes"),
            meet: flag(v, "meet"),
            ..base
        };
        let when = When::of(&text(v, "start"), &text(v, "end"), flag(v, "all_day"));
        if let Some(when) = when {
            form.set_when_in_its_zone(&when, zone);
        }
        form
    }

    /// The addresses Save would invite, in order: the chips, and what's being
    /// typed once it's an address.
    pub fn invites(&self) -> Vec<String> {
        let mut out = self.guests.clone();
        let typed = self.typing.trim();
        let seen = out.iter().any(|g| g.eq_ignore_ascii_case(typed));
        if is_address(typed) && !seen {
            out.push(typed.to_owned());
        }
        out
    }

    /// Who's new to the event: an invite goes to them.
    fn added(&self) -> usize {
        let was = |g: &String| self.before.iter().any(|b| b.eq_ignore_ascii_case(g));
        self.invites().iter().filter(|g| !was(g)).count()
    }

    /// The press names what it does, and only people other than the person
    /// count. An invite goes to whoever's new; with nobody new, everyone who
    /// was on the event still hears of the change, taken off or not.
    pub fn save_line(&self) -> String {
        match (self.added(), self.before.len()) {
            (0, 0) => "Save".to_owned(),
            (0, told) => format!("Save and update {told}"),
            (new, _) => save_line(new),
        }
    }

    /// What's said once this form has gone through.
    pub fn saved_line(&self) -> String {
        match (self.added(), self.before.len()) {
            (0, 1) => "Saved, and 1 guest was told.".to_owned(),
            (0, told) if told > 1 => format!("Saved, and {told} guests were told."),
            (new, _) => saved_line(new),
        }
    }

    /// The fields are read in the event's own zone, so a time typed for
    /// Manila means Manila. `fallback` is for an event that names none.
    fn set_when_in_its_zone<Z: TimeZone>(&mut self, when: &When, fallback: &Z) {
        match self.time_zone.parse::<Tz>() {
            Ok(zone) => set_when(self, when, &zone),
            Err(_) => set_when(self, when, fallback),
        }
    }

    /// What `gws_event_save` takes, read in the zone the form says.
    pub fn save_args(&self) -> Result<Value, String> {
        let args = if self.time_zone.trim().is_empty() {
            self.args(&Local)
        } else {
            match self.time_zone.trim().parse::<Tz>() {
                Ok(zone) => self.args(&zone),
                Err(_) => Err("That isn't a time zone. Type one like Asia/Manila.".to_owned()),
            }
        };
        args.map(|args| crate::shell::owned(args, &self.as_account))
    }

    /// What `gws_event_save` takes, or why it can't go yet. `zone` is the one
    /// the fields are read in.
    pub fn args<Z: TimeZone>(&self, zone: &Z) -> Result<Value, String>
    where
        Z::Offset: std::fmt::Display,
    {
        if self.calendar.is_empty() {
            return Err("Choose a calendar for this event.".to_owned());
        }
        let typed = self.typing.trim();
        if !typed.is_empty() && !is_address(typed) {
            return Err(format!("Check this guest's address: {typed}"));
        }
        let date = |t: &str, which: &str| {
            time::parse_date(t).ok_or_else(|| format!("Type the {which} date like 2026-11-06."))
        };
        let clock = |t: &str, which: &str| {
            time::parse_time(t).ok_or_else(|| format!("Type the {which} time like 13:30."))
        };
        let first = date(&self.start_date, "start")?;
        let last = date(&self.end_date, "end")?;
        let (start, end) = if self.all_day {
            if last < first {
                return Err("It ends before it starts.".to_owned());
            }
            (first.to_string(), time::exclusive_end(last).to_string())
        } else {
            let from = clock(&self.start_time, "start")?;
            let to = clock(&self.end_time, "end")?;
            if last.and_time(to) < first.and_time(from) {
                return Err("It ends before it starts.".to_owned());
            }
            let skipped = || "The clocks skip that time on that day. Choose another.".to_owned();
            (
                time::moment(first, from, zone).ok_or_else(skipped)?,
                time::moment(last, to, zone).ok_or_else(skipped)?,
            )
        };
        let invites = self.invites();
        let guests = if invites.is_empty() {
            Vec::new()
        } else {
            [self.own.clone(), invites].concat()
        };
        let mut args = json!({
            "calendar": self.calendar,
            "title": self.title,
            "all_day": self.all_day,
            "start": start,
            "end": end,
            "time_zone": self.time_zone,
            "repeat": self.repeat,
            "guests": guests,
            "where": self.place,
            "notes": self.notes,
            "meet": self.meet,
        });
        if let Some(event) = &self.event {
            args["id"] = event.id.clone().into();
        }
        if !self.etag.is_empty() {
            args["etag"] = self.etag.clone().into();
        }
        Ok(args)
    }
}

/// The word on the Save press for `n` guests.
pub fn save_line(n: usize) -> String {
    if n == 0 {
        "Save".to_owned()
    } else {
        format!("Save and invite {n}")
    }
}

/// What's said once a save with `n` guests has gone through.
pub fn saved_line(n: usize) -> String {
    match n {
        0 => "Saved.".to_owned(),
        1 => "Saved, and the invite went to 1 guest.".to_owned(),
        n => format!("Saved, and the invite went to {n} guests."),
    }
}

#[cfg(test)]
mod tests {
    use chrono_tz::America::New_York;
    use chrono_tz::Asia::Manila;

    use super::*;

    fn typed_for(zone: &str) -> Form {
        Form {
            calendar: "mine".to_owned(),
            time_zone: zone.to_owned(),
            start_date: "2026-11-06".to_owned(),
            end_date: "2026-11-06".to_owned(),
            start_time: "09:00".to_owned(),
            end_time: "10:00".to_owned(),
            ..Form::default()
        }
    }

    #[test]
    fn a_time_typed_for_a_zone_means_that_zone() {
        let manila = typed_for("Asia/Manila").save_args().unwrap();
        assert_eq!(manila["start"], "2026-11-06T09:00:00+08:00");
        let new_york = typed_for("America/New_York").save_args().unwrap();
        assert_eq!(new_york["start"], "2026-11-06T09:00:00-05:00");
    }

    #[test]
    fn a_slot_keeps_its_moment_on_another_zones_clock() {
        let at = "2026-11-06T09:00:00".parse().unwrap();
        let there = same_moment(at, &New_York, Some(Manila));
        assert_eq!(there.to_string(), "2026-11-06 22:00:00");
        assert_eq!(same_moment(at, &New_York, None), at, "no zone, no change");
    }

    #[test]
    fn a_zone_that_is_not_one_says_so() {
        let why = typed_for("Mars/Base").save_args().unwrap_err();
        assert!(why.contains("time zone"), "{why}");
    }

    #[test]
    fn an_event_opens_on_its_own_zones_clock() {
        let d = detail("2026-11-06T01:00:00Z", "2026-11-06T02:00:00Z", false);
        let form = Form::of_detail(&d, &New_York);
        assert_eq!(form.start_time, "09:00", "01:00 UTC is 09:00 in Manila");
    }

    #[test]
    fn save_says_who_hears_of_it() {
        let mut form = Form::default();
        assert_eq!(form.save_line(), "Save");
        form.guests = vec!["ana@example.com".to_owned(), "ben@example.com".to_owned()];
        assert_eq!(form.save_line(), "Save and invite 2", "a new event");
        form.before = form.guests.clone();
        assert_eq!(form.save_line(), "Save and update 2", "nobody new");
        form.guests.clear();
        assert_eq!(
            form.save_line(),
            "Save and update 2",
            "taken off, still told"
        );
        assert_eq!(form.saved_line(), "Saved, and 2 guests were told.");
        form.guests = vec!["Ana@Example.com".to_owned(), "eve@example.com".to_owned()];
        assert_eq!(form.save_line(), "Save and invite 1", "only Eve is new");
    }

    fn detail(start: &str, end: &str, all_day: bool) -> Detail {
        Detail::of(&json!({
            "id": "e", "calendar": "mine", "title": "T", "start": start, "end": end,
            "all_day": all_day, "etag": "x", "time_zone": "Asia/Manila",
        }))
        .unwrap()
    }

    #[test]
    fn a_room_is_no_chip_and_nobody_to_tell() {
        let d = Detail::of(&json!({
            "id": "e", "calendar": "mine", "title": "T", "all_day": false, "etag": "x",
            "start": "2026-11-05T23:30:00Z", "end": "2026-11-06T00:30:00Z",
            "attendees": [
                {"email": "me@example.com", "self": true},
                {"email": "ana@example.com"},
                {"email": "room@example.com", "resource": true},
            ],
        }))
        .unwrap();
        let form = Form::of_detail(&d, &New_York);
        assert_eq!(form.guests, ["ana@example.com"]);
        assert_eq!(form.save_line(), "Save and update 1");
        assert_eq!(
            form.args(&New_York).unwrap()["guests"],
            json!(["me@example.com", "ana@example.com"])
        );
    }

    #[test]
    fn an_event_reads_into_the_form_and_writes_back_the_same_moment() {
        let mut d = detail("2026-11-05T23:30:00Z", "2026-11-06T00:30:00Z", false);
        let form = Form::of_detail(&d, &New_York);
        assert_eq!(
            (form.start_date.as_str(), form.start_time.as_str()),
            ("2026-11-06", "07:30"),
            "on the clock of its own zone, Manila"
        );
        assert_eq!(
            form.save_args().unwrap()["start"],
            "2026-11-06T07:30:00+08:00"
        );

        d.time_zone.clear();
        let form = Form::of_detail(&d, &New_York);
        assert_eq!(
            (form.start_date.as_str(), form.start_time.as_str()),
            ("2026-11-05", "18:30"),
            "naming no zone, it reads in the person's"
        );
        assert_eq!(
            form.args(&New_York).unwrap()["start"],
            "2026-11-05T18:30:00-05:00"
        );
    }

    #[test]
    fn an_all_day_event_keeps_its_dates_through_the_form_in_any_zone() {
        let d = detail("2026-11-06", "2026-11-08", true);
        for last in [
            Form::of_detail(&d, &New_York).end_date,
            Form::of_detail(&d, &Manila).end_date,
        ] {
            assert_eq!(last, "2026-11-07", "the last day, not Google's end");
        }
        let args = Form::of_detail(&d, &New_York).args(&New_York).unwrap();
        assert_eq!(
            (&args["start"], &args["end"]),
            (&json!("2026-11-06"), &json!("2026-11-08"))
        );
    }

    #[test]
    fn a_time_the_clocks_skip_is_said_and_not_saved() {
        let mut form = Form::of_detail(
            &detail("2026-03-08T12:00:00Z", "2026-03-08T13:00:00Z", false),
            &New_York,
        );
        form.start_time = "02:30".to_owned();
        form.end_time = "03:30".to_owned();
        assert_eq!(
            form.args(&New_York),
            Err("The clocks skip that time on that day. Choose another.".to_owned())
        );
        assert!(form.args(&Manila).is_ok());
    }

    #[test]
    fn a_guest_list_with_nobody_left_is_empty_not_just_the_person() {
        let mut form = Form::of_detail(
            &detail("2026-11-06T01:00:00Z", "2026-11-06T02:00:00Z", false),
            &Manila,
        );
        form.own = vec!["me@example.com".to_owned()];
        assert_eq!(form.args(&Manila).unwrap()["guests"], json!([]));
        form.guests = vec!["ana@example.com".to_owned()];
        assert_eq!(
            form.args(&Manila).unwrap()["guests"],
            json!(["me@example.com", "ana@example.com"])
        );
    }

    #[test]
    fn an_address_needs_someone_and_a_place() {
        for good in ["ana@example.com", "a.b@mail.example.com"] {
            assert!(is_address(good), "{good}");
        }
        for bad in [
            "ana",
            "@example.com",
            "ana@",
            "ana@example",
            "an a@example.com",
        ] {
            assert!(!is_address(bad), "{bad}");
        }
    }

    #[test]
    fn a_rule_the_editor_doesnt_offer_is_shown_as_it_is() {
        assert_eq!(repeat_line("RRULE:FREQ=WEEKLY"), "Every week");
        assert_eq!(repeat_line(""), "Does not repeat");
        assert_eq!(
            repeat_line("RRULE:FREQ=WEEKLY;BYDAY=MO"),
            "RRULE:FREQ=WEEKLY;BYDAY=MO"
        );
    }
}
