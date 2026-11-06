//! Opening files and editing back, against the stub Google: where a hostile
//! name lands, what a conflict and a Google original write (nothing), and when
//! a file is gone. Time moves by the Core's clock, so nothing sleeps. This
//! proves the plugin against the stub, which is written from what's known of
//! Google's API, not against Google.

use std::fs;
use std::path::{Path, PathBuf};

use gws::Core;
use gws::apis::Apis;
use gws::files::LIFE_MS;
use gws_fake_google::{
    FAKE_EMAIL, FakeFile, FakeGoogle, GOOGLE_DOC, GOOGLE_SHEET, GOOGLE_SLIDES, Mail, Seed,
};
use serde_json::{Value, json};

const TOKEN: &str = "fake-access-1";
const DOCX: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
const PPTX: &str = "application/vnd.openxmlformats-officedocument.presentationml.presentation";
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
        self.dir.join("open")
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
            json!({"id": "drv-a", "name": "Roof quote.docx", "state": "edited", "google": false, "changed_on_drive": false, "working": false})
        ]
    );
    fs::write(&path, b"PK as it came").unwrap();
    assert_eq!(
        home.edits(),
        Vec::<Value>::new(),
        "put back, there's no edit"
    );
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
        ("drv-p", PPTX, "Pitch (copy).pptx"),
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
fn sign_out_takes_every_file_edited_or_not() {
    let google = FakeGoogle::start(Seed::default());
    google.seed_file(a_docx("drv-a"));
    google.seed_file(FakeFile::new("drv-b", "Other.docx", DOCX).holding(b"PK"));
    let home = Home::new(&google.base(), "signout");
    let edited = home.open("drv-a");
    let clean = home.open("drv-b");
    fs::write(&edited, b"PK mine").unwrap();
    home.sign_out();
    assert!(!edited.exists() && !clean.exists());
    assert!(!home.open_dir().exists());
    assert_eq!(
        drive_writes(&google),
        Vec::<String>::new(),
        "and sends nothing"
    );
    home.core.account("buwiz.account.set", &push(FAKE_EMAIL));
    assert_eq!(home.edits(), Vec::<Value>::new(), "no record left either");
}

#[test]
fn another_account_taking_over_takes_the_files_of_the_one_it_replaced() {
    let google = FakeGoogle::start(Seed::default());
    google.seed_file(a_docx("drv-a"));
    let home = Home::new(&google.base(), "replaced");
    let path = home.open("drv-a");
    fs::write(&path, b"PK mine").unwrap();
    home.core
        .account("buwiz.account.set", &push("second@example.com"));
    assert!(!path.exists());
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

/// A store that won't open can't say what's whose. Nobody signed in means
/// nothing in the folder is anyone's, so it goes all the same.
#[test]
fn sign_out_takes_the_files_even_with_no_store_to_ask() {
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
    assert!(!path.exists());
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
    }
}

fn open_attachment(home: &Home, n: usize) -> Value {
    home.core.run(
        "gws_mail_attachment_open",
        &json!({"message": "18f1aa", "attachment": format!("att-18f1aa-{n}")}),
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
        &json!({"message": listed["message"], "attachment": listed["id"]}),
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

#[test]
fn gmails_long_changing_ids_still_open() {
    let google = FakeGoogle::start(Seed::default());
    google.deliver(mail_with(vec![("report.docx", DOCX, b"PK fake")]));
    let home = Home::new(&google.base(), "attid");
    // not what a fresh read lists, and past what a folder's name can hold
    let long = format!("{}-0", "A".repeat(700));
    let answer = home.core.run(
        "gws_mail_attachment_open",
        &json!({"message": "18f1aa", "attachment": long, "name": "report.docx", "mime": DOCX}),
    );
    assert_eq!(fs::read(path_in(&answer)).unwrap(), b"PK fake");
}

#[test]
fn an_attachment_has_no_way_back_and_isnt_listed_as_an_edit() {
    let google = FakeGoogle::start(Seed::default());
    google.deliver(mail_with(vec![("report.docx", DOCX, b"PK report")]));
    let home = Home::new(&google.base(), "attedit");
    let path = path_in(&open_attachment(&home, 0));
    fs::write(&path, b"PK mine").unwrap();
    assert_eq!(home.edits(), Vec::<Value>::new());
    let id = path
        .parent()
        .unwrap()
        .file_name()
        .unwrap()
        .to_str()
        .unwrap();
    assert_eq!(refusal(&home.upload(id, "copy")), "unsupported");
    assert_eq!(drive_writes(&google), Vec::<String>::new());
}

#[test]
fn an_attachment_goes_at_24_hours_edited_or_not_and_at_sign_out() {
    let google = FakeGoogle::start(Seed::default());
    google.deliver(mail_with(vec![
        ("a.docx", DOCX, b"PK a"),
        ("b.docx", DOCX, b"PK b"),
    ]));
    let home = Home::new(&google.base(), "attgone");
    let path = path_in(&open_attachment(&home, 0));
    fs::write(&path, b"PK mine").unwrap();
    home.later(LIFE_MS);
    gws::files::sweep(&home.core);
    assert!(!path.exists(), "nothing to answer, so nothing keeps it");
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
        "unsupported",
        "nothing to go by"
    );
    for (message, attachment) in [("../x", "a"), ("18f1aa", "../x"), ("18f1aa", ""), ("", "a")] {
        let answer = home.core.run(
            "gws_mail_attachment_open",
            &json!({"message": message, "attachment": attachment}),
        );
        assert_eq!(refusal(&answer), "invalid_args", "{message} {attachment}");
    }
    let missing = home.core.run(
        "gws_mail_attachment_open",
        &json!({"message": "18f1ff", "attachment": "att-18f1ff-0"}),
    );
    assert_eq!(refusal(&missing), "not_found");
    assert!(!home.open_dir().exists());
}
