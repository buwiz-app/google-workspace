//! The Calendar screens driven as a person drives them, on a headless engine: no
//! window and no plugin. A fake host answers the tool contract from a small
//! store of its own, can hold an answer back, and refuses the AI's press the way
//! the Bot's bridge does: a failure, at the call.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use buwiz_app::{Bridge, BuwizError, Done, Host, Listener};
use chrono::{DateTime, Days, Local, NaiveDate, TimeZone as _};
use chrono_tz::America::{Havana, New_York};
use chrono_tz::Asia::Manila;
use gws_screens::calendar::time::{self, Span, When};
use nsh_core::prelude::*;
use nsh_engine::EngineConfig;
use nsh_testing::Headless;
use serde_json::{Map, Value, json};

mod bot_filter;

use bot_filter::Filter;

const WAIT: Duration = Duration::from_secs(5);
/// What the Bot says, word for word, when a press came from the AI.
const AI_REFUSED: &str = "Only a person can do this in Google Workspace, and this press came from the AI, so nothing was sent. To go ahead, press it yourself.";
const LONG_TITLE: &str = "A very long title for a planning session that goes on and on without any sign of stopping before the edge of the card or of anything else near it";

fn today() -> NaiveDate {
    Local::now().date_naive()
}

/// A moment `days` from today, at `hour:minute` on this Mac's clock.
fn at(days: i64, hour: u32, minute: u32) -> String {
    let day = today() + chrono::Duration::days(days);
    Local
        .from_local_datetime(&day.and_hms_opt(hour, minute, 0).unwrap())
        .earliest()
        .unwrap()
        .to_rfc3339()
}

fn event(id: &str, calendar: &str, title: &str, start: String, end: String) -> Value {
    json!({
        "id": id, "calendar": calendar, "title": title, "start": start, "end": end,
        "all_day": false, "where": "", "status": "confirmed", "response": "none",
        "guests": 0, "meet": false, "recurring": false,
        "notes": "", "organizer": "owner@example.com", "attendees": [], "meet_url": "",
        "repeat": "", "series": "", "time_zone": "Asia/Manila", "can_edit": true, "etag": "e1",
    })
}

fn all_day(id: &str, title: &str, first: NaiveDate, end: NaiveDate) -> Value {
    let mut e = event(id, "mine", title, first.to_string(), end.to_string());
    e["all_day"] = json!(true);
    e
}

fn guest(email: &str, response: &str) -> Value {
    json!({"email": email, "name": "", "response": response, "optional": false, "organizer": false, "self": false})
}

/// A day with what the screens need to show: overlapping events, a place, guests,
/// notes and a link, and an all-day event.
fn a_day() -> Vec<Value> {
    let mut review = event("e1", "mine", "Team review", at(0, 9, 0), at(0, 10, 0));
    review["where"] = json!("Room Seven");
    review["guests"] = json!(2);
    review["meet"] = json!(true);
    review["meet_url"] = json!("https://meet.example.com/abc-defg");
    review["notes"] = json!("Bring the numbers.");
    review["attendees"] = json!([
        guest("ana@example.com", "accepted"),
        guest("ben@example.com", "declined")
    ]);
    let dentist = event("e2", "work", "Dentist", at(0, 9, 30), at(0, 10, 30));
    let offsite = all_day("e3", "Offsite", today(), today() + Days::new(1));
    vec![review, dentist, offsite]
}

fn invite(mut e: Value) -> Value {
    e["response"] = json!("needsAction");
    e["can_edit"] = json!(false);
    e
}

fn in_series(mut e: Value) -> Value {
    e["recurring"] = json!(true);
    e["series"] = json!("base1");
    e["repeat"] = json!("RRULE:FREQ=WEEKLY");
    e
}

fn when_of(e: &Value) -> Option<When> {
    When::of(
        e["start"].as_str()?,
        e["end"].as_str()?,
        e["all_day"].as_bool()?,
    )
}

#[derive(Default)]
struct Fake {
    /// In place of the three calendars.
    calendars: Mutex<Option<Value>>,
    props: Mutex<Value>,
    inline: Mutex<bool>,
    status: Mutex<Option<Value>>,
    hidden: Mutex<BTreeSet<String>>,
    events: Mutex<Vec<Value>>,
    /// A draft's fields.
    draft: Mutex<Value>,
    /// The next event, and the two after it.
    next: Mutex<Value>,
    seq: Mutex<i64>,
    /// The press is the AI's: the bridge fails these calls with this.
    ai: Mutex<bool>,
    /// Writes answer that they're busy, and all reads say offline.
    busy: Mutex<bool>,
    offline: Mutex<bool>,
    /// Google's sign-in went: reads fail as the plugin says so.
    signed_out: Mutex<bool>,
    /// A draft read is refused, as an old ticket is.
    draft_gone: Mutex<bool>,
    /// A draft card's ticket stands for nothing, until its fields come with it.
    draft_lost: Mutex<bool>,
    /// A save is refused: someone else's version of the event, which goes in.
    changed: Mutex<Option<Value>>,
    calls: Mutex<Vec<(String, Value)>>,
    /// Other things the screen asked of the host.
    host: Mutex<Vec<(String, Value)>>,
    hold: Mutex<Vec<String>>,
    held: Mutex<Vec<(String, Value, Done)>>,
}

const WRITES: [&str; 4] = [
    "gws_event_save",
    "gws_event_delete",
    "gws_event_rsvp",
    "gws_event_restore",
];

impl Fake {
    fn called(&self, tool: &str) -> Vec<Value> {
        let calls = self.calls.lock().unwrap();
        calls
            .iter()
            .filter(|(t, _)| t == tool)
            .map(|(_, a)| a.clone())
            .collect()
    }

    fn writes(&self) -> usize {
        WRITES.iter().map(|t| self.called(t).len()).sum()
    }

    fn hold(&self, name: &str) {
        self.hold.lock().unwrap().push(name.to_owned());
    }

    fn release(&self, name: &str) {
        let mut held = self.held.lock().unwrap();
        let at = held
            .iter()
            .position(|(n, _, _)| n == name)
            .expect("that call is held");
        let (_, answer, done) = held.remove(at);
        drop(held);
        done(Ok(answer));
    }

    fn row(&self, id: &str) -> Option<Value> {
        let events = self.events.lock().unwrap();
        events.iter().find(|e| e["id"] == id).cloned()
    }

    /// `gws_cal_range`: what's in the days asked for, over the calendars shown.
    fn range(&self, args: &Value) -> (String, Value) {
        let moment = |k: &str| DateTime::parse_from_rfc3339(args[k].as_str().unwrap()).unwrap();
        let (from, to) = (moment("from"), moment("to"));
        let hidden = self.hidden.lock().unwrap();
        let events = self.events.lock().unwrap();
        let mut rows: Vec<Value> = events
            .iter()
            .filter(|e| !hidden.contains(e["calendar"].as_str().unwrap()))
            .filter(|e| {
                let day = |d: NaiveDate| {
                    Local
                        .from_local_datetime(&d.and_hms_opt(0, 0, 0).unwrap())
                        .earliest()
                        .unwrap()
                };
                match when_of(e) {
                    Some(When::AllDay { first, end }) => {
                        day(first).fixed_offset() < to && day(end).fixed_offset() > from
                    }
                    Some(When::Timed { start, end }) => start < to && end > from,
                    None => false,
                }
            })
            .cloned()
            .collect();
        rows.sort_by_key(|e| when_of(e).map(|w| w.starts(&Local)));
        let name = match (to - from).num_days() {
            1 => "day",
            7 => "week",
            30 => "agenda",
            _ => "month",
        };
        let saved = *self.offline.lock().unwrap();
        (name.to_owned(), json!({"events": rows, "saved": saved}))
    }

    fn save(&self, args: &Value) -> Value {
        if let Some(newer) = self.changed.lock().unwrap().take() {
            let mut events = self.events.lock().unwrap();
            events.retain(|e| e["id"] != newer["id"]);
            events.push(newer);
            return json!({"refused": "changed", "message": "Precondition failed."});
        }
        let mut events = self.events.lock().unwrap();
        let id = args["id"]
            .as_str()
            .map_or(format!("new{}", events.len()), str::to_owned);
        let mut row = event(
            &id,
            args["calendar"].as_str().unwrap(),
            args["title"].as_str().unwrap(),
            args["start"].as_str().unwrap().to_owned(),
            args["end"].as_str().unwrap().to_owned(),
        );
        row["all_day"] = args["all_day"].clone();
        row["where"] = args["where"].clone();
        row["guests"] = json!(args["guests"].as_array().map_or(0, Vec::len));
        events.retain(|e| e["id"] != id);
        events.push(row.clone());
        json!({"event": row})
    }
}

impl Host for Fake {
    fn params(&self) -> Map<String, Value> {
        let placement = if *self.inline.lock().unwrap() {
            "inline"
        } else {
            "full"
        };
        let props = self.props.lock().unwrap().clone();
        let props = if props.is_null() { json!({}) } else { props };
        let p = json!({
            "props": props, "app": {"plugin": "app.buwiz.google-workspace", "app": "calendar"},
            "granted": ["buwiz.call", "buwiz.open"],
            "viewport": {"width": 1000, "height": 600, "placement": placement},
        });
        p.as_object().unwrap().clone()
    }

    fn post(&self, _: &str, _: Value) {}

    fn call(&self, method: &str, data: Value, done: Done) {
        if method != "buwiz.call" {
            self.host.lock().unwrap().push((method.to_owned(), data));
            return done(Ok(Value::Null));
        }
        let (tool, args) = (data["tool"].as_str().unwrap_or_default(), &data["args"]);
        self.calls
            .lock()
            .unwrap()
            .push((tool.to_owned(), args.clone()));
        if WRITES.contains(&tool) && *self.ai.lock().unwrap() {
            return done(Err(BuwizError::new("permission_denied", false, AI_REFUSED)));
        }
        if WRITES.contains(&tool) && *self.busy.lock().unwrap() {
            return done(Err(BuwizError::new("busy", true, "busy")));
        }
        if tool == "gws_cal_range" && *self.signed_out.lock().unwrap() {
            return done(Err(BuwizError::new(
                "tool_error",
                false,
                "Sign in to Google first.",
            )));
        }
        let (name, answer) = match tool {
            "gws_status" => (
                String::new(),
                self.status.lock().unwrap().clone().unwrap_or_else(
                    || json!({"signed_in": true, "features": {"mail": true, "calendar": true, "drive": false}}),
                ),
            ),
            "gws_cal_list" if self.calendars.lock().unwrap().is_some() => (
                String::new(),
                self.calendars.lock().unwrap().clone().unwrap(),
            ),
            "gws_cal_list" => {
                let hidden = self.hidden.lock().unwrap();
                let list = |id: &str, name: &str, primary, writable| json!({"id": id, "name": name, "primary": primary, "writable": writable, "shown": !hidden.contains(id), "time_zone": "Asia/Manila"});
                (
                    String::new(),
                    json!({"calendars": [list("mine", "Mine", true, true), list("work", "Work", false, true), list("feed", "Holidays", false, false)]}),
                )
            }
            "gws_cal_show" => {
                let id = args["calendar"].as_str().unwrap().to_owned();
                let mut hidden = self.hidden.lock().unwrap();
                if args["shown"] == true {
                    hidden.remove(&id);
                } else {
                    hidden.insert(id);
                }
                (tool.to_owned(), json!({"ok": true}))
            }
            "gws_cal_range" => self.range(args),
            "gws_cal_changes" => (String::new(), json!({"seq": *self.seq.lock().unwrap()})),
            "gws_event_read" => {
                let id = args["id"].as_str().unwrap_or_default();
                (
                    id.to_owned(),
                    self.row(id).unwrap_or_else(
                        || json!({"refused": "not_found", "message": "That event is gone."}),
                    ),
                )
            }
            "gws_cal_next" => (String::new(), self.next.lock().unwrap().clone()),
            "gws_search_events" => {
                let query = args["query"].as_str().unwrap_or_default().to_lowercase();
                let events = self.events.lock().unwrap();
                let items: Vec<&Value> = events
                    .iter()
                    .filter(|e| e["title"].as_str().unwrap().to_lowercase().contains(&query))
                    .collect();
                (tool.to_owned(), json!({"items": items, "untrusted": "warning"}))
            }
            "gws_event_save" => (tool.to_owned(), self.save(args)),
            "gws_event_delete" => {
                let row = self.row(args["id"].as_str().unwrap());
                let guests = row.as_ref().map_or(0, |r| r["guests"].as_i64().unwrap());
                let mut events = self.events.lock().unwrap();
                events.retain(|e| e["id"] != args["id"]);
                (tool.to_owned(), json!({"ok": true, "undo": guests == 0}))
            }
            "gws_event_rsvp" => {
                let mut events = self.events.lock().unwrap();
                let row = events.iter_mut().find(|e| e["id"] == args["id"]);
                let row = row.expect("the event");
                row["response"] = args["response"].clone();
                (tool.to_owned(), json!({"event": row.clone()}))
            }
            "gws_event_restore" => {
                let back = event("e2", "work", "Dentist", at(0, 9, 30), at(0, 10, 30));
                self.events.lock().unwrap().push(back.clone());
                (tool.to_owned(), json!({"event": back}))
            }
            // a plugin that binds a lost ticket again to the fields sent along
            "gws_event_draft_read"
                if *self.draft_lost.lock().unwrap() && !args["fields"].is_object() =>
            {
                (
                    tool.to_owned(),
                    json!({"refused": "ticket_lost", "message": LINK_LOST}),
                )
            }
            "gws_event_draft_read" if *self.draft_gone.lock().unwrap() => (
                tool.to_owned(),
                json!({"refused": "not_found", "message": "That draft is gone."}),
            ),
            "gws_event_draft_read" => (tool.to_owned(), self.draft.lock().unwrap().clone()),
            "gws_cal_people" => (
                tool.to_owned(),
                json!({"people": [{"name": "Ana Reyes", "address": "ana@example.com"}]}),
            ),
            other => {
                return done(Err(BuwizError::new(
                    "invalid",
                    false,
                    format!("not faked: {other}"),
                )));
            }
        };
        if self.hold.lock().unwrap().contains(&name) {
            self.held.lock().unwrap().push((name, answer, done));
        } else {
            done(Ok(answer));
        }
    }

