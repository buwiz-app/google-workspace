//! Opening files and editing back, against the stub Google: where a hostile
//! name lands, what a conflict and a Google original write (nothing), and when
//! a file is gone. Time moves by the Core's clock, so nothing sleeps. This
//! proves the plugin against the stub, which is written from what's known of
//! Google's API, not against Google.

use std::fs;
use std::path::{Path, PathBuf};

use gws::Core;
use gws::apis::Apis;
use gws::files::{LIFE_MS, SETTLE_MS};
use gws_fake_google::{
    DRIVE_ROOT, FAKE_EMAIL, FakeFile, FakeGoogle, GOOGLE_DOC, GOOGLE_SHEET, GOOGLE_SLIDES, Mail,
    Seed,
};
use serde_json::{Value, json};

const TOKEN: &str = "fake-access-1";
const DOCX: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
const NOON: i64 = 1_800_000_000_000;

fn push(who: &str) -> Value {
    json!({"account": "google", "label": "default", "provider": "google", "subject": who, "scopes": ["https://www.googleapis.com/auth/drive"], "access_token": TOKEN, "token_type": "Bearer", "expires_at": 4_000_000_000u64})
}

struct Home {
    core: Core,
    dir: PathBuf,
}

impl Home {
    fn new(base: &str, name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("gws-files-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let core = Core::new(&Apis::at(base), Ok(gws_store::Paths::in_dir(&dir)));
        core.unlock("seed");
        core.account("buwiz.account.set", &push(FAKE_EMAIL));
        core.clock.set(NOON);
        Self { core, dir }
    }

    fn open_dir(&self) -> PathBuf {
        gws::files::open_root(&self.core).unwrap()
    }

    fn folder(&self, id: &str) -> PathBuf {
        self.open_dir().join(id)
    }

    /// Opens `id`, and says where it landed.
    fn open(&self, id: &str) -> PathBuf {
        let answer = self.core.run("gws_file_open", &json!({"id": id}));
        path_in(&answer)
    }

    fn upload(&self, id: &str, mode: &str) -> Value {
        self.core
            .run("gws_file_upload", &json!({"id": id, "mode": mode}))
    }

    fn edits(&self) -> Vec<Value> {
        data(self.core.run("gws_file_edits", &json!({})))["files"]
            .as_array()
            .unwrap()
            .clone()
    }

    fn later(&self, ms: i64) {
        self.core.clock.set(NOON + ms);
    }

    fn sign_out(&self) {
        self.core.account(
            "buwiz.account.clear",
            &json!({"account": "google", "label": "default"}),
        );
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
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

fn path_in(answer: &Value) -> PathBuf {
    let path = answer["_meta"]["buwiz/document"]["path"]
        .as_str()
        .unwrap_or_else(|| panic!("no document: {answer}"));
    PathBuf::from(path)
}

fn a_docx(id: &str) -> FakeFile {
    FakeFile::new(id, "Roof quote.docx", DOCX).holding(b"PK as it came")
}

/// The real path of a place, as an answer gives it: the temp folder's a link.
fn real(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap()
}

fn drive_writes(google: &FakeGoogle) -> Vec<String> {
    google
        .writes()
        .into_iter()
        .filter(|s| s.target.contains("/drive/"))
        .map(|s| format!("{} {}", s.method, s.target))
        .collect()
}

/// Everything under `dir`, files and folders.
fn every_path(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if fs::symlink_metadata(&path).unwrap().is_dir() {
            found.extend(every_path(&path));
        }
        found.push(path);
    }
    found
}

#[test]
fn a_file_lands_in_its_folder_and_only_meta_says_where() {
    let google = FakeGoogle::start(Seed::default());
    google.seed_file(a_docx("drv-a"));
    let home = Home::new(&google.base(), "lands");
    let answer = home.core.run("gws_file_open", &json!({"id": "drv-a"}));
    let path = path_in(&answer);
    assert_eq!(
        path,
        real(&home.folder("drv-a")).join("Roof quote.docx"),
        "the real path, so the Bot finds no link on the way"
    );
    assert_eq!(fs::read(&path).unwrap(), b"PK as it came");
    assert_eq!(
        answer["_meta"]["buwiz/document"],
        json!({"path": path, "name": "Roof quote.docx", "mime": DOCX})
    );
    assert_eq!(
        answer["structuredContent"],
        json!({"opened": true, "name": "Roof quote.docx", "kind": "word"})
    );
    assert!(
        !answer["content"].to_string().contains("gws-files"),
        "the path is the Bot's alone: {answer}"
    );
    use std::os::unix::fs::PermissionsExt as _;
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(home.folder("drv-a"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    let names: Vec<_> = fs::read_dir(home.folder("drv-a"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert_eq!(
        names,
        ["Roof quote.docx"],
        "nothing half done left beside it"
    );
    assert!(!answer.to_string().contains(TOKEN));
}

#[test]
fn a_big_real_file_comes_whole_past_any_default_limit() {
    let google = FakeGoogle::start(Seed::default());
    let bytes = vec![7u8; 12 * 1024 * 1024];
    google.seed_file(FakeFile::new("drv-big", "Big.docx", DOCX).holding(&bytes));
    let home = Home::new(&google.base(), "bigreal");
    assert!(fs::read(home.open("drv-big")).unwrap() == bytes);
}

#[test]
fn a_google_file_is_exported_as_its_office_kind() {
    let google = FakeGoogle::start(Seed::default());
    for (id, name, mime) in [
        ("drv-d", "Notes", GOOGLE_DOC),
        ("drv-s", "Invoices", GOOGLE_SHEET),
        ("drv-p", "Pitch", GOOGLE_SLIDES),
    ] {
        google.seed_file(FakeFile::new(id, name, mime).holding(b"PK exported"));
    }
    let home = Home::new(&google.base(), "export");
    for (id, name, mime, kind) in [
        ("drv-d", "Notes.docx", DOCX, "word"),
        ("drv-s", "Invoices.xlsx", XLSX, "spreadsheet"),
    ] {
        let answer = home.core.run("gws_file_open", &json!({"id": id}));
        assert_eq!(answer["_meta"]["buwiz/document"]["mime"], mime);
        assert_eq!(answer["_meta"]["buwiz/document"]["name"], name);
        assert_eq!(answer["structuredContent"]["kind"], kind);
        assert_eq!(fs::read(path_in(&answer)).unwrap(), b"PK exported");
        assert!(
            google
                .log()
                .iter()
                .any(|s| s.target.contains(&format!("/files/{id}/export"))),
            "{id}"
        );
    }
}

#[test]
fn a_hostile_name_never_gets_out_of_its_folder() {
    let google = FakeGoogle::start(Seed::default());
    let long = format!("{}.docx", "n".repeat(5000));
    let names = [
        "../../x",
        "../../../../../../tmp/gws-escaped",
        "/etc/passwd",
        "/",
        "..",
        ".",
        ".hidden",
        ".ssh/authorized_keys",
        "\\..\\..\\x",
        "nul\u{0}byte.docx",
        "bell\u{7}\r\nline.docx",
        "\u{202e}xcod.exe",
        long.as_str(),
    ];
    for (n, name) in names.iter().enumerate() {
        google.seed_file(FakeFile::new(&format!("drv-h{n}"), name, DOCX).holding(b"PK"));
    }
    let home = Home::new(&google.base(), "hostile");
    // a push no longer opens the store, and what's to compare is what's in it
    drop(home.core.store().unwrap());
    let before: Vec<PathBuf> = every_path(&home.dir)
        .into_iter()
        .filter(|p| !p.starts_with(home.open_dir()))
        .collect();
    for (n, name) in names.iter().enumerate() {
        let id = format!("drv-h{n}");
        let answer = home.core.run("gws_file_open", &json!({"id": id}));
        let path = path_in(&answer);
        assert_eq!(
            path.parent().unwrap(),
            real(&home.folder(&id)),
            "{name:?} landed at {path:?}"
        );
        let shown = path.file_name().unwrap().to_str().unwrap();
        assert_eq!(answer["_meta"]["buwiz/document"]["name"], shown);
        assert!(
            !shown.starts_with('.') && !shown.contains("..") && shown.len() <= 200,
            "{shown:?}"
        );
        assert!(!shown.chars().any(char::is_control), "{shown:?}");
    }
    let after: Vec<PathBuf> = every_path(&home.dir)
        .into_iter()
        .filter(|p| !p.starts_with(home.open_dir()))
        .collect();
    assert_eq!(after, before, "nothing landed anywhere else");
    assert!(!Path::new("/tmp/gws-escaped").exists());
}

#[test]
fn an_id_that_isnt_drives_never_becomes_a_folder_or_a_request() {
    let google = FakeGoogle::start(Seed::default());
    let home = Home::new(&google.base(), "badid");
    let long = "x".repeat(257);
    let asked = google.log().len();
    for id in [
        "",
        "..",
        ".",
        "../x",
        "a/b",
        "a\u{0}b",
        long.as_str(),
        "att-1-2",
    ] {
        for (tool, args) in [
            ("gws_file_open", json!({"id": id})),
            ("gws_file_upload", json!({"id": id, "mode": "update"})),
        ] {
            let answer = home.core.run(tool, &args);
            if id == "att-1-2" && tool == "gws_file_upload" {
                assert_eq!(refusal(&answer), "not_found");
            } else {
                assert_eq!(refusal(&answer), "invalid_args", "{id}: {answer}");
            }
        }
    }
    let discard = home.core.run("gws_file_discard", &json!({"id": "../x"}));
    assert_eq!(refusal(&discard), "invalid_args");
    assert_eq!(google.log().len(), asked, "no request");
    assert!(!home.open_dir().exists(), "no folder");
}

#[test]
fn an_export_past_10_mb_points_to_the_browser_and_leaves_nothing() {
    let google = FakeGoogle::start(Seed::default());
    let bytes = vec![1u8; 10 * 1024 * 1024 + 1];
    google.seed_file(FakeFile::new("drv-big-doc", "Huge", GOOGLE_DOC).holding(&bytes));
    let home = Home::new(&google.base(), "toobig");
    let answer = home
        .core
        .run("gws_file_open", &json!({"id": "drv-big-doc"}));
    assert_eq!(refusal(&answer), "too_big", "{answer}");
    assert_eq!(
        answer["structuredContent"]["link"],
        "https://docs.google.com/document/d/drv-big-doc/edit"
    );
    assert_eq!(answer.get("_meta"), None);
    assert!(!home.folder("drv-big-doc").exists());
}

/// Past 10 MB with no refusal from Google: this plugin's own stop.
#[test]
fn an_export_that_keeps_coming_stops_at_10_mb() {
    let fake = gws_fake_google::serve(|target| {
        if target.contains("/export") {
            (200, "x".repeat(10 * 1024 * 1024 + 1))
        } else {
            let meta =
                json!({"id": "drv-long", "name": "Long", "mimeType": GOOGLE_DOC, "version": "1"});
            (200, meta.to_string())
        }
    });
    let dir = std::env::temp_dir().join(format!("gws-files-cap-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let mut apis = Apis::at(&fake.base);
    apis.drive = fake.base.clone();
    let core = Core::new(&apis, Ok(gws_store::Paths::in_dir(&dir)));
    core.unlock("seed");
    core.accounts.set(&push(FAKE_EMAIL));
    let answer = core.run("gws_file_open", &json!({"id": "drv-long"}));
    assert_eq!(refusal(&answer), "too_big", "{answer}");
    assert!(!dir.join("open/drv-long").exists());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_real_file_over_the_cap_is_refused_in_plain_words_without_a_download() {
    let google = FakeGoogle::start(Seed::default());
    let mut film = FakeFile::new("drv-huge", "Scan.docx", DOCX).holding(b"PK fake");
    film.claimed_size = Some(2 * 1024 * 1024 * 1024);
    google.seed_file(film);
    let home = Home::new(&google.base(), "realcap");
    let answer = home.core.run("gws_file_open", &json!({"id": "drv-huge"}));
    assert_eq!(refusal(&answer), "too_big");
    let said = answer["structuredContent"]["message"].as_str().unwrap();
    assert!(said.contains("1 GB") && said.contains("browser"), "{said}");
    assert_eq!(
        answer["structuredContent"]["link"],
        "https://drive.google.com/file/d/drv-huge/view"
    );
    assert!(!google.log().iter().any(|s| s.target.contains("alt=media")));
}

#[test]
fn what_cant_be_read_or_viewed_is_refused_and_leaves_nothing() {
    let google = FakeGoogle::start(Seed::default());
    let mut locked = a_docx("drv-locked");
    locked.can_download = false;
    google.seed_file(locked);
    google.seed_file(FakeFile::new("drv-zip", "All.zip", "application/zip").holding(b"PK"));
    google.seed_file(FakeFile::folder("drv-folder", "Folder"));
    google.seed_file(FakeFile::new(
        "drv-form",
        "Survey",
        "application/vnd.google-apps.form",
    ));
    let home = Home::new(&google.base(), "refused");
    for (id, code) in [
        ("drv-missing", "not_found"),
        ("drv-locked", "no_access"),
        ("drv-zip", "unsupported"),
        ("drv-folder", "unsupported"),
        ("drv-form", "unsupported"),
    ] {
        let answer = home.core.run("gws_file_open", &json!({"id": id}));
        assert_eq!(refusal(&answer), code, "{id}: {answer}");
        assert_eq!(answer.get("_meta"), None, "{id}");
        assert!(!home.folder(id).exists(), "{id}");
    }
    assert_eq!(home.edits(), Vec::<Value>::new());
}

#[test]
fn signed_out_an_open_asks_for_sign_in() {
    let google = FakeGoogle::start(Seed::default());
    google.seed_file(a_docx("drv-a"));
    let home = Home::new(&google.base(), "nosign");
    home.sign_out();
    let answer = home.core.run("gws_file_open", &json!({"id": "drv-a"}));
    assert_eq!(answer["structuredContent"]["code"], "account_required");
    assert_eq!(answer.get("_meta"), None);
}

// --- Editing back ---------------------------------------------------------

#[test]
fn an_unedited_file_isnt_listed_and_an_edit_is_found_by_its_hash() {
    let google = FakeGoogle::start(Seed::default());
    google.seed_file(a_docx("drv-a"));
    let home = Home::new(&google.base(), "edits");
    let path = home.open("drv-a");
    assert_eq!(home.edits(), Vec::<Value>::new());
    // the same size: only what it holds says it's edited
    fs::write(&path, b"PK as it wen").unwrap();
    assert_eq!(fs::metadata(&path).unwrap().len(), 12);
    fs::write(&path, b"PK as it wen!").unwrap();
    assert_eq!(
        home.edits(),
        [
            json!({"id": "drv-a", "name": "Roof quote.docx", "state": "edited", "google": false, "attachment": false, "changed_on_drive": false, "working": false})
        ]
    );
    fs::write(&path, b"PK as it came").unwrap();
    assert_eq!(
        home.edits(),
        Vec::<Value>::new(),
        "put back, there's no edit"
    );
}

/// The clock where `path`'s stamps count as settled, or `at`, if that's later.
fn settle(home: &Home, path: &Path, at: i64) {
    use std::os::unix::fs::MetadataExt as _;
    let meta = fs::metadata(path).unwrap();
    let ms = |s: i64, ns: i64| s * 1000 + ns / 1_000_000;
    let latest = ms(meta.mtime(), meta.mtime_nsec()).max(ms(meta.ctime(), meta.ctime_nsec()));
    home.core.clock.set(at.max(latest + SETTLE_MS));
}

#[test]
fn a_poll_reads_a_settled_file_once() {
    let google = FakeGoogle::start(Seed::default());
    google.seed_file(a_docx("drv-a"));
    let home = Home::new(&google.base(), "pollonce");
    let path = home.open("drv-a");
    fs::write(&path, b"PK mine").unwrap();
    settle(&home, &path, 0);
    assert_eq!(home.edits().len(), 1);
    let runs = home.core.hashes.runs();
    for _ in 0..3 {
        assert_eq!(home.edits().len(), 1);
    }
    assert_eq!(home.core.hashes.runs(), runs, "read once, not on each poll");
    // and past its 24 hours, when the sweep each poll runs asks as well
    settle(&home, &path, NOON + LIFE_MS);
    assert_eq!(home.edits().len(), 1);
    assert_eq!(home.core.hashes.runs(), runs);
}

#[test]
fn a_save_that_keeps_the_size_and_puts_the_time_back_is_still_found() {
    let google = FakeGoogle::start(Seed::default());
    google.seed_file(a_docx("drv-a"));
    let home = Home::new(&google.base(), "polltime");
    let path = home.open("drv-a");
    fs::write(&path, b"PK as it wen!").unwrap();
    settle(&home, &path, 0);
    assert_eq!(home.edits().len(), 1);
    let was = fs::metadata(&path).unwrap();
    fs::write(&path, b"PK as it came").unwrap();
    let file = fs::File::options().write(true).open(&path).unwrap();
    file.set_modified(was.modified().unwrap()).unwrap();
    drop(file);
    let now = fs::metadata(&path).unwrap();
    assert_eq!(now.len(), was.len());
    assert_eq!(now.modified().unwrap(), was.modified().unwrap());
    assert_eq!(home.edits(), Vec::<Value>::new(), "put back, and seen");
}

/// A stamp from this very moment could be shared by the next save, so it's
/// never kept. The clock stands at the moment of the save, which no sleep could
/// promise.
#[test]
fn a_file_saved_this_moment_is_read_on_every_poll() {
    use std::os::unix::fs::MetadataExt as _;
    let google = FakeGoogle::start(Seed::default());
    google.seed_file(a_docx("drv-a"));
    let home = Home::new(&google.base(), "pollnow");
    let path = home.open("drv-a");
    fs::write(&path, b"PK mine").unwrap();
    let meta = fs::metadata(&path).unwrap();
    home.core.clock.set(meta.ctime() * 1000 + 1000);
    home.edits();
    let runs = home.core.hashes.runs();
    home.edits();
    assert_eq!(home.core.hashes.runs(), runs + 1);
}

#[test]
fn update_makes_a_new_revision_after_reading_drive_and_the_local_file_goes() {
    let google = FakeGoogle::start(Seed::default());
    google.seed_file(a_docx("drv-a").inside("drv-folder"));
    google.seed_file(FakeFile::folder("drv-folder", "Folder"));
    let home = Home::new(&google.base(), "update");
    let path = home.open("drv-a");
    fs::write(&path, b"PK edited here").unwrap();
    let was = google.drive_file("drv-a").unwrap();
    let answer = home.upload("drv-a", "update");
    assert_eq!(
        data(answer.clone()),
        json!({"uploaded": true, "mode": "update", "id": "drv-a"})
    );
    let now = google.drive_file("drv-a").unwrap();
    assert_eq!(now.content, b"PK edited here");
    assert_eq!(now.version, was.version + 1);
    assert_eq!(
        (now.name, now.mime, now.parents),
        (was.name, was.mime, was.parents)
    );
    assert!(!home.folder("drv-a").exists(), "the local file goes");
    assert_eq!(home.edits(), Vec::<Value>::new());

    let log = google.log();
    let write = log.iter().position(|s| s.method == "PATCH").unwrap();
    assert!(
        log[..write]
            .iter()
            .rev()
            .find(|s| s.method == "GET")
            .is_some_and(|s| s.target.contains("files/drv-a")
                && s.target.contains("md5Checksum")
                && s.target.contains("version")),
        "Drive is read just before the write"
    );
    assert!(
        log.iter()
            .filter(|s| s.method == "PUT")
            .all(|s| s.authorization.is_empty()),
        "an upload's address needs no token"
    );
    assert_eq!(
        refusal(&home.upload("drv-a", "update")),
        "not_found",
        "a second press has nothing to send"
    );
    assert_eq!(
        drive_writes(&google)
            .iter()
            .filter(|w| w.starts_with("PUT"))
            .count(),
        1
    );
}

#[test]
fn a_big_edit_goes_up_in_pieces_and_arrives_whole() {
    let google = FakeGoogle::start(Seed::default());
    google.seed_file(a_docx("drv-a"));
    let home = Home::new(&google.base(), "pieces");
    let path = home.open("drv-a");
    let bytes: Vec<u8> = (0..20 * 1024 * 1024).map(|n| (n % 251) as u8).collect();
    fs::write(&path, &bytes).unwrap();
    data(home.upload("drv-a", "update"));
    assert!(google.drive_file("drv-a").unwrap().content == bytes);
    assert_eq!(google.log().iter().filter(|s| s.method == "PUT").count(), 3);
}

#[test]
fn a_piece_kept_short_goes_on_from_where_drive_says() {
    let google = FakeGoogle::start(Seed::default());
    google.seed_file(a_docx("drv-a"));
    let home = Home::new(&google.base(), "short");
    let path = home.open("drv-a");
    let bytes: Vec<u8> = (0..20 * 1024 * 1024).map(|n| (n % 251) as u8).collect();
    fs::write(&path, &bytes).unwrap();
    google.drive_keeps_short(5 * 1024 * 1024 + 7);
    data(home.upload("drv-a", "update"));
    assert!(google.drive_file("drv-a").unwrap().content == bytes);
}

/// The last piece has nothing after it to carry what was dropped.
#[test]
fn a_last_piece_kept_short_is_sent_again_and_not_called_done() {
    let google = FakeGoogle::start(Seed::default());
    google.seed_file(a_docx("drv-a"));
    let home = Home::new(&google.base(), "shortlast");
    let path = home.open("drv-a");
    fs::write(&path, b"PK edited here").unwrap();
    google.drive_keeps_short(3);
    data(home.upload("drv-a", "update"));
    assert_eq!(
        google.drive_file("drv-a").unwrap().content,
        b"PK edited here"
    );
}

/// A 308 that keeps nothing can't be sent past, and mustn't be sent forever.
#[test]
fn a_piece_with_nothing_kept_stops_and_the_file_stays() {
    let google = FakeGoogle::start(Seed::default());
    google.seed_file(a_docx("drv-a"));
    let home = Home::new(&google.base(), "shortnone");
    let path = home.open("drv-a");
    fs::write(&path, b"PK edited here").unwrap();
    google.drive_keeps_short(0);
    assert_eq!(home.upload("drv-a", "update")["isError"], true);
    assert_eq!(fs::read(&path).unwrap(), b"PK edited here");
    assert_eq!(google.log().iter().filter(|s| s.method == "PUT").count(), 1);
    data(home.upload("drv-a", "update"));
}

fn puts(google: &FakeGoogle) -> usize {
    google.log().iter().filter(|s| s.method == "PUT").count()
}

#[test]
fn an_answer_that_cant_be_read_after_the_last_piece_is_still_uploaded() {
    let google = FakeGoogle::start(Seed::default());
    google.seed_file(a_docx("drv-a"));
    let home = Home::new(&google.base(), "garbled");
    let path = home.open("drv-a");
    fs::write(&path, b"PK mine").unwrap();
    google.drive_garbles_the_last_answer();
    let answer = data(home.upload("drv-a", "copy"));
    assert_eq!(
        answer,
        json!({"uploaded": true, "mode": "copy"}),
        "no id to give"
    );
    assert_eq!(google.drive_file("drv-copy-1").unwrap().content, b"PK mine");
    assert!(!home.folder("drv-a").exists());
    assert_eq!(home.edits(), Vec::<Value>::new());
    assert_eq!(refusal(&home.upload("drv-a", "copy")), "not_found");
    assert_eq!(puts(&google), 1, "no second copy");
}

#[test]
fn a_folder_that_wont_go_after_an_upload_is_no_failure_and_cant_be_sent_twice() {
    use std::os::unix::fs::PermissionsExt as _;
    let google = FakeGoogle::start(Seed::default());
    google.seed_file(a_docx("drv-a"));
    let home = Home::new(&google.base(), "stuck");
    let path = home.open("drv-a");
    fs::write(&path, b"PK mine").unwrap();
    let folder = home.folder("drv-a");
    let mode = |bits| fs::set_permissions(&folder, fs::Permissions::from_mode(bits)).unwrap();
    mode(0o500);
    let answer = home.upload("drv-a", "copy");
    assert_eq!(data(answer)["uploaded"], true, "Drive has it");
    assert!(path.exists(), "this is the case being tested");
    assert_eq!(home.edits(), Vec::<Value>::new(), "nothing left to answer");
    assert_eq!(refusal(&home.upload("drv-a", "copy")), "not_found");
    assert_eq!(puts(&google), 1, "no second copy");
    assert!(google.drive_file("drv-copy-2").is_none());
    mode(0o700);
    gws::files::sweep(&home.core);
    assert!(!folder.exists(), "the sweep finishes it");
}

/// Each of what can move: the bytes, or only the version, as a rename does.
fn conflict(change: impl FnOnce(&FakeGoogle), name: &str) {
    let google = FakeGoogle::start(Seed::default());
    google.seed_file(a_docx("drv-a"));
    let home = Home::new(&google.base(), name);
    let path = home.open("drv-a");
    fs::write(&path, b"PK mine").unwrap();
    assert_eq!(home.edits()[0]["changed_on_drive"], false);
    change(&google);
    let theirs = google.drive_file("drv-a").unwrap();
    assert_eq!(home.edits()[0]["changed_on_drive"], true);

    let answer = home.upload("drv-a", "update");
    assert_eq!(refusal(&answer), "changed", "{answer}");
    let said = answer["structuredContent"]["message"].as_str().unwrap();
    assert!(said.contains("nothing was uploaded"), "{said}");
    assert_eq!(
        drive_writes(&google),
        Vec::<String>::new(),
        "nothing written"
    );
    assert_eq!(google.drive_file("drv-a").unwrap(), theirs);
    assert_eq!(
        fs::read(&path).unwrap(),
        b"PK mine",
        "the person's work stays"
    );
    assert_eq!(home.edits().len(), 1);

    // the person saw it, and pressed
    data(home.upload("drv-a", "replace"));
    assert_eq!(google.drive_file("drv-a").unwrap().content, b"PK mine");
    assert!(!home.folder("drv-a").exists());
}

#[test]
fn a_conflict_on_the_bytes_writes_nothing_until_replace_is_pressed() {
    conflict(|g| g.change_on_drive("drv-a", b"PK theirs"), "conflict1");
}

#[test]
fn a_conflict_on_the_version_alone_writes_nothing_either() {
    conflict(
        |g| g.edit_file("drv-a", |f| f.name = "Renamed.docx".to_owned()),
        "conflict2",
    );
}

/// md5 moving with `version` held back: each is checked on its own.
#[test]
fn a_conflict_on_the_checksum_alone_writes_nothing_either() {
    let google = FakeGoogle::start(Seed::default());
    google.seed_file(a_docx("drv-a"));
    let home = Home::new(&google.base(), "conflict3");
    let path = home.open("drv-a");
    fs::write(&path, b"PK mine").unwrap();
    google.edit_file("drv-a", |f| {
        f.content = b"PK theirs".to_vec();
        f.version -= 1;
    });
    assert_eq!(refusal(&home.upload("drv-a", "update")), "changed");
    assert_eq!(drive_writes(&google), Vec::<String>::new());
}

#[test]
fn a_copy_goes_beside_the_original_and_leaves_it_alone() {
    let google = FakeGoogle::start(Seed::default());
    google.seed_file(FakeFile::folder("drv-folder", "Folder"));
    google.seed_file(a_docx("drv-a").inside("drv-folder"));
    let home = Home::new(&google.base(), "copy");
    let path = home.open("drv-a");
    fs::write(&path, b"PK mine").unwrap();
    google.change_on_drive("drv-a", b"PK theirs");
    let theirs = google.drive_file("drv-a").unwrap();
    let answer = data(home.upload("drv-a", "copy"));
    let copy = google.drive_file(answer["id"].as_str().unwrap()).unwrap();
    assert_eq!(copy.name, "Roof quote (copy).docx");
    assert_eq!(copy.content, b"PK mine");
    assert_eq!(copy.parents, ["drv-folder"]);
    assert_eq!(google.drive_file("drv-a").unwrap(), theirs);
    assert!(!home.folder("drv-a").exists());
}

#[test]
fn a_copy_of_a_file_gone_from_drive_lands_in_my_drive_and_update_says_its_gone() {
    let google = FakeGoogle::start(Seed::default());
    google.seed_file(a_docx("drv-a"));
    let home = Home::new(&google.base(), "gonecopy");
    let path = home.open("drv-a");
    fs::write(&path, b"PK mine").unwrap();
    google.remove_file("drv-a");
    assert_eq!(home.edits()[0]["changed_on_drive"], true);
    for mode in ["update", "replace"] {
        assert_eq!(refusal(&home.upload("drv-a", mode)), "not_found", "{mode}");
    }
    assert!(path.exists(), "the work is still here");
    let answer = data(home.upload("drv-a", "copy"));
    assert!(google.drive_file(answer["id"].as_str().unwrap()).is_some());
}

#[test]
fn slides_go_to_the_browser_and_nothing_lands_here() {
    let google = FakeGoogle::start(Seed::demo(0));
    google.seed_file(FakeFile::new("drv-p", "Pitch", GOOGLE_SLIDES).holding(b"PK exported"));
    let home = Home::new(&google.base(), "slides");
    let answer = home.core.run("gws_file_open", &json!({"id": "drv-p"}));
    assert_eq!(answer["structuredContent"]["refused"], "in_browser");
    assert!(
        answer["structuredContent"]["link"]
            .as_str()
            .unwrap()
            .contains("/presentation/d/drv-p/")
    );
    assert!(answer["_meta"].is_null(), "the Bot has nothing to open");
    assert!(!google.log().iter().any(|s| s.target.contains("/export")));
}

#[test]
fn a_google_original_is_never_written_over_and_only_a_copy_goes_up() {
    let google = FakeGoogle::start(Seed::default());
    for (id, name, mime) in [
        ("drv-d", "Notes", GOOGLE_DOC),
        ("drv-s", "Invoices", GOOGLE_SHEET),
        ("drv-p", "Pitch", GOOGLE_SLIDES),
    ] {
        google.seed_file(FakeFile::new(id, name, mime).holding(b"PK exported"));
    }
    let home = Home::new(&google.base(), "google");
    for (id, office, name) in [
        ("drv-d", DOCX, "Notes (copy).docx"),
        ("drv-s", XLSX, "Invoices (copy).xlsx"),
    ] {
        let path = home.open(id);
        fs::write(&path, b"PK mine").unwrap();
        let original = google.drive_file(id).unwrap();
        let edit = home.edits();
        let edit = edit.iter().find(|e| e["id"] == id).unwrap();
        assert_eq!(edit["google"], true);
        let before = drive_writes(&google);
        for mode in ["update", "replace"] {
            let answer = home.upload(id, mode);
            assert_eq!(refusal(&answer), "google_original", "{mode}: {answer}");
        }
        let written = drive_writes(&google);
        assert!(written == before, "{id} wrote: {written:?}");
        assert_eq!(google.drive_file(id).unwrap(), original);
        assert_eq!(fs::read(&path).unwrap(), b"PK mine");

        let answer = data(home.upload(id, "copy"));
        let copy = google.drive_file(answer["id"].as_str().unwrap()).unwrap();
        assert_eq!((copy.name.as_str(), copy.mime.as_str()), (name, office));
        assert_eq!(
            google.drive_file(id).unwrap(),
            original,
            "written over: {id}"
        );
        assert!(!home.folder(id).exists());
        let patches = drive_writes(&google);
        assert!(
            !patches.iter().any(|w| w.starts_with("PATCH")),
            "{patches:?}"
        );
    }
}

#[test]
fn nothing_reaches_drive_without_a_press() {
    let google = FakeGoogle::start(Seed::default().with_drive());
    google.seed_file(a_docx("drv-a"));
    let home = Home::new(&google.base(), "nopress");
    let path = home.open("drv-a");
    fs::write(&path, b"PK mine").unwrap();
    for hours in [0, 1, 23, 25, 48] {
        home.later(hours * 3_600_000);
        gws::mail::sync(&home.core).unwrap();
        gws::files::sweep(&home.core);
        home.edits();
        home.open("drv-a");
    }
    assert_eq!(drive_writes(&google), Vec::<String>::new());
    assert_eq!(
        google.drive_file("drv-a").unwrap().content,
        b"PK as it came"
    );
}

#[test]
fn a_failed_upload_keeps_the_file_and_says_so() {
    let google = FakeGoogle::start(Seed::default());
    google.seed_file(a_docx("drv-a"));
    let home = Home::new(&google.base(), "failed");
    let path = home.open("drv-a");
    fs::write(&path, b"PK mine").unwrap();
    google.drive_uploads_fail_after(0, 403);
    assert_eq!(refusal(&home.upload("drv-a", "update")), "no_access");
    assert_eq!(fs::read(&path).unwrap(), b"PK mine");
    google.drive_uploads_fail_after(0, 503);
    assert_eq!(home.upload("drv-a", "update")["isError"], true);
    assert_eq!(fs::read(&path).unwrap(), b"PK mine");
    assert_eq!(
        google.drive_file("drv-a").unwrap().content,
        b"PK as it came"
    );
    google.drive_works();
    data(home.upload("drv-a", "update"));
    assert_eq!(google.drive_file("drv-a").unwrap().content, b"PK mine");
}

#[test]
fn what_has_nothing_to_send_sends_nothing() {
    let google = FakeGoogle::start(Seed::default());
    google.seed_file(a_docx("drv-a"));
    let home = Home::new(&google.base(), "nothing");
    assert_eq!(
        refusal(&home.upload("drv-a", "update")),
        "not_found",
        "never opened"
    );
    let path = home.open("drv-a");
    assert_eq!(refusal(&home.upload("drv-a", "update")), "not_edited");
    assert_eq!(refusal(&home.upload("drv-a", "nonsense")), "invalid_args");
    // a save cut off must not become a revision
    fs::write(&path, b"").unwrap();
    assert_eq!(refusal(&home.upload("drv-a", "replace")), "empty");
    // what's there must be the file, not a way to another one
    fs::remove_file(&path).unwrap();
    let secret = home.dir.join("secret");
    fs::write(&secret, b"not for Drive").unwrap();
    std::os::unix::fs::symlink(&secret, &path).unwrap();
    assert_eq!(refusal(&home.upload("drv-a", "replace")), "not_found");
    assert_eq!(drive_writes(&google), Vec::<String>::new());
}

// --- Opening again --------------------------------------------------------

fn downloads(google: &FakeGoogle) -> usize {
    google
        .log()
        .iter()
        .filter(|s| s.target.contains("alt=media"))
        .count()
}

#[test]
fn opening_again_never_loses_an_edit_and_only_fetches_what_moved() {
    let google = FakeGoogle::start(Seed::default());
    google.seed_file(a_docx("drv-a"));
    let home = Home::new(&google.base(), "again");
    let path = home.open("drv-a");
    assert_eq!(home.open("drv-a"), path);
    assert_eq!(downloads(&google), 1, "the same bytes aren't fetched twice");

    google.change_on_drive("drv-a", b"PK newer");
    assert_eq!(fs::read(home.open("drv-a")).unwrap(), b"PK newer");
    assert_eq!(downloads(&google), 2);
    assert_eq!(
        home.edits(),
        Vec::<Value>::new(),
        "what came is the new base"
    );

    fs::write(&path, b"PK mine").unwrap();
    google.change_on_drive("drv-a", b"PK newest");
    assert_eq!(fs::read(home.open("drv-a")).unwrap(), b"PK mine");
    assert_eq!(downloads(&google), 2);
    assert_eq!(
        home.edits()[0]["changed_on_drive"],
        true,
        "and the conflict stands"
    );
}

#[test]
fn a_rename_on_drive_leaves_one_file_in_the_folder() {
    let google = FakeGoogle::start(Seed::default());
    google.seed_file(a_docx("drv-a"));
    let home = Home::new(&google.base(), "renamed");
    home.open("drv-a");
    google.edit_file("drv-a", |f| f.name = "New name.docx".to_owned());
    let path = home.open("drv-a");
    assert_eq!(path.file_name().unwrap(), "New name.docx");
    assert_eq!(fs::read_dir(home.folder("drv-a")).unwrap().count(), 1);
}

#[test]
fn another_accounts_edits_arent_opened_over() {
    let google = FakeGoogle::start(Seed::default());
    google.seed_file(a_docx("drv-a"));
    let home = Home::new(&google.base(), "accounts");
    let path = home.open("drv-a");
    fs::write(&path, b"PK mine").unwrap();
    let mut second = push("second@example.com");
    second["label"] = json!("work");
    // both signed in; the second is the one in use once the first steps aside
    home.core.account("buwiz.account.set", &second);
    home.core.accounts.set(&json!({"account": "google", "label": "default", "provider": "google", "subject": FAKE_EMAIL, "scopes": [], "access_token": TOKEN, "token_type": "Bearer", "expires_at": 1000}));
    let answer = home.core.run("gws_file_open", &json!({"id": "drv-a"}));
    assert_eq!(refusal(&answer), "in_use", "{answer}");
    assert_eq!(fs::read(&path).unwrap(), b"PK mine");
    assert_eq!(
        refusal(&home.upload("drv-a", "update")),
        "not_found",
        "and one account can't send another's edit"
    );
}

// --- When a file goes -----------------------------------------------------

#[test]
fn discard_takes_the_file_and_tells_drive_nothing() {
    let google = FakeGoogle::start(Seed::default());
    google.seed_file(a_docx("drv-a"));
    let home = Home::new(&google.base(), "discard");
    let path = home.open("drv-a");
    fs::write(&path, b"PK mine").unwrap();
    data(home.core.run("gws_file_discard", &json!({"id": "drv-a"})));
    assert!(!home.folder("drv-a").exists());
    assert_eq!(home.edits(), Vec::<Value>::new());
    assert_eq!(drive_writes(&google), Vec::<String>::new());
    assert_eq!(
        google.drive_file("drv-a").unwrap().content,
        b"PK as it came"
    );
    // nothing on record: this opens fresh
    assert_eq!(fs::read(home.open("drv-a")).unwrap(), b"PK as it came");
}

#[test]
fn only_its_own_account_can_discard_a_file() {
    let google = FakeGoogle::start(Seed::default());
    google.seed_file(a_docx("drv-a"));
    let home = Home::new(&google.base(), "discardwho");
    let path = home.open("drv-a");
    fs::write(&path, b"PK mine").unwrap();
    let discard = || home.core.run("gws_file_discard", &json!({"id": "drv-a"}));
    home.core
        .account("buwiz.account.set", &push("second@example.com"));
    assert_eq!(refusal(&discard()), "not_found", "as an upload says");
    assert_eq!(fs::read(&path).unwrap(), b"PK mine");
    home.core.account("buwiz.account.set", &push(FAKE_EMAIL));
    data(discard());
    assert!(!home.folder("drv-a").exists());
}

#[test]
fn an_edit_can_be_discarded_with_nobody_signed_in() {
    let google = FakeGoogle::start(Seed::default());
    google.seed_file(a_docx("drv-a"));
    let home = Home::new(&google.base(), "discardout");
    let path = home.open("drv-a");
    fs::write(&path, b"PK mine").unwrap();
    home.sign_out();
    data(home.core.run("gws_file_discard", &json!({"id": "drv-a"})));
    assert!(!home.folder("drv-a").exists());
}

#[test]
fn a_file_goes_at_24_hours_and_not_before() {
    let google = FakeGoogle::start(Seed::default());
    google.seed_file(a_docx("drv-a"));
    let home = Home::new(&google.base(), "day");
    let path = home.open("drv-a");
    home.later(LIFE_MS - 1);
    gws::files::sweep(&home.core);
    assert!(path.exists(), "a moment before");
    home.later(LIFE_MS);
    gws::files::sweep(&home.core);
    assert!(!home.folder("drv-a").exists());
    assert_eq!(
        fs::read(home.open("drv-a")).unwrap(),
        b"PK as it came",
        "no stale record"
    );
}

#[test]
fn looking_at_the_edits_is_enough_to_sweep() {
    let google = FakeGoogle::start(Seed::default());
    google.seed_file(a_docx("drv-a"));
    let home = Home::new(&google.base(), "sweepread");
    home.open("drv-a");
    home.later(LIFE_MS);
    home.edits();
    assert!(!home.folder("drv-a").exists());
}

#[test]
fn opening_again_starts_the_24_hours_over() {
    let google = FakeGoogle::start(Seed::default());
    google.seed_file(a_docx("drv-a"));
    let home = Home::new(&google.base(), "restart");
    let path = home.open("drv-a");
    home.later(LIFE_MS - 1);
    home.open("drv-a");
    home.later(LIFE_MS + 1000);
    gws::files::sweep(&home.core);
    assert!(path.exists(), "the viewer has it, from an hour ago");
}

#[test]
fn an_edited_file_stays_past_24_hours_until_the_person_answers() {
    let google = FakeGoogle::start(Seed::default());
    google.seed_file(a_docx("drv-a"));
    let home = Home::new(&google.base(), "stays");
    let path = home.open("drv-a");
    fs::write(&path, b"PK mine").unwrap();
    home.later(10 * LIFE_MS);
    gws::files::sweep(&home.core);
    assert_eq!(fs::read(&path).unwrap(), b"PK mine");
    assert_eq!(home.edits().len(), 1, "still there to answer");
    data(home.upload("drv-a", "update"));
    assert!(!home.folder("drv-a").exists());
}

#[test]
fn a_row_whose_file_went_and_a_folder_with_no_row_are_swept() {
    let google = FakeGoogle::start(Seed::default());
    google.seed_file(a_docx("drv-a"));
    let home = Home::new(&google.base(), "strays");
    let path = home.open("drv-a");
    fs::remove_file(&path).unwrap();
    let stray = home.folder("drv-crashed");
    fs::create_dir_all(&stray).unwrap();
    fs::write(stray.join("left.docx"), b"PK").unwrap();
    gws::files::sweep(&home.core);
    assert!(!stray.exists() && !home.folder("drv-a").exists());
    assert_eq!(fs::read(home.open("drv-a")).unwrap(), b"PK as it came");
}

#[test]
fn sign_out_keeps_unsaved_work_and_takes_the_rest() {
    let google = FakeGoogle::start(Seed::default());
    google.seed_file(a_docx("drv-a"));
    google.seed_file(FakeFile::new("drv-b", "Other.docx", DOCX).holding(b"PK"));
    let home = Home::new(&google.base(), "signout");
    let edited = home.open("drv-a");
    let clean = home.open("drv-b");
    fs::write(&edited, b"PK mine").unwrap();
    home.sign_out();
    assert_eq!(fs::read(&edited).unwrap(), b"PK mine", "it's nowhere else");
    assert!(!clean.exists(), "it can be fetched again");
    assert_eq!(
        drive_writes(&google),
        Vec::<String>::new(),
        "and sends nothing"
    );
    home.core.account("buwiz.account.set", &push(FAKE_EMAIL));
    let edits = home.edits();
    assert_eq!(edits.len(), 1, "back for the person to answer");
    assert_eq!(edits[0]["id"], "drv-a");
}

#[test]
fn a_sweep_after_sign_out_leaves_unsaved_work() {
    let google = FakeGoogle::start(Seed::default());
    google.seed_file(a_docx("drv-a"));
    let home = Home::new(&google.base(), "signoutsweep");
    let edited = home.open("drv-a");
    fs::write(&edited, b"PK mine").unwrap();
    home.sign_out();
    home.later(LIFE_MS * 3);
    gws::files::sweep(&home.core);
    assert_eq!(fs::read(&edited).unwrap(), b"PK mine");
}

/// Whatever the pushes do, in whatever order, and whatever is cleared after
/// the last: what was edited here stays, for its account to come back to.
#[test]
fn unsaved_work_stays_through_pushes_and_the_clear_after_them() {
    let google = FakeGoogle::start(Seed::default());
    google.seed_file(a_docx("drv-a"));
    let home = Home::new(&google.base(), "pushes");
    let edited = home.open("drv-a");
    fs::write(&edited, b"PK mine").unwrap();
    home.sign_out();
    for (label, who) in [("one", "one@example.com"), ("two", "two@example.com")] {
        let mut next = push(who);
        next["label"] = json!(label);
        home.core.account("buwiz.account.set", &next);
        gws::mail::finish_wipes(&home.core);
    }
    home.later(gws::mail::SETTLE_MS);
    gws::mail::finish_wipes(&home.core);
    assert_eq!(fs::read(&edited).unwrap(), b"PK mine");
    home.core.account("buwiz.account.set", &push(FAKE_EMAIL));
    assert_eq!(home.edits().len(), 1, "back for the person to answer");
}

#[test]
fn another_account_taking_over_never_sees_the_work_it_replaced() {
    let google = FakeGoogle::start(Seed::default());
    google.seed_file(a_docx("drv-a"));
    let home = Home::new(&google.base(), "replaced");
    let path = home.open("drv-a");
    fs::write(&path, b"PK mine").unwrap();
    home.core
        .account("buwiz.account.set", &push("second@example.com"));
    assert!(path.exists(), "it's still the first account's work");
    assert_eq!(
        home.edits(),
        Vec::<Value>::new(),
        "and none of the second's"
    );
    home.core.account("buwiz.account.set", &push(FAKE_EMAIL));
    assert_eq!(home.edits().len(), 1);
}

#[test]
fn a_second_account_signing_out_leaves_the_others_files() {
    let google = FakeGoogle::start(Seed::default());
    google.seed_file(a_docx("drv-a"));
    let home = Home::new(&google.base(), "twoaccounts");
    let path = home.open("drv-a");
    let mut second = push("second@example.com");
    second["label"] = json!("work");
    home.core.account("buwiz.account.set", &second);
    home.core.account(
        "buwiz.account.clear",
        &json!({"account": "google", "label": "work"}),
    );
    assert!(path.exists(), "that one is still signed in");
}

/// A store that won't open can't say what's edited, so nothing goes.
#[test]
fn sign_out_with_no_store_to_ask_takes_nothing() {
    let google = FakeGoogle::start(Seed::default());
    google.seed_file(a_docx("drv-a"));
    let home = Home::new(&google.base(), "nostore");
    let path = home.open("drv-a");
    let dir = home.dir.clone();
    let locked = Core::new(
        &Apis::at(&google.base()),
        Ok(gws_store::Paths::in_dir(&dir)),
    );
    locked.account("buwiz.account.set", &push(FAKE_EMAIL));
    locked.account(
        "buwiz.account.clear",
        &json!({"account": "google", "label": "default"}),
    );
    assert!(path.exists());
}

#[test]
fn an_uninstall_takes_the_folder() {
    let google = FakeGoogle::start(Seed::default());
    google.seed_file(a_docx("drv-a"));
    let home = Home::new(&google.base(), "uninstall");
    home.open("drv-a");
    home.core.uninstall().unwrap();
    assert!(!home.open_dir().exists());
}

// --- Attachments ----------------------------------------------------------

fn mail_with(attachments: Vec<(&str, &str, &[u8])>) -> Mail {
    Mail {
        id: "18f1aa".to_owned(),
        thread: "18f1aa".to_owned(),
        labels: vec!["INBOX".to_owned()],
        from: "Ana Reyes <ana@example.com>".to_owned(),
        to: FAKE_EMAIL.to_owned(),
        subject: "A file".to_owned(),
        at: NOON,
        text: "Here it is.".to_owned(),
        html: None,
        attachments: attachments
            .into_iter()
            .map(|(n, m, b)| (n.to_owned(), m.to_owned(), b.to_vec()))
            .collect(),
        message_id: "<a@example.com>".to_owned(),
        inline: Vec::new(),
    }
}

fn open_attachment(home: &Home, n: usize) -> Value {
    home.core.run(
        "gws_mail_attachment_open",
        &json!({"message": "18f1aa", "part": (n + 1).to_string()}),
    )
}

#[test]
fn an_attachment_opens_the_same_way_in_a_folder_of_its_own() {
    let google = FakeGoogle::start(Seed::default());
    google.deliver(mail_with(vec![("report.docx", DOCX, b"PK report")]));
    let home = Home::new(&google.base(), "att");
    let answer = open_attachment(&home, 0);
    let path = path_in(&answer);
    assert_eq!(fs::read(&path).unwrap(), b"PK report");
    assert_eq!(answer["_meta"]["buwiz/document"]["name"], "report.docx");
    assert_eq!(answer["_meta"]["buwiz/document"]["mime"], DOCX);
    assert_eq!(answer["structuredContent"]["opened"], true);
    let folder = path.parent().unwrap();
    assert_eq!(folder.parent().unwrap(), real(&home.open_dir()));
    let name = folder.file_name().unwrap().to_str().unwrap();
    assert!(name.starts_with("att-18f1aa-"), "{name}");
    assert_eq!(
        open_attachment(&home, 0)["_meta"],
        answer["_meta"],
        "kept, not fetched again"
    );
    assert_eq!(
        google
            .log()
            .iter()
            .filter(|s| s.target.contains("/attachments/"))
            .count(),
        1
    );
}

#[test]
fn a_thread_says_which_message_each_attachment_is_on() {
    let google = FakeGoogle::start(Seed::default());
    google.deliver(mail_with(vec![("report.docx", DOCX, b"PK report")]));
    let home = Home::new(&google.base(), "attthread");
    let thread = data(
        home.core
            .run("gws_mail_thread", &json!({"thread": "18f1aa"})),
    );
    let listed = &thread["attachments"][0];
    assert_eq!(listed["message"], "18f1aa", "{thread}");
    let answer = home.core.run(
        "gws_mail_attachment_open",
        &json!({"message": listed["message"], "part": listed["part"]}),
    );
    assert_eq!(fs::read(path_in(&answer)).unwrap(), b"PK report");
}

#[test]
fn an_attachments_hostile_name_stays_in_its_folder() {
    let google = FakeGoogle::start(Seed::default());
    google.deliver(mail_with(vec![
        ("../../../evil.docx", DOCX, b"PK"),
        ("/etc/passwd.docx", DOCX, b"PK fake"),
        (".hidden.docx", "application/octet-stream", b"PK fake"),
    ]));
    let home = Home::new(&google.base(), "atthostile");
    for n in 0..3 {
        let path = path_in(&open_attachment(&home, n));
        assert!(path.starts_with(real(&home.open_dir())), "{path:?}");
        assert_eq!(path.ancestors().nth(2).unwrap(), real(&home.open_dir()));
        assert!(!path.file_name().unwrap().to_str().unwrap().starts_with('.'));
    }
}

/// What a fresh read of the mail says of its first attachment. The thread tool
/// keeps what it showed, so it's Gmail that's asked here.
fn read_again(home: &Home) -> Value {
    let auth = home.core.accounts.google().unwrap();
    let mail = home.core.gmail.full(&auth, "18f1aa").unwrap().unwrap();
    gws::gmail::attachments(&mail)[0].clone()
}

#[test]
fn an_id_that_changes_between_reads_is_still_the_same_attachment() {
    let google = FakeGoogle::start(Seed::default());
    google.deliver(mail_with(vec![("report.docx", DOCX, b"PK report")]));
    google.attachment_ids_change();
    let home = Home::new(&google.base(), "attid");
    let first = read_again(&home);
    let path = path_in(&open_attachment(&home, 0));
    fs::write(&path, b"PK mine").unwrap();
    let second = read_again(&home);
    assert_ne!(first["id"], second["id"], "this is the case being tested");
    assert_eq!(first["part"], second["part"]);
    assert_eq!(first["part"], "1");
    assert_eq!(
        path_in(&open_attachment(&home, 0)),
        path,
        "the edit, not a second folder"
    );
    assert_eq!(fs::read(&path).unwrap(), b"PK mine");
    assert_eq!(fs::read_dir(home.open_dir()).unwrap().count(), 1);
    assert_eq!(home.edits().len(), 1);
}

/// Both signed in, and the second is the one in use.
fn second_account_in_use(home: &Home) {
    let mut second = push("second@example.com");
    second["label"] = json!("work");
    home.core.account("buwiz.account.set", &second);
    home.core.accounts.set(&json!({"account": "google", "label": "default", "provider": "google", "subject": FAKE_EMAIL, "scopes": [], "access_token": TOKEN, "token_type": "Bearer", "expires_at": 1000}));
}

#[test]
fn another_accounts_edited_attachment_isnt_opened_over() {
    let google = FakeGoogle::start(Seed::default());
    google.deliver(mail_with(vec![("report.docx", DOCX, b"PK report")]));
    let home = Home::new(&google.base(), "attaccounts");
    let path = path_in(&open_attachment(&home, 0));
    fs::write(&path, b"PK mine").unwrap();
    second_account_in_use(&home);
    let answer = open_attachment(&home, 0);
    assert_eq!(refusal(&answer), "in_use", "{answer}");
    assert_eq!(answer.get("_meta"), None);
    assert_eq!(fs::read(&path).unwrap(), b"PK mine");
}

#[test]
fn another_accounts_clean_attachment_is_fetched_for_this_one() {
    let google = FakeGoogle::start(Seed::default());
    google.deliver(mail_with(vec![("report.docx", DOCX, b"PK report")]));
    let home = Home::new(&google.base(), "attclean");
    let path = path_in(&open_attachment(&home, 0));
    second_account_in_use(&home);
    assert_eq!(path_in(&open_attachment(&home, 0)), path);
    assert_eq!(
        google
            .log()
            .iter()
            .filter(|s| s.target.contains("/attachments/"))
            .count(),
        2,
        "it was the other's, so it's fetched"
    );
}

/// Opens the first attachment and edits it, and says what its folder's called.
fn edited_attachment(home: &Home) -> (PathBuf, String) {
    let path = path_in(&open_attachment(home, 0));
    fs::write(&path, b"PK mine").unwrap();
    let id = path.parent().unwrap().file_name().unwrap();
    let id = id.to_str().unwrap().to_owned();
    (path, id)
}

#[test]
fn an_edited_attachment_is_listed_and_saves_as_a_new_file_in_my_drive() {
    let google = FakeGoogle::start(Seed::default());
    google.deliver(mail_with(vec![("report.docx", DOCX, b"PK report")]));
    let home = Home::new(&google.base(), "attedit");
    let (path, id) = edited_attachment(&home);
    let edits = home.edits();
    assert_eq!(
        edits,
        [
            json!({"id": id, "name": "report.docx", "state": "edited", "google": false, "attachment": true, "changed_on_drive": false, "working": false})
        ]
    );
    for mode in ["update", "replace"] {
        assert_eq!(refusal(&home.upload(&id, mode)), "unsupported", "{mode}");
    }
    assert_eq!(drive_writes(&google), Vec::<String>::new());
    assert_eq!(fs::read(&path).unwrap(), b"PK mine");
    let answer = data(home.upload(&id, "copy"));
    let copy = google.drive_file(answer["id"].as_str().unwrap()).unwrap();
    assert_eq!(copy.name, "report (copy).docx");
    assert_eq!(copy.content, b"PK mine");
    assert_eq!(copy.parents, [DRIVE_ROOT], "there's no folder it came from");
    assert!(!home.folder(&id).exists());
    assert!(
        !google
            .log()
            .iter()
            .any(|s| s.target.contains(&format!("files/{id}"))),
        "Drive was never asked about a folder's name"
    );
}

#[test]
fn an_edited_attachment_goes_only_when_it_is_discarded() {
    let google = FakeGoogle::start(Seed::default());
    google.deliver(mail_with(vec![("report.docx", DOCX, b"PK report")]));
    let home = Home::new(&google.base(), "attdiscard");
    let (path, id) = edited_attachment(&home);
    data(home.core.run("gws_file_discard", &json!({"id": id})));
    assert!(!path.exists());
    assert_eq!(home.edits(), Vec::<Value>::new());
}

#[test]
fn an_edited_attachment_outlasts_its_24_hours_and_a_sign_out() {
    let google = FakeGoogle::start(Seed::default());
    google.deliver(mail_with(vec![("report.docx", DOCX, b"PK report")]));
    let home = Home::new(&google.base(), "attstays");
    let (path, _) = edited_attachment(&home);
    home.later(LIFE_MS * 3);
    gws::files::sweep(&home.core);
    assert_eq!(fs::read(&path).unwrap(), b"PK mine", "past its time");
    home.sign_out();
    assert_eq!(fs::read(&path).unwrap(), b"PK mine", "through a sign-out");
    home.later(LIFE_MS * 6);
    gws::files::sweep(&home.core);
    assert_eq!(fs::read(&path).unwrap(), b"PK mine", "and a sweep after it");
    home.core.account("buwiz.account.set", &push(FAKE_EMAIL));
    assert_eq!(home.edits().len(), 1, "back for the person to answer");
}

/// What can't be read can't be shown to be unedited, so it stays.
#[test]
fn a_file_that_cant_be_read_is_kept_and_not_taken_for_unedited() {
    use std::os::unix::fs::PermissionsExt;
    let google = FakeGoogle::start(Seed::default());
    google.seed_file(a_docx("drv-a"));
    let home = Home::new(&google.base(), "unreadable");
    let path = home.open("drv-a");
    fs::write(&path, b"PK mine").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
    home.later(LIFE_MS * 3);
    gws::files::sweep(&home.core);
    assert!(path.exists(), "through a sweep past its time");
    home.sign_out();
    assert!(path.exists(), "and through a sign-out");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"PK mine");
}

#[test]
fn an_attachment_with_no_edit_goes_at_24_hours_and_at_sign_out() {
    let google = FakeGoogle::start(Seed::default());
    google.deliver(mail_with(vec![
        ("a.docx", DOCX, b"PK a"),
        ("b.docx", DOCX, b"PK b"),
    ]));
    let home = Home::new(&google.base(), "attgone");
    let path = path_in(&open_attachment(&home, 0));
    home.later(LIFE_MS);
    gws::files::sweep(&home.core);
    assert!(!path.exists(), "it can be fetched again");
    let path = path_in(&open_attachment(&home, 1));
    home.sign_out();
    assert!(!path.exists());
}

#[test]
fn what_cant_be_viewed_or_found_on_a_mail_is_refused() {
    let google = FakeGoogle::start(Seed::default());
    google.deliver(mail_with(vec![(
        "tool.exe",
        "application/x-msdownload",
        b"MZ",
    )]));
    let home = Home::new(&google.base(), "attrefused");
    assert_eq!(refusal(&open_attachment(&home, 0)), "unsupported");
    assert_eq!(
        refusal(&open_attachment(&home, 5)),
        "not_found",
        "no such part on the mail"
    );
    for (message, part) in [("../x", "1"), ("18f1aa", "../x"), ("18f1aa", ""), ("", "1")] {
        let answer = home.core.run(
            "gws_mail_attachment_open",
            &json!({"message": message, "part": part}),
        );
        assert_eq!(refusal(&answer), "invalid_args", "{message} {part}");
    }
    let missing = home.core.run(
        "gws_mail_attachment_open",
        &json!({"message": "18f1ff", "part": "1"}),
    );
    assert_eq!(refusal(&missing), "not_found");
    assert!(!home.open_dir().exists());
}

// --- What the Bot can take, and Save ----------------------------------------

const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR fake";
const PDF: &[u8] = b"%PDF-1.4\n%fake\n";
const EVERYTHING: [&str; 6] = ["word", "sheet", "image", "pdf", "open_with", "save"];

/// The Bot's `initialize`, saying it can do `what`.
fn bot_can(home: &Home, what: &[&str]) {
    home.core.documents.heard(&json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": "2025-11-25", "capabilities": {"experimental": {"buwiz/documents": what}}}}));
}

fn save_attachment(home: &Home, n: usize) -> Value {
    home.core.run(
        "gws_mail_attachment_save",
        &json!({"message": "18f1aa", "part": (n + 1).to_string()}),
    )
}

/// What a refusal says to the person.
fn says(answer: &Value) -> String {
    answer["structuredContent"]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("nothing said: {answer}"))
        .to_owned()
}

/// The one hand-off an answer carries, by its key, and the path in it.
fn handed(answer: &Value) -> (String, PathBuf) {
    let meta = answer["_meta"]
        .as_object()
        .unwrap_or_else(|| panic!("no _meta: {answer}"));
    assert_eq!(meta.len(), 1, "{answer}");
    let (key, value) = meta.iter().next().unwrap();
    (key.clone(), PathBuf::from(value["path"].as_str().unwrap()))
}

fn fetches(google: &FakeGoogle) -> usize {
    google
        .log()
        .iter()
        .filter(|s| s.target.contains("/attachments/"))
        .count()
}

fn name_on_disk(path: &Path) -> &str {
    path.file_name().unwrap().to_str().unwrap()
}

#[test]
fn each_kind_of_attachment_takes_its_own_way_to_the_bot() {
    let google = FakeGoogle::start(Seed::default());
    google.deliver(mail_with(vec![
        ("IMG_4250.png", "image/png", PNG),
        ("receipt.pdf", "application/pdf", PDF),
        ("report.docx", DOCX, b"PK report"),
        ("site-plans.zip", "application/zip", b"PK\x05\x06"),
        ("invite.ics", "application/ics", b"BEGIN:VCALENDAR"),
        ("notes.txt", "text/plain", b"hello"),
        ("tool.exe", "application/x-msdownload", b"MZ"),
        ("page.html", "text/html", b"<p>"),
    ]));
    let home = Home::new(&google.base(), "ways");
    bot_can(&home, &EVERYTHING);
    for (n, key, mime, name) in [
        (0, "buwiz/document", Some("image/png"), "IMG_4250.png"),
        (1, "buwiz/document", Some("application/pdf"), "receipt.pdf"),
        (2, "buwiz/document", Some(DOCX), "report.docx"),
        (3, "buwiz/open_with", None, "site-plans.zip"),
        (4, "buwiz/open_with", None, "invite.ics"),
        (5, "buwiz/open_with", None, "notes.txt"),
    ] {
        let answer = open_attachment(&home, n);
        let (got, path) = handed(&answer);
        assert_eq!(got, key, "{name}: {answer}");
        assert_eq!(name_on_disk(&path), name);
        assert_eq!(path.ancestors().nth(2).unwrap(), real(&home.open_dir()));
        assert_eq!(
            answer["_meta"][key].get("mime").and_then(Value::as_str),
            mime
        );
        assert_eq!(answer["structuredContent"]["opened"], true);
        assert!(
            !answer["structuredContent"]
                .to_string()
                .contains("gws-files"),
            "no path but in _meta: {answer}"
        );
    }
    let before = fetches(&google);
    for n in [6, 7] {
        let answer = open_attachment(&home, n);
        assert_eq!(refusal(&answer), "unsupported", "{answer}");
        assert_eq!(
            says(&answer),
            "This kind of file can't open here. Save it instead."
        );
        assert_eq!(answer.get("_meta"), None);
    }
    assert_eq!(fetches(&google), before, "nothing fetched for a refusal");
}

#[test]
fn the_rich_mails_photo_pdf_zip_and_invite_each_hand_over() {
    let google = FakeGoogle::start(Seed::default().with_rich_mail("http://127.0.0.1:1"));
    let home = Home::new(&google.base(), "rich");
    bot_can(&home, &EVERYTHING);
    let auth = home.core.accounts.google().unwrap();
    let mut seen = Vec::new();
    for message in ["000019f00000001e", "000019f000000014"] {
        let mail = home.core.gmail.full(&auth, message).unwrap().unwrap();
        for listed in gws::gmail::attachments(&mail) {
            let answer = home.core.run(
                "gws_mail_attachment_open",
                &json!({"message": message, "part": listed["part"]}),
            );
            let (key, path) = handed(&answer);
            seen.push((name_on_disk(&path).to_owned(), key));
        }
    }
    let seen: Vec<(&str, &str)> = seen.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
    assert_eq!(
        seen,
        [
            ("IMG_4250.png", "buwiz/document"),
            ("receipt.pdf", "buwiz/document"),
            ("site-plans.zip", "buwiz/open_with"),
            ("invite.ics", "buwiz/open_with"),
            ("invite.ics", "buwiz/open_with"),
        ]
    );
}

#[test]
fn with_no_viewer_for_it_an_image_goes_to_the_computers_app() {
    let google = FakeGoogle::start(Seed::default());
    google.deliver(mail_with(vec![
        ("IMG_4250.png", "image/png", PNG),
        ("report.docx", DOCX, b"PK report"),
    ]));
    let home = Home::new(&google.base(), "openwith");
    bot_can(&home, &["open_with"]);
    for n in 0..2 {
        assert_eq!(handed(&open_attachment(&home, n)).0, "buwiz/open_with");
    }
    bot_can(&home, &["image"]);
    assert_eq!(
        handed(&open_attachment(&home, 0)).0,
        "buwiz/document",
        "opened again, the way this Bot takes it"
    );
}

#[test]
fn a_disguised_file_cant_pass_as_an_image_or_a_pdf() {
    let google = FakeGoogle::start(Seed::default());
    google.deliver(mail_with(vec![
        ("IMG_1.png", "image/png", b"<html><script>"),
        ("bill.pdf", "application/octet-stream", b"MZ\x90\0"),
        ("photo.jpg", "image/jpeg", PNG),
        ("scan.png", "image/png", PDF),
    ]));
    let home = Home::new(&google.base(), "disguised");
    bot_can(&home, &EVERYTHING);
    for n in [0, 1, 3] {
        let answer = open_attachment(&home, n);
        assert_eq!(refusal(&answer), "unsupported", "{n}: {answer}");
        assert_eq!(
            says(&answer),
            "This kind of file can't open here. Save it instead."
        );
        assert_eq!(answer.get("_meta"), None);
    }
    assert!(!home.open_dir().exists(), "nothing stays");
    // a PNG sent as a JPEG is a PNG, and its name says so
    let answer = open_attachment(&home, 2);
    assert_eq!(answer["_meta"]["buwiz/document"]["mime"], "image/png");
    assert_eq!(name_on_disk(&path_in(&answer)), "photo.jpg.png");
}

#[test]
fn the_name_on_disk_ends_as_its_bytes_are() {
    let google = FakeGoogle::start(Seed::default());
    google.deliver(mail_with(vec![
        ("Scan", "application/pdf", PDF),
        ("IMG_9.PNG", "application/octet-stream", PNG),
        ("../plans", "application/zip", b"PK"),
        ("schedule.ics.exe", "application/ics", b"BEGIN:VCALENDAR"),
    ]));
    let home = Home::new(&google.base(), "diskname");
    bot_can(&home, &EVERYTHING);
    for (n, name) in [
        (0, "Scan.pdf"),
        (1, "IMG_9.PNG"),
        (2, "plans.zip"),
        (3, "schedule.ics.exe.ics"),
    ] {
        assert_eq!(name_on_disk(&handed(&open_attachment(&home, n)).1), name);
    }
}

#[test]
fn save_hands_over_every_kind_as_it_came() {
    let all: Vec<(&str, &str, &[u8])> = vec![
        ("IMG_4250.png", "image/png", PNG),
        ("receipt.pdf", "application/pdf", PDF),
        ("report.docx", DOCX, b"PK report"),
        ("site-plans.zip", "application/zip", b"PK\x05\x06"),
        ("tool.exe", "application/x-msdownload", b"MZ"),
        ("IMG_1.png", "image/png", b"<html>"),
        ("Scan", "application/octet-stream", PDF),
        ("../../evil", "", b"x"),
    ];
    let google = FakeGoogle::start(Seed::default());
    google.deliver(mail_with(all.clone()));
    let home = Home::new(&google.base(), "save");
    bot_can(&home, &EVERYTHING);
    // an open edit of the same part stays as it is
    let edited = path_in(&open_attachment(&home, 2));
    fs::write(&edited, b"PK mine").unwrap();
    let names = [
        "IMG_4250.png",
        "receipt.pdf",
        "report.docx",
        "site-plans.zip",
        "tool.exe",
        "IMG_1.png",
        "Scan.pdf",
        "evil",
    ];
    for (n, (_, _, bytes)) in all.iter().enumerate() {
        let answer = save_attachment(&home, n);
        assert_eq!(data(answer.clone())["saved"], true, "{answer}");
        assert_eq!(answer["structuredContent"]["name"], names[n]);
        let (key, path) = handed(&answer);
        assert_eq!(key, "buwiz/save");
        assert_eq!(answer["_meta"]["buwiz/save"]["name"], names[n]);
        assert_eq!(name_on_disk(&path), names[n]);
        assert_eq!(fs::read(&path).unwrap(), *bytes);
        assert_eq!(path.ancestors().nth(2).unwrap(), real(&home.open_dir()));
        assert_ne!(path.parent(), edited.parent(), "a folder of its own");
    }
    assert_eq!(fs::read(&edited).unwrap(), b"PK mine");
    assert_eq!(home.edits().len(), 1, "the edit, and no saved copy");
}

#[test]
fn each_press_a_bot_cant_answer_says_to_update_it() {
    let google = FakeGoogle::start(Seed::default());
    google.deliver(mail_with(vec![
        ("IMG_4250.png", "image/png", PNG),
        ("receipt.pdf", "application/pdf", PDF),
        ("site-plans.zip", "application/zip", b"PK"),
        ("report.docx", DOCX, b"PK report"),
        ("budget.xlsx", XLSX, b"PK budget"),
    ]));
    let home = Home::new(&google.base(), "update");
    let open = "Update Buwiz Bot to open this here.";
    // an older Bot, which says nothing: what it always took still opens
    for n in 0..3 {
        let answer = open_attachment(&home, n);
        assert_eq!(refusal(&answer), "needs_update", "{answer}");
        assert_eq!(says(&answer), open);
        assert_eq!(answer.get("_meta"), None);
    }
    let answer = save_attachment(&home, 0);
    assert_eq!(says(&answer), "Update Buwiz Bot to save this here.");
    assert_eq!(answer.get("_meta"), None);
    assert_eq!(fetches(&google), 0, "nothing fetched in vain");
    assert_eq!(handed(&open_attachment(&home, 3)).0, "buwiz/document");
    assert_eq!(handed(&open_attachment(&home, 4)).0, "buwiz/document");
    // a Bot that says, and lacks the view and the app for each in turn
    for (lacks, n) in [
        ("image", 0),
        ("pdf", 1),
        ("open_with", 2),
        ("sheet", 4),
        ("word", 3),
    ] {
        let what: Vec<&str> = EVERYTHING
            .iter()
            .copied()
            .filter(|w| *w != lacks && *w != "open_with")
            .collect();
        bot_can(&home, &what);
        let answer = open_attachment(&home, n);
        assert_eq!(refusal(&answer), "needs_update", "{lacks}: {answer}");
        assert_eq!(says(&answer), open, "{lacks}");
        assert_eq!(answer.get("_meta"), None);
    }
    assert_eq!(fetches(&google), 2, "only the two the older Bot opened");
    bot_can(&home, &["word", "sheet", "image", "pdf", "open_with"]);
    assert_eq!(
        says(&save_attachment(&home, 0)),
        "Update Buwiz Bot to save this here."
    );
}

#[test]
fn an_image_or_pdf_never_shows_as_edited() {
    let google = FakeGoogle::start(Seed::default());
    google.deliver(mail_with(vec![
        ("IMG_4250.png", "image/png", PNG),
        ("receipt.pdf", "application/pdf", PDF),
        ("site-plans.zip", "application/zip", b"PK"),
    ]));
    let home = Home::new(&google.base(), "readonly");
    bot_can(&home, &EVERYTHING);
    let mut ids = Vec::new();
    for n in 0..3 {
        let path = handed(&open_attachment(&home, n)).1;
        fs::write(&path, b"changed on this Mac").unwrap();
        ids.push(
            path.parent()
                .unwrap()
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned(),
        );
    }
    assert_eq!(home.edits(), Vec::<Value>::new());
    for id in &ids {
        assert_eq!(refusal(&home.upload(id, "copy")), "not_edited", "{id}");
    }
    assert_eq!(drive_writes(&google), Vec::<String>::new());
}

#[test]
fn images_pdfs_and_saved_copies_go_at_24_hours_and_at_sign_out() {
    let google = FakeGoogle::start(Seed::default());
    google.deliver(mail_with(vec![
        ("IMG_4250.png", "image/png", PNG),
        ("receipt.pdf", "application/pdf", PDF),
    ]));
    let home = Home::new(&google.base(), "readonlygone");
    bot_can(&home, &EVERYTHING);
    let opened = |home: &Home| {
        let paths = [
            path_in(&open_attachment(home, 0)),
            path_in(&open_attachment(home, 1)),
            handed(&save_attachment(home, 0)).1,
        ];
        // even changed, none of them is the person's work
        for path in &paths {
            fs::write(path, b"changed").unwrap();
        }
        paths
    };
    let paths = opened(&home);
    home.later(LIFE_MS);
    gws::files::sweep(&home.core);
    for path in &paths {
        assert!(!path.exists(), "{path:?}");
    }
    let paths = opened(&home);
    home.sign_out();
    for path in &paths {
        assert!(!path.exists(), "{path:?}");
    }
    assert_eq!(every_path(&home.open_dir()), Vec::<PathBuf>::new());
}
