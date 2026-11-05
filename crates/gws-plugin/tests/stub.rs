//! The tools against the stub Google: a whole fake account, reached the way
//! the real one is, with a token the plugin was pushed.

use gws::Core;
use gws::apis::Apis;
use gws_fake_google::{FakeGoogle, Seed};
use serde_json::{Value, json};

fn signed_in(google: &FakeGoogle, name: &str) -> Core {
    let dir = std::env::temp_dir().join(format!("gws-stub-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let core = Core::new(&Apis::at(&google.base()), gws_store::Paths::in_dir(dir));
    core.accounts.set(&json!({"account": "google", "label": "default", "provider": "google", "subject": "owner@example.com", "scopes": ["https://www.googleapis.com/auth/gmail.readonly"], "access_token": "fake-access-1", "token_type": "Bearer", "expires_at": 4_000_000_000u64}));
    core
}

#[test]
fn search_and_read_work_against_a_whole_fake_account_and_change_nothing() {
    let google = FakeGoogle::start(Seed::demo(20));
    let core = signed_in(&google, "read");
    let found = core.run("gws_search_mail", &json!({"query": "from:ana", "limit": 5}));
    let items = found["structuredContent"]["items"].as_array().unwrap();
    assert!(!items.is_empty() && items.iter().all(|m| m["from"].as_str().unwrap().contains("ana@example.com")), "{found}");
    let id = items[0]["id"].as_str().unwrap();
    let mail = core.run("gws_read_mail", &json!({"id": id}));
    assert!(mail["structuredContent"]["body"].as_str().unwrap().contains("fake mail number"), "{mail}");
    assert_eq!(google.writes(), [], "reading changes nothing at Google");
}

#[test]
fn status_says_who_is_signed_in_and_never_the_token() {
    let google = FakeGoogle::start(Seed::default());
    let core = signed_in(&google, "status");
    let status: Value = core.run("gws_status", &json!({}));
    assert_eq!(status["structuredContent"]["subject"], "owner@example.com");
    assert!(!status.to_string().contains("fake-access-1"));
    core.accounts.clear(&json!({"account": "google", "label": "default"}));
    assert_eq!(core.run("gws_status", &json!({}))["structuredContent"]["signed_in"], false);
}

#[test]
fn each_command_opens_its_own_screen() {
    let google = FakeGoogle::start(Seed::default());
    let core = signed_in(&google, "resolve");
    let open = |name: &str, value: &str| core.run("gws_resolve", &json!({"kind": "command", "name": name, "value": value}))["structuredContent"]["app"].clone();
    assert_eq!(open("mail", ""), "mail");
    assert_eq!(open("calendar", ""), "calendar");
    assert_eq!(open("drive", ""), "drive");
    assert_eq!(open("gws", "calendar"), "calendar");
    assert_eq!(open("gws", "DRIVE "), "drive");
    assert_eq!(open("gws", ""), "mail");
    assert_eq!(open("gws", "nonsense"), "mail");
}

#[test]
fn uninstall_takes_every_token_and_every_file() {
    let google = FakeGoogle::start(Seed::default());
    let core = signed_in(&google, "uninstall");
    drop(core.store().unwrap());
    assert!(core.paths.db().exists());
    core.uninstall();
    assert!(core.accounts.google().is_none());
    assert!(!core.paths.dir.exists());
}
