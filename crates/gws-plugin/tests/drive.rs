//! Drive against the stub Google: lists that page with nothing lost, a list
//! that stops partway, what a search can't do to `q`, what a preview can't
//! return, and a changes sync that keeps to itself. This proves the plugin
//! against the stub, which is written from what's known of Google's API, not
//! against Google.

use std::collections::HashSet;

use base64::Engine as _;
use gws::Core;
use gws::apis::Apis;
use gws::tools::UNTRUSTED_DRIVE;
use gws_fake_google::{
    FAKE_EMAIL, FakeCalendar, FakeFile, FakeGoogle, GOOGLE_DOC, GOOGLE_SHEET, Seed, TINY_PNG,
};
use serde_json::{Value, json};

const TOKEN: &str = "fake-access-1";
const NY: &str = "America/New_York";

fn signed_in_at(base: &str, name: &str, fresh: bool) -> Core {
    let dir = std::env::temp_dir().join(format!("gws-drive-{name}-{}", std::process::id()));
    if fresh {
        let _ = std::fs::remove_dir_all(&dir);
    }
    let core = Core::new(&Apis::at(base), Ok(gws_store::Paths::in_dir(dir)));
    core.unlock("seed");
    core.accounts.set(&json!({"account": "google", "label": "default", "provider": "google", "subject": FAKE_EMAIL, "scopes": ["https://www.googleapis.com/auth/drive"], "access_token": TOKEN, "token_type": "Bearer", "expires_at": 4_000_000_000u64}));
    core
}

