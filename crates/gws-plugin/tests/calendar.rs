//! Calendar against the stub Google: what the person presses is what reaches
//! Google, nothing goes to a guest without it, and a range holds the same
//! events whatever zone asks. This proves the plugin against the stub, which
//! is written from what's known of Google's API, not against Google.

use chrono::{Datelike as _, Duration, NaiveDate, Utc, Weekday};
use gws::Core;
use gws::apis::Apis;
use gws_fake_google::{FAKE_EMAIL, FakeCalendar, FakeGoogle, Seed};
use serde_json::{Value, json};

const TEAM: &str = "team@group.example.com";
const NY: &str = "America/New_York";
const ROOM: &str = "room@example.com";

fn signed_in_at(base: &str, name: &str, fresh: bool) -> Core {
    let dir = std::env::temp_dir().join(format!("gws-calendar-{name}-{}", std::process::id()));
    if fresh {
        let _ = std::fs::remove_dir_all(&dir);
    }
    let core = Core::new(&Apis::at(base), Ok(gws_store::Paths::in_dir(dir)));
    core.unlock("seed");
    core.accounts.set(&json!({"account": "google", "label": "default", "provider": "google", "subject": FAKE_EMAIL, "scopes": ["https://www.googleapis.com/auth/calendar.events"], "access_token": "fake-access-1", "token_type": "Bearer", "expires_at": 4_000_000_000u64}));
    core
}

fn signed_in(google: &FakeGoogle, name: &str) -> Core {
    signed_in_at(&google.base(), name, true)
}

/// The owner's calendar and a team's, both empty.
fn account() -> FakeGoogle {
    FakeGoogle::start(Seed {
        calendars: vec![
            FakeCalendar::new(FAKE_EMAIL, "Owner", true, NY),
            FakeCalendar::new(TEAM, "Team", false, NY),
        ],
        ..Seed::default()
    })
}

fn data(answer: Value) -> Value {
    assert_ne!(answer["isError"], true, "{answer}");
    assert_eq!(answer["structuredContent"].get("refused"), None, "{answer}");
    answer["structuredContent"].clone()
}

fn refusal(answer: &Value) -> &str {
    answer["structuredContent"]["refused"]
        .as_str()
        .unwrap_or_else(|| panic!("not refused: {answer}"))
}

/// A date `n` days from today, always inside what a first sync reads.
fn day(n: i64) -> NaiveDate {
    Utc::now().date_naive() + Duration::days(n)
}

/// `time` on that day at `zone`.
fn at(n: i64, time: &str, zone: &str) -> String {
    format!("{}T{time}{zone}", day(n))
}

fn meeting(n: i64) -> Value {
    json!({"calendar": FAKE_EMAIL, "title": "Roof call", "all_day": false, "start": at(n, "09:00:00", "-05:00"), "end": at(n, "10:00:00", "-05:00"), "time_zone": NY, "repeat": "", "guests": [], "where": "", "notes": "", "meet": false})
}

fn with(mut args: Value, change: Value) -> Value {
    for (k, v) in change.as_object().unwrap() {
        args[k] = v.clone();
    }
    args
}

fn saved_event(core: &Core, args: Value) -> Value {
    data(core.run("gws_event_save", &args))["event"].clone()
}

/// What the person sees in `from..to`.
fn range(core: &Core, from: &str, to: &str) -> Vec<Value> {
    let got = data(core.run("gws_cal_range", &json!({"from": from, "to": to})));
    got["events"].as_array().unwrap().clone()
}

fn titles(events: &[Value]) -> Vec<&str> {
    events
        .iter()
        .map(|e| e["title"].as_str().unwrap())
        .collect()
}

fn starts(events: &[Value]) -> Vec<&str> {
    events
        .iter()
        .map(|e| e["start"].as_str().unwrap())
        .collect()
}

/// `id` as `gws_event_save` takes it back from the editor: what was read.
fn form_of(core: &Core, calendar: &str, id: &str) -> Value {
    let e = data(core.run("gws_event_read", &json!({"calendar": calendar, "id": id})));
    let guests: Vec<Value> = e["attendees"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|a| a["self"] != true)
        .map(|a| a["email"].clone())
        .collect();
    json!({"calendar": calendar, "id": id, "etag": e["etag"], "title": e["title"], "all_day": e["all_day"], "start": e["start"], "end": e["end"], "time_zone": e["time_zone"], "repeat": e["repeat"], "guests": guests, "where": e["where"], "notes": e["notes"], "meet": e["meet"]})
}

fn sync(core: &Core) {
    gws::mail::sync(core).unwrap();
}

/// A weekly series of four from `n` days on, at 09:00 in New York.
fn series(core: &Core, n: i64) -> Vec<Value> {
    saved_event(
        core,
        with(
            meeting(n),
            json!({"title": "Standup", "repeat": "RRULE:FREQ=WEEKLY;COUNT=4"}),
        ),
    );
    range(
        core,
        &at(n - 1, "00:00:00", "Z"),
        &at(n + 40, "00:00:00", "Z"),
    )
}

mod attendee_changes {
    use super::*;

    #[test]
    fn with_no_guests_nobody_hears_and_with_guests_they_all_do() {
        let google = account();
        let core = signed_in(&google, "updates");
        let quiet = saved_event(&core, meeting(10));
        let invited = saved_event(
            &core,
            with(
                meeting(11),
                json!({"guests": ["ana@example.com", "ben@example.com"]}),
            ),
        );
        let id = |e: &Value| e["id"].as_str().unwrap().to_owned();
        saved_event(
            &core,
            with(
                form_of(&core, FAKE_EMAIL, &id(&quiet)),
                json!({"title": "Quiet edit"}),
            ),
        );
        saved_event(
            &core,
            with(
                form_of(&core, FAKE_EMAIL, &id(&invited)),
                json!({"title": "Loud edit"}),
            ),
        );
        let del = |e: &Value| json!({"calendar": FAKE_EMAIL, "id": e["id"]});
        data(core.run("gws_event_delete", &del(&quiet)));
        data(core.run("gws_event_delete", &del(&invited)));
        let sent: Vec<(String, Option<String>)> = google
            .calendar_writes()
            .into_iter()
            .map(|w| (w.method, w.send_updates))
            .collect();
        let want = |method: &str, updates: &str| (method.to_owned(), Some(updates.to_owned()));
        assert_eq!(
            sent,
            [
                want("POST", "none"),
                want("POST", "all"),
                want("PATCH", "none"),
                want("PATCH", "all"),
                want("DELETE", "none"),
                want("DELETE", "all"),
            ]
        );
    }

    #[test]
    fn a_guest_added_later_is_invited_and_those_there_keep_their_answers() {
        let google = account();
        let core = signed_in(&google, "added");
        let e = saved_event(
            &core,
            with(meeting(10), json!({"guests": ["ana@example.com"]})),
        );
        let id = e["id"].as_str().unwrap();
        google.edit_event(
            FAKE_EMAIL,
            id,
            &json!({"attendees": [{"email": "ana@example.com", "responseStatus": "accepted"}]}),
        );
        sync(&core);
        saved_event(
            &core,
            with(
                form_of(&core, FAKE_EMAIL, id),
                json!({"guests": ["ana@example.com", "ben@example.com"]}),
            ),
        );
        let write = google.calendar_writes().pop().unwrap();
        assert_eq!(write.send_updates.as_deref(), Some("all"));
        let held = google.event(FAKE_EMAIL, id).unwrap();
        assert_eq!(held["attendees"][0]["responseStatus"], "accepted");
        assert_eq!(held["attendees"][1]["email"], "ben@example.com");
    }

    #[test]
    fn the_last_guest_taken_off_is_told() {
        let google = account();
        let core = signed_in(&google, "removed");
        let e = saved_event(
            &core,
            with(meeting(10), json!({"guests": ["ana@example.com"]})),
        );
        let id = e["id"].as_str().unwrap();
        saved_event(
            &core,
            with(form_of(&core, FAKE_EMAIL, id), json!({"guests": []})),
        );
        let write = google.calendar_writes().pop().unwrap();
        assert_eq!(write.send_updates.as_deref(), Some("all"));
    }