    fn listen(&self, _: Listener) {}

    fn fail(&self, message: &str) {
        panic!("the screen failed: {message}");
    }
}

fn open_with(fake: Arc<Fake>, width: f32, height: f32) -> Headless {
    let def = gws_screens::screens()
        .iter()
        .find(|s| s.id == "gws.calendar")
        .expect("Calendar");
    let root = (def.build)(Bridge::new(fake));
    let mut app = Headless::start(EngineConfig::new(root)).expect("the engine starts");
    app.set_viewport(size(px(width), px(height)))
        .expect("the viewport sets");
    app
}

fn start_with(events: Vec<Value>) -> (Headless, Arc<Fake>) {
    let fake = Arc::new(Fake::default());
    *fake.events.lock().unwrap() = events;
    let mut app = open_with(fake.clone(), 1000.0, 600.0);
    app.wait_for_within("label:Today", WAIT)
        .expect("the screen shows");
    settle(&mut app);
    (app, fake)
}

/// The now-line is drawn over the events, and takes no press: an event
/// starting this minute opens from its title, which the line lies on. The
/// grid shows 07:00 to past 16:00 as it opens, so outside those hours the
/// event is out of view and there's nothing to press.
#[test]
fn an_event_under_the_now_line_still_opens() {
    use chrono::Timelike as _;
    let now = Local::now();
    if !(7..16).contains(&now.hour()) {
        eprintln!(
            "the now-line is out of view at {}: nothing to press",
            now.format("%H:%M")
        );
        return;
    }
    let start = at(0, now.hour(), now.minute());
    let end = at(0, now.hour() + 1, now.minute());
    let (mut app, _) = start_with(vec![event("n1", "mine", "Right now", start, end)]);
    assert!(
        app.screen().matches("id:now-line").unwrap(),
        "the line shows today"
    );
    open(&mut app, "Right now");
}

fn start() -> (Headless, Arc<Fake>) {
    let (app, fake) = start_with(a_day());
    assert!(
        app.screen().shows("Team review"),
        "{:?}",
        app.screen().texts()
    );
    (app, fake)
}

fn settle(app: &mut Headless) {
    app.wait(Duration::from_millis(300)).expect("time passes");
}

fn key(app: &mut Headless, keys: &str) {
    app.press_key(keys).expect("the key presses");
    settle(app);
}

fn press(app: &mut Headless, selector: &str) {
    app.press(selector).expect("it presses");
    settle(app);
}

fn open(app: &mut Headless, title: &str) {
    press(app, &format!("text:{title}"));
    app.wait_for_within("label:Back to the calendar", WAIT)
        .expect("the event opens");
}

/// The value `field` shows.
fn field(app: &Headless, label: &str) -> String {
    app.screen()
        .input_text(&format!("label:{label}"))
        .unwrap()
        .unwrap_or_else(|| panic!("no {label} field"))
        .to_owned()
}

/// Replaces what `label` holds.
fn fill(app: &mut Headless, label: &str, text: &str) {
    let selector = format!("label:{label}");
    app.clear(&selector).expect("the field clears");
    app.type_text(&selector, text).expect("the field takes it");
}

/// A new event with this title, in the editor, ready to save.
fn write_one(app: &mut Headless, title: &str) {
    key(app, "c");
    app.wait_for_within(
        "label:Title, the name guests will see on their calendars",
        WAIT,
    )
    .expect("the editor opens");
    app.type_text(
        "label:Title, the name guests will see on their calendars",
        title,
    )
    .expect("it takes a title");
}

fn add_guest(app: &mut Headless, address: &str) {
    app.type_text("label:Guest's address, like name@company.com", address)
        .expect("it takes the address");
    app.submit("label:Guest's address, like name@company.com")
        .expect("it takes the address in");
    settle(app);
}

fn last(fake: &Fake, tool: &str) -> Value {
    fake.called(tool).last().expect("a call").clone()
}

// --- Views -------------------------------------------------------------------

#[test]
fn the_week_shows_events_all_day_and_the_now_line() {
    let (app, _) = start();
    for line in ["Team review", "Dentist", "Offsite", "All day"] {
        assert!(app.screen().shows(line), "{line}");
    }
    assert!(app.screen().matches("id:now-line").unwrap());
    let title = time::range_title(Span::Week, today());
    assert!(app.screen().shows(&title), "{title}");
}

#[test]
fn views_switch_by_press_and_by_key_and_each_asks_for_its_days() {
    let (mut app, fake) = start();
    for (keys, days, span) in [
        ("d", 1, Span::Day),
        ("m", 0, Span::Month),
        ("a", 30, Span::Agenda),
        ("w", 7, Span::Week),
    ] {
        key(&mut app, keys);
        let args = last(&fake, "gws_cal_range");
        let moment = |k: &str| DateTime::parse_from_rfc3339(args[k].as_str().unwrap()).unwrap();
        let asked = (moment("to") - moment("from")).num_days();
        assert_eq!(
            args["to_date"],
            time::range(span, today()).1.to_string(),
            "{keys} says which date it stops at"
        );
        if days == 0 {
            assert!(asked >= 28, "{keys} asked {asked} days");
        } else {
            assert_eq!(asked, days, "{keys}");
        }
        assert!(
            app.screen().shows("Team review"),
            "{keys}: {:?}",
            app.screen().texts()
        );
    }
    press(&mut app, "label:Month");
    assert!(app.screen().shows(&today().format("%B %Y").to_string()));
    press(&mut app, "label:Day");
    assert!(
        app.screen()
            .shows(&today().format("%a %b %-d, %Y").to_string())
    );
    press(&mut app, "label:List");
    assert!(
        app.screen()
            .shows(&format!("From {}", today().format("%b %-d, %Y")))
    );
}

#[test]
fn a_screen_opened_on_a_view_shows_that_view() {
    let fake = Arc::new(Fake::default());
    *fake.events.lock().unwrap() = a_day();
    *fake.props.lock().unwrap() = json!({"view": "month"});
    let mut app = open_with(fake.clone(), 1000.0, 600.0);
    app.wait_for_within("text:Team review", WAIT)
        .expect("it shows");
    assert!(app.screen().shows(&today().format("%B %Y").to_string()));
}

#[test]
fn a_screen_opened_on_an_event_opens_it() {
    let fake = Arc::new(Fake::default());
    *fake.events.lock().unwrap() = a_day();
    *fake.props.lock().unwrap() = json!({"event": {"calendar": "mine", "id": "e1"}});
    let mut app = open_with(fake.clone(), 1000.0, 600.0);
    app.wait_for_within("text:Bring the numbers.", WAIT)
        .expect("it opens");
}

#[test]
fn a_full_month_day_says_how_many_more_and_opens_them() {
    let rows: Vec<Value> = (0..7)
        .map(|n| {
            event(
                &format!("m{n}"),
                "mine",
                &format!("Meeting {n}"),
                at(0, 8 + n, 0),
                at(0, 8 + n, 30),
            )
        })
        .collect();
    let (mut app, _) = start_with(rows);
    key(&mut app, "m");
    let more = app
        .screen()
        .texts()
        .iter()
        .find(|t| t.ends_with(" more"))
        .map(|t| (*t).to_owned())
        .unwrap_or_else(|| panic!("no more line: {:?}", app.screen().texts()));
    let hidden: usize = more
        .trim_start_matches('+')
        .trim_end_matches(" more")
        .parse()
        .unwrap();
    let chips = (0..7)
        .filter(|n| app.screen().shows(&format!("Meeting {n}")))
        .count();
    assert_eq!(chips + hidden, 7, "{more}");
    assert!(chips >= 1);

    press(&mut app, &format!("text:{more}"));
    assert!(
        app.screen()
            .shows(&today().format("%a %b %-d, %Y").to_string())
    );
    assert!(app.screen().shows("Meeting 6"), "the day shows them all");
}

#[test]
fn j_k_and_t_move_the_days_shown() {
    let (mut app, fake) = start();
    key(&mut app, "j");
    let next = time::step(Span::Week, today(), true);
    assert!(app.screen().shows(&time::range_title(Span::Week, next)));
    assert!(!app.screen().shows("Team review"), "that's another week");
    key(&mut app, "k");
    key(&mut app, "k");
    assert!(app.screen().shows(&time::range_title(
        Span::Week,
        time::step(Span::Week, today(), false)
    )));
    key(&mut app, "t");
    assert!(app.screen().shows("Team review"));
    press(&mut app, "label:Next");
    assert!(app.screen().shows(&time::range_title(Span::Week, next)));
    press(&mut app, "label:Previous");
    press(&mut app, "label:Today");
    assert!(app.screen().shows("Team review"));
    assert_eq!(fake.writes(), 0);
}

#[test]
fn g_goes_to_a_date_and_says_when_it_cant() {
    let (mut app, _) = start();
    key(&mut app, "g");
    app.wait_for_within("label:Go to date", WAIT)
        .expect("it opens");
    app.type_text("label:Go to date", "someday")
        .expect("it types");
    app.submit("label:Go to date").expect("it goes");
    settle(&mut app);
    assert!(app.screen().shows("Type a date like 2026-11-06."));

    fill(&mut app, "Go to date", "2031-03-05");
    app.submit("label:Go to date").expect("it goes");
    settle(&mut app);
    let day = NaiveDate::from_ymd_opt(2031, 3, 5).unwrap();
    assert!(app.screen().shows(&time::range_title(Span::Week, day)));
    assert!(
        !app.screen().matches("label:Go to date").unwrap(),
        "the field closes"
    );
}

#[test]
fn slash_searches_and_a_view_press_leaves_the_results() {
    let (mut app, fake) = start();
    key(&mut app, "/");
    app.type_text("label:Search events", "dent")
        .expect("it types");
    app.submit("label:Search events").expect("it searches");
    settle(&mut app);
    assert_eq!(last(&fake, "gws_search_events")["query"], "dent");
    assert!(app.screen().shows("Search results"));
    assert!(app.screen().shows("Dentist"));
    assert!(!app.screen().shows("Team review"));
    press(&mut app, "label:Week");
    assert!(app.screen().shows("Team review"));
    assert!(!app.screen().shows("Search results"));
}

#[test]
fn hiding_a_calendar_takes_its_events_away_and_showing_brings_them_back() {
    let (mut app, fake) = start();
    press(&mut app, "text:Work");
    assert_eq!(
        last(&fake, "gws_cal_show"),
        json!({"calendar": "work", "shown": false})
    );
    assert!(!app.screen().shows("Dentist"));
    assert!(app.screen().shows("Team review"));
    press(&mut app, "text:Work");
    assert_eq!(
        last(&fake, "gws_cal_show"),
        json!({"calendar": "work", "shown": true})
    );
    assert!(app.screen().shows("Dentist"));
}

#[test]
fn a_store_change_shows_within_the_check() {
    let (mut app, fake) = start();
    fake.events.lock().unwrap().push(event(
        "x",
        "mine",
        "Added elsewhere",
        at(0, 15, 0),
        at(0, 16, 0),
    ));
    *fake.seq.lock().unwrap() += 1;
    app.wait_for_within("text:Added elsewhere", Duration::from_secs(20))
        .expect("the view reads it again");
}

// --- Late answers ------------------------------------------------------------

#[test]
fn a_late_view_answer_does_not_replace_a_newer_one() {
    let (mut app, fake) = start();
    fake.hold("month");
    key(&mut app, "m");
    // the month's answer is out holding these rows; the day read after it has none
    fake.events.lock().unwrap().clear();
    key(&mut app, "d");
    assert!(!app.screen().shows("Team review"), "the day read nothing");

    fake.release("month");
    settle(&mut app);
    assert!(
        !app.screen().shows("Team review"),
        "the month came late: {:?}",
        app.screen().texts()
    );
}

