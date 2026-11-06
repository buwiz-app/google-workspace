//! Date maths, all pure. Everything that depends on where the person is takes
//! the zone as an argument, so a test can run it in any zone: the screen passes
//! `chrono::Local`.
//!
//! An all-day event is a date, not a moment: it never goes through a zone, and
//! its end is Google's exclusive end. The time grid is a wall clock, so on a day
//! the clocks change it still has 24 rows and an event sits at the time it reads.

use chrono::{
    DateTime, Datelike, Days, Duration, FixedOffset, Months, NaiveDate, NaiveDateTime, NaiveTime,
    TimeZone, Weekday,
};

/// What the main area shows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Span {
    Day,
    Week,
    Month,
    Agenda,
}

impl Span {
    /// The name a `view` prop uses for this.
    pub fn of(name: &str) -> Option<Span> {
        match name {
            "day" => Some(Span::Day),
            "week" => Some(Span::Week),
            "month" => Some(Span::Month),
            "agenda" => Some(Span::Agenda),
            _ => None,
        }
    }
}

/// How many days the list reads at a time.
pub const AGENDA_DAYS: u64 = 30;

/// When an event happens, as the tools wrote it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum When {
    /// `end` is exclusive: the first day it's over.
    AllDay { first: NaiveDate, end: NaiveDate },
    Timed {
        start: DateTime<FixedOffset>,
        end: DateTime<FixedOffset>,
    },
}

impl When {
    /// Reads a row's `start` and `end`. An all-day event with an end that
    /// isn't after its start still takes up its one day.
    pub fn of(start: &str, end: &str, all_day: bool) -> Option<When> {
        if all_day {
            let first = parse_date(start)?;
            let end = parse_date(end)
                .filter(|e| *e > first)
                .unwrap_or_else(|| next_day(first));
            return Some(When::AllDay { first, end });
        }
        let start = DateTime::parse_from_rfc3339(start).ok()?;
        let end = DateTime::parse_from_rfc3339(end).ok().unwrap_or(start);
        Some(When::Timed { start, end })
    }

    /// Whether any of it falls on `day`, in `zone`. A timed event that ends at
    /// midnight is over before the next day starts.
    pub fn touches<Z: TimeZone>(&self, day: NaiveDate, zone: &Z) -> bool {
        match *self {
            When::AllDay { first, end } => first <= day && day < end,
            When::Timed { .. } => slice(self, day, zone).is_some(),
        }
    }

    /// Whether it sits in the all-day row: an all-day event, or a timed one
    /// that runs across a whole day and more, as a calendar shows it.
    pub fn in_all_day_row<Z: TimeZone>(&self, zone: &Z) -> bool {
        match *self {
            When::AllDay { .. } => true,
            When::Timed { start, end } => {
                let (start, end) = (wall(&start, zone), wall(&end, zone));
                end - start >= Duration::hours(24)
            }
        }
    }

    /// The moment it starts, for ordering: an all-day event at its day's start.
    pub fn starts<Z: TimeZone>(&self, zone: &Z) -> NaiveDateTime {
        match self {
            When::AllDay { first, .. } => first.and_time(NaiveTime::MIN),
            When::Timed { start, .. } => wall(start, zone),
        }
    }
}

fn wall<Z: TimeZone>(at: &DateTime<FixedOffset>, zone: &Z) -> NaiveDateTime {
    at.with_timezone(zone).naive_local()
}

pub fn parse_date(text: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(text.trim(), "%Y-%m-%d").ok()
}

pub fn parse_time(text: &str) -> Option<NaiveTime> {
    NaiveTime::parse_from_str(text.trim(), "%H:%M").ok()
}

/// The day after, and the same day at the end of time rather than a panic.
fn next_day(day: NaiveDate) -> NaiveDate {
    day.succ_opt().unwrap_or(day)
}

/// The last day an all-day event covers, for a person to read and edit.
pub fn last_day(end_exclusive: NaiveDate) -> NaiveDate {
    end_exclusive.pred_opt().unwrap_or(end_exclusive)
}

/// What Google wants as the end of an all-day event whose last day is `last`.
pub fn exclusive_end(last: NaiveDate) -> NaiveDate {
    next_day(last)
}

