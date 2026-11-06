//! Drafts and Send against the stub Google: what's written is what arrives.

use gws::Core;
use gws::apis::Apis;
use gws_fake_google::{FAKE_EMAIL, FakeGoogle, Seed};
use serde_json::{Value, json};

fn signed_in(google: &FakeGoogle, name: &str) -> Core {
    let dir = std::env::temp_dir().join(format!("gws-compose-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let core = Core::new(&Apis::at(&google.base()), Ok(gws_store::Paths::in_dir(dir)));
    core.accounts.set(&json!({"account": "google", "label": "default", "provider": "google", "subject": FAKE_EMAIL, "scopes": ["https://www.googleapis.com/auth/gmail.modify"], "access_token": "fake-access-1", "token_type": "Bearer", "expires_at": 4_000_000_000u64}));
    core
}

fn data(answer: Value) -> Value {
    assert_ne!(answer["isError"], true, "{answer}");
    answer["structuredContent"].clone()
}

fn lunch() -> Value {
    json!({"to": ["Ana <ana@example.com>"], "cc": ["ben@example.com"], "bcc": ["cara@example.com"], "subject": "Lunch ñ", "body": "Noon?\nThe usual place."})
}

fn send(core: &Core, draft: &Value) -> Value {
    core.run(
        "gws_mail_send",
        &json!({"draft": draft["draft"], "revision": draft["revision"], "message_id": draft["message_id"]}),
    )
}

#[test]
fn what_was_read_is_what_arrives() {
    let google = FakeGoogle::start(Seed::demo(2));
    let core = signed_in(&google, "exact");
    let saved = data(core.run("gws_mail_draft_save", &lunch()));
    assert_eq!(google.sent(), [], "saving sends nothing");
    let read = data(core.run("gws_mail_draft_read", &json!({"draft": saved["draft"]})));
    assert_eq!(read["to"], "ana@example.com");
    assert_eq!(read["cc"], "ben@example.com");
    assert_eq!(read["bcc"], "cara@example.com");
    assert_eq!(read["subject"], "Lunch ñ");
    assert_eq!(read["body"], "Noon?\nThe usual place.");

    data(send(&core, &read));
    let sent = google.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].to, "ana@example.com");
    assert_eq!(sent[0].cc, "ben@example.com");
    assert_eq!(sent[0].bcc, "cara@example.com");
    assert_eq!(sent[0].subject, "Lunch ñ");
    assert_eq!(sent[0].body, "Noon?\nThe usual place.");
}

#[test]
fn a_draft_changed_since_it_was_read_is_not_sent() {
    let google = FakeGoogle::start(Seed::demo(2));
    let core = signed_in(&google, "changed");
    let read = data(core.run("gws_mail_draft_save", &lunch()));
    let mut edit = lunch();
    edit["draft"] = read["draft"].clone();
    edit["to"] = json!(["someone-else@example.com"]);
    let edited = data(core.run("gws_mail_draft_save", &edit));
    assert_ne!(edited["revision"], read["revision"]);

    let refused = send(&core, &read);
    assert_eq!(refused["isError"], true, "{refused}");
    assert!(refused.to_string().contains("changed after you read it"));
    assert_eq!(google.sent(), []);
}

#[test]
fn a_send_pressed_again_after_a_lost_answer_goes_once() {
    let google = FakeGoogle::start(Seed::demo(2));
    let core = signed_in(&google, "twice");
    let saved = data(core.run("gws_mail_draft_save", &lunch()));
    data(send(&core, &saved));
    let again = data(send(&core, &saved));
    assert_eq!(again["already"], true);
    assert_eq!(google.sent().len(), 1);
}

#[test]
fn a_draft_deleted_elsewhere_says_so_and_nothing_goes() {
    let google = FakeGoogle::start(Seed::demo(2));
    let core = signed_in(&google, "gone");
    let saved = data(core.run("gws_mail_draft_save", &lunch()));
    data(core.run("gws_mail_draft_delete", &json!({"draft": saved["draft"]})));
    let refused = send(&core, &saved);
    assert_eq!(refused["isError"], true, "{refused}");
    assert!(refused.to_string().contains("That draft is gone."));
    assert_eq!(google.sent(), []);
}

