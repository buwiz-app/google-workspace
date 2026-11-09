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
    let armed = data(core.run(
        "gws_mail_send_arm",
        &json!({"draft": draft["draft"], "revision": draft["revision"]}),
    ));
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
    assert_eq!(read["to"], json!(["ana@example.com"]));
    assert_eq!(read["cc"], json!(["ben@example.com"]));
    assert_eq!(read["bcc"], json!(["cara@example.com"]));
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
    assert!(
        refused["structuredContent"]["refused"].is_string(),
        "{refused}"
    );
    assert_eq!(google.sent(), []);

    let press = data(core.run(
        "gws_mail_send_arm",
        &json!({"draft": saved["draft"], "revision": saved["revision"]}),
    ));
    data(core.run("gws_mail_send", &args(&press["armed"])));
    let second = data(core.run("gws_mail_draft_save", &lunch()));
    let again = core.run(
        "gws_mail_send",
        &json!({"draft": second["draft"], "revision": second["revision"], "armed": press["armed"]}),
    );
    assert!(
        again["structuredContent"]["refused"].is_string(),
        "a press is good for one send: {again}"
    );
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
    assert!(
        refused["structuredContent"]["refused"].is_string(),
        "{refused}"
    );
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
    assert!(
        refused["structuredContent"]["refused"].is_string(),
        "{refused}"
    );
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
    let reply = data(core.run(
        "gws_mail_reply_start",
        &json!({"thread": id, "mode": "reply"}),
    ));
    let to = reply["to"].as_array().unwrap();
    assert_eq!(to.len(), 1, "{reply}");
    assert_ne!(to[0], FAKE_EMAIL);
    assert!(reply["subject"].as_str().unwrap().starts_with("Re: "));
    assert_eq!(reply["thread"], id);
    let all = data(core.run(
        "gws_mail_reply_start",
        &json!({"thread": id, "mode": "all"}),
    ));
    assert!(!all["to"].to_string().contains(FAKE_EMAIL), "{all}");
    let forward = data(core.run(
        "gws_mail_reply_start",
        &json!({"thread": id, "mode": "forward"}),
    ));
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
    let mut start = data(core.run(
        "gws_mail_reply_start",
        &json!({"thread": id, "mode": "reply"}),
    ));
    let them = start["to"][0].clone();
    start["body"] = json!("Bump");
    let saved = data(core.run("gws_mail_draft_save", &start));
    data(send(&core, &saved));

    let again = data(core.run(
        "gws_mail_reply_start",
        &json!({"thread": id, "mode": "reply"}),
    ));
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
    assert!(
        got == &bytes,
        "{} of {} bytes, or changed",
        got.len(),
        bytes.len()
    );
    assert_eq!(sent[0].body, "Noon?\nThe usual place.");
    assert_eq!(sent[0].to, "ana@example.com");
    assert_eq!(
        core.uploads.lock().unwrap().held_bytes(),
        0,
        "sent, so freed"
    );
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
    let first = data(core.run(
        "gws_upload_chunk",
        &json!({"name": "a.txt", "at": 0, "data": chunk}),
    ));
    let upload = first["upload"].clone();
    let again = data(core.run(
        "gws_upload_chunk",
        &json!({"upload": upload, "at": 0, "data": chunk}),
    ));
    assert_eq!(again["size"], 3);
    let next = data(core.run(
        "gws_upload_chunk",
        &json!({"upload": upload, "at": 3, "data": chunk}),
    ));
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
    let started = data(core.run("gws_mail_job_start", &json!({})));
    let j1 = started["job"].clone();
    let asked = data(core.run("gws_mail_job", &json!({"job": j1})));
    assert_eq!(
        (&asked["done"], &asked["gone"]),
        (&json!(false), &json!(false))
    );
    let mut draft = lunch();
    draft["job"] = j1.clone();
    let saved = data(core.run("gws_mail_draft_save", &draft));
    let job = data(core.run("gws_mail_job", &json!({"job": j1})));
    assert_eq!(job["done"], true);
    assert_eq!(job["answer"]["structuredContent"]["draft"], saved["draft"]);
    let told = data(core.run("gws_mail_job", &json!({"job": j1})));
    assert_eq!(told["gone"], true, "told once");
}

