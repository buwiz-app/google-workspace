//! Invites in Mail. A Calendar invite carries its event as an `.ics`: the
//! text/calendar part inside the letter, and often again as an
//! application/ics file. A small, strict reader takes the one VEVENT from it,
//! never the letter's HTML; anything it doesn't follow is no invite, so a
//! stranger's `.ics` gets no card rather than a wrong one.
//!
//! The answer goes through Calendar: the event is found by the UID the `.ics`
//! carries, and answered as Calendar's own Yes, No and Maybe are
//! ([`calendar::rsvp`]), so the organiser hears. Both tools are the screen's;
//! only the person's press answers.

use base64::Engine as _;
use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, SecondsFormat, TimeDelta, Utc};
use chrono::{TimeZone, offset::LocalResult};
use chrono_tz::Tz;
use serde_json::{Value, json};

use crate::accounts::Bearer;
use crate::calendar;
use crate::gmail::{self, Failure, HEADER_MAX, cap_bytes};
use crate::mail::{now_ms, signed_in, with_own_store, with_store};
use crate::tools::{MAIL_INVITE, MAIL_INVITE_REPLY, fail, failed, ok};
use crate::{Core, js, pictures};

/// The most of an `.ics` read: Google's run to a few KiB.
pub const ICS_MAX: usize = 256 * 1024;
const LINES_MAX: usize = 5000;
const PARAMS_MAX: usize = 32;
const NESTING_MAX: usize = 8;
const EVENTS_MAX: usize = 64;
const ATTENDEES_MAX: usize = 500;
/// How far a part is looked for inside a mail.
const DEPTH_MAX: usize = 16;
/// The longest an event may run: a DURATION past this is no invite.
const DAYS_MAX: i64 = 3660;

const NOT_YET: &str = "Not in your Calendar yet.";
const DIFFERS: &str = "This invite doesn't match your Calendar.";
const UNVERIFIED: &str = "Gmail couldn't confirm this invite came from its organiser.";
const CANCELLED: &str = "This event was cancelled.";

/// What an invite asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// An invitation, or an update of one.
    Request,
    Cancel,
    /// A guest's answer to the person's own invite.
    Reply,
}

impl Method {
    pub fn name(self) -> &'static str {
        match self {
            Method::Request => "request",
            Method::Cancel => "cancel",
            Method::Reply => "reply",
        }
    }
}

/// One side of an event: a moment, or for an all-day event a date.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Time {
    At(DateTime<Utc>),
    Day(NaiveDate),
}

impl Time {
    /// As Calendar's rows have it, so the screen reads both alike.
    fn text(self) -> String {
        match self {
            Time::At(at) => at.to_rfc3339_opts(SecondsFormat::Secs, true),
            Time::Day(day) => day.format("%Y-%m-%d").to_string(),
        }
    }
}

/// The organiser or a guest.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Person {
    /// CN, which may be empty.
    pub name: String,
    /// From `mailto:`, which may be empty.
    pub address: String,
    /// PARTSTAT as Calendar names it: `accepted`, `declined`, `tentative`
    /// or `needsAction`.
    pub response: Option<&'static str>,
}

impl Person {
    /// What a person reads: the name, else the address.
    pub fn shown(&self) -> &str {
        if self.name.is_empty() {
            &self.address
        } else {
            &self.name
        }
    }
}

/// The event an invite carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    /// iCalUID: how Calendar finds the event.
    pub uid: String,
    pub method: Method,
    pub title: String,
    pub start: Time,
    /// Exclusive, for an all-day event as for Google.
    pub end: Time,
    pub place: String,
    pub organizer: Person,
    pub attendees: Vec<Person>,
}

impl Event {
    /// The guest who is `address`.
    #[cfg(test)]
    pub fn me(&self, address: &str) -> Option<&Person> {
        self.attendees
            .iter()
            .find(|a| !address.is_empty() && a.address.eq_ignore_ascii_case(address))
    }

    /// For a REPLY: who answered, and what.
    fn answer(&self) -> Option<(&Person, &'static str)> {
        self.attendees.iter().find_map(|a| {
            a.response
                .filter(|r| *r != "needsAction")
                .filter(|_| !a.shown().is_empty())
                .map(|r| (a, r))
        })
    }

    /// The thread view's `invite`, for message `message`.
    fn json(&self, message: &str) -> Option<Value> {
        let mut v = json!({
            "message": message,
            "uid": self.uid,
            "method": self.method.name(),
            "title": self.title,
            "start": self.start.text(),
            "end": self.end.text(),
            "all_day": matches!(self.start, Time::Day(_)),
            "place": self.place,
            "organizer": self.organizer.shown(),
            // what Calendar's copy has to match before an answer goes
            "organizer_address": self.organizer.address,
        });
        if self.method == Method::Reply {
            let (who, response) = self.answer()?;
            v["reply"] = json!({"who": who.shown(), "response": response});
        }
        Some(v)
    }
}

// --- The reader ----------------------------------------------------------------

/// One content line: `NAME;PARAM=value:value`.
struct Line {
    name: String,
    params: Vec<(String, String)>,
    value: String,
}

