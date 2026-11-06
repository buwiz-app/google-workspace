//! Drive's writes against the stub Google: what each sends, where the row is
//! afterwards with no sync run, what Undo puts back, and what a failure leaves
//! alone. No sync runs here, so a list that's right was made right by the write.
//! This proves the plugin against the stub, which is written from what's known
//! of Google's API, not against Google.

use base64::Engine as _;
use gws::Core;
use gws::apis::Apis;
use gws_fake_google::{DRIVE_ROOT, FAKE_EMAIL, FOLDER, FakeFile, FakeGoogle, GOOGLE_DOC, Seed};
use serde_json::{Value, json};

const TOKEN: &str = "fake-access-1";
const HOSTILE: &str = r#"it's' or trashed = true or name contains '\ "x" & y=1 / ..%2f"#;

fn signed_in_at(base: &str, name: &str) -> Core {
    let dir = std::env::temp_dir().join(format!("gws-dwrite-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let core = Core::new(&Apis::at(base), Ok(gws_store::Paths::in_dir(dir)));
    core.unlock("seed");
    core.accounts.set(&json!({"account": "google", "label": "default", "provider": "google", "subject": FAKE_EMAIL, "scopes": ["https://www.googleapis.com/auth/drive"], "access_token": TOKEN, "token_type": "Bearer", "expires_at": 4_000_000_000u64}));
    core
}

fn signed_in(google: &FakeGoogle, name: &str) -> Core {
    signed_in_at(&google.base(), name)
}

fn data(answer: Value) -> Value {
    assert_ne!(answer["isError"], true, "{answer}");
    assert_eq!(answer["structuredContent"].get("refused"), None, "{answer}");
    answer["structuredContent"].clone()
}

fn refusal(answer: &Value) -> String {
    answer["structuredContent"]["refused"]
        .as_str()
        .unwrap_or_else(|| panic!("not refused: {answer}"))
        .to_owned()
}

fn ids(core: &Core, args: Value) -> Vec<String> {
    let page = data(core.run("gws_drive_list", &args));
    page["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["id"].as_str().unwrap().to_owned())
        .collect()
}

fn my(core: &Core) -> Vec<String> {
    ids(core, json!({"view": "my"}))
}

fn folder(core: &Core, id: &str) -> Vec<String> {
    ids(core, json!({"view": "my", "folder": id}))
}

fn trash(core: &Core) -> Vec<String> {
    ids(core, json!({"view": "trash"}))
}

fn row(core: &Core, view: Value, id: &str) -> Value {
    let page = data(core.run("gws_drive_list", &view));
    page["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["id"] == id)
        .cloned()
        .unwrap_or_else(|| panic!("{id} isn't listed: {page}"))
}

fn drive() -> FakeGoogle {
    let google = FakeGoogle::start(Seed::default());
    google.seed_file(FakeFile::folder("drv-clients", "Clients"));
    google.seed_file(FakeFile::folder("drv-photos", "Photos"));
    google.seed_file(FakeFile::new("drv-a", "Alpha.txt", "text/plain").holding(b"a"));
    google.seed_file(FakeFile::new("drv-b", "Beta.txt", "text/plain").holding(b"b"));
    google.seed_file(FakeFile::new("drv-in", "Inside.txt", "text/plain").inside("drv-clients"));
    google
}

/// Every file as Google has it, for a before and an after.
fn whole(google: &FakeGoogle) -> Vec<FakeFile> {
    let mut all = google.drive_files();
    for f in &mut all {
        // what a write moves that a person can't see, and Undo can't move back
        f.version = 0;
        f.modified_ms = 0;
    }
    all
}

// --- A new folder ------------------------------------------------------------

#[test]
fn a_new_folder_is_in_its_list_at_once_and_on_google() {
    let google = drive();
    let core = signed_in(&google, "folder");
    assert_eq!(folder(&core, "drv-clients"), ["drv-in"]);
    let made = data(core.run(
        "gws_drive_folder",
        &json!({"parent": "drv-clients", "name": "  Acme  "}),
    ));
    let id = made["files"][0]["id"].as_str().unwrap().to_owned();
    assert_eq!(made["files"][0]["kind"], "folder");
    assert_eq!(made["files"][0]["name"], "Acme");
    assert_eq!(
        folder(&core, "drv-clients"),
        [id.as_str(), "drv-in"],
        "folders first"
    );
    let held = google.drive_file(&id).unwrap();
    assert_eq!(held.parents, ["drv-clients"]);
    assert_eq!(held.mime, FOLDER);
    let writes = google.drive_writes();
    assert_eq!(writes.len(), 1);
    assert_eq!(
        (writes[0].method.as_str(), writes[0].path.as_str()),
        ("POST", "files")
    );
}

#[test]
fn the_top_of_my_drive_goes_out_as_its_real_id() {
    let google = drive();
    let core = signed_in(&google, "root");
    let before = my(&core);
    let made = data(core.run(
        "gws_drive_folder",
        &json!({"parent": "root", "name": "Top"}),
    ));
    assert_eq!(
        google.drive_writes()[0].body["parents"],
        json!([DRIVE_ROOT])
    );
    assert_eq!(my(&core).len(), before.len() + 1, "{made}");
}

#[test]
fn a_name_goes_in_json_whole_and_never_in_a_query_or_a_path() {
    let google = drive();
    let core = signed_in(&google, "hostile");
    my(&core);
    let made = data(core.run(
        "gws_drive_folder",
        &json!({"parent": "root", "name": HOSTILE}),
    ));
    let id = made["files"][0]["id"].as_str().unwrap().to_owned();
    assert_eq!(google.drive_file(&id).unwrap().name, HOSTILE);
    data(core.run("gws_drive_rename", &json!({"id": "drv-a", "name": HOSTILE})));
    assert_eq!(google.drive_file("drv-a").unwrap().name, HOSTILE);
    assert_eq!(google.drive_writes()[1].path, "files/drv-a");
    assert_eq!(google.drive_writes()[1].body, json!({"name": HOSTILE}));
    for seen in google.log() {
        for word in ["trashed = true", "name contains", "%2f", "..", "y=1"] {
            assert!(!seen.target.contains(word), "{word} in {}", seen.target);
        }
    }
    assert_eq!(
        row(&core, json!({"view": "my"}), "drv-a")["name"],
        HOSTILE,
        "and back as it went"
    );
}

#[test]
fn a_name_that_cant_be_one_never_leaves() {
    let google = drive();
    let core = signed_in(&google, "bad-name");
    for name in ["", "   ", "two\nlines", "tab\there", &"x".repeat(2000)] {
        let answer = core.run("gws_drive_folder", &json!({"parent": "root", "name": name}));
        assert_eq!(refusal(&answer), "invalid_args", "{name}");
        let answer = core.run("gws_drive_rename", &json!({"id": "drv-a", "name": name}));
        assert_eq!(refusal(&answer), "invalid_args", "{name}");
    }
    for id in ["", "../x", "a b", "a'b", "a/permissions"] {
        let answer = core.run("gws_drive_rename", &json!({"id": id, "name": "x"}));
        assert_eq!(refusal(&answer), "invalid_args", "{id}");
        let answer = core.run("gws_drive_folder", &json!({"parent": id, "name": "x"}));
        assert_eq!(refusal(&answer), "invalid_args", "{id}");
        let answer = core.run("gws_drive_move", &json!({"id": "drv-a", "to": id}));
        assert_eq!(refusal(&answer), "invalid_args", "{id}");
    }
    assert_eq!(google.writes(), []);
}

// --- Rename and move ---------------------------------------------------------

#[test]
fn a_rename_shows_at_once_in_its_place_and_undo_puts_the_name_back() {
    let google = drive();
    let core = signed_in(&google, "rename");
    assert_eq!(my(&core), ["drv-clients", "drv-photos", "drv-a", "drv-b"]);
    let before = whole(&google);
    let done = data(core.run(
        "gws_drive_rename",
        &json!({"id": "drv-a", "name": "Zed.txt"}),
    ));
    assert_eq!(done["files"][0]["name"], "Zed.txt");
    assert_eq!(
        my(&core),
        ["drv-clients", "drv-photos", "drv-b", "drv-a"],
        "the list has it in its new place with no sync"
    );
    assert_eq!(google.drive_file("drv-a").unwrap().name, "Zed.txt");
    let back = data(core.run("gws_drive_undo", &json!({"undo": done["undo"]})));
    assert_eq!(back["files"][0]["name"], "Alpha.txt");
    assert_eq!(whole(&google), before, "exactly what was there");
    assert_eq!(my(&core), ["drv-clients", "drv-photos", "drv-a", "drv-b"]);
    assert_eq!(
        refusal(&core.run("gws_drive_undo", &json!({"undo": done["undo"]}))),
        "not_found",
        "an Undo is spent"
    );
}

#[test]
fn undo_goes_by_what_google_held_and_not_what_a_screen_showed() {
    let google = drive();
    let core = signed_in(&google, "rename-live");
    my(&core);
    // someone renames it behind the store's back
    google.edit_file("drv-a", |f| f.name = "Theirs.txt".to_owned());
    let done = data(core.run(
        "gws_drive_rename",
        &json!({"id": "drv-a", "name": "Mine.txt"}),
    ));
    data(core.run("gws_drive_undo", &json!({"undo": done["undo"]})));
    assert_eq!(google.drive_file("drv-a").unwrap().name, "Theirs.txt");
}

#[test]
fn a_move_leaves_one_list_for_the_other_at_once_and_undo_goes_back() {
    let google = drive();
    let core = signed_in(&google, "move");
    my(&core);
    assert_eq!(folder(&core, "drv-clients"), ["drv-in"]);
    let before = whole(&google);
    let done = data(core.run(
        "gws_drive_move",
        &json!({"id": "drv-a", "to": "drv-clients"}),
    ));
    assert!(!my(&core).contains(&"drv-a".to_owned()));
    assert_eq!(folder(&core, "drv-clients"), ["drv-a", "drv-in"]);
    let writes = google.drive_writes();
    assert_eq!(writes[0].method, "PATCH");
    assert_eq!(writes[0].path, "files/drv-a");
    assert_eq!(writes[0].query["addParents"], "drv-clients");
    assert_eq!(writes[0].query["removeParents"], DRIVE_ROOT);
    assert_eq!(writes[0].body, json!({}));
    data(core.run("gws_drive_undo", &json!({"undo": done["undo"]})));
    assert_eq!(whole(&google), before);
    assert_eq!(folder(&core, "drv-clients"), ["drv-in"]);
    assert!(my(&core).contains(&"drv-a".to_owned()));
}

#[test]
fn undo_of_a_move_puts_back_every_parent_a_file_had() {
    let google = drive();
    google.edit_file("drv-a", |f| {
        f.parents = vec![DRIVE_ROOT.to_owned(), "drv-photos".to_owned()];
    });
    let core = signed_in(&google, "two-parents");
    let done = data(core.run(
        "gws_drive_move",
        &json!({"id": "drv-a", "to": "drv-clients"}),
    ));
    assert_eq!(google.drive_file("drv-a").unwrap().parents, ["drv-clients"]);
    data(core.run("gws_drive_undo", &json!({"undo": done["undo"]})));
    let mut parents = google.drive_file("drv-a").unwrap().parents;
    parents.sort();
    let mut was = vec![DRIVE_ROOT.to_owned(), "drv-photos".to_owned()];
    was.sort();
    assert_eq!(parents, was);
}

#[test]
fn a_folder_cant_go_inside_itself_or_where_it_already_is() {
    let google = drive();
    let core = signed_in(&google, "cycle");
    google.seed_file(FakeFile::folder("drv-sub", "Sub").inside("drv-clients"));
    assert_eq!(
        refusal(&core.run(
            "gws_drive_move",
            &json!({"id": "drv-clients", "to": "drv-clients"})
        )),
        "invalid_args"
    );
    assert_eq!(
        refusal(&core.run(
            "gws_drive_move",
            &json!({"id": "drv-clients", "to": "drv-sub"})
        )),
        "no_access",
        "Google's own refusal, in words"
    );
    assert_eq!(
        refusal(&core.run(
            "gws_drive_move",
            &json!({"id": "drv-in", "to": "drv-clients"})
        )),
        "already_there"
    );
    assert_eq!(google.drive_writes(), []);
}

// --- The Trash ---------------------------------------------------------------

#[test]
fn trash_only_ever_sets_trashed_and_the_file_is_still_on_google() {
    let google = drive();
    let core = signed_in(&google, "trash");
    my(&core);
    assert_eq!(trash(&core), Vec::<String>::new());
    let done = data(core.run("gws_drive_trash", &json!({"ids": ["drv-a", "drv-b"]})));
    assert_eq!(done["files"].as_array().unwrap().len(), 2);
    assert_eq!(
        my(&core),
        ["drv-clients", "drv-photos"],
        "gone with no sync"
    );
    assert_eq!(trash(&core), ["drv-a", "drv-b"]);
    let writes = google.writes();
    assert_eq!(writes.len(), 2);
    for (seen, id) in writes.iter().zip(["drv-a", "drv-b"]) {
        assert_eq!(seen.method, "PATCH", "never a DELETE");
        assert!(
            seen.target.starts_with(&format!("/drive/v3/files/{id}?")),
            "{seen:?}"
        );
        assert_eq!(seen.body, r#"{"trashed":true}"#);
    }
    for id in ["drv-a", "drv-b"] {
        let held = google.drive_file(id).expect("still there");
        assert!(held.trashed);
    }
}

#[test]
fn undo_restores_what_this_press_trashed_and_nothing_else() {
    let google = drive();
    google.edit_file("drv-b", |f| f.trashed = true);
    let core = signed_in(&google, "untrash");
    my(&core);
    let before = whole(&google);
    let done = data(core.run("gws_drive_trash", &json!({"ids": ["drv-a", "drv-b"]})));
    assert_eq!(
        done["files"].as_array().unwrap().len(),
        1,
        "it was in the Trash already"
    );
    assert!(
        google
            .drive_writes()
            .iter()
            .all(|w| w.path == "files/drv-a")
    );
    let back = data(core.run("gws_drive_undo", &json!({"undo": done["undo"]})));
    assert_eq!(back["files"][0]["trashed"], false);
    assert_eq!(
        whole(&google),
        before,
        "and what was there before stays there"
    );
    assert!(my(&core).contains(&"drv-a".to_owned()));
    assert_eq!(trash(&core), ["drv-b"]);
}

#[test]
fn a_folder_in_the_trash_takes_its_files_and_undo_brings_them_back() {
    let google = drive();
    google.seed_file(FakeFile::new("drv-early", "Early.txt", "text/plain").inside("drv-clients"));
    let core = signed_in(&google, "folder-trash");
    my(&core);
    google.edit_file("drv-early", |f| f.trashed = true);
    let done = data(core.run("gws_drive_trash", &json!({"ids": ["drv-clients"]})));
    assert!(
        google.drive_file("drv-in").unwrap().trashed,
        "the stub's own rule"
    );
    data(core.run("gws_drive_undo", &json!({"undo": done["undo"]})));
    assert!(!google.drive_file("drv-in").unwrap().trashed);
    assert!(google.drive_file("drv-early").unwrap().trashed);
}

#[test]
fn a_press_on_many_stops_at_the_first_refusal_and_keeps_what_went() {
    let google = drive();
    google.seed_file(
        FakeFile::new("drv-theirs", "Theirs.txt", "text/plain")
            .shared_by("Ana Reyes", "ana@example.com"),
    );
    let core = signed_in(&google, "partial");
    my(&core);
    let done = data(core.run(
        "gws_drive_trash",
        &json!({"ids": ["drv-a", "drv-theirs", "drv-b"]}),
    ));
    assert_eq!(done["partial"], true);
    assert_eq!(done["files"].as_array().unwrap().len(), 1);
    assert!(google.drive_file("drv-a").unwrap().trashed);
    assert!(!google.drive_file("drv-theirs").unwrap().trashed);
    assert!(!google.drive_file("drv-b").unwrap().trashed, "never asked");
    data(core.run("gws_drive_undo", &json!({"undo": done["undo"]})));
    assert!(!google.drive_file("drv-a").unwrap().trashed);
}

#[test]
fn nothing_can_be_trashed_past_the_limit_or_by_a_bad_id() {
    let google = drive();
    let core = signed_in(&google, "trash-args");
    let many: Vec<String> = (0..101).map(|n| format!("drv-{n}")).collect();
    for ids in [
        json!([]),
        json!(many),
        json!(["../x"]),
        json!(1),
        json!([1]),
    ] {
        let answer = core.run("gws_drive_trash", &json!({"ids": ids}));
        assert_eq!(refusal(&answer), "invalid_args", "{ids}");
    }
    assert_eq!(google.writes(), []);
}

/// The stub keeps the rule so a code path that ever tried it would fail here.
#[test]
fn a_permanent_delete_is_no_call_the_stub_or_the_api_has() {
    let source = include_str!("../src/drive/write.rs");
    let deletes = source.matches(".delete(").count();
    assert_eq!(deletes, 1, "the one in `unshare`");
    assert!(
        source.contains(r#".delete(format!("{}/files/{id}/permissions/{person}", self.base))"#)
    );
    assert!(!source.contains("emptyTrash"));
}

// --- What a failure leaves alone ---------------------------------------------

/// What every write here must do on a refusal: nothing, anywhere.
fn each_write(core: &Core) -> Vec<(&'static str, Value)> {
    let seen = data(core.run("gws_drive_access", &json!({"id": "drv-a"})))["seen"].clone();
    vec![
        ("gws_drive_folder", json!({"parent": "root", "name": "New"})),
        ("gws_drive_rename", json!({"id": "drv-a", "name": "New"})),
        (
            "gws_drive_move",
            json!({"id": "drv-a", "to": "drv-clients"}),
        ),
        ("gws_drive_trash", json!({"ids": ["drv-a"]})),
        (
            "gws_drive_share",
            json!({"id": "drv-a", "add": [{"email": "ana@example.com", "role": "reader"}], "seen": seen}),
        ),
    ]
}

#[test]
fn a_write_google_refuses_changes_nothing_and_says_why() {
    for (status, code) in [(403, "no_access"), (404, "not_found")] {
        let google = drive();
        let core = signed_in(&google, "refused");
        let (listed, in_folder) = (my(&core), folder(&core, "drv-clients"));
        let before = whole(&google);
        let writes = each_write(&core);
        google.drive_writes_fail_after(0, status);
        for (tool, args) in writes {
            let answer = core.run(tool, &args);
            assert_eq!(refusal(&answer), code, "{tool}");
            assert_eq!(answer["structuredContent"].get("undo"), None, "{tool}");
            let words = answer["structuredContent"]["message"].as_str().unwrap();
            assert!(words.contains("nothing changed"), "{tool}: {words}");
            assert!(!answer.to_string().contains(TOKEN));
        }
        assert_eq!(my(&core), listed, "the list");
        assert_eq!(folder(&core, "drv-clients"), in_folder);
        assert_eq!(trash(&core), Vec::<String>::new());
        assert_eq!(whole(&google), before, "and Google");
        assert_eq!(google.drive_writes(), []);
    }
}

#[test]
fn a_write_google_cant_do_right_now_changes_nothing_and_says_try_again() {
    for status in [500, 429] {
        let google = drive();
        let core = signed_in(&google, "busy");
        let listed = my(&core);
        let writes = each_write(&core);
        google.drive_writes_fail_after(0, status);
        for (tool, args) in writes {
            let answer = core.run(tool, &args);
            assert_eq!(answer["isError"], true, "{tool}: {answer}");
            let words = answer["content"][0]["text"].as_str().unwrap();
            assert!(words.contains("Google Drive"), "{tool}: {words}");
        }
        assert_eq!(my(&core), listed);
        assert_eq!(google.drive_writes(), []);
    }
}

#[test]
fn no_connection_is_said_in_plain_words_with_nothing_of_the_network_in_them() {
    let core = signed_in_at("http://127.0.0.1:9", "offline");
    for (tool, args) in [
        ("gws_drive_rename", json!({"id": "drv-a", "name": "New"})),
        ("gws_drive_trash", json!({"ids": ["drv-a"]})),
        ("gws_drive_move", json!({"id": "drv-a", "to": "drv-b"})),
        (
            "gws_drive_folder",
            json!({"parent": "drv-b", "name": "New"}),
        ),
    ] {
        let answer = core.run(tool, &args);
        assert_eq!(refusal(&answer), "offline", "{tool}");
        let words = answer["structuredContent"]["message"].as_str().unwrap();
        assert_eq!(
            words,
            "Google Drive can't be reached right now, so nothing changed."
        );
    }
}

#[test]
fn a_failed_undo_can_be_pressed_again_and_changes_nothing_till_it_works() {
    let google = drive();
    let core = signed_in(&google, "undo-fails");
    my(&core);
    let done = data(core.run("gws_drive_trash", &json!({"ids": ["drv-a", "drv-b"]})));
    google.drive_writes_fail_after(1, 403);
    let first = core.run("gws_drive_undo", &json!({"undo": done["undo"]}));
    assert_eq!(refusal(&first), "no_access");
    assert!(
        !google.drive_file("drv-a").unwrap().trashed,
        "the first went"
    );
    assert!(google.drive_file("drv-b").unwrap().trashed);
    google.drive_works();
    data(core.run("gws_drive_undo", &json!({"undo": done["undo"]})));
    assert!(
        !google.drive_file("drv-b").unwrap().trashed,
        "the rest, under the same id"
    );
    assert_eq!(trash(&core), Vec::<String>::new());
}

#[test]
fn a_sign_in_lost_at_a_write_asks_for_sign_in() {
    let google = drive();
    let core = signed_in(&google, "signed-out");
    core.account(
        "buwiz.account.clear",
        &json!({"account": "google", "label": "default"}),
    );
    let answer = core.run("gws_drive_trash", &json!({"ids": ["drv-a"]}));
    assert_eq!(answer["structuredContent"]["code"], "account_required");
    assert_eq!(google.writes(), []);
}

#[test]
fn a_sign_in_without_write_access_says_so() {
    let google = drive();
    let core = signed_in(&google, "scope");
    my(&core);
    google.only_grants("gmail");
    let answer = core.run("gws_drive_rename", &json!({"id": "drv-a", "name": "x"}));
    assert_eq!(
        answer["structuredContent"]["code"], "needs_access",
        "{answer}"
    );
}

// --- What a file allows ------------------------------------------------------

#[test]
fn a_shared_file_the_person_cant_edit_says_so_and_google_refuses_it_anyway() {
    let google = drive();
    google.seed_file(
        FakeFile::new("drv-read", "Brief", GOOGLE_DOC)
            .shared_by("Ana Reyes", "ana@example.com")
            .read_only(),
    );
    let core = signed_in(&google, "read-only");
    let held = row(&core, json!({"view": "shared"}), "drv-read");
    assert_eq!(held["can"], json!([]), "{held}");
    let mine = row(&core, json!({"view": "my"}), "drv-a");
    assert_eq!(mine["can"], json!(["rename", "trash", "move", "share"]));
    let box_ = row(&core, json!({"view": "my"}), "drv-clients");
    assert!(box_["can"].as_array().unwrap().contains(&json!("add")));
    assert_eq!(mine["parents"], json!([DRIVE_ROOT]));

    let stale = [
        ("gws_drive_rename", json!({"id": "drv-read", "name": "x"})),
        ("gws_drive_trash", json!({"ids": ["drv-read"]})),
        ("gws_drive_access", json!({"id": "drv-read"})),
    ];
    for (tool, args) in stale {
        assert_eq!(refusal(&core.run(tool, &args)), "no_access", "{tool}");
    }
    assert_eq!(google.drive_writes(), []);
}

#[test]
fn a_list_says_if_a_new_file_can_go_in_it() {
    let google = drive();
    google.seed_file(
        FakeFile::folder("drv-theirs", "Theirs")
            .shared_by("Ana Reyes", "ana@example.com")
            .read_only(),
    );
    let core = signed_in(&google, "can-add");
    let can_add = |args: Value| data(core.run("gws_drive_list", &args))["can_add"].clone();
    assert_eq!(can_add(json!({"view": "my"})), true);
    assert_eq!(
        can_add(json!({"view": "my", "folder": "drv-clients"})),
        true
    );
    for view in ["shared", "recent", "starred", "trash"] {
        assert_eq!(can_add(json!({"view": view})), false, "{view}");
    }
    assert_eq!(
        can_add(json!({"view": "shared", "folder": "drv-theirs"})),
        false
    );
    let at = |args: Value| data(core.run("gws_drive_list", &args))["at"].clone();
    assert_eq!(at(json!({"view": "my"})), DRIVE_ROOT);
    assert_eq!(
        at(json!({"view": "my", "folder": "drv-clients"})),
        "drv-clients"
    );
}

// --- Uploads -----------------------------------------------------------------

/// A file in by chunks, as the screen sends it: its upload id.
fn chunks(core: &Core, name: &str, mime: &str, bytes: &[u8]) -> Value {
    let mut upload = Value::Null;
    let mut at = 0;
    loop {
        let end = (at + 180 * 1024).min(bytes.len());
        let piece = base64::engine::general_purpose::STANDARD.encode(&bytes[at..end]);
        let mut args = json!({"name": name, "type": mime, "at": at, "data": piece});
        if !upload.is_null() {
            args["upload"] = upload.clone();
        }
        let got = data(core.run("gws_upload_chunk", &args));
        upload = got["upload"].clone();
        at = got["size"].as_u64().unwrap() as usize;
        if at >= bytes.len() {
            return upload;
        }
    }
}

#[test]
fn a_picked_file_lands_in_the_open_folder_and_its_list_at_once() {
    let google = drive();
    let core = signed_in(&google, "upload");
    assert_eq!(folder(&core, "drv-clients"), ["drv-in"]);
    let upload = chunks(&core, "Roof quote.pdf", "application/pdf", b"%PDF fake");
    let done = data(core.run(
        "gws_drive_upload",
        &json!({"upload": upload, "parent": "drv-clients"}),
    ));
    let id = done["files"][0]["id"].as_str().unwrap().to_owned();
    assert_eq!(done["files"][0]["name"], "Roof quote.pdf");
    assert_eq!(done["files"][0]["kind"], "pdf");
    let held = google.drive_file(&id).unwrap();
    assert_eq!(held.content, b"%PDF fake");
    assert_eq!(held.parents, ["drv-clients"]);
    assert_eq!(folder(&core, "drv-clients"), ["drv-in", id.as_str()]);
    assert!(
        google.writes()[0].target.contains("uploadType=resumable"),
        "the resumable route: {:?}",
        google.writes()[0]
    );
    assert_eq!(
        core.uploads.lock().unwrap().held_bytes(),
        0,
        "nothing stays in memory"
    );
    assert_eq!(
        refusal(&core.run(
            "gws_drive_upload",
            &json!({"upload": upload, "parent": "drv-clients"})
        )),
        "not_found",
        "a press is spent"
    );
    assert_eq!(
        google.drive_files().iter().filter(|f| f.id == id).count(),
        1
    );
}

#[test]
fn a_big_file_goes_in_pieces_and_arrives_whole() {
    let google = drive();
    let core = signed_in(&google, "upload-big");
    let bytes: Vec<u8> = (0..9 * 1024 * 1024).map(|n| (n % 251) as u8).collect();
    let upload = chunks(&core, "Film.bin", "application/octet-stream", &bytes);
    let done = data(core.run(
        "gws_drive_upload",
        &json!({"upload": upload, "parent": "root"}),
    ));
    let id = done["files"][0]["id"].as_str().unwrap();
    assert_eq!(google.drive_file(id).unwrap().content, bytes);
    let pieces = google.log().iter().filter(|s| s.method == "PUT").count();
    assert_eq!(pieces, 2, "8 MB and the rest");
}

#[test]
fn an_empty_file_is_still_a_file() {
    let google = drive();
    let core = signed_in(&google, "upload-empty");
    let upload = chunks(&core, "Empty.txt", "text/plain", b"");
    let done = data(core.run(
        "gws_drive_upload",
        &json!({"upload": upload, "parent": "root"}),
    ));
    let id = done["files"][0]["id"].as_str().unwrap();
    let held = google.drive_file(id).unwrap();
    assert_eq!((held.name.as_str(), held.content.len()), ("Empty.txt", 0));
}

#[test]
fn a_file_cant_say_it_is_a_google_file_and_a_hostile_name_stays_a_name() {
    let google = drive();
    let core = signed_in(&google, "upload-mime");
    let upload = chunks(&core, HOSTILE, GOOGLE_DOC, b"words");
    let done = data(core.run(
        "gws_drive_upload",
        &json!({"upload": upload, "parent": "root"}),
    ));
    let id = done["files"][0]["id"].as_str().unwrap();
    let held = google.drive_file(id).unwrap();
    assert_eq!(held.mime, "application/octet-stream");
    assert_eq!(held.content, b"words");
    assert!(!google.log().iter().any(|s| s.target.contains("contains")));
}

#[test]
fn an_upload_google_refuses_makes_nothing_and_frees_the_bytes() {
    let google = drive();
    let core = signed_in(&google, "upload-fails");
    my(&core);
    let files = google.drive_files().len();
    let upload = chunks(&core, "A.pdf", "application/pdf", b"%PDF");
    google.drive_uploads_fail_after(0, 403);
    let answer = core.run(
        "gws_drive_upload",
        &json!({"upload": upload, "parent": "root"}),
    );
    assert_eq!(refusal(&answer), "no_access");
    assert_eq!(google.drive_files().len(), files);
    assert_eq!(my(&core).len(), 4, "the list as it was");
    assert_eq!(core.uploads.lock().unwrap().held_bytes(), 0);
}

#[test]
fn an_upload_into_a_folder_gone_says_so() {
    let google = drive();
    let core = signed_in(&google, "upload-gone");
    let upload = chunks(&core, "A.pdf", "application/pdf", b"%PDF");
    let answer = core.run(
        "gws_drive_upload",
        &json!({"upload": upload, "parent": "drv-nowhere"}),
    );
    assert_eq!(refusal(&answer), "not_found");
}

// --- Share -------------------------------------------------------------------

fn seen_of(core: &Core, id: &str) -> Value {
    data(core.run("gws_drive_access", &json!({"id": id})))["seen"].clone()
}

fn with_people() -> FakeGoogle {
    let google = drive();
    google.edit_file("drv-a", |f| {
        f.shared = true;
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
    google
}

fn emails(people: &Value) -> Vec<String> {
    people
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["email"].as_str().unwrap().to_owned())
        .collect()
}

fn permission(core: &Core, id: &str, email: &str) -> Value {
    let all = data(core.run("gws_drive_access", &json!({"id": id})));
    let found = all["people"].as_array().unwrap().iter();
    found.into_iter().find(|p| p["email"] == email).unwrap()["id"].clone()
}

#[test]
fn access_lists_everyone_however_many_pages_and_the_link_too() {
    let google = with_people();
    google.drive_people_page(2);
    google.edit_file("drv-a", |f| {
        for n in 0..30 {
            f.people.push((
                format!("Person {n}"),
                format!("p{n}@example.com"),
                "reader".to_owned(),
            ));
        }
    });
    let core = signed_in(&google, "access");
    let all = data(core.run("gws_drive_access", &json!({"id": "drv-a"})));
    let people = all["people"].as_array().unwrap();
    assert_eq!(
        people.len(),
        1 + 2 + 30 + 1,
        "the owner, the rest, the link"
    );
    assert_eq!(people[0]["role"], "owner");
    assert_eq!(people[0]["you"], true);
    assert_eq!(people[0]["removable"], false);
    assert_eq!(people[1]["name"], "Ana Reyes");
    assert_eq!(people[1]["removable"], true);
    let link = people.last().unwrap();
    assert_eq!(link["name"], "Anyone with the link");
    assert_eq!(link["removable"], false);
    assert!(all["seen"].as_str().unwrap().len() >= 16);
    let pages = google
        .log()
        .iter()
        .filter(|s| s.target.contains("/permissions"))
        .count();
    assert_eq!(pages, 17, "2 at a time");
}

#[test]
fn share_adds_each_person_with_their_role_and_tells_them() {
    let google = with_people();
    let core = signed_in(&google, "share");
    let seen = seen_of(&core, "drv-a");
    let done = data(core.run(
        "gws_drive_share",
        &json!({"id": "drv-a", "add": [
            {"email": "cara@example.com", "role": "writer"},
            {"email": "dan@example.com", "role": "commenter"},
        ], "message": "See this & that = fine + more", "seen": seen}),
    ));
    assert_eq!((&done["added"], &done["removed"]), (&json!(2), &json!(0)));
    let writes = google.drive_writes();
    assert_eq!(writes.len(), 2);
    assert_eq!(writes[0].method, "POST");
    assert_eq!(writes[0].path, "files/drv-a/permissions");
    assert_eq!(
        writes[0].body,
        json!({"type": "user", "role": "writer", "emailAddress": "cara@example.com"})
    );
    assert_eq!(writes[1].body["role"], "commenter");
    for write in &writes {
        assert_eq!(write.query["sendNotificationEmail"], "true");
        assert_eq!(write.query["emailMessage"], "See this & that = fine + more");
    }
    let people = data(core.run("gws_drive_access", &json!({"id": "drv-a"})));
    assert!(emails(&people["people"]).contains(&"dan@example.com".to_owned()));
}

#[test]
fn taking_someone_off_tells_nobody() {
    let google = with_people();
    let core = signed_in(&google, "unshare");
    let ben = permission(&core, "drv-a", "ben@example.com");
    let seen = seen_of(&core, "drv-a");
    let done = data(core.run(
        "gws_drive_share",
        &json!({"id": "drv-a", "remove": [ben], "message": "Goodbye", "seen": seen}),
    ));
    assert_eq!((&done["added"], &done["removed"]), (&json!(0), &json!(1)));
    let writes = google.drive_writes();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].method, "DELETE");
    assert!(
        writes[0].query.is_empty(),
        "no mail, no message: {:?}",
        writes[0]
    );
    assert!(
        !google
            .log()
            .iter()
            .any(|s| s.target.contains("sendNotificationEmail"))
    );
    assert_eq!(google.drive_file("drv-a").unwrap().people.len(), 1);
}

#[test]
fn with_no_message_none_is_sent() {
    let google = with_people();
    let core = signed_in(&google, "no-message");
    let seen = seen_of(&core, "drv-a");
    data(core.run(
        "gws_drive_share",
        &json!({"id": "drv-a", "add": [{"email": "cara@example.com", "role": "reader"}], "message": "   ", "seen": seen}),
    ));
    assert!(!google.drive_writes()[0].query.contains_key("emailMessage"));
}

#[test]
fn someone_new_on_the_file_since_the_list_was_read_stops_the_press() {
    let google = with_people();
    let core = signed_in(&google, "stale");
    let seen = seen_of(&core, "drv-a");
    google.edit_file("drv-a", |f| {
        f.people.push((
            "Eve".to_owned(),
            "eve@example.com".to_owned(),
            "writer".to_owned(),
        ));
    });
    let answer = core.run(
        "gws_drive_share",
        &json!({"id": "drv-a", "add": [{"email": "cara@example.com", "role": "reader"}], "seen": seen}),
    );
    assert_eq!(refusal(&answer), "changed");
    assert_eq!(google.drive_writes(), []);
    // and a role that moved is a list that moved
    let seen = seen_of(&core, "drv-a");
    google.edit_file("drv-a", |f| f.people[0].2 = "reader".to_owned());
    let answer = core.run(
        "gws_drive_share",
        &json!({"id": "drv-a", "add": [{"email": "cara@example.com", "role": "reader"}], "seen": seen}),
    );
    assert_eq!(refusal(&answer), "changed");
    let answer = core.run(
        "gws_drive_share",
        &json!({"id": "drv-a", "add": [{"email": "cara@example.com", "role": "reader"}]}),
    );
    assert_eq!(refusal(&answer), "changed", "no mark is no list read");
    assert_eq!(google.drive_writes(), []);
}

#[test]
fn share_gives_no_owner_and_takes_off_no_owner_link_or_person_themselves() {
    let google = with_people();
    let core = signed_in(&google, "share-limits");
    let seen = seen_of(&core, "drv-a");
    let owner = permission(&core, "drv-a", FAKE_EMAIL);
    let cases = [
        json!({"add": [{"email": "cara@example.com", "role": "owner"}]}),
        json!({"add": [{"email": "cara@example.com", "role": "organizer"}]}),
        json!({"add": [{"email": "not an address", "role": "reader"}]}),
        json!({"add": [{"email": "ANA@example.com", "role": "writer"}]}),
        json!({"remove": [owner]}),
        json!({"remove": ["anyoneWithLink"]}),
        json!({"remove": ["../x"]}),
        json!({"remove": ["prm-unknown"]}),
        json!({}),
    ];
    for mut case in cases {
        case["id"] = json!("drv-a");
        case["seen"] = seen.clone();
        let answer = core.run("gws_drive_share", &case);
        assert_eq!(refusal(&answer), "invalid_args", "{case}");
    }
    assert_eq!(google.drive_writes(), []);
    assert!(google.drive_file("drv-a").unwrap().link.is_some());
}

#[test]
fn a_share_that_goes_partway_says_so_and_the_rest_stays_as_it_was() {
    let google = with_people();
    let core = signed_in(&google, "share-partial");
    let ben = permission(&core, "drv-a", "ben@example.com");
    let seen = seen_of(&core, "drv-a");
    google.drive_writes_fail_after(1, 403);
    let done = data(core.run(
        "gws_drive_share",
        &json!({"id": "drv-a", "add": [
            {"email": "cara@example.com", "role": "reader"},
            {"email": "dan@example.com", "role": "reader"},
        ], "remove": [ben], "seen": seen}),
    ));
    assert_eq!(done["partial"], true);
    assert_eq!((&done["added"], &done["removed"]), (&json!(1), &json!(0)));
    let held = google.drive_file("drv-a").unwrap();
    assert_eq!(held.people.len(), 3, "Ben stays, Cara came, Dan didn't");
}

#[test]
fn a_share_that_changes_nothing_says_why() {
    let google = with_people();
    let core = signed_in(&google, "share-fails");
    let seen = seen_of(&core, "drv-a");
    let before = whole(&google);
    google.drive_writes_fail_after(0, 403);
    let answer = core.run(
        "gws_drive_share",
        &json!({"id": "drv-a", "add": [{"email": "cara@example.com", "role": "reader"}], "seen": seen}),
    );
    assert_eq!(refusal(&answer), "no_access");
    assert_eq!(whole(&google), before);
}

#[test]
fn a_press_read_from_one_file_never_lands_on_another_with_the_same_people() {
    let google = with_people();
    let people = google.drive_file("drv-a").unwrap().people;
    google.edit_file("drv-b", |f| {
        f.shared = true;
        f.people = people;
        f.link = Some("reader".to_owned());
    });
    let core = signed_in(&google, "share-other");
    let seen = seen_of(&core, "drv-a");
    let answer = core.run(
        "gws_drive_share",
        &json!({"id": "drv-b", "add": [{"email": "cara@example.com", "role": "reader"}], "seen": seen}),
    );
    assert_eq!(refusal(&answer), "changed");
    assert_eq!(google.drive_writes(), []);
}

#[test]
fn a_row_says_it_is_shared_as_soon_as_it_is_and_not_after_the_last_goes() {
    let google = drive();
    let core = signed_in(&google, "share-row");
    assert_eq!(row(&core, json!({"view": "my"}), "drv-b")["shared"], false);
    let seen = seen_of(&core, "drv-b");
    data(core.run(
        "gws_drive_share",
        &json!({"id": "drv-b", "add": [{"email": "cara@example.com", "role": "reader"}], "seen": seen}),
    ));
    assert_eq!(
        row(&core, json!({"view": "my"}), "drv-b")["shared"],
        true,
        "with no sync"
    );
    let cara = permission(&core, "drv-b", "cara@example.com");
    let seen = seen_of(&core, "drv-b");
    data(core.run(
        "gws_drive_share",
        &json!({"id": "drv-b", "remove": [cara], "seen": seen}),
    ));
    assert_eq!(row(&core, json!({"view": "my"}), "drv-b")["shared"], false);
}

/// The screen reads these by name, and `gws-screens/tests/drive_real.rs` drives
/// it through them. Exact, so a key dropped here fails here too.
#[test]
fn each_answer_holds_what_the_screen_reads() {
    let keys = |v: &Value| {
        let mut keys: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        keys
    };
    let google = with_people();
    let core = signed_in(&google, "shapes");
    let page = data(core.run("gws_drive_list", &json!({"view": "my"})));
    assert_eq!(
        keys(&page),
        ["at", "can_add", "files", "more", "path", "saved"]
    );
    let row = keys(&page["files"][0]);
    for key in ["can", "parents", "trashed", "name", "id", "kind"] {
        assert!(row.contains(&key.to_owned()), "{key}");
    }
    let who = data(core.run("gws_drive_access", &json!({"id": "drv-a"})));
    assert_eq!(keys(&who), ["people", "seen"]);
    assert_eq!(
        keys(&who["people"][0]),
        ["email", "id", "kind", "name", "removable", "role", "you"]
    );
    let renamed = data(core.run("gws_drive_rename", &json!({"id": "drv-a", "name": "N"})));
    assert_eq!(keys(&renamed), ["files", "undo"]);
    let moved = data(core.run(
        "gws_drive_move",
        &json!({"id": "drv-a", "to": "drv-clients"}),
    ));
    assert_eq!(keys(&moved), ["files", "undo"]);
    let trashed = data(core.run("gws_drive_trash", &json!({"ids": ["drv-b"]})));
    assert_eq!(keys(&trashed), ["files", "undo"]);
    let back = data(core.run("gws_drive_undo", &json!({"undo": trashed["undo"]})));
    assert_eq!(keys(&back), ["files"]);
    let made = data(core.run("gws_drive_folder", &json!({"parent": "root", "name": "N"})));
    assert_eq!(keys(&made), ["files"]);
    let shared = data(core.run(
        "gws_drive_share",
        &json!({"id": "drv-b", "add": [{"email": "c@example.com", "role": "reader"}], "seen": seen_of(&core, "drv-b")}),
    ));
    assert_eq!(keys(&shared), ["added", "files", "removed"]);
}

// --- With the sync -----------------------------------------------------------

#[test]
fn the_sync_finding_the_same_write_again_changes_nothing() {
    let google = drive();
    let core = signed_in(&google, "sync-after");
    assert_eq!(my(&core).len(), 4);
    data(core.run(
        "gws_drive_rename",
        &json!({"id": "drv-a", "name": "Zed.txt"}),
    ));
    data(core.run("gws_drive_trash", &json!({"ids": ["drv-b"]})));
    let after = my(&core);
    gws::mail::sync(&core).ok();
    assert_eq!(my(&core), after);
    assert_eq!(trash(&core), ["drv-b"]);
}

#[test]
fn a_write_tells_the_screen_the_store_moved() {
    let google = drive();
    let core = signed_in(&google, "seq");
    my(&core);
    let seq = || {
        data(core.run("gws_drive_changes", &json!({})))["seq"]
            .as_i64()
            .unwrap()
    };
    let before = seq();
    data(core.run(
        "gws_drive_rename",
        &json!({"id": "drv-a", "name": "Zed.txt"}),
    ));
    assert!(seq() > before);
}
