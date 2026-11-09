//! Recent mail kept ahead of time, against the stub Google: what comes without
//! an open, what doesn't, where a round stops, that a person's open never waits
//! for one, and that what's kept shows with no connection.

use std::sync::Arc;
use std::time::{Duration, Instant};

use gws::Core;
use gws::apis::Apis;
use gws_fake_google::{FAKE_EMAIL, FakeGoogle, Mail, Seed};
use serde_json::{Value, json};

const DAY: i64 = 86_400_000;
const NOW: i64 = 1_800_000_000_000;
const DOCX: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
const PNG: &str = "image/png";

fn mail(n: u64, days_ago: i64) -> Mail {
    let id = format!("1a{n:014x}");
    Mail {
        id: id.clone(),
        thread: id,
        labels: vec!["INBOX".to_owned()],
        from: "Ana Reyes <ana@example.com>".to_owned(),
        to: FAKE_EMAIL.to_owned(),
        subject: format!("Mail {n}"),
        at: NOW - days_ago * DAY,
        text: format!("Fake mail number {n}."),
        html: None,
        attachments: Vec::new(),
        message_id: String::new(),
        inline: Vec::new(),
    }
}

fn id(n: u64) -> String {
    format!("1a{n:014x}")
}

fn sign_in(expires_at: u64) -> Value {
    json!({"account": "google", "label": "default", "provider": "google", "subject": FAKE_EMAIL, "scopes": ["https://www.googleapis.com/auth/gmail.modify"], "access_token": "fake-access-1", "token_type": "Bearer", "expires_at": expires_at})
}

/// A signed-in plugin at `base`, on the store in the folder `name` names.
fn plugin(base: &str, name: &str, fresh: bool) -> Arc<Core> {
    let dir = std::env::temp_dir().join(format!("gws-ahead-{name}-{}", std::process::id()));
    if fresh {
        let _ = std::fs::remove_dir_all(&dir);
    }
    let core = Core::new(&Apis::at(base), Ok(gws_store::Paths::in_dir(dir)));
    core.unlock("seed");
    let set = core.account("buwiz.account.set", &sign_in(4_000_000_000));
    assert_ne!(set["isError"], true, "{set}");
    core.clock.set(NOW);
    Arc::new(core)
}

fn synced(google: &FakeGoogle, name: &str) -> Arc<Core> {
    let core = plugin(&google.base(), name, true);
    gws::mail::sync(&core).unwrap();
    core
}

fn seeded(mails: Vec<Mail>) -> FakeGoogle {
    FakeGoogle::start(Seed {
        mails,
        ..Seed::default()
    })
}

fn has_view(core: &Core, thread: &str) -> bool {
    let held = core.store().unwrap();
    held.as_ref()
        .unwrap()
        .view(FAKE_EMAIL, thread, 0)
        .unwrap()
        .is_some()
}

fn has_file(core: &Core, message: &str, part: &str) -> bool {
    let held = core.store().unwrap();
    held.as_ref()
        .unwrap()
        .has_attachment(FAKE_EMAIL, message, part)
        .unwrap()
}

/// Whole-thread reads Google has had.
fn thread_reads(google: &FakeGoogle) -> Vec<String> {
    google
        .log()
        .into_iter()
        .filter(|s| s.target.contains("/threads/") && s.target.contains("format=full"))
        .map(|s| s.target)
        .collect()
}

fn attachment_reads(google: &FakeGoogle) -> usize {
    google
        .log()
        .iter()
        .filter(|s| s.target.contains("/attachments/"))
        .count()
}

/// What the last sync said, once one has run.
fn synced_at(core: &Core) -> Option<String> {
    let held = core.store().unwrap();
    let status = held
        .as_ref()
        .unwrap()
        .state(FAKE_EMAIL, "mail_status")
        .unwrap();
    status.filter(|s| s == "ok")
}

/// The Core's clock, `ms` on: past a failed round's wait.
fn later(core: &Core, ms: i64) {
    core.clock.set(core.clock.now_ms() + ms);
}

fn data(answer: Value) -> Value {
    assert_ne!(answer["isError"], true, "{answer}");
    answer["structuredContent"].clone()
}

