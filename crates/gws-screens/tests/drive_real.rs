//! The Drive screen on the real plugin and the stub Google, with no fake in
//! between: each press goes through the tool the screen names and the answer
//! comes back as the plugin words it. `drive_ui.rs` fakes the tools, so this is
//! what says the two sides still agree. It proves them against the stub, which
//! is written from what's known of Google's API, not against Google.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use buwiz_app::{Bridge, BuwizError, Done, Host, Listener};
use gws::Core;
use gws::apis::Apis;
use gws_fake_google::{DRIVE_ROOT, FAKE_EMAIL, FakeFile, FakeGoogle, GOOGLE_DOC, Seed};
use nsh_core::prelude::*;
use nsh_engine::EngineConfig;
use nsh_testing::Headless;
use serde_json::{Map, Value, json};

const WAIT: Duration = Duration::from_secs(5);

/// The Bot, as far as a person's press goes.
struct Real {
    core: Core,
    picks: Mutex<Vec<(String, Vec<u8>)>>,
}

impl Host for Real {
    fn params(&self) -> Map<String, Value> {
        let p = json!({
            "props": {}, "app": {"plugin": "app.buwiz.google-workspace", "app": "drive"},
            "granted": ["buwiz.call", "buwiz.open", "buwiz.pickFile"],
            "viewport": {"width": 1000, "height": 600, "placement": "full"},
        });
        p.as_object().unwrap().clone()
    }

    fn post(&self, _: &str, _: Value) {}

    fn call(&self, method: &str, data: Value, done: Done) {
        if method == "buwiz.pickFile" {
            let picks = self.picks.lock().unwrap();
            if let Some(id) = data["read"].as_str() {
                let bytes = &picks[id.parse::<usize>().unwrap()].1;
                let text = base64::engine::general_purpose::STANDARD.encode(bytes);
                return done(Ok(json!(text)));
            }
            let files: Vec<Value> = picks
                .iter()
                .enumerate()
                .map(|(at, (name, bytes))| {
                    json!({"id": at.to_string(), "name": name, "size": bytes.len(), "type": "text/plain"})
                })
                .collect();
            return done(Ok(json!(files)));
        }
        if method != "buwiz.call" {
            return done(Ok(Value::Null));
        }
        let tool = data["tool"].as_str().unwrap_or_default();
        let answer = self.core.run(tool, &data["args"]);
        if answer["isError"] == true {
            let words = answer["content"][0]["text"].as_str().unwrap_or_default();
            return done(Err(BuwizError::new("tool_error", false, words)));
        }
        done(Ok(answer["structuredContent"].clone()));
    }

    fn listen(&self, _: Listener) {}

    fn fail(&self, message: &str) {
        panic!("the screen failed: {message}");
    }
}

fn drive() -> FakeGoogle {
    let google = FakeGoogle::start(Seed::default());
    google.seed_file(FakeFile::folder("drv-clients", "Clients"));
    google.seed_file(FakeFile::new("drv-a", "Alpha.txt", "text/plain").holding(b"a"));
    google.seed_file(FakeFile::new("drv-b", "Beta.txt", "text/plain").holding(b"b"));
    google
}

