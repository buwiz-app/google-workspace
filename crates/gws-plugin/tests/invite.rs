//! An invite in Mail, against the stub Google: the thread view reads its event
//! from the `.ics`, Calendar is asked by the UID it carries, and an answer is
//! one write that the organiser hears of. This proves the plugin against the
//! stub, which is written from what's known of Google's API, not against Google.

use gws::Core;
use gws::apis::Apis;
use gws_fake_google::{FAKE_EMAIL, FakeGoogle, Mail, Seed};
use serde_json::{Value, json};

/// The rich stub's invite, as its thread and its message.
const INVITE: &str = "000019f000000014";

fn signed_in(google: &FakeGoogle, name: &str) -> Core {
    let dir = std::env::temp_dir().join(format!("gws-invite-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let core = Core::new(&Apis::at(&google.base()), Ok(gws_store::Paths::in_dir(dir)));
    core.unlock("seed");
    core.accounts.set(&json!({"account": "google", "label": "default", "provider": "google", "subject": FAKE_EMAIL, "scopes": ["https://www.googleapis.com/auth/gmail.modify", "https://www.googleapis.com/auth/calendar.events"], "access_token": "fake-access-1", "token_type": "Bearer", "expires_at": 4_000_000_000u64}));
    core
}

fn rich() -> Seed {
    Seed::default().with_rich_mail("http://127.0.0.1:1")
}

fn data(answer: Value) -> Value {
    assert_ne!(answer["isError"], true, "{answer}");
    answer["structuredContent"].clone()
}

fn mail_with(ics: &str, n: u64) -> Mail {
    Mail {
        id: format!("{:016x}", 0x1a00_0000_0000_u64 + n),
        thread: format!("{:016x}", 0x1a00_0000_0000_u64 + n),
        labels: vec!["INBOX".into()],
        from: "Ana Reyes <calendar-notification@google.com>".into(),
        to: FAKE_EMAIL.into(),
        subject: "Invitation".into(),
        at: 1_793_955_600_000,
        text: "An invite".into(),
        html: None,
        attachments: vec![(
            "invite.ics".into(),
            "text/calendar".into(),
            ics.as_bytes().to_vec(),
        )],
        message_id: String::new(),
        inline: Vec::new(),
    }
}

const CANCEL: &str = "BEGIN:VCALENDAR\r\nMETHOD:CANCEL\r\nBEGIN:VEVENT\r\nUID:fake-roof-inspection@google.com\r\nDTSTART:20261110T020000Z\r\nDTEND:20261110T030000Z\r\nSUMMARY:Roof inspection\r\nORGANIZER;CN=Ana Reyes:mailto:ana@example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

#[test]
fn the_card_reads_the_ics_and_the_thread_view_keeps_it() {
    let google = FakeGoogle::start(rich());
    let core = signed_in(&google, "view");
    let view = data(core.run("gws_mail_thread", &json!({"thread": INVITE})));
    assert_eq!(
        view["invite"],
        json!({"pending": true}),
        "the card fetches it"
    );
    data(core.run("gws_mail_invite", &json!({"thread": INVITE})));
    let view = data(core.run("gws_mail_thread", &json!({"thread": INVITE})));
    assert_eq!(
        view["invite"],
        json!({"message": INVITE, "uid": "fake-roof-inspection@google.com", "method": "request", "title": "Roof inspection", "start": "2026-11-10T02:00:00Z", "end": "2026-11-10T03:00:00Z", "all_day": false, "place": "12 Mabini St", "organizer": "Ana Reyes", "organizer_address": "ana@example.com", "sender_ok": true, "sender_rule": 2})
    );
    // a mail with none says so, so a saved view tells "none" from "not read yet"
    let other = data(core.run("gws_mail_thread", &json!({"thread": "000019f00000001e"})));
    assert_eq!(other.get("invite"), Some(&Value::Null), "{other}");
}

#[test]
fn calendar_has_the_invites_event_and_the_person_has_not_answered() {
    let google = FakeGoogle::start(rich());
    let core = signed_in(&google, "look");
    for args in [json!({"thread": INVITE}), json!({"message": INVITE})] {
        let got = data(core.run("gws_mail_invite", &args));
        assert_eq!(got["invite"]["title"], "Roof inspection", "{args}");
        assert_eq!(
            got["calendar"],
            json!({"found": true, "cancelled": false, "matches": true, "guest": true, "response": "needsAction", "event": {"title": "Roof inspection", "start": "2026-11-10T02:00:00Z", "end": "2026-11-10T03:00:00Z", "all_day": false, "place": "12 Mabini St", "organizer": "Ana Reyes"}}),
            "{args}"
        );
    }
    assert!(
        google
            .log()
            .iter()
            .any(|s| s.target.contains("iCalUID=fake-roof-inspection")),
        "found by its UID"
    );
    assert!(google.calendar_writes().is_empty(), "a look writes nothing");
}

#[test]
fn yes_is_one_patch_the_organiser_hears_of_with_the_persons_answer() {
    let google = FakeGoogle::start(rich());
    let core = signed_in(&google, "yes");
    let answer = core.run(
        "gws_mail_invite_reply",
        &json!({"message": INVITE, "response": "accepted"}),
    );
    assert_eq!(answer.get("_meta"), None, "no card, nothing for the model");
    assert_eq!(data(answer)["event"]["response"], "accepted");
    let writes = google.calendar_writes();
    assert_eq!(writes.len(), 1, "{writes:?}");
    let w = &writes[0];
    assert_eq!(
        (w.method.as_str(), w.event.as_str()),
        ("PATCH", "roofinspection")
    );
    assert_eq!(
        w.send_updates.as_deref(),
        Some("all"),
        "the organiser hears"
    );
    let mine: Vec<&Value> = w.body["attendees"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|a| a["email"] == FAKE_EMAIL)
        .collect();
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0]["responseStatus"], "accepted");
    let ana = w.body["attendees"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["email"] == "ana@example.com")
        .unwrap();
    assert_eq!(
        ana["responseStatus"], "accepted",
        "the others stay as they were"
    );
    let now = data(core.run("gws_mail_invite", &json!({"thread": INVITE})));
    assert_eq!(now["calendar"]["response"], "accepted");
}