#[test]
fn a_late_event_answer_does_not_replace_the_open_one() {
    let (mut app, fake) = start();
    fake.hold("e1");
    press(&mut app, "text:Team review");
    key(&mut app, "escape");
    press(&mut app, "text:Dentist");
    app.wait_for_within("label:Back to the calendar", WAIT)
        .expect("it opens");
    fake.release("e1");
    settle(&mut app);
    assert!(app.screen().shows("Dentist"));
    assert!(
        !app.screen().shows("Bring the numbers."),
        "the first came late"
    );
}

#[test]
fn an_event_answer_after_leaving_opens_nothing() {
    let (mut app, fake) = start();
    fake.hold("e1");
    press(&mut app, "text:Team review");
    key(&mut app, "escape");
    fake.release("e1");
    settle(&mut app);
    assert!(!app.screen().shows("Bring the numbers."));
    assert!(app.screen().matches("label:Today").unwrap());
}

// --- The list ----------------------------------------------------------------

/// Numbers on screen, from rows titled `Row <n>`.
fn rows_shown(app: &Headless) -> BTreeSet<usize> {
    let texts = app.screen().texts();
    texts
        .iter()
        .filter_map(|t| t.strip_prefix("Row ")?.parse().ok())
        .collect()
}

fn assert_unbroken(app: &Headless, all: &[usize]) {
    let shown = rows_shown(app);
    let place = |n: &usize| {
        all.iter()
            .position(|a| a == n)
            .unwrap_or_else(|| panic!("{n} isn't in this list: {shown:?}"))
    };
    let (first, last) = (
        place(shown.first().expect("rows")),
        place(shown.last().expect("rows")),
    );
    assert_eq!(last - first + 1, shown.len(), "a gap in {shown:?}");
}

/// Two hundred events over the list's thirty days: an hour apart from 00:00.
fn many() -> Vec<Value> {
    (0..200)
        .map(|n| {
            let (day, hour) = ((n / 7) as i64, (n % 7) as u32 * 2 + 6);
            event(
                &format!("r{n}"),
                "mine",
                &format!("Row {n}"),
                at(day, hour, 0),
                at(day, hour, 30),
            )
        })
        .collect()
}

#[test]
fn rows_stay_right_through_a_scroll_and_a_view_change() {
    let (mut app, _) = start_with(many());
    let all: Vec<usize> = (0..200).collect();
    key(&mut app, "a");
    app.wait_for_within("text:Row 0", WAIT)
        .expect("the list shows");
    app.scroll("text:Row 0", px(2500.0))
        .expect("the list scrolls");
    settle(&mut app);
    assert_unbroken(&app, &all);
    assert!(!app.screen().shows("Row 0"), "scrolled past the top");

    key(&mut app, "m");
    key(&mut app, "a");
    app.wait_for_within("text:Row 0", WAIT)
        .expect("the list is back");
    assert_unbroken(&app, &all);
}

#[test]
fn a_press_after_a_scroll_opens_the_row_it_shows() {
    let (mut app, fake) = start_with(many());
    key(&mut app, "a");
    app.wait_for_within("text:Row 0", WAIT)
        .expect("the list shows");
    app.scroll("text:Row 0", px(2500.0))
        .expect("the list scrolls");
    settle(&mut app);
    let n = *rows_shown(&app).iter().nth(3).expect("rows on screen");
    open(&mut app, &format!("Row {n}"));
    assert_eq!(last(&fake, "gws_event_read")["id"], format!("r{n}"));
}

#[test]
fn a_shorter_list_after_a_longer_one_shows_only_its_own_rows() {
    let (mut app, fake) = start_with(many());
    key(&mut app, "a");
    app.wait_for_within("text:Row 0", WAIT)
        .expect("the list shows");
    app.scroll("text:Row 0", px(2500.0))
        .expect("the list scrolls");
    settle(&mut app);
    fake.events
        .lock()
        .unwrap()
        .retain(|e| e["id"] == "r1" || e["id"] == "r2");
    key(&mut app, "m");
    key(&mut app, "a");
    app.wait_for_within("text:Row 1", WAIT)
        .expect("the short list shows");
    assert_eq!(rows_shown(&app), BTreeSet::from([1, 2]));
}

// --- Detail ------------------------------------------------------------------

fn on_its_guest_list(people: Value, guests: i64) -> Vec<Value> {
    let mut e = event("e1", "mine", "Team review", at(0, 9, 0), at(0, 10, 0));
    e["attendees"] = people;
    e["guests"] = json!(guests);
    vec![e]
}

#[test]
fn the_guest_count_leaves_out_the_person_and_the_room() {
    let mut me = guest("owner@example.com", "accepted");
    me["self"] = json!(true);
    let mut room = guest("room@example.com", "accepted");
    room["resource"] = json!(true);
    let people = json!([me, guest("ana@example.com", "accepted"), room]);
    let (mut app, _) = start_with(on_its_guest_list(people, 1));
    open(&mut app, "Team review");
    assert!(app.screen().shows("1 guest"), "{:?}", app.screen().texts());
    assert!(!app.screen().shows("3 guests"));
}

#[test]
fn alone_on_the_list_the_person_has_no_guests() {
    let mut me = guest("owner@example.com", "accepted");
    me["self"] = json!(true);
    let (mut app, _) = start_with(on_its_guest_list(json!([me]), 0));
    open(&mut app, "Team review");
    assert!(
        app.screen().shows("No other guests"),
        "{:?}",
        app.screen().texts()
    );
}

/// It sits under a mail's attachment presses there, and "Open" twice read as one.
#[test]
fn the_link_bars_press_says_open_link() {
    let (mut app, _) = start();
    open(&mut app, "Team review");
    press(&mut app, "label:Open the Meet link");
    assert!(
        app.screen().shows("Open link"),
        "{:?}",
        app.screen().texts()
    );
    assert!(!app.screen().shows("Open"));
}

#[test]
fn detail_shows_the_event_with_replies_and_opens_meet_through_the_link_check() {
    let (mut app, _) = start();
    open(&mut app, "Team review");
    for line in [
        "Room Seven",
        "Bring the numbers.",
        "ana@example.com",
        "ben@example.com",
        "Yes",
        "No",
        "2 guests",
        "Mine",
    ] {
        assert!(
            app.screen().shows(line),
            "{line}: {:?}",
            app.screen().texts()
        );
    }
    assert!(
        !app.screen().matches("label:Answer maybe").unwrap(),
        "not an invite"
    );

    press(&mut app, "label:Open the Meet link");
    assert!(
        app.screen().shows("Open meet.example.com?"),
        "it asks first"
    );
    assert!(app.screen().opened_urls().is_empty());
    press(&mut app, "label:Open this link");
    app.wait_for_within("text:Team review", WAIT)
        .expect("still here");
    assert_eq!(
        app.screen().opened_urls(),
        ["https://meet.example.com/abc-defg"]
    );
}

#[test]
fn an_invite_takes_a_reply() {
    let mut row = invite(event("i1", "mine", "Launch", at(0, 11, 0), at(0, 12, 0)));
    row["guests"] = json!(1);
    let (mut app, fake) = start_with(vec![row]);
    open(&mut app, "Launch");
    assert!(
        !app.screen().matches("label:Edit this event").unwrap(),
        "not this person's to edit"
    );
    press(&mut app, "label:Answer maybe");
    assert_eq!(
        last(&fake, "gws_event_rsvp"),
        json!({"calendar": "mine", "id": "i1", "response": "tentative"})
    );
    assert!(app.screen().shows("Your reply was sent."));
}

#[test]
fn e_opens_the_next_thing_and_escape_leaves() {
    let (mut app, fake) = start();
    key(&mut app, "e");
    app.wait_for_within("label:Back to the calendar", WAIT)
        .expect("it opens");
    assert_eq!(fake.called("gws_event_read").len(), 1);
    key(&mut app, "escape");
    assert!(!app.screen().matches("label:Back to the calendar").unwrap());
}

// --- Saving ------------------------------------------------------------------

#[test]
fn save_says_what_it_will_do_and_the_call_carries_those_guests() {
    let (mut app, fake) = start();
    write_one(&mut app, "Board dinner");
    assert!(app.screen().shows("Save"));
    add_guest(&mut app, "ana@example.com");
    assert!(app.screen().shows("Save and invite 1"));
    add_guest(&mut app, "ben@example.com");
    assert!(app.screen().shows("Save and invite 2"));
    assert!(!app.screen().shows("Save"), "the plain word has gone");
    fill(
        &mut app,
        "Place, an address or a room that guests will read",
        "The long table",
    );
    press(&mut app, "label:Save this event");

    let saved = last(&fake, "gws_event_save");
    assert_eq!(
        saved["guests"],
        json!(["ana@example.com", "ben@example.com"])
    );
    assert_eq!(saved["title"], "Board dinner");
    assert_eq!(saved["where"], "The long table");
    assert_eq!(saved["calendar"], "mine");
    assert_eq!(saved["time_zone"], "Asia/Manila");
    assert!(saved.get("id").is_none() && saved.get("scope").is_none());
    assert!(
        app.screen()
            .shows("Saved, and the invite went to 2 guests.")
    );
    assert!(
        !app.screen()
            .matches("label:Title, the name guests will see on their calendars")
            .unwrap(),
        "the editor closed"
    );
}

#[test]
fn an_address_still_being_typed_counts_and_goes() {
    let (mut app, fake) = start();
    write_one(&mut app, "Lunch");
    app.type_text(
        "label:Guest's address, like name@company.com",
        "ana@example.com",
    )
    .expect("it types");
    settle(&mut app);
    assert!(
        app.screen().shows("Save and invite 1"),
        "{:?}",
        app.screen().texts()
    );
    key(&mut app, "mod+s");
    assert_eq!(
        last(&fake, "gws_event_save")["guests"],
        json!(["ana@example.com"])
    );
}

#[test]
fn a_comma_ends_an_address_and_a_guest_can_be_taken_off() {
    let (mut app, fake) = start();
    write_one(&mut app, "Lunch");
    app.type_text(
        "label:Guest's address, like name@company.com",
        "ana@example.com, ben@example.com, ",
    )
    .expect("it types");
    settle(&mut app);
    assert!(
        app.screen().shows("Save and invite 2"),
        "{:?}",
        app.screen().texts()
    );
    press(&mut app, "label:Remove this guest >> text:ben@example.com");
    assert!(app.screen().shows("Save and invite 1"));
    press(&mut app, "label:Save this event");
    assert_eq!(
        last(&fake, "gws_event_save")["guests"],
        json!(["ana@example.com"])
    );
}

#[test]
fn a_guest_that_isnt_an_address_stops_the_save_and_says_so() {
    let (mut app, fake) = start();
    write_one(&mut app, "Lunch");
    app.type_text("label:Guest's address, like name@company.com", "ana")
        .expect("it types");
    press(&mut app, "label:Save this event");
    assert!(app.screen().shows("Check this guest's address: ana"));
    assert_eq!(fake.writes(), 0);
    assert!(
        app.screen()
            .matches("label:Title, the name guests will see on their calendars")
            .unwrap()
    );
}

#[test]
fn a_suggestion_becomes_a_guest() {
    let (mut app, fake) = start();
    write_one(&mut app, "Lunch");
    app.type_text("label:Guest's address, like name@company.com", "an")
        .expect("it types");
    app.wait_for_within("label:Use this address", WAIT)
        .expect("it suggests");
    assert_eq!(last(&fake, "gws_cal_people")["query"], "an");
    press(&mut app, "label:Use this address");
    assert!(app.screen().shows("Save and invite 1"));
    assert_eq!(field(&app, "Guest's address, like name@company.com"), "");
    assert!(!app.screen().matches("label:Use this address").unwrap());
}

#[test]
fn an_all_day_event_saves_googles_exclusive_end_and_reads_back_the_last_day() {
    let (mut app, fake) = start();
    write_one(&mut app, "Conference");
    press(&mut app, "label:All day");
    fill(&mut app, "Start date, like 2026-11-06", "2030-05-01");
    fill(&mut app, "End date, like 2026-11-06", "2030-05-03");
    assert!(
        !app.screen()
            .matches("label:Start time, like 09:30 or 14:30 or 18:05")
            .unwrap()
    );
    press(&mut app, "label:Save this event");
    let saved = last(&fake, "gws_event_save");
    assert_eq!(
        (&saved["start"], &saved["end"]),
        (&json!("2030-05-01"), &json!("2030-05-04"))
    );
    assert_eq!(saved["all_day"], true);

    // it moved the view there, so the person sees what they saved
    open(&mut app, "Conference >> nth=0");
    press(&mut app, "label:Edit this event");
    app.wait_for_within(
        "label:Title, the name guests will see on their calendars",
        WAIT,
    )
    .expect("the editor opens");
    assert_eq!(
        field(&app, "End date, like 2026-11-06"),
        "2030-05-03",
        "the last day, not Google's end"
    );
}