    #[test]
    fn the_person_is_no_guest_of_their_own_event() {
        let google = account();
        let core = signed_in(&google, "own");
        saved_event(&core, with(meeting(10), json!({"guests": [FAKE_EMAIL]})));
        let write = google.calendar_writes().pop().unwrap();
        assert_eq!(write.send_updates.as_deref(), Some("none"));
        assert_eq!(write.body.get("attendees"), None);
    }

    /// A person signed in to an event with a room on it, as Google holds it.
    fn with_a_room(google: &FakeGoogle, name: &str, guests: &[&str]) -> Core {
        let mut people = vec![
            json!({"email": FAKE_EMAIL, "self": true, "organizer": true, "responseStatus": "accepted"}),
            json!({"email": ROOM, "resource": true, "responseStatus": "accepted"}),
        ];
        people.extend(
            guests
                .iter()
                .map(|g| json!({"email": g, "responseStatus": "accepted"})),
        );
        google.add_event(
            FAKE_EMAIL,
            json!({"id": "booked", "summary": "Roof call", "organizer": {"email": FAKE_EMAIL, "self": true}, "start": {"dateTime": at(10, "09:00:00", "Z")}, "end": {"dateTime": at(10, "10:00:00", "Z")}, "attendees": people}),
        );
        let core = signed_in(google, name);
        sync(&core);
        core
    }

    #[test]
    fn a_room_read_back_on_a_form_changes_nothing_and_tells_nobody() {
        let google = account();
        let core = with_a_room(&google, "room-same", &["ana@example.com"]);
        let read = data(core.run(
            "gws_event_read",
            &json!({"calendar": FAKE_EMAIL, "id": "booked"}),
        ));
        let room = read["attendees"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["email"] == ROOM);
        assert_eq!(
            room.unwrap()["resource"],
            true,
            "the screen is told what it is"
        );
        let writes = google.calendar_writes().len();
        saved_event(&core, form_of(&core, FAKE_EMAIL, "booked"));
        assert_eq!(google.calendar_writes().len(), writes, "nothing to write");
    }

    #[test]
    fn a_guest_added_beside_a_room_leaves_one_room_marked_as_a_room() {
        let google = account();
        let core = with_a_room(&google, "room-add", &["ana@example.com"]);
        let form = form_of(&core, FAKE_EMAIL, "booked");
        let guests = json!(["ana@example.com", ROOM, "ben@example.com"]);
        saved_event(&core, with(form, json!({"guests": guests})));
        let write = google.calendar_writes().pop().unwrap();
        let list = write.body["attendees"].as_array().unwrap();
        let rooms: Vec<&Value> = list.iter().filter(|a| a["email"] == ROOM).collect();
        assert_eq!(rooms.len(), 1, "{list:?}");
        assert_eq!(rooms[0]["resource"], true);
        assert_eq!(write.send_updates.as_deref(), Some("all"));
    }

    #[test]
    fn with_only_a_room_on_it_a_change_tells_nobody() {
        let google = account();
        let core = with_a_room(&google, "room-alone", &[]);
        let form = form_of(&core, FAKE_EMAIL, "booked");
        saved_event(&core, with(form, json!({"title": "Moved on"})));
        let write = google.calendar_writes().pop().unwrap();
        assert_eq!(write.send_updates.as_deref(), Some("none"));
        assert!(write.body.get("attendees").is_none(), "{}", write.body);
    }

    #[test]
    fn a_save_with_nothing_changed_writes_nothing_and_tells_nobody() {
        let google = account();
        let core = signed_in(&google, "noop");
        let e = saved_event(
            &core,
            with(
                meeting(10),
                json!({"guests": ["ana@example.com"], "notes": "a\nb"}),
            ),
        );
        let before = google.calendar_writes().len();
        saved_event(&core, form_of(&core, FAKE_EMAIL, e["id"].as_str().unwrap()));
        assert_eq!(google.calendar_writes().len(), before);
    }

    #[test]
    fn a_draft_is_local_and_only_a_press_of_save_writes() {
        let google = account();
        let core = signed_in(&google, "draft");
        let args = json!({"title": "Lunch", "start": at(5, "12:00:00", "+08:00"), "end": at(5, "13:00:00", "+08:00"), "guests": ["ana@example.com"], "where": "Cafe", "meet": true});
        let answer = core.run_for_chat("gws_event_draft", &args);
        let ticket = data(answer.clone())["ticket"].as_str().unwrap().to_owned();
        assert_eq!(
            answer["_meta"]["buwiz/open"],
            json!({"app": "calendar", "props": {"draft": ticket}, "title": "Calendar", "placement": "inline"})
        );
        assert!(
            !answer["_meta"].to_string().contains("ana@example.com"),
            "props hold a ticket, not the event"
        );
        assert_eq!(google.log(), [], "a draft calls no Google");

        let draft = data(core.run("gws_event_draft_read", &json!({"ticket": ticket})));
        assert_eq!(draft["title"], "Lunch");
        assert_eq!(draft["guests"], json!(["ana@example.com"]));
        assert_eq!(draft["where"], "Cafe");
        assert_eq!(draft["meet"], true);
        assert_eq!(google.log(), [], "nor does reading one");
        assert_eq!(
            refusal(&core.run("gws_event_draft_read", &json!({"ticket": "tkt_nope"}))),
            "not_found"
        );

        // the card's Save: only now does anyone hear
        let mut press = draft.clone();
        press["calendar"] = json!(FAKE_EMAIL);
        saved_event(&core, press);
        let writes = google.calendar_writes();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].send_updates.as_deref(), Some("all"));
    }

    #[test]
    fn a_drafts_card_is_titled_calendar_and_says_nothing_of_the_event() {
        use buwiz_plugin::Plugin as _;
        let google = account();
        let core = std::sync::Arc::new(signed_in(&google, "title"));
        let args = json!({"title": "Secret plan", "start": at(5, "12:00:00", "Z"), "end": at(5, "13:00:00", "Z")});
        let ticket = data(core.run("gws_event_draft", &args))["ticket"].clone();
        let mut plugin = gws::Gws { core };
        assert_eq!(
            plugin.ticket_title(ticket.as_str().unwrap()).as_deref(),
            Some("Calendar")
        );
    }

    #[test]
    fn a_draft_that_makes_no_sense_is_refused_and_a_routine_gets_no_card() {
        let google = account();
        let core = signed_in(&google, "baddraft");
        let bad = core.run_for_chat(
            "gws_event_draft",
            &json!({"title": "x", "start": "soon", "end": "later"}),
        );
        assert_eq!(bad["isError"], true, "{bad}");
        assert_eq!(bad.get("_meta"), None);
        let args =
            json!({"title": "x", "start": at(5, "12:00:00", "Z"), "end": at(5, "13:00:00", "Z")});
        assert_eq!(
            core.run_for_model("gws_event_draft", &args).get("_meta"),
            None
        );
    }
}

mod recurring {
    use super::*;

    #[test]
    fn this_changes_that_instance_and_no_other() {
        let google = account();
        let core = signed_in(&google, "this");
        let held = series(&core, 10);
        assert_eq!(held.len(), 4, "{held:?}");
        assert!(held.iter().all(|e| e["recurring"] == true));
        let id = held[1]["id"].as_str().unwrap();
        let form = form_of(&core, FAKE_EMAIL, id);
        assert_eq!(
            form["repeat"], "RRULE:FREQ=WEEKLY;COUNT=4",
            "the rule, from the series"
        );
        saved_event(
            &core,
            with(form, json!({"scope": "this", "title": "Moved standup"})),
        );
        let write = google.calendar_writes().pop().unwrap();
        assert_eq!(write.event, id, "to the instance, not the series");
        assert_eq!(write.body, json!({"summary": "Moved standup"}));
        let now = range(&core, &at(9, "00:00:00", "Z"), &at(50, "00:00:00", "Z"));
        assert_eq!(
            titles(&now),
            ["Standup", "Moved standup", "Standup", "Standup"]
        );
    }