#[test]
fn maybe_and_no_answer_as_they_say_and_anything_else_writes_nothing() {
    let google = FakeGoogle::start(rich());
    let core = signed_in(&google, "maybe");
    for bad in ["maybe", "yes", ""] {
        let got = data(core.run(
            "gws_mail_invite_reply",
            &json!({"message": INVITE, "response": bad}),
        ));
        assert_eq!(got["refused"], "invalid_args", "{bad}");
    }
    let got = data(core.run(
        "gws_mail_invite_reply",
        &json!({"message": "../x", "response": "accepted"}),
    ));
    assert_eq!(got["refused"], "invalid_args");
    assert!(google.calendar_writes().is_empty());
    for response in ["tentative", "declined"] {
        data(core.run(
            "gws_mail_invite_reply",
            &json!({"message": INVITE, "response": response}),
        ));
        let held = google.event(FAKE_EMAIL, "roofinspection").unwrap();
        let me = held["attendees"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["email"] == FAKE_EMAIL)
            .unwrap()
            .clone();
        assert_eq!(me["responseStatus"], response);
    }
}

#[test]
fn an_invite_not_in_calendar_yet_is_shown_and_never_answered() {
    let mut seed = rich();
    for c in &mut seed.calendars {
        c.events.clear();
    }
    let google = FakeGoogle::start(seed);
    let core = signed_in(&google, "missing");
    let got = data(core.run("gws_mail_invite", &json!({"thread": INVITE})));
    assert_eq!(got["invite"]["title"], "Roof inspection");
    assert_eq!(got["calendar"]["found"], false);
    let refused = data(core.run(
        "gws_mail_invite_reply",
        &json!({"message": INVITE, "response": "accepted"}),
    ));
    assert_eq!(refused["refused"], "not_found");
    assert_eq!(refused["message"], "Not in your Calendar yet.");
    assert!(google.calendar_writes().is_empty());
}

#[test]
fn a_cancelled_event_is_said_and_cannot_be_answered() {
    let google = FakeGoogle::start(rich());
    let mail = mail_with(CANCEL, 1);
    google.deliver(mail.clone());
    let core = signed_in(&google, "cancel");
    let got = data(core.run("gws_mail_invite", &json!({"thread": mail.thread})));
    assert_eq!(got["invite"]["method"], "cancel");
    assert_eq!(got["calendar"], Value::Null, "nothing to look up");
    let refused = data(core.run(
        "gws_mail_invite_reply",
        &json!({"message": mail.id, "response": "accepted"}),
    ));
    assert_eq!(refused["refused"], "invalid_args");
    assert!(google.calendar_writes().is_empty());
}