#[test]
fn an_end_before_the_start_writes_nothing_and_says_so() {
    let (mut app, fake) = start();
    write_one(&mut app, "Backwards");
    fill(&mut app, "Start date, like 2026-11-06", "2030-05-01");
    fill(&mut app, "End date, like 2026-11-06", "2030-05-01");
    fill(
        &mut app,
        "Start time, like 09:30 or 14:30 or 18:05",
        "15:00",
    );
    fill(&mut app, "End time, like 10:30 or 15:30 or 19:05", "14:00");
    press(&mut app, "label:Save this event");
    assert!(app.screen().shows("It ends before it starts."));
    assert_eq!(fake.writes(), 0);
}

#[test]
fn an_edit_carries_its_id_and_etag_and_keeps_its_guests() {
    let (mut app, fake) = start();
    open(&mut app, "Team review");
    press(&mut app, "label:Edit this event");
    app.wait_for_within(
        "label:Title, the name guests will see on their calendars",
        WAIT,
    )
    .expect("the editor opens");
    assert!(app.screen().shows("Save and update 2"));
    fill(
        &mut app,
        "Title, the name guests will see on their calendars",
        "Team review, moved",
    );
    press(&mut app, "label:Save this event");
    let saved = last(&fake, "gws_event_save");
    assert_eq!((&saved["id"], &saved["etag"]), (&json!("e1"), &json!("e1")));
    assert_eq!(
        saved["guests"],
        json!(["ana@example.com", "ben@example.com"])
    );
}

#[test]
fn the_persons_own_place_on_the_list_stays_and_isnt_an_invite() {
    let mut row = event("o1", "mine", "Standup", at(0, 11, 0), at(0, 12, 0));
    let mut me = guest("owner@example.com", "accepted");
    me["self"] = json!(true);
    row["attendees"] = json!([me, guest("ana@example.com", "accepted")]);
    row["guests"] = json!(2);
    let (mut app, fake) = start_with(vec![row]);
    open(&mut app, "Standup");
    press(&mut app, "label:Edit this event");
    app.wait_for_within(
        "label:Title, the name guests will see on their calendars",
        WAIT,
    )
    .expect("the editor opens");
    assert!(
        app.screen().shows("Save and update 1"),
        "{:?}",
        app.screen().texts()
    );
    press(&mut app, "label:Save this event");
    assert_eq!(
        last(&fake, "gws_event_save")["guests"],
        json!(["owner@example.com", "ana@example.com"])
    );
}

#[test]
fn a_refused_change_writes_nothing_more_and_shows_the_newer_one() {
    let (mut app, fake) = start();
    open(&mut app, "Team review");
    press(&mut app, "label:Edit this event");
    app.wait_for_within(
        "label:Title, the name guests will see on their calendars",
        WAIT,
    )
    .expect("the editor opens");
    fill(
        &mut app,
        "Title, the name guests will see on their calendars",
        "My version",
    );

    let mut newer = fake.row("e1").unwrap();
    newer["title"] = json!("Their version");
    newer["where"] = json!("Room Nine");
    newer["etag"] = json!("e2");
    *fake.changed.lock().unwrap() = Some(newer);
    press(&mut app, "label:Save this event");
    assert_eq!(fake.called("gws_event_save").len(), 1, "tried once");
    assert!(app.screen().shows(
        "Someone else changed this event, so yours wasn't saved. The newer one is shown here."
    ));
    assert!(app.screen().shows("Their version"));
    assert!(app.screen().shows("Room Nine"));
    assert_eq!(
        field(&app, "Title, the name guests will see on their calendars"),
        "My version",
        "their words aren't lost"
    );
    assert!(!app.screen().matches("label:Save this event").unwrap());
    key(&mut app, "mod+s");
    assert_eq!(
        fake.called("gws_event_save").len(),
        1,
        "no way round the choice"
    );

    press(&mut app, "label:Keep my changes");
    assert_eq!(
        field(&app, "Title, the name guests will see on their calendars"),
        "My version"
    );
    press(&mut app, "label:Save this event");
    let saved = fake.called("gws_event_save");
    assert_eq!(saved.len(), 2);
    assert_eq!(saved[1]["etag"], "e2", "this one knows what it overwrites");
}

#[test]
fn use_the_newer_one_fills_the_form_with_it() {
    let (mut app, fake) = start();
    open(&mut app, "Team review");
    press(&mut app, "label:Edit this event");
    app.wait_for_within(
        "label:Title, the name guests will see on their calendars",
        WAIT,
    )
    .expect("the editor opens");
    let mut newer = fake.row("e1").unwrap();
    newer["title"] = json!("Their version");
    newer["etag"] = json!("e2");
    *fake.changed.lock().unwrap() = Some(newer);
    press(&mut app, "label:Save this event");
    press(&mut app, "label:Use the newer one");
    assert_eq!(
        field(&app, "Title, the name guests will see on their calendars"),
        "Their version"
    );
    assert_eq!(fake.called("gws_event_save").len(), 1);
}

#[test]
fn a_double_press_on_save_is_one_save() {
    let (mut app, fake) = start();
    write_one(&mut app, "Once");
    fake.hold("gws_event_save");
    press(&mut app, "label:Save this event");
    let _ = app.press("label:Save this event");
    settle(&mut app);
    assert_eq!(fake.called("gws_event_save").len(), 1);
    fake.release("gws_event_save");
    settle(&mut app);
}

// --- A series ----------------------------------------------------------------

fn a_series() -> Vec<Value> {
    let mut row = in_series(event(
        "s1",
        "mine",
        "Weekly sync",
        at(0, 14, 0),
        at(0, 15, 0),
    ));
    row["response"] = json!("needsAction");
    vec![row]
}

#[test]
fn a_series_asks_before_an_edit_and_the_call_carries_the_scope() {
    for (press_on, scope) in [("This event", "this"), ("All events", "all")] {
        let (mut app, fake) = start_with(a_series());
        open(&mut app, "Weekly sync");
        press(&mut app, "label:Edit this event");
        app.wait_for_within(
            "label:Title, the name guests will see on their calendars",
            WAIT,
        )
        .expect("the editor opens");
        fill(
            &mut app,
            "Title, the name guests will see on their calendars",
            "Weekly sync, new",
        );
        press(&mut app, "label:Save this event");
        assert_eq!(fake.writes(), 0, "not before it's answered");
        assert!(app.screen().shows("Save this change to which events?"));
        press(&mut app, &format!("label:{press_on}"));
        let saved = last(&fake, "gws_event_save");
        assert_eq!(saved["scope"], scope);
        assert_eq!(saved["id"], "s1");
        assert_eq!(saved["repeat"], "RRULE:FREQ=WEEKLY", "the rule isn't lost");
    }
}

#[test]
fn a_series_question_can_be_called_off_and_a_change_drops_it() {
    let (mut app, fake) = start_with(a_series());
    open(&mut app, "Weekly sync");
    press(&mut app, "label:Edit this event");
    app.wait_for_within(
        "label:Title, the name guests will see on their calendars",
        WAIT,
    )
    .expect("the editor opens");
    press(&mut app, "label:Save this event");
    press(&mut app, "label:Cancel this");
    assert_eq!(fake.writes(), 0);
    press(&mut app, "label:Save this event");
    fill(
        &mut app,
        "Title, the name guests will see on their calendars",
        "Changed after the question",
    );
    assert!(
        !app.screen().matches("label:All events").unwrap(),
        "asked about something else"
    );
    assert_eq!(fake.writes(), 0);
}

#[test]
fn a_series_asks_before_a_delete_and_a_reply() {
    let (mut app, fake) = start_with(a_series());
    open(&mut app, "Weekly sync");
    press(&mut app, "label:Answer yes");
    assert_eq!(fake.writes(), 0);
    press(&mut app, "label:All events");
    let reply = last(&fake, "gws_event_rsvp");
    assert_eq!(
        (&reply["response"], &reply["scope"]),
        (&json!("accepted"), &json!("all"))
    );

    press(&mut app, "label:Delete this event");
    assert_eq!(fake.called("gws_event_delete").len(), 0);
    press(&mut app, "label:This event");
    assert_eq!(last(&fake, "gws_event_delete")["scope"], "this");
}

// --- Delete and Undo ---------------------------------------------------------

#[test]
fn delete_asks_once_and_undo_shows_when_the_answer_says_so() {
    let (mut app, fake) = start();
    open(&mut app, "Dentist");
    key(&mut app, "delete");
    assert!(app.screen().shows("Delete this event?"));
    assert_eq!(fake.writes(), 0, "it asks first");
    press(&mut app, "label:Delete it");
    assert_eq!(fake.called("gws_event_delete").len(), 1);
    assert!(!app.screen().shows("Dentist"), "{:?}", app.screen().texts());
    assert!(app.screen().matches("label:Undo that").unwrap());

    key(&mut app, "z");
    assert_eq!(
        last(&fake, "gws_event_restore"),
        json!({"calendar": "work", "id": "e2"})
    );
    assert!(app.screen().shows("Dentist"), "it came back");
    assert!(!app.screen().matches("label:Undo that").unwrap());
}

#[test]
fn a_delete_with_guests_says_they_will_be_told_and_offers_no_undo() {
    let (mut app, fake) = start();
    open(&mut app, "Team review");
    press(&mut app, "label:Delete this event");
    assert!(
        app.screen()
            .shows("Delete this event? Its 2 guests will be told.")
    );
    press(&mut app, "label:Delete it");
    assert_eq!(fake.called("gws_event_delete").len(), 1);
    assert!(app.screen().shows("Event deleted. Its guests were told."));
    assert!(!app.screen().matches("label:Undo that").unwrap());
    key(&mut app, "z");
    assert!(fake.called("gws_event_restore").is_empty());
}

#[test]
fn cancelling_the_question_deletes_nothing() {
    let (mut app, fake) = start();
    open(&mut app, "Dentist");
    press(&mut app, "label:Delete this event");
    press(&mut app, "label:Cancel this");
    assert_eq!(fake.writes(), 0);
    assert!(app.screen().shows("Dentist"));
}

// --- The AI's press ----------------------------------------------------------

#[test]
fn the_ais_press_on_save_changes_nothing_and_says_so() {
    let (mut app, fake) = start();
    write_one(&mut app, "From the AI");
    add_guest(&mut app, "ana@example.com");
    let before = fake.events.lock().unwrap().clone();
    *fake.ai.lock().unwrap() = true;
    press(&mut app, "label:Save this event");
    assert!(app.screen().shows(AI_REFUSED), "{:?}", app.screen().texts());
    assert!(
        app.screen()
            .matches("label:Title, the name guests will see on their calendars")
            .unwrap(),
        "the editor stays"
    );
    assert_eq!(
        field(&app, "Title, the name guests will see on their calendars"),
        "From the AI"
    );
    assert!(app.screen().shows("Save and invite 1"));
    assert_eq!(*fake.events.lock().unwrap(), before);
    assert!(!app.screen().shows("Saved, and the invite went to 1 guest."));
}

#[test]
fn the_ais_press_on_delete_changes_nothing_and_says_so() {
    let (mut app, fake) = start();
    open(&mut app, "Dentist");
    press(&mut app, "label:Delete this event");
    *fake.ai.lock().unwrap() = true;
    press(&mut app, "label:Delete it");
    assert!(app.screen().shows(AI_REFUSED));
    assert!(app.screen().shows("Dentist"), "still open");
    assert!(!app.screen().matches("label:Undo that").unwrap());
    assert_eq!(fake.events.lock().unwrap().len(), 3);
}

#[test]
fn the_ais_press_on_a_reply_changes_nothing_and_says_so() {
    let (mut app, fake) = start_with(vec![invite(event(
        "i1",
        "mine",
        "Launch",
        at(0, 11, 0),
        at(0, 12, 0),
    ))]);
    open(&mut app, "Launch");
    *fake.ai.lock().unwrap() = true;
    press(&mut app, "label:Answer yes");
    assert!(app.screen().shows(AI_REFUSED));
    assert_eq!(fake.row("i1").unwrap()["response"], "needsAction");
    assert!(!app.screen().shows("Your reply was sent."));
}

#[test]
fn the_ais_press_on_undo_keeps_the_bar() {
    let (mut app, fake) = start();
    open(&mut app, "Dentist");
    press(&mut app, "label:Delete this event");
    press(&mut app, "label:Delete it");
    *fake.ai.lock().unwrap() = true;
    press(&mut app, "label:Undo that");
    assert!(app.screen().shows(AI_REFUSED));
    assert!(app.screen().matches("label:Undo that").unwrap());
    assert!(!app.screen().shows("Dentist"));
}

