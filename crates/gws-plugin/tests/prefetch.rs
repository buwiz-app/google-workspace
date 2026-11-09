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

fn data(answer: Value) -> Value {
    assert_ne!(answer["isError"], true, "{answer}");
    answer["structuredContent"].clone()
}

#[test]
fn recent_threads_arrive_without_an_open_and_old_ones_dont() {
    let google = seeded(vec![mail(1, 1), mail(2, 29), mail(3, 31), mail(4, 90)]);
    let core = synced(&google, "recent");
    // the background sync does it, as it would with a screen open
    let worker = gws::mail::start(core.clone());
    core.sync.wake();
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
        let view = data(core.run("gws_mail_thread", &json!({"thread": id(2)})));
        assert!(
            view["page"]
                .as_str()
                .unwrap()
                .contains("Fake mail number 2.")
        );
        assert!(!round.is_finished(), "the round was still waiting");
        google.release();
        round.join().unwrap().unwrap();
    });
    assert!(has_view(&core, &id(1)));
}

#[test]
fn attachments_under_5_mb_are_kept_and_larger_ones_arent() {
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
    assert!(
        has_file(&core, &id(1), &part_of("logo.png")),
        "an inline picture too"
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
    gws::prefetch::run(&core).unwrap();
    assert!(has_file(&core, &id(1), "1"));
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