#[test]
fn a_broken_ics_is_no_invite() {
    let google = FakeGoogle::start(rich());
    let mail = mail_with("BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\n", 2);
    google.deliver(mail.clone());
    let core = signed_in(&google, "broken");
    data(core.run("gws_mail_thread", &json!({"thread": mail.thread})));
    for args in [json!({"message": mail.id}), json!({"thread": mail.thread})] {
        let got = data(core.run("gws_mail_invite", &args));
        assert_eq!(got["invite"], Value::Null, "{args}");
    }
    let view = data(core.run("gws_mail_thread", &json!({"thread": mail.thread})));
    assert_eq!(view["invite"], Value::Null);
}

/// The rich invite's `.ics` with `change` made to it.
fn rich_ics(change: impl Fn(String) -> String) -> String {
    change("BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nDTSTART:20261110T020000Z\r\nDTEND:20261110T030000Z\r\nUID:fake-roof-inspection@google.com\r\nORGANIZER;CN=Ana Reyes:mailto:ana@example.com\r\nATTENDEE;PARTSTAT=NEEDS-ACTION:mailto:owner@example.com\r\nSUMMARY:Roof inspection\r\nLOCATION:12 Mabini St\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n".to_owned())
}

#[test]
fn an_invite_that_didnt_come_is_asked_for_again_not_kept_as_none() {
    let google = FakeGoogle::start(rich());
    let core = signed_in(&google, "refetch");
    google.attachments_fail(true);
    let view = data(core.run("gws_mail_thread", &json!({"thread": INVITE})));
    assert_eq!(
        view["invite"],
        json!({"pending": true}),
        "not none: not read yet"
    );
    let look = core.run("gws_mail_invite", &json!({"thread": INVITE}));
    assert_eq!(look["structuredContent"]["code"], "busy", "{look}");
    let view = data(core.run("gws_mail_thread", &json!({"thread": INVITE})));
    assert_eq!(view["invite"], json!({"pending": true}), "still not none");
    google.attachments_fail(false);
    let got = data(core.run("gws_mail_invite", &json!({"thread": INVITE})));
    assert_eq!(got["invite"]["title"], "Roof inspection");
    let view = data(core.run("gws_mail_thread", &json!({"thread": INVITE})));
    assert_eq!(view["invite"]["title"], "Roof inspection");
}

#[test]
fn a_view_saved_before_invites_were_read_is_read_again() {
    let google = FakeGoogle::start(rich());
    let core = signed_in(&google, "old-view");
    data(core.run("gws_mail_thread", &json!({"thread": INVITE})));
    {
        let held = core.store().unwrap();
        let old = json!({"id": INVITE, "page": "<p>old</p>", "links": [], "images": 0, "attachments": []});
        held.as_ref()
            .unwrap()
            .put_view(FAKE_EMAIL, INVITE, &old.to_string(), 1)
            .unwrap();
    }
    let got = data(core.run("gws_mail_invite", &json!({"thread": INVITE})));
    assert_eq!(got["invite"]["title"], "Roof inspection");
}

#[test]
fn a_forged_or_stale_invite_naming_a_real_event_answers_nothing() {
    let forged = [
        rich_ics(|s| s.replace("mailto:ana@example.com", "mailto:mallory@evil.example")),
        rich_ics(|s| s.replace("20261110T0", "20261111T0")),
    ];
    for (n, ics) in forged.iter().enumerate() {
        let google = FakeGoogle::start(rich());
        let mail = mail_with(ics, 10 + n as u64);
        google.deliver(mail.clone());
        let core = signed_in(&google, &format!("forged-{n}"));
        let got = data(core.run("gws_mail_invite", &json!({"message": mail.id})));
        assert_eq!(got["calendar"]["found"], true, "{n}");
        assert_eq!(got["calendar"]["matches"], false, "{n}");
        let refused = data(core.run(
            "gws_mail_invite_reply",
            &json!({"message": mail.id, "response": "accepted"}),
        ));
        assert_eq!(refused["refused"], "changed", "{n}");
        assert!(google.calendar_writes().is_empty(), "{n}");
    }
    // the invite as it was sent still answers
    let google = FakeGoogle::start(rich());
    let mail = mail_with(&rich_ics(|s| s), 20);
    google.deliver(mail.clone());
    let core = signed_in(&google, "forged-none");
    data(core.run(
        "gws_mail_invite_reply",
        &json!({"message": mail.id, "response": "accepted"}),
    ));
    assert_eq!(google.calendar_writes().len(), 1);
}