    #[test]
    fn all_changes_the_series_and_an_edited_instance_keeps_its_edit() {
        let google = account();
        let core = signed_in(&google, "all");
        let held = series(&core, 10);
        let (one, three) = (
            held[1]["id"].as_str().unwrap(),
            held[3]["id"].as_str().unwrap(),
        );
        let master = {
            let e = data(core.run(
                "gws_event_read",
                &json!({"calendar": FAKE_EMAIL, "id": one}),
            ));
            e["series"].as_str().unwrap().to_owned()
        };
        saved_event(
            &core,
            with(
                form_of(&core, FAKE_EMAIL, one),
                json!({"scope": "this", "title": "Special"}),
            ),
        );
        saved_event(
            &core,
            with(
                form_of(&core, FAKE_EMAIL, three),
                json!({"scope": "all", "title": "Daily sync"}),
            ),
        );
        let write = google.calendar_writes().pop().unwrap();
        assert_eq!(write.event, master, "to the series");
        assert_eq!(
            write.body,
            json!({"summary": "Daily sync"}),
            "times untouched"
        );
        let now = range(&core, &at(9, "00:00:00", "Z"), &at(50, "00:00:00", "Z"));
        assert_eq!(
            titles(&now),
            ["Daily sync", "Special", "Daily sync", "Daily sync"]
        );
    }

    /// The series keeps its first date: writing the instance's times to it
    /// would start it over there and lose what came before.
    #[test]
    fn moving_all_from_a_later_instance_shifts_the_time_and_keeps_the_first_date() {
        let google = account();
        let core = signed_in(&google, "shift");
        let held = series(&core, 10);
        let id = held[2]["id"].as_str().unwrap();
        let form = form_of(&core, FAKE_EMAIL, id);
        let later = |key: &str| {
            let t = chrono::DateTime::parse_from_rfc3339(form[key].as_str().unwrap()).unwrap();
            (t + Duration::hours(1)).to_rfc3339()
        };
        let moved = json!({"scope": "all", "start": later("start"), "end": later("end")});
        saved_event(&core, with(form.clone(), moved));
        let now = range(&core, &at(9, "00:00:00", "Z"), &at(50, "00:00:00", "Z"));
        assert_eq!(now.len(), 4, "{now:?}");
        let was: Vec<&str> = starts(&held);
        for (new, old) in starts(&now).iter().zip(was) {
            let (new, old) = (
                chrono::DateTime::parse_from_rfc3339(new).unwrap(),
                chrono::DateTime::parse_from_rfc3339(old).unwrap(),
            );
            assert_eq!(new - old, Duration::hours(1), "{new} from {old}");
        }
    }

    #[test]
    fn an_instance_has_to_say_which_it_means() {
        let google = account();
        let core = signed_in(&google, "scope");
        let held = series(&core, 10);
        let id = held[0]["id"].as_str().unwrap();
        let before = google.calendar_writes().len();
        let save = with(form_of(&core, FAKE_EMAIL, id), json!({"title": "x"}));
        assert_eq!(refusal(&core.run("gws_event_save", &save)), "invalid_args");
        let del = json!({"calendar": FAKE_EMAIL, "id": id});
        assert_eq!(refusal(&core.run("gws_event_delete", &del)), "invalid_args");
        assert_eq!(google.calendar_writes().len(), before);
    }

    #[test]
    fn deleting_this_leaves_the_rest_and_all_leaves_nothing() {
        let google = account();
        let core = signed_in(&google, "delete");
        let held = series(&core, 10);
        let one = json!({"calendar": FAKE_EMAIL, "id": held[1]["id"], "scope": "this"});
        data(core.run("gws_event_delete", &one));
        let (from, to) = (at(9, "00:00:00", "Z"), at(50, "00:00:00", "Z"));
        assert_eq!(range(&core, &from, &to).len(), 3);
        let all = json!({"calendar": FAKE_EMAIL, "id": held[0]["id"], "scope": "all"});
        data(core.run("gws_event_delete", &all));
        assert_eq!(range(&core, &from, &to), Vec::<Value>::new());
    }

    #[test]
    fn a_reply_to_one_instance_stays_with_it() {
        let google = account();
        let core = signed_in(&google, "rsvp-series");
        google.add_event(FAKE_EMAIL, json!({"id": "inviteseries", "summary": "Weekly", "organizer": {"email": "ana@example.com"}, "start": {"dateTime": at(10, "09:00:00", "-05:00"), "timeZone": NY}, "end": {"dateTime": at(10, "10:00:00", "-05:00"), "timeZone": NY}, "recurrence": ["RRULE:FREQ=WEEKLY;COUNT=3"], "attendees": [{"email": "ana@example.com", "responseStatus": "accepted"}, {"email": FAKE_EMAIL, "responseStatus": "needsAction"}]}));
        let (from, to) = (at(9, "00:00:00", "Z"), at(40, "00:00:00", "Z"));
        let held = range(&core, &from, &to);
        data(core.run(
            "gws_event_rsvp",
            &json!({"calendar": FAKE_EMAIL, "id": held[0]["id"], "response": "declined"}),
        ));
        let responses = |events: Vec<Value>| -> Vec<String> {
            events
                .iter()
                .map(|e| e["response"].as_str().unwrap().to_owned())
                .collect()
        };
        assert_eq!(
            responses(range(&core, &from, &to)),
            ["declined", "needsAction", "needsAction"]
        );
        data(core.run("gws_event_rsvp", &json!({"calendar": FAKE_EMAIL, "id": held[1]["id"], "response": "accepted", "scope": "all"})));
        assert_eq!(
            responses(range(&core, &from, &to)),
            ["declined", "accepted", "accepted"]
        );
    }
}

mod zones {
    use super::*;

    fn monday_around(change: NaiveDate) -> NaiveDate {
        assert_eq!(change.weekday(), Weekday::Sun);
        change + Duration::days(1)
    }

    fn sunday_from(day: NaiveDate) -> NaiveDate {
        day + Duration::days(i64::from(7 - day.weekday().days_since(Weekday::Sun)) % 7)
    }

    /// The first Sunday of November, next year: inside any first sync.
    fn autumn_change() -> NaiveDate {
        sunday_from(NaiveDate::from_ymd_opt(Utc::now().year() + 1, 11, 1).unwrap())
    }

    /// The second Sunday of March, next year.
    fn spring_change() -> NaiveDate {
        sunday_from(NaiveDate::from_ymd_opt(Utc::now().year() + 1, 3, 8).unwrap())
    }

    /// A weekly 09:00 New York series across `change`, `before` and `after`
    /// being the offsets either side. Reads it back in full, then a range
    /// around the change, then the hour of the first instance after it.
    fn holds_nine_oclock(name: &str, change: NaiveDate, before: &str, after: &str) {
        let google = account();
        let core = signed_in(&google, name);
        let monday = monday_around(change);
        let first = monday - Duration::days(14);
        saved_event(
            &core,
            json!({"calendar": FAKE_EMAIL, "title": "Standup", "start": format!("{first}T09:00:00{before}"), "end": format!("{first}T10:00:00{before}"), "time_zone": NY, "repeat": "RRULE:FREQ=WEEKLY;COUNT=4", "guests": [], "where": "", "notes": ""}),
        );
        let days = |n: i64| monday + Duration::days(n);
        let all = range(
            &core,
            &format!("{}T00:00:00Z", days(-15)),
            &format!("{}T00:00:00Z", days(15)),
        );
        assert_eq!(
            starts(&all),
            [
                format!("{}T09:00:00{before}", days(-14)),
                format!("{}T09:00:00{before}", days(-7)),
                format!("{}T09:00:00{after}", days(0)),
                format!("{}T09:00:00{after}", days(7)),
            ]
        );
        // the same week around the change, asked in two zones: the same events
        let in_zone = |zone: &str| {
            let (from, to) = (days(-8), days(1));
            range(
                &core,
                &format!("{from}T00:00:00{zone}"),
                &format!("{to}T00:00:00{zone}"),
            )
        };
        for zone in ["-05:00", "-04:00"] {
            assert_eq!(
                starts(&in_zone(zone)),
                [
                    format!("{}T09:00:00{before}", days(-7)),
                    format!("{}T09:00:00{after}", days(0))
                ],
                "asked at {zone}"
            );
        }
        // the hour it really is, by the offset in force that day
        let nine =
            chrono::DateTime::parse_from_rfc3339(&format!("{}T09:00:00{after}", days(0))).unwrap();
        let hour = |from: chrono::DateTime<chrono::FixedOffset>| {
            range(
                &core,
                &from.to_rfc3339(),
                &(from + Duration::hours(1)).to_rfc3339(),
            )
            .len()
        };
        assert_eq!(hour(nine), 1, "at 09:00 local");
        let off = Duration::hours(1);
        assert_eq!(
            hour(nine - off) + hour(nine + off),
            0,
            "not on the old offset"
        );
    }