#[test]
fn a_job_the_plugin_did_not_start_plants_nothing_and_holds_nobody() {
    let google = FakeGoogle::start(Seed::demo(2));
    let core = signed_in(&google, "job-model");
    let started = data(core.run("gws_mail_job_start", &json!({})));
    let mut draft = lunch();
    draft["job"] = started["job"].clone();
    data(core.run_for_model("gws_mail_draft_save", &draft));
    let asked = data(core.run("gws_mail_job", &json!({"job": started["job"]})));
    assert_eq!(
        asked["done"], false,
        "the model can't answer for it: {asked}"
    );

    draft["job"] = json!("made-up");
    data(core.run("gws_mail_draft_save", &draft));
    let made_up = data(core.run("gws_mail_job", &json!({"job": "made-up"})));
    assert_eq!(
        (&made_up["done"], &made_up["gone"]),
        (&json!(false), &json!(true))
    );
}

#[test]
fn a_running_job_is_never_pushed_out() {
    let google = FakeGoogle::start(Seed::demo(2));
    let core = signed_in(&google, "jobs-full");
    let first = data(core.run("gws_mail_job_start", &json!({})));
    for _ in 1..16 {
        data(core.run("gws_mail_job_start", &json!({})));
    }
    let refused = core.run("gws_mail_job_start", &json!({}));
    assert_eq!(refused["isError"], true, "{refused}");
    let asked = data(core.run("gws_mail_job", &json!({"job": first["job"]})));
    assert_eq!(asked["gone"], false, "the first is still someone's");
}

#[test]
fn failed_saves_leave_nothing_held() {
    let google = FakeGoogle::start(Seed::demo(2));
    let core = signed_in(&google, "held");
    let bytes = file(1024);
    let mut draft = lunch();
    draft["attachments"] = json!([attach(&core, "a.pdf", &bytes)]);
    let saved = data(core.run("gws_mail_draft_save", &draft));
    let read = data(core.run("gws_mail_draft_read", &json!({"draft": saved["draft"]})));
    let file0 = &read["attachments"][0];
    let good = json!({"message": read["revision"], "id": file0["id"], "name": "a.pdf", "mime": "application/pdf"});
    let before = core.uploads.lock().unwrap().held_bytes();
    for bad in [
        json!({"message": "nope!", "id": "x", "name": "b.pdf", "mime": "application/pdf"}),
        json!({"message": read["revision"], "id": "att-9", "name": "b.pdf", "mime": "application/pdf"}),
    ] {
        let mut edit = lunch();
        edit["draft"] = saved["draft"].clone();
        edit["keep"] = json!([good, bad]);
        let refused = core.run("gws_mail_draft_save", &edit);
        assert_eq!(refused["isError"], true, "{refused}");
        assert_eq!(
            core.uploads.lock().unwrap().held_bytes(),
            before,
            "nothing left behind"
        );
    }
}

#[test]
fn a_long_subject_is_read_whole() {
    let google = FakeGoogle::start(Seed::demo(2));
    let core = signed_in(&google, "subject");
    let subject = "Long ".repeat(400).trim_end().to_owned();
    let saved = data(core.run(
        "gws_mail_draft_save",
        &json!({"to": ["ana@example.com"], "subject": subject, "body": "Hi"}),
    ));
    let read = data(core.run("gws_mail_draft_read", &json!({"draft": saved["draft"]})));
    assert_eq!(read["subject"], subject.as_str());
}