#[test]
fn the_organiser_hears_even_when_the_persons_copy_lists_no_one_else() {
    let mut seed = rich();
    for c in &mut seed.calendars {
        for e in &mut c.events {
            e["attendees"] = json!([{"email": FAKE_EMAIL, "responseStatus": "needsAction"}]);
        }
    }
    let google = FakeGoogle::start(seed);
    let core = signed_in(&google, "alone");
    data(core.run(
        "gws_mail_invite_reply",
        &json!({"message": INVITE, "response": "accepted"}),
    ));
    let writes = google.calendar_writes();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].send_updates.as_deref(), Some("all"));
    // Calendar's own answer keeps its rule: nobody else listed, nobody told
    data(core.run(
        "gws_event_rsvp",
        &json!({"calendar": FAKE_EMAIL, "id": "roofinspection", "response": "declined"}),
    ));
    let writes = google.calendar_writes();
    assert_eq!(writes[1].send_updates.as_deref(), Some("none"));
}

#[test]
fn a_series_past_a_page_of_changed_instances_is_answered_on_the_series() {
    let google = FakeGoogle::start(instances_first(7, true));
    google.calendar_page(3);
    let core = signed_in(&google, "pages");
    data(core.run(
        "gws_mail_invite_reply",
        &json!({"message": INVITE, "response": "accepted"}),
    ));
    let writes = google.calendar_writes();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].event, "roofinspection");
}

#[test]
fn an_invite_whose_bytes_didnt_come_asks_for_a_try_again_not_no_invite() {
    let google = FakeGoogle::start(rich());
    let core = signed_in(&google, "busy");
    google.attachments_fail(true);
    let look = core.run("gws_mail_invite", &json!({"thread": INVITE}));
    assert_eq!(look["isError"], true, "{look}");
    assert_eq!(look["structuredContent"]["code"], "busy");
    let reply = core.run(
        "gws_mail_invite_reply",
        &json!({"message": INVITE, "response": "accepted"}),
    );
    assert_eq!(reply["structuredContent"]["code"], "busy", "{reply}");
    assert!(google.calendar_writes().is_empty());
    google.attachments_fail(false);
    let look = data(core.run("gws_mail_invite", &json!({"thread": INVITE})));
    assert_eq!(look["invite"]["title"], "Roof inspection");
}

#[test]
fn an_attachment_gmail_no_longer_holds_is_no_invite_for_good() {
    let google = FakeGoogle::start(rich());
    let core = signed_in(&google, "gone");
    google.attachments_gone(true);
    data(core.run("gws_mail_thread", &json!({"thread": INVITE})));
    let got = data(core.run("gws_mail_invite", &json!({"thread": INVITE})));
    assert_eq!(got["invite"], Value::Null);
    google.attachments_gone(false);
    let got = data(core.run("gws_mail_invite", &json!({"thread": INVITE})));
    assert_eq!(got["invite"], Value::Null, "the view kept it");
    let view = data(core.run("gws_mail_thread", &json!({"thread": INVITE})));
    assert_eq!(view["invite"], Value::Null);
}

#[test]
fn a_damaged_ics_is_a_final_cant_read_not_a_try_again() {
    let google = FakeGoogle::start(rich());
    let core = signed_in(&google, "damaged");
    google.attachments_damaged(true);
    data(core.run("gws_mail_thread", &json!({"thread": INVITE})));
    let got = data(core.run("gws_mail_invite", &json!({"thread": INVITE})));
    assert_eq!(got["invite"], json!({"damaged": true}), "{got}");
    let fetched = |google: &FakeGoogle| {
        google
            .log()
            .iter()
            .filter(|s| s.target.contains("/attachments/"))
            .count()
    };
    let once = fetched(&google);
    let again = data(core.run("gws_mail_invite", &json!({"thread": INVITE})));
    assert_eq!(again["invite"], json!({"damaged": true}));
    assert_eq!(fetched(&google), once, "kept, not fetched again");
}

/// The rich seed with `n` changed instances of the invite's series ahead of
/// it, and the series itself only if `master`.
fn instances_first(n: usize, master: bool) -> Seed {
    let mut seed = rich();
    let primary = seed.calendars.iter_mut().find(|c| c.primary).unwrap();
    let series = primary.events.pop().unwrap();
    for i in 0..n {
        let mut instance = series.clone();
        instance["id"] = json!(format!("roofinspection_{i}"));
        instance["recurringEventId"] = json!("roofinspection");
        primary.events.push(instance);
    }
    if master {
        primary.events.push(series);
    }
    seed
}