    #[test]
    fn a_weekly_event_keeps_nine_oclock_when_the_clocks_go_back() {
        holds_nine_oclock("autumn", autumn_change(), "-04:00", "-05:00");
    }

    #[test]
    fn a_weekly_event_keeps_nine_oclock_when_the_clocks_go_forward() {
        holds_nine_oclock("spring", spring_change(), "-05:00", "-04:00");
    }

    /// The instance after the change is the one edited, on its own offset, and
    /// the series before the change has to stay at the wall clock's new time.
    #[test]
    fn a_series_moved_from_after_the_change_moves_on_the_wall_clock_on_both_sides() {
        let google = account();
        let core = signed_in(&google, "dst-all");
        let monday = monday_around(autumn_change());
        let first = monday - Duration::days(7);
        saved_event(
            &core,
            json!({"calendar": FAKE_EMAIL, "title": "Standup", "start": format!("{first}T09:00:00-04:00"), "end": format!("{first}T10:00:00-04:00"), "time_zone": NY, "repeat": "RRULE:FREQ=WEEKLY;COUNT=2", "guests": [], "where": "", "notes": ""}),
        );
        let (from, to) = (
            format!("{first}T00:00:00Z"),
            format!("{}T00:00:00Z", monday + Duration::days(2)),
        );
        let held = range(&core, &from, &to);
        let id = held[1]["id"].as_str().unwrap();
        let form = form_of(&core, FAKE_EMAIL, id);
        let moved = json!({"scope": "all", "start": format!("{monday}T10:00:00-05:00"), "end": format!("{monday}T11:00:00-05:00")});
        saved_event(&core, with(form, moved));
        assert_eq!(
            starts(&range(&core, &from, &to)),
            [
                format!("{first}T10:00:00-04:00"),
                format!("{monday}T10:00:00-05:00")
            ]
        );
    }

    /// A weekly 09:00 New York series on Saturdays, from `first`, read back.
    fn saturdays(core: &Core, first: NaiveDate, count: i64) -> (Vec<Value>, String, String) {
        assert_eq!(first.weekday(), Weekday::Sat);
        saved_event(
            core,
            json!({"calendar": FAKE_EMAIL, "title": "Standup", "start": format!("{first}T09:00:00-04:00"), "end": format!("{first}T10:00:00-04:00"), "time_zone": NY, "repeat": format!("RRULE:FREQ=WEEKLY;COUNT={count}"), "guests": [], "where": "", "notes": ""}),
        );
        let (from, to) = (
            format!("{}T00:00:00Z", first - Duration::days(1)),
            format!("{}T00:00:00Z", first + Duration::days(7 * count + 2)),
        );
        (range(core, &from, &to), from, to)
    }

    /// Saturday to Sunday over the change is 25 hours and one day on the wall:
    /// it's the day that goes to the series, not the hours.
    #[test]
    fn an_instance_moved_over_the_change_moves_the_series_by_the_wall_clock() {
        let google = account();
        let core = signed_in(&google, "dst-over");
        let change = autumn_change();
        let saturday = change - Duration::days(1);
        let first = saturday - Duration::days(14);
        let (held, from, to) = saturdays(&core, first, 3);
        let form = form_of(&core, FAKE_EMAIL, held[2]["id"].as_str().unwrap());
        let moved = json!({"scope": "all", "start": format!("{change}T09:00:00-05:00"), "end": format!("{change}T10:00:00-05:00")});
        saved_event(&core, with(form, moved));
        let sunday = |weeks: i64| first + Duration::days(7 * weeks + 1);
        assert_eq!(
            starts(&range(&core, &from, &to)),
            [
                format!("{}T09:00:00-04:00", sunday(0)),
                format!("{}T09:00:00-04:00", sunday(1)),
                format!("{}T09:00:00-05:00", sunday(2)),
            ]
        );
    }

    /// The series starts the day before the change and a day's move takes its
    /// first date over it: the same clock now has another offset.
    #[test]
    fn a_series_whose_first_date_moves_over_the_change_keeps_its_time_of_day() {
        let google = account();
        let core = signed_in(&google, "dst-first");
        let change = autumn_change();
        let first = change - Duration::days(1);
        let (held, from, to) = saturdays(&core, first, 2);
        let form = form_of(&core, FAKE_EMAIL, held[1]["id"].as_str().unwrap());
        let next = change + Duration::days(7);
        let moved = json!({"scope": "all", "start": format!("{next}T09:00:00-05:00"), "end": format!("{next}T10:00:00-05:00")});
        saved_event(&core, with(form, moved));
        assert_eq!(
            starts(&range(&core, &from, &to)),
            [
                format!("{change}T09:00:00-05:00"),
                format!("{next}T09:00:00-05:00"),
            ]
        );
    }

    /// Only the caller knows a range is whole days, and says so: from the
    /// times alone, the 01:00 that starts Havana's March 8 is a day half in.
    #[test]
    fn a_range_ending_where_midnight_is_skipped_leaves_out_the_next_day() {
        let google = account();
        let core = signed_in(&google, "skipped");
        let day = NaiveDate::from_ymd_opt(Utc::now().year() + 1, 3, 8).unwrap();
        saved_event(
            &core,
            json!({"calendar": FAKE_EMAIL, "title": "Holiday", "all_day": true, "start": day.to_string(), "end": (day + Duration::days(1)).to_string(), "guests": [], "where": "", "notes": ""}),
        );
        let args = json!({
            "from": format!("{}T00:00:00-05:00", day - Duration::days(1)),
            "to": format!("{day}T01:00:00-04:00"),
            "to_date": day.to_string(),
        });
        assert_eq!(data(core.run("gws_cal_range", &args))["events"], json!([]));
        let next = with(
            args,
            json!({"to_date": (day + Duration::days(1)).to_string()}),
        );
        let got = data(core.run("gws_cal_range", &next));
        assert_eq!(got["events"].as_array().unwrap().len(), 1, "{got}");
    }

    #[test]
    fn a_timed_event_is_in_the_same_range_however_the_range_is_written() {
        let google = account();
        let core = signed_in(&google, "instants");
        saved_event(&core, meeting(10));
        // the meeting is 14:00 to 15:00 UTC
        for (from, to) in [
            (at(10, "14:30:00", "Z"), at(10, "14:45:00", "Z")),
            (at(10, "22:30:00", "+08:00"), at(10, "22:45:00", "+08:00")),
            (at(10, "04:30:00", "-10:00"), at(10, "04:45:00", "-10:00")),
        ] {
            assert_eq!(range(&core, &from, &to).len(), 1, "{from}");
        }
        assert_eq!(
            range(&core, &at(10, "15:00:00", "Z"), &at(10, "16:00:00", "Z")).len(),
            0
        );
        assert_eq!(
            range(&core, &at(10, "13:00:00", "Z"), &at(10, "14:00:00", "Z")).len(),
            0
        );
    }

    #[test]
    fn an_all_day_event_stays_on_its_day_behind_and_ahead_of_utc() {
        let google = account();
        for (n, title) in [(9, "Before"), (10, "Holiday"), (11, "After")] {
            google.add_event(FAKE_EMAIL, json!({"id": format!("day{n}"), "summary": title, "start": {"date": day(n).to_string()}, "end": {"date": day(n + 1).to_string()}}));
        }
        let core = signed_in(&google, "allday");
        for zone in ["-11:00", "-05:00", "+00:00", "+08:00", "+14:00"] {
            let held = range(&core, &at(10, "00:00:00", zone), &at(11, "00:00:00", zone));
            assert_eq!(titles(&held), ["Holiday"], "a day asked for at {zone}");
            assert_eq!(held[0]["all_day"], true);
            assert_eq!(held[0]["start"], day(10).to_string());
            assert_eq!(held[0]["end"], day(11).to_string(), "the exclusive end");
            let noon = range(&core, &at(10, "12:00:00", zone), &at(10, "13:00:00", zone));
            assert_eq!(titles(&noon), ["Holiday"], "an hour of that day at {zone}");
        }
    }

