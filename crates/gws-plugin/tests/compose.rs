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
    let armed = data(core.run("gws_mail_send_arm", &json!({})));
    core.run(
        "gws_mail_send",
        &json!({"draft": draft["draft"], "revision": draft["revision"], "message_id": draft["message_id"], "armed": armed["armed"]}),
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
fn a_send_with_no_press_behind_it_goes_nowhere() {
    let google = FakeGoogle::start(Seed::demo(2));
    let core = signed_in(&google, "unarmed");
    let saved = data(core.run("gws_mail_draft_save", &lunch()));
    let args = |armed: &Value| json!({"draft": saved["draft"], "revision": saved["revision"], "armed": armed});
    let refused = core.run("gws_mail_send", &args(&json!("a1-made-up")));
    assert_eq!(refused["isError"], true, "{refused}");
    assert_eq!(google.sent(), []);

    let press = data(core.run("gws_mail_send_arm", &json!({})));
    data(core.run("gws_mail_send", &args(&press["armed"])));
    let second = data(core.run("gws_mail_draft_save", &lunch()));
    let again = core.run(
        "gws_mail_send",
        &json!({"draft": second["draft"], "revision": second["revision"], "armed": press["armed"]}),
    );
    assert_eq!(again["isError"], true, "a press is good for one send: {again}");
    assert_eq!(google.sent().len(), 1);
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

/// `bytes` handed over in chunks, as a screen does: its upload id.
fn attach(core: &Core, name: &str, bytes: &[u8]) -> Value {
    use base64::Engine as _;
    let mut upload = Value::Null;
    let mut at = 0;
    for chunk in bytes.chunks(180 * 1024) {
        let mut args = json!({"name": name, "type": "application/pdf", "at": at, "data": base64::engine::general_purpose::STANDARD.encode(chunk)});
        if !upload.is_null() {
            args["upload"] = upload.clone();
        }
        let got = data(core.run("gws_upload_chunk", &args));
        upload = got["upload"].clone();
        at = got["size"].as_u64().unwrap();
    }
    upload
}

fn file(size: usize) -> Vec<u8> {
    (0..size).map(|i| (i * 31 % 251) as u8).collect()
}

#[test]
fn a_25_mb_attachment_arrives_whole() {
    let google = FakeGoogle::start(Seed::demo(2));
    let core = signed_in(&google, "big");
    let bytes = file(25 * 1024 * 1024);
    let upload = attach(&core, "año/../report é.pdf", &bytes);
    let mut draft = lunch();
    draft["attachments"] = json!([upload]);
    let saved = data(core.run("gws_mail_draft_save", &draft));
    data(send(&core, &saved));
    let sent = google.sent();
    let [(name, kind, got)] = sent[0].attachments.as_slice() else {
        panic!("one attachment: {}", sent[0].attachments.len());
    };
    assert_eq!(name, "report é.pdf");
    assert_eq!(kind, "application/pdf");
    assert!(got == &bytes, "{} of {} bytes, or changed", got.len(), bytes.len());
    assert_eq!(sent[0].body, "Noon?\nThe usual place.");
    assert_eq!(sent[0].to, "ana@example.com");
    assert_eq!(core.uploads.lock().unwrap().held_bytes(), 0, "sent, so freed");
}

#[test]
fn past_the_limit_is_refused_and_nothing_is_held() {
    use base64::Engine as _;
    let google = FakeGoogle::start(Seed::demo(2));
    let core = signed_in(&google, "over");
    let bytes = file(25 * 1024 * 1024);
    let upload = attach(&core, "a.pdf", &bytes);
    let more = core.run(
        "gws_upload_chunk",
        &json!({"upload": upload, "at": bytes.len(), "data": base64::engine::general_purpose::STANDARD.encode(b"x")}),
    );
    assert_eq!(more["isError"], true, "{more}");
    assert!(more.to_string().contains("25 MB"));
    assert_eq!(core.uploads.lock().unwrap().held_bytes(), 0);
}

#[test]
fn two_files_over_the_limit_together_are_refused_on_saving() {
    let google = FakeGoogle::start(Seed::demo(2));
    let core = signed_in(&google, "both");
    let (a, b) = (file(15 * 1024 * 1024), file(15 * 1024 * 1024));
    let mut draft = lunch();
    draft["attachments"] = json!([attach(&core, "a.pdf", &a), attach(&core, "b.pdf", &b)]);
    let refused = core.run("gws_mail_draft_save", &draft);
    assert_eq!(refused["isError"], true, "{refused}");
    assert!(refused.to_string().contains("25 MB"));
}

#[test]
fn a_chunk_sent_twice_is_kept_once() {
    use base64::Engine as _;
    let google = FakeGoogle::start(Seed::demo(2));
    let core = signed_in(&google, "resend");
    let chunk = base64::engine::general_purpose::STANDARD.encode(b"abc");
    let first = data(core.run("gws_upload_chunk", &json!({"name": "a.txt", "at": 0, "data": chunk})));
    let upload = first["upload"].clone();
    let again = data(core.run("gws_upload_chunk", &json!({"upload": upload, "at": 0, "data": chunk})));
    assert_eq!(again["size"], 3);
    let next = data(core.run("gws_upload_chunk", &json!({"upload": upload, "at": 3, "data": chunk})));
    assert_eq!(next["size"], 6);
}

#[test]
fn saving_a_draft_again_keeps_the_files_it_had() {
    let google = FakeGoogle::start(Seed::demo(2));
    let core = signed_in(&google, "keep");
    let bytes = file(1024);
    let mut draft = lunch();
    draft["attachments"] = json!([attach(&core, "a.pdf", &bytes)]);
    let saved = data(core.run("gws_mail_draft_save", &draft));
    let read = data(core.run("gws_mail_draft_read", &json!({"draft": saved["draft"]})));
    let files = read["attachments"].as_array().unwrap();
    assert_eq!(files.len(), 1, "{read}");
    assert_eq!(files[0]["name"], "a.pdf");

    let mut edit = lunch();
    edit["draft"] = saved["draft"].clone();
    edit["body"] = json!("Edited");
    edit["keep"] = json!([{"message": read["revision"], "id": files[0]["id"], "name": files[0]["name"], "mime": files[0]["mime"]}]);
    let saved = data(core.run("gws_mail_draft_save", &edit));
    data(send(&core, &saved));
    let sent = google.sent();
    assert_eq!(sent[0].body, "Edited");
    assert_eq!(sent[0].attachments.len(), 1);
    assert!(sent[0].attachments[0].2 == bytes);
}

#[test]
fn the_model_attaches_nothing() {
    let google = FakeGoogle::start(Seed::demo(2));
    let core = signed_in(&google, "model-file");
    let upload = attach(&core, "private.pdf", b"secret");
    let mut draft = lunch();
    draft["attachments"] = json!([upload]);
    let refused = core.run_for_model("gws_mail_draft_save", &draft);
    assert_eq!(refused["isError"], true, "{refused}");
}

#[test]
fn a_save_too_slow_for_its_caller_can_be_asked_after() {
    let google = FakeGoogle::start(Seed::demo(2));
    let core = signed_in(&google, "job");
    assert_eq!(data(core.run("gws_mail_job", &json!({"job": "j1"})))["done"], false);
    let mut draft = lunch();
    draft["job"] = json!("j1");
    let saved = data(core.run("gws_mail_draft_save", &draft));
    let job = data(core.run("gws_mail_job", &json!({"job": "j1"})));
    assert_eq!(job["done"], true);
    assert_eq!(job["answer"]["structuredContent"]["draft"], saved["draft"]);
    assert_eq!(data(core.run("gws_mail_job", &json!({"job": "j1"})))["done"], false, "told once");
}

fn suggested(core: &Core, query: &str) -> Vec<String> {
    let found = data(core.run("gws_people_suggest", &json!({"query": query})));
    found["people"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["address"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn suggestions_come_from_mail_and_contacts_and_leave_out_the_person() {
    let google = FakeGoogle::start(Seed::demo(12));
    let core = signed_in(&google, "people");
    core.unlock("a-seed");
    gws::mail::sync(&core).unwrap();
    assert_eq!(suggested(&core, "ana"), ["ana@example.com"], "one person, once");
    assert_eq!(suggested(&core, "zed"), ["zed@contacts.example"]);
    assert_eq!(suggested(&core, "owner"), Vec::<String>::new());
    assert_eq!(suggested(&core, "  "), Vec::<String>::new());
    assert_eq!(google.writes(), [], "suggesting changes nothing");
}

#[test]
fn a_sign_in_without_contacts_still_suggests_who_wrote() {
    let mut seed = Seed::demo(12);
    seed.only_grants = Some("gmail.modify".to_owned());
    let google = FakeGoogle::start(seed);
    let core = signed_in(&google, "no-contacts");
    core.unlock("a-seed");
    gws::mail::sync(&core).unwrap();
    assert_eq!(suggested(&core, "ana"), ["ana@example.com"]);
    assert_eq!(suggested(&core, "zed"), Vec::<String>::new());
}