#[test]
fn an_instance_is_never_answered_in_place_of_its_series() {
    // only instances: the series isn't there to answer
    let google = FakeGoogle::start(instances_first(3, false));
    let core = signed_in(&google, "instances");
    let refused = data(core.run(
        "gws_mail_invite_reply",
        &json!({"message": INVITE, "response": "accepted"}),
    ));
    assert_eq!(refused["refused"], "not_found");
    assert!(google.calendar_writes().is_empty());
    // past every page it looks at: no answer, rather than an instance's
    let google = FakeGoogle::start(instances_first(25, true));
    google.calendar_page(1);
    let core = signed_in(&google, "past-pages");
    let answer = core.run(
        "gws_mail_invite_reply",
        &json!({"message": INVITE, "response": "accepted"}),
    );
    assert_eq!(answer["isError"], true, "{answer}");
    assert!(google.calendar_writes().is_empty());
}

#[test]
fn a_forgery_with_the_uid_organiser_and_start_but_other_words_answers_nothing() {
    for change in [
        (
            "SUMMARY:Roof inspection",
            "SUMMARY:Roof inspection, all welcome",
        ),
        ("LOCATION:12 Mabini St", "LOCATION:Elsewhere"),
    ] {
        let google = FakeGoogle::start(rich());
        let ics = rich_ics(|s| s.replace(change.0, change.1));
        let mail = mail_with(&ics, 30);
        google.deliver(mail.clone());
        let core = signed_in(&google, "co-attendee");
        let got = data(core.run("gws_mail_invite", &json!({"message": mail.id})));
        assert_eq!(got["calendar"]["matches"], false, "{change:?}");
        // nothing of the event shows under Calendar's name, nor whose answer it holds
        assert_eq!(
            got["calendar"],
            json!({"found": true, "cancelled": false, "matches": false}),
            "{change:?}"
        );
        let refused = data(core.run(
            "gws_mail_invite_reply",
            &json!({"message": mail.id, "response": "accepted"}),
        ));
        assert_eq!(refused["refused"], "changed", "{change:?}");
        assert!(google.calendar_writes().is_empty());
    }
}

#[test]
fn a_written_answer_is_sent_whatever_the_read_after_it_finds() {
    let google = FakeGoogle::start(rich());
    google.calendar_patch_answers_empty(true);
    google.whole_reads_fail(true);
    let core = signed_in(&google, "written");
    let got = data(core.run(
        "gws_mail_invite_reply",
        &json!({"message": INVITE, "response": "accepted"}),
    ));
    assert_eq!(got["sent"], true, "{got}");
    assert_eq!(google.calendar_writes().len(), 1);
}

#[test]
fn a_patch_answered_200_with_a_body_that_wont_parse_is_sent() {
    let google = FakeGoogle::start(rich());
    google.calendar_patch_answers_junk(true);
    let core = signed_in(&google, "junk");
    let got = data(core.run(
        "gws_mail_invite_reply",
        &json!({"message": INVITE, "response": "accepted"}),
    ));
    assert_eq!(got["sent"], true, "{got}");
    assert_eq!(google.calendar_writes().len(), 1);
}

#[test]
fn an_answer_is_said_before_calendar_syncs_so_a_slow_sync_cant_send_it_twice() {
    let google = FakeGoogle::start(rich());
    let core = signed_in(&google, "quick");
    let before = google.log().len();
    let got = data(core.run(
        "gws_mail_invite_reply",
        &json!({"message": INVITE, "response": "accepted"}),
    ));
    assert_eq!(got["sent"], true, "{got}");
    assert_eq!(got["event"]["response"], "accepted", "{got}");
    let synced: Vec<String> = google.log()[before..]
        .iter()
        .filter(|s| s.method == "GET" && s.target.contains("singleEvents=true"))
        .map(|s| s.target.clone())
        .collect();
    assert!(synced.is_empty(), "the sync thread's to do: {synced:?}");
}

#[test]
fn an_event_cancelled_in_calendar_says_so_and_answers_nothing() {
    let mut seed = rich();
    for c in &mut seed.calendars {
        for e in &mut c.events {
            e["status"] = json!("cancelled");
        }
    }
    let google = FakeGoogle::start(seed);
    let core = signed_in(&google, "cancelled");
    let got = data(core.run("gws_mail_invite", &json!({"thread": INVITE})));
    assert_eq!(got["calendar"], json!({"found": true, "cancelled": true}));
    let refused = data(core.run(
        "gws_mail_invite_reply",
        &json!({"message": INVITE, "response": "accepted"}),
    ));
    assert_eq!(refused["message"], "This event was cancelled.");
    assert!(google.calendar_writes().is_empty());
}