/// The Bot takes who made a call from the moment it's made. So the call has to
/// go in the press, even with the queue stuck behind something else: a call that
/// waited its turn would be judged after the AI's press had ended.
#[test]
fn a_write_is_called_in_the_press_not_when_the_queue_gets_to_it() {
    let (mut app, fake) = start();
    open(&mut app, "Dentist");
    fake.hold("gws_cal_show");
    press(&mut app, "text:Work");
    press(&mut app, "label:Delete this event");
    press(&mut app, "label:Delete it");
    assert_eq!(
        fake.called("gws_event_delete").len(),
        1,
        "called with the queue held"
    );
    fake.release("gws_cal_show");
    settle(&mut app);
}

#[test]
fn a_write_is_never_tried_twice_on_its_own() {
    let (mut app, fake) = start();
    open(&mut app, "Dentist");
    press(&mut app, "label:Delete this event");
    *fake.busy.lock().unwrap() = true;
    press(&mut app, "label:Delete it");
    app.wait(Duration::from_secs(2)).expect("time passes");
    assert_eq!(fake.called("gws_event_delete").len(), 1);
    assert!(app.screen().shows("busy"));
    assert!(app.screen().shows("Dentist"));
}

// --- Keys --------------------------------------------------------------------

#[test]
fn keys_do_nothing_while_typing_in_a_field() {
    let (mut app, fake) = start();
    key(&mut app, "/");
    let asked = fake.called("gws_cal_range").len();
    for k in ["d", "w", "m", "a", "j", "k", "t", "c", "e", "g", "z"] {
        key(&mut app, k);
    }
    assert_eq!(fake.called("gws_cal_range").len(), asked, "no view moved");
    assert!(
        !app.screen()
            .matches("label:Title, the name guests will see on their calendars")
            .unwrap(),
        "no editor opened"
    );
    assert!(!app.screen().matches("label:Back to the calendar").unwrap());
    assert!(!app.screen().matches("label:Go to date").unwrap());
}

#[test]
fn keys_stay_out_of_the_editor_even_with_no_field_focused() {
    let (mut app, fake) = start();
    key(&mut app, "c");
    app.wait_for_within(
        "label:Title, the name guests will see on their calendars",
        WAIT,
    )
    .expect("the editor opens");
    let asked = fake.called("gws_cal_range").len();
    let date = field(&app, "Start date, like 2026-11-06");
    key(&mut app, "m");
    key(&mut app, "j");
    key(&mut app, "c");
    key(&mut app, "delete");
    assert_eq!(fake.called("gws_cal_range").len(), asked);
    assert_eq!(field(&app, "Start date, like 2026-11-06"), date);
    assert_eq!(
        field(&app, "Title, the name guests will see on their calendars"),
        ""
    );
}

#[test]
fn cmd_s_saves_while_typing_and_a_second_is_the_same_save() {
    let (mut app, fake) = start();
    write_one(&mut app, "By keyboard");
    app.press("label:Title, the name guests will see on their calendars")
        .ok();
    key(&mut app, "mod+s");
    assert_eq!(fake.called("gws_event_save").len(), 1);
    key(&mut app, "mod+s");
    assert_eq!(fake.called("gws_event_save").len(), 1, "the editor closed");
}

// --- Account states ----------------------------------------------------------

fn opened_with_status(status: Value) -> (Headless, Arc<Fake>) {
    let fake = Arc::new(Fake::default());
    *fake.status.lock().unwrap() = Some(status);
    let mut app = open_with(fake.clone(), 1000.0, 600.0);
    settle(&mut app);
    (app, fake)
}

#[test]
fn signed_out_says_so_and_reads_nothing() {
    let (app, fake) = opened_with_status(json!({"signed_in": false}));
    assert!(
        app.screen()
            .shows("Connect your Google account on this app's page.")
    );
    assert!(fake.called("gws_cal_range").is_empty());
    assert!(!app.screen().matches("label:Today").unwrap());
}

#[test]
fn a_sign_in_that_doesnt_cover_calendar_says_so() {
    let (app, fake) = opened_with_status(
        json!({"signed_in": true, "features": {"mail": true, "calendar": false}}),
    );
    assert!(
        app.screen()
            .shows("Your Google sign-in doesn't cover Calendar yet.")
    );
    assert!(fake.called("gws_cal_range").is_empty());
}

#[test]
fn scopes_not_on_record_get_to_try() {
    let (app, fake) = opened_with_status(json!({"signed_in": true, "features": null}));
    assert!(app.screen().matches("label:Today").unwrap());
    assert_eq!(fake.called("gws_cal_range").len(), 1);
}

#[test]
fn a_sign_in_lost_while_open_says_so() {
    let (mut app, fake) = start();
    *fake.signed_out.lock().unwrap() = true;
    key(&mut app, "m");
    app.wait_for_within("text:Connect your Google account on this app's page.", WAIT)
        .expect("it says so");
}

#[test]
fn saved_events_say_they_are_saved() {
    let (mut app, fake) = start();
    assert!(!app.screen().shows("Showing saved events"));
    *fake.offline.lock().unwrap() = true;
    key(&mut app, "m");
    assert!(app.screen().shows("Showing saved events"));
    assert!(app.screen().shows("Team review"));
}

// --- Cards -------------------------------------------------------------------

#[test]
fn slash_calendar_new_opens_the_editor() {
    let fake = Arc::new(Fake::default());
    *fake.props.lock().unwrap() = json!({"view": "new"});
    let mut app = open_with(fake.clone(), 1000.0, 600.0);
    app.wait_for_within("label:Save this event", WAIT)
        .expect("the editor opens");
}

fn card(props: Value, set: impl FnOnce(&Fake), width: f32, height: f32) -> (Headless, Arc<Fake>) {
    let fake = Arc::new(Fake::default());
    *fake.inline.lock().unwrap() = true;
    *fake.props.lock().unwrap() = props;
    set(&fake);
    let app = open_with(fake.clone(), width, height);
    (app, fake)
}

fn next_answer() -> Value {
    let row = |id: &str, title: &str, hour| {
        let mut e = event(id, "mine", title, at(1, hour, 0), at(1, hour + 1, 0));
        e["where"] = json!("Room Seven");
        e
    };
    json!({"event": row("n1", "First up", 9), "after": [row("n2", "Then this", 11), row("n3", "And this", 13)]})
}

#[test]
fn the_next_card_shows_three_with_no_click_and_a_row_opens_the_event() {
    let (mut app, fake) = card(
        json!({"view": "next"}),
        |f| *f.next.lock().unwrap() = next_answer(),
        420.0,
        300.0,
    );
    app.wait_for_within("text:First up", WAIT)
        .expect("it shows");
    assert!(app.screen().shows("Then this"));
    assert!(app.screen().shows("And this"));
    press(&mut app, "text:Then this");
    let opened = fake.host.lock().unwrap().clone();
    assert_eq!(opened[0].0, "buwiz.open");
    assert_eq!(opened[0].1["app"], "calendar");
    assert_eq!(
        opened[0].1["props"]["event"],
        json!({"calendar": "mine", "id": "n2"})
    );
}

#[test]
fn the_next_card_with_nothing_coming_says_so() {
    let (mut app, _) = card(
        json!({"view": "next"}),
        |f| *f.next.lock().unwrap() = json!({"event": null, "after": []}),
        420.0,
        300.0,
    );
    app.wait_for_within("text:Nothing coming up.", WAIT)
        .expect("it says so");
}

fn a_draft(guests: usize) -> Value {
    let list: Vec<String> = (0..guests)
        .map(|n| format!("guest{n}@example.com"))
        .collect();
    json!({
        "title": "Quarterly plan", "start": at(2, 10, 0), "end": at(2, 11, 0), "all_day": false,
        "time_zone": "Asia/Manila", "calendar": "work", "guests": list, "where": "Room Seven",
        "notes": "Bring the plan.", "repeat": "RRULE:FREQ=WEEKLY", "meet": true,
    })
}

fn draft_card(draft: Value, width: f32) -> (Headless, Arc<Fake>) {
    let (mut app, fake) = card(
        json!({"draft": "tk1"}),
        |f| *f.draft.lock().unwrap() = draft,
        width,
        420.0,
    );
    app.wait_for_within("label:Keep draft", WAIT)
        .expect("the card shows");
    (app, fake)
}

const LINK_LOST: &str = "This card lost its link when the plugin restarted.";

#[test]
fn a_draft_card_whose_ticket_is_lost_says_so_and_opens_calendar() {
    let (mut app, fake) = card(
        json!({"draft": "tk1"}),
        |f| *f.draft_lost.lock().unwrap() = true,
        420.0,
        420.0,
    );
    app.wait_for_within(&format!("text:{LINK_LOST}"), WAIT)
        .expect("it says what happened");
    assert!(!app.screen().matches("label:Save this event").unwrap());
    assert!(!app.screen().matches("label:Try again").unwrap());
    assert_eq!(
        last(&fake, "gws_event_draft_read"),
        json!({"ticket": "tk1"})
    );
    press(&mut app, "label:Open Calendar");
    let opened = fake.host.lock().unwrap().clone();
    assert_eq!(opened[0].1["app"], "calendar");
    assert_eq!(opened[0].1["props"], json!({}));
}

#[test]
fn a_draft_card_sends_what_the_chat_drafted_so_a_lost_ticket_is_bound_again() {
    let drafted = json!({"title": "Quarterly plan", "start": at(2, 10, 0), "end": at(2, 11, 0)});
    let (mut app, fake) = card(
        json!({"draft": "tk1", "origin": {"tool": "gws_event_draft", "args": drafted}}),
        |f| {
            *f.draft_lost.lock().unwrap() = true;
            *f.draft.lock().unwrap() = a_draft(0);
        },
        420.0,
        420.0,
    );
    app.wait_for_within("label:Keep draft", WAIT)
        .expect("the card shows");
    assert_eq!(
        last(&fake, "gws_event_draft_read"),
        json!({"ticket": "tk1", "fields": drafted})
    );
    assert!(app.screen().shows("Quarterly plan"));
    assert!(!app.screen().shows(LINK_LOST));
}

/// The card says the account it was drafted on: the fields go with it, so
/// the plugin binds the ticket to that account and no other.
#[test]
fn a_draft_card_sends_the_account_it_was_drafted_on_with_the_chats_fields() {
    let drafted = json!({"title": "Quarterly plan", "start": at(2, 10, 0), "end": at(2, 11, 0)});
    let (mut app, fake) = card(
        json!({"draft": "tk1", "account": "me@example.com", "origin": {"tool": "gws_event_draft", "args": drafted}}),
        |f| {
            *f.draft_lost.lock().unwrap() = true;
            *f.draft.lock().unwrap() = a_draft(0);
        },
        420.0,
        420.0,
    );
    app.wait_for_within("label:Keep draft", WAIT)
        .expect("the card shows");
    let mut sent = drafted.clone();
    sent["as_account"] = json!("me@example.com");
    assert_eq!(
        last(&fake, "gws_event_draft_read"),
        json!({"ticket": "tk1", "fields": sent})
    );
}

#[test]
fn a_lost_draft_card_that_knows_its_event_can_try_again() {
    let drafted = json!({"title": "Quarterly plan", "start": at(2, 10, 0), "end": at(2, 11, 0)});
    let (mut app, fake) = card(
        // a search's call is not this card's
        json!({"draft": "tk1", "origin": {"tool": "gws_search_events", "args": drafted}}),
        |f| *f.draft_lost.lock().unwrap() = true,
        420.0,
        420.0,
    );
    app.wait_for_within(&format!("text:{LINK_LOST}"), WAIT)
        .expect("it says what happened");
    assert_eq!(
        last(&fake, "gws_event_draft_read"),
        json!({"ticket": "tk1"})
    );

    let (mut app, fake) = card(
        json!({"draft": "tk1", "origin": {"tool": "gws_event_draft", "args": drafted}}),
        // a plugin from before tickets were kept takes no fields
        |f| {
            *f.draft.lock().unwrap() =
                json!({"refused": "not_found", "message": "This card is out of date."})
        },
        420.0,
        420.0,
    );
    app.wait_for_within(&format!("text:{LINK_LOST}"), WAIT)
        .expect("it says what happened");
    assert!(!app.screen().shows("This card is out of date."));
    *fake.draft.lock().unwrap() = a_draft(0);
    press(&mut app, "label:Try again");
    app.wait_for_within("label:Keep draft", WAIT)
        .expect("the card shows");
}