impl Line {
    fn param(&self, key: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

fn is_name(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// Lines put back together: a line that starts with a space or a tab goes on
/// from the one before.
fn unfold(ics: &str) -> Option<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for raw in ics.split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        if let Some(rest) = raw.strip_prefix([' ', '\t']) {
            out.last_mut()?.push_str(rest);
        } else if !raw.is_empty() {
            if out.len() >= LINES_MAX {
                return None;
            }
            out.push(raw.to_owned());
        }
    }
    Some(out)
}

/// RFC 6868's escapes in a parameter: `^n`, `^'` and `^^`.
fn param_text(v: &str) -> String {
    // one pass, left to right, so `^^n` is a caret and an n
    let mut out = String::with_capacity(v.len());
    let mut chars = v.chars().peekable();
    while let Some(c) = chars.next() {
        let said = match (c, chars.peek()) {
            ('^', Some('n')) => ' ',
            ('^', Some('\'')) => '"',
            ('^', Some('^')) => '^',
            _ => {
                out.push(c);
                continue;
            }
        };
        chars.next();
        out.push(said);
    }
    out
}

fn line(text: &str) -> Option<Line> {
    let end = text.find([';', ':'])?;
    let name = &text[..end];
    if !is_name(name) {
        return None;
    }
    let mut rest = &text[end..];
    let mut params = Vec::new();
    while let Some(after) = rest.strip_prefix(';') {
        let eq = after.find('=')?;
        let key = &after[..eq];
        if !is_name(key) || params.len() >= PARAMS_MAX {
            return None;
        }
        let v = &after[eq + 1..];
        let (value, next) = match v.strip_prefix('"') {
            Some(quoted) => {
                let close = quoted.find('"')?;
                (&quoted[..close], &quoted[close + 1..])
            }
            None => {
                let stop = v.find([';', ':'])?;
                (&v[..stop], &v[stop..])
            }
        };
        params.push((key.to_ascii_uppercase(), param_text(value)));
        rest = next;
    }
    Some(Line {
        name: name.to_ascii_uppercase(),
        params,
        value: rest.strip_prefix(':')?.to_owned(),
    })
}

/// A TEXT value with `\,` `\;` `\n` and `\\` read.
fn unescape(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut chars = v.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n' | 'N') => out.push('\n'),
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

/// A text on one line, as the card shows it, held to [`HEADER_MAX`].
fn one_line(v: &str) -> String {
    let flat: String = v
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let words: Vec<&str> = flat.split_whitespace().collect();
    cap_bytes(&words.join(" "), HEADER_MAX)
}

fn digits(s: &str, at: std::ops::Range<usize>) -> Option<u32> {
    let part = s.get(at)?;
    if !part.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    part.parse().ok()
}

fn date_of(s: &str) -> Option<NaiveDate> {
    if s.len() != 8 {
        return None;
    }
    NaiveDate::from_ymd_opt(
        i32::try_from(digits(s, 0..4)?).ok()?,
        digits(s, 4..6)?,
        digits(s, 6..8)?,
    )
}

fn wall_of(s: &str) -> Option<NaiveDateTime> {
    if s.len() != 15 || s.as_bytes()[8] != b'T' {
        return None;
    }
    date_of(&s[..8])?.and_hms_opt(digits(s, 9..11)?, digits(s, 11..13)?, digits(s, 13..15)?)
}

/// Outlook names zones as Windows does: CLDR's windowsZones table, each by
/// the IANA zone it gives for the whole world ("001"), and the old names
/// Windows has since renamed.
fn windows_zone(name: &str) -> Option<&'static str> {
    Some(match name {
        "UTC" | "Coordinated Universal Time" => "UTC",
        "Dateline Standard Time" => "Etc/GMT+12",
        "UTC-11" => "Etc/GMT+11",
        "Aleutian Standard Time" => "America/Adak",
        "Hawaiian Standard Time" => "Pacific/Honolulu",
        "Marquesas Standard Time" => "Pacific/Marquesas",
        "Alaskan Standard Time" => "America/Anchorage",
        "UTC-09" => "Etc/GMT+9",
        "Pacific Standard Time (Mexico)" => "America/Tijuana",
        "UTC-08" => "Etc/GMT+8",
        "Pacific Standard Time" => "America/Los_Angeles",
        "US Mountain Standard Time" => "America/Phoenix",
        "British Columbia Standard Time" => "America/Vancouver",
        "Alberta Standard Time" => "America/Edmonton",
        "Mountain Standard Time (Mexico)" | "Mexico Standard Time 2" => "America/Mazatlan",
        "Mountain Standard Time" => "America/Denver",
        "Yukon Standard Time" => "America/Whitehorse",
        "Central America Standard Time" => "America/Guatemala",
        "Central Standard Time" => "America/Chicago",
        "Easter Island Standard Time" => "Pacific/Easter",
        "Central Standard Time (Mexico)" | "Mexico Standard Time" => "America/Mexico_City",
        "Canada Central Standard Time" => "America/Regina",
        "Manitoba Standard Time" => "America/Winnipeg",
        "SA Pacific Standard Time" => "America/Bogota",
        "Eastern Standard Time (Mexico)" => "America/Cancun",
        "Eastern Standard Time" => "America/New_York",
        "Haiti Standard Time" => "America/Port-au-Prince",
        "Cuba Standard Time" => "America/Havana",
        "US Eastern Standard Time" => "America/Indiana/Indianapolis",
        "Turks And Caicos Standard Time" => "America/Grand_Turk",
        "Paraguay Standard Time" => "America/Asuncion",
        "Atlantic Standard Time" => "America/Halifax",
        "Venezuela Standard Time" => "America/Caracas",
        "Central Brazilian Standard Time" => "America/Cuiaba",
        "SA Western Standard Time" => "America/La_Paz",
        "Pacific SA Standard Time" => "America/Santiago",
        "Newfoundland Standard Time" => "America/St_Johns",
        "Tocantins Standard Time" => "America/Araguaina",
        "E. South America Standard Time" => "America/Sao_Paulo",
        "SA Eastern Standard Time" => "America/Cayenne",
        "Argentina Standard Time" => "America/Argentina/Buenos_Aires",
        "Greenland Standard Time" => "America/Nuuk",
        "Montevideo Standard Time" => "America/Montevideo",
        "Magallanes Standard Time" => "America/Punta_Arenas",
        "Saint Pierre Standard Time" => "America/Miquelon",
        "Bahia Standard Time" => "America/Bahia",
        "UTC-02" | "Mid-Atlantic Standard Time" => "Etc/GMT+2",
        "Azores Standard Time" => "Atlantic/Azores",
        "Cape Verde Standard Time" => "Atlantic/Cape_Verde",
        "GMT Standard Time" => "Europe/London",
        "Greenwich Standard Time" => "Atlantic/Reykjavik",
        "Sao Tome Standard Time" => "Africa/Sao_Tome",
        "Morocco Standard Time" => "Africa/Casablanca",
        "W. Europe Standard Time" => "Europe/Berlin",
        "Central Europe Standard Time" => "Europe/Budapest",
        "Romance Standard Time" => "Europe/Paris",
        "Central European Standard Time" => "Europe/Warsaw",
        "W. Central Africa Standard Time" => "Africa/Lagos",
        "Jordan Standard Time" => "Asia/Amman",
        "GTB Standard Time" => "Europe/Bucharest",
        "Middle East Standard Time" => "Asia/Beirut",
        "Egypt Standard Time" => "Africa/Cairo",
        "E. Europe Standard Time" => "Europe/Chisinau",
        "Syria Standard Time" => "Asia/Damascus",
        "West Bank Standard Time" => "Asia/Hebron",
        "South Africa Standard Time" => "Africa/Johannesburg",
        "FLE Standard Time" => "Europe/Kiev",
        "Israel Standard Time" => "Asia/Jerusalem",
        "South Sudan Standard Time" => "Africa/Juba",
        "Kaliningrad Standard Time" => "Europe/Kaliningrad",
        "Sudan Standard Time" => "Africa/Khartoum",
        "Libya Standard Time" => "Africa/Tripoli",
        "Namibia Standard Time" => "Africa/Windhoek",
        "Arabic Standard Time" => "Asia/Baghdad",
        "Turkey Standard Time" => "Europe/Istanbul",
        "Arab Standard Time" => "Asia/Riyadh",
        "Belarus Standard Time" => "Europe/Minsk",
        "Russian Standard Time" => "Europe/Moscow",
        "E. Africa Standard Time" => "Africa/Nairobi",
        "Volgograd Standard Time" => "Europe/Volgograd",
        "Iran Standard Time" => "Asia/Tehran",
        "Arabian Standard Time" => "Asia/Dubai",
        "Astrakhan Standard Time" => "Europe/Astrakhan",
        "Azerbaijan Standard Time" => "Asia/Baku",
        "Russia Time Zone 3" => "Europe/Samara",
        "Mauritius Standard Time" => "Indian/Mauritius",
        "Saratov Standard Time" => "Europe/Saratov",
        "Georgian Standard Time" => "Asia/Tbilisi",
        "Caucasus Standard Time" | "Armenian Standard Time" => "Asia/Yerevan",
        "Afghanistan Standard Time" => "Asia/Kabul",
        "West Asia Standard Time" => "Asia/Tashkent",
        "Ekaterinburg Standard Time" => "Asia/Yekaterinburg",
        "Pakistan Standard Time" => "Asia/Karachi",
        "Qyzylorda Standard Time" => "Asia/Qyzylorda",
        "India Standard Time" => "Asia/Kolkata",
        "Sri Lanka Standard Time" => "Asia/Colombo",
        "Nepal Standard Time" => "Asia/Kathmandu",
        "Central Asia Standard Time" => "Asia/Bishkek",
        "Bangladesh Standard Time" => "Asia/Dhaka",
        "Omsk Standard Time" => "Asia/Omsk",
        "Myanmar Standard Time" => "Asia/Yangon",
        "SE Asia Standard Time" => "Asia/Bangkok",
        "Altai Standard Time" => "Asia/Barnaul",
        "W. Mongolia Standard Time" => "Asia/Hovd",
        "North Asia Standard Time" => "Asia/Krasnoyarsk",
        "N. Central Asia Standard Time" => "Asia/Novosibirsk",
        "Tomsk Standard Time" => "Asia/Tomsk",
        "China Standard Time" => "Asia/Shanghai",
        "North Asia East Standard Time" => "Asia/Irkutsk",
        "Singapore Standard Time" => "Asia/Singapore",
        "W. Australia Standard Time" => "Australia/Perth",
        "Taipei Standard Time" => "Asia/Taipei",
        "Ulaanbaatar Standard Time" => "Asia/Ulaanbaatar",
        "Aus Central W. Standard Time" => "Australia/Eucla",
        "Transbaikal Standard Time" => "Asia/Chita",
        "Tokyo Standard Time" => "Asia/Tokyo",
        "North Korea Standard Time" => "Asia/Pyongyang",
        "Korea Standard Time" => "Asia/Seoul",
        "Yakutsk Standard Time" => "Asia/Yakutsk",
        "Cen. Australia Standard Time" => "Australia/Adelaide",
        "AUS Central Standard Time" => "Australia/Darwin",
        "E. Australia Standard Time" => "Australia/Brisbane",
        "AUS Eastern Standard Time" => "Australia/Sydney",
        "West Pacific Standard Time" => "Pacific/Port_Moresby",
        "Tasmania Standard Time" => "Australia/Hobart",
        "Vladivostok Standard Time" => "Asia/Vladivostok",
        "Lord Howe Standard Time" => "Australia/Lord_Howe",
        "Bougainville Standard Time" => "Pacific/Bougainville",
        "Russia Time Zone 10" => "Asia/Srednekolymsk",
        "Magadan Standard Time" => "Asia/Magadan",
        "Norfolk Standard Time" => "Pacific/Norfolk",
        "Sakhalin Standard Time" => "Asia/Sakhalin",
        "Central Pacific Standard Time" => "Pacific/Guadalcanal",
        "Russia Time Zone 11" | "Kamchatka Standard Time" => "Asia/Kamchatka",
        "New Zealand Standard Time" => "Pacific/Auckland",
        "UTC+12" => "Etc/GMT-12",
        "Fiji Standard Time" => "Pacific/Fiji",
        "Chatham Islands Standard Time" => "Pacific/Chatham",
        "UTC+13" => "Etc/GMT-13",
        "Tonga Standard Time" => "Pacific/Tongatapu",
        "Samoa Standard Time" => "Pacific/Apia",
        "Line Islands Standard Time" => "Pacific/Kiritimati",
        _ => return None,
    })
}

/// A zone the `.ics` defines for itself: its id and every offset it moves to.
struct Defined {
    id: String,
    offsets: Vec<i32>,
}

/// `+0800`, `-0530` or `+053000` as seconds east of UTC.
fn offset_of(v: &str) -> Option<i32> {
    let v = v.trim();
    let sign = match v.get(..1)? {
        "+" => 1,
        "-" => -1,
        _ => return None,
    };
    let secs = match v.len() {
        5 => digits(v, 1..3)? * 3600 + digits(v, 3..5)? * 60,
        7 => digits(v, 1..3)? * 3600 + digits(v, 3..5)? * 60 + digits(v, 5..7)?,
        _ => return None,
    };
    Some(sign * i32::try_from(secs).ok()?)
}

fn on_clock<Z: TimeZone>(zone: &Z, wall: NaiveDateTime) -> Option<DateTime<Utc>> {
    // a time the clocks repeat is the first; one they skip is no time
    match zone.from_local_datetime(&wall) {
        LocalResult::Single(at) | LocalResult::Ambiguous(at, _) => Some(at.with_timezone(&Utc)),
        LocalResult::None => None,
    }
}

/// `wall` on the clock of zone `tzid`: an IANA name, a Windows one, or a zone
/// the `.ics` defines at one offset all year. A zone with rules of its own
/// that's none of those can't be read, so it's no invite.
fn in_zone(wall: NaiveDateTime, tzid: &str, defined: &[Defined]) -> Option<DateTime<Utc>> {
    let name = tzid.trim();
    // or it would find a VTIMEZONE that names no zone
    if name.is_empty() {
        return None;
    }
    let iana = name
        .parse::<Tz>()
        .ok()
        .or_else(|| windows_zone(name)?.parse::<Tz>().ok());
    if let Some(zone) = iana {
        return on_clock(&zone, wall);
    }
    let offsets = &defined.iter().find(|d| d.id == name)?.offsets;
    let first = *offsets.first()?;
    if offsets.iter().any(|o| *o != first) {
        return None;
    }
    on_clock(&FixedOffset::east_opt(first)?, wall)
}

fn time_of(l: &Line, defined: &[Defined]) -> Option<Time> {
    let v = l.value.trim();
    match l.param("VALUE") {
        Some(kind) if kind.eq_ignore_ascii_case("DATE") => return date_of(v).map(Time::Day),
        Some(kind) if !kind.eq_ignore_ascii_case("DATE-TIME") => return None,
        _ => {}
    }
    if v.len() == 8 {
        return date_of(v).map(Time::Day);
    }
    if let Some(utc) = v.strip_suffix('Z') {
        return wall_of(utc).map(|w| Time::At(w.and_utc()));
    }
    // a time with no zone is the reader's own clock, which an invite can't mean
    in_zone(wall_of(v)?, l.param("TZID")?, defined).map(Time::At)
}

/// A DURATION: `P1D`, `PT1H30M`, `P2W`.
fn duration_of(v: &str) -> Option<TimeDelta> {
    let v = v.trim();
    let rest = v.strip_prefix('+').unwrap_or(v).strip_prefix('P')?;
    let (date, time) = match rest.split_once('T') {
        Some((d, t)) if !t.is_empty() => (d, t),
        Some(_) => return None,
        None => (rest, ""),
    };
    // "P" alone says nothing of how long
    if date.is_empty() && time.is_empty() {
        return None;
    }
    let mut secs: i64 = 0;
    for (part, units) in [
        (date, &[('W', 604_800), ('D', 86_400)][..]),
        (time, &[('H', 3600), ('M', 60), ('S', 1)][..]),
    ] {
        let mut n = String::new();
        for c in part.chars() {
            if c.is_ascii_digit() {
                if n.len() >= 6 {
                    return None;
                }
                n.push(c);
                continue;
            }
            let (_, each) = units.iter().find(|(u, _)| *u == c)?;
            secs += n.parse::<i64>().ok()? * each;
            n.clear();
        }
        if !n.is_empty() {
            return None;
        }
    }
    // an event of no length is no meeting to answer
    (secs > 0 && secs <= DAYS_MAX * 86_400).then(|| TimeDelta::seconds(secs))
}

/// A `mailto:` value's address, if it's a plain one, as Compose takes them.
fn mailto(v: &str) -> String {
    let v = v.trim();
    match v.get(..7) {
        Some(scheme) if scheme.eq_ignore_ascii_case("mailto:") && !v[7..].contains('<') => {
            crate::compose::address(&v[7..]).unwrap_or_default()
        }
        _ => String::new(),
    }
}

fn person(l: &Line) -> Person {
    let response = l.param("PARTSTAT").and_then(|p| {
        Some(match p.to_ascii_uppercase().as_str() {
            "ACCEPTED" => "accepted",
            "DECLINED" => "declined",
            "TENTATIVE" => "tentative",
            "NEEDS-ACTION" => "needsAction",
            _ => return None,
        })
    });
    Person {
        name: one_line(l.param("CN").unwrap_or_default()),
        address: mailto(&l.value),
        response,
    }
}

/// The one VEVENT of an `.ics` and what it asks; `None` for anything this
/// reader doesn't follow.
pub fn read(ics: &str) -> Option<Event> {
    if ics.len() > ICS_MAX {
        return None;
    }
    // Outlook and Windows tools start a file with a byte-order mark
    let ics = ics.strip_prefix('\u{feff}').unwrap_or(ics);
    let mut stack: Vec<String> = Vec::new();
    let (mut method, mut calendars) = (None, 0);
    let mut events: Vec<Vec<Line>> = Vec::new();
    let mut defined: Vec<Defined> = Vec::new();
    for text in unfold(ics)? {
        let l = line(&text)?;
        match l.name.as_str() {
            "BEGIN" => {
                let what = l.value.trim().to_ascii_uppercase();
                if !is_name(&what) || stack.len() >= NESTING_MAX {
                    return None;
                }
                match (stack.len(), what.as_str()) {
                    (0, "VCALENDAR") => calendars += 1,
                    (0, _) => return None,
                    (1, "VEVENT") if events.len() < EVENTS_MAX => events.push(Vec::new()),
                    (_, "VEVENT") => return None,
                    (1, "VTIMEZONE") => defined.push(Defined {
                        id: String::new(),
                        offsets: Vec::new(),
                    }),
                    _ => {}
                }
                stack.push(what);
            }
            "END" => {
                if stack.pop()? != l.value.trim().to_ascii_uppercase() {
                    return None;
                }
            }
            _ => match (stack.len(), stack.last().map(String::as_str)) {
                (0, _) => return None,
                (1, _) if l.name == "METHOD" => {
                    if method.is_some() {
                        return None;
                    }
                    method = Some(match l.value.trim().to_ascii_uppercase().as_str() {
                        "REQUEST" => Method::Request,
                        "CANCEL" => Method::Cancel,
                        "REPLY" => Method::Reply,
                        _ => return None,
                    });
                }
                (2, Some("VEVENT")) => events.last_mut()?.push(l),
                (2, Some("VTIMEZONE")) if l.name == "TZID" => {
                    defined.last_mut()?.id = l.value.trim().to_owned();
                }
                (3, Some("STANDARD" | "DAYLIGHT"))
                    if stack[1] == "VTIMEZONE" && l.name == "TZOFFSETTO" =>
                {
                    defined.last_mut()?.offsets.push(offset_of(&l.value)?);
                }
                _ => {}
            },
        }
    }
    if !stack.is_empty() || calendars != 1 {
        return None;
    }
    // a series with changed instances comes as one VEVENT each: the series is
    // the one that isn't an instance
    let mut masters = events
        .iter()
        .filter(|e| !e.iter().any(|l| l.name == "RECURRENCE-ID"));
    let (Some(event), None) = (masters.next(), masters.next()) else {
        return None;
    };
    of_lines(event, method?, &defined)
}

fn of_lines(lines: &[Line], method: Method, defined: &[Defined]) -> Option<Event> {
    let once = |name: &str| -> Option<Option<&Line>> {
        let mut found = lines.iter().filter(|l| l.name == name);
        let first = found.next();
        found.next().is_none().then_some(first)
    };
    let uid = once("UID")?.map(|l| l.value.trim().to_owned())?;
    if uid.is_empty() || uid.len() > HEADER_MAX || uid.chars().any(char::is_control) {
        return None;
    }
    let start = time_of(once("DTSTART")??, defined)?;
    let end = match (once("DTEND")?, once("DURATION")?) {
        (Some(_), Some(_)) => return None,
        (Some(l), None) => time_of(l, defined)?,
        (None, Some(l)) => {
            let long = duration_of(&l.value)?;
            match start {
                Time::At(at) => Time::At(at.checked_add_signed(long)?),
                Time::Day(day) if long.num_seconds() % 86_400 == 0 => {
                    Time::Day(day.checked_add_signed(long)?)
                }
                Time::Day(_) => return None,
            }
        }
        (None, None) => match start {
            Time::At(_) => start,
            Time::Day(day) => Time::Day(day.succ_opt()?),
        },
    };
    let end = match (start, end) {
        (Time::At(from), Time::At(to)) if to >= from => end,
        // an end on or before the start still takes up its one day
        (Time::Day(from), Time::Day(to)) if to <= from => Time::Day(from.succ_opt()?),
        (Time::Day(_), Time::Day(_)) => end,
        _ => return None,
    };
    let text = |name: &str| -> Option<String> {
        Some(once(name)?.map_or_else(String::new, |l| one_line(&unescape(&l.value))))
    };
    let attendees = lines
        .iter()
        .filter(|l| l.name == "ATTENDEE")
        .take(ATTENDEES_MAX)
        .map(person)
        .collect();
    // an invitation that says it's cancelled is a cancel, however it's sent
    let cancelled =
        once("STATUS")?.is_some_and(|l| l.value.trim().eq_ignore_ascii_case("CANCELLED"));
    let method = match method {
        Method::Request if cancelled => Method::Cancel,
        other => other,
    };
    Some(Event {
        uid,
        method,
        title: text("SUMMARY")?,
        start,
        end,
        place: text("LOCATION")?,
        organizer: once("ORGANIZER")?.map(person).unwrap_or_default(),
        attendees,
    })
}

// --- In a mail -------------------------------------------------------------------

fn mime(part: &Value) -> String {
    part.get("mimeType")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase()
}

/// Where a message's invite parts are, as JSON pointers: its text/calendar
/// part first, then an application/ics file, each the first of its kind.
fn invite_parts(message: &Value) -> Vec<String> {
    fn walk(part: &Value, at: String, depth: usize, found: &mut [Option<String>; 2]) {
        match mime(part).as_str() {
            "text/calendar" if found[0].is_none() => found[0] = Some(at.clone()),
            "application/ics" if found[1].is_none() => found[1] = Some(at.clone()),
            _ => {}
        }
        if depth >= DEPTH_MAX {
            return;
        }
        let parts = part.get("parts").and_then(Value::as_array);
        for (i, p) in parts.into_iter().flatten().enumerate() {
            walk(p, format!("{at}/parts/{i}"), depth + 1, found);
        }
    }
    let mut found = [None, None];
    if let Some(payload) = message.get("payload") {
        walk(payload, "/payload".to_owned(), 0, &mut found);
    }
    found.into_iter().flatten().collect()
}

/// The event in the part at `at`, if it reads as one.
fn event_at(message: &Value, at: &str) -> Option<Event> {
    message
        .pointer(at)
        .and_then(text_of)
        .and_then(|ics| read(&ics))
}

/// A part's `body.data` as text in its charset, as Gmail's bodies are read,
/// if it holds no more than [`ICS_MAX`].
fn text_of(part: &Value) -> Option<String> {
    let data = part.pointer("/body/data")?.as_str()?;
    if data.len() > ICS_MAX / 3 * 4 + 8 {
        return None;
    }
    let bytes = gmail::part_bytes(part);
    // a byte-order mark before decoding, or a single-byte charset makes it words
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&bytes);
    Some(gmail::in_charset(bytes, &gmail::part_charset(part)))
}