#[test]
fn a_lapsed_sign_in_on_the_ics_is_said_as_itself() {
    let google = FakeGoogle::start(rich());
    let core = signed_in(&google, "lapsed");
    google.attachments_answer(401);
    let look = core.run("gws_mail_invite", &json!({"thread": INVITE}));
    assert_eq!(
        look["structuredContent"]["code"], "account_required",
        "{look}"
    );
    // the thread doesn't fetch the `.ics`, so a refused one can't fail it
    let opened = data(core.run("gws_mail_thread", &json!({"thread": INVITE})));
    assert_eq!(opened["invite"], json!({"pending": true}), "{opened}");
    google.attachments_answer(403);
    let look = core.run("gws_mail_invite", &json!({"thread": INVITE}));
    assert_eq!(look["isError"], true, "{look}");
    assert_eq!(look["structuredContent"]["code"], "needs_access", "{look}");
}

#[test]
fn the_pictures_view_keeps_the_invite() {
    let google = FakeGoogle::start(rich());
    let core = signed_in(&google, "pictures");
    data(core.run("gws_mail_thread", &json!({"thread": INVITE})));
    data(core.run("gws_mail_invite", &json!({"thread": INVITE})));
    let view = data(core.run("gws_mail_pictures", &json!({"thread": INVITE})));
    assert_eq!(view["invite"]["title"], "Roof inspection", "{view}");
}

/// Waits a half minute for the stub, as the reader of an opening thread can't.
#[test]
fn a_slow_ics_gives_up_after_half_a_minute_not_ten() {
    let google = FakeGoogle::start(rich());
    let core = signed_in(&google, "slow");
    google.hold("GET", "/attachments/att-000019f000000014-0-0");
    let (tell, heard) = std::sync::mpsc::channel();
    std::thread::scope(|s| {
        s.spawn(|| {
            let _ = tell.send(core.run("gws_mail_invite", &json!({"thread": INVITE})));
        });
        google.wait_for_held();
        let answer = heard.recv_timeout(std::time::Duration::from_secs(50));
        google.release();
        let answer = answer.expect("an answer within the half minute and some");
        assert_eq!(answer["structuredContent"]["code"], "busy", "{answer}");
    });
}

#[test]
fn a_slow_ics_never_holds_up_the_thread_opening() {
    let google = FakeGoogle::start(rich());
    let core = signed_in(&google, "slow-open");
    google.hold("GET", "/attachments/att-000019f000000014-0-0");
    let (tell, heard) = std::sync::mpsc::channel();
    std::thread::scope(|s| {
        s.spawn(|| {
            let _ = tell.send(core.run("gws_mail_thread", &json!({"thread": INVITE})));
        });
        let answer = heard.recv_timeout(std::time::Duration::from_secs(10));
        google.release();
        let answer = answer.expect("the thread opens without the .ics");
        assert_eq!(
            answer["structuredContent"]["invite"],
            json!({"pending": true})
        );
    });
}

/// A mail with the rich invite's `.ics`, from `from`.
fn sent_by(from: &str, n: u64) -> Mail {
    Mail {
        from: from.into(),
        ..mail_with(&rich_ics(|s| s), n)
    }
}

#[test]
fn an_invite_gmail_cant_vouch_for_lights_no_answer() {
    let google = FakeGoogle::start(rich());
    let forged = sent_by("Ana Reyes <calendar-notification@google.com>", 40);
    google.deliver(forged.clone());
    google.forge_sender(&forged.id);
    // vouched for, but not from the organiser's domain nor Google Calendar
    let stranger = sent_by("Mallory <mallory@evil.example>", 41);
    google.deliver(stranger.clone());
    let core = signed_in(&google, "unvouched");
    for mail in [&forged, &stranger] {
        let got = data(core.run("gws_mail_invite", &json!({"message": mail.id})));
        assert_eq!(got["invite"]["sender_ok"], false, "{}", mail.from);
        assert_eq!(
            got["calendar"],
            json!({"unverified": true}),
            "{}",
            mail.from
        );
        let refused = data(core.run(
            "gws_mail_invite_reply",
            &json!({"message": mail.id, "response": "accepted"}),
        ));
        assert_eq!(refused["refused"], "unverified", "{}", mail.from);
    }
    assert!(google.calendar_writes().is_empty());
    // the organiser's own domain, vouched for, answers
    let own = sent_by("Ana Reyes <ana@example.com>", 42);
    google.deliver(own.clone());
    let got = data(core.run("gws_mail_invite", &json!({"message": own.id})));
    assert_eq!(got["calendar"]["matches"], true, "{got}");
    data(core.run(
        "gws_mail_invite_reply",
        &json!({"message": own.id, "response": "accepted"}),
    ));
    assert_eq!(google.calendar_writes().len(), 1);
}