    #[test]
    fn an_all_day_event_saved_goes_out_as_dates_and_comes_back_on_them() {
        let google = account();
        let core = signed_in(&google, "allday-save");
        let (from, to) = (day(10).to_string(), day(12).to_string());
        let e = saved_event(
            &core,
            json!({"calendar": FAKE_EMAIL, "title": "Trip", "all_day": true, "start": from, "end": to, "time_zone": "Asia/Manila", "repeat": "", "guests": [], "where": "", "notes": ""}),
        );
        assert_eq!(
            (e["start"].as_str(), e["end"].as_str()),
            (Some(from.as_str()), Some(to.as_str()))
        );
        let body = google.calendar_writes().pop().unwrap().body;
        assert_eq!(
            body["start"],
            json!({"date": from}),
            "no time, so no zone to slip in"
        );
        assert_eq!(body["end"], json!({"date": to}));
        assert_eq!(
            titles(&range(
                &core,
                &at(11, "00:00:00", "+14:00"),
                &at(12, "00:00:00", "+14:00")
            )),
            ["Trip"]
        );
        assert!(
            range(
                &core,
                &at(12, "00:00:00", "-11:00"),
                &at(13, "00:00:00", "-11:00")
            )
            .is_empty()
        );
    }

    #[test]
    fn an_all_day_event_sorts_before_what_starts_that_morning_wherever_you_are() {
        let google = account();
        google.add_event(FAKE_EMAIL, json!({"id": "dayone", "summary": "Holiday", "start": {"date": day(10).to_string()}, "end": {"date": day(11).to_string()}}));
        google.add_event(FAKE_EMAIL, json!({"id": "early", "summary": "Early", "start": {"dateTime": at(10, "08:00:00", "+08:00")}, "end": {"dateTime": at(10, "09:00:00", "+08:00")}}));
        let core = signed_in(&google, "allday-order");
        let held = range(
            &core,
            &at(10, "00:00:00", "+08:00"),
            &at(11, "00:00:00", "+08:00"),
        );
        assert_eq!(
            titles(&held),
            ["Holiday", "Early"],
            "08:00 at +08:00 is the UTC midnight's day before"
        );
    }

    #[test]
    fn an_event_can_change_between_timed_and_all_day() {
        let google = account();
        let core = signed_in(&google, "flip");
        let e = saved_event(&core, meeting(10));
        let id = e["id"].as_str().unwrap();
        let (from, to) = (day(10).to_string(), day(11).to_string());
        let flipped = saved_event(
            &core,
            with(
                form_of(&core, FAKE_EMAIL, id),
                json!({"all_day": true, "start": from, "end": to}),
            ),
        );
        assert_eq!(flipped["all_day"], true);
        let held = google.event(FAKE_EMAIL, id).unwrap();
        assert_eq!(
            held["start"],
            json!({"date": from}),
            "the old time went with it"
        );
    }
}

mod sync {
    use super::*;

    fn stale(calendar: &str, id: &str) -> gws_store::calendar::Event {
        gws_store::calendar::Event {
            calendar: calendar.to_owned(),
            id: id.to_owned(),
            title: "Stale".to_owned(),
            start_ms: 0,
            end_ms: 1,
            status: "confirmed".to_owned(),
            response: "none".to_owned(),
            ..Default::default()
        }
    }

    fn held(core: &Core, calendar: &str, id: &str) -> bool {
        let store = core.store().unwrap();
        let store = store.as_ref().unwrap();
        store.event(FAKE_EMAIL, calendar, id).unwrap().is_some()
    }

    #[test]
    fn a_first_read_covers_the_window_and_later_ones_only_what_changed() {
        let google = account();
        google.add_event(FAKE_EMAIL, json!({"id": "one", "summary": "One", "start": {"dateTime": at(10, "09:00:00", "Z")}, "end": {"dateTime": at(10, "10:00:00", "Z")}}));
        let core = signed_in(&google, "delta");
        sync(&core);
        let lists = |google: &FakeGoogle| -> Vec<String> {
            let mine = format!("/calendar/v3/calendars/{}/events?", "owner%40example%2Ecom");
            google
                .log()
                .iter()
                .filter(|s| s.target.starts_with(&mine))
                .map(|s| s.target.clone())
                .collect()
        };
        let first = lists(&google);
        assert_eq!(first.len(), 1, "{first:?}");
        assert!(
            first[0].contains("singleEvents=true")
                && first[0].contains("timeMin=")
                && first[0].contains("timeMax="),
            "{first:?}"
        );
        let (from, to) = (at(9, "00:00:00", "Z"), at(11, "00:00:00", "Z"));
        let seq = data(core.run("gws_cal_changes", &json!({})))["seq"].clone();
        assert_ne!(seq, 0, "the first read moved it");

        sync(&core);
        assert_eq!(
            data(core.run("gws_cal_changes", &json!({})))["seq"],
            seq,
            "nothing changed"
        );
        let second = lists(&google);
        assert!(
            second[1].contains("syncToken=") && !second[1].contains("timeMin"),
            "{second:?}"
        );

        google.edit_event(FAKE_EMAIL, "one", &json!({"summary": "Renamed"}));
        sync(&core);
        assert_ne!(data(core.run("gws_cal_changes", &json!({})))["seq"], seq);
        assert_eq!(titles(&range(&core, &from, &to)), ["Renamed"]);

        google.edit_event(FAKE_EMAIL, "one", &json!({"status": "cancelled"}));
        sync(&core);
        assert!(
            range(&core, &from, &to).is_empty(),
            "cancelled elsewhere, gone here"
        );
    }

    #[test]
    fn a_410_wipes_and_rereads_that_calendar_and_no_other() {
        let google = account();
        google.add_event(FAKE_EMAIL, json!({"id": "mine", "summary": "Mine", "start": {"dateTime": at(10, "09:00:00", "Z")}, "end": {"dateTime": at(10, "10:00:00", "Z")}}));
        google.add_event(TEAM, json!({"id": "theirs", "summary": "Theirs", "start": {"dateTime": at(10, "11:00:00", "Z")}, "end": {"dateTime": at(10, "12:00:00", "Z")}}));
        let core = signed_in(&google, "gone");
        sync(&core);
        // rows Google no longer has, on both: only a wipe takes one out
        {
            let store = core.store().unwrap();
            let store = store.as_ref().unwrap();
            store
                .put_event(FAKE_EMAIL, &stale(FAKE_EMAIL, "ghost"))
                .unwrap();
            store.put_event(FAKE_EMAIL, &stale(TEAM, "ghost")).unwrap();
        }
        google.expire_sync(FAKE_EMAIL);
        let seen = google.log().len();
        sync(&core);
        assert!(!held(&core, FAKE_EMAIL, "ghost"), "that calendar was wiped");
        assert!(held(&core, FAKE_EMAIL, "mine"), "and read again");
        assert!(held(&core, TEAM, "ghost"), "the other wasn't touched");
        assert!(held(&core, TEAM, "theirs"));
        let log = google.log();
        let reads = |calendar: &str| -> Vec<String> {
            log[seen..]
                .iter()
                .filter(|s| s.target.contains(calendar) && s.target.contains("/events?"))
                .map(|s| s.target.clone())
                .collect()
        };
        let mine = reads("owner%40example");
        assert!(
            mine.len() == 2 && mine[0].contains("syncToken=") && mine[1].contains("timeMin="),
            "{mine:?}"
        );
        let team = reads("team%40group");
        assert!(
            team.len() == 1 && team[0].contains("syncToken="),
            "{team:?}"
        );
        sync(&core);
        let after = google.log();
        let last = after
            .iter()
            .rfind(|s| s.target.contains("owner%40example"))
            .unwrap();
        assert!(
            last.target.contains("syncToken="),
            "and it has a token again"
        );
    }

