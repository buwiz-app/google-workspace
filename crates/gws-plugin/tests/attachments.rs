//! A mail's files as the reader lists them, and the pictures their cards show,
//! against the stub Google and its mail as a real inbox has it.

use std::fs;
use std::path::PathBuf;

use gws::Core;
use gws::apis::Apis;
use gws_fake_google::{FAKE_EMAIL, FakeGoogle, Mail, Seed};
use serde_json::{Value, json};

const TOKEN: &str = "fake-access-1";
const INVITE: &str = "000019f000000014";
const PHOTOS: &str = "000019f00000001e";
const NEWS: &str = "000019f000000028";

struct Home {
    core: Core,
    dir: PathBuf,
}

impl Home {
    fn new(base: &str, name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("gws-cards-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let core = Core::new(&Apis::at(base), Ok(gws_store::Paths::in_dir(&dir)));
        core.unlock("seed");
        core.account(
            "buwiz.account.set",
            &json!({"account": "google", "label": "default", "provider": "google", "subject": FAKE_EMAIL, "scopes": ["https://www.googleapis.com/auth/gmail.modify"], "access_token": TOKEN, "token_type": "Bearer", "expires_at": 4_000_000_000u64}),
        );
        Self { core, dir }
    }

    fn files(&self, thread: &str) -> Vec<Value> {
        let answer = self.core.run("gws_mail_thread", &json!({"thread": thread}));
        answer["structuredContent"]["attachments"]
            .as_array()
            .unwrap_or_else(|| panic!("no files: {answer}"))
            .clone()
    }

    fn preview(&self, message: &str, part: &Value) -> Value {
        self.core.run(
            "gws_mail_attachment_preview",
            &json!({"message": message, "part": part}),
        )["structuredContent"]
            .clone()
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn rich() -> FakeGoogle {
    FakeGoogle::start(Seed::default().with_rich_mail("http://127.0.0.1:1"))
}

fn names(files: &[Value]) -> Vec<&str> {
    files.iter().map(|f| f["name"].as_str().unwrap()).collect()
}

fn fetches(google: &FakeGoogle) -> usize {
    google
        .log()
        .iter()
        .filter(|s| s.target.contains("/attachments/"))
        .count()
}

/// One mail of the person's with these files.
fn mail_with(files: Vec<(&str, &str, Vec<u8>)>) -> Mail {
    Mail {
        id: "18f1aa".to_owned(),
        thread: "18f1aa".to_owned(),
        labels: vec!["INBOX".to_owned()],
        from: "Ana Reyes <ana@example.com>".to_owned(),
        to: FAKE_EMAIL.to_owned(),
        subject: "Pictures".to_owned(),
        at: 1_800_000_000_000,
        text: "Here.".to_owned(),
        html: None,
        attachments: files
            .into_iter()
            .map(|(n, m, b)| (n.to_owned(), m.to_owned(), b))
            .collect(),
        message_id: "<a@example.com>".to_owned(),
        inline: Vec::new(),
    }
}

#[test]
fn invite_ics_shows_once_on_the_invite() {
    let google = rich();
    let home = Home::new(&google.base(), "invite");
    let files = home.files(INVITE);
    assert_eq!(names(&files), ["invite.ics"]);
    assert_eq!(
        files[0]["mime"], "application/ics",
        "the file, not the letter's part"
    );
}

#[test]
fn a_rescheduled_invite_of_the_same_size_shows_both_events() {
    let first = b"BEGIN:VCALENDAR\r\nSEQUENCE:0\r\nEND:VCALENDAR\r\n".to_vec();
    let moved = b"BEGIN:VCALENDAR\r\nSEQUENCE:1\r\nEND:VCALENDAR\r\n".to_vec();
    assert_eq!(first.len(), moved.len(), "this is the case being tested");
    let google = FakeGoogle::start(Seed::default());
    google.deliver(mail_with(vec![
        ("invite.ics", "text/calendar", first.clone()),
        ("invite.ics", "application/ics", moved),
    ]));
    let home = Home::new(&google.base(), "moved");
    assert_eq!(names(&home.files("18f1aa")), ["invite.ics", "invite.ics"]);
    // and the same bytes are one
    let google = FakeGoogle::start(Seed::default());
    google.deliver(mail_with(vec![
        ("invite.ics", "text/calendar", first.clone()),
        ("invite.ics", "application/ics", first),
    ]));
    let home = Home::new(&google.base(), "same");
    assert_eq!(names(&home.files("18f1aa")), ["invite.ics"]);
}

#[test]
fn two_files_of_one_name_and_size_are_two_cards() {
    let google = FakeGoogle::start(Seed::default());
    google.deliver(mail_with(vec![
        ("scan.png", "image/png", b"\x89PNG one".to_vec()),
        ("scan.png", "image/png", b"\x89PNG two".to_vec()),
    ]));
    let home = Home::new(&google.base(), "scans");
    assert_eq!(names(&home.files("18f1aa")), ["scan.png", "scan.png"]);
}

#[test]
fn the_newsletters_logo_is_the_letters_and_not_a_file() {
    let google = rich();
    let home = Home::new(&google.base(), "news");
    assert!(home.files(NEWS).is_empty());
}

#[test]
fn every_listed_file_is_one_open_finds() {
    let google = rich();
    let home = Home::new(&google.base(), "open");
    for thread in [INVITE, PHOTOS] {
        for f in home.files(thread) {
            let answer = home.core.run(
                "gws_mail_attachment_open",
                &json!({"message": f["message"], "part": f["part"]}),
            );
            assert_ne!(
                answer["structuredContent"]["refused"], "not_found",
                "{f}: {answer}"
            );
        }
    }
}

#[test]
fn a_small_real_picture_has_a_preview_of_its_own_kind() {
    let google = rich();
    let home = Home::new(&google.base(), "photo");
    let files = home.files(PHOTOS);
    assert_eq!(
        names(&files),
        ["IMG_4250.png", "receipt.pdf", "site-plans.zip"]
    );
    let shown = home.preview(PHOTOS, &files[0]["part"]);
    let url = shown["preview"]
        .as_str()
        .unwrap_or_else(|| panic!("{shown}"));
    assert!(
        url.starts_with("data:image/png;base64,iVBORw0KGgo"),
        "{url:.40}"
    );
    for other in &files[1..] {
        let answer = home.preview(PHOTOS, &other["part"]);
        assert_eq!(answer["refused"], "unsupported", "{answer}");
        assert_eq!(answer.get("preview"), None);
    }
}

#[test]
fn a_file_dressed_as_a_picture_has_no_preview() {
    let google = FakeGoogle::start(Seed::default());
    google.deliver(mail_with(vec![
        ("photo.png", "image/png", b"<svg onload=alert(1)>".to_vec()),
        ("photo.jpg", "image/jpeg", b"%PDF-1.4 not a jpeg".to_vec()),
    ]));
    let home = Home::new(&google.base(), "dressed");
    for part in ["1", "2"] {
        let answer = home.preview("18f1aa", &json!(part));
        assert_eq!(answer["refused"], "unsupported", "{answer}");
        assert_eq!(answer.get("preview"), None);
    }
}

/// A PNG's start: its signature and a header that says how big it is.
fn png_saying(w: u32, h: u32) -> Vec<u8> {
    let mut b = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
    b.extend(w.to_be_bytes());
    b.extend(h.to_be_bytes());
    b.extend([8, 6, 0, 0, 0]);
    b
}

#[test]
fn a_small_file_that_says_it_is_a_huge_picture_has_no_preview() {
    let google = FakeGoogle::start(Seed::default());
    google.deliver(mail_with(vec![
        ("bomb.png", "image/png", png_saying(30000, 30000)),
        ("big.png", "image/png", png_saying(8000, 5000)),
        ("ok.png", "image/png", png_saying(4000, 4000)),
    ]));
    let home = Home::new(&google.base(), "bomb");
    let answer = home.preview("18f1aa", &json!("1"));
    assert_eq!(answer["refused"], "too_big", "{answer}");
    assert_eq!(answer.get("preview"), None);
    let answer = home.preview("18f1aa", &json!("2"));
    assert_eq!(answer["refused"], "too_big", "40 megapixels: {answer}");
    let answer = home.preview("18f1aa", &json!("3"));
    assert!(answer["preview"].is_string(), "{answer}");
}

#[test]
fn a_picture_past_the_limit_is_never_fetched_for_a_preview() {
    let mut big = b"\x89PNG\r\n\x1a\n".to_vec();
    big.resize(gws::attachment_preview::PREVIEW_MAX as usize + 1, 0);
    let mut fits = png_saying(1, 1);
    fits.resize(gws::attachment_preview::PREVIEW_MAX as usize, 0);
    let google = FakeGoogle::start(Seed::default());
    google.deliver(mail_with(vec![
        ("big.png", "image/png", big),
        ("fits.png", "image/png", fits),
    ]));
    let home = Home::new(&google.base(), "big");
    let answer = home.preview("18f1aa", &json!("1"));
    assert_eq!(answer["refused"], "too_big", "{answer}");
    assert_eq!(fetches(&google), 0, "nothing was downloaded to find out");
    let answer = home.preview("18f1aa", &json!("2"));
    assert!(answer["preview"].is_string(), "the limit itself is in");
    assert_eq!(fetches(&google), 1);
}

#[test]
fn a_preview_takes_only_a_part_the_mail_lists() {
    let google = rich();
    let home = Home::new(&google.base(), "unlisted");
    // the newsletter's logo is there, but it's the letter's, not a file
    let answer = home.preview(NEWS, &json!("1"));
    assert_eq!(answer["refused"], "not_found", "{answer}");
    let answer = home.preview(PHOTOS, &json!("../1"));
    assert_eq!(answer["refused"], "invalid_args", "{answer}");
}

#[test]
fn a_view_saved_before_this_listing_is_made_again() {
    let google = rich();
    let home = Home::new(&google.base(), "oldview");
    // opens the store, as any read does
    home.files(NEWS);
    let account = home.core.accounts.session().unwrap().1;
    let old = json!({"page": "<p>old</p>", "links": [], "images": 0, "attachments": [
        {"name": "invite.ics", "mime": "text/calendar", "size": 640, "id": "a", "part": "0.2", "message": INVITE},
        {"name": "invite.ics", "mime": "application/ics", "size": 640, "id": "b", "part": "1", "message": INVITE},
    ]});
    {
        let held = home.core.store().unwrap();
        let store = held.as_ref().unwrap();
        store
            .put_view(&account, INVITE, &old.to_string(), 1_800_000_000_000)
            .unwrap();
    }
    let files = home.files(INVITE);
    assert_eq!(names(&files), ["invite.ics"], "{files:?}");
    // and the one made now is kept as it is
    assert_eq!(home.files(INVITE), files);
}

/// A file small enough for Gmail to send in its part, with no attachment id.
fn small_file_home(name: &str, mime: &str, bytes: Vec<u8>, tag: &str) -> (FakeGoogle, Home) {
    let google = FakeGoogle::start(Seed::default());
    google.deliver(mail_with(vec![(name, mime, bytes)]));
    google.small_files_in_parts(4096);
    let home = Home::new(&google.base(), tag);
    (google, home)
}

#[test]
fn a_small_file_in_its_part_opens_from_the_message() {
    const DOCX: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
    let (google, home) = small_file_home("note.docx", DOCX, b"PK small".to_vec(), "smallopen");
    let files = home.files("18f1aa");
    assert_eq!(files[0]["id"], Value::Null, "this is the case being tested");
    let answer = home.core.run(
        "gws_mail_attachment_open",
        &json!({"message": "18f1aa", "part": files[0]["part"]}),
    );
    let path = answer["_meta"]["buwiz/document"]["path"]
        .as_str()
        .unwrap_or_else(|| panic!("{answer}"));
    assert_eq!(fs::read(path).unwrap(), b"PK small");
    assert_eq!(fetches(&google), 0);
}

#[test]
fn a_small_picture_in_its_part_has_a_preview() {
    let (_google, home) = small_file_home("dot.png", "image/png", png_saying(1, 1), "smallpic");
    let files = home.files("18f1aa");
    assert_eq!(files[0]["id"], Value::Null);
    let answer = home.preview("18f1aa", &files[0]["part"]);
    assert!(answer["preview"].is_string(), "{answer}");
}