#[test]
fn recent_threads_arrive_without_an_open_and_old_ones_dont() {
    let google = seeded(vec![mail(1, 1), mail(2, 29), mail(3, 31), mail(4, 90)]);
    let core = synced(&google, "recent");
    let worker = gws::mail::start(core.clone());
    // a wake the person didn't cause, as a model's write makes: a sync, no round
    core.sync.wake();
    let by = Instant::now() + Duration::from_secs(20);
    while synced_at(&core).is_none() && Instant::now() < by {
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_secs(2));
    assert!(thread_reads(&google).is_empty(), "no round for that wake");
    // the person's own: a round
    core.sync.wake_by_person();
    let by = Instant::now() + Duration::from_secs(20);
    while !(has_view(&core, &id(1)) && has_view(&core, &id(2))) && Instant::now() < by {
        std::thread::sleep(Duration::from_millis(20));
    }
    core.sync.stop();
    worker.join().unwrap();
    assert!(has_view(&core, &id(1)) && has_view(&core, &id(2)));
    assert!(!has_view(&core, &id(3)) && !has_view(&core, &id(4)));
    let reads = thread_reads(&google);
    let before = reads.len();
    assert!(
        reads
            .iter()
            .all(|t| !t.contains(&id(3)) && !t.contains(&id(4)))
    );

    let view = data(core.run("gws_mail_thread", &json!({"thread": id(1)})));
    assert!(
        view["page"]
            .as_str()
            .unwrap()
            .contains("Fake mail number 1.")
    );
    assert_eq!(
        thread_reads(&google).len(),
        before,
        "the open asked Google for nothing"
    );
}

#[test]
fn a_round_takes_the_newest_twenty_and_the_next_takes_the_rest() {
    let google = seeded((1..=25).map(|n| mail(n, n as i64 % 20)).collect());
    let core = synced(&google, "twenty");
    gws::prefetch::run(&core).unwrap();
    assert_eq!(thread_reads(&google).len(), 20);
    // the five oldest, 15 to 19 days back, wait
    assert!((15..=19).all(|n| !has_view(&core, &id(n))));
    assert_eq!((1..=25).filter(|n| has_view(&core, &id(*n))).count(), 20);
    gws::prefetch::run(&core).unwrap();
    assert!((1..=25).all(|n| has_view(&core, &id(n))));
    assert_eq!(thread_reads(&google).len(), 25, "none read twice");
}

#[test]
fn a_round_stops_at_a_busy_answer_and_the_next_goes_on() {
    let google = seeded((1..=5).map(|n| mail(n, n as i64)).collect());
    let core = synced(&google, "busy");
    google.thread_reads_fail_after(2, 429);
    assert!(gws::prefetch::run(&core).is_err());
    assert_eq!(thread_reads(&google).len(), 3, "nothing after the busy one");
    assert!(has_view(&core, &id(1)) && has_view(&core, &id(2)));
    assert!(!has_view(&core, &id(3)));

    google.thread_reads_work();
    // straight after a busy answer, a round asks Google nothing
    gws::prefetch::run(&core).unwrap();
    assert_eq!(thread_reads(&google).len(), 3, "it waits");
    later(&core, gws::prefetch::BACKOFF_MIN_MS);
    gws::prefetch::run(&core).unwrap();
    assert!((1..=5).all(|n| has_view(&core, &id(n))));
    assert_eq!(thread_reads(&google).len(), 6, "only the three left");
}

#[test]
fn a_round_stops_at_a_failure_too() {
    let google = seeded((1..=3).map(|n| mail(n, n as i64)).collect());
    let core = synced(&google, "fails");
    google.thread_reads_fail_after(0, 500);
    assert!(gws::prefetch::run(&core).is_err());
    assert_eq!(thread_reads(&google).len(), 1);
    google.thread_reads_work();
    later(&core, gws::prefetch::BACKOFF_MIN_MS);
    gws::prefetch::run(&core).unwrap();
    assert!((1..=3).all(|n| has_view(&core, &id(n))));
}

