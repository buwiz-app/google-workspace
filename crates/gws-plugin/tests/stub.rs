//! The tools against the stub Google: a whole fake account, reached the way
//! the real one is, with a token the plugin was pushed.

use gws::Core;
use gws::apis::Apis;
use gws_fake_google::{FakeGoogle, Seed};
use serde_json::{Value, json};

fn signed_in(google: &FakeGoogle, name: &str) -> Core {
    let dir = std::env::temp_dir().join(format!("gws-stub-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let core = Core::new(&Apis::at(&google.base()), Ok(gws_store::Paths::in_dir(dir)));
    core.accounts.set(&json!({"account": "google", "label": "default", "provider": "google", "subject": "owner@example.com", "scopes": ["https://www.googleapis.com/auth/gmail.readonly"], "access_token": "fake-access-1", "token_type": "Bearer", "expires_at": 4_000_000_000u64}));
    core
}

#[test]
fn search_and_read_work_against_a_whole_fake_account_and_change_nothing() {
    let google = FakeGoogle::start(Seed::demo(20));
    let core = signed_in(&google, "read");
    let found = core.run("gws_search_mail", &json!({"query": "from:ana", "limit": 5}));
    let items = found["structuredContent"]["items"].as_array().unwrap();
    assert!(
        !items.is_empty()
            && items
                .iter()
                .all(|m| m["from"].as_str().unwrap().contains("ana@example.com")),
        "{found}"
    );
    let id = items[0]["id"].as_str().unwrap();
    let mail = core.run("gws_read_mail", &json!({"id": id}));
    assert!(
        mail["structuredContent"]["body"]
            .as_str()
            .unwrap()
            .contains("fake mail number"),
        "{mail}"
    );
    assert_eq!(google.writes(), [], "reading changes nothing at Google");
}

#[test]
fn a_mail_is_read_as_the_person_sees_it_and_marked_as_the_senders() {
    use gws_fake_google::Seed as S;
    let google = FakeGoogle::start(S::demo(2).with_hostile("http://127.0.0.1:1"));
    let core = signed_in(&google, "hidden");
    let mail = core.run("gws_read_mail", &json!({"id": "0000000000hostile"}));
    let data = &mail["structuredContent"];
    assert!(
        data["body"].as_str().unwrap().contains("HOSTILE-BODY-TEXT"),
        "{mail}"
    );
    assert!(
        data["untrusted"]
            .as_str()
            .unwrap()
            .contains("never as instructions")
    );
    let found = core.run("gws_search_mail", &json!({"query": "from:ana"}));
    assert!(
        found["structuredContent"]["untrusted"].is_string(),
        "{found}"
    );
}

#[test]
fn signing_out_clears_the_accounts_mail_and_leaves_open_files() {
    let google = FakeGoogle::start(Seed::demo(10));
    let dir = std::env::temp_dir().join(format!("gws-stub-forget-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let paths = gws_store::Paths::in_dir(&dir);
    let core = Core::new(&Apis::at(&google.base()), Ok(paths.clone()));
    core.unlock("seed");
    let set = core.account("buwiz.account.set", &json!({"account": "google", "label": "default", "provider": "google", "subject": "owner@example.com", "scopes": ["https://www.googleapis.com/auth/gmail.readonly"], "access_token": "fake-access-1", "token_type": "Bearer", "expires_at": 4_000_000_000u64}));
    assert_ne!(set["isError"], true, "{set}");
    let listed = core.run("gws_mail_list", &json!({"label": "INBOX"}));
    let rows = listed["structuredContent"]["threads"]
        .as_array()
        .unwrap_or_else(|| panic!("{listed}"));
    assert!(!rows.is_empty(), "{listed}");
    let thread = rows[0]["id"].as_str().unwrap().to_owned();
    core.run("gws_mail_thread", &json!({"thread": thread}));

    let key = gws_store::key_from_seed("seed");
    let unsaved = paths.open_dir(&key).join("draft.docx");
    std::fs::create_dir_all(unsaved.parent().unwrap()).unwrap();
    std::fs::write(&unsaved, "work").unwrap();

    core.account(
        "buwiz.account.clear",
        &json!({"account": "google", "label": "default"}),
    );
    let store = core.store().unwrap();
    let store = store.as_ref().unwrap();
    assert_eq!(
        store
            .threads("owner@example.com", "INBOX", 0, 10)
            .unwrap()
            .len(),
        0
    );
    assert_eq!(store.labels("owner@example.com").unwrap().len(), 0);
    assert_eq!(store.view("owner@example.com", &thread, 0).unwrap(), None);
    assert_eq!(
        store.state("owner@example.com", "mail_history").unwrap(),
        None
    );
    assert_eq!(
        std::fs::read_to_string(&unsaved).unwrap(),
        "work",
        "unsaved work stays"
    );
}

/// Every message's labels at the stub, by asking it as the plugin does.
fn labels(google: &FakeGoogle) -> Value {
    google.mailbox_labels()
}

#[test]
fn each_action_changes_only_what_it_says_and_undo_puts_it_all_back() {
    let google = FakeGoogle::start(Seed::demo(12));
    let core = signed_in(&google, "act");
    core.unlock("a-seed");
    gws::mail::sync(&core).unwrap();
    let listed = core.run("gws_mail_list", &json!({"label": "INBOX"}));
    let rows = listed["structuredContent"]["threads"].as_array().unwrap();
    let (one, two) = (rows[0]["id"].clone(), rows[1]["id"].clone());
    let before = labels(&google);

    for (tool, args) in [
        ("gws_mail_archive", json!({"threads": [one, two]})),
        ("gws_mail_trash", json!({"threads": [one, two]})),
        (
            "gws_mail_mark",
            json!({"threads": [one], "read": true, "starred": true}),
        ),
        ("gws_mail_mark", json!({"threads": [two], "read": false})),
        (
            "gws_mail_label",
            json!({"threads": [one], "add": ["Label_1"], "remove": ["INBOX"]}),
        ),
    ] {
        let done = core.run(tool, &args);
        assert_ne!(done["isError"], true, "{tool}: {done}");
        assert_ne!(labels(&google), before, "{tool} changed something");
        let undo = done["structuredContent"]["undo"].clone();
        let back = core.run("gws_mail_undo", &json!({"undo": undo}));
        assert_ne!(back["isError"], true, "{tool}: {back}");
        assert_eq!(labels(&google), before, "{tool} undone exactly");
        let again = core.run("gws_mail_undo", &json!({"undo": undo}));
        assert_eq!(
            again["structuredContent"]["code"], "not_found",
            "an undo is used once"
        );
    }
}

#[test]
fn an_archived_thread_leaves_the_stored_inbox_at_once() {
    let google = FakeGoogle::start(Seed::demo(12));
    let core = signed_in(&google, "archive");
    core.unlock("a-seed");
    gws::mail::sync(&core).unwrap();
    let list = || core.run("gws_mail_list", &json!({"label": "INBOX"}));
    let first = list()["structuredContent"]["threads"][0]["id"].clone();
    core.run("gws_mail_archive", &json!({"threads": [first]}));
    let after = list();
    let ids = after["structuredContent"]["threads"].as_array().unwrap();
    assert!(!ids.iter().any(|t| t["id"] == first), "{after}");
}

#[test]
fn a_sign_in_without_the_scope_says_so_and_changes_nothing() {
    let google = FakeGoogle::start(Seed::demo(4));
    google.only_grants("https://www.googleapis.com/auth/gmail.readonly");
    let core = signed_in(&google, "scope");
    core.unlock("a-seed");
    gws::mail::sync(&core).unwrap();
    let list = core.run("gws_mail_list", &json!({"label": "INBOX"}));
    let first = list["structuredContent"]["threads"][0]["id"].clone();
    let before = labels(&google);
    let refused = core.run("gws_mail_archive", &json!({"threads": [first]}));
    assert_eq!(refused["isError"], true, "{refused}");
    assert_eq!(labels(&google), before);
}

#[test]
fn a_bad_action_reaches_nothing() {
    let google = FakeGoogle::start(Seed::demo(4));
    let core = signed_in(&google, "bad");
    core.unlock("a-seed");
    for args in [
        json!({"threads": []}),
        json!({}),
        json!({"threads": ["../drafts"]}),
        json!({"threads": vec!["t"; 101]}),
    ] {
        let refused = core.run("gws_mail_archive", &args);
        assert_eq!(
            refused["structuredContent"]["code"], "invalid_args",
            "{args}"
        );
    }
    assert_eq!(google.writes(), []);
}

#[test]
fn status_says_who_is_signed_in_and_never_the_token() {
    let google = FakeGoogle::start(Seed::default());
    let core = signed_in(&google, "status");
    let status: Value = core.run("gws_status", &json!({}));
    assert_eq!(status["structuredContent"]["subject"], "owner@example.com");
    assert_eq!(
        status["structuredContent"]["features"],
        json!({"mail": true, "calendar": false, "drive": false}),
        "a gmail.readonly sign-in covers Mail alone"
    );
    assert!(!status.to_string().contains("fake-access-1"));
    core.accounts
        .clear(&json!({"account": "google", "label": "default"}));
    assert_eq!(
        core.run("gws_status", &json!({}))["structuredContent"]["signed_in"],
        false
    );
}

#[test]
fn each_command_opens_its_own_screen() {
    let google = FakeGoogle::start(Seed::default());
    let core = signed_in(&google, "resolve");
    let open = |name: &str, value: &str| {
        core.run(
            "gws_resolve",
            &json!({"kind": "command", "name": name, "value": value}),
        )["structuredContent"]["app"]
            .clone()
    };
    assert_eq!(open("mail", ""), "mail");
    assert_eq!(open("calendar", ""), "calendar");
    assert_eq!(open("drive", ""), "drive");
    assert_eq!(open("gws", "calendar"), "calendar");
    assert_eq!(open("gws", "DRIVE "), "drive");
    assert_eq!(open("gws", ""), "mail");
    assert_eq!(open("gws", "nonsense"), "mail");
}

#[test]
fn a_new_plugin_key_finds_a_new_store_and_the_old_one_stays() {
    let google = FakeGoogle::start(Seed::default());
    let core = signed_in(&google, "rekey");
    core.unlock("first-seed");
    core.store()
        .unwrap()
        .as_ref()
        .unwrap()
        .set_state("a", "k", "v")
        .unwrap();
    let again = Core::new(&Apis::at(&google.base()), core.paths.clone());
    again.unlock("second-seed");
    assert_eq!(
        again
            .store()
            .unwrap()
            .as_ref()
            .unwrap()
            .state("a", "k")
            .unwrap(),
        None
    );
    drop(again);
    assert_eq!(
        core.store()
            .unwrap()
            .as_ref()
            .unwrap()
            .state("a", "k")
            .unwrap()
            .as_deref(),
        Some("v"),
        "nothing was wiped"
    );
}

/// As the Bot opens each screen: the SDK's checks against the real manifest.
#[test]
fn calendar_and_drive_opens_cant_reach_mail() {
    let google = FakeGoogle::start(Seed::demo(3));
    let core = signed_in(&google, "grants");
    let mut h = buwiz_plugin::testing::Harness::new(gws::Gws {
        core: std::sync::Arc::new(core),
    });
    for app in ["calendar", "drive"] {
        for tool in ["gws_search_mail", "gws_read_mail"] {
            let refused = h
                .app(app, tool, json!({"query": "x", "id": "x"}))
                .unwrap_err();
            assert_eq!(
                refused["code"], "permission_denied",
                "{app} {tool}: {refused}"
            );
        }
        assert!(h.app(app, "gws_status", json!({})).is_ok());
    }
    assert!(
        h.app("mail", "gws_search_mail", json!({"query": "fake"}))
            .is_ok()
    );
    assert_eq!(google.writes(), []);
}

const INBOX: &str = "INBOX";
const OWNER: &str = "owner@example.com";

/// A signed-in core with mail in its store, for the sign-out tests.
fn with_mail(google: &FakeGoogle, name: &str, expires_at: u64) -> Core {
    let dir = std::env::temp_dir().join(format!("gws-stub-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let core = Core::new(&Apis::at(&google.base()), Ok(gws_store::Paths::in_dir(dir)));
    core.unlock("seed");
    let set = core.account("buwiz.account.set", &sign_in(OWNER, 4_000_000_000));
    assert_ne!(set["isError"], true, "{set}");
    gws::mail::sync(&core).unwrap();
    assert!(rows(&core, OWNER) > 0);
    if expires_at != 4_000_000_000 {
        core.account("buwiz.account.set", &sign_in(OWNER, expires_at));
    }
    core
}

fn sign_in(subject: &str, expires_at: u64) -> Value {
    json!({"account": "google", "label": "default", "provider": "google", "subject": subject, "scopes": ["https://www.googleapis.com/auth/gmail.readonly"], "access_token": "fake-access-1", "token_type": "Bearer", "expires_at": expires_at})
}

fn rows(core: &Core, account: &str) -> usize {
    let held = core.store().unwrap();
    held.as_ref()
        .unwrap()
        .threads(account, INBOX, 0, 100)
        .unwrap()
        .len()
}

fn sign_out() -> Value {
    json!({"account": "google", "label": "default"})
}

#[test]
fn an_expired_sign_in_still_takes_its_mail_with_it() {
    let google = FakeGoogle::start(Seed::demo(6));
    let core = with_mail(&google, "lapsed-sign-out", 1);
    assert!(core.accounts.signed_in().is_none(), "the token has expired");
    core.account("buwiz.account.clear", &sign_out());
    assert_eq!(rows(&core, OWNER), 0);
}

#[test]
fn signing_in_as_someone_else_on_the_label_forgets_the_last_one() {
    let google = FakeGoogle::start(Seed::demo(6));
    let core = with_mail(&google, "swap", 4_000_000_000);
    core.account(
        "buwiz.account.set",
        &sign_in("other@example.com", 4_000_000_000),
    );
    assert_eq!(rows(&core, OWNER), 0);
}

#[test]
fn a_refreshed_token_keeps_the_mail() {
    let google = FakeGoogle::start(Seed::demo(6));
    let core = with_mail(&google, "refresh", 4_000_000_000);
    let before = rows(&core, OWNER);
    core.account("buwiz.account.set", &sign_in(OWNER, 4_000_000_001));
    assert_eq!(rows(&core, OWNER), before);
}

#[test]
fn uninstall_before_provision_says_its_files_stayed() {
    let google = FakeGoogle::start(Seed::default());
    let dir = std::env::temp_dir().join(format!("gws-stub-unprov-{}", std::process::id()));
    let core = Core::new(&Apis::at(&google.base()), Ok(gws_store::Paths::in_dir(dir)));
    assert_eq!(core.uninstall().unwrap_err().code, "not_provisioned");
}

#[test]
fn uninstall_says_when_it_could_not_reach_the_files() {
    let google = FakeGoogle::start(Seed::default());
    let core = Core::new(
        &Apis::at(&google.base()),
        Err(gws_store::Error {
            code: "no_home",
            message: "no home".to_owned(),
        }),
    );
    core.unlock("a-seed");
    assert_eq!(core.uninstall().unwrap_err().code, "no_home");
}

/// The screens' side of mail, on a whole fake account.
mod mail {
    use super::*;
    use gws_fake_google::Mail;

    fn ready(name: &str, mails: usize) -> (FakeGoogle, Core) {
        let google = FakeGoogle::start(Seed::demo(mails));
        let core = signed_in(&google, name);
        core.unlock("a-seed");
        gws::mail::sync(&core).unwrap();
        (google, core)
    }

    fn data(r: Value) -> Value {
        assert_ne!(r["isError"], true, "{r}");
        r["structuredContent"].clone()
    }

    fn fresh(id: &str) -> Mail {
        Mail {
            id: id.into(),
            thread: id.into(),
            labels: vec!["INBOX".into(), "UNREAD".into()],
            from: "New Person <new@example.com>".into(),
            to: "owner@example.com".into(),
            subject: "Just arrived".into(),
            at: 1_800_000_000_000,
            text: "hello".into(),
            html: None,
            attachments: Vec::new(),
        }
    }

    #[test]
    fn a_first_sync_fills_the_inbox_and_the_labels_and_writes_nothing() {
        let (google, core) = ready("first", 20);
        let list = data(core.run("gws_mail_list", &json!({})));
        let rows = list["threads"].as_array().unwrap();
        assert_eq!(rows.len(), 10, "20 mails in threads of two");
        assert_eq!(rows[0]["messages"], 2);
        assert!(
            rows.windows(2)
                .all(|w| w[0]["at"].as_i64() >= w[1]["at"].as_i64()),
            "newest first"
        );
        let labels = data(core.run("gws_mail_labels", &json!({})));
        let inbox = labels["labels"]
            .as_array()
            .unwrap()
            .iter()
            .find(|l| l["id"] == "INBOX")
            .unwrap()
            .clone();
        assert_eq!(inbox["total"], 20);
        assert_eq!(google.writes(), []);
    }

    #[test]
    fn opening_a_thread_shows_it_cleaned_and_marks_nothing_read() {
        let (google, core) = ready("open", 4);
        let list = data(core.run("gws_mail_list", &json!({})));
        let id = list["threads"][0]["id"].as_str().unwrap().to_owned();
        let view = data(core.run("gws_mail_thread", &json!({"thread": id})));
        assert!(view["page"].as_str().unwrap().contains("fake"), "{view}");
        assert_eq!(
            google.writes(),
            [],
            "opening writes nothing: reading isn't marking read"
        );
        let before = google.log().len();
        data(core.run("gws_mail_thread", &json!({"thread": id})));
        assert_eq!(
            google.log().len(),
            before,
            "the second open comes from the store"
        );
    }

    #[test]
    fn hostile_html_arrives_clean() {
        let google = FakeGoogle::start(Seed { only_grants: None, mails: vec![Mail { html: Some(r#"<p>hi</p><meta http-equiv=refresh content="0;url=https://evil.example/"><img src="https://evil.example/p.png"><a href="https://real.example/">go</a>"#.into()), ..fresh("aaa1") }] });
        let core = signed_in(&google, "hostile");
        core.unlock("a-seed");
        gws::mail::sync(&core).unwrap();
        let view = data(core.run("gws_mail_thread", &json!({"thread": "aaa1"})));
        assert!(!view.to_string().contains("evil.example"), "{view}");
        assert_eq!(
            (view["links"].clone(), view["images"].clone()),
            (json!(["https://real.example/"]), json!(1))
        );
    }

    #[test]
    fn new_mail_comes_by_history_and_tells_the_screen() {
        let (google, core) = ready("history", 4);
        let seen = data(core.run("gws_mail_changes", &json!({})))["seq"]
            .as_i64()
            .unwrap();
        assert_eq!(
            data(core.run("gws_mail_changes", &json!({"since": seen})))["changed"],
            false
        );
        google.deliver(fresh("bbb2"));
        gws::mail::sync(&core).unwrap();
        assert_eq!(
            data(core.run("gws_mail_changes", &json!({"since": seen})))["changed"],
            true
        );
        assert_eq!(
            data(core.run("gws_mail_list", &json!({})))["threads"][0]["subject"],
            "Just arrived"
        );
        let full_lists = google
            .log()
            .iter()
            .filter(|s| s.target.contains("/threads?"))
            .count();
        google.deliver(fresh("ccc3"));
        gws::mail::sync(&core).unwrap();
        assert_eq!(
            google
                .log()
                .iter()
                .filter(|s| s.target.contains("/threads?"))
                .count(),
            full_lists,
            "history, not a full list"
        );
    }

    #[test]
    fn a_history_google_forgot_means_a_full_sync() {
        let (google, core) = ready("expired", 4);
        google.deliver(fresh("ddd4"));
        google.expire_history();
        gws::mail::sync(&core).unwrap();
        assert_eq!(
            data(core.run("gws_mail_list", &json!({})))["threads"][0]["subject"],
            "Just arrived"
        );
    }

    #[test]
    fn a_search_finds_mail_and_with_no_connection_searches_whats_stored() {
        let (google, core) = ready("search", 8);
        let found = data(core.run("gws_mail_list", &json!({"query": "from:ana"})));
        assert!(!found["threads"].as_array().unwrap().is_empty());
        assert_eq!(found["saved"], false);
        drop(google);
        let dead = Core::new(&Apis::at("http://127.0.0.1:1"), core.paths.clone());
        dead.unlock("a-seed");
        dead.accounts.set(&json!({"account": "google", "label": "default", "subject": "owner@example.com", "scopes": ["x"], "access_token": "fake-access-1", "expires_at": 4_000_000_000u64}));
        drop(core);
        let saved = data(dead.run("gws_mail_list", &json!({"query": "Lunch"})));
        assert_eq!(saved["saved"], true);
        assert!(!saved["threads"].as_array().unwrap().is_empty());
        assert!(
            !data(dead.run("gws_mail_list", &json!({})))["threads"]
                .as_array()
                .unwrap()
                .is_empty(),
            "the inbox still reads"
        );
        let _ = gws::mail::sync(&dead);
        assert_eq!(
            data(dead.run("gws_mail_changes", &json!({})))["status"],
            "offline"
        );
    }

    #[test]
    fn a_chat_search_offers_a_card_with_no_mail_in_its_props() {
        let (_google, core) = ready("card", 12);
        let result = core.run_for_chat("gws_search_mail", &json!({"query": "from:ana"}));
        let open = result["_meta"]["buwiz/open"].clone();
        assert_eq!(
            (open["app"].clone(), open["placement"].clone()),
            (json!("mail"), json!("inline"))
        );
        let ticket = open["props"]["search"].as_str().unwrap().to_owned();
        assert!(buwiz_plugin::is_ticket(&ticket));
        assert!(
            !open.to_string().contains("ana") && !open.to_string().contains("Ana"),
            "{open}"
        );
        let card = data(core.run("gws_mail_card", &json!({"ticket": ticket})));
        let rows = card["rows"].as_array().unwrap();
        assert!(!rows.is_empty() && rows.len() <= 5);
        assert!(
            rows.iter()
                .all(|r| r["sender"].as_str().unwrap().contains("Ana")),
            "{card}"
        );
        let opened = data(core.run("gws_mail_thread", &json!({"thread": rows[0]["ticket"]})));
        assert!(
            opened["page"].as_str().unwrap().contains("fake"),
            "a row opens by its ticket"
        );
        let full = data(core.run("gws_mail_list", &json!({"search": ticket})));
        assert!(
            !full["threads"].as_array().unwrap().is_empty(),
            "maximize carries the search"
        );
    }

    #[test]
    fn nothing_found_or_a_routine_gets_no_card() {
        let (_google, core) = ready("nocard", 4);
        assert_eq!(
            core.run_for_chat("gws_search_mail", &json!({"query": "zzzzzz"}))
                .get("_meta"),
            None
        );
        assert_eq!(
            core.run("gws_search_mail", &json!({"query": "from:ana"}))
                .get("_meta"),
            None
        );
        assert_eq!(
            core.run(
                "gws_mail_card",
                &json!({"ticket": "tkt_00000000000000000000000000"})
            )["structuredContent"]["code"],
            "not_found"
        );
    }

    #[test]
    fn signed_out_the_tools_ask_for_sign_in() {
        let google = FakeGoogle::start(Seed::default());
        let core = signed_in(&google, "out");
        core.unlock("a-seed");
        core.accounts
            .clear(&json!({"account": "google", "label": "default"}));
        assert_eq!(
            core.run("gws_mail_list", &json!({}))["structuredContent"]["code"],
            "account_required"
        );
    }

    #[test]
    fn a_page_past_the_stored_rows_is_fetched() {
        let (_google, core) = ready("pages", 300);
        let first = data(core.run("gws_mail_list", &json!({})));
        assert_eq!(first["threads"].as_array().unwrap().len(), 50);
        assert_eq!(first["more"], true);
        let second = data(core.run("gws_mail_list", &json!({"skip": 50})));
        assert_eq!(second["threads"].as_array().unwrap().len(), 50);
        assert_ne!(first["threads"][0]["id"], second["threads"][0]["id"]);
    }
}

#[test]
fn drive_file_is_not_a_persons_drive_and_no_scopes_means_unknown() {
    let google = FakeGoogle::start(Seed::default());
    let core = signed_in(&google, "scopes");
    let set = |scopes: Value| {
        core.accounts.set(&json!({"account": "google", "label": "default", "subject": "owner@example.com", "scopes": scopes, "access_token": "fake-access-1", "expires_at": 4_000_000_000u64}));
        core.run("gws_status", &json!({}))["structuredContent"]["features"].clone()
    };
    assert_eq!(
        set(json!(["https://www.googleapis.com/auth/drive.file"]))["drive"],
        false
    );
    assert_eq!(
        set(json!(["https://www.googleapis.com/auth/drive"]))["drive"],
        true
    );
    assert_eq!(
        set(json!([])),
        Value::Null,
        "unknown, which isn't the same as missing"
    );
}

#[test]
fn uninstall_takes_every_token_and_every_file() {
    let google = FakeGoogle::start(Seed::default());
    let core = signed_in(&google, "uninstall");
    assert!(
        core.store().is_err(),
        "the store stays shut until the Bot provisions"
    );
    core.unlock("a-fake-seed");
    drop(core.store().unwrap());
    let paths = core.paths.as_ref().unwrap().clone();
    let db = paths.db(&gws_store::key_from_seed("a-fake-seed"));
    assert!(db.exists());
    core.uninstall().unwrap();
    assert!(core.accounts.google().is_none());
    assert!(!db.exists() && !paths.dir.exists());
}