#[test]
fn the_draft_card_shows_everything_save_would_write_and_writes_that() {
    let (mut app, fake) = draft_card(a_draft(2), 420.0);
    let when = time::when_line(
        &When::of(&at(2, 10, 0), &at(2, 11, 0), false).unwrap(),
        &Manila,
    );
    for line in [
        "Quarterly plan",
        when.as_str(),
        "Asia/Manila",
        "Every week",
        "guest0@example.com, guest1@example.com",
        "Room Seven",
        "Work",
        "A Meet link will be added",
        "Bring the plan.",
        "Save and invite 2",
    ] {
        assert!(
            app.screen().shows(line),
            "{line}: {:?}",
            app.screen().texts()
        );
    }
    assert_eq!(fake.writes(), 0, "showing a card writes nothing");
    press(&mut app, "label:Save this event");
    let saved = last(&fake, "gws_event_save");
    assert_eq!(saved["title"], "Quarterly plan");
    assert_eq!(
        saved["guests"],
        json!(["guest0@example.com", "guest1@example.com"])
    );
    assert_eq!(saved["meet"], true);
    assert_eq!(saved["calendar"], "work");
    assert_eq!(saved["repeat"], "RRULE:FREQ=WEEKLY");
    assert_eq!(saved["notes"], "Bring the plan.");
    assert!(
        app.screen()
            .shows("Saved, and the invite went to 2 guests.")
    );
}

#[test]
fn a_draft_card_with_no_guests_just_says_save_and_keep_writes_nothing() {
    let (mut app, fake) = draft_card(a_draft(0), 420.0);
    assert!(app.screen().shows("Save"));
    press(&mut app, "label:Keep draft");
    assert!(app.screen().shows("Kept. Nothing was saved."));
    assert_eq!(fake.writes(), 0);
}

#[test]
fn a_draft_cards_edit_opens_the_screen_on_that_draft_with_its_fields() {
    let (mut app, fake) = draft_card(a_draft(1), 420.0);
    press(&mut app, "label:Edit this event");
    let opened = fake.host.lock().unwrap().clone();
    assert_eq!(opened[0].1["props"], json!({"draft": "tk1"}));

    let fake = Arc::new(Fake::default());
    *fake.props.lock().unwrap() = json!({"draft": "tk1"});
    *fake.draft.lock().unwrap() = a_draft(1);
    let mut app = open_with(fake.clone(), 1000.0, 600.0);
    app.wait_for_within(
        "label:Title, the name guests will see on their calendars",
        WAIT,
    )
    .expect("the editor opens");
    assert_eq!(
        field(&app, "Title, the name guests will see on their calendars"),
        "Quarterly plan"
    );
    assert!(app.screen().shows("Save and invite 1"));
    assert!(app.screen().shows("guest0@example.com"));
}

#[test]
fn the_ais_press_on_a_cards_save_stops_at_the_press() {
    let (mut app, fake) = draft_card(a_draft(2), 420.0);
    *fake.ai.lock().unwrap() = true;
    press(&mut app, "label:Save this event");
    assert!(app.screen().shows(AI_REFUSED));
    assert!(app.screen().shows("Quarterly plan"), "still showing");
    assert!(
        app.screen().shows("Save and invite 2"),
        "and can be pressed"
    );
    assert!(fake.events.lock().unwrap().is_empty());
}

#[test]
fn a_draft_that_is_gone_says_so_and_offers_no_save() {
    let (mut app, _) = card(
        json!({"draft": "old"}),
        |f| *f.draft_gone.lock().unwrap() = true,
        420.0,
        300.0,
    );
    app.wait_for_within("text:That draft is gone.", WAIT)
        .expect("it says so");
    assert!(!app.screen().matches("label:Save this event").unwrap());
}

#[test]
fn many_invites_on_a_card_ask_once_more() {
    let (mut app, fake) = draft_card(a_draft(6), 420.0);
    press(&mut app, "label:Save this event");
    assert_eq!(fake.writes(), 0, "not yet");
    assert!(app.screen().shows("Invite 6 people?"));
    press(&mut app, "label:Invite them");
    assert_eq!(fake.called("gws_event_save").len(), 1);
}

// --- Narrow ------------------------------------------------------------------

/// 300 is the width that matters. The rest are narrower than anyone will use,
/// because `shows` counts a press with any of it on screen: only by going on
/// until a row that doesn't wrap has pushed a whole press off does the test see
/// a row that's over by less than one press.
const WIDTHS: [f32; 7] = [300.0, 280.0, 240.0, 200.0, 160.0, 120.0, 100.0];

fn guests(n: usize) -> Vec<String> {
    (0..n)
        .map(|i| format!("a.rather.long.address.for.guest.number.{i}@subdomain.example.com"))
        .collect()
}

fn assert_at_every_width(app: &mut Headless, height: f32, lines: &[&str]) {
    for width in WIDTHS {
        app.set_viewport(size(px(width), px(height)))
            .expect("the viewport sets");
        settle(app);
        for line in lines {
            assert!(
                app.screen().shows(line),
                "{line} went at {width}: {:?}",
                app.screen().texts()
            );
        }
    }
}

#[test]
fn a_long_title_and_many_guests_leave_a_cards_presses_on_it() {
    let mut draft = a_draft(0);
    draft["title"] = json!(LONG_TITLE);
    draft["guests"] = json!(guests(12));
    let (mut app, _) = draft_card(draft, 300.0);
    assert_at_every_width(&mut app, 420.0, &["Keep", "Edit", "Save and invite 12"]);
}

#[test]
fn a_cards_second_press_stays_on_it() {
    let mut draft = a_draft(0);
    draft["title"] = json!(LONG_TITLE);
    draft["guests"] = json!(guests(12));
    let (mut app, _) = draft_card(draft, 300.0);
    press(&mut app, "label:Save this event");
    assert_at_every_width(
        &mut app,
        420.0,
        &[
            "Keep",
            "Edit",
            "Not yet",
            "Invite them",
            "Invite 12 people?",
        ],
    );
}

#[test]
fn a_long_title_and_many_guests_leave_the_editors_presses_on_it() {
    let fake = Arc::new(Fake::default());
    *fake.props.lock().unwrap() = json!({"draft": "tk1"});
    let mut draft = a_draft(0);
    draft["title"] = json!(LONG_TITLE);
    draft["guests"] = json!(guests(12));
    *fake.draft.lock().unwrap() = draft;
    let mut app = open_with(fake.clone(), 300.0, 500.0);
    app.wait_for_within(
        "label:Title, the name guests will see on their calendars",
        WAIT,
    )
    .expect("the editor opens");
    assert_at_every_width(&mut app, 500.0, &["Cancel", "Save and invite 12"]);
    assert_eq!(fake.writes(), 0);
}

#[test]
fn a_long_title_leaves_the_details_presses_and_the_questions_on_it() {
    let mut row = event("l1", "mine", LONG_TITLE, at(0, 11, 0), at(0, 12, 0));
    row["guests"] = json!(12);
    row["attendees"] = Value::Array(guests(12).iter().map(|g| guest(g, "accepted")).collect());
    row["response"] = json!("accepted");
    let fake = Arc::new(Fake::default());
    *fake.events.lock().unwrap() = vec![row];
    *fake.props.lock().unwrap() = json!({"event": {"calendar": "mine", "id": "l1"}});
    let mut app = open_with(fake.clone(), 300.0, 500.0);
    app.wait_for_within("label:Back to the calendar", WAIT)
        .expect("it opens");
    assert_at_every_width(
        &mut app,
        500.0,
        &["Back", "Edit", "Delete", "Yes", "Maybe", "No"],
    );
    press(&mut app, "label:Delete this event");
    assert_at_every_width(
        &mut app,
        500.0,
        &["Cancel", "Delete this event? Its 12 guests will be told."],
    );
    assert!(app.screen().matches("label:Delete it").unwrap());
}

#[test]
fn a_series_question_stays_on_a_narrow_screen() {
    let fake = Arc::new(Fake::default());
    *fake.events.lock().unwrap() = a_series();
    *fake.props.lock().unwrap() = json!({"event": {"calendar": "mine", "id": "s1"}});
    let mut app = open_with(fake.clone(), 300.0, 500.0);
    app.wait_for_within("label:Back to the calendar", WAIT)
        .expect("it opens");
    press(&mut app, "label:Delete this event");
    assert_at_every_width(&mut app, 500.0, &["Cancel", "This event", "All events"]);
}

#[test]
fn a_long_title_leaves_the_next_cards_rows_on_it() {
    let mut answer = next_answer();
    answer["event"]["title"] = json!(LONG_TITLE);
    let (mut app, _) = card(
        json!({"view": "next"}),
        |f| *f.next.lock().unwrap() = answer,
        300.0,
        300.0,
    );
    app.wait_for_within("text:Then this", WAIT)
        .expect("it shows");
    assert_at_every_width(&mut app, 300.0, &["Then this", "And this"]);
}

// --- The label rule ----------------------------------------------------------

/// Nothing in `secrets` is in any label on screen.
fn assert_no_label_holds(app: &Headless, secrets: &[&str], at: &str) {
    for secret in secrets {
        let selector = format!("label:/(?i){}/", regex_quoted(secret));
        assert!(
            !app.screen().matches(&selector).unwrap(),
            "{at}: a label holds {secret}"
        );
    }
    assert!(
        // the one fixed sample address keeps the AI out of the guest field
        !app.screen().matches("label:/@example/").unwrap(),
        "{at}: a label holds an address"
    );
}

fn regex_quoted(text: &str) -> String {
    text.chars()
        .flat_map(|c| {
            let escape = !c.is_alphanumeric() && c != ' ';
            escape.then_some('\\').into_iter().chain([c])
        })
        .collect()
}

#[test]
fn no_screen_puts_a_title_a_guest_or_a_place_in_a_label() {
    let secrets = [
        "Team review",
        "Dentist",
        "Offsite",
        "Room Seven",
        "Bring the numbers",
        "ana@example.com",
        "ben",
        "Board dinner",
        "Ana Reyes",
    ];
    let (mut app, _) = start();
    // the texts really are on screen, so the audit below isn't looking at nothing
    assert!(app.screen().shows("Team review"));
    assert_no_label_holds(&app, &secrets, "week");
    for (keys, name) in [("d", "day"), ("m", "month"), ("a", "list")] {
        key(&mut app, keys);
        assert!(app.screen().shows("Team review"), "{name}");
        assert_no_label_holds(&app, &secrets, name);
    }
    key(&mut app, "w");

    open(&mut app, "Team review");
    assert!(app.screen().shows("ana@example.com"));
    assert_no_label_holds(&app, &secrets, "detail");
    press(&mut app, "label:Delete this event");
    assert_no_label_holds(&app, &secrets, "the question");
    press(&mut app, "label:Cancel this");
    press(&mut app, "label:Edit this event");
    app.wait_for_within(
        "label:Title, the name guests will see on their calendars",
        WAIT,
    )
    .expect("the editor opens");
    assert!(app.screen().shows("ana@example.com"));
    assert_no_label_holds(&app, &secrets, "editor");

    app.type_text("label:Guest's address, like name@company.com", "an")
        .expect("it types");
    app.wait_for_within("label:Use this address", WAIT)
        .expect("it suggests");
    assert_no_label_holds(&app, &secrets, "suggestions");
}

#[test]
fn neither_card_puts_a_title_a_guest_or_a_place_in_a_label() {
    let (mut app, _) = card(
        json!({"view": "next"}),
        |f| *f.next.lock().unwrap() = next_answer(),
        420.0,
        300.0,
    );
    app.wait_for_within("text:First up", WAIT)
        .expect("it shows");
    assert_no_label_holds(&app, &["First up", "Then this", "Room Seven"], "next card");

    let (app, _) = draft_card(a_draft(2), 420.0);
    assert!(app.screen().shows("Quarterly plan"));
    assert_no_label_holds(
        &app,
        &["Quarterly plan", "guest0", "Room Seven", "Bring the plan"],
        "draft card",
    );
}

// --- Empty states ------------------------------------------------------------

fn audited(fake: &Arc<Fake>, width: f32, height: f32) -> (Headless, Filter) {
    let def = gws_screens::screens()
        .iter()
        .find(|s| s.id == "gws.calendar")
        .expect("Calendar");
    let root = (def.build)(Bridge::new(fake.clone()));
    let mut app = Headless::start(EngineConfig::new(root)).expect("the engine starts");
    let filter = Filter::watch(&mut app);
    app.set_viewport(size(px(width), px(height)))
        .expect("the viewport sets");
    (app, filter)
}

fn no_calendars(fake: &Fake) {
    *fake.calendars.lock().unwrap() = Some(json!({"calendars": []}));
}

fn all_hidden(fake: &Fake) {
    let mut hidden = fake.hidden.lock().unwrap();
    hidden.extend(["mine", "work", "feed"].map(str::to_owned));
}

const NO_CALENDARS: &str =
    "No calendars on this account yet. Make one in Google Calendar and it shows here.";

#[test]
fn every_empty_view_says_so_and_offers_the_next_press() {
    let (mut app, _) = start_with(Vec::new());
    assert!(
        app.screen()
            .shows("Nothing this week. Press New to add an event."),
        "{:?}",
        app.screen().texts()
    );
    for (keys, line) in [
        ("d", "Nothing on this day. Press New to add an event."),
        ("m", "Nothing this month. Press New to add an event."),
        ("a", "Nothing in these days. Press New to add an event."),
    ] {
        key(&mut app, keys);
        assert!(app.screen().shows(line), "{:?}", app.screen().texts());
    }
    app.type_text("label:Search events", "zzz").unwrap();
    app.submit("label:Search events").unwrap();
    settle(&mut app);
    assert!(app.screen().shows("No events match that. Try other words."));
}

