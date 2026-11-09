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
        json!({"message": INVITE, "uid": "fake-roof-inspection@google.com", "method": "request", "title": "Roof inspection", "start": "2026-11-10T02:00:00Z", "end": "2026-11-10T03:00:00Z", "all_day": false, "place": "12 Mabini St", "organizer": "Ana Reyes"})
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
            json!({"found": true, "guest": true, "response": "needsAction"}),
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