#[test]
fn a_press_sends_only_the_draft_and_revision_it_was_for_and_a_refusal_spends_it() {
    let google = FakeGoogle::start(Seed::demo(2));
    let core = signed_in(&google, "bound");
    let mine = data(core.run("gws_mail_draft_save", &lunch()));
    let other = data(core.run("gws_mail_draft_save", &lunch()));
    let press = data(core.run(
        "gws_mail_send_arm",
        &json!({"draft": mine["draft"], "revision": mine["revision"]}),
    ));
    let elsewhere = core.run(
        "gws_mail_send",
        &json!({"draft": other["draft"], "revision": other["revision"], "armed": press["armed"]}),
    );
    assert!(
        elsewhere["structuredContent"]["refused"].is_string(),
        "{elsewhere}"
    );
    let spent = core.run(
        "gws_mail_send",
        &json!({"draft": mine["draft"], "revision": mine["revision"], "armed": press["armed"]}),
    );
    assert!(
        spent["structuredContent"]["refused"].is_string(),
        "a refusal spent it: {spent}"
    );
    assert_eq!(google.sent(), []);

    let press = data(core.run("gws_mail_send_arm", &json!({})));
    let unbound = core.run(
        "gws_mail_send",
        &json!({"draft": mine["draft"], "revision": mine["revision"], "armed": press["armed"]}),
    );
    assert!(
        unbound["structuredContent"]["refused"].is_string(),
        "bound to nothing, it sends nothing"
    );

    let press = data(core.run("gws_mail_send_arm", &json!({})));
    let mut again = lunch();
    again["draft"] = mine["draft"].clone();
    again["armed"] = press["armed"].clone();
    let saved = data(core.run("gws_mail_draft_save", &again));
    data(core.run(
        "gws_mail_send",
        &json!({"draft": saved["draft"], "revision": saved["revision"], "armed": press["armed"]}),
    ));
    assert_eq!(google.sent().len(), 1, "the save bound it");
}

#[test]
fn saving_a_sent_draft_again_says_so_and_writes_nothing() {
    let google = FakeGoogle::start(Seed::demo(2));
    let core = signed_in(&google, "resave");
    let saved = data(core.run("gws_mail_draft_save", &lunch()));
    data(send(&core, &saved));
    let mut again = lunch();
    again["draft"] = saved["draft"].clone();
    again["message_id"] = saved["message_id"].clone();
    let refused = core.run("gws_mail_draft_save", &again);
    assert_eq!(
        refused["structuredContent"]["refused"], "already_sent",
        "{refused}"
    );
    assert_eq!(google.sent().len(), 1);
}

#[test]
fn a_long_list_is_read_whole() {
    let google = FakeGoogle::start(Seed::demo(2));
    let core = signed_in(&google, "whole");
    let many: Vec<String> = (0..60)
        .map(|n| format!("person{n}@long-domain-name.example"))
        .collect();
    let saved = data(core.run("gws_mail_draft_save", &json!({"to": many, "body": "Hi"})));
    let read = data(core.run(
        "gws_mail_draft_read",
        &json!({"draft": saved["draft"], "review": true}),
    ));
    assert_eq!(read["to"], json!(many), "every one, none cut");
    assert_eq!(read["new"], json!(many));
}

#[test]
fn reply_all_keeps_everyone_on_a_long_list() {
    let google = FakeGoogle::start(Seed::demo(2));
    let core = signed_in(&google, "reply-all");
    let many: Vec<String> = (0..60)
        .map(|n| format!("person{n}@long-domain-name.example"))
        .collect();
    let saved = data(core.run("gws_mail_draft_save", &json!({"to": many, "body": "Hi"})));
    data(send(&core, &saved));
    core.unlock("a-seed");
    let sent = data(core.run("gws_mail_list", &json!({"label": "SENT"})));
    let id = sent["threads"][0]["id"].clone();
    let all = data(core.run(
        "gws_mail_reply_start",
        &json!({"thread": id, "mode": "all"}),
    ));
    assert_eq!(all["to"].as_array().unwrap().len(), 60, "{all}");
}

#[test]
fn over_the_encoded_limit_is_refused_before_it_goes_up() {
    let google = FakeGoogle::start(Seed::demo(2));
    let core = signed_in(&google, "encoded");
    let bytes = file(25 * 1024 * 1024);
    let mut draft = lunch();
    draft["body"] = json!("x".repeat(1024 * 1024));
    draft["attachments"] = json!([attach(&core, "a.pdf", &bytes)]);
    let refused = core.run("gws_mail_draft_save", &draft);
    assert_eq!(refused["isError"], true);
    assert!(
        refused.to_string().contains("once it's encoded"),
        "{refused}"
    );
}