#[test]
fn a_view_with_events_says_nothing_of_the_kind() {
    let (mut app, _) = start();
    assert!(
        !app.screen()
            .shows("Nothing this week. Press New to add an event.")
    );
    key(&mut app, "d");
    assert!(
        !app.screen()
            .shows("Nothing on this day. Press New to add an event.")
    );
}

#[test]
fn no_calendars_is_said_on_the_view_the_column_and_the_editor() {
    let fake = Arc::new(Fake::default());
    no_calendars(&fake);
    let mut app = open_with(fake.clone(), 1000.0, 600.0);
    app.wait_for_within(&format!("text:{NO_CALENDARS}"), WAIT)
        .expect("it says so");
    assert!(app.screen().shows("None yet."));
    key(&mut app, "a");
    assert!(app.screen().shows(NO_CALENDARS));
    key(&mut app, "c");
    assert!(app.screen().shows("You have no calendar to add this to."));
    press(&mut app, "label:Save this event");
    assert!(app.screen().shows("Choose a calendar for this event."));
    assert_eq!(fake.writes(), 0);
}

#[test]
fn every_calendar_hidden_is_not_the_same_as_nothing_on() {
    let fake = Arc::new(Fake::default());
    all_hidden(&fake);
    *fake.events.lock().unwrap() = a_day();
    let mut app = open_with(fake, 1000.0, 600.0);
    app.wait_for_within(
        "text:Every calendar is hidden. Turn one on to see its events.",
        WAIT,
    )
    .expect("it says so");
    assert!(!app.screen().shows("Nothing this week."));
}

/// The line sits in the flow above the grid, and wraps: nothing it says may run
/// off the screen, and a screen too narrow for New offers its key.
#[test]
fn the_empty_line_stays_whole_at_every_width() {
    let fake = Arc::new(Fake::default());
    let (mut app, filter) = audited(&fake, 1000.0, 600.0);
    app.wait_for_within("label:Today", WAIT).expect("it shows");
    settle(&mut app);
    filter.assert_inside("Nothing this week. Press New to add an event.", 1000.0);
    for width in WIDTHS {
        app.set_viewport(size(px(width), px(600.0)))
            .expect("the viewport sets");
        settle(&mut app);
        filter.assert_inside("Nothing this week. Press c to add an event.", width);
    }
}

// --- What the AI can reach, and what labels hold ------------------------------

/// What the Bot's filter names on any Calendar screen, and why each is safe.
/// Everything else, and every field of the editor, stays unnamed: a new control
/// has to be added here, and so has to say why.
const MAY_REACH: [&str; 12] = [
    // move the view: no press sends anything by what's shown
    "Day",
    "Week",
    "Month",
    "List",
    "Today",
    // a search with no way to submit it, and a date the same
    "Search events",
    "Go to date",
    // closes the editor, and sends nothing
    "Cancel",
    // each calls its write in the press, which the Bot refuses the AI
    "This event",
    "All events",
    "Invite them",
    // takes back a question
    "Not yet",
];

const SECRETS: [&str; 12] = [
    "Team review",
    "Dentist",
    "Offsite",
    "Room Seven",
    "Bring the numbers",
    "ana@example.com",
    "ben@example.com",
    "Ana Reyes",
    "Weekly sync",
    "Their version",
    "Quarterly plan",
    "First up",
];

/// Every state of every Calendar screen and card, each handed to `look`.
fn every_state(mut look: impl FnMut(&Headless, &Filter, &str)) {
    let fake = Arc::new(Fake::default());
    *fake.events.lock().unwrap() = a_day();
    let (mut app, filter) = audited(&fake, 1000.0, 600.0);
    app.wait_for_within("text:Team review", WAIT)
        .expect("it shows");
    settle(&mut app);
    look(&app, &filter, "week");
    for (keys, name) in [("d", "day"), ("m", "month"), ("a", "list"), ("w", "week")] {
        key(&mut app, keys);
        look(&app, &filter, name);
    }
    key(&mut app, "g");
    look(&app, &filter, "go to date");
    key(&mut app, "escape");
    app.type_text("label:Search events", "Team").unwrap();
    app.submit("label:Search events").unwrap();
    settle(&mut app);
    look(&app, &filter, "search");
    app.clear("label:Search events").unwrap();
    app.submit("label:Search events").unwrap();
    settle(&mut app);

    open(&mut app, "Team review");
    assert!(app.screen().shows("ana@example.com"));
    look(&app, &filter, "an event");
    press(&mut app, "label:Open the Meet link");
    assert!(app.screen().matches("label:Open this link").unwrap());
    look(&app, &filter, "the link's question");
    press(&mut app, "label:Don't open this link");
    press(&mut app, "label:Delete this event");
    look(&app, &filter, "asked to delete");
    press(&mut app, "label:Cancel this");

    press(&mut app, "label:Edit this event");
    app.wait_for_within(&format!("label:{TITLE}"), WAIT)
        .expect("the editor opens");
    assert!(filter.unnamed_fields() >= 9, "the audit sees the fields");
    look(&app, &filter, "the editor");
    app.type_text("label:Guest's address, like name@company.com", "an")
        .expect("it types");
    app.wait_for_within("label:Use this address", WAIT)
        .expect("it suggests");
    look(&app, &filter, "suggestions");
    press(&mut app, "label:Use this address");
    fill(&mut app, TITLE, "My version");
    let mut newer = fake.row("e1").unwrap();
    newer["title"] = json!("Their version");
    newer["etag"] = json!("e2");
    *fake.changed.lock().unwrap() = Some(newer);
    press(&mut app, "label:Save this event");
    assert!(app.screen().shows("Their version"));
    look(&app, &filter, "someone else's version");
    press(&mut app, "label:Cancel");
    settle(&mut app);
    look(&app, &filter, "told something");

    let (mut app, filter) = {
        let fake = Arc::new(Fake::default());
        *fake.events.lock().unwrap() = a_day();
        audited(&fake, 1000.0, 600.0)
    };
    app.wait_for_within("text:Dentist", WAIT).expect("it shows");
    settle(&mut app);
    open(&mut app, "Dentist");
    press(&mut app, "label:Delete this event");
    press(&mut app, "label:Delete it");
    assert!(app.screen().matches("label:Undo that").unwrap());
    look(&app, &filter, "undo");

    let fake = Arc::new(Fake::default());
    *fake.events.lock().unwrap() = a_series().into_iter().map(invite).collect();
    let (mut app, filter) = audited(&fake, 1000.0, 600.0);
    app.wait_for_within("text:Weekly sync", WAIT)
        .expect("it shows");
    settle(&mut app);
    open(&mut app, "Weekly sync");
    assert!(app.screen().matches("label:Answer yes").unwrap());
    look(&app, &filter, "an invite in a series");
    press(&mut app, "label:Answer no");
    assert!(app.screen().shows("Reply to which events?"));
    look(&app, &filter, "a series' question");

    let fake = Arc::new(Fake::default());
    let (mut app, filter) = audited(&fake, 1000.0, 600.0);
    app.wait_for_within("label:Today", WAIT).expect("it shows");
    settle(&mut app);
    for (keys, name) in [
        ("w", "an empty week"),
        ("d", "an empty day"),
        ("m", "an empty month"),
        ("a", "an empty list"),
    ] {
        key(&mut app, keys);
        look(&app, &filter, name);
    }
    app.set_viewport(size(px(300.0), px(600.0)))
        .expect("the viewport sets");
    settle(&mut app);
    look(&app, &filter, "narrow and empty");

    for (set, name) in [
        (no_calendars as fn(&Fake), "no calendars"),
        (all_hidden, "every calendar hidden"),
    ] {
        let fake = Arc::new(Fake::default());
        set(&fake);
        let (mut app, filter) = audited(&fake, 1000.0, 600.0);
        app.wait_for_within("label:Today", WAIT).expect("it shows");
        settle(&mut app);
        look(&app, &filter, name);
        key(&mut app, "c");
        look(&app, &filter, "a new event");
    }

    let fake = Arc::new(Fake::default());
    *fake.status.lock().unwrap() = Some(json!({"signed_in": false}));
    let (mut app, filter) = audited(&fake, 1000.0, 600.0);
    settle(&mut app);
    look(&app, &filter, "signed out");

    let fake = Arc::new(Fake::default());
    *fake.inline.lock().unwrap() = true;
    *fake.props.lock().unwrap() = json!({"view": "next"});
    *fake.next.lock().unwrap() = next_answer();
    let (mut app, filter) = audited(&fake, 420.0, 300.0);
    app.wait_for_within("text:First up", WAIT)
        .expect("it shows");
    look(&app, &filter, "the next card");
    let fake = Arc::new(Fake::default());
    *fake.inline.lock().unwrap() = true;
    *fake.props.lock().unwrap() = json!({"view": "next"});
    *fake.next.lock().unwrap() = json!({"event": null, "after": []});
    let (mut app, filter) = audited(&fake, 420.0, 300.0);
    app.wait_for_within("text:Nothing coming up.", WAIT)
        .expect("it shows");
    look(&app, &filter, "an empty next card");

    let fake = Arc::new(Fake::default());
    *fake.inline.lock().unwrap() = true;
    *fake.props.lock().unwrap() = json!({"draft": "tk1"});
    *fake.draft.lock().unwrap() = a_draft(7);
    let (mut app, filter) = audited(&fake, 420.0, 420.0);
    app.wait_for_within("label:Keep draft", WAIT)
        .expect("it shows");
    look(&app, &filter, "a draft card");
    press(&mut app, "label:Save this event");
    assert!(app.screen().matches("label:Invite them").unwrap());
    look(&app, &filter, "a draft card that asks");
}

const TITLE: &str = "Title, the name guests will see on their calendars";

#[test]
fn no_label_in_any_state_holds_a_persons_data() {
    let mut states = 0;
    every_state(|_, filter, at| {
        filter.assert_no_data_in_labels(&SECRETS, at);
        states += 1;
    });
    assert!(states >= 25, "{states}");
}

/// The AI's only way to an event is `gws_event_draft`, shown on a card for the
/// person to read. Nothing on a screen may be another way.
#[test]
fn nothing_a_press_would_send_can_be_reached() {
    bot_filter::assert_the_copy_works();
    let mut reached = BTreeSet::new();
    every_state(|_, filter, at| {
        filter.assert_stages_nothing(&MAY_REACH, at);
        reached.extend(filter.reachable().into_iter().map(|(_, name)| name));
    });
    // or the audit is looking at nothing
    assert!(reached.contains("Week"), "{reached:?}");
    assert!(reached.contains("Search events"), "{reached:?}");
}

// --- Date maths --------------------------------------------------------------

fn date(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).unwrap()
}

fn timed(start: &str, end: &str) -> When {
    When::of(start, end, false).unwrap()
}

#[test]
fn a_moment_falls_on_the_day_it_is_in_each_zone() {
    // 23:30 UTC on Nov 5: evening in New York, already morning in Manila
    let when = timed("2026-11-05T23:30:00Z", "2026-11-06T00:30:00Z");
    assert_eq!(
        time::slice(&when, date(2026, 11, 5), &New_York),
        Some((18 * 60 + 30, 19 * 60 + 30))
    );
    assert_eq!(time::slice(&when, date(2026, 11, 6), &New_York), None);
    assert_eq!(
        time::slice(&when, date(2026, 11, 6), &Manila),
        Some((7 * 60 + 30, 8 * 60 + 30))
    );
    assert_eq!(time::slice(&when, date(2026, 11, 5), &Manila), None);
    assert_eq!(
        time::when_line(&when, &New_York),
        "Thu Nov 5, 18:30 - 19:30"
    );
    assert_eq!(time::when_line(&when, &Manila), "Fri Nov 6, 07:30 - 08:30");
}

#[test]
fn the_grid_reads_the_wall_clock_on_a_day_the_clocks_change() {
    // New York springs forward on Mar 8, 2026: 01:30 EST to 03:30 EDT is one hour
    let spring = timed("2026-03-08T01:30:00-05:00", "2026-03-08T03:30:00-04:00");
    assert_eq!(
        time::slice(&spring, date(2026, 3, 8), &New_York),
        Some((90, 210))
    );
    // and falls back on Nov 1: 09:00 is 09:00 on either side of it
    let fall = timed("2026-11-01T14:00:00Z", "2026-11-01T15:00:00Z");
    assert_eq!(
        time::slice(&fall, date(2026, 11, 1), &New_York),
        Some((9 * 60, 10 * 60))
    );
    // the same moment a week before is an hour later on the clock
    let before = timed("2026-10-25T14:00:00Z", "2026-10-25T15:00:00Z");
    assert_eq!(
        time::slice(&before, date(2026, 10, 25), &New_York),
        Some((10 * 60, 11 * 60))
    );
    // Manila has no change: the same moment stays at 22:00
    assert_eq!(
        time::slice(&fall, date(2026, 11, 1), &Manila),
        Some((22 * 60, 23 * 60))
    );
    assert_eq!(
        time::slice(&before, date(2026, 10, 25), &Manila),
        Some((22 * 60, 23 * 60))
    );
}