fn open(google: &FakeGoogle, name: &str) -> (Headless, Arc<Real>) {
    let dir = std::env::temp_dir().join(format!("gws-real-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let core = Core::new(&Apis::at(&google.base()), Ok(gws_store::Paths::in_dir(dir)));
    core.unlock("seed");
    core.accounts.set(&json!({"account": "google", "label": "default", "provider": "google", "subject": FAKE_EMAIL, "scopes": ["https://www.googleapis.com/auth/drive"], "access_token": "fake-access-1", "token_type": "Bearer", "expires_at": 4_000_000_000u64}));
    let real = Arc::new(Real {
        core,
        picks: Mutex::default(),
    });
    let def = gws_screens::screens()
        .iter()
        .find(|s| s.id == "gws.drive")
        .expect("Drive");
    let root = (def.build)(Bridge::new(real.clone()));
    let mut app = Headless::start(EngineConfig::new(root)).expect("the engine starts");
    app.set_viewport(size(px(1000.0), px(600.0)))
        .expect("the viewport sets");
    app.wait_for_within("text:Beta.txt", WAIT)
        .expect("the list");
    settle(&mut app);
    (app, real)
}

fn settle(app: &mut Headless) {
    app.wait(Duration::from_millis(300)).expect("time passes");
}

fn press(app: &mut Headless, selector: &str) {
    app.press(selector)
        .unwrap_or_else(|e| panic!("{selector}: {e}: {:?}", app.screen().texts()));
    settle(app);
}

fn key(app: &mut Headless, keys: &str) {
    app.press_key(keys).expect("the key presses");
    settle(app);
}

/// Picks row `n`: Clients, Alpha.txt, Beta.txt.
fn pick(app: &mut Headless, n: usize) {
    for _ in 0..=n {
        key(app, "down");
    }
}

#[test]
fn a_rename_and_its_undo_reach_google_and_the_list_shows_each() {
    let google = drive();
    let (mut app, _) = open(&google, "rename");
    pick(&mut app, 1);
    key(&mut app, "f2");
    app.clear("label:New name").unwrap();
    app.type_text("label:New name", "Zed's \"notes\".txt")
        .unwrap();
    app.submit("label:New name").unwrap();
    settle(&mut app);
    assert_eq!(
        google.drive_file("drv-a").unwrap().name,
        "Zed's \"notes\".txt"
    );
    assert!(
        app.screen().shows("Zed's \"notes\".txt"),
        "{:?}",
        app.screen().texts()
    );
    key(&mut app, "cmd+z");
    assert_eq!(google.drive_file("drv-a").unwrap().name, "Alpha.txt");
    assert!(app.screen().shows("Alpha.txt"));
}

#[test]
fn many_rows_go_to_the_trash_and_come_back_and_none_is_deleted() {
    let google = drive();
    let (mut app, _) = open(&google, "trash");
    pick(&mut app, 1);
    key(&mut app, "shift+down");
    press(&mut app, "label:Move to trash");
    for id in ["drv-a", "drv-b"] {
        assert!(google.drive_file(id).expect("still on Drive").trashed);
    }
    assert!(!app.screen().shows("Alpha.txt") && !app.screen().shows("Beta.txt"));
    assert!(app.screen().shows("Moved 2 items to Trash."));
    press(&mut app, "label:Trash");
    assert!(app.screen().shows("Alpha.txt"), "and in the Trash at once");
    press(&mut app, "label:My Drive");
    press(&mut app, "label:Undo that");
    for id in ["drv-a", "drv-b"] {
        assert!(!google.drive_file(id).unwrap().trashed);
    }
    assert!(app.screen().shows("Alpha.txt") && app.screen().shows("Beta.txt"));
    assert!(google.writes().iter().all(|w| w.method == "PATCH"));
}

#[test]
fn a_move_through_the_chooser_reaches_google_and_undo_goes_back() {
    let google = drive();
    let (mut app, _) = open(&google, "move");
    pick(&mut app, 1);
    press(&mut app, "label:Move");
    assert!(
        app.screen().shows("It's in this folder now."),
        "My Drive is where it is: {:?}",
        app.screen().texts()
    );
    assert!(!app.screen().matches("label:Move here").unwrap());
    press(&mut app, "label:Go into this folder >> text:Clients");
    press(&mut app, "label:Move here");
    assert_eq!(google.drive_file("drv-a").unwrap().parents, ["drv-clients"]);
    assert!(!app.screen().shows("Alpha.txt"));
    press(&mut app, "label:Undo that");
    assert_eq!(google.drive_file("drv-a").unwrap().parents, [DRIVE_ROOT]);
    assert!(app.screen().shows("Alpha.txt"));
}

#[test]
fn share_shows_everyone_google_has_and_the_press_reaches_google_as_shown() {
    let google = drive();
    google.edit_file("drv-a", |f| {
        f.people = vec![
            (
                "Ana Reyes".to_owned(),
                "ana@example.com".to_owned(),
                "writer".to_owned(),
            ),
            (
                "Ben Cruz".to_owned(),
                "ben@example.com".to_owned(),
                "reader".to_owned(),
            ),
        ];
        f.link = Some("reader".to_owned());
    });
    google.drive_people_page(2);
    let (mut app, _) = open(&google, "share");
    pick(&mut app, 1);
    press(&mut app, "label:Share");
    app.wait_for_within("text:Who will have access", WAIT)
        .unwrap();
    settle(&mut app);
    for who in [
        "Owner Fake <owner@example.com> (you)",
        "Ana Reyes <ana@example.com>",
        "Ben Cruz <ben@example.com>",
        "Anyone with the link",
    ] {
        assert!(app.screen().shows(who), "{who}: {:?}", app.screen().texts());
    }
    assert_eq!(app.screen().count("label:Someone with access").unwrap(), 4);
    assert_eq!(
        app.screen().count("label:Take this person off").unwrap(),
        2,
        "not the owner, the person or the link"
    );
    app.type_text("label:Add a person's email address", "cara@example.com")
        .unwrap();
    press(&mut app, "label:Can edit");
    press(&mut app, "label:Add this person");
    app.type_text("label:Message to new people", "Please look")
        .unwrap();
    press(&mut app, "label:Take this person off >> nth=1");
    assert!(app.screen().shows("Share with 1 person and remove 1"));
    press(&mut app, "label:Share with these people");
    assert!(
        app.screen().shows("Shared with 1 person and removed 1."),
        "{:?}",
        app.screen().texts()
    );
    let held = google.drive_file("drv-a").unwrap();
    let who: Vec<(&str, &str)> = held
        .people
        .iter()
        .map(|p| (p.1.as_str(), p.2.as_str()))
        .collect();
    assert_eq!(
        who,
        [
            ("ana@example.com", "writer"),
            ("cara@example.com", "writer")
        ]
    );
    let writes = google.drive_writes();
    assert_eq!(writes[0].query["emailMessage"], "Please look");
    assert_eq!(writes[0].query["sendNotificationEmail"], "true");
    assert_eq!(writes[1].method, "DELETE");
}

#[test]
fn someone_new_since_the_list_was_read_is_shown_before_anything_goes() {
    let google = drive();
    let (mut app, _) = open(&google, "stale");
    pick(&mut app, 1);
    press(&mut app, "label:Share");
    app.wait_for_within("text:Who will have access", WAIT)
        .unwrap();
    app.type_text("label:Add a person's email address", "cara@example.com")
        .unwrap();
    press(&mut app, "label:Add this person");
    google.edit_file("drv-a", |f| {
        f.people.push((
            "Eve Stone".to_owned(),
            "eve@example.com".to_owned(),
            "writer".to_owned(),
        ));
    });
    press(&mut app, "label:Share with these people");
    assert_eq!(google.drive_writes(), []);
    assert!(
        app.screen().shows("Eve Stone <eve@example.com>"),
        "{:?}",
        app.screen().texts()
    );
    assert!(app.screen().shows("cara@example.com"));
    press(&mut app, "label:Share with these people");
    assert_eq!(
        google.drive_writes().len(),
        1,
        "with the list read, it goes"
    );
}

#[test]
fn a_new_folder_and_a_picked_file_land_where_the_screen_is() {
    let google = drive();
    let (mut app, real) = open(&google, "made");
    *real.picks.lock().unwrap() = vec![("Quote.txt".to_owned(), b"fake quote".to_vec())];
    app.press("label:Open this item >> text:Clients").unwrap();
    settle(&mut app);
    press(&mut app, "label:New folder");
    app.type_text("label:Folder name", "Acme").unwrap();
    app.submit("label:Folder name").unwrap();
    settle(&mut app);
    press(&mut app, "label:Upload a file to Google Drive");
    settle(&mut app);
    assert!(app.screen().shows("Acme"), "{:?}", app.screen().texts());
    assert!(app.screen().shows("Quote.txt"));
    let files = google.drive_files();
    let held = |name: &str| files.iter().find(|f| f.name == name).unwrap();
    assert_eq!(held("Acme").parents, ["drv-clients"]);
    assert_eq!(held("Quote.txt").parents, ["drv-clients"]);
    assert_eq!(held("Quote.txt").content, b"fake quote");
    assert_eq!(real.core.uploads.lock().unwrap().held_bytes(), 0);
}

#[test]
fn a_file_the_person_cant_edit_offers_nothing_from_what_google_says() {
    let google = drive();
    google.seed_file(
        FakeFile::new("drv-read", "Brief", GOOGLE_DOC)
            .shared_by("Ana Reyes", "ana@example.com")
            .read_only(),
    );
    let (mut app, _) = open(&google, "read-only");
    press(&mut app, "label:Shared");
    app.wait_for_within("text:Brief", WAIT).unwrap();
    key(&mut app, "down");
    assert!(app.screen().shows("You can't change this here."));
    for press in ["Rename", "Move", "Share", "Move to trash"] {
        assert!(
            !app.screen().matches(&format!("label:{press}")).unwrap(),
            "{press}"
        );
    }
    key(&mut app, "f2");
    assert!(!app.screen().matches("label:New name").unwrap());
    assert_eq!(google.writes(), []);
}

#[test]
fn a_write_google_refuses_leaves_the_screen_and_google_as_they_were() {
    let google = drive();
    let (mut app, _) = open(&google, "refused");
    pick(&mut app, 1);
    google.drive_writes_fail_after(0, 403);
    press(&mut app, "label:Move to trash");
    assert!(
        app.screen()
            .shows("This account can't do that in Google Drive, so nothing changed.")
    );
    assert!(app.screen().shows("Alpha.txt"));
    assert!(!google.drive_file("drv-a").unwrap().trashed);
    assert!(!app.screen().matches("label:Undo that").unwrap());
}