#[test]
fn calendar_not_answering_keeps_the_invite_gmail_sent_with_the_reason() {
    let google = FakeGoogle::start(instances_first(25, true));
    google.calendar_page(1);
    let core = signed_in(&google, "calendar-down");
    let got = data(core.run("gws_mail_invite", &json!({"thread": INVITE})));
    assert_eq!(got["invite"]["title"], "Roof inspection", "{got}");
    assert_eq!(
        got["calendar"],
        json!({"error": "Calendar didn't list this event just now. Try again."})
    );
}

#[test]
fn a_live_copy_of_the_event_wins_over_a_cancelled_one() {
    let mut seed = rich();
    let primary = seed.calendars.iter_mut().find(|c| c.primary).unwrap();
    let live = primary.events.pop().unwrap();
    let mut gone = live.clone();
    gone["id"] = json!("roofinspectionold");
    gone["iCalUID"] = json!("fake-roof-inspection@google.com");
    gone["status"] = json!("cancelled");
    let mut live = live;
    live["iCalUID"] = json!("fake-roof-inspection@google.com");
    primary.events.push(gone);
    primary.events.push(live);
    let google = FakeGoogle::start(seed);
    let core = signed_in(&google, "two-copies");
    let got = data(core.run("gws_mail_invite", &json!({"thread": INVITE})));
    assert_eq!(got["calendar"]["cancelled"], false, "{got}");
    assert_eq!(got["calendar"]["matches"], true, "{got}");
}

#[test]
fn an_unreadable_newer_invite_never_hides_an_older_one() {
    let google = FakeGoogle::start(rich());
    let older = mail_with(&rich_ics(|s| s), 50);
    let newer = Mail {
        id: format!("{:016x}", 0x1a00_0000_0000_u64 + 51),
        at: older.at + 60_000,
        ..mail_with("BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\n", 50)
    };
    google.deliver(older.clone());
    google.deliver(newer);
    let core = signed_in(&google, "newest-that-reads");
    data(core.run("gws_mail_thread", &json!({"thread": older.thread})));
    let got = data(core.run("gws_mail_invite", &json!({"thread": older.thread})));
    assert_eq!(got["invite"]["message"], older.id.as_str(), "{got}");
}

#[test]
fn at_a_public_mail_domain_only_the_organiser_and_google_calendar_send_an_invite() {
    let google = FakeGoogle::start(rich());
    let ics = rich_ics(|s| s.replace("mailto:ana@example.com", "mailto:ana@gmail.com"));
    let from = |who: &str, n: u64| Mail {
        from: who.into(),
        ..mail_with(&ics, n)
    };
    let cases = [
        (from("Stranger <stranger@gmail.com>", 60), false),
        (from("Someone <someone@google.com>", 61), false),
        (from("Ana <ana@gmail.com>", 62), true),
        (from("Ana <calendar-notification@google.com>", 63), true),
    ];
    for (mail, _) in &cases {
        google.deliver(mail.clone());
    }
    let core = signed_in(&google, "public-domains");
    for (mail, lights) in &cases {
        let got = data(core.run("gws_mail_invite", &json!({"message": mail.id})));
        assert_eq!(got["invite"]["sender_ok"], *lights, "{}", mail.from);
        if !lights {
            assert_eq!(
                got["calendar"],
                json!({"unverified": true}),
                "{}",
                mail.from
            );
            let refused = data(core.run(
                "gws_mail_invite_reply",
                &json!({"message": mail.id, "response": "accepted"}),
            ));
            assert_eq!(refused["refused"], "unverified", "{}", mail.from);
        }
    }
    assert!(google.calendar_writes().is_empty());
}

#[test]
fn a_strangers_cancel_after_the_real_invite_never_hides_it() {
    let google = FakeGoogle::start(rich());
    let real = sent_by("Ana Reyes <calendar-notification@google.com>", 70);
    let cancel = Mail {
        id: format!("{:016x}", 0x1a00_0000_0000_u64 + 71),
        at: real.at + 60_000,
        from: "Mallory <mallory@evil.example>".into(),
        ..mail_with(
            &rich_ics(|s| s.replace("METHOD:REQUEST", "METHOD:CANCEL")),
            70,
        )
    };
    google.deliver(real.clone());
    google.deliver(cancel.clone());
    let core = signed_in(&google, "strangers-cancel");
    data(core.run("gws_mail_thread", &json!({"thread": real.thread})));
    let got = data(core.run("gws_mail_invite", &json!({"thread": real.thread})));
    assert_eq!(got["invite"]["message"], real.id.as_str(), "{got}");
    assert_eq!(got["invite"]["method"], "request");
    assert_eq!(got["calendar"]["matches"], true, "{got}");
    // alone, the stranger's cancel is said to be unchecked
    let alone = data(core.run("gws_mail_invite", &json!({"message": cancel.id})));
    assert_eq!(alone["invite"]["method"], "cancel");
    assert_eq!(alone["calendar"], json!({"unverified": true}), "{alone}");
}