#[test]
fn an_open_during_a_round_doesnt_wait_for_it() {
    let google = seeded(vec![mail(1, 1), mail(2, 40)]);
    let core = synced(&google, "open");
    google.hold("GET", &format!("/threads/{}", id(1)));
    std::thread::scope(|s| {
        let round = s.spawn(|| gws::prefetch::run(&core));
        google.wait_for_held();
        // the round is stuck on Google: the person's open goes straight past it
        let open = s.spawn(|| core.run("gws_mail_thread", &json!({"thread": id(2)})));
        let by = Instant::now() + Duration::from_secs(10);
        while !open.is_finished() && Instant::now() < by {
            std::thread::sleep(Duration::from_millis(10));
        }
        let waited = !open.is_finished();
        let still_held = !round.is_finished();
        // let go before judging, so a failure ends rather than hangs
        google.release();
        assert!(!waited, "the open waited for the round");
        assert!(still_held, "the round was still waiting on Google");
        let view = data(open.join().unwrap());
        assert!(
            view["page"]
                .as_str()
                .unwrap()
                .contains("Fake mail number 2.")
        );
        round.join().unwrap().unwrap();
    });
    assert!(has_view(&core, &id(1)));
}

#[test]
fn attachments_under_5_mb_that_open_are_kept_and_the_rest_arent() {
    let mut with_files = mail(1, 2);
    with_files.html = Some("<p>See <img src=\"cid:logo\"></p>".to_owned());
    with_files.attachments = vec![
        (
            "receipt.docx".to_owned(),
            DOCX.to_owned(),
            b"PK fake".to_vec(),
        ),
        (
            "scan.docx".to_owned(),
            DOCX.to_owned(),
            vec![0; 5 * 1024 * 1024],
        ),
        (
            "run.sh".to_owned(),
            "application/x-sh".to_owned(),
            b"#!/bin/sh".to_vec(),
        ),
        (
            "photo.png".to_owned(),
            PNG.to_owned(),
            b"\x89PNG photo".to_vec(),
        ),
    ];
    with_files.inline = vec![(
        "logo".to_owned(),
        "logo.png".to_owned(),
        PNG.to_owned(),
        b"\x89PNG fake".to_vec(),
    )];
    let mut old = mail(2, 45);
    old.attachments = vec![("old.docx".to_owned(), DOCX.to_owned(), b"PK old".to_vec())];
    let google = seeded(vec![with_files, old]);
    google.attachment_ids_change();
    let core = synced(&google, "files");
    gws::prefetch::run(&core).unwrap();

    let view = data(core.run("gws_mail_thread", &json!({"thread": id(1)})));
    let part_of = |name: &str| {
        view["attachments"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["name"] == name)
            .unwrap_or_else(|| panic!("{view}"))["part"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    assert!(has_file(&core, &id(1), &part_of("receipt.docx")));
    assert!(has_file(&core, &id(1), &part_of("photo.png")), "a picture");
    assert!(
        !has_file(&core, &id(1), &part_of("logo.png")),
        "a picture inside the mail is the page's own"
    );
    assert!(
        !has_file(&core, &id(1), &part_of("run.sh")),
        "what can't open isn't worth the room"
    );
    assert!(
        !has_file(&core, &id(1), &part_of("scan.docx")),
        "5 MB waits for the press"
    );
    assert!(!has_file(&core, &id(2), "1"), "an old mail's waits too");
    assert_eq!(attachment_reads(&google), 2);

    // a second round finds nothing left to fetch
    gws::prefetch::run(&core).unwrap();
    assert_eq!(attachment_reads(&google), 2);
}

#[test]
fn a_kept_attachment_opens_with_no_call_to_google() {
    let mut with_file = mail(1, 2);
    with_file.attachments = vec![(
        "receipt.docx".to_owned(),
        DOCX.to_owned(),
        b"PK fake receipt".to_vec(),
    )];
    let google = seeded(vec![with_file]);
    google.attachment_ids_change();
    let core = synced(&google, "kept");
    gws::prefetch::run(&core).unwrap();
    assert!(has_file(&core, &id(1), "1"));
    let asked = google.log().len();

    let opened = core.run(
        "gws_mail_attachment_open",
        &json!({"message": id(1), "part": "1"}),
    );
    let path = opened["_meta"]["buwiz/document"]["path"]
        .as_str()
        .unwrap_or_else(|| panic!("{opened}"));
    assert_eq!(std::fs::read(path).unwrap(), b"PK fake receipt");
    assert_eq!(opened["_meta"]["buwiz/document"]["mime"], DOCX);
    assert_eq!(google.log().len(), asked, "Google was asked nothing");
}

#[test]
fn a_kept_attachment_is_still_checked_as_a_fetched_one_is() {
    let mut with_file = mail(1, 2);
    with_file.attachments = vec![(
        "run.sh".to_owned(),
        "application/x-sh".to_owned(),
        b"#!/bin/sh".to_vec(),
    )];
    let google = seeded(vec![with_file]);
    let core = synced(&google, "checked");
    // kept by an older round, or by anything else that put it there
    {
        let held = core.store().unwrap();
        let saved = gws_store::prefetch::Saved {
            thread: id(1),
            at: NOW,
            message: id(1),
            part: "1".to_owned(),
            name: "run.sh".to_owned(),
            mime: "application/x-sh".to_owned(),
            bytes: b"#!/bin/sh".to_vec(),
        };
        held.as_ref()
            .unwrap()
            .put_attachment(FAKE_EMAIL, &saved, 1)
            .unwrap();
    }
    let answer = core.run(
        "gws_mail_attachment_open",
        &json!({"message": id(1), "part": "1"}),
    );
    assert!(answer["_meta"]["buwiz/document"].is_null(), "{answer}");
    assert_eq!(
        answer["structuredContent"]["refused"], "unsupported",
        "{answer}"
    );
}

#[test]
fn signing_out_forgets_the_kept_attachments() {
    let mut with_file = mail(1, 2);
    with_file.attachments = vec![("receipt.docx".to_owned(), DOCX.to_owned(), b"PK".to_vec())];
    let google = seeded(vec![with_file]);
    let core = synced(&google, "signout");
    gws::prefetch::run(&core).unwrap();
    assert!(has_file(&core, &id(1), "1"));
    core.account(
        "buwiz.account.clear",
        &json!({"account": "google", "label": "default"}),
    );
    assert!(!has_file(&core, &id(1), "1"));
    assert!(!has_view(&core, &id(1)));
}

/// The plugin starts again with no connection: the Bot pushes the token it
/// has, which has run out, and can't refresh it.
#[test]
fn offline_a_reopened_mail_shows_the_saved_list_and_threads() {
    let google = seeded(vec![mail(1, 1), mail(2, 3)]);
    let core = synced(&google, "offline");
    gws::prefetch::run(&core).unwrap();
    drop(core);

    for (name, expires_at) in [("lapsed", 1000), ("live", 4_000_000_000)] {
        // nothing listens on port 9
        let core = plugin("http://127.0.0.1:9", "offline", false);
        core.account("buwiz.account.set", &sign_in(expires_at));
        let list = data(core.run("gws_mail_list", &json!({"label": "INBOX"})));
        let ids: Vec<&str> = list["threads"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, [id(1), id(2)], "{name}: {list}");
        // the live token's sync finds Google gone; the lapsed one can't try
        gws::mail::sync(&core).unwrap_err();
        let changes = data(core.run("gws_mail_changes", &json!({"since": -1})));
        assert_eq!(changes["status"], "offline", "{name}");
        let view = data(core.run("gws_mail_thread", &json!({"thread": id(2)})));
        assert!(
            view["page"]
                .as_str()
                .unwrap()
                .contains("Fake mail number 2."),
            "{name}"
        );
        let labels = data(core.run("gws_mail_labels", &json!({})));
        assert!(!labels["labels"].as_array().unwrap().is_empty(), "{name}");
        if name == "lapsed" {
            assert_eq!(list["saved"], true);
            // what isn't kept says Google can't be reached, not to sign in
            let unknown = core.run("gws_mail_thread", &json!({"thread": "1a99"}));
            assert!(
                unknown.to_string().contains("Can't reach Google"),
                "{unknown}"
            );
        }
    }
}

/// Two sign-ins, both lapsed, and the Bot made the second active: only its
/// saved mail shows, never the first's.
#[test]
fn offline_only_the_active_accounts_saved_mail_shows() {
    use gws_store::mail::{Label, Thread};
    let dir = std::env::temp_dir().join(format!("gws-ahead-two-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let core = Core::new(
        &Apis::at("http://127.0.0.1:9"),
        Ok(gws_store::Paths::in_dir(dir)),
    );
    core.unlock("seed");
    let list = json!([
        {"account": "google", "label": "default", "active": false},
        {"account": "google", "label": "work", "active": true},
    ]);
    for (label, who) in [
        ("default", "first@example.com"),
        ("work", "second@example.com"),
    ] {
        let mut push = sign_in(1000);
        push["label"] = json!(label);
        push["subject"] = json!(who);
        push["accounts"] = list.clone();
        let set = core.account("buwiz.account.set", &push);
        assert_ne!(set["isError"], true, "{set}");
    }
    {
        let held = core.store().unwrap();
        let store = held.as_ref().unwrap();
        for (who, thread, words) in [
            ("first@example.com", "1f00", "First's mail"),
            ("second@example.com", "2f00", "Second's mail"),
        ] {
            let row = Thread {
                id: thread.into(),
                subject: words.into(),
                at: NOW,
                messages: 1,
                labels: vec!["INBOX".into()],
                ..Thread::default()
            };
            store.put_thread(who, &row).unwrap();
            store
                .put_view(
                    who,
                    thread,
                    &json!({"id": thread, "page": words}).to_string(),
                    1,
                )
                .unwrap();
            let label = Label {
                id: format!("Label_{thread}"),
                name: words.into(),
                kind: "user".into(),
                unread: 0,
                total: 1,
            };
            store.put_labels(who, &[label]).unwrap();
        }
    }
    let list = data(core.run("gws_mail_list", &json!({"label": "INBOX"})));
    let ids: Vec<&str> = list["threads"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["2f00"], "{list}");
    let view = data(core.run("gws_mail_thread", &json!({"thread": "2f00"})));
    assert_eq!(view["page"], "Second's mail");
    let first = core.run("gws_mail_thread", &json!({"thread": "1f00"}));
    assert!(!first.to_string().contains("First's mail"), "{first}");
    let labels = data(core.run("gws_mail_labels", &json!({})));
    assert_eq!(labels["labels"][0]["name"], "Second's mail", "{labels}");
    assert_eq!(labels["labels"].as_array().unwrap().len(), 1);
    // a screen's write for the active account isn't taken for a stale one
    let marked = core.run(
        "gws_mail_mark",
        &json!({"threads": ["2f00"], "read": true, "as_account": "second@example.com"}),
    );
    assert_ne!(
        marked["structuredContent"]["refused"], "account_changed",
        "{marked}"
    );
    assert!(!marked.to_string().contains("You switched"), "{marked}");

    // the Bot says no sign-in is active: nobody's mail, and no guess
    let mut none = sign_in(1000);
    none["label"] = json!("work");
    none["subject"] = json!("second@example.com");
    none["accounts"] = json!([
        {"account": "google", "label": "default", "active": false},
        {"account": "google", "label": "work", "active": false},
    ]);
    core.account("buwiz.account.set", &none);
    let list = core.run("gws_mail_list", &json!({"label": "INBOX"}));
    assert_eq!(list["isError"], true, "{list}");
    assert!(!list.to_string().contains("1f00") && !list.to_string().contains("2f00"));
}

fn with_file(n: u64, days_ago: i64, name: &str, bytes: Vec<u8>) -> Mail {
    let mut m = mail(n, days_ago);
    m.attachments = vec![(name.to_owned(), DOCX.to_owned(), bytes)];
    m
}

fn fetched(core: &Core, thread: &str) -> bool {
    let held = core.store().unwrap();
    let left = held
        .as_ref()
        .unwrap()
        .views_unfetched_since(FAKE_EMAIL, 0, 100)
        .unwrap();
    !left.iter().any(|(t, _, _)| t == thread)
}

#[test]
fn offline_a_kept_attachment_still_opens() {
    let google = seeded(vec![with_file(1, 2, "receipt.docx", b"PK kept".to_vec())]);
    let core = synced(&google, "offline-file");
    gws::prefetch::run(&core).unwrap();
    assert!(has_file(&core, &id(1), "1"));
    drop(core);
    let core = plugin("http://127.0.0.1:9", "offline-file", false);
    core.account("buwiz.account.set", &sign_in(1000));
    let opened = core.run(
        "gws_mail_attachment_open",
        &json!({"message": id(1), "part": "1"}),
    );
    let path = opened["_meta"]["buwiz/document"]["path"]
        .as_str()
        .unwrap_or_else(|| panic!("{opened}"));
    assert_eq!(std::fs::read(path).unwrap(), b"PK kept");
}

#[test]
fn a_round_downloads_at_most_its_share_and_the_next_goes_on() {
    let four = 4 * 1024 * 1024;
    let mut big = mail(1, 1);
    big.attachments = (0..14)
        .map(|n| {
            (
                format!("part{n}.docx"),
                DOCX.to_owned(),
                vec![n as u8; four],
            )
        })
        .collect();
    let google = seeded(vec![big]);
    let core = synced(&google, "share");
    gws::prefetch::run(&core).unwrap();
    // 4 MiB comes as 5.3 of base64: nine make 48 MiB, and a tenth would pass
    // the round's 50
    assert_eq!(attachment_reads(&google), 9);
    assert!(!fetched(&core, &id(1)));
    gws::prefetch::run(&core).unwrap();
    assert_eq!(attachment_reads(&google), 14);
    assert!(fetched(&core, &id(1)));
}

#[test]
fn with_the_budget_full_of_newer_mail_nothing_older_is_downloaded() {
    let google = seeded(vec![with_file(1, 3, "old.docx", b"PK old".to_vec())]);
    let core = synced(&google, "room");
    {
        let held = core.store().unwrap();
        held.as_ref()
            .unwrap()
            .conn()
            .execute_batch(&format!(
                "INSERT INTO attachment_bytes(account, message, part, thread, at, name, mime, size, used, bytes) VALUES('{FAKE_EMAIL}', 'newer', '1', 'newer', {NOW}, 'n.docx', 'x', {}, 1, x'00')",
                gws_store::prefetch::ATTACHMENTS_MAX
            ))
            .unwrap();
    }
    gws::prefetch::run(&core).unwrap();
    assert!(has_view(&core, &id(1)));
    assert_eq!(attachment_reads(&google), 0, "no download to throw away");
    assert!(has_file(&core, "newer", "1"), "the newer mail's stays");
}

#[test]
fn a_sign_out_during_a_round_ends_it_before_the_next_call() {
    let google = seeded(vec![with_file(1, 2, "receipt.docx", b"PK".to_vec())]);
    let core = synced(&google, "gone-mid");
    // the view is kept by an open, so the round reads only for the attachment
    data(core.run("gws_mail_thread", &json!({"thread": id(1)})));
    google.hold("GET", &format!("/threads/{}", id(1)));
    std::thread::scope(|s| {
        let round = s.spawn(|| gws::prefetch::run(&core));
        google.wait_for_held();
        core.account(
            "buwiz.account.clear",
            &json!({"account": "google", "label": "default"}),
        );
        google.release();
        assert!(round.join().unwrap().is_err());
    });
    assert_eq!(attachment_reads(&google), 0, "no call after the sign-out");
}

#[test]
fn an_attachment_that_didnt_come_is_tried_again() {
    let google = seeded(vec![with_file(1, 2, "receipt.docx", b"PK again".to_vec())]);
    let core = synced(&google, "again");
    google.attachments_gone(true);
    gws::prefetch::run(&core).unwrap();
    assert!(!has_file(&core, &id(1), "1"));
    assert!(!fetched(&core, &id(1)), "not done with it");
    google.attachments_gone(false);
    gws::prefetch::run(&core).unwrap();
    assert!(has_file(&core, &id(1), "1"));
    assert!(fetched(&core, &id(1)));
}

#[test]
fn with_the_views_full_no_thread_is_downloaded() {
    let google = seeded(vec![mail(1, 1), mail(2, 2)]);
    let core = synced(&google, "views-full");
    {
        let held = core.store().unwrap();
        held.as_ref()
            .unwrap()
            .conn()
            .execute_batch(&format!(
                "INSERT INTO bodies(account, thread, view, size, used) VALUES('{FAKE_EMAIL}', '{}', '{{}}', {}, 1)",
                id(2),
                gws_store::mail::BODIES_MAX
            ))
            .unwrap();
    }
    gws::prefetch::run(&core).unwrap();
    assert!(
        thread_reads(&google).is_empty(),
        "{:?}",
        thread_reads(&google)
    );
}

#[test]
fn a_thread_google_no_longer_has_is_read_once() {
    let google = seeded(vec![mail(1, 1)]);
    let core = synced(&google, "vanished");
    {
        let held = core.store().unwrap();
        let row = gws_store::mail::Thread {
            id: "1a99".into(),
            at: NOW,
            messages: 1,
            labels: vec!["INBOX".into()],
            ..Default::default()
        };
        held.as_ref().unwrap().put_thread(FAKE_EMAIL, &row).unwrap();
    }
    gws::prefetch::run(&core).unwrap();
    gws::prefetch::run(&core).unwrap();
    let gone = thread_reads(&google)
        .iter()
        .filter(|t| t.contains("/threads/1a99"))
        .count();
    assert_eq!(gone, 1);
    assert!(has_view(&core, &id(1)));
}

/// Below the Core's own turning of a sign-in card into the same words: the
/// tools themselves say Google can't be reached.
#[test]
fn lapsed_with_nothing_kept_the_tools_say_google_cant_be_reached() {
    let google = seeded(vec![with_file(1, 2, "receipt.docx", b"PK".to_vec())]);
    let core = synced(&google, "unreached");
    core.account("buwiz.account.set", &sign_in(1000));
    let thread = gws::mail::run(&core, "gws_mail_thread", &json!({"thread": id(1)})).unwrap();
    assert!(
        thread.to_string().contains("Can't reach Google"),
        "{thread}"
    );
    assert_ne!(thread["structuredContent"]["code"], "account_required");
    let file = gws::files::run(
        &core,
        "gws_mail_attachment_open",
        &json!({"message": id(1), "part": "1"}),
    )
    .unwrap();
    assert!(file.to_string().contains("Can't reach Google"), "{file}");
}

#[test]
fn a_part_google_named_no_id_for_is_tried_again() {
    let google = seeded(vec![with_file(1, 2, "receipt.docx", b"PK no id".to_vec())]);
    let core = synced(&google, "no-id");
    google.attachment_ids_missing(true);
    gws::prefetch::run(&core).unwrap();
    assert!(!has_file(&core, &id(1), "1"));
    assert!(!fetched(&core, &id(1)), "not done with it");
    google.attachment_ids_missing(false);
    gws::prefetch::run(&core).unwrap();
    assert!(has_file(&core, &id(1), "1"));
}

#[test]
fn a_round_counts_what_came_not_what_was_kept() {
    let three = "x".repeat(3 * 1024 * 1024);
    let google = seeded(
        (1..=20)
            .map(|n| {
                let mut m = mail(n, n as i64);
                m.text = three.clone();
                m
            })
            .collect(),
    );
    let core = synced(&google, "came");
    gws::prefetch::run(&core).unwrap();
    // each read is 4 MB of base64, though the view kept of it is a line:
    // twelve make 48 MiB, and the thirteenth passes the round's 50
    assert_eq!(thread_reads(&google).len(), 13);
}

#[test]
fn a_wake_during_a_round_brings_one_more_after_it() {
    let google = seeded(vec![mail(1, 2)]);
    let core = synced(&google, "again");
    google.hold("GET", &format!("/threads/{}", id(1)));
    gws::prefetch::kick(&core);
    google.wait_for_held();
    // new mail lands while the round waits on Google, and the person's wake asks again
    google.deliver(mail(2, 1));
    gws::mail::sync(&core).unwrap();
    gws::prefetch::kick(&core);
    google.release();
    let by = Instant::now() + Duration::from_secs(20);
    while !has_view(&core, &id(2)) && Instant::now() < by {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(has_view(&core, &id(1)) && has_view(&core, &id(2)));
}

#[test]
fn a_sign_out_ends_the_wait_after_a_failure() {
    let google = seeded(vec![mail(1, 2)]);
    let core = synced(&google, "wait-out");
    google.thread_reads_fail_after(0, 500);
    assert!(gws::prefetch::run(&core).is_err());
    google.thread_reads_work();
    core.account(
        "buwiz.account.clear",
        &json!({"account": "google", "label": "default"}),
    );
    assert!(gws::prefetch::run(&core).is_err(), "nobody to round for");
    core.account("buwiz.account.set", &sign_in(4_000_000_000));
    gws::mail::sync(&core).unwrap();
    let before = thread_reads(&google).len();
    gws::prefetch::run(&core).unwrap();
    assert_eq!(thread_reads(&google).len(), before + 1, "no wait left over");
}

#[test]
fn another_account_has_no_wait_from_the_last_ones_failure() {
    let google = seeded(vec![mail(1, 2)]);
    let core = synced(&google, "wait-switch");
    google.thread_reads_fail_after(0, 500);
    assert!(gws::prefetch::run(&core).is_err());
    google.thread_reads_work();
    let mut other = sign_in(4_000_000_000);
    other["subject"] = json!("other@example.com");
    core.account("buwiz.account.set", &other);
    gws::mail::sync(&core).unwrap();
    let before = thread_reads(&google).len();
    gws::prefetch::run(&core).unwrap();
    assert_eq!(thread_reads(&google).len(), before + 1);
}