#[test]
fn saving_twice_keeps_a_drafts_file_though_its_message_changed() {
    let google = FakeGoogle::start(Seed::demo(2));
    let core = signed_in(&google, "twice-keep");
    let bytes = file(2048);
    let mut draft = lunch();
    draft["attachments"] = json!([attach(&core, "a.pdf", &bytes)]);
    let saved = data(core.run("gws_mail_draft_save", &draft));
    let read = data(core.run("gws_mail_draft_read", &json!({"draft": saved["draft"]})));
    let file0 = &read["attachments"][0];
    let mut edit = lunch();
    edit["draft"] = saved["draft"].clone();
    edit["keep"] = json!([{"message": read["revision"], "id": file0["id"], "name": file0["name"], "mime": file0["mime"]}]);
    let second = data(core.run("gws_mail_draft_save", &edit));
    let upload = second["kept"][0]["upload"].clone();
    assert!(upload.is_string(), "{second}");
    let mut third = lunch();
    third["draft"] = saved["draft"].clone();
    third["attachments"] = json!([upload]);
    let third = data(core.run("gws_mail_draft_save", &third));
    data(send(&core, &third));
    assert!(google.sent()[0].attachments[0].2 == bytes);
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
fn calendar_gets_contacts_and_no_one_from_the_mail() {
    let google = FakeGoogle::start(Seed::demo(12));
    let core = signed_in(&google, "invitees");
    core.unlock("a-seed");
    gws::mail::sync(&core).unwrap();
    assert_eq!(
        suggested(&core, "ana"),
        ["ana@example.com"],
        "mail knows Ana"
    );
    let found =
        |query: &str| data(core.run("gws_cal_people", &json!({"query": query})))["people"].clone();
    assert_eq!(found("ana"), json!([]), "Calendar reads no mail");
    assert_eq!(found("zed")[0]["address"], "zed@contacts.example");
}

#[test]
fn suggestions_come_from_mail_and_contacts_and_leave_out_the_person() {
    let google = FakeGoogle::start(Seed::demo(12));
    let core = signed_in(&google, "people");
    core.unlock("a-seed");
    gws::mail::sync(&core).unwrap();
    assert_eq!(
        suggested(&core, "ana"),
        ["ana@example.com"],
        "one person, once"
    );
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

#[test]
fn a_chats_draft_comes_with_its_card_and_a_review_names_who_is_new() {
    let google = FakeGoogle::start(Seed::demo(2));
    let core = signed_in(&google, "card");
    let first = data(core.run(
        "gws_mail_draft_save",
        &json!({"to": ["ana@example.com"], "body": "Hi"}),
    ));
    data(send(&core, &first));

    let result = core.run_for_chat(
        "gws_mail_draft_save",
        &json!({"to": ["ana@example.com"], "cc": ["new@example.com"], "subject": "Hi", "body": "Noon?"}),
    );
    let open = &result["_meta"]["buwiz/open"];
    assert_eq!(open["app"], "mail");
    assert_eq!(open["placement"], "inline");
    assert_eq!(
        open["title"], "Mail",
        "its full screen is the whole of Mail"
    );
    let ticket = open["props"]["draft"].clone();
    assert!(
        !result["_meta"].to_string().contains("Noon?"),
        "nothing of the mail in it"
    );

    let read = data(core.run("gws_mail_draft_read", &json!({"ticket": ticket})));
    assert_eq!(read["from"], FAKE_EMAIL);
    assert_eq!(read["body"], "Noon?");
    assert_eq!(
        read["new"],
        json!(["new@example.com"]),
        "ana's been written to"
    );
    let stale = core.run("gws_mail_draft_read", &json!({"ticket": "nonsense"}));
    assert_eq!(stale["isError"], true, "{stale}");
}

#[test]
fn a_forward_carries_the_files_of_the_mail_it_forwards() {
    let google = FakeGoogle::start(Seed::demo(2));
    let core = signed_in(&google, "forward");
    let bytes = file(4096);
    let mut first = lunch();
    first["attachments"] = json!([attach(&core, "a.pdf", &bytes)]);
    let saved = data(core.run("gws_mail_draft_save", &first));
    data(send(&core, &saved));

    core.unlock("a-seed");
    let sent = data(core.run("gws_mail_list", &json!({"label": "SENT"})));
    let id = sent["threads"][0]["id"].clone();
    let start = data(core.run(
        "gws_mail_reply_start",
        &json!({"thread": id, "mode": "forward"}),
    ));
    let files = start["attachments"].as_array().unwrap();
    assert_eq!(files.len(), 1, "{start}");
    let keep = json!([{"message": start["source"], "id": files[0]["id"], "name": files[0]["name"], "mime": files[0]["mime"]}]);
    let forward = json!({"to": ["ben@example.com"], "subject": start["subject"], "body": start["body"], "keep": keep});
    let saved = data(core.run("gws_mail_draft_save", &forward));
    data(send(&core, &saved));
    let sent = google.sent();
    assert_eq!(sent.len(), 2);
    assert!(sent[1].attachments[0].2 == bytes);
}

/// A forward carries every part on, as before the reader's cards: the
/// pictures the letter shows, and an invite's two parts.
#[test]
fn a_forward_keeps_the_letters_pictures_and_every_part() {
    let google = FakeGoogle::start(Seed::default().with_rich_mail("http://127.0.0.1:1"));
    let core = signed_in(&google, "forward-rich");
    let files = |thread: &str| {
        let start = data(core.run(
            "gws_mail_reply_start",
            &json!({"thread": thread, "mode": "forward"}),
        ));
        let names: Vec<String> = start["attachments"]
            .as_array()
            .unwrap_or_else(|| panic!("{start}"))
            .iter()
            .map(|f| f["name"].as_str().unwrap().to_owned())
            .collect();
        names
    };
    // the newsletter, and the invite
    assert_eq!(files("000019f000000028"), ["logo.png"]);
    assert_eq!(files("000019f000000014"), ["invite.ics", "invite.ics"]);
}

/// The newsletter's logo comes in its part, with no attachment id, as Gmail
/// sends a small one: the forward still carries it, through a real save.
#[test]
fn a_forward_carries_a_small_inline_picture_from_its_part() {
    let google = FakeGoogle::start(Seed::default().with_rich_mail("http://127.0.0.1:1"));
    google.small_files_in_parts(64 * 1024);
    let core = signed_in(&google, "forward-small");
    let start = data(core.run(
        "gws_mail_reply_start",
        &json!({"thread": "000019f000000028", "mode": "forward"}),
    ));
    let files = start["attachments"].as_array().unwrap();
    assert_eq!(files.len(), 1, "{start}");
    assert_eq!(files[0]["id"], Value::Null, "this is the case being tested");
    let keep = json!([{"message": start["source"], "id": files[0]["id"], "part": files[0]["part"], "name": files[0]["name"], "mime": files[0]["mime"]}]);
    let forward = json!({"to": ["ben@example.com"], "subject": start["subject"], "body": start["body"], "keep": keep});
    let saved = data(core.run("gws_mail_draft_save", &forward));
    assert_eq!(saved["kept"].as_array().map(Vec::len), Some(1), "{saved}");
    data(send(&core, &saved));
    let sent = google.sent();
    let logo = &sent.last().unwrap().attachments[0];
    assert_eq!(logo.0, "logo.png");
    assert_eq!(&logo.2[..8], b"\x89PNG\r\n\x1a\n");
}

#[test]
fn sent_and_drafts_name_who_the_mail_is_for() {
    let google = FakeGoogle::start(Seed::demo(3));
    let core = signed_in(&google, "addressed");
    core.unlock("a-seed");
    let rows =
        |label: &str| data(core.run("gws_mail_list", &json!({"label": label})))["threads"].clone();

    let draft = data(core.run("gws_mail_draft_save", &lunch()));
    assert_eq!(rows("DRAFT")[0]["sender"], "To: ana@example.com");
    data(send(&core, &draft));
    assert_eq!(rows("SENT")[0]["sender"], "To: ana@example.com");
    assert!(
        !rows("INBOX").to_string().contains("To: "),
        "the inbox still names who wrote"
    );
}

#[test]
fn a_sent_draft_leaves_drafts_at_the_next_check() {
    let google = FakeGoogle::start(Seed::demo(3));
    let core = signed_in(&google, "sent-draft");
    core.unlock("a-seed");
    gws::mail::sync(&core).unwrap();
    let drafts = || data(core.run("gws_mail_list", &json!({"label": "DRAFT"})))["threads"].clone();

    let draft = data(core.run("gws_mail_draft_save", &lunch()));
    assert_eq!(drafts().as_array().unwrap().len(), 1);
    data(send(&core, &draft));
    gws::mail::sync(&core).unwrap();
    assert_eq!(drafts(), json!([]));
}

/// The model's draft, shown on a card, with the person's press on it.
fn armed_card(core: &Core) -> (Value, Value) {
    let shown = data(core.run_for_model("gws_mail_draft_save", &lunch()));
    let press = data(core.run(
        "gws_mail_send_arm",
        &json!({"draft": shown["draft"], "revision": shown["revision"]}),
    ));
    (shown, press["armed"].clone())
}

fn rewrite(shown: &Value) -> Value {
    json!({"draft": shown["draft"], "to": ["eve@example.com"], "body": "Rewritten"})
}

fn rearmed(core: &Core, shown: &Value) -> (Value, Value) {
    let args = json!({"draft": shown["draft"], "revision": shown["revision"]});
    let armed = data(core.run("gws_mail_send_arm", &args))["armed"].clone();
    (shown.clone(), armed)
}

fn send_args(shown: &Value, armed: &Value) -> Value {
    json!({"draft": shown["draft"], "revision": shown["revision"], "message_id": shown["message_id"], "armed": armed})
}

#[test]
fn a_press_stops_the_model_rewriting_the_draft_until_undo() {
    let google = FakeGoogle::start(Seed::demo(2));
    let core = signed_in(&google, "armed");
    let (shown, armed) = armed_card(&core);
    let writes = google.writes().len();
    let refused = core.run_for_model("gws_mail_draft_save", &rewrite(&shown));
    assert_eq!(refused["isError"], true, "{refused}");
    assert!(refused.to_string().contains("in hand"), "{refused}");
    assert_eq!(google.writes().len(), writes, "nothing reached Google");

    data(core.run("gws_mail_send_disarm", &json!({"armed": armed})));
    data(core.run_for_model("gws_mail_draft_save", &rewrite(&shown)));
    let spent = core.run("gws_mail_send", &send_args(&shown, &armed));
    assert!(
        spent["structuredContent"]["refused"].is_string(),
        "Undo spent the press: {spent}"
    );
    assert_eq!(google.sent(), []);
}

/// However a send fails, the press is spent: the AI gets the draft back, and
/// the account can be switched.
#[test]
fn a_send_that_never_reached_the_draft_still_spends_its_press() {
    let google = FakeGoogle::start(Seed::demo(2));
    let core = signed_in(&google, "spent");
    let (shown, armed) = armed_card(&core);
    google.draft_reads_fail(true);
    let tried = core.run("gws_mail_send", &send_args(&shown, &armed));
    google.draft_reads_fail(false);
    assert_eq!(tried["isError"], true, "{tried}");
    assert_eq!(google.sent(), []);
    let again = core.run_for_model("gws_mail_draft_save", &rewrite(&shown));
    assert_ne!(again["isError"], true, "the AI has it back: {again}");
}

/// The panel's press knows only the draft until its own save comes back.
#[test]
fn a_newer_press_stops_the_older_one_before_its_own_save() {
    let google = FakeGoogle::start(Seed::demo(2));
    let core = signed_in(&google, "newest");
    let (shown, older) = armed_card(&core);
    data(core.run("gws_mail_send_arm", &json!({"draft": shown["draft"]})));
    let refused = core.run("gws_mail_send", &send_args(&shown, &older));
    assert_eq!(
        refused["structuredContent"]["refused"], "not_allowed",
        "{refused}"
    );
    assert_eq!(google.sent(), []);
}

#[test]
fn a_newer_press_on_a_draft_replaces_the_one_before() {
    let google = FakeGoogle::start(Seed::demo(2));
    let core = signed_in(&google, "newer");
    let (shown, first) = armed_card(&core);
    let (_, second) = rearmed(&core, &shown);
    let refused = core.run("gws_mail_send", &send_args(&shown, &first));
    assert_eq!(
        refused["structuredContent"]["refused"], "not_allowed",
        "{refused}"
    );
    assert_eq!(google.sent(), []);
    data(core.run("gws_mail_send", &send_args(&shown, &second)));
    assert_eq!(google.sent().len(), 1);
}

/// The worst case: send has looked at the draft and is on its way to `drafts/send`.
#[test]
fn a_model_save_in_the_gap_before_the_send_does_not_change_what_goes() {
    let google = FakeGoogle::start(Seed::demo(2));
    let core = signed_in(&google, "gap");
    let (shown, armed) = armed_card(&core);
    google.hold("POST", "/drafts/send");
    std::thread::scope(|threads| {
        let sending = threads.spawn(|| core.run("gws_mail_send", &send_args(&shown, &armed)));
        google.wait_for_held();
        let refused = core.run_for_model("gws_mail_draft_save", &rewrite(&shown));
        assert_eq!(refused["isError"], true, "{refused}");
        google.release();
        data(sending.join().unwrap());
    });
    let sent = google.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].to, "ana@example.com", "what the person saw");
    assert_eq!(sent[0].body, "Noon?\nThe usual place.");
}