/// Google Calendar's own sender of invites.
const CALENDAR_SENDER: &str = "calendar-notification@google.com";

/// Mail domains anyone can have an address at: there, the domain says
/// nothing of who the organiser is.
const PUBLIC_DOMAINS: &[&str] = &[
    "gmail.com",
    "googlemail.com",
    "outlook.com",
    "hotmail.com",
    "live.com",
    "yahoo.com",
    "icloud.com",
    "me.com",
    "mac.com",
    "proton.me",
    "protonmail.com",
    "aol.com",
    "gmx.com",
    "gmx.net",
];

/// Whether the mail comes from who sends the organiser's invites: Gmail's
/// own Authentication-Results vouch for its From ([`pictures::vouched`]),
/// and that From is Google Calendar's own sender, the organiser's address
/// at a public mail domain, or else the organiser's domain. Any other
/// google.com sender is a person, who could forward a crafted `.ics`. The
/// `.ics` is the sender's to write, UID and all, so only this says who sent it.
fn sent_by_organizer(message: &Value, organizer: &str) -> bool {
    let domain = |a: &str| a.rsplit_once('@').map(|(_, d)| d.to_ascii_lowercase());
    let (Some(from), Some(theirs)) = (pictures::from_of(message), domain(organizer)) else {
        return false;
    };
    let Some(from_domain) = domain(&from) else {
        return false;
    };
    if !pictures::vouched(message) {
        return false;
    }
    if from == CALENDAR_SENDER {
        return true;
    }
    if from_domain == "google.com" {
        return false;
    }
    if PUBLIC_DOMAINS.contains(&theirs.as_str()) {
        return from.eq_ignore_ascii_case(organizer);
    }
    pictures::aligned(&from_domain, &theirs) || pictures::aligned(&theirs, &from_domain)
}