/// The Sunday that starts `day`'s week.
pub fn week_start(day: NaiveDate) -> NaiveDate {
    day.week(Weekday::Sun).first_day()
}

fn first_of_month(day: NaiveDate) -> NaiveDate {
    day.with_day(1).unwrap_or(day)
}

/// The days a month's grid shows, in whole weeks.
pub fn month_days(day: NaiveDate) -> Vec<NaiveDate> {
    let first = first_of_month(day);
    let next = first + Months::new(1);
    let start = week_start(first);
    let weeks = ((next - start).num_days() + 6) / 7;
    start
        .iter_days()
        .take(usize::try_from(weeks * 7).unwrap_or(0))
        .collect()
}

/// The days `span` shows around `anchor`: the first, and the first it doesn't.
pub fn range(span: Span, anchor: NaiveDate) -> (NaiveDate, NaiveDate) {
    match span {
        Span::Day => (anchor, next_day(anchor)),
        Span::Week => {
            let first = week_start(anchor);
            (first, first + Days::new(7))
        }
        Span::Month => {
            let days = month_days(anchor);
            let first = days.first().copied().unwrap_or(anchor);
            let last = days.last().copied().unwrap_or(anchor);
            (first, next_day(last))
        }
        Span::Agenda => (anchor, anchor + Days::new(AGENDA_DAYS)),
    }
}

/// Where Previous or Next goes. A month keeps its day where it can: Jan 31 goes
/// to the last of February, not into March.
pub fn step(span: Span, anchor: NaiveDate, forward: bool) -> NaiveDate {
    let days = |n| {
        if forward {
            anchor + Days::new(n)
        } else {
            anchor - Days::new(n)
        }
    };
    match span {
        Span::Day => days(1),
        Span::Week => days(7),
        Span::Agenda => days(AGENDA_DAYS),
        Span::Month if forward => anchor + Months::new(1),
        Span::Month => anchor - Months::new(1),
    }
}

/// The moment `day` starts in `zone`. A few zones change their clocks at
/// midnight, so a day can start at 01:00.
pub fn day_start<Z: TimeZone>(day: NaiveDate, zone: &Z) -> Option<DateTime<Z>> {
    (0..=3).find_map(|hour| {
        let at = day.and_hms_opt(hour, 0, 0)?;
        zone.from_local_datetime(&at).earliest()
    })
}

/// `gws_cal_range`'s `from` and `to` for the days from `first` up to `end`.
pub fn range_args<Z: TimeZone>(
    first: NaiveDate,
    end: NaiveDate,
    zone: &Z,
) -> Option<(String, String)>
where
    Z::Offset: std::fmt::Display,
{
    Some((
        day_start(first, zone)?.to_rfc3339(),
        day_start(end, zone)?.to_rfc3339(),
    ))
}

/// The part of a timed event that falls on `day`, as minutes on its wall clock,
/// from 0 up to 1440. An all-day event has none.
pub fn slice<Z: TimeZone>(when: &When, day: NaiveDate, zone: &Z) -> Option<(u32, u32)> {
    let When::Timed { start, end } = when else {
        return None;
    };
    let (start, end) = (wall(start, zone), wall(end, zone));
    let midnight = day.and_time(NaiveTime::MIN);
    let next = next_day(day).and_time(NaiveTime::MIN);
    // the clocks going back can put a short event's end before its start
    let end = end.max(start);
    let empty = start == end;
    if start >= next || end < midnight || (end == midnight && !empty) {
        return None;
    }
    let minutes = |at: NaiveDateTime| (at - midnight).num_minutes().clamp(0, 1440) as u32;
    Some((minutes(start.max(midnight)), minutes(end.min(next))))
}