/// The other way round: the model's PUT is out when the send starts.
#[test]
fn a_send_does_not_start_under_a_model_save_on_its_way() {
    let google = FakeGoogle::start(Seed::demo(2));
    let core = signed_in(&google, "inflight");
    let shown = data(core.run_for_model("gws_mail_draft_save", &lunch()));
    let put = format!("/drafts/{}", shown["draft"].as_str().unwrap());
    google.hold("PUT", &put);
    std::thread::scope(|threads| {
        let saving = threads.spawn(|| core.run_for_model("gws_mail_draft_save", &rewrite(&shown)));
        google.wait_for_held();
        let press = data(core.run(
            "gws_mail_send_arm",
            &json!({"draft": shown["draft"], "revision": shown["revision"]}),
        ));
        let refused = core.run("gws_mail_send", &send_args(&shown, &press["armed"]));
        assert_eq!(refused["structuredContent"]["refused"], "busy", "{refused}");
        google.release();
        data(saving.join().unwrap());
    });
    assert_eq!(google.sent(), []);
}

/// A lost answer's second try, with the first still at Google.
#[test]
fn a_second_try_behind_a_send_still_going_does_not_say_it_stayed() {
    let google = FakeGoogle::start(Seed::demo(2));
    let core = signed_in(&google, "second");
    let (shown, armed) = armed_card(&core);
    google.hold("POST", "/drafts/send");
    std::thread::scope(|threads| {
        let first = threads.spawn(|| core.run("gws_mail_send", &send_args(&shown, &armed)));
        google.wait_for_held();
        let second = core.run("gws_mail_send", &send_args(&shown, &armed));
        assert_eq!(second["structuredContent"]["refused"], "busy", "{second}");
        google.release();
        data(first.join().unwrap());
    });
    assert_eq!(google.sent().len(), 1);
}