#[test]
fn a_cancelled_event_is_said_only_for_the_invite_it_matches() {
    let mut seed = rich();
    for c in &mut seed.calendars {
        for e in &mut c.events {
            e["status"] = json!("cancelled");
        }
    }
    let google = FakeGoogle::start(seed);
    let other = mail_with(
        &rich_ics(|s| s.replace("SUMMARY:Roof inspection", "SUMMARY:Party")),
        72,
    );
    google.deliver(other.clone());
    let core = signed_in(&google, "cancelled-other");
    let got = data(core.run("gws_mail_invite", &json!({"message": other.id})));
    assert_eq!(
        got["calendar"],
        json!({"found": true, "cancelled": false, "matches": false}),
        "{got}"
    );
    let refused = data(core.run(
        "gws_mail_invite_reply",
        &json!({"message": other.id, "response": "accepted"}),
    ));
    assert_eq!(refused["refused"], "changed");
}

#[test]
fn a_patch_that_failed_on_its_way_is_never_a_plain_failure() {
    let google = FakeGoogle::start(rich());
    google.calendar_patch_answers_500(true);
    let core = signed_in(&google, "patch-500");
    let answer = core.run(
        "gws_mail_invite_reply",
        &json!({"message": INVITE, "response": "accepted"}),
    );
    assert_eq!(answer["isError"], true, "{answer}");
    assert_eq!(
        answer["structuredContent"]["code"], "unconfirmed",
        "{answer}"
    );
    assert_eq!(google.calendar_writes().len(), 1, "it landed");
}

#[test]
fn a_saved_invite_judged_by_an_older_sender_rule_is_judged_again() {
    let google = FakeGoogle::start(rich());
    let core = signed_in(&google, "old-rule");
    data(core.run("gws_mail_thread", &json!({"thread": INVITE})));
    {
        let held = core.store().unwrap();
        let store = held.as_ref().unwrap();
        let mut view: Value =
            serde_json::from_str(&store.view(FAKE_EMAIL, INVITE, 1).unwrap().unwrap()).unwrap();
        // as the any-google.com rule saved it
        view["invite"] = json!({"message": INVITE, "uid": "fake-roof-inspection@google.com", "method": "request", "title": "Old words", "start": "2026-11-10T02:00:00Z", "end": "2026-11-10T03:00:00Z", "all_day": false, "place": "", "organizer": "", "organizer_address": "ana@example.com", "sender_ok": true});
        store
            .put_view(FAKE_EMAIL, INVITE, &view.to_string(), 1)
            .unwrap();
    }
    let got = data(core.run("gws_mail_invite", &json!({"thread": INVITE})));
    assert_eq!(got["invite"]["title"], "Roof inspection", "{got}");
}

#[test]
fn a_patch_calendar_refused_is_not_sent_and_a_lapsed_sign_in_is_said() {
    let google = FakeGoogle::start(rich());
    let core = signed_in(&google, "patch-refused");
    for status in [403, 409, 400] {
        google.calendar_patch_refuses(Some(status));
        let answer = core.run(
            "gws_mail_invite_reply",
            &json!({"message": INVITE, "response": "accepted"}),
        );
        assert_ne!(
            answer["structuredContent"]["code"], "unconfirmed",
            "{status}: {answer}"
        );
        let refused = &answer["structuredContent"]["refused"];
        assert!(refused.is_string(), "{status}: {answer}");
    }
    google.calendar_patch_refuses(Some(403));
    let got = data(core.run(
        "gws_mail_invite_reply",
        &json!({"message": INVITE, "response": "accepted"}),
    ));
    assert_eq!(got["refused"], "not_sent", "{got}");
    assert!(
        got["message"]
            .as_str()
            .unwrap()
            .starts_with("Your reply wasn't sent."),
        "{got}"
    );
    google.calendar_patch_refuses(Some(401));
    let lapsed = core.run(
        "gws_mail_invite_reply",
        &json!({"message": INVITE, "response": "accepted"}),
    );
    assert_eq!(
        lapsed["structuredContent"]["code"], "account_required",
        "{lapsed}"
    );
    assert!(google.calendar_writes().is_empty(), "nothing went");
}