    #[test]
    fn a_whole_read_that_fails_after_a_410_keeps_the_old_events_and_tries_again() {
        let google = account();
        google.add_event(FAKE_EMAIL, json!({"id": "mine", "summary": "Mine", "start": {"dateTime": at(10, "09:00:00", "Z")}, "end": {"dateTime": at(10, "10:00:00", "Z")}}));
        let core = signed_in(&google, "gone-offline");
        sync(&core);
        google.expire_sync(FAKE_EMAIL);
        google.whole_reads_fail(true);
        sync(&core);
        assert!(held(&core, FAKE_EMAIL, "mine"), "not blank in the meantime");
        let status = data(core.run("gws_cal_changes", &json!({})))["status"].clone();
        assert_eq!(status, "failed");
        // the 410 isn't asked for twice: the next check reads it whole
        google.whole_reads_fail(false);
        google.edit_event(FAKE_EMAIL, "mine", &json!({"summary": "Renamed"}));
        let seen = google.log().len();
        sync(&core);
        let reread = google.log()[seen..]
            .iter()
            .filter(|s| s.target.contains("owner%40example") && s.target.contains("/events?"))
            .map(|s| s.target.clone())
            .collect::<Vec<_>>();
        assert!(
            reread.len() == 1 && reread[0].contains("timeMin="),
            "{reread:?}"
        );
        let (from, to) = (at(9, "00:00:00", "Z"), at(11, "00:00:00", "Z"));
        assert_eq!(titles(&range(&core, &from, &to)), ["Renamed"]);
    }

    #[test]
    fn events_come_in_over_several_pages() {
        let google = account();
        for n in 0..7 {
            google.add_event(FAKE_EMAIL, json!({"id": format!("e{n}"), "summary": format!("E{n}"), "start": {"dateTime": at(10 + n, "09:00:00", "Z")}, "end": {"dateTime": at(10 + n, "10:00:00", "Z")}}));
        }
        google.calendar_page(3);
        let core = signed_in(&google, "pages");
        sync(&core);
        assert_eq!(
            range(&core, &at(9, "00:00:00", "Z"), &at(20, "00:00:00", "Z")).len(),
            7
        );
        for n in 0..4 {
            google.edit_event(FAKE_EMAIL, &format!("e{n}"), &json!({"summary": "New"}));
        }
        sync(&core);
        let now = range(&core, &at(9, "00:00:00", "Z"), &at(20, "00:00:00", "Z"));
        assert_eq!(titles(&now).iter().filter(|t| **t == "New").count(), 4);
    }