/// A person's save never overtakes a send: it waits, and finds the mail gone.
#[test]
fn a_person_save_behind_a_send_waits_and_writes_nothing() {
    let google = FakeGoogle::start(Seed::demo(2));
    let core = signed_in(&google, "behind");
    let (shown, armed) = armed_card(&core);
    let mut edit = lunch();
    edit["draft"] = shown["draft"].clone();
    edit["message_id"] = shown["message_id"].clone();
    edit["to"] = json!(["ben@example.com"]);
    google.hold("POST", "/drafts/send");
    std::thread::scope(|threads| {
        let sending = threads.spawn(|| core.run("gws_mail_send", &send_args(&shown, &armed)));
        google.wait_for_held();
        let saving = threads.spawn(|| core.run("gws_mail_draft_save", &edit));
        // a save that didn't wait lands here. The fix can't fail on this wait: it
        // only gives the wrong order the time to show
        std::thread::sleep(std::time::Duration::from_millis(300));
        google.release();
        data(sending.join().unwrap());
        let answer = saving.join().unwrap();
        assert_eq!(
            answer["structuredContent"]["refused"], "already_sent",
            "{answer}"
        );
    });
    let sent = google.sent();
    assert_eq!((sent.len(), sent[0].to.as_str()), (1, "ana@example.com"));
}