fn signed_in(google: &FakeGoogle, name: &str) -> Core {
    signed_in_at(&google.base(), name, true)
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

fn list(core: &Core, args: Value) -> Value {
    data(core.run("gws_drive_list", &args))
}

fn ids(page: &Value) -> Vec<String> {
    let rows = page.get("files").or(page.get("items")).unwrap();
    rows.as_array()
        .unwrap()
        .iter()
        .map(|f| f["id"].as_str().unwrap().to_owned())
        .collect()
}

/// Every row of a list, page by page, as a screen walks it.
fn walk(core: &Core, mut args: Value) -> Vec<Value> {
    let mut all = Vec::new();
    loop {
        args["skip"] = json!(all.len());
        let page = list(core, args.clone());
        let rows = page["files"].as_array().unwrap().clone();
        assert!(rows.len() <= 100, "a page holds 100 at most");
        all.extend(rows);
        if page["more"] != true {
            return all;
        }
    }
}

fn text(row: &Value, key: &str) -> String {
    row[key].as_str().unwrap().to_owned()
}

fn sync(core: &Core) {
    gws::mail::sync(core).unwrap();
}

fn listings(google: &FakeGoogle) -> Vec<String> {
    google
        .log()
        .into_iter()
        .filter(|s| s.target.starts_with("/drive/v3/files?"))
        .map(|s| s.target)
        .collect()
}

fn big() -> FakeGoogle {
    FakeGoogle::start(Seed::default().with_big_folder("big", "Big", 5000))
}

fn big_args() -> Value {
    json!({"view": "my", "folder": "big"})
}

fn assert_whole_and_once(rows: &[Value]) {
    let seen: HashSet<String> = rows.iter().map(|r| text(r, "id")).collect();
    assert_eq!(rows.len(), 5000, "nothing skipped");
    assert_eq!(seen.len(), 5000, "nothing doubled");
}

#[test]
fn a_folder_of_5000_lists_in_pages_with_nothing_lost_and_folders_first() {
    let google = big();
    let core = signed_in(&google, "big");
    let first = list(&core, big_args());
    assert_eq!(first["files"].as_array().unwrap().len(), 100);
    assert_eq!(first["more"], true);
    assert_eq!(first["saved"], false);
    let before = listings(&google)
        .iter()
        .filter(|t| t.contains("big"))
        .count();
    assert_eq!(before, 1, "a page at a time, not the whole folder at once");

    let rows = walk(&core, big_args());
    assert_whole_and_once(&rows);
    let folders = rows.iter().take_while(|r| r["kind"] == "folder").count();
    assert_eq!(folders, 100, "every folder is ahead of every file");
    assert!(rows[folders..].iter().all(|r| r["kind"] != "folder"));
    let names: Vec<String> = rows[folders..].iter().map(|r| text(r, "name")).collect();
    assert!(names.is_sorted(), "and then by name");
    assert!(text(&rows[0], "name") < text(&rows[1], "name"));

    let asked = listings(&google);
    let pages = asked.iter().filter(|t| t.contains("big")).count();
    assert_eq!(pages, 50, "each of Google's pages is read once");
    assert!(asked.iter().any(|t| t.contains("orderBy=folder%2Cname")));
    // a walk from the store reaches Google no more
    walk(&core, big_args());
    assert_eq!(listings(&google).len(), asked.len());
}

/// Google can answer with fewer rows than it was asked for, and still have
/// more: a short page isn't the end, and the next one starts where it stopped.
#[test]
fn short_pages_from_google_change_nothing() {
    let google = big();
    let core = signed_in(&google, "short");
    google.drive_page(37);
    assert_whole_and_once(&walk(&core, big_args()));
}

/// A file that appears while a list is being read pushes a row from one of
/// Google's pages onto the next. The store holds it once, in one place, and
/// the changes sync then brings the new file in among the folders.
#[test]
fn a_row_google_gives_twice_shows_once() {
    let google = big();
    let core = signed_in(&google, "twice");
    sync(&core);
    list(&core, big_args());
    google.add_file(FakeFile::folder("drv-new", "A new folder").inside("big"));
    assert_whole_and_once(&walk(&core, big_args()));
    sync(&core);
    let rows = walk(&core, big_args());
    assert_eq!(text(&rows[0], "id"), "drv-new");
    let seen: HashSet<String> = rows.iter().map(|r| text(r, "id")).collect();
    assert_eq!((rows.len(), seen.len()), (5001, 5001));
}

fn revoked_partway(status: u16, code: &str, name: &str) {
    let google = big();
    let core = signed_in(&google, name);
    let first = list(&core, big_args());
    let second = list(&core, json!({"view": "my", "folder": "big", "skip": 100}));
    google.drive_list_fails_after(0, status);

    let answer = core.run(
        "gws_drive_list",
        &json!({"view": "my", "folder": "big", "skip": 200}),
    );
    assert_eq!(refusal(&answer), code, "{answer}");
    let said = answer["structuredContent"]["message"].as_str().unwrap();
    assert!(
        said.contains("partway") && !said.contains(&status.to_string()),
        "plain words: {said}"
    );
    assert_ne!(
        answer["isError"], true,
        "a refusal keeps its code on the way"
    );

    // what's shown is still there, as it was, and nothing wrong went in
    assert_eq!(ids(&list(&core, big_args())), ids(&first));
    let again = list(&core, json!({"view": "my", "folder": "big", "skip": 100}));
    assert_eq!(ids(&again), ids(&second));
    assert_eq!(again["more"], true, "the list isn't taken to be over");

    // a later try carries on from the page that failed
    google.drive_works();
    assert_whole_and_once(&walk(&core, big_args()));
}

#[test]
fn a_403_on_a_later_page_says_so_and_keeps_what_is_shown() {
    revoked_partway(403, "no_access", "revoked403");
}

#[test]
fn a_401_on_a_later_page_says_so_and_keeps_what_is_shown() {
    revoked_partway(401, "sign_in", "revoked401");
}

/// Rows stored for a page still come when Google won't give the rest of it.
#[test]
fn a_part_page_comes_before_the_refusal() {
    let google = big();
    let core = signed_in(&google, "part");
    google.drive_page(60);
    list(&core, big_args());
    google.drive_list_fails_after(0, 403);
    let got = list(&core, json!({"view": "my", "folder": "big", "skip": 60}));
    assert_eq!(got["files"].as_array().unwrap().len(), 60);
    assert_eq!(got["more"], true);
    let stopped = core.run(
        "gws_drive_list",
        &json!({"view": "my", "folder": "big", "skip": 120}),
    );
    assert_eq!(refusal(&stopped), "no_access");
}

#[test]
fn a_401_on_the_first_page_asks_for_sign_in_and_a_403_says_it_cant_read() {
    let google = FakeGoogle::start(Seed::default().with_drive());
    let core = signed_in(&google, "first");
    sync(&core);
    google.drive_list_fails_after(0, 401);
    let answer = core.run("gws_drive_list", &json!({"view": "my"}));
    assert_eq!(
        answer["structuredContent"]["code"], "account_required",
        "{answer}"
    );
    google.drive_list_fails_after(0, 403);
    let answer = core.run("gws_drive_list", &json!({"view": "my"}));
    assert_eq!(refusal(&answer), "no_access");
}

#[test]
fn a_page_token_google_no_longer_takes_starts_the_list_over() {
    let google = big();
    let core = signed_in(&google, "stale");
    list(&core, big_args());
    google.drive_expire_pages();
    let answer = core.run(
        "gws_drive_list",
        &json!({"view": "my", "folder": "big", "skip": 100}),
    );
    assert_eq!(refusal(&answer), "stale", "{answer}");
    assert_whole_and_once(&walk(&core, big_args()));
}

#[test]
fn a_folder_that_is_gone_says_so() {
    let google = FakeGoogle::start(Seed::default().with_drive());
    let core = signed_in(&google, "gonefolder");
    sync(&core);
    list(&core, json!({"view": "my", "folder": "drv-photos"}));
    google.remove_file("drv-photos");
    sync(&core);
    let answer = core.run(
        "gws_drive_list",
        &json!({"view": "my", "folder": "drv-photos"}),
    );
    assert_eq!(refusal(&answer), "not_found", "{answer}");
    let answer = core.run("gws_drive_file", &json!({"id": "drv-photos"}));
    assert_eq!(refusal(&answer), "not_found");
    let answer = core.run("gws_drive_list", &json!({"view": "my", "folder": "a'b"}));
    assert_eq!(refusal(&answer), "invalid_args");
    assert!(
        !google.log().iter().any(|s| s.target.contains("a%27b")),
        "an id that isn't Drive's is never sent"
    );
}

fn seeded() -> FakeGoogle {
    FakeGoogle::start(Seed::default().with_drive())
}

const BREAKOUTS: [&str; 5] = [
    "x' or trashed = true or name contains 'y",
    r"x\' or trashed = true or name contains \'y",
    r"x\\' or trashed = true or name contains \\'y",
    "' or name contains '",
    r"\",
];

#[test]
fn a_query_cant_break_out_of_drives_q() {
    let google = seeded();
    let core = signed_in(&google, "quotes");
    for query in BREAKOUTS {
        let answer = core.run("gws_search_drive", &json!({"query": query}));
        // a broken `q` would be a 400; one that broke out would find the trash
        assert_eq!(data(answer)["items"], json!([]), "{query}");
    }
    let q = google.log().last().unwrap().target.clone();
    assert!(
        q.contains("fullText+contains") || q.contains("fullText%20contains"),
        "{q}"
    );
    assert!(
        !google
            .log()
            .iter()
            .any(|s| s.target.contains("orderBy") && s.target.contains("fullText")),
        "Google won't sort a full text search"
    );
}

#[test]
fn a_name_with_quotes_and_backslashes_is_found_as_it_is() {
    let google = seeded();
    google.seed_file(FakeFile::new(
        "drv-odd",
        r"Ana's \ fake 'draft'.txt",
        "text/plain",
    ));
    let core = signed_in(&google, "oddname");
    for query in ["Ana's", r"s \ fake", "'draft'"] {
        let found = data(core.run("gws_search_drive", &json!({"query": query})));
        assert_eq!(ids(&found), ["drv-odd"], "{query}");
    }
}

#[test]
fn trash_shows_only_trashed_files_and_every_other_view_leaves_them_out() {
    let google = seeded();
    google.edit_file("drv-doc-tax", |f| f.trashed = true);
    google.seed_file(
        FakeFile::new("drv-in-clients", "Trashed in a folder.txt", "text/plain")
            .inside("drv-clients"),
    );
    google.edit_file("drv-in-clients", |f| f.trashed = true);
    let core = signed_in(&google, "trash");
    let trash = walk(&core, json!({"view": "trash"}));
    let mut got: Vec<String> = trash.iter().map(|r| text(r, "id")).collect();
    got.sort();
    assert_eq!(got, ["drv-doc-tax", "drv-in-clients", "drv-old-draft"]);
    assert!(trash.iter().all(|r| r["trashed"] == true));
    for args in [
        json!({"view": "my"}),
        json!({"view": "shared"}),
        json!({"view": "recent"}),
        json!({"view": "starred"}),
        json!({"view": "my", "folder": "drv-clients"}),
    ] {
        let rows = walk(&core, args.clone());
        assert!(rows.iter().all(|r| r["trashed"] == false), "{args}");
    }
    let found = data(core.run("gws_search_drive", &json!({"query": "draft"})));
    assert!(!ids(&found).contains(&"drv-old-draft".to_owned()));
}

#[test]
fn each_view_holds_what_it_should_in_its_order() {
    let google = seeded();
    let core = signed_in(&google, "views");
    let my = walk(&core, json!({"view": "my"}));
    let names: Vec<String> = my.iter().map(|r| text(r, "name")).collect();
    assert_eq!(names[..2], ["Clients", "Photos"], "folders first");
    assert_eq!(
        names[2..],
        [
            "Invoices",
            "Meeting notes",
            "Pitch",
            "Tax checklist",
            "To do.txt"
        ]
    );
    assert_eq!(
        ids(&list(&core, json!({"view": "starred"}))),
        ["drv-doc-tax"]
    );
    let shared = walk(&core, json!({"view": "shared"}));
    assert_eq!(text(&shared[0], "id"), "drv-shared-brief");
    assert_eq!(shared[0]["mine"], false);
    assert_eq!(shared[0]["owner"], "Ana Reyes");
    let recent = walk(&core, json!({"view": "recent"}));
    let times: Vec<String> = recent.iter().map(|r| text(r, "changed")).collect();
    assert!(times.is_sorted_by(|a, b| a >= b), "newest first: {times:?}");
    assert_eq!(
        text(&recent[0], "id"),
        "drv-doc-notes",
        "folders get no head start"
    );

    let row = my.iter().find(|r| r["id"] == "drv-text-todo").unwrap();
    assert_eq!(
        row,
        &json!({"id": "drv-text-todo", "name": "To do.txt", "kind": "file", "mime": "text/plain", "owner": "Owner Fake", "mine": true, "changed": row["changed"], "size": 32, "starred": false, "shared": false, "trashed": false})
    );
    let doc = my.iter().find(|r| r["id"] == "drv-doc-notes").unwrap();
    assert_eq!((&doc["kind"], &doc["size"]), (&json!("doc"), &Value::Null));
    assert_eq!(
        my.iter().find(|r| r["name"] == "Pitch").unwrap()["kind"],
        "slides"
    );
    assert_eq!(
        my.iter().find(|r| r["name"] == "Invoices").unwrap()["kind"],
        "sheet"
    );

    let bad = core.run("gws_drive_list", &json!({"view": "nothing"}));
    assert_eq!(refusal(&bad), "invalid_args");
    let bad = core.run(
        "gws_drive_list",
        &json!({"view": "trash", "folder": "drv-clients"}),
    );
    assert_eq!(refusal(&bad), "invalid_args");
}

#[test]
fn a_folders_path_runs_from_under_the_root_down_to_it() {
    let google = seeded();
    let core = signed_in(&google, "path");
    let inside = list(&core, json!({"view": "my", "folder": "drv-reyes"}));
    assert_eq!(
        inside["path"],
        json!([{"id": "drv-clients", "name": "Clients"}, {"id": "drv-reyes", "name": "Reyes Bakery"}])
    );
    let names: Vec<String> = inside["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| text(r, "name"))
        .collect();
    assert_eq!(
        names,
        ["Budget 2026.xlsx", "Roof quote.docx", "Signed contract.pdf"]
    );
    let kinds: Vec<String> = inside["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| text(r, "kind"))
        .collect();
    assert_eq!(kinds, ["spreadsheet", "word", "pdf"]);
    assert_eq!(list(&core, json!({"view": "my"}))["path"], json!([]));
}

#[test]
fn a_files_details_are_plain_and_say_who_has_it() {
    let google = seeded();
    let core = signed_in(&google, "detail");
    let f = data(core.run("gws_drive_file", &json!({"id": "drv-doc-notes"})));
    assert_eq!(f["name"], "Meeting notes");
    assert_eq!(f["path"], json!([]));
    assert_eq!(f["description"], "Notes from Monday", "no markup");
    assert_eq!(f["modified_by"], "Owner Fake");
    assert_eq!(f["can_download"], true);
    assert!(f["created"].as_str().unwrap().starts_with("2026-11-0"));
    assert_eq!(
        f["people"],
        json!([{"name": "Owner Fake", "email": FAKE_EMAIL, "role": "owner"}, {"name": "Ana Reyes", "email": "ana@example.com", "role": "writer"}])
    );
    let deep = data(core.run("gws_drive_file", &json!({"id": "drv-quote"})));
    assert_eq!(
        deep["path"],
        json!([{"id": "drv-clients", "name": "Clients"}, {"id": "drv-reyes", "name": "Reyes Bakery"}])
    );
    let bad = core.run("gws_drive_file", &json!({"id": "../about"}));
    assert_eq!(refusal(&bad), "invalid_args");
}

#[test]
fn storage_comes_in_bytes() {
    let google = seeded();
    let core = signed_in(&google, "about");
    let about = data(core.run("gws_drive_about", &json!({})));
    assert_eq!(about["limit"], 16_106_127_360_i64);
    assert_eq!(about["user"], "Owner Fake");
    assert!(about["used"].as_i64().unwrap() > 0, "{about}");
}

fn preview(core: &Core, id: &str) -> Value {
    data(core.run("gws_drive_preview", &json!({"id": id})))
}

fn downloads(google: &FakeGoogle) -> Vec<String> {
    google
        .log()
        .into_iter()
        .filter(|s| s.target.contains("alt=media") || s.target.contains("/export"))
        .map(|s| s.target)
        .collect()
}

#[test]
fn text_comes_from_a_plain_export_and_never_as_html() {
    let google = seeded();
    let core = signed_in(&google, "export");
    let doc = preview(&core, "drv-doc-notes");
    assert_eq!(
        doc,
        json!({"kind": "text", "text": "Fake meeting notes.\nRoof first, then the fence."})
    );
    assert_eq!(
        preview(&core, "drv-sheet-invoices")["text"],
        "number,total\n1042,1200\n"
    );
    assert_eq!(preview(&core, "drv-slides-pitch")["kind"], "text");
    assert_eq!(
        preview(&core, "drv-text-todo")["text"],
        "Call the roofer\nPay the invoice\n"
    );
    let asked = downloads(&google);
    assert_eq!(asked.len(), 4);
    assert!(
        asked.iter().all(|t| !t.contains("html")),
        "HTML is never asked for: {asked:?}"
    );
}

#[test]
fn an_image_comes_as_its_bytes_never_a_link() {
    let google = seeded();
    let core = signed_in(&google, "image");
    let image = preview(&core, "drv-storefront");
    let expected = base64::engine::general_purpose::STANDARD.encode(TINY_PNG);
    assert_eq!(
        image,
        json!({"kind": "image", "data_url": format!("data:image/png;base64,{expected}")})
    );
    let raw = core.run("gws_drive_preview", &json!({"id": "drv-storefront"}));
    assert_eq!(
        raw.to_string().matches(&expected).count(),
        1,
        "an image goes once, not in the text as well"
    );
    for answer in [
        preview(&core, "drv-doc-notes"),
        preview(&core, "drv-text-todo"),
        image,
    ] {
        let said = answer.to_string();
        assert!(!said.contains("http") && !said.contains("<html"), "{said}");
    }
}

/// What the Bot refuses past 1 MiB, whole, so an image has to fit in that.
const RESULT_MAX: usize = 1024 * 1024;

#[test]
fn an_image_that_fits_a_result_comes_and_a_bigger_one_says_why_with_no_download() {
    let google = seeded();
    let png = |n: usize| {
        let mut bytes = TINY_PNG.to_vec();
        bytes.resize(n, 0);
        bytes
    };
    google.seed_file(FakeFile::new("drv-fits", "Fits.png", "image/png").holding(&png(700_000)));
    google.seed_file(FakeFile::new("drv-big", "Big.png", "image/png").holding(&png(2_000_000)));
    google.seed_file(
        FakeFile::new("drv-huge", "Huge.png", "image/png").holding(&png(8 * 1024 * 1024 + 1)),
    );
    let core = signed_in(&google, "imagecap");
    let fits = core.run("gws_drive_preview", &json!({"id": "drv-fits"}));
    assert_eq!(fits["structuredContent"]["kind"], "image");
    assert!(fits.to_string().len() <= RESULT_MAX);
    for id in ["drv-big", "drv-huge"] {
        let answer = preview(&core, id);
        assert_eq!(answer["kind"], "none", "{id}");
        assert!(answer["why"].as_str().unwrap().contains("too big"));
    }
    assert_eq!(
        downloads(&google).len(),
        1,
        "a file over the cap isn't fetched"
    );
}

/// What a file says of its own size is only what Google was told.
#[test]
fn a_size_that_lies_doesnt_get_past_the_cap() {
    let google = seeded();
    let mut bytes = TINY_PNG.to_vec();
    bytes.resize(3_000_000, 0);
    let mut liar = FakeFile::new("drv-liar", "Small.png", "image/png").holding(&bytes);
    liar.claimed_size = Some(100);
    google.seed_file(liar);
    let core = signed_in(&google, "liar");
    assert_eq!(preview(&core, "drv-liar")["kind"], "none");
}

#[test]
fn an_image_that_isnt_one_is_not_shown() {
    let google = seeded();
    google.seed_file(
        FakeFile::new("drv-fake-png", "Fake.png", "image/png")
            .holding(b"<html><script>alert(1)</script></html>"),
    );
    let core = signed_in(&google, "notimage");
    let answer = preview(&core, "drv-fake-png");
    assert_eq!(answer["kind"], "none");
    assert!(!answer.to_string().contains("script"));
}

#[test]
fn text_over_the_cap_answers_none_and_an_export_is_cut_to_it() {
    let google = seeded();
    let long = "é".repeat(40 * 1024);
    google.seed_file(FakeFile::new("drv-long", "Long.txt", "text/plain").holding(long.as_bytes()));
    google
        .seed_file(FakeFile::new("drv-long-doc", "Long doc", GOOGLE_DOC).holding(long.as_bytes()));
    let mut exact = FakeFile::new("drv-exact", "Exact.txt", "text/plain");
    exact.content = vec![b'x'; 64 * 1024];
    google.seed_file(exact);
    let core = signed_in(&google, "textcap");

    let over = preview(&core, "drv-long");
    assert_eq!(over["kind"], "none");
    assert!(over["why"].as_str().unwrap().contains("too long"));
    assert!(downloads(&google).is_empty(), "not fetched");

    let doc = preview(&core, "drv-long-doc");
    let text = doc["text"].as_str().unwrap();
    assert!(text.len() <= 64 * 1024 && text.ends_with("…[truncated]") && text.starts_with("éé"));
    assert_eq!(
        preview(&core, "drv-exact")["text"].as_str().unwrap().len(),
        64 * 1024
    );

    let read = data(core.run("gws_read_doc_text", &json!({"id": "drv-long"})));
    assert!(
        read["text"].as_str().unwrap().len() <= 64 * 1024,
        "a read is cut, not refused"
    );
}

#[test]
fn a_type_outside_the_list_has_no_preview_and_nothing_is_fetched() {
    let google = seeded();
    google.seed_file(
        FakeFile::new("drv-html", "Page.html", "text/html").holding(b"<script>alert(1)</script>"),
    );
    google.seed_file(FakeFile::new("drv-svg", "Logo.svg", "image/svg+xml").holding(b"<svg/>"));
    google.seed_file(FakeFile::new("drv-bmp", "Scan.bmp", "image/bmp").holding(b"BM"));
    let core = signed_in(&google, "types");
    for id in [
        "drv-html",
        "drv-svg",
        "drv-bmp",
        "drv-quote",
        "drv-contract",
        "drv-clients",
    ] {
        let answer = preview(&core, id);
        assert_eq!(answer["kind"], "none", "{id}");
        assert!(answer["why"].as_str().is_some_and(|w| !w.is_empty()));
    }
    assert!(downloads(&google).is_empty(), "{:?}", downloads(&google));
    let gone = core.run("gws_drive_preview", &json!({"id": "drv-nothing"}));
    assert_eq!(refusal(&gone), "not_found");
}

#[test]
fn a_file_that_cant_be_downloaded_says_so() {
    let google = seeded();
    let mut locked = FakeFile::new("drv-locked", "Locked.txt", "text/plain").holding(b"words");
    locked.can_download = false;
    google.seed_file(locked);
    let mut huge = FakeFile::new("drv-huge-doc", "Huge doc", GOOGLE_SHEET);
    huge.content = vec![b'x'; 10 * 1024 * 1024 + 1];
    google.seed_file(huge);
    let core = signed_in(&google, "locked");
    assert_eq!(preview(&core, "drv-locked")["kind"], "none");
    let big = preview(&core, "drv-huge-doc");
    assert!(big["why"].as_str().unwrap().contains("too big"), "{big}");
}

#[test]
fn what_the_model_reads_carries_the_warning() {
    let google = seeded();
    let core = signed_in(&google, "warning");
    assert!(UNTRUSTED_DRIVE.contains("file") && !UNTRUSTED_DRIVE.contains("mail"));
    let found = data(core.run("gws_search_drive", &json!({"query": "meeting"})));
    assert_eq!(found["untrusted"], UNTRUSTED_DRIVE);
    assert_eq!(ids(&found), ["drv-doc-notes"]);
    let read = data(core.run("gws_read_doc_text", &json!({"id": "drv-doc-notes"})));
    assert_eq!(
        read,
        json!({"id": "drv-doc-notes", "name": "Meeting notes", "text": "Fake meeting notes.\nRoof first, then the fence.", "untrusted": UNTRUSTED_DRIVE})
    );
    let text = data(core.run("gws_read_doc_text", &json!({"id": "drv-text-todo"})));
    assert_eq!(text["untrusted"], UNTRUSTED_DRIVE);
}

#[test]
fn only_docs_and_text_files_can_be_read_by_the_model() {
    let google = seeded();
    let core = signed_in(&google, "readonly");
    for id in [
        "drv-sheet-invoices",
        "drv-slides-pitch",
        "drv-quote",
        "drv-contract",
        "drv-storefront",
        "drv-clients",
    ] {
        let answer = core.run("gws_read_doc_text", &json!({"id": id}));
        assert_eq!(refusal(&answer), "unsupported", "{id}");
    }
    assert!(downloads(&google).is_empty());
    let none = core.run("gws_read_doc_text", &json!({}));
    assert_eq!(none["isError"], true);
    let gone = core.run("gws_read_doc_text", &json!({"id": "drv-nothing"}));
    assert_eq!(refusal(&gone), "not_found");
}

/// Mail, Calendar and Drive on one fake account.
fn whole_account() -> FakeGoogle {
    FakeGoogle::start(Seed {
        calendars: vec![FakeCalendar::new(FAKE_EMAIL, "Owner", true, NY)],
        ..Seed::demo(4).with_drive()
    })
}

fn state(core: &Core, tool: &str) -> Value {
    data(core.run(tool, &json!({})))
}

#[test]
fn the_changes_sync_updates_and_removes_rows_and_moves_them_between_views() {
    let google = seeded();
    let core = signed_in(&google, "changes");
    sync(&core);
    let my = |core: &Core| walk(core, json!({"view": "my"}));
    let inside = |core: &Core| ids(&list(core, json!({"view": "my", "folder": "drv-reyes"})));
    assert!(my(&core).iter().any(|r| r["id"] == "drv-text-todo"));
    inside(&core);
    walk(&core, json!({"view": "trash"}));
    let seq = state(&core, "gws_drive_changes")["seq"].as_i64().unwrap();
    let asked = listings(&google).len();

    google.edit_file("drv-doc-notes", |f| f.name = "Agenda".to_owned());
    google.edit_file("drv-sheet-invoices", |f| f.starred = true);
    google.edit_file("drv-slides-pitch", |f| f.trashed = true);
    google.edit_file("drv-budget", |f| {
        f.parents = vec!["0AFakeDriveRootPk9".to_owned()]
    });
    google.remove_file("drv-text-todo");
    google.add_file(FakeFile::new("drv-added", "Brand new.txt", "text/plain").holding(b"new"));
    sync(&core);

    let after = state(&core, "gws_drive_changes");
    assert_eq!(after["status"], "ok");
    assert!(
        after["seq"].as_i64().unwrap() > seq,
        "the screen hears of it"
    );
    let rows = my(&core);
    let names: Vec<String> = rows.iter().map(|r| text(r, "name")).collect();
    assert_eq!(
        names,
        [
            "Clients",
            "Photos",
            "Agenda",
            "Brand new.txt",
            "Budget 2026.xlsx",
            "Invoices",
            "Tax checklist"
        ],
        "renamed in place, moved in, added, removed, trashed"
    );
    assert_eq!(rows[5]["starred"], true);
    assert!(
        !inside(&core).contains(&"drv-budget".to_owned()),
        "moved out"
    );
    assert!(
        ids(&list(&core, json!({"view": "starred"}))).contains(&"drv-sheet-invoices".to_owned())
    );
    assert!(ids(&list(&core, json!({"view": "trash"}))).contains(&"drv-slides-pitch".to_owned()));
    assert!(!ids(&list(&core, json!({"view": "recent"}))).contains(&"drv-slides-pitch".to_owned()));
    assert_eq!(
        listings(&google).len(),
        asked,
        "all of that came from the changes feed"
    );

    // and nothing changed is no news
    let seq = state(&core, "gws_drive_changes")["seq"].clone();
    sync(&core);
    assert_eq!(state(&core, "gws_drive_changes")["seq"], seq);
}

#[test]
fn a_changes_failure_leaves_mail_and_calendar_syncing() {
    let google = whole_account();
    let core = signed_in(&google, "isolated");
    sync(&core);
    assert_eq!(state(&core, "gws_drive_changes")["status"], "ok");
    let (mail, calendar) = (
        state(&core, "gws_mail_changes")["seq"].as_i64().unwrap(),
        state(&core, "gws_cal_changes")["seq"].as_i64().unwrap(),
    );

    for status in [500, 403, 401] {
        google.drive_changes_fail_after(0, status);
        google.deliver(gws_fake_google::Mail {
            id: format!("fff000000000{status}"),
            thread: format!("eee000000000{status}"),
            labels: vec!["INBOX".to_owned()],
            from: "Ana Reyes <ana@example.com>".to_owned(),
            to: FAKE_EMAIL.to_owned(),
            subject: "After the failure".to_owned(),
            at: 1_793_955_700_000,
            text: "Fake".to_owned(),
            html: None,
            attachments: Vec::new(),
            message_id: String::new(),
        });
        google.add_event(
            FAKE_EMAIL,
            json!({"id": format!("evt{status}"), "summary": "After the failure", "start": {"dateTime": "2026-11-20T09:00:00-05:00", "timeZone": NY}, "end": {"dateTime": "2026-11-20T10:00:00-05:00", "timeZone": NY}}),
        );
        sync(&core);
        let mail_now = state(&core, "gws_mail_changes");
        assert_eq!(mail_now["status"], "ok", "{status}");
        assert!(
            mail_now["seq"].as_i64().unwrap() > mail,
            "{status}: mail came in"
        );
        let cal_now = state(&core, "gws_cal_changes");
        assert_eq!(cal_now["status"], "ok", "{status}");
        assert!(
            cal_now["seq"].as_i64().unwrap() > calendar,
            "{status}: events came in"
        );
        // a 401 sets no status: it stands as the 403 left it
        assert_eq!(
            state(&core, "gws_drive_changes")["status"],
            "failed",
            "{status}"
        );
    }

    // and it picks up from where it was
    google.drive_works();
    google.edit_file("drv-doc-notes", |f| f.name = "Agenda".to_owned());
    walk(&core, json!({"view": "my"}));
    sync(&core);
    assert_eq!(state(&core, "gws_drive_changes")["status"], "ok");
    let rows = walk(&core, json!({"view": "my"}));
    assert!(rows.iter().any(|r| r["name"] == "Agenda"));
}

#[test]
fn a_changes_page_that_fails_keeps_the_ones_before_it_and_asks_again() {
    let google = seeded();
    let core = signed_in(&google, "feedpages");
    sync(&core);
    walk(&core, json!({"view": "my"}));
    google.edit_file("drv-doc-notes", |f| f.name = "First".to_owned());
    google.edit_file("drv-doc-tax", |f| f.name = "Second".to_owned());
    google.edit_file("drv-text-todo", |f| f.name = "Third".to_owned());
    google.drive_changes_page(1);
    google.drive_changes_fail_after(1, 500);
    let seq = state(&core, "gws_drive_changes")["seq"].as_i64().unwrap();
    sync(&core);
    assert_eq!(state(&core, "gws_drive_changes")["status"], "failed");
    assert!(
        state(&core, "gws_drive_changes")["seq"].as_i64().unwrap() > seq,
        "what came in shows"
    );
    let names = |core: &Core| -> Vec<String> {
        walk(core, json!({"view": "my"}))
            .iter()
            .map(|r| text(r, "name"))
            .collect()
    };
    let got = names(&core);
    assert!(
        got.contains(&"First".to_owned()) && !got.contains(&"Second".to_owned()),
        "{got:?}"
    );

    google.drive_works();
    sync(&core);
    let got = names(&core);
    assert!(
        ["First", "Second", "Third"]
            .iter()
            .all(|n| got.contains(&(*n).to_owned())),
        "{got:?}"
    );
    assert_eq!(state(&core, "gws_drive_changes")["status"], "ok");
}

#[test]
fn a_sign_in_without_drive_leaves_mail_and_calendar_alone() {
    let google = whole_account();
    google.only_grants("https://www.googleapis.com/auth/gmail.modify https://www.googleapis.com/auth/calendar.events");
    let core = signed_in(&google, "nodrive");
    sync(&core);
    assert_eq!(state(&core, "gws_mail_changes")["status"], "ok");
    assert_eq!(state(&core, "gws_cal_changes")["status"], "ok");
    assert_eq!(state(&core, "gws_drive_changes")["status"], "needs_access");
    let answer = core.run("gws_drive_list", &json!({"view": "my"}));
    assert_eq!(
        answer["structuredContent"]["code"], "needs_access",
        "{answer}"
    );
    assert!(
        answer["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Sign in again")
    );
}

#[test]
fn signing_out_clears_drive_and_the_token_with_it() {
    let google = seeded();
    let core = signed_in(&google, "signout");
    sync(&core);
    walk(&core, json!({"view": "my"}));
    core.account(
        "buwiz.account.clear",
        &json!({"account": "google", "label": "default"}),
    );
    core.account("buwiz.account.set", &json!({"account": "google", "label": "default", "provider": "google", "subject": "other@example.com", "scopes": [], "access_token": "fake-access-2", "token_type": "Bearer", "expires_at": 4_000_000_000u64}));
    let store = core.store().unwrap();
    let store = store.as_ref().unwrap();
    assert_eq!(store.view_total(FAKE_EMAIL, "my").unwrap(), 0);
    assert_eq!(store.drive_file(FAKE_EMAIL, "drv-doc-notes").unwrap(), None);
    assert_eq!(
        store
            .state(FAKE_EMAIL, gws_store::drive::CHANGES_TOKEN)
            .unwrap(),
        None
    );
}

#[test]
fn a_lost_connection_shows_what_was_stored_and_says_so() {
    let google = seeded();
    let core = signed_in(&google, "offline");
    sync(&core);
    let shown = walk(&core, json!({"view": "my"}));
    drop(core);
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let core = signed_in_at(&format!("http://127.0.0.1:{port}"), "offline", false);
    assert!(gws::mail::sync(&core).is_err());
    assert_eq!(state(&core, "gws_drive_changes")["status"], "offline");
    let page = list(&core, json!({"view": "my"}));
    assert_eq!(page["saved"], true);
    assert_eq!(page["files"].as_array().unwrap().len(), shown.len());
    let never = list(&core, json!({"view": "my", "folder": "drv-photos"}));
    assert_eq!(
        (
            never["files"].clone(),
            never["more"].clone(),
            never["saved"].clone()
        ),
        (json!([]), json!(false), json!(true))
    );
    let found = data(core.run("gws_search_drive", &json!({"query": "meeting"})));
    assert_eq!(
        ids(&found),
        ["drv-doc-notes"],
        "a search falls back on stored names"
    );
    let row = data(core.run("gws_drive_file", &json!({"id": "drv-doc-notes"})));
    assert_eq!(row["name"], "Meeting notes");
    let bad = core.run("gws_drive_preview", &json!({"id": "drv-doc-notes"}));
    assert_eq!(bad["isError"], true);
}

fn open_of(result: &Value) -> &Value {
    &result["_meta"]["buwiz/open"]
}

#[test]
fn a_chats_search_offers_a_card_and_the_ticket_reads_it_back() {
    let google = seeded();
    for n in 0..7 {
        google.seed_file(FakeFile::new(
            &format!("drv-many-{n}"),
            &format!("Many {n}"),
            GOOGLE_DOC,
        ));
    }
    let core = signed_in(&google, "card");
    let plain = core.run("gws_search_drive", &json!({"query": "many"}));
    assert_eq!(
        plain.get("_meta"),
        None,
        "a screen's own search has no card"
    );
    let chat = core.run_for_chat("gws_search_drive", &json!({"query": "many"}));
    let ticket = open_of(&chat)["props"]["search"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(open_of(&chat)["title"], "Drive");
    assert!(
        !chat["_meta"].to_string().contains("Many"),
        "no names in _meta"
    );

    let card = data(core.run("gws_drive_card", &json!({"ticket": ticket})));
    assert_eq!(card["rows"].as_array().unwrap().len(), 5, "up to five");
    assert_eq!(card["more"], true);
    assert_eq!(card["saved"], false);

    let full = list(&core, json!({"search": ticket}));
    assert_eq!(full["files"].as_array().unwrap().len(), 7);
    assert_eq!(full["more"], false);
    assert_eq!(
        list(&core, json!({"search": ticket, "skip": 7}))["files"],
        json!([])
    );

    for (tool, args) in [
        ("gws_drive_card", json!({"ticket": "tkt_nothing"})),
        ("gws_drive_list", json!({"search": "tkt_nothing"})),
    ] {
        assert_eq!(refusal(&core.run(tool, &args)), "not_found", "{tool}");
    }
}

#[test]
fn a_mail_ticket_opens_no_drive_search_and_a_drive_one_no_mail_search() {
    let google = FakeGoogle::start(Seed::demo(4).with_drive());
    let core = signed_in(&google, "kinds");
    let mail = core.run_for_chat("gws_search_mail", &json!({"query": "lunch"}));
    let ticket = open_of(&mail)["props"]["search"].as_str().unwrap();
    assert_eq!(
        refusal(&core.run("gws_drive_card", &json!({"ticket": ticket}))),
        "not_found"
    );
    let drive = core.run_for_chat("gws_search_drive", &json!({"query": "meeting"}));
    let ticket = open_of(&drive)["props"]["search"].as_str().unwrap();
    let card = core.run("gws_mail_card", &json!({"ticket": ticket}));
    assert_eq!(card["structuredContent"]["code"], "not_found", "{card}");
}

fn resolved(core: &Core, args: Value) -> Value {
    core.run("gws_resolve", &args)["structuredContent"].clone()
}

#[test]
fn file_offers_stored_names_as_tickets_and_opens_drive_on_the_search() {
    let google = seeded();
    let core = signed_in(&google, "file");
    sync(&core);
    let n = google.log().len();
    for kind in ["scope", "command"] {
        let got = resolved(
            &core,
            json!({"kind": kind, "name": "file", "value": "meet", "complete": true}),
        );
        assert_eq!(got["result"], "choices");
        let choices = got["choices"].as_array().unwrap();
        assert_eq!(choices.len(), 1, "{got}");
        assert_eq!(choices[0]["label"], "Drive file", "a name is no label");
        assert_eq!(choices[0]["detail"], "Meeting notes");
        let value = choices[0]["value"].as_str().unwrap();
        assert!(
            !value.to_lowercase().contains("meeting"),
            "a ticket, never the name"
        );
    }
    assert_eq!(google.log().len(), n, "typing asks no Google");

    let typed = resolved(
        &core,
        json!({"kind": "scope", "name": "file", "value": "meet", "complete": true}),
    );
    let chosen = typed["choices"][0]["value"].as_str().unwrap();
    let open = resolved(
        &core,
        json!({"kind": "scope", "name": "file", "value": chosen, "complete": false}),
    );
    assert_eq!(
        (
            &open["result"],
            &open["app"],
            &open["placement"],
            &open["title"]
        ),
        (
            &json!("open"),
            &json!("drive"),
            &json!("full"),
            &json!("Drive")
        )
    );
    assert_eq!(open["props"]["search"], chosen);
    assert_eq!(
        ids(&list(&core, json!({"search": chosen}))),
        ["drv-doc-notes"]
    );

    let said = resolved(
        &core,
        json!({"kind": "command", "name": "file", "value": "Meeting notes"}),
    );
    assert_eq!(said["title"], "Drive", "a title reaches the model");
    assert!(!said.to_string().contains("Meeting"), "{said}");
    let ticket = said["props"]["search"].as_str().unwrap();
    assert_eq!(
        ids(&list(&core, json!({"search": ticket}))),
        ["drv-doc-notes"]
    );

    let bare = resolved(
        &core,
        json!({"kind": "command", "name": "file", "value": ""}),
    );
    assert_eq!(bare["props"], json!({"view": "my"}));
    let none = resolved(
        &core,
        json!({"kind": "scope", "name": "file", "value": "", "complete": true}),
    );
    assert_eq!(none["choices"], json!([]));
    let trashed = resolved(
        &core,
        json!({"kind": "scope", "name": "file", "value": "old draft", "complete": true}),
    );
    assert_eq!(trashed["choices"], json!([]), "the trash isn't offered");
}

#[test]
fn drive_opens_on_my_drive_and_a_tickets_title_is_drives() {
    let google = seeded();
    let core = std::sync::Arc::new(signed_in(&google, "title"));
    let open = resolved(
        &core,
        json!({"kind": "command", "name": "drive", "value": ""}),
    );
    assert_eq!(
        (&open["app"], &open["title"]),
        (&json!("drive"), &json!("Drive"))
    );
    assert_eq!(open["props"], json!({"view": "my"}));
    let gws = resolved(
        &core,
        json!({"kind": "command", "name": "gws", "value": "drive"}),
    );
    assert_eq!(gws["props"], json!({"view": "my"}));

    let chat = core.run_for_chat("gws_search_drive", &json!({"query": "meeting"}));
    let ticket = open_of(&chat)["props"]["search"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut plugin = gws::Gws { core: core.clone() };
    let title = buwiz_plugin::Plugin::ticket_title(&mut plugin, &ticket);
    assert_eq!(title.as_deref(), Some("Drive"));
}

#[test]
fn nothing_here_writes_to_drive_and_the_token_goes_only_to_it() {
    let google = whole_account();
    let core = signed_in(&google, "readonly-all");
    sync(&core);
    let mut said = String::new();
    for (tool, args) in [
        ("gws_drive_list", json!({"view": "my"})),
        (
            "gws_drive_list",
            json!({"view": "my", "folder": "drv-reyes"}),
        ),
        ("gws_drive_about", json!({})),
        ("gws_drive_file", json!({"id": "drv-doc-notes"})),
        ("gws_drive_preview", json!({"id": "drv-doc-notes"})),
        ("gws_drive_preview", json!({"id": "drv-storefront"})),
        ("gws_search_drive", json!({"query": "meeting"})),
        ("gws_read_doc_text", json!({"id": "drv-doc-notes"})),
    ] {
        said.push_str(&core.run_for_chat(tool, &args).to_string());
    }
    google.edit_file("drv-doc-notes", |f| f.name = "Agenda".to_owned());
    sync(&core);
    let drive: Vec<_> = google
        .log()
        .into_iter()
        .filter(|s| s.target.starts_with("/drive/"))
        .collect();
    assert!(drive.len() > 8);
    assert!(drive.iter().all(|s| s.method == "GET"), "{drive:?}");
    assert!(
        drive
            .iter()
            .all(|s| s.authorization == format!("Bearer {TOKEN}"))
    );
    assert!(!said.contains(TOKEN), "never in a result");
}