#[test]
fn an_hour_the_clocks_repeat_never_runs_backwards() {
    // 01:50 EDT to 01:10 EST: forty minutes that read as going back
    let when = timed("2026-11-01T01:50:00-04:00", "2026-11-01T01:10:00-05:00");
    let (from, to) = time::slice(&when, date(2026, 11, 1), &New_York).unwrap();
    assert!(from <= to, "{from} to {to}");
}

#[test]
fn a_time_the_clocks_skip_isnt_a_moment_and_a_repeated_one_is_the_first() {
    let clock = |h, m| chrono::NaiveTime::from_hms_opt(h, m, 0).unwrap();
    assert_eq!(
        time::moment(date(2026, 3, 8), clock(2, 30), &New_York),
        None
    );
    assert_eq!(
        time::moment(date(2026, 3, 8), clock(2, 30), &Manila).as_deref(),
        Some("2026-03-08T02:30:00+08:00")
    );
    assert_eq!(
        time::moment(date(2026, 11, 1), clock(1, 30), &New_York).as_deref(),
        Some("2026-11-01T01:30:00-04:00")
    );
    // each side of a change gets its own offset
    assert_eq!(
        time::moment(date(2026, 3, 7), clock(12, 0), &New_York).as_deref(),
        Some("2026-03-07T12:00:00-05:00")
    );
    assert_eq!(
        time::moment(date(2026, 3, 9), clock(12, 0), &New_York).as_deref(),
        Some("2026-03-09T12:00:00-04:00")
    );
}

#[test]
fn a_days_range_asks_for_its_own_midnights_in_each_zone() {
    let (first, end) = time::range(Span::Day, date(2026, 3, 8));
    assert_eq!(
        time::range_args(first, end, &New_York),
        Some((
            "2026-03-08T00:00:00-05:00".to_owned(),
            "2026-03-09T00:00:00-04:00".to_owned()
        ))
    );
    assert_eq!(
        time::range_args(first, end, &Manila),
        Some((
            "2026-03-08T00:00:00+08:00".to_owned(),
            "2026-03-09T00:00:00+08:00".to_owned()
        ))
    );
    // Havana's clocks change at midnight, so this day starts at 01:00
    assert_eq!(
        time::day_start(date(2026, 3, 8), &Havana)
            .map(|d| d.to_rfc3339())
            .as_deref(),
        Some("2026-03-08T01:00:00-04:00")
    );
}

#[test]
fn an_all_day_event_is_its_dates_in_any_zone_and_its_end_is_exclusive() {
    let when = When::of("2026-11-06", "2026-11-08", true).unwrap();
    for zone_says in [
        [true, true, false, false],
        [
            when.touches(date(2026, 11, 6), &New_York),
            when.touches(date(2026, 11, 7), &New_York),
            when.touches(date(2026, 11, 8), &New_York),
            when.touches(date(2026, 11, 5), &New_York),
        ],
        [
            when.touches(date(2026, 11, 6), &Manila),
            when.touches(date(2026, 11, 7), &Manila),
            when.touches(date(2026, 11, 8), &Manila),
            when.touches(date(2026, 11, 5), &Manila),
        ],
    ] {
        assert_eq!(zone_says, [true, true, false, false]);
    }
    assert_eq!(
        time::when_line(&when, &New_York),
        "Fri Nov 6 - Sat Nov 7, all day"
    );
    assert_eq!(
        time::when_line(&when, &Manila),
        "Fri Nov 6 - Sat Nov 7, all day"
    );
    let one = When::of("2026-11-06", "2026-11-07", true).unwrap();
    assert_eq!(time::when_line(&one, &Manila), "Fri Nov 6, all day");
    assert_eq!(time::last_day(date(2026, 11, 8)), date(2026, 11, 7));
    assert_eq!(time::exclusive_end(date(2026, 12, 31)), date(2027, 1, 1));
    // across the clocks changing it's still two days
    let change = When::of("2026-03-07", "2026-03-09", true).unwrap();
    assert!(change.touches(date(2026, 3, 8), &New_York));
    assert!(!change.touches(date(2026, 3, 9), &New_York));
}

#[test]
fn an_event_ending_at_midnight_isnt_on_the_next_day() {
    let when = timed("2026-11-06T22:00:00+08:00", "2026-11-07T00:00:00+08:00");
    assert_eq!(
        time::slice(&when, date(2026, 11, 6), &Manila),
        Some((22 * 60, 1440))
    );
    assert!(!when.touches(date(2026, 11, 7), &Manila));
    // over midnight, it's on both, cut at the day's edge
    let late = timed("2026-11-06T23:00:00+08:00", "2026-11-07T01:00:00+08:00");
    assert_eq!(
        time::slice(&late, date(2026, 11, 6), &Manila),
        Some((23 * 60, 1440))
    );
    assert_eq!(
        time::slice(&late, date(2026, 11, 7), &Manila),
        Some((0, 60))
    );
}

#[test]
fn a_long_timed_event_sits_with_the_all_day_ones_in_any_zone() {
    let long = timed("2026-11-06T10:00:00+08:00", "2026-11-07T11:00:00+08:00");
    let short = timed("2026-11-06T10:00:00+08:00", "2026-11-06T11:00:00+08:00");
    for short_and_long in [
        (short.in_all_day_row(&Manila), long.in_all_day_row(&Manila)),
        (
            short.in_all_day_row(&New_York),
            long.in_all_day_row(&New_York),
        ),
    ] {
        assert_eq!(short_and_long, (false, true));
    }
}

#[test]
fn weeks_months_and_steps_land_on_the_right_days() {
    assert_eq!(time::week_start(date(2026, 11, 6)), date(2026, 11, 1));
    assert_eq!(
        time::week_start(date(2026, 11, 1)),
        date(2026, 11, 1),
        "a Sunday"
    );
    assert_eq!(
        time::range(Span::Week, date(2026, 11, 6)),
        (date(2026, 11, 1), date(2026, 11, 8))
    );
    let month = time::month_days(date(2026, 2, 14));
    assert_eq!(
        month.first(),
        Some(&date(2026, 2, 1)),
        "Feb 2026 starts on a Sunday"
    );
    assert_eq!(month.len(), 28, "and fills four weeks exactly");
    let month = time::month_days(date(2026, 8, 1));
    assert_eq!(month.first(), Some(&date(2026, 7, 26)));
    assert_eq!(month.len(), 42);
    assert_eq!(
        time::step(Span::Month, date(2026, 1, 31), true),
        date(2026, 2, 28)
    );
    assert_eq!(
        time::step(Span::Month, date(2026, 3, 31), false),
        date(2026, 2, 28)
    );
    assert_eq!(
        time::step(Span::Week, date(2026, 12, 28), true),
        date(2027, 1, 4)
    );
    assert_eq!(
        time::step(Span::Day, date(2026, 3, 1), false),
        date(2026, 2, 28)
    );
}

#[test]
fn range_titles_read_like_the_sketch() {
    assert_eq!(
        time::range_title(Span::Week, date(2026, 11, 6)),
        "Nov 1-7, 2026"
    );
    assert_eq!(
        time::range_title(Span::Week, date(2026, 12, 2)),
        "Nov 29 - Dec 5, 2026"
    );
    assert_eq!(
        time::range_title(Span::Week, date(2026, 12, 31)),
        "Dec 27, 2026 - Jan 2, 2027"
    );
    assert_eq!(
        time::range_title(Span::Month, date(2026, 11, 6)),
        "November 2026"
    );
    assert_eq!(
        time::range_title(Span::Day, date(2026, 11, 6)),
        "Fri Nov 6, 2026"
    );
}

#[test]
fn overlapping_events_sit_side_by_side_and_others_keep_the_whole_width() {
    // a and b overlap; c fits under a, beside b; d stands alone
    let spans = [(540, 600), (570, 660), (600, 630), (700, 760)];
    assert_eq!(time::lanes(&spans), [(0, 2), (1, 2), (0, 2), (0, 1)]);
    // three at once, given out of order
    assert_eq!(
        time::lanes(&[(60, 120), (0, 120), (30, 120)]),
        [(2, 3), (0, 3), (1, 3)]
    );
    // one that ends as the next starts doesn't overlap it
    assert_eq!(time::lanes(&[(0, 60), (60, 120)]), [(0, 1), (0, 1)]);
    assert_eq!(time::lanes(&[]), []);
}

#[test]
fn a_new_event_starts_on_the_next_hour_today_and_at_nine_on_another_day() {
    let now = date(2026, 11, 6).and_hms_opt(13, 20, 0).unwrap();
    let (start, end) = time::new_slot(date(2026, 11, 6), now);
    assert_eq!(start, date(2026, 11, 6).and_hms_opt(14, 0, 0).unwrap());
    assert_eq!(end, date(2026, 11, 6).and_hms_opt(15, 0, 0).unwrap());
    let (start, _) = time::new_slot(date(2026, 11, 9), now);
    assert_eq!(start, date(2026, 11, 9).and_hms_opt(9, 0, 0).unwrap());
    let late = date(2026, 11, 6).and_hms_opt(23, 30, 0).unwrap();
    let (start, end) = time::new_slot(date(2026, 11, 6), late);
    assert_eq!(start, date(2026, 11, 7).and_hms_opt(0, 0, 0).unwrap());
    assert_eq!(end, date(2026, 11, 7).and_hms_opt(1, 0, 0).unwrap());
}

#[test]
fn a_new_event_names_its_account() {
    let (mut app, fake) = start();
    *fake.status.lock().unwrap() = Some(
        json!({"signed_in": true, "subject": "one@example.com", "features": {"mail": true, "calendar": true, "drive": false}}),
    );
    let asked = fake.called("gws_status").len();
    let by = std::time::Instant::now() + Duration::from_secs(20);
    while fake.called("gws_status").len() == asked {
        assert!(std::time::Instant::now() < by, "the check comes round");
        settle(&mut app);
    }
    settle(&mut app);
    write_one(&mut app, "Board dinner");
    press(&mut app, "label:Save this event");
    assert_eq!(
        last(&fake, "gws_event_save")["as_account"],
        "one@example.com"
    );
}

/// `fake` says `who` is active, and the screen's next check has read it.
fn active(app: &mut Headless, fake: &Fake, who: &str) {
    *fake.status.lock().unwrap() = Some(
        json!({"signed_in": true, "subject": who, "features": {"mail": true, "calendar": true, "drive": false}}),
    );
    let asked = fake.called("gws_status").len();
    let by = std::time::Instant::now() + Duration::from_secs(20);
    while fake.called("gws_status").len() == asked {
        assert!(std::time::Instant::now() < by, "the check comes round");
        settle(app);
    }
    settle(app);
}

/// Its calendar and its guests are that account's: the plugin refuses it there
/// only if the save still says whose it is.
#[test]
fn an_event_opened_before_a_switch_stays_the_account_it_was_opened_on() {
    let (mut app, fake) = start();
    active(&mut app, &fake, "one@example.com");
    write_one(&mut app, "Board dinner");
    active(&mut app, &fake, "two@example.com");
    press(&mut app, "label:Save this event");
    assert_eq!(
        last(&fake, "gws_event_save")["as_account"],
        "one@example.com"
    );
}

#[test]
fn the_newer_version_of_an_event_stays_the_account_it_was_opened_on() {
    let (mut app, fake) = start();
    active(&mut app, &fake, "one@example.com");
    open(&mut app, "Team review");
    press(&mut app, "label:Edit this event");
    app.wait_for_within(
        "label:Title, the name guests will see on their calendars",
        WAIT,
    )
    .expect("the editor opens");
    let mut newer = fake.row("e1").unwrap();
    newer["etag"] = json!("e2");
    *fake.changed.lock().unwrap() = Some(newer);
    press(&mut app, "label:Save this event");
    active(&mut app, &fake, "two@example.com");
    press(&mut app, "label:Use the newer one");
    press(&mut app, "label:Save this event");
    assert_eq!(
        last(&fake, "gws_event_save")["as_account"],
        "one@example.com"
    );
}

#[test]
fn a_draft_card_saves_to_the_account_it_was_made_on() {
    let mut draft = a_draft(1);
    draft["as_account"] = json!("one@example.com");
    let (mut app, fake) = draft_card(draft, 420.0);
    press(&mut app, "label:Save this event");
    assert_eq!(
        last(&fake, "gws_event_save")["as_account"],
        "one@example.com"
    );
}