/// The thread view's `invite` for a message, `None` for a reply that says
/// nothing.
fn json_of(message: &Value, event: &Event) -> Option<Value> {
    let mut v = event.json(&js::string_or_empty(message.get("id")))?;
    v["sender_ok"] = json!(sent_by_organizer(message, &event.organizer.address));
    Some(v)
}

/// What a message's invite parts came to.
enum Found {
    Reads(Value),
    /// Its bytes were wanted and didn't come: there's no answer yet.
    NotIn,
    /// Gmail sent bytes that won't decode, which a try again won't change.
    Damaged,
    Nothing,
}

/// The thread view's `invite` for `thread` (a `format=full` thread): the
/// newest invite in it that reads, or null. A part that doesn't read, or
/// came back damaged, never hides an older one that does; one whose bytes
/// didn't come stops the search, as it may be the newest that reads, and
/// gives `None`: then there's no answer yet, and a view saved with none
/// would say "no invite" for good. `{"damaged": true}` when nothing reads
/// and a part came back damaged.
///
/// With `fetch`, a part Gmail sends by attachment id is fetched; without,
/// it's not in. A sign-in that's gone or doesn't cover Gmail is said as itself.
pub fn fill(fetch: Option<(&Core, &Bearer)>, thread: &mut Value) -> Result<Option<Value>, Failure> {
    let Some(messages) = thread.get_mut("messages").and_then(Value::as_array_mut) else {
        return Ok(Some(Value::Null));
    };
    in_messages(fetch, messages)
}

fn in_messages(
    fetch: Option<(&Core, &Bearer)>,
    messages: &mut [Value],
) -> Result<Option<Value>, Failure> {
    let mut damaged = false;
    for message in messages.iter_mut().rev() {
        match in_message(fetch, message)? {
            Found::Reads(v) => return Ok(Some(v)),
            Found::NotIn => return Ok(None),
            Found::Damaged => damaged = true,
            Found::Nothing => {}
        }
    }
    Ok(Some(if damaged {
        json!({"damaged": true})
    } else {
        Value::Null
    }))
}

/// What a thread view says of an invite whose bytes didn't come.
pub fn pending() -> Value {
    json!({"pending": true})
}

/// The letter's part first; the file only when the letter's gives no event.
fn in_message(fetch: Option<(&Core, &Bearer)>, message: &mut Value) -> Result<Found, Failure> {
    let mut damaged = false;
    for at in invite_parts(message) {
        match bytes_in(fetch, message, &at)? {
            Bytes::In => {}
            Bytes::NotIn => return Ok(Found::NotIn),
            Bytes::Damaged => {
                damaged = true;
                continue;
            }
        }
        if let Some(v) = event_at(message, &at).and_then(|e| json_of(message, &e)) {
            return Ok(Found::Reads(v));
        }
    }
    Ok(if damaged {
        Found::Damaged
    } else {
        Found::Nothing
    })
}

/// How long an `.ics` may take.
const FETCH_WITHIN: std::time::Duration = std::time::Duration::from_secs(30);

enum Bytes {
    /// In the part, or never coming: too big, or gone from Gmail.
    In,
    NotIn,
    Damaged,
}

fn bytes_in(
    fetch: Option<(&Core, &Bearer)>,
    message: &mut Value,
    at: &str,
) -> Result<Bytes, Failure> {
    let id = js::string_or_empty(message.get("id"));
    let Some(part) = message.pointer_mut(at) else {
        return Ok(Bytes::In);
    };
    let size = part.pointer("/body/size").and_then(Value::as_u64);
    let attachment = part.pointer("/body/attachmentId").and_then(Value::as_str);
    // one too big is no invite whenever it's asked, so that answer stands
    let wanted = part.pointer("/body/data").is_none() && size.is_some_and(|s| s <= ICS_MAX as u64);
    let (true, Some(attachment)) = (wanted, attachment) else {
        return Ok(Bytes::In);
    };
    let Some((core, auth)) = fetch else {
        return Ok(Bytes::NotIn);
    };
    match core
        .gmail
        .attachment_within(auth, &id, attachment, FETCH_WITHIN)
    {
        Ok(Some(bytes)) if bytes.len() <= ICS_MAX => {
            part["body"]["data"] =
                json!(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes));
            Ok(Bytes::In)
        }
        // too big, or gone from Gmail: that lasts
        Ok(_) => Ok(Bytes::In),
        Err(Failure::Plain(why)) if why == gmail::DAMAGED => Ok(Bytes::Damaged),
        Err(e @ (Failure::AccountRequired | Failure::NeedsAccess)) => Err(e),
        Err(_) => Ok(Bytes::NotIn),
    }
}

/// [`fill`]'s answer for messages whose bytes are all in, else pending.
#[cfg(test)]
fn of(messages: &[Value]) -> Value {
    in_messages(None, &mut messages.to_vec())
        .unwrap()
        .unwrap_or_else(pending)
}

// --- The tools -------------------------------------------------------------------

fn refusal(code: &str, why: &str) -> Value {
    ok(json!({"refused": code, "message": why}))
}

/// Whether a saved view's `invite` is a whole answer: not pending, and read
/// since invites carried who sent them. A view saved before invites were
/// read has no `invite` at all, which is "not read yet", not "none".
fn settled(invite: &Value) -> bool {
    invite.is_null() || invite.get("damaged").is_some() || invite.get("sender_ok").is_some()
}

/// The invite in thread `id`: from its saved view, else from Gmail, then
/// kept in that view so the next look needn't fetch it again. `None` when its
/// bytes didn't come: ask again.
fn of_thread(
    core: &Core,
    auth: &Bearer,
    account: &str,
    id: &str,
) -> Result<Option<Value>, Failure> {
    let saved = with_store(core, |store| store.view(account, id, now_ms()))?;
    if let Some(invite) = saved
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|mut v| v.get_mut("invite").map(Value::take))
        .filter(settled)
    {
        return Ok(Some(invite));
    }
    let Some(mut full) = core.gmail.thread_full(auth, id)? else {
        return Ok(Some(Value::Null));
    };
    let invite = fill(Some((core, auth)), &mut full)?;
    if let Some(invite) = &invite {
        keep(core, account, id, invite);
    }
    Ok(invite)
}

/// `invite` into thread `id`'s saved view, if it has one. Only a thread's
/// opening saves a view; this one is a cache, so a miss costs a fetch later.
fn keep(core: &Core, account: &str, id: &str, invite: &Value) {
    let saved = with_store(core, |store| store.view(account, id, now_ms()));
    let Some(mut view) = saved
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
    else {
        return;
    };
    view["invite"] = invite.clone();
    let put = with_own_store(core, account, |store| {
        store.put_view(account, id, &view.to_string(), now_ms())
    });
    if put.is_err() {
        eprintln!("buwiz-gws: couldn't keep an invite in its thread's view");
    }
}

/// The invite a thread's saved view holds, for a view made again from it.
pub fn saved(core: &Core, account: &str, id: &str) -> Option<Value> {
    let saved = with_store(core, |store| store.view(account, id, now_ms())).ok()??;
    let mut view = serde_json::from_str::<Value>(&saved).ok()?;
    view.get_mut("invite").map(Value::take)
}

fn of_message(core: &Core, auth: &Bearer, id: &str) -> Result<Option<Value>, Failure> {
    let Some(message) = core.gmail.full(auth, id)? else {
        return Ok(Some(Value::Null));
    };
    in_messages(Some((core, auth)), &mut [message])
}

/// An invite whose bytes didn't come: a try again, never "no invite".
fn again() -> Value {
    fail(
        "busy",
        "Gmail didn't send this invite just now. Try again in a moment.",
    )
}

/// The person's primary calendar and the event in it with `uid`, cancelled
/// or not.
fn in_calendar(
    core: &Core,
    auth: &Bearer,
    account: &str,
    uid: &str,
) -> Result<Option<(String, Value)>, Failure> {
    let calendar = calendar::calendar_or_primary(core, account, "")?;
    let event = core.calendar.by_ical_uid(auth, &calendar, uid)?;
    Ok(event.map(|e| (calendar, e)))
}

fn is_cancelled(event: &Value) -> bool {
    event.get("status").and_then(Value::as_str) == Some("cancelled")
}

/// The person among an event's guests.
fn mine<'a>(event: &'a Value, account: &str) -> Option<&'a Value> {
    let guests = event.get("attendees").and_then(Value::as_array);
    guests
        .into_iter()
        .flatten()
        .find(|a| calendar::is_self(a, account))
}

/// A side of a Calendar event as the invite writes it: a UTC moment, or a date.
fn side_of(event: &Value, key: &str) -> Option<String> {
    let side = event.get(key)?;
    if let Some(day) = side.get("date").and_then(Value::as_str) {
        return Some(day.to_owned());
    }
    let at = DateTime::parse_from_rfc3339(side.get("dateTime")?.as_str()?).ok()?;
    Some(
        at.with_timezone(&Utc)
            .to_rfc3339_opts(SecondsFormat::Secs, true),
    )
}

/// Calendar's event as the card shows it, held to one line each as the
/// invite's words are: what a press answers is what Calendar holds.
fn shown(event: &Value) -> Value {
    let organizer = event.get("organizer").unwrap_or(&Value::Null);
    let name = match calendar::text(organizer, "displayName") {
        name if name.trim().is_empty() => calendar::text(organizer, "email"),
        name => name,
    };
    json!({
        "title": one_line(&calendar::text(event, "summary")),
        "start": side_of(event, "start"),
        "end": side_of(event, "end"),
        "all_day": event.pointer("/start/date").is_some(),
        "place": one_line(&calendar::text(event, "location")),
        "organizer": one_line(&name),
    })
}