/// Which lane each of `spans` takes, and how many lanes its group shares, so
/// events that overlap sit side by side. `spans` are `(from, to)` minutes.
/// A group is everything joined by overlaps, so its events share one width.
pub fn lanes(spans: &[(u32, u32)]) -> Vec<(usize, usize)> {
    let mut order: Vec<usize> = (0..spans.len()).collect();
    order.sort_by_key(|&i| (spans[i].0, std::cmp::Reverse(spans[i].1)));
    let mut out = vec![(0, 1); spans.len()];
    let mut group: Vec<usize> = Vec::new();
    // when each lane of the group comes free
    let mut free: Vec<u32> = Vec::new();
    let close = |group: &mut Vec<usize>, free: &mut Vec<u32>, out: &mut Vec<(usize, usize)>| {
        for &i in group.iter() {
            out[i].1 = free.len();
        }
        group.clear();
        free.clear();
    };
    for i in order {
        let (from, to) = spans[i];
        if free.iter().all(|end| *end <= from) {
            close(&mut group, &mut free, &mut out);
        }
        let lane = match free.iter().position(|end| *end <= from) {
            Some(lane) => {
                free[lane] = to;
                lane
            }
            None => {
                free.push(to);
                free.len() - 1
            }
        };
        out[i].0 = lane;
        group.push(i);
    }
    close(&mut group, &mut free, &mut out);
    out
}

/// A time typed on `day` as the moment it is in `zone`, for the tools. `None` is
/// a time the clocks skip that day. A time they repeat means the first one.
pub fn moment<Z: TimeZone>(day: NaiveDate, time: NaiveTime, zone: &Z) -> Option<String>
where
    Z::Offset: std::fmt::Display,
{
    zone.from_local_datetime(&day.and_time(time))
        .earliest()
        .map(|at| at.to_rfc3339())
}

fn clock<Z: TimeZone>(at: &DateTime<FixedOffset>, zone: &Z) -> String {
    wall(at, zone).format("%H:%M").to_string()
}

/// A row's date and time for a person: "Fri Nov 6, 12:00 - 13:00", in `zone`.
pub fn when_line<Z: TimeZone>(when: &When, zone: &Z) -> String {
    let date = |day: NaiveDate| day.format("%a %b %-d").to_string();
    match when {
        When::AllDay { first, end } => {
            let last = last_day(*end);
            if last == *first {
                format!("{}, all day", date(*first))
            } else {
                format!("{} - {}, all day", date(*first), date(last))
            }
        }
        When::Timed { start, end } => {
            let (from, to) = (wall(start, zone), wall(end, zone));
            if from.date() == to.date() {
                format!(
                    "{}, {} - {}",
                    date(from.date()),
                    clock(start, zone),
                    clock(end, zone)
                )
            } else {
                format!(
                    "{}, {} - {}, {}",
                    date(from.date()),
                    clock(start, zone),
                    date(to.date()),
                    clock(end, zone)
                )
            }
        }
    }
}

/// The start of a row's time, short, for the grid: "09:30", or "All day".
pub fn start_line<Z: TimeZone>(when: &When, zone: &Z) -> String {
    match when {
        When::AllDay { .. } => "All day".to_owned(),
        When::Timed { start, .. } => clock(start, zone),
    }
}

/// The header's date range.
pub fn range_title(span: Span, anchor: NaiveDate) -> String {
    match span {
        Span::Day => anchor.format("%a %b %-d, %Y").to_string(),
        Span::Month => anchor.format("%B %Y").to_string(),
        Span::Week => {
            let (first, end) = range(Span::Week, anchor);
            let last = last_day(end);
            if first.year() != last.year() {
                format!(
                    "{} - {}",
                    first.format("%b %-d, %Y"),
                    last.format("%b %-d, %Y")
                )
            } else if first.month() != last.month() {
                format!(
                    "{} - {}, {}",
                    first.format("%b %-d"),
                    last.format("%b %-d"),
                    first.year()
                )
            } else {
                format!("{}-{}", first.format("%b %-d"), last.format("%-d, %Y"))
            }
        }
        Span::Agenda => format!("From {}", anchor.format("%b %-d, %Y")),
    }
}

/// Where a new event starts and ends: the next whole hour when `anchor` is
/// `now`'s day, else nine o'clock on `anchor`, for an hour.
pub fn new_slot(anchor: NaiveDate, now: NaiveDateTime) -> (NaiveDateTime, NaiveDateTime) {
    let start = if anchor == now.date() {
        let hour = now.date().and_hms_opt(chrono::Timelike::hour(&now), 0, 0);
        hour.map_or(now, |h| h + Duration::hours(1))
    } else {
        anchor.and_hms_opt(9, 0, 0).unwrap_or(now)
    };
    (start, start + Duration::hours(1))
}
