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
fn the_thread_view_carries_the_invites_event_from_its_ics() {
    let google = FakeGoogle::start(rich());
    let core = signed_in(&google, "view");
    let view = data(core.run("gws_mail_thread", &json!({"thread": INVITE})));
    assert_eq!(
        view["invite"],
        json!({"message": INVITE, "uid": "fake-roof-inspection@google.com", "method": "request", "title": "Roof inspection", "start": "2026-11-10T02:00:00Z", "end": "2026-11-10T03:00:00Z", "all_day": false, "place": "12 Mabini St", "organizer": "Ana Reyes", "organizer_address": "ana@example.com"})
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
            json!({"found": true, "matches": true, "guest": true, "response": "needsAction"}),
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
    let view = data(core.run("gws_mail_thread", &json!({"thread": mail.thread})));
    assert_eq!(view["invite"], Value::Null);
    let got = data(core.run("gws_mail_invite", &json!({"message": mail.id})));
    assert_eq!(got["invite"], Value::Null);
}

/// The rich invite's `.ics` with `change` made to it.
fn rich_ics(change: impl Fn(String) -> String) -> String {
    change("BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nDTSTART:20261110T020000Z\r\nDTEND:20261110T030000Z\r\nUID:fake-roof-inspection@google.com\r\nORGANIZER;CN=Ana Reyes:mailto:ana@example.com\r\nATTENDEE;PARTSTAT=NEEDS-ACTION:mailto:owner@example.com\r\nSUMMARY:Roof inspection\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n".to_owned())
}

#[test]
fn an_invite_that_didnt_come_is_asked_for_again_not_kept_as_none() {
    let google = FakeGoogle::start(rich());
    let core = signed_in(&google, "refetch");
    google.attachments_fail(true);
    let view = data(core.run("gws_mail_thread", &json!({"thread": INVITE})));
    assert_eq!(view["invite"], Value::Null);
    google.attachments_fail(false);
    let view = data(core.run("gws_mail_thread", &json!({"thread": INVITE})));
    assert_eq!(view["invite"]["title"], "Roof inspection");
    let got = data(core.run("gws_mail_invite", &json!({"thread": INVITE})));
    assert_eq!(got["invite"]["title"], "Roof inspection");
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
    let view = data(core.run("gws_mail_thread", &json!({"thread": INVITE})));
    assert_eq!(view["invite"], Value::Null);
    google.attachments_gone(false);
    let view = data(core.run("gws_mail_thread", &json!({"thread": INVITE})));
    assert_eq!(view["invite"], Value::Null, "the view was kept");
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