#[test]
fn the_model_rewrites_only_what_it_wrote_and_the_person_left_alone() {
    let google = FakeGoogle::start(Seed::demo(2));
    let core = signed_in(&google, "model");
    let theirs = data(core.run("gws_mail_draft_save", &lunch()));
    let mut over = lunch();
    over["draft"] = theirs["draft"].clone();
    let refused = core.run_for_model("gws_mail_draft_save", &over);
    assert_eq!(refused["isError"], true, "{refused}");
    assert!(refused.to_string().contains("the person's"));

    let mine = data(core.run_for_model("gws_mail_draft_save", &lunch()));
    let mut again = lunch();
    again["draft"] = mine["draft"].clone();
    data(core.run_for_model("gws_mail_draft_save", &again));

    data(core.run("gws_mail_draft_save", &again));
    let refused = core.run_for_model("gws_mail_draft_save", &again);
    assert_eq!(refused["isError"], true, "the person's edit made it theirs");
}

/// A conversation: its id, from the sender given, with one message from them.
fn thread_from(core: &Core) -> Value {
    gws::mail::sync(core).unwrap();
    let listed = data(core.run("gws_mail_list", &json!({"label": "INBOX"})));
    listed["threads"][0]["id"].clone()
}

#[test]
fn a_reply_goes_to_the_sender_and_reply_all_leaves_out_the_person() {
    let google = FakeGoogle::start(Seed::demo(3));
    let core = signed_in(&google, "reply");
    core.unlock("a-seed");
    let id = thread_from(&core);
    let reply = data(core.run("gws_mail_reply_start", &json!({"thread": id, "mode": "reply"})));
    let to = reply["to"].as_array().unwrap();
    assert_eq!(to.len(), 1, "{reply}");
    assert_ne!(to[0], FAKE_EMAIL);
    assert!(reply["subject"].as_str().unwrap().starts_with("Re: "));
    assert_eq!(reply["thread"], id);
    let all = data(core.run("gws_mail_reply_start", &json!({"thread": id, "mode": "all"})));
    assert!(!all["to"].to_string().contains(FAKE_EMAIL), "{all}");
    let forward = data(core.run("gws_mail_reply_start", &json!({"thread": id, "mode": "forward"})));
    assert_eq!(forward["to"], json!([]));
    assert!(forward["subject"].as_str().unwrap().starts_with("Fwd: "));
    assert_eq!(forward["thread"], Value::Null, "a forward starts its own");
}

#[test]
fn a_reply_to_ones_own_last_mail_goes_on_to_whoever_it_went_to() {
    let google = FakeGoogle::start(Seed::demo(3));
    let core = signed_in(&google, "self");
    core.unlock("a-seed");
    let id = thread_from(&core);
    let mut start = data(core.run("gws_mail_reply_start", &json!({"thread": id, "mode": "reply"})));
    let them = start["to"][0].clone();
    start["body"] = json!("Bump");
    let saved = data(core.run("gws_mail_draft_save", &start));
    data(send(&core, &saved));

    let again = data(core.run("gws_mail_reply_start", &json!({"thread": id, "mode": "reply"})));
    assert_eq!(again["to"], json!([them]), "{again}");
}

#[test]
fn an_address_that_would_start_a_header_is_refused_before_anything_leaves() {
    let google = FakeGoogle::start(Seed::demo(2));
    let core = signed_in(&google, "smuggle");
    let refused = core.run(
        "gws_mail_draft_save",
        &json!({"to": ["a@b.c\r\nBcc: spy@example.com"], "body": "x"}),
    );
    assert_eq!(refused["isError"], true, "{refused}");
    assert_eq!(google.sent(), []);
}