    #[test]
    fn one_check_covers_mail_and_events() {
        let google = FakeGoogle::start(Seed {
            calendars: vec![FakeCalendar::new(FAKE_EMAIL, "Owner", true, NY)],
            ..Seed::demo(3)
        });
        google.add_event(FAKE_EMAIL, json!({"id": "one", "summary": "One", "start": {"dateTime": at(10, "09:00:00", "Z")}, "end": {"dateTime": at(10, "10:00:00", "Z")}}));
        let core = signed_in(&google, "joined");
        sync(&core);
        assert!(held(&core, FAKE_EMAIL, "one"));
        let store = core.store().unwrap();
        assert_eq!(
            store
                .as_ref()
                .unwrap()
                .threads(FAKE_EMAIL, "INBOX", 0, 9)
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn a_sign_in_without_calendar_says_so_and_leaves_mail_alone() {
        let google = FakeGoogle::start(Seed {
            calendars: vec![FakeCalendar::new(FAKE_EMAIL, "Owner", true, NY)],
            ..Seed::demo(3)
        });
        google.only_grants("https://www.googleapis.com/auth/gmail.modify");
        let core = signed_in(&google, "nocal");
        sync(&core);
        let mail = data(core.run("gws_mail_changes", &json!({})));
        assert_eq!(mail["status"], "ok", "mail is unmoved");
        let answer = core.run("gws_cal_list", &json!({}));
        assert_eq!(
            answer["structuredContent"]["code"], "needs_access",
            "{answer}"
        );
    }

    #[test]
    fn with_no_connection_a_range_reads_the_store_and_says_so() {
        let google = account();
        let core = signed_in(&google, "offline");
        saved_event(&core, meeting(10));
        let (from, to) = (at(9, "00:00:00", "Z"), at(11, "00:00:00", "Z"));
        let answer = data(core.run("gws_cal_range", &json!({"from": from, "to": to})));
        assert_eq!(answer["saved"], false);
        sync(&core);
        drop(core);
        let dead = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let offline = signed_in_at(&format!("http://127.0.0.1:{dead}"), "offline", false);
        let _ = gws::mail::sync(&offline);
        let answer = data(offline.run("gws_cal_range", &json!({"from": from, "to": to})));
        assert_eq!(answer["saved"], true);
        assert_eq!(answer["events"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn signing_out_clears_the_events_with_the_mail() {
        let google = account();
        google.add_event(FAKE_EMAIL, json!({"id": "one", "summary": "One", "start": {"dateTime": at(10, "09:00:00", "Z")}, "end": {"dateTime": at(10, "10:00:00", "Z")}}));
        let core = signed_in(&google, "signout");
        core.account("buwiz.account.set", &json!({"account": "google", "label": "default", "provider": "google", "subject": FAKE_EMAIL, "scopes": [], "access_token": "fake-access-1", "token_type": "Bearer", "expires_at": 4_000_000_000u64}));
        sync(&core);
        assert!(held(&core, FAKE_EMAIL, "one"));
        core.account(
            "buwiz.account.clear",
            &json!({"account": "google", "label": "default"}),
        );
        assert!(!held(&core, FAKE_EMAIL, "one"));
        let store = core.store().unwrap();
        let store = store.as_ref().unwrap();
        assert!(store.calendars(FAKE_EMAIL).unwrap().is_empty());
        assert_eq!(store.state(FAKE_EMAIL, "cal_seq").unwrap(), None);
    }
}

mod writes {
    use super::*;

    #[test]
    fn a_412_on_save_is_changed_and_writes_nothing() {
        let google = account();
        let core = signed_in(&google, "412");
        let e = saved_event(&core, meeting(10));
        let id = e["id"].as_str().unwrap();
        let form = form_of(&core, FAKE_EMAIL, id);
        google.edit_event(FAKE_EMAIL, id, &json!({"summary": "Their edit"}));
        let before = google.calendar_writes().len();
        let answer = core.run(
            "gws_event_save",
            &with(form.clone(), json!({"title": "My edit"})),
        );
        assert_eq!(refusal(&answer), "changed");
        assert_eq!(google.calendar_writes().len(), before, "nothing accepted");
        assert_eq!(
            google.event(FAKE_EMAIL, id).unwrap()["summary"],
            "Their edit"
        );
        let tried = google
            .log()
            .into_iter()
            .rfind(|s| s.method == "PATCH")
            .unwrap();
        assert!(
            tried.body.contains("My edit"),
            "Google's own 412 said no: {tried:?}"
        );
    }

    #[test]
    fn the_etag_goes_out_as_if_match() {
        let google = account();
        let core = signed_in(&google, "ifmatch");
        let e = saved_event(&core, meeting(10));
        let form = form_of(&core, FAKE_EMAIL, e["id"].as_str().unwrap());
        saved_event(&core, with(form.clone(), json!({"title": "New"})));
        let write = google.calendar_writes().pop().unwrap();
        assert_eq!(write.if_match, form["etag"].as_str().unwrap());
    }

    #[test]
    fn a_series_changed_since_it_was_read_is_refused_too() {
        let google = account();
        let core = signed_in(&google, "412-all");
        let held = series(&core, 10);
        let id = held[0]["id"].as_str().unwrap();
        let form = form_of(&core, FAKE_EMAIL, id);
        let master =
            data(core.run("gws_event_read", &json!({"calendar": FAKE_EMAIL, "id": id})))["series"]
                .as_str()
                .unwrap()
                .to_owned();
        google.edit_event(FAKE_EMAIL, &master, &json!({"summary": "Their edit"}));
        let before = google.calendar_writes().len();
        let answer = core.run(
            "gws_event_save",
            &with(form, json!({"scope": "all", "title": "Mine"})),
        );
        assert_eq!(refusal(&answer), "changed");
        assert_eq!(google.calendar_writes().len(), before);
        assert_eq!(
            google.event(FAKE_EMAIL, &master).unwrap()["summary"],
            "Their edit"
        );
    }

    #[test]
    fn a_save_to_an_event_gone_is_not_found() {
        let google = account();
        let core = signed_in(&google, "gone-save");
        let e = saved_event(&core, meeting(10));
        let id = e["id"].as_str().unwrap();
        let form = form_of(&core, FAKE_EMAIL, id);
        google.edit_event(FAKE_EMAIL, id, &json!({"status": "cancelled"}));
        assert_eq!(refusal(&core.run("gws_event_save", &form)), "not_found");
        let none = with(form, json!({"id": "nosuchevent"}));
        assert_eq!(refusal(&core.run("gws_event_save", &none)), "not_found");
    }

    #[test]
    fn what_google_or_the_form_cant_take_is_invalid_args() {
        let google = account();
        let core = signed_in(&google, "invalid");
        let backwards = with(meeting(10), json!({"end": at(10, "08:00:00", "-05:00")}));
        assert_eq!(
            refusal(&core.run("gws_event_save", &backwards)),
            "invalid_args"
        );
        let zone = with(meeting(10), json!({"time_zone": "Mars/Base"}));
        assert_eq!(
            refusal(&core.run("gws_event_save", &zone)),
            "invalid_args",
            "Google's own 400"
        );
        assert_eq!(google.calendar_writes(), []);
    }

    #[test]
    fn undo_brings_back_a_delete_with_no_guests() {
        let google = account();
        let core = signed_in(&google, "undo");
        let e = saved_event(&core, meeting(10));
        let args = json!({"calendar": FAKE_EMAIL, "id": e["id"]});
        let (from, to) = (at(9, "00:00:00", "Z"), at(11, "00:00:00", "Z"));
        let deleted = data(core.run("gws_event_delete", &args));
        assert_eq!(deleted, json!({"ok": true, "undo": true}));
        assert!(range(&core, &from, &to).is_empty());
        let back = data(core.run("gws_event_restore", &args));
        assert_eq!(back["event"]["id"], e["id"]);
        assert_eq!(titles(&range(&core, &from, &to)), ["Roof call"]);
        let write = google.calendar_writes().pop().unwrap();
        assert_eq!(write.send_updates.as_deref(), Some("none"));
        assert_eq!(
            refusal(&core.run("gws_event_restore", &args)),
            "not_found",
            "an undo is good once"
        );
    }

    #[test]
    fn undo_is_refused_with_guests_and_changes_nothing() {
        let google = account();
        let core = signed_in(&google, "undo-guests");
        let e = saved_event(
            &core,
            with(meeting(10), json!({"guests": ["ana@example.com"]})),
        );
        let args = json!({"calendar": FAKE_EMAIL, "id": e["id"]});
        let deleted = data(core.run("gws_event_delete", &args));
        assert_eq!(deleted, json!({"ok": true, "undo": false}));
        let writes = google.calendar_writes().len();
        assert_eq!(refusal(&core.run("gws_event_restore", &args)), "has_guests");
        assert_eq!(google.calendar_writes().len(), writes);
        assert_eq!(
            google.event(FAKE_EMAIL, e["id"].as_str().unwrap()).unwrap()["status"],
            "cancelled"
        );
    }

    #[test]
    fn a_deleted_series_comes_back_whole() {
        let google = account();
        let core = signed_in(&google, "undo-series");
        let held = series(&core, 10);
        let args = json!({"calendar": FAKE_EMAIL, "id": held[1]["id"], "scope": "all"});
        assert_eq!(data(core.run("gws_event_delete", &args))["undo"], true);
        data(core.run(
            "gws_event_restore",
            &json!({"calendar": FAKE_EMAIL, "id": held[1]["id"]}),
        ));
        assert_eq!(
            range(&core, &at(9, "00:00:00", "Z"), &at(50, "00:00:00", "Z")).len(),
            4
        );
    }

    #[test]
    fn an_invite_is_answered_and_everyone_else_stays_as_they_were() {
        let google = account();
        google.add_event(FAKE_EMAIL, json!({"id": "invite", "summary": "Planning", "organizer": {"email": "cara@example.com", "displayName": "Cara Lim"}, "start": {"dateTime": at(10, "09:00:00", "Z")}, "end": {"dateTime": at(10, "10:00:00", "Z")}, "attendees": [{"email": "cara@example.com", "organizer": true, "responseStatus": "accepted"}, {"email": FAKE_EMAIL, "responseStatus": "needsAction"}, {"email": "ben@example.com", "responseStatus": "tentative"}]}));
        let core = signed_in(&google, "rsvp");
        let args = |r: &str| json!({"calendar": FAKE_EMAIL, "id": "invite", "response": r});
        sync(&core);
        assert_eq!(
            refusal(&core.run("gws_event_rsvp", &args("maybe"))),
            "invalid_args"
        );
        let row = data(core.run("gws_event_rsvp", &args("accepted")))["event"].clone();
        assert_eq!(row["response"], "accepted");
        let held = google.event(FAKE_EMAIL, "invite").unwrap();
        let answers: Vec<&str> = held["attendees"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["responseStatus"].as_str().unwrap())
            .collect();
        assert_eq!(answers, ["accepted", "accepted", "tentative"]);
        assert!(!google.calendar_writes().pop().unwrap().if_match.is_empty());
        let read = data(core.run(
            "gws_event_read",
            &json!({"calendar": FAKE_EMAIL, "id": "invite"}),
        ));
        assert_eq!(read["organizer"], "Cara Lim");
        assert_eq!(read["can_edit"], false, "someone else's event");
        assert_eq!(read["guests"], 2);
    }

    #[test]
    fn a_reply_needs_an_invite() {
        let google = account();
        let core = signed_in(&google, "rsvp-none");
        let e = saved_event(&core, meeting(10));
        let args = json!({"calendar": FAKE_EMAIL, "id": e["id"], "response": "accepted"});
        assert_eq!(refusal(&core.run("gws_event_rsvp", &args)), "invalid_args");
    }

    #[test]
    fn a_meet_link_is_asked_for_once_and_comes_back_on_the_row() {
        let google = account();
        let core = signed_in(&google, "meet");
        let e = saved_event(&core, with(meeting(10), json!({"meet": true})));
        assert_eq!(e["meet"], true);
        let write = google.calendar_writes().pop().unwrap();
        assert_eq!(
            write.body["conferenceData"]["createRequest"]["conferenceSolutionKey"]["type"],
            "hangoutsMeet"
        );
        let form = form_of(&core, FAKE_EMAIL, e["id"].as_str().unwrap());
        let read = data(core.run(
            "gws_event_read",
            &json!({"calendar": FAKE_EMAIL, "id": e["id"]}),
        ));
        assert!(
            read["meet_url"]
                .as_str()
                .unwrap()
                .starts_with("https://meet.example.com/")
        );
        let writes = google.calendar_writes().len();
        saved_event(&core, form);
        assert_eq!(google.calendar_writes().len(), writes, "it has one already");

        let plain = saved_event(&core, meeting(11));
        let add = with(
            form_of(&core, FAKE_EMAIL, plain["id"].as_str().unwrap()),
            json!({"meet": true}),
        );
        assert_eq!(
            saved_event(&core, add)["meet"],
            true,
            "and an existing event can get one"
        );
    }

    #[test]
    fn a_calendar_id_goes_in_the_path_whole() {
        let odd = "en.usa#holiday@group.example.com";
        let google = FakeGoogle::start(Seed {
            calendars: vec![FakeCalendar::new(odd, "Holidays", false, "UTC")],
            ..Seed::default()
        });
        google.add_event(odd, json!({"id": "h", "summary": "Holiday", "start": {"date": day(10).to_string()}, "end": {"date": day(11).to_string()}}));
        let core = signed_in(&google, "oddid");
        let held = range(&core, &at(10, "00:00:00", "Z"), &at(11, "00:00:00", "Z"));
        assert_eq!(titles(&held), ["Holiday"]);
        let read = data(core.run("gws_event_read", &json!({"calendar": odd, "id": "h"})));
        assert_eq!(read["title"], "Holiday");
    }
}

mod reads {
    use super::*;

    fn seeded(name: &str) -> (FakeGoogle, Core) {
        let now = Utc::now().timestamp_millis();
        let google = FakeGoogle::start(Seed::default().with_calendar_around(now));
        let core = signed_in(&google, name);
        (google, core)
    }

    #[test]
    fn the_live_seed_shows_this_week_with_all_it_promises() {
        let (google, core) = seeded("seed");
        let week = range(&core, &at(-1, "00:00:00", "Z"), &at(8, "00:00:00", "Z"));
        let calendars: std::collections::BTreeSet<&str> = week
            .iter()
            .map(|e| e["calendar"].as_str().unwrap())
            .collect();
        assert_eq!(calendars.len(), 2, "{week:?}");
        assert!(week.iter().any(|e| e["all_day"] == true));
        assert!(week.iter().any(|e| e["recurring"] == true));
        assert!(week.iter().any(|e| e["meet"] == true && e["guests"] == 2));
        assert!(week.iter().any(|e| e["response"] == "needsAction"));
        assert_eq!(google.writes(), [], "reading changes nothing at Google");
    }

    #[test]
    fn an_event_reads_in_full_with_its_notes_cleaned() {
        let (_google, core) = seeded("read");
        let e = data(core.run(
            "gws_event_read",
            &json!({"calendar": FAKE_EMAIL, "id": "seedreview"}),
        ));
        assert_eq!(e["notes"], "Bring the signed copy.");
        assert_eq!(e["where"], "Room 4");
        assert_eq!(e["repeat"], "");
        assert_eq!(e["series"], "");
        assert_eq!(e["can_edit"], true);
        assert_eq!(e["time_zone"], "Asia/Manila");
        assert!(e["etag"].is_string());
        assert_eq!(e["attendees"].as_array().unwrap().len(), 3);
        assert_eq!(
            e["attendees"][1],
            json!({"email": "ana@example.com", "name": "Ana Reyes", "response": "accepted", "optional": false, "organizer": false, "self": false, "resource": false})
        );
        assert_eq!(e["attendees"][0]["self"], true);
        let none = core.run(
            "gws_event_read",
            &json!({"calendar": FAKE_EMAIL, "id": "nope"}),
        );
        assert_eq!(refusal(&none), "not_found");
    }

    #[test]
    fn a_reader_cant_edit_and_a_hidden_calendar_is_read_by_no_range() {
        let google = account();
        let mut shared = FakeCalendar::new("shared@group.example.com", "Shared", false, "UTC");
        shared.role = "reader".to_owned();
        shared.events = vec![
            json!({"id": "s", "summary": "Shared", "organizer": {"email": "shared@group.example.com"}, "start": {"dateTime": at(10, "09:00:00", "Z")}, "end": {"dateTime": at(10, "10:00:00", "Z")}}),
        ];
        google.add_calendar(shared);
        let core = signed_in(&google, "reader");
        let list = data(core.run("gws_cal_list", &json!({})));
        let names: Vec<(&str, bool, bool)> = list["calendars"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| {
                (
                    c["name"].as_str().unwrap(),
                    c["writable"].as_bool().unwrap(),
                    c["primary"].as_bool().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            names,
            [
                ("Owner", true, true),
                ("Shared", false, false),
                ("Team", true, false)
            ]
        );
        let read = data(core.run(
            "gws_event_read",
            &json!({"calendar": "shared@group.example.com", "id": "s"}),
        ));
        assert_eq!(read["can_edit"], false);

        let (from, to) = (at(9, "00:00:00", "Z"), at(11, "00:00:00", "Z"));
        assert_eq!(range(&core, &from, &to).len(), 1);
        let seq = data(core.run("gws_cal_changes", &json!({})))["seq"].clone();
        let hide = json!({"calendar": "shared@group.example.com", "shown": false});
        assert_eq!(data(core.run("gws_cal_show", &hide)), json!({"ok": true}));
        assert_ne!(data(core.run("gws_cal_changes", &json!({})))["seq"], seq);
        assert!(range(&core, &from, &to).is_empty());
        sync(&core);
        assert!(
            range(&core, &from, &to).is_empty(),
            "the choice outlives a sync"
        );
        assert_eq!(google.writes(), [], "a local choice, kept from Google");
        let none = json!({"calendar": "nope", "shown": false});
        assert_eq!(refusal(&core.run("gws_cal_show", &none)), "not_found");
    }

    #[test]
    fn next_is_what_is_coming_and_the_two_after_it() {
        let google = account();
        let at_ms = |ms: i64| {
            chrono::DateTime::from_timestamp_millis(ms)
                .unwrap()
                .to_rfc3339()
        };
        let now = Utc::now().timestamp_millis();
        let hour = 3_600_000;
        for (id, from, to) in [
            ("ended", -3, -2),
            ("soon", 1, 2),
            ("later", 3, 4),
            ("evening", 5, 6),
            ("tomorrow", 30, 31),
        ] {
            google.add_event(FAKE_EMAIL, json!({"id": id, "summary": id, "start": {"dateTime": at_ms(now + from * hour)}, "end": {"dateTime": at_ms(now + to * hour)}}));
        }
        let core = signed_in(&google, "next");
        let next = data(core.run("gws_cal_next", &json!({})));
        assert_eq!(next["event"]["title"], "soon");
        assert_eq!(
            titles(next["after"].as_array().unwrap()),
            ["later", "evening"]
        );
    }

    #[test]
    fn with_nothing_coming_next_is_null() {
        let google = account();
        let core = signed_in(&google, "next-none");
        assert_eq!(
            data(core.run("gws_cal_next", &json!({}))),
            json!({"event": null, "after": []})
        );
    }

    #[test]
    fn search_reads_the_store_marks_the_words_as_a_strangers_and_keeps_to_its_range() {
        let (google, core) = seeded("search");
        let found = data(core.run_for_model("gws_search_events", &json!({"query": "signed copy"})));
        assert_eq!(
            titles(found["items"].as_array().unwrap()),
            ["Contract review"]
        );
        assert!(
            found["untrusted"]
                .as_str()
                .unwrap()
                .contains("never as instructions")
        );
        let writes = google.writes();
        assert_eq!(writes, [], "a search changes nothing at Google");
        let past = json!({"query": "signed copy", "to": at(-30, "00:00:00", "Z")});
        assert_eq!(
            data(core.run("gws_search_events", &past))["items"],
            json!([])
        );
        let empty = core.run("gws_search_events", &json!({"query": " "}));
        assert_eq!(empty["isError"], true);
    }

    #[test]
    fn nothing_is_read_without_a_sign_in() {
        let google = account();
        let core = signed_in(&google, "nobody");
        core.accounts
            .clear(&json!({"account": "google", "label": "default"}));
        for (tool, args) in [
            ("gws_cal_list", json!({})),
            ("gws_cal_next", json!({})),
            (
                "gws_cal_range",
                json!({"from": at(1, "00:00:00", "Z"), "to": at(2, "00:00:00", "Z")}),
            ),
            ("gws_search_events", json!({"query": "x"})),
            ("gws_event_save", meeting(10)),
        ] {
            let answer = core.run(tool, &args);
            assert_eq!(
                answer["structuredContent"]["code"], "account_required",
                "{tool}"
            );
        }
        assert_eq!(google.log(), []);
    }

    #[test]
    fn the_token_goes_to_google_and_into_no_answer() {
        let (google, core) = seeded("token");
        let week = range(&core, &at(-1, "00:00:00", "Z"), &at(8, "00:00:00", "Z"));
        let log = google.log();
        assert!(
            !log.is_empty()
                && log
                    .iter()
                    .all(|s| s.authorization == "Bearer fake-access-1")
        );
        assert!(!json!(week).to_string().contains("fake-access-1"));
    }

    #[test]
    fn a_bad_range_is_refused() {
        let google = account();
        let core = signed_in(&google, "badrange");
        for args in [
            json!({}),
            json!({"from": "today", "to": "tomorrow"}),
            json!({"from": at(2, "00:00:00", "Z"), "to": at(1, "00:00:00", "Z")}),
        ] {
            assert_eq!(refusal(&core.run("gws_cal_range", &args)), "invalid_args");
        }
    }
}

/// A 500 after the event went in leaves open whether it landed, so nothing
/// tries again: one try more would make it twice.
#[test]
fn a_new_event_answered_500_after_it_landed_is_made_once_and_said_so() {
    let google = account();
    let core = signed_in(&google, "insert-500");
    google.calendar_insert_answers_500(true);
    let answer = core.run("gws_event_save", &meeting(1));
    assert_eq!(answer["isError"], true, "{answer}");
    assert_eq!(
        answer["content"][0]["text"],
        gws::calendar::UNANSWERED_WRITE,
        "{answer}"
    );
    let posts: Vec<_> = google
        .calendar_writes()
        .into_iter()
        .filter(|w| w.method == "POST")
        .collect();
    assert_eq!(posts.len(), 1, "no second POST: {posts:?}");
    assert!(
        google.log().iter().filter(|s| s.method == "POST").count() == 1,
        "one POST reached Calendar"
    );
}