/// Whether Calendar's event is the one the invite describes: same
/// organiser, start, title and place. Each of those is the sender's to write
/// as much as the UID is, so this alone can't tell a forger from the
/// organiser ([`sent_by_organizer`] does); what it holds is that a press
/// answers only an event that reads as the invite did.
fn matches(invite: &Value, event: &Value) -> bool {
    let organizer = event
        .pointer("/organizer/email")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let named = calendar::text(invite, "organizer_address");
    let start = invite.get("start").and_then(Value::as_str);
    let ours = shown(event);
    !named.is_empty()
        && organizer.eq_ignore_ascii_case(&named)
        && start.is_some()
        && side_of(event, "start").as_deref() == start
        && ours["title"] == invite["title"]
        && ours["place"] == invite["place"]
}

/// A failure in the words a person reads.
fn words_of(e: Failure) -> String {
    let v = failed(e);
    v.pointer("/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or("Calendar didn't answer just now. Try again.")
        .to_owned()
}

/// What Calendar says of an invitation. Nothing of the event when it doesn't
/// match: a forger's words must not show under Calendar's name. Calendar not
/// answering is a line on the card, which keeps what Gmail sent.
fn calendar_says(core: &Core, auth: &Bearer, account: &str, invite: &Value) -> Value {
    if invite.get("sender_ok") != Some(&Value::Bool(true)) {
        return json!({"unverified": true});
    }
    let uid = js::string_or_empty(invite.get("uid"));
    match in_calendar(core, auth, account, &uid) {
        Err(e) => json!({"error": words_of(e)}),
        Ok(None) => json!({"found": false}),
        Ok(Some((_, event))) if is_cancelled(&event) => json!({"found": true, "cancelled": true}),
        Ok(Some((_, event))) if !matches(invite, &event) => {
            json!({"found": true, "cancelled": false, "matches": false})
        }
        Ok(Some((_, event))) => {
            let me = mine(&event, account);
            json!({"found": true, "cancelled": false, "matches": true, "guest": me.is_some(), "response": me.and_then(|a| a.get("responseStatus")), "event": shown(&event)})
        }
    }
}

/// `gws_mail_invite`: the invite, and for an invitation whether Calendar has
/// its event, whether that's the one it describes, what the person answered
/// there, and Calendar's own words for it.
fn look(core: &Core, args: &Value) -> Result<Value, Failure> {
    let (auth, account) = signed_in(core)?;
    let thread = js::trim(&js::string_or_empty(args.get("thread"))).to_owned();
    let message = js::trim(&js::string_or_empty(args.get("message"))).to_owned();
    let invite = match (message.is_empty(), thread.is_empty()) {
        (false, _) if gmail::id_ok(&message) => of_message(core, &auth, &message)?,
        (true, false) if gmail::id_ok(&thread) => of_thread(core, &auth, &account, &thread)?,
        _ => return Ok(refusal("invalid_args", "That isn't a mail.")),
    };
    let Some(invite) = invite else {
        return Ok(again());
    };
    if invite.get("method").and_then(Value::as_str) != Some("request") {
        return Ok(ok(json!({"invite": invite, "calendar": null})));
    }
    let calendar = calendar_says(core, &auth, &account, &invite);
    Ok(ok(json!({"invite": invite, "calendar": calendar})))
}

/// `gws_mail_invite_reply`: the person's Yes, No or Maybe, through Calendar.
/// It always tells the organiser: that's what answering an invite is for.
/// Once the answer is written it's sent, and said at once: Calendar's sync
/// comes after, so a slow one can't make a second press send it again.
fn reply(core: &Core, args: &Value) -> Result<Value, Failure> {
    let (auth, account) = signed_in(core)?;
    let message = js::trim(&js::string_or_empty(args.get("message"))).to_owned();
    if !gmail::id_ok(&message) {
        return Ok(refusal("invalid_args", "That isn't a mail."));
    }
    let response = js::string_or_empty(args.get("response"));
    if !["accepted", "declined", "tentative"].contains(&response.as_str()) {
        return Ok(refusal(
            "invalid_args",
            "A reply is accepted, declined or tentative.",
        ));
    }
    let Some(invite) = of_message(core, &auth, &message)? else {
        return Ok(again());
    };
    match invite.get("method").and_then(Value::as_str) {
        Some("request") => {}
        Some(_) => {
            return Ok(refusal(
                "invalid_args",
                "Only an invitation can be answered.",
            ));
        }
        None => return Ok(refusal("not_found", "This mail holds no invite.")),
    }
    if invite.get("sender_ok") != Some(&Value::Bool(true)) {
        return Ok(refusal("unverified", UNVERIFIED));
    }
    let uid = js::string_or_empty(invite.get("uid"));
    let Some((calendar, event)) = in_calendar(core, &auth, &account, &uid)? else {
        return Ok(refusal("not_found", NOT_YET));
    };
    if is_cancelled(&event) {
        return Ok(refusal("not_found", CANCELLED));
    }
    if !matches(&invite, &event) {
        return Ok(refusal("changed", DIFFERS));
    }
    let args = json!({"calendar": calendar, "id": event.get("id"), "response": response});
    let how = calendar::Answering {
        notify_all: true,
        stands_after_write: true,
    };
    let answer = calendar::rsvp_with(core, &args, Some(event), how)?;
    let content = &answer["structuredContent"];
    if answer.get("isError") == Some(&Value::Bool(true)) || content.get("refused").is_some() {
        return Ok(answer);
    }
    Ok(ok(
        json!({"sent": true, "event": content.get("event").cloned().unwrap_or(Value::Null)}),
    ))
}

/// Runs an invite tool; `None` for any other tool.
pub fn run(core: &Core, tool: &str, args: &Value) -> Option<Value> {
    let result = match tool {
        MAIL_INVITE => look(core, args),
        MAIL_INVITE_REPLY => reply(core, args),
        _ => return None,
    };
    core.sync.seen();
    Some(result.unwrap_or_else(failed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> Time {
        Time::At(DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc))
    }

    fn day(s: &str) -> Time {
        Time::Day(NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap())
    }

    /// Google Calendar's own, as in the rich stub: UTC times, CRLF.
    const GOOGLE: &str = "BEGIN:VCALENDAR\r\nPRODID:-//Google Inc//Google Calendar 70.9054//EN\r\nVERSION:2.0\r\nCALSCALE:GREGORIAN\r\nMETHOD:REQUEST\r\nBEGIN:VTIMEZONE\r\nTZID:Asia/Manila\r\nX-LIC-LOCATION:Asia/Manila\r\nBEGIN:STANDARD\r\nTZOFFSETFROM:+0800\r\nTZOFFSETTO:+0800\r\nTZNAME:PST\r\nDTSTART:19700101T000000\r\nEND:STANDARD\r\nEND:VTIMEZONE\r\nBEGIN:VEVENT\r\nDTSTART:20261110T020000Z\r\nDTEND:20261110T030000Z\r\nDTSTAMP:20261106T090000Z\r\nORGANIZER;CN=Ana Reyes:mailto:ana@example.com\r\nUID:fake-roof-inspection@google.com\r\nATTENDEE;CUTYPE=INDIVIDUAL;ROLE=REQ-PARTICIPANT;PARTSTAT=ACCEPTED;RSVP=TRUE\r\n ;CN=Ana Reyes;X-NUM-GUESTS=0:mailto:ana@example.com\r\nATTENDEE;CUTYPE=INDIVIDUAL;ROLE=REQ-PARTICIPANT;PARTSTAT=NEEDS-ACTION;RSVP=\r\n TRUE;CN=owner@example.com;X-NUM-GUESTS=0:mailto:owner@example.com\r\nX-GOOGLE-CONFERENCE:https://meet.example.com/abc-defg-hij\r\nCREATED:20261106T085900Z\r\nDESCRIPTION:Bring the ladder\\, and the quote.\\n\\n-::~:~::~:~:~:~:~:~:~:~:~:~:~:~:~:~:~:~:~:~:~:~:~:~::~:~::-\r\nLAST-MODIFIED:20261106T085900Z\r\nLOCATION:12 Mabini St\\, Makati\r\nSEQUENCE:0\r\nSTATUS:CONFIRMED\r\nSUMMARY:Roof inspection\r\nTRANSP:OPAQUE\r\nBEGIN:VALARM\r\nACTION:DISPLAY\r\nDESCRIPTION:This is an event reminder\r\nTRIGGER:-P0DT0H10M0S\r\nEND:VALARM\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

    /// Outlook's: a Windows zone name, quoted names, a long UID folded with a tab.
    const OUTLOOK: &str = "BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nPRODID:Microsoft Exchange Server 2010\r\nVERSION:2.0\r\nBEGIN:VTIMEZONE\r\nTZID:Pacific Standard Time\r\nBEGIN:STANDARD\r\nDTSTART:16010101T020000\r\nTZOFFSETFROM:-0700\r\nTZOFFSETTO:-0800\r\nRRULE:FREQ=YEARLY;INTERVAL=1;BYDAY=1SU;BYMONTH=11\r\nEND:STANDARD\r\nBEGIN:DAYLIGHT\r\nDTSTART:16010101T020000\r\nTZOFFSETFROM:-0800\r\nTZOFFSETTO:-0700\r\nRRULE:FREQ=YEARLY;INTERVAL=1;BYDAY=2SU;BYMONTH=3\r\nEND:DAYLIGHT\r\nEND:VTIMEZONE\r\nBEGIN:VEVENT\r\nORGANIZER;CN=\"Reyes, Ana\":mailto:ana@example.com\r\nATTENDEE;ROLE=REQ-PARTICIPANT;PARTSTAT=NEEDS-ACTION;RSVP=TRUE;CN=\"Owner\":mailto:Owner@Example.com\r\nDESCRIPTION;LANGUAGE=en-US:Quarterly review\\n\r\nUID:040000008200E00074C5B7101A82E00800000000A0B1C2D3E4F5A60100000000000000001000\r\n\t0000D1E2F3A4B5C6D7E8F9\r\nSUMMARY;LANGUAGE=en-US:Budget review\\; Q4\r\nDTSTART;TZID=Pacific Standard Time:20261203T093000\r\nDTEND;TZID=Pacific Standard Time:20261203T103000\r\nCLASS:PUBLIC\r\nPRIORITY:5\r\nDTSTAMP:20261106T170000Z\r\nTRANSP:OPAQUE\r\nSTATUS:CONFIRMED\r\nSEQUENCE:0\r\nLOCATION;LANGUAGE=en-US:Conf Room 4\r\nX-MICROSOFT-CDO-APPT-SEQUENCE:0\r\nX-MICROSOFT-CDO-BUSYSTATUS:TENTATIVE\r\nX-MICROSOFT-CDO-ALLDAYEVENT:FALSE\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

    /// Apple Calendar's: an IANA TZID with its zone written out, LF lines, an
    /// all-day event.
    const APPLE: &str = "BEGIN:VCALENDAR\nVERSION:2.0\nPRODID:-//Apple Inc.//macOS 15.1//EN\nCALSCALE:GREGORIAN\nMETHOD:REQUEST\nBEGIN:VEVENT\nTRANSP:TRANSPARENT\nDTEND;VALUE=DATE:20261126\nUID:6C1D4A11-2B0E-4F5A-9C8D-7E6F5A4B3C2D\nDTSTAMP:20261106T090000Z\nLOCATION:Lola's house\\nBacolod\nSEQUENCE:0\nSUMMARY:Family day \\\\o/\nDTSTART;VALUE=DATE:20261124\nORGANIZER;CN=\"Cara Lim\";EMAIL=\"cara@example.com\":mailto:cara@example.com\nATTENDEE;CN=\"Owner\";CUTYPE=INDIVIDUAL;EMAIL=\"owner@example.com\";PARTSTAT=TENTATIVE;ROLE=REQ-PARTICIPANT:mailto:owner@example.com\nEND:VEVENT\nEND:VCALENDAR\n";

    #[test]
    fn a_google_invite_reads_with_its_folded_guests_and_escapes() {
        let e = read(GOOGLE).unwrap();
        assert_eq!(e.method, Method::Request);
        assert_eq!(e.uid, "fake-roof-inspection@google.com");
        assert_eq!(e.title, "Roof inspection");
        assert_eq!(e.place, "12 Mabini St, Makati");
        assert_eq!(e.start, at("2026-11-10T02:00:00Z"));
        assert_eq!(e.end, at("2026-11-10T03:00:00Z"));
        assert_eq!(e.organizer.shown(), "Ana Reyes");
        assert_eq!(e.organizer.address, "ana@example.com");
        let me = e.me("OWNER@example.com").unwrap();
        assert_eq!(me.response, Some("needsAction"));
        assert_eq!(e.attendees.len(), 2, "the folded line is one guest");
    }

    #[test]
    fn an_outlook_invite_reads_its_windows_zone_and_its_folded_uid() {
        let e = read(OUTLOOK).unwrap();
        assert_eq!(
            e.uid,
            "040000008200E00074C5B7101A82E00800000000A0B1C2D3E4F5A601000000000000000010000000D1E2F3A4B5C6D7E8F9"
        );
        assert_eq!(e.title, "Budget review; Q4");
        assert_eq!(e.organizer.shown(), "Reyes, Ana");
        // 09:30 in Los Angeles in December is 17:30 UTC
        assert_eq!(e.start, at("2026-12-03T17:30:00Z"));
        assert_eq!(e.end, at("2026-12-03T18:30:00Z"));
        assert_eq!(e.me("owner@example.com").unwrap().name, "Owner");
    }

    #[test]
    fn an_apple_all_day_invite_is_its_dates() {
        let e = read(APPLE).unwrap();
        assert_eq!(e.start, day("2026-11-24"));
        assert_eq!(e.end, day("2026-11-26"));
        assert_eq!(e.title, "Family day \\o/");
        assert_eq!(e.place, "Lola's house Bacolod", "on one line");
        assert_eq!(e.organizer.shown(), "Cara Lim");
        assert_eq!(
            e.me("owner@example.com").unwrap().response,
            Some("tentative")
        );
        let v = e.json("m1").unwrap();
        assert_eq!(v["all_day"], true);
        assert_eq!(v["start"], "2026-11-24");
    }

    #[test]
    fn a_tzid_is_read_on_that_zones_clock_across_daylight_saving() {
        let ics = |start: &str| {
            format!(
                "BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:u1\r\nDTSTART;TZID=America/New_York:{start}\r\nDURATION:PT1H30M\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
            )
        };
        let summer = read(&ics("20260701T090000")).unwrap();
        assert_eq!(summer.start, at("2026-07-01T13:00:00Z"));
        assert_eq!(summer.end, at("2026-07-01T14:30:00Z"));
        let winter = read(&ics("20261201T090000")).unwrap();
        assert_eq!(winter.start, at("2026-12-01T14:00:00Z"));
        // 02:30 that March morning never happens in New York
        assert_eq!(read(&ics("20260308T023000")), None);
    }

    #[test]
    fn a_zone_the_ics_defines_at_one_offset_reads_and_one_with_rules_does_not() {
        let ics = |zone: &str| {
            format!(
                "BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VTIMEZONE\r\nTZID:Office time\r\n{zone}END:VTIMEZONE\r\nBEGIN:VEVENT\r\nUID:u1\r\nDTSTART;TZID=Office time:20261110T100000\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
            )
        };
        let fixed = read(&ics(
            "BEGIN:STANDARD\r\nDTSTART:19700101T000000\r\nTZOFFSETTO:+0800\r\nEND:STANDARD\r\n",
        ))
        .unwrap();
        assert_eq!(fixed.start, at("2026-11-10T02:00:00Z"));
        assert_eq!(fixed.end, fixed.start, "no end is the start");
        let ruled = ics(
            "BEGIN:STANDARD\r\nTZOFFSETTO:-0500\r\nEND:STANDARD\r\nBEGIN:DAYLIGHT\r\nTZOFFSETTO:-0400\r\nEND:DAYLIGHT\r\n",
        );
        assert_eq!(read(&ruled), None);
    }

    #[test]
    fn a_cancel_and_a_reply_say_what_they_are() {
        let cancel = GOOGLE.replace("METHOD:REQUEST", "METHOD:CANCEL");
        assert_eq!(read(&cancel).unwrap().method, Method::Cancel);
        let reply = "BEGIN:VCALENDAR\r\nMETHOD:REPLY\r\nBEGIN:VEVENT\r\nUID:u1\r\nDTSTART:20261110T020000Z\r\nDTEND:20261110T030000Z\r\nSUMMARY:Roof inspection\r\nORGANIZER:mailto:owner@example.com\r\nATTENDEE;PARTSTAT=DECLINED;CN=Ben Cruz:mailto:ben@example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let e = read(reply).unwrap();
        assert_eq!(e.method, Method::Reply);
        let v = e.json("m1").unwrap();
        assert_eq!(
            v["reply"],
            json!({"who": "Ben Cruz", "response": "declined"})
        );
        assert_eq!(
            v["organizer"], "owner@example.com",
            "an address stands in for a name"
        );
        // a reply that says nothing is no card
        let silent = reply.replace("PARTSTAT=DECLINED", "PARTSTAT=NEEDS-ACTION");
        assert_eq!(read(&silent).unwrap().json("m1"), None);
    }

    #[test]
    fn a_series_with_a_changed_instance_is_the_series() {
        let ics = "BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:s1\r\nDTSTART:20261110T020000Z\r\nRRULE:FREQ=WEEKLY\r\nSUMMARY:Standup\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:s1\r\nRECURRENCE-ID:20261117T020000Z\r\nDTSTART:20261117T030000Z\r\nSUMMARY:Standup, later\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        assert_eq!(read(ics).unwrap().title, "Standup");
    }

    #[test]
    fn anything_odd_is_no_invite() {
        let cases = [
            // no event, no method, a method it doesn't answer
            "BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nEND:VCALENDAR\r\n".to_owned(),
            GOOGLE.replace("METHOD:REQUEST\r\n", ""),
            GOOGLE.replace("METHOD:REQUEST", "METHOD:PUBLISH"),
            GOOGLE.replace("METHOD:REQUEST", "METHOD:COUNTER"),
            // two events that are both the series
            GOOGLE.replace(
                "END:VEVENT\r\n",
                "END:VEVENT\r\nBEGIN:VEVENT\r\nUID:x\r\nDTSTART:20261110T020000Z\r\nEND:VEVENT\r\n",
            ),
            // broken nesting, no end, text before it starts
            GOOGLE.replace("END:VALARM\r\n", ""),
            GOOGLE.replace("END:VCALENDAR\r\n", ""),
            format!("hello\r\n{GOOGLE}"),
            format!(" folded first\r\n{GOOGLE}"),
            // a line with no colon, a parameter with no value
            GOOGLE.replace("TRANSP:OPAQUE", "TRANSP OPAQUE"),
            GOOGLE.replace("ORGANIZER;CN=Ana Reyes", "ORGANIZER;CN"),
            GOOGLE.replace("ORGANIZER;CN=Ana Reyes", "ORGANIZER;CN=\"Ana Reyes"),
            // no UID, two UIDs, no start, a floating time, a bad date
            GOOGLE.replace("UID:fake-roof-inspection@google.com\r\n", ""),
            GOOGLE.replace("UID:fake", "UID:a\r\nUID:fake"),
            GOOGLE.replace("DTSTART:20261110T020000Z\r\n", ""),
            GOOGLE.replace("DTSTART:20261110T020000Z", "DTSTART:20261110T020000"),
            GOOGLE.replace("DTSTART:20261110T020000Z", "DTSTART:20261310T020000Z"),
            GOOGLE.replace("DTSTART:20261110T020000Z", "DTSTART:2026111OT020000Z"),
            GOOGLE.replace(
                "DTSTART:20261110T020000Z",
                "DTSTART;TZID=Mars/Olympus:20261110T020000",
            ),
            // an end before the start, an end of another kind, both an end and a duration
            GOOGLE.replace("DTEND:20261110T030000Z", "DTEND:20261110T010000Z"),
            GOOGLE.replace("DTEND:20261110T030000Z", "DTEND;VALUE=DATE:20261111"),
            GOOGLE.replace(
                "DTEND:20261110T030000Z",
                "DTEND:20261110T030000Z\r\nDURATION:PT1H",
            ),
            GOOGLE.replace("DTEND:20261110T030000Z", "DURATION:P99999999D"),
            GOOGLE.replace("DTEND:20261110T030000Z", "DURATION:1H"),
        ];
        for (i, ics) in cases.iter().enumerate() {
            assert_eq!(read(ics), None, "case {i}");
        }
    }

    #[test]
    fn a_huge_or_endless_ics_is_no_invite_and_no_panic() {
        let huge = format!(
            "{}{}",
            GOOGLE.replace("END:VCALENDAR\r\n", ""),
            "X-PAD:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\r\n".repeat(ICS_MAX / 40)
        );
        assert_eq!(read(&huge), None);
        let lines = format!(
            "BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\n{}END:VCALENDAR\r\n",
            "X:1\r\n".repeat(LINES_MAX)
        );
        assert!(lines.len() < ICS_MAX);
        assert_eq!(read(&lines), None);
        let deep = format!(
            "BEGIN:VCALENDAR\r\n{}",
            "BEGIN:X\r\n".repeat(NESTING_MAX + 1)
        );
        assert_eq!(read(&deep), None);
        // whatever the bytes, an answer
        for junk in [
            "",
            "\r\n\r\n",
            ":",
            ";;;:",
            "BEGIN:VCALENDAR",
            "END:VCALENDAR\r\n",
            "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nDTSTART:\u{fffd}\u{0}\r\n",
            "BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:\u{7}\r\nDTSTART:20261110T020000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
            "BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:é\r\nDTSTART:2026111é0200Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
        ] {
            assert_eq!(read(junk), None, "{junk:?}");
        }
    }

    #[test]
    fn a_strangers_long_words_are_held_to_one_short_line() {
        let long = "w".repeat(HEADER_MAX * 4);
        let ics = GOOGLE.replace(
            "SUMMARY:Roof inspection",
            &format!("SUMMARY:Roof\\ninspection\u{7}{long}"),
        );
        let e = read(&ics).unwrap();
        assert!(
            e.title.starts_with("Roof inspection w"),
            "{}",
            &e.title[..20]
        );
        assert!(e.title.len() <= HEADER_MAX);
        assert!(!e.title.chars().any(char::is_control));
        let extra = "ATTENDEE;PARTSTAT=ACCEPTED:mailto:x@example.com\r\n".repeat(ATTENDEES_MAX);
        let many = GOOGLE.replace("ATTENDEE;CUTYPE", &format!("{extra}ATTENDEE;CUTYPE"));
        assert!(read(&many).unwrap().attendees.len() <= ATTENDEES_MAX);
        // an address that isn't one is none
        let odd = GOOGLE.replace(
            "mailto:ana@example.com\r\nUID",
            "mailto:ana at example\r\nUID",
        );
        assert_eq!(read(&odd).unwrap().organizer.address, "");
    }

    #[test]
    fn a_byte_order_mark_is_no_part_of_the_invite() {
        let e = read(&format!("\u{feff}{GOOGLE}")).unwrap();
        assert_eq!(e.title, "Roof inspection");
    }

    #[test]
    fn a_tzid_with_spaces_finds_the_zone_the_ics_defines() {
        let ics = "BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VTIMEZONE\r\nTZID:Office time\r\nBEGIN:STANDARD\r\nTZOFFSETTO:+0800\r\nEND:STANDARD\r\nEND:VTIMEZONE\r\nBEGIN:VEVENT\r\nUID:u1\r\nDTSTART;TZID=\" Office time \":20261110T100000\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        assert_eq!(read(ics).unwrap().start, at("2026-11-10T02:00:00Z"));
    }

    fn message(id: &str, parts: Value) -> Value {
        json!({"id": id, "payload": {"mimeType": "multipart/mixed", "parts": parts}})
    }

    fn part(mime: &str, ics: &str) -> Value {
        json!({"mimeType": mime, "filename": "invite.ics", "body": {"data": base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(ics)}})
    }

    fn part_in(charset: &str, bytes: &[u8]) -> Value {
        json!({"mimeType": "text/calendar", "headers": [{"name": "Content-Type", "value": format!("text/calendar; charset=\"{charset}\"; method=REQUEST")}], "body": {"data": base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)}})
    }

    #[test]
    fn a_part_reads_in_the_charset_it_names() {
        let ics = |title: &[u8]| {
            let mut out = b"BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:u1\r\nDTSTART:20261110T020000Z\r\nSUMMARY:".to_vec();
            out.extend_from_slice(title);
            out.extend_from_slice(b"\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n");
            out
        };
        let title = |p: Value| of(&[message("m1", json!([p]))])["title"].clone();
        // "Café “Tuesday”" as Outlook writes it in windows-1252
        assert_eq!(
            title(part_in("windows-1252", &ics(b"Caf\xe9 \x93Tuesday\x94"))),
            "Caf\u{e9} \u{201c}Tuesday\u{201d}"
        );
        assert_eq!(title(part_in("ISO-8859-1", &ics(b"Caf\xe9"))), "Caf\u{e9}");
        assert_eq!(title(part_in("utf-8", &ics("Café".as_bytes()))), "Café");
        // one it doesn't know is read as UTF-8, as a mail's body is
        assert_eq!(title(part_in("x-klingon", &ics(b"Cafe"))), "Cafe");
    }

    #[test]
    fn a_byte_order_mark_in_a_single_byte_charset_is_still_no_part_of_it() {
        let mut bytes = b"\xef\xbb\xbf".to_vec();
        bytes.extend_from_slice(GOOGLE.as_bytes());
        let v = of(&[message("m1", json!([part_in("windows-1252", &bytes)]))]);
        assert_eq!(v["title"], "Roof inspection");
    }

    #[test]
    fn an_empty_tzid_is_no_zone_even_beside_a_zone_with_no_name() {
        let ics = "BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VTIMEZONE\r\nBEGIN:STANDARD\r\nTZOFFSETTO:+0800\r\nEND:STANDARD\r\nEND:VTIMEZONE\r\nBEGIN:VEVENT\r\nUID:u1\r\nDTSTART;TZID=:20261110T100000\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        assert_eq!(read(ics), None);
        assert_eq!(read(&ics.replace("TZID=:", "TZID=\" \":")), None);
    }

    #[test]
    fn a_letter_part_with_no_event_falls_back_to_the_file() {
        let m = message(
            "m1",
            json!([{"mimeType": "multipart/alternative", "parts": [part("text/calendar", "BEGIN:VCALENDAR\r\n")]}, part("application/ics", OUTLOOK)]),
        );
        assert_eq!(of(&[m])["title"], "Budget review; Q4");
    }

    #[test]
    fn calendars_event_matches_only_with_the_invites_organiser_start_title_and_place() {
        let e = read(GOOGLE).unwrap().json("m1").unwrap();
        let event = |organizer: &str, start: Value, title: &str, place: &str| json!({"organizer": {"email": organizer}, "start": start, "summary": title, "location": place});
        let at = json!({"dateTime": "2026-11-10T10:00:00+08:00"});
        let (title, place) = ("Roof inspection", "12 Mabini St, Makati");
        assert!(matches(
            &e,
            &event("ANA@example.com", at.clone(), title, place)
        ));
        assert!(!matches(
            &e,
            &event("mallory@evil.example", at.clone(), title, place)
        ));
        assert!(!matches(
            &e,
            &event(
                "ana@example.com",
                json!({"dateTime": "2026-11-10T11:00:00+08:00"}),
                title,
                place
            )
        ));
        assert!(!matches(
            &e,
            &event(
                "ana@example.com",
                json!({"date": "2026-11-10"}),
                title,
                place
            )
        ));
        // a co-attendee's forgery: the UID, organiser and start, other words
        assert!(!matches(
            &e,
            &event(
                "ana@example.com",
                at.clone(),
                "Roof inspection, moved",
                place
            )
        ));
        assert!(!matches(
            &e,
            &event("ana@example.com", at.clone(), title, "Another street")
        ));
        // Calendar's words are held as the invite's are
        assert!(matches(
            &e,
            &event("ana@example.com", at, "  Roof\ninspection ", place)
        ));
        let all_day = read(APPLE).unwrap().json("m1").unwrap();
        assert!(matches(
            &all_day,
            &event(
                "cara@example.com",
                json!({"date": "2026-11-24"}),
                "Family day \\o/",
                "Lola's house Bacolod"
            )
        ));
        // an invite naming no organiser can't be checked, so it answers nothing
        let mut nobody = e.clone();
        nobody["organizer_address"] = json!("");
        assert!(!matches(
            &nobody,
            &event(
                "",
                json!({"dateTime": "2026-11-10T02:00:00Z"}),
                title,
                place
            )
        ));
    }

    #[test]
    fn a_duration_with_no_length_is_no_invite() {
        for empty in ["P", "PT", "+P"] {
            let ics = GOOGLE.replace("DTEND:20261110T030000Z", &format!("DURATION:{empty}"));
            assert_eq!(read(&ics), None, "{empty}");
        }
        // nor does one of no length at all
        for zero in ["P0D", "PT0S", "P0W", "PT0H0M"] {
            let ics = GOOGLE.replace("DTEND:20261110T030000Z", &format!("DURATION:{zero}"));
            assert_eq!(read(&ics), None, "{zero}");
        }
        let ics = GOOGLE.replace("DTEND:20261110T030000Z", "DURATION:PT1S");
        assert!(read(&ics).is_some());
    }

    /// CLDR's windowsZones.xml, its world ("001") rows: every Windows name
    /// it knows reads, on the same clock as the zone CLDR gives, whichever of
    /// its names (Asia/Calcutta, Asia/Kolkata) either side uses.
    #[test]
    fn every_windows_name_cldr_knows_keeps_cldrs_clock() {
        use chrono::Offset as _;
        let xml = include_str!("../tests/data/windowsZones.xml");
        let attr = |line: &str, key: &str| -> String {
            let at = line.find(&format!("{key}=\"")).unwrap() + key.len() + 2;
            line[at..].split('"').next().unwrap().to_owned()
        };
        let rows: Vec<(String, String)> = xml
            .lines()
            .filter(|l| l.contains("<mapZone") && l.contains("territory=\"001\""))
            .map(|l| (attr(l, "other"), attr(l, "type")))
            .collect();
        assert!(rows.len() > 130, "{}", rows.len());
        // the first of each month, either side of every change in recent years
        let instants: Vec<NaiveDateTime> = (2018..=2027)
            .flat_map(|y| (1..=12).map(move |m| (y, m)))
            .map(|(y, m)| {
                NaiveDate::from_ymd_opt(y, m, 1)
                    .unwrap()
                    .and_hms_opt(12, 0, 0)
                    .unwrap()
            })
            .collect();
        let mut wrong = Vec::new();
        for (name, cldr) in &rows {
            let Some(ours) = windows_zone(name) else {
                wrong.push(format!("{name}: missing"));
                continue;
            };
            let (ours, theirs) = (ours.parse::<Tz>().unwrap(), cldr.parse::<Tz>().unwrap());
            let differs = instants.iter().find(|at| {
                ours.offset_from_utc_datetime(at).fix() != theirs.offset_from_utc_datetime(at).fix()
            });
            if let Some(at) = differs {
                wrong.push(format!("{name}: {ours} is not {theirs} at {at}"));
            }
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
    }

    #[test]
    fn every_windows_zone_names_a_zone_and_the_rarer_ones_read() {
        let names = include_str!("invite.rs")
            .lines()
            .skip_while(|l| !l.starts_with("fn windows_zone"))
            .take_while(|l| !l.starts_with('}'))
            .filter(|l| l.contains(" => \""))
            .flat_map(|l| l.split(" => ").next().unwrap().split(" | "))
            .map(|n| n.trim().trim_matches('"').to_owned())
            .collect::<Vec<_>>();
        assert!(names.len() > 130, "{}", names.len());
        for name in &names {
            let iana = windows_zone(name).unwrap_or_else(|| panic!("{name}"));
            assert!(iana.parse::<Tz>().is_ok(), "{name}: {iana}");
        }
        // 09:30 in Hobart in December is daylight time, 22:30 UTC the day before
        let ics = OUTLOOK.replace("Pacific Standard Time", "Tasmania Standard Time");
        assert_eq!(read(&ics).unwrap().start, at("2026-12-02T22:30:00Z"));
        let ics = OUTLOOK.replace("Pacific Standard Time", "Nepal Standard Time");
        assert_eq!(read(&ics).unwrap().start, at("2026-12-03T03:45:00Z"));
    }

    #[test]
    fn a_parameters_carets_read_left_to_right() {
        assert_eq!(param_text("a^^n"), "a^n");
        assert_eq!(param_text("^'Ana^' ^^ ^n^x"), "\"Ana\" ^  ^x");
        let ics = GOOGLE.replace("ORGANIZER;CN=Ana Reyes", "ORGANIZER;CN=Ana ^^n Reyes");
        assert_eq!(read(&ics).unwrap().organizer.name, "Ana ^n Reyes");
    }

    #[test]
    fn an_invitation_that_says_it_is_cancelled_is_a_cancel() {
        let ics = GOOGLE.replace("STATUS:CONFIRMED", "STATUS:CANCELLED");
        assert_eq!(read(&ics).unwrap().method, Method::Cancel);
        assert_eq!(read(GOOGLE).unwrap().method, Method::Request);
    }

    #[test]
    fn the_shared_view_of_a_thread_has_no_invite_in_it() {
        // only `thread()` reads one, so pictures' views don't say "none"
        let view = crate::mail::view_of(
            &json!({"messages": [message("m1", json!([part("text/calendar", GOOGLE)]))]}),
        );
        assert_eq!(view.get("invite"), None);
    }

    #[test]
    fn the_letters_text_calendar_part_wins_over_the_file() {
        let file = OUTLOOK;
        let m = message(
            "m1",
            json!([part("application/ics", file), {"mimeType": "multipart/alternative", "parts": [{"mimeType": "text/plain", "body": {"data": "aGk"}}, part("TEXT/CALENDAR", GOOGLE)]}]),
        );
        let v = of(&[m]);
        assert_eq!(v["title"], "Roof inspection");
        assert_eq!(v["message"], "m1");
        assert_eq!(v["method"], "request");
        assert_eq!(v["start"], "2026-11-10T02:00:00Z");
        assert_eq!(v["all_day"], false);
        // with no letter part, the file
        let only_file = message("m2", json!([part("application/ics", file)]));
        assert_eq!(of(&[only_file])["title"], "Budget review; Q4");
    }

    #[test]
    fn the_newest_invite_in_a_thread_is_the_one_shown() {
        let first = message("m1", json!([part("text/calendar", GOOGLE)]));
        let cancel = GOOGLE.replace("METHOD:REQUEST", "METHOD:CANCEL");
        let then = message("m2", json!([part("text/calendar", &cancel)]));
        let plain = message(
            "m3",
            json!([{"mimeType": "text/plain", "body": {"data": "aGk"}}]),
        );
        let v = of(&[first, then, plain]);
        assert_eq!(
            (v["message"].as_str(), v["method"].as_str()),
            (Some("m2"), Some("cancel"))
        );
        assert_eq!(v["sender_ok"], false, "no Authentication-Results");
    }

    #[test]
    fn the_newest_invite_that_reads_wins_over_a_newer_one_that_doesnt() {
        let first = message("m1", json!([part("text/calendar", GOOGLE)]));
        let broken = message("m2", json!([part("text/calendar", "BEGIN:VCALENDAR\r\n")]));
        let silent_reply = message(
            "m3",
            json!([part(
                "text/calendar",
                &GOOGLE
                    .replace("METHOD:REQUEST", "METHOD:REPLY")
                    .replace("PARTSTAT=ACCEPTED", "PARTSTAT=NEEDS-ACTION"),
            )]),
        );
        let v = of(&[first.clone(), broken.clone(), silent_reply]);
        assert_eq!(v["message"], "m1", "{v}");
        // one whose bytes haven't come may be the newest that reads: not yet
        let unfetched = message(
            "m4",
            json!([{"mimeType": "text/calendar", "body": {"attachmentId": "a1", "size": 900}}]),
        );
        assert_eq!(of(&[first, broken, unfetched]), pending());
    }

    fn signed(from: &str, results: &str) -> Value {
        json!({"id": "m1", "payload": {"headers": [{"name": "Authentication-Results", "value": results}, {"name": "From", "value": from}]}})
    }

    #[test]
    fn only_a_vouched_for_sender_of_the_organisers_domain_or_google_calendar_sends_an_invite() {
        let pass =
            |d: &str| format!("mx.google.com; dkim=pass header.i=@{d}; dmarc=pass header.from={d}");
        let fail = "mx.google.com; spf=softfail smtp.mailfrom=x; dmarc=fail header.from=google.com";
        let cases = [
            (
                "Ana <calendar-notification@google.com>",
                pass("google.com"),
                true,
            ),
            ("Ana <ana@example.com>", pass("example.com"), true),
            ("Ana <ana@mail.example.com>", pass("mail.example.com"), true),
            (
                "Ana <calendar-notification@google.com>",
                fail.to_owned(),
                false,
            ),
            ("Mallory <m@evil.example>", pass("evil.example"), false),
            ("Mallory <m@notexample.com>", pass("notexample.com"), false),
            // a person at Google is not Google Calendar
            ("Someone <someone@google.com>", pass("google.com"), false),
        ];
        for (from, results, ok) in cases {
            assert_eq!(
                sent_by_organizer(&signed(from, &results), "ana@example.com"),
                ok,
                "{from} {results}"
            );
        }
        // Gmail's verdict is the top one: one further down is the sender's own
        let planted = json!({"id": "m1", "payload": {"headers": [{"name": "Authentication-Results", "value": fail}, {"name": "Authentication-Results", "value": pass("google.com")}, {"name": "From", "value": "calendar-notification@google.com"}]}});
        assert!(!sent_by_organizer(&planted, "ana@example.com"));
        // no organiser to align with
        assert!(!sent_by_organizer(
            &signed("ana@example.com", &pass("example.com")),
            ""
        ));
        // at a public mail domain, only the organiser's own address
        let gmail = pass("gmail.com");
        assert!(!sent_by_organizer(
            &signed("Stranger <stranger@gmail.com>", &gmail),
            "ana@gmail.com"
        ));
        assert!(sent_by_organizer(
            &signed("Ana <Ana@Gmail.com>", &gmail),
            "ana@gmail.com"
        ));
        // Google Calendar's sender, whoever organises
        for organizer in ["ana@gmail.com", "ana@example.com", "boss@outlook.com"] {
            assert!(
                sent_by_organizer(
                    &signed("calendar-notification@google.com", &pass("google.com")),
                    organizer
                ),
                "{organizer}"
            );
        }
    }

    #[test]
    fn a_mail_with_no_invite_or_a_broken_one_has_none() {
        assert_eq!(of(&[]), Value::Null);
        let plain = message(
            "m1",
            json!([{"mimeType": "text/plain", "body": {"data": "aGk"}}]),
        );
        assert_eq!(of(&[plain]), Value::Null);
        let broken = message("m1", json!([part("text/calendar", "BEGIN:VCALENDAR\r\n")]));
        assert_eq!(of(&[broken]), Value::Null);
        // bytes not yet brought in: not none, not read yet
        let unfetched = message(
            "m1",
            json!([{"mimeType": "text/calendar", "body": {"attachmentId": "a1", "size": 900}}]),
        );
        assert_eq!(of(&[unfetched]), pending());
        let huge = message(
            "m1",
            json!([{"mimeType": "text/calendar", "body": {"data": "A".repeat(ICS_MAX * 2)}}]),
        );
        assert_eq!(of(&[huge]), Value::Null);
    }

    #[test]
    fn a_part_buried_past_any_real_mail_is_not_looked_for() {
        let mut p = part("text/calendar", GOOGLE);
        for _ in 0..DEPTH_MAX + 2 {
            p = json!({"mimeType": "multipart/mixed", "parts": [p]});
        }
        assert_eq!(of(&[json!({"id": "m1", "payload": p})]), Value::Null);
    }

    /// The screen's calls are in Mail's manifest entry, or the Bot refuses them.
    #[test]
    fn mail_lists_the_tools_the_invite_card_calls() {
        let screen = include_str!("../../gws-screens/src/mail/invite.rs");
        let mail = crate::manifest::APPS
            .iter()
            .find(|a| a.tools.contains(&crate::tools::MAIL_LIST))
            .unwrap();
        for name in [MAIL_INVITE, MAIL_INVITE_REPLY] {
            assert!(screen.contains(&format!("\"{name}\"")), "{name}");
            assert!(mail.tools.contains(&name), "{name}");
        }
    }

    #[test]
    fn both_tools_are_the_screens_and_the_reply_is_a_write() {
        for (name, read_only) in [(MAIL_INVITE, true), (MAIL_INVITE_REPLY, false)] {
            let def = crate::tools::defs()
                .into_iter()
                .find(|d| d.name == name)
                .unwrap();
            assert_eq!(def.audience, crate::tools::APP_ONLY, "{name}");
            assert_eq!(def.read_only, read_only, "{name}");
        }
    }
}
