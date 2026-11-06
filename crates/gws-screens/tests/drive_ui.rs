//! The Drive screen driven as a person drives it, on a headless engine: no
//! window and no plugin. A fake host answers the tool contract from a small tree
//! of its own, and can hold an answer back or fail from a page on.
//!
//! Note `shows` is weak for layout: it counts a view with any of it on screen,
//! and inside a scroll view nothing is clipped across at all. So width tests
//! read the frames the engine really gave, through [`Frames`].

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use buwiz_app::{Bridge, BuwizError, Done, Host, Listener};
use nsh_core::prelude::*;
use nsh_core::{MountItem, NodeId};
use nsh_engine::EngineConfig;
use nsh_testing::Headless;
use serde_json::{Map, Value, json};

const WAIT: Duration = Duration::from_secs(5);
const PAGE: usize = 100;
const LONG_NAME: &str = "A very long file name for a quarterly planning spreadsheet that goes on and on without any sign of stopping before the edge of the screen 2026 final final.xlsx";
const LONG_OWNER: &str = "Maria Concepcion Fernandez de la Cruz y Villanueva Santos";
/// One pixel, as an image's first bytes must be: the screen looks at them.
const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGNgYGBgAAAABQABpfZFQAAAAABJRU5ErkJggg==";

fn file(id: &str, name: &str, kind: &str, owner: &str) -> Value {
    json!({
        "id": id, "name": name, "kind": kind, "mime": "application/x-test", "owner": owner,
        "mine": owner == "me", "changed": "2024-09-30T04:00:00Z", "size": 245760,
        "starred": false, "shared": false, "trashed": false,
    })
}

fn numbered(n: usize) -> Value {
    file(&format!("n{n}"), &format!("Item {n}"), "file", "me")
}

/// What a list holds, by view or folder. `big` is the 5,000-item folder, `some`
/// a list long enough to scroll and page.
fn all_rows(args: &Value) -> Vec<Value> {
    let folder = args["folder"].as_str();
    match (args["view"].as_str().unwrap_or_default(), folder) {
        (_, Some("clients")) => vec![
            file("acme", "Acme", "folder", "me"),
            file("plan", "Plan 2026.docx", "word", "me"),
        ],
        (_, Some("acme")) => vec![file("deep", "Deep notes", "doc", "Ana Reyes")],
        (_, Some("big")) => (0..5000).map(numbered).collect(),
        (_, Some("some")) => (0..250).map(numbered).collect(),
        (_, Some("shared-folder")) => vec![file("inside", "Inside shared", "doc", "Ben Cruz")],
        (_, Some(_)) => Vec::new(),
        ("my", None) => vec![
            file("clients", "Clients", "folder", "me"),
            file("big", "Big folder", "folder", "me"),
            file("some", "Some folder", "folder", "me"),
            file("notes", "Notes", "doc", "me"),
            file("budget", "Budget.xlsx", "spreadsheet", "Ana Reyes"),
            file("photo", "Photo.png", "image", "me"),
            file("film", "Film.mp4", "file", "Ben Cruz"),
        ],
        ("shared", None) => vec![
            file("shared-folder", "From Ben", "folder", "Ben Cruz"),
            file("shared-doc", "Shared doc", "doc", "Ben Cruz"),
        ],
        ("recent", None) => vec![file("latest", "Latest thing", "sheet", "me")],
        ("starred", None) => vec![
            file("star-folder", "Starred folder", "folder", "Ben Cruz"),
            file("star-file", "Starred file", "pdf", "me"),
        ],
        ("trash", None) => {
            let mut gone = file("gone", "Old folder", "folder", "me");
            gone["trashed"] = json!(true);
            vec![gone]
        }
        _ => Vec::new(),
    }
}

fn path_of(args: &Value) -> Value {
    match args["folder"].as_str() {
        Some("clients") => json!([{"id": "clients", "name": "Clients"}]),
        Some("acme") => {
            json!([{"id": "clients", "name": "Clients"}, {"id": "acme", "name": "Acme"}])
        }
        Some("big") => json!([{"id": "big", "name": "Big folder"}]),
        Some("some") => json!([{"id": "some", "name": "Some folder"}]),
        Some("shared-folder") => json!([{"id": "shared-folder", "name": "From Ben"}]),
        _ => json!([]),
    }
}

#[derive(Default)]
struct Fake {
    props: Mutex<Value>,
    inline: Mutex<bool>,
    status: Mutex<Option<Value>>,
    /// In place of what `all_rows` holds, whatever is asked.
    rows: Mutex<Option<Vec<Value>>>,
    /// In place of what `path_of` says.
    path: Mutex<Option<Value>>,
    about: Mutex<Option<Value>>,
    /// In place of each file's usual preview, by id.
    previews: Mutex<HashMap<String, Value>>,
    card: Mutex<Option<Value>>,
    /// Lists read from the store.
    saved: Mutex<bool>,
    seq: Mutex<i64>,
    change_status: Mutex<String>,
    /// From this skip on a list fails, with these words.
    fail_from: Mutex<Option<(usize, String)>>,
    /// A first page fails this way, until taken out.
    refuse_list: Mutex<bool>,
    /// What's edited here, as `gws_file_edits` answers.
    edits: Mutex<Vec<Value>>,
    /// In place of a good answer, by file id: to an open, to an upload.
    opens: Mutex<HashMap<String, Value>>,
    uploads: Mutex<HashMap<String, Value>>,
    /// An open's failure, as the Bot reports it.
    open_fails: Mutex<Option<BuwizError>>,
    calls: Mutex<Vec<(String, Value)>>,
    host: Mutex<Vec<(String, Value)>>,
    hold: Mutex<Vec<String>>,
    held: Mutex<Vec<(String, Value, Done)>>,
}

impl Fake {
    fn called(&self, tool: &str) -> Vec<Value> {
        let calls = self.calls.lock().unwrap();
        calls
            .iter()
            .filter(|(t, _)| t == tool)
            .map(|(_, a)| a.clone())
            .collect()
    }

    fn skips(&self) -> Vec<u64> {
        self.called("gws_drive_list")
            .iter()
            .filter(|a| a["folder"] == "big")
            .map(|a| a["skip"].as_u64().unwrap())
            .collect()
    }

    fn hold(&self, name: &str) {
        self.hold.lock().unwrap().push(name.to_owned());
    }

    fn release(&self, name: &str) {
        self.hold.lock().unwrap().retain(|n| n != name);
        let mut held = self.held.lock().unwrap();
        let at = held
            .iter()
            .position(|(n, _, _)| n == name)
            .expect("that call is held");
        let (_, answer, done) = held.remove(at);
        drop(held);
        done(Ok(answer));
    }

    fn list(&self, args: &Value) -> Value {
        let skip = args["skip"].as_u64().unwrap_or(0) as usize;
        let rows = self.rows.lock().unwrap().clone();
        let all = rows.unwrap_or_else(|| all_rows(args));
        let page: Vec<Value> = all.iter().skip(skip).take(PAGE).cloned().collect();
        let path = self.path.lock().unwrap().clone();
        json!({
            "files": page, "more": skip + PAGE < all.len(), "saved": *self.saved.lock().unwrap(),
            "path": path.unwrap_or_else(|| path_of(args)),
        })
    }

    fn details(&self, id: &str) -> Value {
        let mut row = file(id, "ignored", "doc", "me");
        row["path"] = json!([{"id": "clients", "name": "Clients"}, {"id": "acme", "name": "Acme"}]);
        row["modified_by"] = json!("Ana Reyes");
        row["created"] = json!("2024-01-01T00:00:00Z");
        row["description"] = json!(format!("About {id}"));
        row["people"] = json!([
            {"name": "Ana Reyes", "email": "ana@example.com", "role": "writer"},
            {"name": "Ben Cruz", "email": "ben@example.com", "role": "reader"},
        ]);
        row["can_download"] = json!(true);
        row
    }

    fn preview(&self, id: &str) -> Value {
        if let Some(answer) = self.previews.lock().unwrap().get(id) {
            return answer.clone();
        }
        match id {
            "photo" => json!({"kind": "image", "data_url": format!("data:image/png;base64,{PNG}")}),
            "film" => json!({"kind": "none", "why": "This kind of file has no preview."}),
            other => json!({"kind": "text", "text": format!("The words of {other}")}),
        }
    }
}

impl Host for Fake {
    fn params(&self) -> Map<String, Value> {
        let placement = if *self.inline.lock().unwrap() {
            "inline"
        } else {
            "full"
        };
        let props = self.props.lock().unwrap().clone();
        let props = if props.is_null() { json!({}) } else { props };
        let p = json!({
            "props": props, "app": {"plugin": "app.buwiz.google-workspace", "app": "drive"},
            "granted": ["buwiz.call", "buwiz.open"],
            "viewport": {"width": 1000, "height": 600, "placement": placement},
        });
        p.as_object().unwrap().clone()
    }

    fn post(&self, _: &str, _: Value) {}

    fn call(&self, method: &str, data: Value, done: Done) {
        if method != "buwiz.call" {
            self.host.lock().unwrap().push((method.to_owned(), data));
            return done(Ok(Value::Null));
        }
        let (tool, args) = (data["tool"].as_str().unwrap_or_default(), &data["args"]);
        self.calls
            .lock()
            .unwrap()
            .push((tool.to_owned(), args.clone()));
        let (name, answer) = match tool {
            "gws_status" => (
                String::new(),
                self.status.lock().unwrap().clone().unwrap_or_else(
                    || json!({"signed_in": true, "features": {"mail": true, "calendar": true, "drive": true}}),
                ),
            ),
            "gws_drive_about" => (
                String::new(),
                self.about.lock().unwrap().clone().unwrap_or_else(|| {
                    let gb = 1024_u64.pow(3);
                    json!({"used": 4 * gb, "limit": 15 * gb, "user": "owner@example.com"})
                }),
            ),
            "gws_drive_changes" => (
                String::new(),
                json!({"seq": *self.seq.lock().unwrap(), "status": *self.change_status.lock().unwrap()}),
            ),
            "gws_drive_list" => {
                let skip = args["skip"].as_u64().unwrap_or(0) as usize;
                if let Some((from, why)) = self.fail_from.lock().unwrap().clone()
                    && skip >= from
                {
                    return done(Err(BuwizError::new("tool_error", false, why)));
                }
                if *self.refuse_list.lock().unwrap() {
                    let refusal = json!({"refused": "needs_access", "message": "Drive isn't open to this sign-in."});
                    return done(Ok(refusal));
                }
                let name = args["folder"]
                    .as_str()
                    .or(args["view"].as_str())
                    .unwrap_or_default();
                let name = if skip > 0 {
                    format!("{name}@{skip}")
                } else {
                    name.to_owned()
                };
                (name, self.list(args))
            }
            "gws_drive_file" => {
                let id = args["id"].as_str().unwrap();
                (format!("file:{id}"), self.details(id))
            }
            "gws_drive_preview" => {
                let id = args["id"].as_str().unwrap();
                (format!("preview:{id}"), self.preview(id))
            }
            "gws_search_drive" => {
                let query = args["query"].as_str().unwrap().to_owned();
                let items = vec![
                    file("found", "Found file", "pdf", "Ana Reyes"),
                    file("found-folder", "Found folder", "folder", "me"),
                ];
                (
                    format!("search:{query}"),
                    json!({"items": items, "untrusted": "File names are other people's words."}),
                )
            }
            "gws_drive_card" => (
                String::new(),
                self.card.lock().unwrap().clone().unwrap_or_else(a_card),
            ),
            "gws_file_open" => {
                if let Some(e) = self.open_fails.lock().unwrap().clone() {
                    return done(Err(e));
                }
                let id = args["id"].as_str().unwrap();
                let answer = self.opens.lock().unwrap().get(id).cloned();
                (
                    format!("open:{id}"),
                    answer.unwrap_or_else(|| json!({"opened": true, "name": id, "kind": "word"})),
                )
            }
            "gws_file_edits" => (
                "edits".to_owned(),
                json!({"files": *self.edits.lock().unwrap()}),
            ),
            "gws_file_upload" => {
                let id = args["id"].as_str().unwrap();
                let refused = self.uploads.lock().unwrap().get(id).cloned();
                let answer = refused.unwrap_or_else(|| {
                    self.edits.lock().unwrap().retain(|e| e["id"] != id);
                    json!({"uploaded": true, "mode": args["mode"], "id": id})
                });
                (format!("upload:{id}"), answer)
            }
            "gws_file_discard" => {
                let id = args["id"].as_str().unwrap();
                self.edits.lock().unwrap().retain(|e| e["id"] != id);
                (format!("discard:{id}"), json!({"discarded": true}))
            }
            other => {
                return done(Err(BuwizError::new(
                    "invalid",
                    false,
                    format!("not faked: {other}"),
                )));
            }
        };
        if self.hold.lock().unwrap().contains(&name) {
            self.held.lock().unwrap().push((name, answer, done));
        } else {
            done(Ok(answer));
        }
    }

    fn listen(&self, _: Listener) {}

    fn fail(&self, message: &str) {
        panic!("the screen failed: {message}");
    }
}

fn a_card() -> Value {
    let rows: Vec<Value> = (0..7)
        .map(|n| {
            file(
                &format!("c{n}"),
                &format!("Card file {n}"),
                "pdf",
                "Ana Reyes",
            )
        })
        .collect();
    json!({"rows": rows, "saved": false})
}

/// A view's parent, its left edge there and its width.
type Placed = (Option<NodeId>, f32, f32);

/// Where the engine put every view, read from what it told the platform, and
/// each image it was asked to show.
#[derive(Clone, Default)]
struct Frames {
    views: Arc<Mutex<HashMap<NodeId, Placed>>>,
    images: Arc<Mutex<Vec<String>>>,
    /// Each text's view, by what it reads now.
    texts: Arc<Mutex<HashMap<NodeId, String>>>,
}

impl Frames {
    fn watch(&self, app: &mut Headless) {
        let (views, images, texts) = (self.views.clone(), self.images.clone(), self.texts.clone());
        app.observe(move |_, batch| {
            let mut views = views.lock().unwrap();
            let mut texts = texts.lock().unwrap();
            for item in &batch.items {
                match item {
                    MountItem::Insert { parent, child, .. } => {
                        views.entry(*child).or_default().0 = Some(*parent);
                    }
                    MountItem::Frame { id, frame } => {
                        let view = views.entry(*id).or_default();
                        (view.1, view.2) = (frame.origin.x.0, frame.size.width.0);
                    }
                    MountItem::Text { id, text, .. } => {
                        texts.insert(*id, text.text.clone());
                    }
                    MountItem::Image {
                        source: Some(source),
                        ..
                    } => images.lock().unwrap().push(source.uri.to_string()),
                    _ => {}
                }
            }
        });
    }

    /// The left and right edges of every text reading `line`.
    fn text_edges(&self, line: &str) -> Vec<(f32, f32)> {
        let ids: Vec<NodeId> = {
            let texts = self.texts.lock().unwrap();
            let found = texts.iter().filter(|(_, t)| t.as_str() == line);
            found.map(|(id, _)| *id).collect()
        };
        ids.into_iter().map(|id| self.edges(id)).collect()
    }

    /// How wide the one text reading `line` is, and the view it's in.
    fn width_in_parent(&self, line: &str) -> (f32, f32) {
        let id = {
            let texts = self.texts.lock().unwrap();
            let mut found = texts.iter().filter(|(_, t)| t.as_str() == line);
            *found.next().expect("that text").0
        };
        let views = self.views.lock().unwrap();
        let (parent, _, width) = views[&id];
        (width, views[&parent.expect("a parent")].2)
    }

    /// `id`'s left and right edges on screen.
    fn edges(&self, id: NodeId) -> (f32, f32) {
        let views = self.views.lock().unwrap();
        let (mut parent, mut left, width) = *views.get(&id).expect("a view with a frame");
        while let Some(at) = parent {
            let Some((up, x, _)) = views.get(&at) else {
                break;
            };
            left += x;
            parent = *up;
        }
        (left, left + width)
    }
}

fn open_app(fake: Arc<Fake>, width: f32, height: f32) -> (Headless, Frames) {
    let def = gws_screens::screens()
        .iter()
        .find(|s| s.id == "gws.drive")
        .expect("Drive");
    let root = (def.build)(Bridge::new(fake));
    let mut app = Headless::start(EngineConfig::new(root)).expect("the engine starts");
    let frames = Frames::default();
    frames.watch(&mut app);
    app.set_viewport(size(px(width), px(height)))
        .expect("the viewport sets");
    (app, frames)
}

fn start_sized(width: f32, height: f32) -> (Headless, Arc<Fake>, Frames) {
    let fake = Arc::new(Fake::default());
    let (mut app, frames) = open_app(fake.clone(), width, height);
    app.wait_for_within("text:Notes", WAIT)
        .expect("My Drive shows");
    settle(&mut app);
    (app, fake, frames)
}

fn start() -> (Headless, Arc<Fake>) {
    let (app, fake, _) = start_sized(1000.0, 600.0);
    (app, fake)
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

/// Presses the row showing `name`.
fn open(app: &mut Headless, name: &str) {
    press(app, &format!("label:Open this item >> text:{name}"));
}

/// Shows the details of the row showing `name`: a press picks it, and `i` is
/// the same as its Details press, which has its own test.
fn details(app: &mut Headless, name: &str) {
    open(app, name);
    key(app, "i");
}

fn crumb(app: &mut Headless, name: &str) {
    press(app, &format!("label:Open this folder >> text:{name}"));
}

fn big_folder(app: &mut Headless) {
    open(app, "Big folder");
    app.wait_for_within("text:Item 0", WAIT)
        .expect("the folder shows");
}

fn some_folder(app: &mut Headless) {
    open(app, "Some folder");
    app.wait_for_within("text:Item 0", WAIT)
        .expect("the folder shows");
}

/// The numbers of the rows on screen.
fn shown(app: &Headless) -> BTreeSet<usize> {
    let texts = app.screen().texts();
    texts
        .iter()
        .filter_map(|t| t.strip_prefix("Item ")?.parse().ok())
        .collect()
}

/// The rows on screen, none of them twice.
fn once_each(app: &Headless) -> BTreeSet<usize> {
    let texts = app.screen().texts();
    let rows: Vec<&&str> = texts.iter().filter(|t| t.starts_with("Item ")).collect();
    let set = shown(app);
    assert_eq!(set.len(), rows.len(), "a row twice in {rows:?}");
    set
}

/// The rows on screen run on from each other. A cell that kept an old row
/// shows up as a number from somewhere else; one just off the screen, waiting
/// to be reused, can leave a step of a few.
fn assert_unbroken(app: &Headless) {
    let shown: Vec<usize> = shown(app).into_iter().collect();
    assert!(!shown.is_empty(), "rows on screen");
    for pair in shown.windows(2) {
        assert!(pair[1] - pair[0] <= 4, "a gap in {shown:?}");
    }
}

fn selected(app: &Headless) -> Vec<usize> {
    let rows = app.screen().count("label:Open this item").unwrap();
    (0..rows)
        .filter(|n| {
            app.screen()
                .selected(&format!("label:Open this item >> nth={n}"))
                .unwrap()
        })
        .collect()
}

fn last_of(fake: &Fake, tool: &str) -> Value {
    fake.called(tool).last().expect("a call").clone()
}

// --- Views and folders -------------------------------------------------------

#[test]
fn each_view_in_the_column_loads_its_own_rows() {
    let (mut app, fake) = start();
    for (name, shows, gone) in [
        ("Shared", "Shared doc", "Notes"),
        ("Recent", "Latest thing", "Shared doc"),
        ("Starred", "Starred file", "Latest thing"),
        ("Trash", "Old folder", "Starred file"),
        ("My Drive", "Notes", "Old folder"),
    ] {
        press(&mut app, &format!("label:{name}"));
        assert!(app.screen().shows(shows), "{name}");
        assert!(!app.screen().shows(gone), "{name} kept {gone}");
    }
    let views: Vec<Value> = fake
        .called("gws_drive_list")
        .iter()
        .map(|a| a["view"].clone())
        .collect();
    assert_eq!(views, ["my", "shared", "recent", "starred", "trash", "my"]);
}

#[test]
fn the_list_has_its_columns_and_a_folder_comes_as_a_folder() {
    let (app, _) = start();
    for line in [
        "Name",
        "Owner",
        "Changed",
        "Size",
        "Folder",
        "Doc",
        "Spreadsheet",
    ] {
        assert!(app.screen().shows(line), "{line}");
    }
    assert!(app.screen().shows("Ana Reyes"));
    assert!(app.screen().shows("240 KB"));
    assert!(app.screen().shows("Sep 30, 2024"));
    assert!(app.screen().shows("me"));
}

#[test]
fn a_folder_opens_and_each_part_of_the_breadcrumb_goes_back() {
    let (mut app, fake) = start();
    open(&mut app, "Clients");
    assert!(app.screen().shows("Plan 2026.docx"));
    assert!(!app.screen().shows("Notes"));
    open(&mut app, "Acme");
    assert!(app.screen().shows("Deep notes"));
    assert_eq!(last_of(&fake, "gws_drive_list")["folder"], "acme");
    for name in ["My Drive", "Clients", "Acme"] {
        assert!(
            app.screen()
                .matches(&format!("label:Open this folder >> text:{name}"))
                .unwrap(),
            "{name}"
        );
    }

    crumb(&mut app, "Clients");
    assert!(app.screen().shows("Plan 2026.docx"));
    assert!(!app.screen().shows("Deep notes"));
    assert!(
        !app.screen()
            .matches("label:Open this folder >> text:Acme")
            .unwrap(),
        "the path got shorter"
    );
    crumb(&mut app, "My Drive");
    assert!(app.screen().shows("Notes"));
    let last = last_of(&fake, "gws_drive_list");
    assert!(last.get("folder").is_none(), "{last}");
}

#[test]
fn the_breadcrumb_is_what_the_tool_says_once_it_answers() {
    let (mut app, fake) = start();
    *fake.path.lock().unwrap() = Some(json!([
        {"id": "x1", "name": "Really here"},
        {"id": "clients", "name": "Clients"},
    ]));
    open(&mut app, "Clients");
    assert!(app.screen().shows("Really here"));
    crumb(&mut app, "Really here");
    assert_eq!(last_of(&fake, "gws_drive_list")["folder"], "x1");
}

#[test]
fn a_folder_from_a_view_with_no_folders_opens_in_one_that_has_them() {
    let (mut app, fake) = start();
    press(&mut app, "label:Starred");
    open(&mut app, "Starred folder");
    let last = last_of(&fake, "gws_drive_list");
    assert_eq!(last["view"], "shared", "it's someone else's");
    assert_eq!(last["folder"], "star-folder");
}

#[test]
fn a_folder_in_the_trash_shows_its_details_and_doesnt_open() {
    let (mut app, fake) = start();
    press(&mut app, "label:Trash");
    open(&mut app, "Old folder");
    assert!(app.screen().matches("label:Close these details").unwrap());
    assert_eq!(last_of(&fake, "gws_drive_list")["view"], "trash");
}

#[test]
fn the_screen_opens_on_the_view_folder_or_search_it_was_given() {
    let fake = Arc::new(Fake::default());
    *fake.props.lock().unwrap() = json!({"view": "shared"});
    let (mut app, _) = open_app(fake.clone(), 1000.0, 600.0);
    app.wait_for_within("text:Shared doc", WAIT)
        .expect("Shared");

    let fake = Arc::new(Fake::default());
    *fake.props.lock().unwrap() = json!({"folder": "clients"});
    let (mut app, _) = open_app(fake.clone(), 1000.0, 600.0);
    app.wait_for_within("text:Plan 2026.docx", WAIT)
        .expect("the folder");
    app.wait_for_within("text:Clients", WAIT)
        .expect("its breadcrumb");
    assert_eq!(fake.called("gws_drive_list")[0]["view"], "my");

    let fake = Arc::new(Fake::default());
    *fake.props.lock().unwrap() = json!({"search": "tkt1"});
    *fake.rows.lock().unwrap() = Some(vec![file("t1", "From the chat", "doc", "me")]);
    let (mut app, _) = open_app(fake.clone(), 1000.0, 600.0);
    app.wait_for_within("text:From the chat", WAIT)
        .expect("the search");
    assert_eq!(fake.called("gws_drive_list")[0]["search"], "tkt1");
    assert!(app.screen().shows("Search results"));

    let fake = Arc::new(Fake::default());
    *fake.props.lock().unwrap() = json!({"view": "nonsense"});
    let (mut app, _) = open_app(fake.clone(), 1000.0, 600.0);
    app.wait_for_within("text:Notes", WAIT)
        .expect("a view it doesn't know is My Drive");
}

#[test]
fn storage_says_of_how_much_and_only_whats_used_with_no_limit() {
    let (app, _) = start();
    assert!(app.screen().shows("4 of 15 GB"));

    let fake = Arc::new(Fake::default());
    *fake.about.lock().unwrap() = Some(json!({"used": 4 * 1024_u64.pow(3), "limit": null}));
    let (mut app, _) = open_app(fake, 1000.0, 600.0);
    app.wait_for_within("text:4 GB used", WAIT)
        .expect("it says");
}

#[test]
fn a_store_change_shows_within_the_check() {
    let (mut app, fake) = start();
    *fake.rows.lock().unwrap() = Some(vec![file("x", "Added elsewhere", "doc", "me")]);
    *fake.seq.lock().unwrap() += 1;
    app.wait_for_within("text:Added elsewhere", Duration::from_secs(20))
        .expect("the view reads it again");
}

// --- Late answers ------------------------------------------------------------

#[test]
fn a_late_view_answer_does_not_replace_a_newer_one() {
    let (mut app, fake) = start();
    fake.hold("shared");
    press(&mut app, "label:Shared");
    press(&mut app, "label:Recent");
    assert!(app.screen().shows("Latest thing"));

    fake.release("shared");
    settle(&mut app);
    assert!(app.screen().shows("Latest thing"));
    assert!(!app.screen().shows("Shared doc"), "Shared came late");
}

#[test]
fn a_late_page_of_a_folder_left_adds_nothing_to_the_next() {
    let (mut app, fake, _) = start_sized(1000.0, 600.0);
    fake.hold("some@100");
    some_folder(&mut app);
    app.scroll("text:Item 0", px(4600.0)).expect("it scrolls");
    settle(&mut app);
    assert!(
        fake.called("gws_drive_list")
            .iter()
            .any(|a| a["skip"] == 100)
    );
    press(&mut app, "label:Recent");

    fake.release("some@100");
    settle(&mut app);
    assert!(app.screen().shows("Latest thing"));
    assert!(shown(&app).is_empty(), "{:?}", shown(&app));
    assert_eq!(app.screen().count("label:Open this item").unwrap(), 1);
}

#[test]
fn a_late_details_answer_does_not_replace_the_open_file() {
    let (mut app, fake) = start();
    fake.hold("file:notes");
    fake.hold("preview:notes");
    details(&mut app, "Notes");
    details(&mut app, "Budget.xlsx");
    assert!(app.screen().shows("About budget"));

    fake.release("file:notes");
    fake.release("preview:notes");
    settle(&mut app);
    assert!(app.screen().shows("About budget"));
    assert!(app.screen().shows("The words of budget"));
    assert!(!app.screen().shows("About notes"));
    assert!(!app.screen().shows("The words of notes"));
}

#[test]
fn a_details_answer_after_closing_opens_nothing() {
    let (mut app, fake) = start();
    fake.hold("file:notes");
    details(&mut app, "Notes");
    press(&mut app, "label:Close these details");
    fake.release("file:notes");
    settle(&mut app);
    assert!(!app.screen().shows("About notes"));
    assert!(!app.screen().matches("label:Close these details").unwrap());
}

// --- Recycled rows -----------------------------------------------------------

#[test]
fn rows_stay_right_through_a_scroll_and_a_view_change() {
    let (mut app, _) = start();
    some_folder(&mut app);
    app.scroll("text:Item 0", px(2500.0)).expect("it scrolls");
    settle(&mut app);
    assert_unbroken(&app);
    assert!(!app.screen().shows("Item 0"), "scrolled past the top");

    press(&mut app, "label:Shared");
    assert!(app.screen().shows("Shared doc"));
    assert_eq!(app.screen().count("label:Open this item").unwrap(), 2);
    assert!(shown(&app).is_empty(), "{:?}", shown(&app));

    press(&mut app, "label:My Drive");
    some_folder(&mut app);
    assert_unbroken(&app);
    assert!(app.screen().shows("Item 0"), "a new list starts at its top");
}

#[test]
fn a_press_after_a_scroll_opens_the_row_it_shows() {
    let (mut app, fake) = start();
    some_folder(&mut app);
    app.scroll("text:Item 0", px(2500.0)).expect("it scrolls");
    settle(&mut app);
    let n = *shown(&app).iter().nth(3).expect("rows on screen");
    press(&mut app, &format!("text:Item {n}"));
    assert_eq!(last_of(&fake, "gws_file_open")["id"], format!("n{n}"));
    assert_eq!(selected(&app).len(), 1);
}

#[test]
fn a_switch_to_grid_and_back_keeps_the_place_and_the_right_rows() {
    let (mut app, fake) = start();
    some_folder(&mut app);
    app.scroll("text:Item 0", px(2500.0)).expect("it scrolls");
    settle(&mut app);
    let top = *shown(&app).first().unwrap();

    press(&mut app, "label:Grid");
    assert_unbroken(&app);
    let cards = shown(&app);
    assert!(
        cards.contains(&(top + 12)),
        "the row at the top was about {}, the grid shows {cards:?}",
        top + 12
    );
    let n = *cards.iter().nth(3).expect("cards on screen");
    press(&mut app, &format!("text:Item {n}"));
    assert_eq!(last_of(&fake, "gws_file_open")["id"], format!("n{n}"));

    press(&mut app, "label:List");
    assert_unbroken(&app);
    let rows = shown(&app);
    assert!(rows.contains(&(top + 12)), "back near {top}: {rows:?}");
}

// --- Paging ------------------------------------------------------------------

/// Walked by key down a very wide grid, a line of cards a key: a scroll here
/// moves ten points a frame, which over a list's 240,000 points is minutes.
/// Other tests page the list by scrolling, through the same code.
#[test]
fn a_5000_item_folder_pages_with_nothing_skipped_or_doubled() {
    let (mut app, fake, _) = start_sized(4000.0, 900.0);
    press(&mut app, "label:Grid");
    big_folder(&mut app);
    let mut seen = BTreeSet::new();
    let mut steps = 0;
    while !seen.contains(&4999) {
        seen.extend(once_each(&app));
        steps += 1;
        assert!(steps < 400, "stuck at {} rows", seen.len());
        app.press_key("down").expect("the key presses");
        app.wait(Duration::from_millis(40)).expect("time passes");
    }
    seen.extend(once_each(&app));
    assert_eq!(seen, (0..5000).collect::<BTreeSet<_>>());
    assert_eq!(
        fake.skips(),
        (0..50).map(|p| p * 100).collect::<Vec<u64>>(),
        "each page asked for once, in order"
    );

    // the whole list, in the other view, still opens the row it shows
    press(&mut app, "label:List");
    assert_unbroken(&app);
    let n = *shown(&app).iter().last().expect("rows on screen");
    assert!(
        n > 4600,
        "the top of that last screenful of cards, not the top of the list: {n}"
    );
    press(&mut app, &format!("text:Item {n}"));
    assert_eq!(last_of(&fake, "gws_file_open")["id"], format!("n{n}"));
}

#[test]
fn a_row_read_from_a_later_page_opens_itself() {
    let (mut app, fake) = start();
    some_folder(&mut app);
    for _ in 0..4 {
        // past the end for now, until the next page comes
        let _ = app.scroll("text:/^Item /", px(4600.0));
        settle(&mut app);
    }
    let n = *shown(&app).iter().nth(2).expect("rows on screen");
    assert!(n >= 100, "row {n} is from the second page or later");
    press(&mut app, &format!("text:Item {n}"));
    assert_eq!(last_of(&fake, "gws_file_open")["id"], format!("n{n}"));
}

#[test]
fn a_doubled_row_between_pages_shows_once() {
    let (mut app, fake) = start();
    let mut rows: Vec<Value> = (0..150).map(numbered).collect();
    // a file moved up between the two reads: page two starts with it again
    rows.insert(100, numbered(99));
    *fake.rows.lock().unwrap() = Some(rows);
    some_folder(&mut app);
    for _ in 0..101 {
        app.press_key("down").expect("the key presses");
    }
    settle(&mut app);
    key(&mut app, "enter");
    assert_eq!(
        last_of(&fake, "gws_file_open")["id"],
        "n100",
        "the 101st row"
    );
}

// --- Failures ----------------------------------------------------------------

#[test]
fn a_failure_on_a_later_page_keeps_the_rows_and_says_so() {
    let (mut app, fake) = start();
    *fake.fail_from.lock().unwrap() = Some((100, "Google Drive didn't answer.".to_owned()));
    some_folder(&mut app);
    app.scroll("text:/^Item /", px(4600.0)).expect("it scrolls");
    settle(&mut app);
    let before = shown(&app);
    assert!(!before.is_empty());
    assert_eq!(before, shown(&app));
    assert!(
        app.screen()
            .shows("Showing the 100 items already here. Google Drive didn't answer."),
        "{:?}",
        app.screen().texts()
    );
    assert_unbroken(&app);

    // the press still works on what stayed
    let n = *before.iter().nth(1).unwrap();
    press(&mut app, &format!("text:Item {n}"));
    assert_eq!(last_of(&fake, "gws_file_open")["id"], format!("n{n}"));

    *fake.fail_from.lock().unwrap() = None;
    press(&mut app, "label:Try again");
    assert!(!app.screen().matches("label:Try again").unwrap());
    let last = last_of(&fake, "gws_drive_list");
    assert_eq!(last["skip"], 100, "from where it stopped, not from the top");
}

#[test]
fn access_revoked_mid_list_keeps_the_rows_and_says_to_sign_in() {
    let (mut app, fake) = start();
    *fake.fail_from.lock().unwrap() = Some((100, "Sign in to Google first.".to_owned()));
    some_folder(&mut app);
    app.scroll("text:/^Item /", px(4600.0)).expect("it scrolls");
    settle(&mut app);
    assert!(!shown(&app).is_empty(), "the rows stayed");
    assert!(
        app.screen().shows(
            "Showing the 100 items already here. Your Google sign-in stopped working. Sign in again on this app's page."
        ),
        "{:?}",
        app.screen().texts()
    );
}

#[test]
fn a_first_page_that_fails_says_why_and_can_be_tried_again() {
    let (mut app, fake) = start();
    *fake.fail_from.lock().unwrap() = Some((0, "Google Drive didn't answer.".to_owned()));
    press(&mut app, "label:Shared");
    assert!(app.screen().shows("Google Drive didn't answer."));
    assert!(
        !app.screen().shows("Notes"),
        "My Drive's rows don't stay under Shared"
    );

    *fake.fail_from.lock().unwrap() = None;
    press(&mut app, "label:Try again");
    assert!(app.screen().shows("Shared doc"));
}

#[test]
fn a_refusal_that_comes_as_an_answer_is_said_as_a_failure() {
    let (mut app, fake) = start();
    *fake.refuse_list.lock().unwrap() = true;
    press(&mut app, "label:Shared");
    assert!(app.screen().shows("Drive isn't open to this sign-in."));
    assert_eq!(app.screen().count("label:Open this item").unwrap(), 0);
}

#[test]
fn a_sign_in_lost_with_nothing_on_screen_says_so() {
    let (mut app, fake) = start();
    *fake.fail_from.lock().unwrap() = Some((0, "Sign in to Google first.".to_owned()));
    press(&mut app, "label:Shared");
    app.wait_for_within("text:Connect your Google account on this app's page.", WAIT)
        .expect("it says so");
}

#[test]
fn saved_files_say_they_are_saved() {
    let fake = Arc::new(Fake::default());
    *fake.saved.lock().unwrap() = true;
    let (mut app, _) = open_app(fake, 1000.0, 600.0);
    app.wait_for_within("text:Offline. Showing your saved files.", WAIT)
        .expect("it says so");
    assert!(app.screen().shows("Notes"));
}

#[test]
fn a_new_list_forgets_a_failure_and_the_saved_line() {
    let (mut app, fake) = start();
    *fake.fail_from.lock().unwrap() = Some((100, "No.".to_owned()));
    some_folder(&mut app);
    app.scroll("text:/^Item /", px(4600.0)).expect("it scrolls");
    settle(&mut app);
    assert!(app.screen().matches("label:Try again").unwrap());
    press(&mut app, "label:Shared");
    assert!(!app.screen().matches("label:Try again").unwrap());
}

fn opened_with_status(status: Value) -> (Headless, Arc<Fake>) {
    let fake = Arc::new(Fake::default());
    *fake.status.lock().unwrap() = Some(status);
    let (mut app, _) = open_app(fake.clone(), 1000.0, 600.0);
    settle(&mut app);
    (app, fake)
}

#[test]
fn signed_out_says_so_and_reads_nothing() {
    let (app, fake) = opened_with_status(json!({"signed_in": false}));
    assert!(
        app.screen()
            .shows("Connect your Google account on this app's page.")
    );
    assert!(fake.called("gws_drive_list").is_empty());
    assert!(!app.screen().matches("label:My Drive").unwrap());
}

#[test]
fn a_sign_in_that_doesnt_cover_drive_says_so() {
    let (app, fake) = opened_with_status(json!({"signed_in": true, "features": {"drive": false}}));
    assert!(
        app.screen()
            .shows("Your Google sign-in doesn't cover Drive yet.")
    );
    assert!(fake.called("gws_drive_list").is_empty());
}

#[test]
fn scopes_not_on_record_get_to_try() {
    let (app, fake) = opened_with_status(json!({"signed_in": true, "features": null}));
    assert!(app.screen().matches("label:My Drive").unwrap());
    assert_eq!(fake.called("gws_drive_list").len(), 1);
}

#[test]
fn keys_do_nothing_on_a_screen_with_no_drive() {
    let (mut app, fake) = opened_with_status(json!({"signed_in": false}));
    for k in ["down", "enter", "/"] {
        key(&mut app, k);
    }
    assert!(fake.called("gws_drive_file").is_empty());
}

// --- Details and preview -----------------------------------------------------

#[test]
fn a_file_shows_its_details_beside_the_list() {
    let (mut app, fake) = start();
    details(&mut app, "Budget.xlsx");
    for line in [
        "Kind",
        "Owner",
        "Changed",
        "Size",
        "Path",
        "Clients / Acme",
        "Who has access",
        "Ana Reyes <ana@example.com>",
        "Can edit",
        "Ben Cruz <ben@example.com>",
        "Can view",
        "Description",
        "About budget",
        "The words of budget",
    ] {
        assert!(
            app.screen().shows(line),
            "{line}: {:?}",
            app.screen().texts()
        );
    }
    assert!(app.screen().shows("Notes"), "the list stays beside it");
    assert_eq!(last_of(&fake, "gws_drive_preview")["id"], "budget");
    press(&mut app, "label:Close these details");
    assert!(!app.screen().shows("About budget"));
}

#[test]
fn a_text_preview_is_selectable_plain_text() {
    let (mut app, fake) = start();
    fake.previews.lock().unwrap().insert(
        "notes".to_owned(),
        json!({"kind": "text", "text": "<b>not bold</b> <img src=https://example.com/x>"}),
    );
    details(&mut app, "Notes");
    let line = "text:<b>not bold</b> <img src=https://example.com/x>";
    assert!(app.screen().matches(line).unwrap(), "word for word");
    assert!(app.screen().in_selectable_view(line).unwrap());
}

#[test]
fn an_image_preview_shows_through_its_data_url_and_only_that() {
    let (mut app, _, frames) = start_sized(1000.0, 600.0);
    details(&mut app, "Photo.png");
    assert!(app.screen().matches("label:Preview of this file").unwrap());
    let images = frames.images.lock().unwrap().clone();
    assert_eq!(images.len(), 1, "{images:?}");
    assert!(images[0].starts_with("data:image/png"), "{}", images[0]);
}

#[test]
fn no_preview_says_why() {
    let (mut app, _) = start();
    details(&mut app, "Film.mp4");
    assert!(app.screen().shows("This kind of file has no preview."));
    assert!(!app.screen().matches("label:Preview of this file").unwrap());
}

#[test]
fn a_data_url_that_isnt_an_allowed_image_shows_nothing() {
    let svg = "PHN2ZyB4bWxucz0iaHR0cDovL3d3dy53My5vcmcvMjAwMC9zdmciLz4=";
    for bad in [
        "https://example.com/tracker.png".to_owned(),
        format!("data:image/svg+xml;base64,{svg}"),
        format!("data:text/html;base64,{svg}"),
        // says PNG, isn't one
        format!("data:image/png;base64,{svg}"),
        "javascript:alert(1)".to_owned(),
        String::new(),
    ] {
        let (mut app, fake, frames) = start_sized(1000.0, 600.0);
        fake.previews.lock().unwrap().insert(
            "photo".to_owned(),
            json!({"kind": "image", "data_url": bad.clone()}),
        );
        details(&mut app, "Photo.png");
        assert!(app.screen().shows("About photo"), "the rest still shows");
        assert!(
            !app.screen().matches("label:Preview of this file").unwrap(),
            "{bad}"
        );
        assert!(frames.images.lock().unwrap().is_empty(), "{bad}");
        assert!(!app.screen().shows("Reading…"), "{bad}: it stopped reading");
    }
}

#[test]
fn a_preview_that_fails_says_why_and_the_details_stay() {
    let (mut app, fake) = start();
    fake.previews.lock().unwrap().insert(
        "notes".to_owned(),
        json!({"refused": "too_big", "message": "This file is too big to preview."}),
    );
    details(&mut app, "Notes");
    assert!(app.screen().shows("This file is too big to preview."));
    assert!(app.screen().shows("About notes"));
}

// --- Grid --------------------------------------------------------------------

#[test]
fn the_grid_shows_cards_with_their_kind_and_opens_like_the_list() {
    let (mut app, fake) = start();
    press(&mut app, "label:Grid");
    assert!(!app.screen().shows("Name"), "no column headings on cards");
    for line in [
        "Clients",
        "Notes",
        "Budget.xlsx",
        "Folder",
        "Doc",
        "Spreadsheet",
        "Ana Reyes",
    ] {
        assert!(app.screen().shows(line), "{line}");
    }
    open(&mut app, "Clients");
    assert!(app.screen().shows("Plan 2026.docx"));
    assert!(app.screen().shows("Word"));
    details(&mut app, "Plan 2026.docx");
    assert_eq!(last_of(&fake, "gws_drive_file")["id"], "plan");
}

#[test]
fn the_grid_pages_and_lays_cards_side_by_side() {
    let (mut app, fake, frames) = start_sized(1000.0, 600.0);
    press(&mut app, "label:Grid");
    some_folder(&mut app);
    let left = |n| {
        let id = app
            .screen()
            .press_target(&format!("label:Open this item >> text:Item {n}"))
            .unwrap()
            .expect("a card");
        frames.edges(id).0
    };
    assert!(left(1) > left(0), "the next card is beside it");

    for _ in 0..6 {
        let _ = app.scroll("text:/^Item /", px(1000.0));
        settle(&mut app);
    }
    assert_unbroken(&app);
    assert!(
        fake.called("gws_drive_list")
            .iter()
            .any(|a| a["skip"] == 100),
        "the second page came"
    );
}

// --- Search ------------------------------------------------------------------

#[test]
fn a_search_shows_results_and_a_view_press_leaves_them() {
    let (mut app, fake) = start();
    app.type_text("label:Search Google Drive", "plan \"x\"")
        .expect("it takes the words");
    app.submit("label:Search Google Drive").expect("it submits");
    settle(&mut app);
    assert_eq!(last_of(&fake, "gws_search_drive")["query"], "plan \"x\"");
    assert!(app.screen().shows("Found file"));
    assert!(app.screen().shows("Search results"));
    assert!(!app.screen().shows("Notes"));

    press(&mut app, "label:Shared");
    assert!(app.screen().shows("Shared doc"));
    assert!(!app.screen().shows("Search results"));
    assert_eq!(
        app.screen()
            .input_text("label:Search Google Drive")
            .unwrap()
            .unwrap(),
        ""
    );
}

#[test]
fn a_late_search_does_not_replace_a_view_pressed_after_it() {
    let (mut app, fake) = start();
    fake.hold("search:plan");
    app.type_text("label:Search Google Drive", "plan")
        .expect("it takes the words");
    app.submit("label:Search Google Drive").expect("it submits");
    settle(&mut app);
    press(&mut app, "label:Recent");
    fake.release("search:plan");
    settle(&mut app);
    assert!(app.screen().shows("Latest thing"));
    assert!(!app.screen().shows("Found file"));
}

#[test]
fn an_empty_search_goes_back_to_the_view() {
    let (mut app, _) = start();
    app.type_text("label:Search Google Drive", "plan")
        .expect("it types");
    app.submit("label:Search Google Drive").expect("it submits");
    settle(&mut app);
    app.clear("label:Search Google Drive").expect("it clears");
    app.submit("label:Search Google Drive").expect("it submits");
    settle(&mut app);
    assert!(app.screen().shows("Notes"));
    assert!(!app.screen().shows("Search results"));
}

#[test]
fn a_folder_in_the_results_opens() {
    let (mut app, fake) = start();
    app.type_text("label:Search Google Drive", "plan")
        .expect("it types");
    app.submit("label:Search Google Drive").expect("it submits");
    settle(&mut app);
    open(&mut app, "Found folder");
    assert_eq!(last_of(&fake, "gws_drive_list")["folder"], "found-folder");
    assert!(!app.screen().shows("Search results"));
}

// --- Keys --------------------------------------------------------------------

#[test]
fn arrows_move_the_selection_and_shift_arrows_grow_it() {
    let (mut app, _) = start();
    key(&mut app, "down");
    assert_eq!(selected(&app), [0], "the first row");
    key(&mut app, "down");
    key(&mut app, "down");
    assert_eq!(selected(&app), [2]);
    key(&mut app, "up");
    assert_eq!(selected(&app), [1]);

    key(&mut app, "shift+down");
    key(&mut app, "shift+down");
    assert_eq!(selected(&app), [1, 2, 3]);
    key(&mut app, "shift+up");
    assert_eq!(selected(&app), [1, 2], "back toward where it began");
    key(&mut app, "shift+up");
    key(&mut app, "shift+up");
    assert_eq!(selected(&app), [0, 1], "and past it the other way");
    key(&mut app, "down");
    assert_eq!(selected(&app), [1], "a plain move leaves one");
}

#[test]
fn enter_opens_a_folder_and_opens_a_file() {
    let (mut app, fake) = start();
    key(&mut app, "down");
    key(&mut app, "enter");
    assert!(app.screen().shows("Plan 2026.docx"), "Clients opened");
    assert!(selected(&app).is_empty(), "nothing picked in a new folder");

    key(&mut app, "down");
    key(&mut app, "down");
    key(&mut app, "enter");
    assert_eq!(last_of(&fake, "gws_file_open")["id"], "plan");
    key(&mut app, "i");
    assert_eq!(last_of(&fake, "gws_drive_file")["id"], "plan");
    assert!(app.screen().shows("About plan"));
}

#[test]
fn enter_with_nothing_picked_does_nothing() {
    let (mut app, fake) = start();
    key(&mut app, "enter");
    assert_eq!(fake.called("gws_drive_list").len(), 1);
    assert!(fake.called("gws_drive_file").is_empty());
}

#[test]
fn a_press_moves_the_selection_so_keys_go_on_from_it() {
    let (mut app, _) = start();
    details(&mut app, "Notes");
    assert_eq!(selected(&app), [3]);
    press(&mut app, "label:Close these details");
    key(&mut app, "down");
    assert_eq!(selected(&app), [4]);
}

#[test]
fn in_the_grid_arrows_go_across_and_by_lines() {
    let (mut app, _) = start();
    press(&mut app, "label:Grid");
    key(&mut app, "right");
    key(&mut app, "right");
    assert_eq!(selected(&app), [1]);
    key(&mut app, "left");
    assert_eq!(selected(&app), [0]);
    key(&mut app, "down");
    // 1000 wide less the column is about four across
    let row = selected(&app)[0];
    assert!(row >= 3, "down a line, not one card: {row}");
    key(&mut app, "up");
    assert_eq!(selected(&app), [0]);
}

#[test]
fn left_and_right_do_nothing_in_the_list() {
    let (mut app, _) = start();
    key(&mut app, "down");
    key(&mut app, "right");
    assert_eq!(selected(&app), [0]);
}

#[test]
fn a_key_move_past_the_screen_keeps_the_row_in_view_and_pages() {
    let (mut app, fake) = start();
    some_folder(&mut app);
    for _ in 0..30 {
        app.press_key("down").expect("the key presses");
    }
    settle(&mut app);
    assert!(
        app.screen().shows("Item 29"),
        "the picked row is on screen: {:?}",
        shown(&app)
    );
    for _ in 0..60 {
        app.press_key("down").expect("the key presses");
    }
    settle(&mut app);
    assert!(app.screen().shows("Item 89"));
    assert!(
        fake.called("gws_drive_list")
            .iter()
            .any(|a| a["skip"] == 100),
        "the next page came before the end"
    );
    key(&mut app, "enter");
    assert_eq!(last_of(&fake, "gws_file_open")["id"], "n89");
}

#[test]
fn slash_goes_to_search_and_keys_stay_out_of_a_field() {
    let (mut app, fake) = start();
    key(&mut app, "down");
    key(&mut app, "/");
    for k in ["down", "up", "shift+down", "left", "right"] {
        key(&mut app, k);
    }
    assert_eq!(selected(&app), [0], "the selection stayed");
    key(&mut app, "/");
    assert_eq!(
        app.screen()
            .input_text("label:Search Google Drive")
            .unwrap()
            .unwrap(),
        "/",
        "typed, as a slash is in a field"
    );
    app.clear("label:Search Google Drive").expect("it clears");
    key(&mut app, "enter");
    assert!(fake.called("gws_drive_file").is_empty());
    assert_eq!(fake.called("gws_drive_list").len(), 1, "nothing read again");
    assert_eq!(
        selected(&app),
        [0],
        "Return in an empty field changed nothing"
    );
}

// --- Narrow ------------------------------------------------------------------

/// 300 is the width that matters. The rest are narrower than anyone will use,
/// and go on to where anything fixed-width has to give.
const WIDTHS: [f32; 7] = [300.0, 280.0, 240.0, 200.0, 160.0, 120.0, 100.0];

/// Every press `selectors` names lies whole inside `width`.
fn assert_whole(app: &Headless, frames: &Frames, selectors: &[&str], width: f32) {
    for selector in selectors {
        let id = app
            .screen()
            .press_target(selector)
            .unwrap_or_else(|e| panic!("{selector} at {width}: {e}"))
            .unwrap_or_else(|| panic!("{selector} went at {width}: {:?}", app.screen().texts()));
        let (left, right) = frames.edges(id);
        assert!(
            left >= -0.5 && right <= width + 0.5,
            "{selector} at {width} runs from {left} to {right}"
        );
    }
}

fn sweep(app: &mut Headless, frames: &Frames, selectors: &[&str]) {
    for width in WIDTHS {
        app.set_viewport(size(px(width), px(700.0)))
            .expect("the viewport sets");
        settle(app);
        assert_whole(app, frames, selectors, width);
    }
}

const SCREEN_PRESSES: [&str; 10] = [
    "label:My Drive",
    "label:Shared",
    "label:Recent",
    "label:Starred",
    "label:Trash",
    "label:List",
    "label:Grid",
    "label:Open this folder >> nth=0",
    "label:Open this item >> nth=0",
    "label:Show details >> nth=0",
];

fn long_rows() -> Vec<Value> {
    let mut long = file("long", LONG_NAME, "spreadsheet", LONG_OWNER);
    long["size"] = json!(1_073_636_000_u64);
    let folder = file("long-folder", LONG_NAME, "folder", LONG_OWNER);
    vec![long, folder, file("short", "Short", "doc", "me")]
}

fn long_path() -> Value {
    let parts: Vec<Value> = (0..12)
        .map(|n| json!({"id": format!("p{n}"), "name": format!("{n} {LONG_NAME}")}))
        .collect();
    json!(parts)
}

fn start_long() -> (Headless, Arc<Fake>, Frames) {
    let fake = Arc::new(Fake::default());
    *fake.rows.lock().unwrap() = Some(long_rows());
    *fake.path.lock().unwrap() = Some(long_path());
    let (mut app, frames) = open_app(fake.clone(), 1000.0, 700.0);
    app.wait_for_within("text:Short", WAIT).expect("the rows");
    settle(&mut app);
    (app, fake, frames)
}

#[test]
fn every_press_stays_whole_at_every_width_in_the_list() {
    let (mut app, _, frames) = start_long();
    assert_whole(&app, &frames, &SCREEN_PRESSES, 1000.0);
    for (_, right) in frames.text_edges("1023.9 MB") {
        assert!(right <= 1000.5, "a long name pushed the size to {right}");
    }
    sweep(&mut app, &frames, &SCREEN_PRESSES);
}

#[test]
fn rows_scrolled_to_after_a_resize_read_the_narrow_way() {
    let (mut app, _) = start();
    some_folder(&mut app);
    app.set_viewport(size(px(420.0), px(600.0)))
        .expect("the viewport sets");
    settle(&mut app);
    app.scroll("text:Item 0", px(2500.0)).expect("it scrolls");
    settle(&mut app);
    let line = "File, me, Sep 30, 2024, 240 KB";
    let (narrow, wide) = (
        app.screen().count(&format!("text:{line}")).unwrap(),
        app.screen().count("text:240 KB").unwrap(),
    );
    assert!(narrow > 5, "{:?}", app.screen().texts());
    assert!(wide <= 2, "{wide} rows kept their columns");
}

#[test]
fn every_press_stays_whole_at_every_width_in_the_grid() {
    let (mut app, _, frames) = start_long();
    press(&mut app, "label:Grid");
    assert_whole(&app, &frames, &SCREEN_PRESSES, 1000.0);
    sweep(&mut app, &frames, &SCREEN_PRESSES);
}

#[test]
fn a_long_path_leaves_every_crumb_whole_and_the_rows_their_room() {
    let (mut app, _, frames) = start_long();
    for width in [1000.0, 640.0, 300.0, 100.0] {
        app.set_viewport(size(px(width), px(700.0)))
            .expect("the viewport sets");
        settle(&mut app);
        let crumbs = app.screen().count("label:Open this folder").unwrap();
        assert_eq!(crumbs, 13, "all there, the root and twelve");
        for n in 0..crumbs {
            assert_whole(
                &app,
                &frames,
                &[&format!("label:Open this folder >> nth={n}")],
                width,
            );
        }
        assert!(
            app.screen().shows("Short"),
            "at {width} the rows still show"
        );
    }
}

#[test]
fn details_keep_their_press_at_every_width_and_cover_the_rows_when_they_must() {
    let (mut app, _, frames) = start_long();
    press(&mut app, "text:Short");
    key(&mut app, "i");
    assert!(app.screen().shows("About short"));
    assert!(
        app.screen().matches("label:Open this item").unwrap(),
        "beside"
    );
    sweep(&mut app, &frames, &["label:Close these details"]);
    assert!(
        !app.screen().matches("label:Open this item").unwrap(),
        "over them"
    );
    assert!(app.screen().shows("Ana Reyes <ana@example.com>"));
    press(&mut app, "label:Close these details");
    assert!(app.screen().matches("label:Open this item").unwrap());
}

#[test]
fn the_keys_leave_rows_alone_while_details_cover_them() {
    let (mut app, _, _) = start_sized(500.0, 600.0);
    details(&mut app, "Notes");
    key(&mut app, "down");
    key(&mut app, "enter");
    press(&mut app, "label:Close these details");
    assert_eq!(selected(&app), [3], "it stayed on what was open");
}

#[test]
fn a_look_at_a_file_over_the_rows_keeps_the_place_in_the_list() {
    let (mut app, _, _) = start_sized(500.0, 600.0);
    some_folder(&mut app);
    app.scroll("text:Item 0", px(2500.0)).expect("it scrolls");
    settle(&mut app);
    let before = shown(&app);
    let n = *before.iter().nth(2).unwrap();
    press(&mut app, &format!("text:Item {n}"));
    key(&mut app, "i");
    assert!(
        !app.screen().matches("label:Open this item").unwrap(),
        "the rows are covered"
    );
    press(&mut app, "label:Close these details");
    assert!(shown(&app).contains(&n), "back at {n}: {:?}", shown(&app));
    assert_unbroken(&app);
}

#[test]
fn a_failures_press_stays_whole_at_every_width() {
    let (mut app, fake, frames) = start_sized(1000.0, 700.0);
    *fake.fail_from.lock().unwrap() = Some((100, "Google Drive didn't answer.".to_owned()));
    some_folder(&mut app);
    app.scroll("text:/^Item /", px(4600.0)).expect("it scrolls");
    settle(&mut app);
    sweep(&mut app, &frames, &["label:Try again"]);
}

#[test]
fn narrow_rows_still_say_kind_owner_changed_and_size() {
    let (mut app, _, _) = start_sized(420.0, 600.0);
    assert!(!app.screen().shows("Owner"), "no room for the columns");
    assert!(
        app.screen()
            .shows("Spreadsheet, Ana Reyes, Sep 30, 2024, 240 KB"),
        "{:?}",
        app.screen().texts()
    );
    app.set_viewport(size(px(1000.0), px(600.0)))
        .expect("the viewport sets");
    settle(&mut app);
    assert!(app.screen().shows("Owner"), "and they come back");
}

#[test]
fn the_widest_values_fit_their_columns() {
    let fake = Arc::new(Fake::default());
    let mut biggest = file("wide", "Wide values", "file", "me");
    biggest["size"] = json!(1_073_636_000_u64);
    *fake.rows.lock().unwrap() = Some(vec![biggest]);
    let (mut app, frames) = open_app(fake, 1000.0, 600.0);
    app.wait_for_within("text:Wide values", WAIT)
        .expect("the row");
    settle(&mut app);
    // each text takes the width it needs, and one that doesn't fit is cut to
    // its column's: the engine's font isn't the Mac's, so this wants room to spare
    for line in ["1023.9 MB", "Sep 30, 2024"] {
        assert!(app.screen().shows(line), "{:?}", app.screen().texts());
        let (text, column) = frames.width_in_parent(line);
        assert!(text < column - 4.0, "{line} takes {text} of {column}");
    }
}

// --- Opening files and editing back ------------------------------------------

fn edit(id: &str, name: &str, google: bool, changed: bool) -> Value {
    json!({"id": id, "name": name, "state": "edited", "google": google, "changed_on_drive": changed, "working": false})
}

/// A screen that finds `edits` on its first look.
fn start_edited(width: f32, edits: Vec<Value>) -> (Headless, Arc<Fake>, Frames) {
    let fake = Arc::new(Fake::default());
    *fake.edits.lock().unwrap() = edits;
    let (mut app, frames) = open_app(fake.clone(), width, 700.0);
    app.wait_for_within("text:Notes", WAIT).expect("My Drive");
    settle(&mut app);
    (app, fake, frames)
}

const BUTTONS: [&str; 6] = [
    "Save to Google Drive",
    "Upload to Google Drive",
    "Save as a copy",
    "Replace the one on Drive",
    "Save a copy to Google Drive",
    "Discard",
];

/// Exactly these of the bar's presses are on screen.
fn assert_buttons(app: &Headless, want: &[&str]) {
    for name in BUTTONS {
        let selector = format!("label:{name}");
        assert_eq!(
            app.screen().matches(&selector).unwrap(),
            want.contains(&name),
            "{name}: {:?}",
            app.screen().texts()
        );
    }
}

#[test]
fn a_press_on_a_file_opens_it_and_details_stay_a_press_away() {
    let (mut app, fake) = start();
    open(&mut app, "Budget.xlsx");
    assert_eq!(fake.called("gws_file_open"), [json!({"id": "budget"})]);
    assert!(fake.called("gws_drive_file").is_empty(), "no details yet");
    assert!(!app.screen().shows("Kind"));
    assert_eq!(selected(&app), [4]);

    press(&mut app, "label:Show details >> nth=4");
    assert_eq!(last_of(&fake, "gws_drive_file")["id"], "budget");
    assert!(app.screen().shows("About budget"));
    assert_eq!(fake.called("gws_file_open").len(), 1, "that opens nothing");
    press(&mut app, "label:Close these details");

    // the same in the grid, for a file and for a folder's own details
    press(&mut app, "label:Grid");
    press(&mut app, "label:Open this item >> text:Notes");
    assert_eq!(last_of(&fake, "gws_file_open")["id"], "notes");
    press(&mut app, "label:Show details >> nth=0");
    assert_eq!(last_of(&fake, "gws_drive_file")["id"], "clients");
    assert_eq!(
        fake.called("gws_drive_list").len(),
        1,
        "a folder's details don't go in"
    );
}

#[test]
fn a_folder_goes_in_and_opens_nothing() {
    let (mut app, fake) = start();
    open(&mut app, "Clients");
    assert!(app.screen().shows("Plan 2026.docx"));
    assert!(fake.called("gws_file_open").is_empty());
}

#[test]
fn a_file_in_the_trash_shows_its_details_and_opens_nothing() {
    let (mut app, fake) = start();
    let mut gone = file("gone-file", "Old file.docx", "word", "me");
    gone["trashed"] = json!(true);
    *fake.rows.lock().unwrap() = Some(vec![gone]);
    press(&mut app, "label:Trash");
    open(&mut app, "Old file.docx");
    assert!(fake.called("gws_file_open").is_empty());
    assert_eq!(last_of(&fake, "gws_drive_file")["id"], "gone-file");
}

#[test]
fn opening_says_so_until_the_answer_and_a_late_one_changes_nothing() {
    let (mut app, fake) = start();
    fake.hold("open:budget");
    open(&mut app, "Budget.xlsx");
    assert!(app.screen().shows("Opening…"));
    fake.release("open:budget");
    settle(&mut app);
    assert!(!app.screen().shows("Opening…"));

    // a refusal for a file left behind doesn't speak over the next press
    fake.opens.lock().unwrap().insert(
        "budget".to_owned(),
        json!({"refused": "no_access", "message": "An old refusal."}),
    );
    fake.hold("open:budget");
    open(&mut app, "Budget.xlsx");
    open(&mut app, "Notes");
    fake.release("open:budget");
    settle(&mut app);
    assert!(!app.screen().shows("An old refusal."));
}

#[test]
fn what_no_viewer_takes_shows_its_details_instead() {
    let (mut app, fake) = start();
    fake.opens.lock().unwrap().insert(
        "film".to_owned(),
        json!({"refused": "unsupported", "message": "This kind of file can't be opened here."}),
    );
    open(&mut app, "Film.mp4");
    assert_eq!(last_of(&fake, "gws_drive_file")["id"], "film");
    assert!(app.screen().shows("This kind of file has no preview."));
}

#[test]
fn too_big_says_why_and_offers_the_browser() {
    let (mut app, fake) = start();
    let link = "https://docs.google.com/document/d/notes/edit";
    fake.opens.lock().unwrap().insert(
        "notes".to_owned(),
        json!({"refused": "too_big", "message": "Google won't export a file over 10 MB.", "link": link}),
    );
    open(&mut app, "Notes");
    assert!(app.screen().shows("Google won't export a file over 10 MB."));
    assert!(fake.host.lock().unwrap().is_empty(), "not without a press");
    press(&mut app, "label:Open in the browser");
    let host = fake.host.lock().unwrap().clone();
    assert_eq!(host, [("buwiz.open".to_owned(), json!({"url": link}))]);
    press(&mut app, "label:Dismiss");
    assert!(!app.screen().shows("Google won't export a file over 10 MB."));
}

#[test]
fn a_refusal_or_a_call_the_bot_gave_up_on_says_so_in_plain_words() {
    let (mut app, fake) = start();
    fake.opens.lock().unwrap().insert(
        "notes".to_owned(),
        json!({"refused": "not_found", "message": "That's gone from Google Drive."}),
    );
    open(&mut app, "Notes");
    assert!(app.screen().shows("That's gone from Google Drive."));
    *fake.open_fails.lock().unwrap() = Some(BuwizError::new("timeout", false, "timed out"));
    open(&mut app, "Budget.xlsx");
    assert!(
        app.screen()
            .shows("Google Drive is taking a while. It's still going, so try again in a moment.")
    );
    assert!(!app.screen().shows("timed out"));
}

#[test]
fn an_edited_file_shows_edited_and_the_table_s_first_buttons() {
    let (mut app, fake, _) =
        start_edited(1000.0, vec![edit("budget", "Budget.xlsx", false, false)]);
    assert!(app.screen().shows("Budget.xlsx has changes here"));
    assert_buttons(&app, &["Upload to Google Drive", "Discard"]);
    assert!(app.screen().shows("edited"), "{:?}", app.screen().texts());
    assert_eq!(
        app.screen().count("text:edited").unwrap(),
        1,
        "on its own row and no other"
    );
    assert!(
        app.screen()
            .matches("label:Open this item >> text:edited")
            .unwrap()
    );
    press(&mut app, "label:Grid");
    assert_eq!(app.screen().count("text:edited").unwrap(), 1);
    assert!(
        fake.called("gws_file_upload").is_empty(),
        "nothing goes by itself"
    );
}

#[test]
fn an_edit_made_since_shows_with_the_next_look() {
    let (mut app, fake) = start();
    assert_buttons(&app, &[]);
    fake.edits
        .lock()
        .unwrap()
        .push(edit("budget", "Budget.xlsx", false, false));
    app.wait_for_within("label:Discard", Duration::from_secs(20))
        .expect("the bar comes");
    assert!(fake.called("gws_file_upload").is_empty(), "and only offers");
}

#[test]
fn a_file_changed_on_drive_too_offers_a_copy_or_a_replace_and_never_a_plain_upload() {
    let (app, _, _) = start_edited(1000.0, vec![edit("budget", "Budget.xlsx", false, true)]);
    assert!(
        app.screen()
            .shows("Google Drive also changed since you opened it.")
    );
    assert_buttons(
        &app,
        &["Save as a copy", "Replace the one on Drive", "Discard"],
    );
}

#[test]
fn a_google_file_offers_a_copy_and_nothing_that_writes_over_it() {
    // Drive moving makes no difference: nothing writes over one, ever
    for changed in [false, true] {
        let (app, _, _) = start_edited(1000.0, vec![edit("notes", "Notes.docx", true, changed)]);
        assert_buttons(&app, &["Save a copy to Google Drive", "Discard"]);
    }
}

/// It has no file on Drive, so nothing offers to write one over, and the person
/// isn't left with Discard as the only way to answer it.
#[test]
fn an_edited_attachment_offers_a_new_file_or_discard_and_says_where_it_came_from() {
    let mut edited = edit("att-m1-ab", "report.docx", false, false);
    edited["attachment"] = json!(true);
    let (mut app, fake, _) = start_edited(1000.0, vec![edited]);
    assert_buttons(&app, &["Save to Google Drive", "Discard"]);
    assert!(
        app.screen().shows(
            "This came from a mail. It stays here until you save it as a new file or discard it."
        ),
        "{:?}",
        app.screen().texts()
    );
    app.press("label:Save to Google Drive").expect("it presses");
    settle(&mut app);
    assert_eq!(
        fake.called("gws_file_upload"),
        [json!({"id": "att-m1-ab", "mode": "copy"})]
    );
}

#[test]
fn each_button_makes_its_own_call_and_the_bar_goes_with_the_file() {
    for (edited, button, tool, args) in [
        (
            edit("budget", "Budget.xlsx", false, false),
            "Upload to Google Drive",
            "gws_file_upload",
            json!({"id": "budget", "mode": "update"}),
        ),
        (
            edit("budget", "Budget.xlsx", false, true),
            "Save as a copy",
            "gws_file_upload",
            json!({"id": "budget", "mode": "copy"}),
        ),
        (
            edit("budget", "Budget.xlsx", false, true),
            "Replace the one on Drive",
            "gws_file_upload",
            json!({"id": "budget", "mode": "replace"}),
        ),
        (
            edit("notes", "Notes.docx", true, false),
            "Save a copy to Google Drive",
            "gws_file_upload",
            json!({"id": "notes", "mode": "copy"}),
        ),
        (
            edit("budget", "Budget.xlsx", false, false),
            "Discard",
            "gws_file_discard",
            json!({"id": "budget"}),
        ),
    ] {
        let (mut app, fake, _) = start_edited(1000.0, vec![edited]);
        press(&mut app, &format!("label:{button}"));
        if button == "Discard" {
            assert!(fake.called(tool).is_empty(), "the edits exist nowhere else");
            press(&mut app, "label:Discard my edits");
        }
        assert_eq!(fake.called(tool), [args], "{button}");
        assert_buttons(&app, &[]);
        assert!(!app.screen().shows("edited"), "{button}");
    }
}

#[test]
fn keeping_the_edits_after_discard_asks_changes_nothing() {
    let (mut app, fake, _) =
        start_edited(1000.0, vec![edit("budget", "Budget.xlsx", false, false)]);
    press(&mut app, "label:Discard");
    press(&mut app, "label:Keep my edits");
    assert!(fake.called("gws_file_discard").is_empty());
    assert_buttons(&app, &["Upload to Google Drive", "Discard"]);
}

#[test]
fn a_press_can_t_go_twice_while_the_first_is_on_its_way() {
    let (mut app, fake, _) =
        start_edited(1000.0, vec![edit("budget", "Budget.xlsx", false, false)]);
    fake.hold("upload:budget");
    press(&mut app, "label:Upload to Google Drive");
    assert_buttons(&app, &[]);
    assert!(app.screen().shows("Working on it…"));
    fake.release("upload:budget");
    settle(&mut app);
    assert_eq!(fake.called("gws_file_upload").len(), 1);
    assert!(app.screen().shows("Uploaded to Google Drive."));
}

#[test]
fn an_upload_still_going_past_its_call_shows_as_under_way() {
    let mut going = edit("budget", "Budget.xlsx", false, false);
    going["working"] = json!(true);
    let (app, _, _) = start_edited(1000.0, vec![going]);
    assert_buttons(&app, &[]);
    assert!(app.screen().shows("Working on it…"));
}

/// A conflict found at the press: nothing went, and the buttons change to fit.
#[test]
fn a_conflict_at_the_press_says_so_and_brings_the_other_buttons() {
    let (mut app, fake, _) =
        start_edited(1000.0, vec![edit("budget", "Budget.xlsx", false, false)]);
    fake.uploads.lock().unwrap().insert(
        "budget".to_owned(),
        json!({"refused": "changed", "message": "Google Drive changed this file since you opened it, so nothing was uploaded."}),
    );
    *fake.edits.lock().unwrap() = vec![edit("budget", "Budget.xlsx", false, true)];
    press(&mut app, "label:Upload to Google Drive");
    assert!(
        app.screen()
            .shows("Google Drive changed this file since you opened it, so nothing was uploaded.")
    );
    assert_buttons(
        &app,
        &["Save as a copy", "Replace the one on Drive", "Discard"],
    );
}

#[test]
fn a_bar_for_each_file_and_each_press_names_its_own() {
    let (mut app, fake, _) = start_edited(
        1000.0,
        vec![
            edit("budget", "Budget.xlsx", false, false),
            edit("elsewhere", "In another folder.docx", false, false),
        ],
    );
    assert_eq!(app.screen().count("label:Discard").unwrap(), 2);
    assert!(
        app.screen()
            .shows("In another folder.docx has changes here")
    );
    press(&mut app, "label:Discard >> nth=1");
    assert!(fake.called("gws_file_discard").is_empty(), "it asks first");
    press(&mut app, "label:Discard my edits");
    assert_eq!(
        fake.called("gws_file_discard"),
        [json!({"id": "elsewhere"})]
    );
    assert_eq!(app.screen().count("label:Discard").unwrap(), 1);
}

fn sweep_a_bar(edited: Value, presses: &[&str]) {
    let (mut app, _, frames) = start_edited(1000.0, vec![edited]);
    assert_whole(&app, &frames, presses, 1000.0);
    sweep(&mut app, &frames, presses);
    // and the rows' own presses, which the bar mustn't push out
    assert_whole(
        &app,
        &frames,
        &[
            "label:Open this item >> nth=0",
            "label:Show details >> nth=0",
        ],
        100.0,
    );
}

#[test]
fn the_bar_keeps_every_press_whole_at_every_width() {
    sweep_a_bar(
        edit("long", LONG_NAME, false, false),
        &["label:Upload to Google Drive", "label:Discard"],
    );
    sweep_a_bar(
        edit("long", LONG_NAME, false, true),
        &[
            "label:Save as a copy",
            "label:Replace the one on Drive",
            "label:Discard",
        ],
    );
    sweep_a_bar(
        edit("long", LONG_NAME, true, false),
        &["label:Save a copy to Google Drive", "label:Discard"],
    );
}

#[test]
fn what_a_press_said_keeps_its_presses_whole_at_every_width() {
    let (mut app, fake, frames) = start_sized(1000.0, 700.0);
    fake.opens.lock().unwrap().insert(
        "notes".to_owned(),
        json!({"refused": "too_big", "message": "Google won't export a file over 10 MB. You can open it in the browser.", "link": "https://docs.google.com/document/d/notes/edit"}),
    );
    open(&mut app, "Notes");
    sweep(
        &mut app,
        &frames,
        &["label:Open in the browser", "label:Dismiss"],
    );
}

#[test]
fn a_narrow_bar_and_a_long_name_leave_the_rows_on_screen() {
    let (mut app, fake, _) = start_edited(1000.0, vec![edit("long", LONG_NAME, false, true)]);
    app.set_viewport(size(px(240.0), px(600.0)))
        .expect("it sets");
    settle(&mut app);
    assert!(
        app.screen()
            .matches("label:Open this item >> nth=0")
            .unwrap()
    );
    press(&mut app, "label:Replace the one on Drive");
    assert_eq!(
        fake.called("gws_file_upload").len(),
        1,
        "a press that's seen is one that works"
    );
}

// --- The label rule ----------------------------------------------------------

fn assert_no_label_holds(app: &Headless, secrets: &[&str], at: &str) {
    for secret in secrets {
        let selector = format!("label:/(?i){}/", regex_quoted(secret));
        assert!(
            !app.screen().matches(&selector).unwrap(),
            "{at}: a label holds {secret}"
        );
    }
    assert!(
        !app.screen().matches("label:/@/").unwrap(),
        "{at}: a label holds an address"
    );
}

fn regex_quoted(text: &str) -> String {
    text.chars()
        .flat_map(|c| {
            let escape = !c.is_alphanumeric() && c != ' ';
            escape.then_some('\\').into_iter().chain([c])
        })
        .collect()
}

const SECRETS: [&str; 14] = [
    "Clients",
    "Acme",
    "Budget",
    "Plan 2026",
    "Notes",
    "Photo",
    "Deep",
    "Ana",
    "Reyes",
    "Ben",
    "Cruz",
    "ana@example.com",
    "owner@example.com",
    "About",
];

#[test]
fn no_label_on_the_screen_holds_a_name_an_owner_an_address_or_a_path() {
    let (mut app, _) = start();
    assert!(
        app.screen().shows("Budget.xlsx"),
        "the audit isn't looking at nothing"
    );
    assert_no_label_holds(&app, &SECRETS, "list");

    press(&mut app, "label:Grid");
    assert!(app.screen().shows("Budget.xlsx"));
    assert_no_label_holds(&app, &SECRETS, "grid");
    press(&mut app, "label:List");

    open(&mut app, "Clients");
    open(&mut app, "Acme");
    assert!(app.screen().shows("Deep notes"));
    assert_no_label_holds(&app, &SECRETS, "breadcrumb");

    details(&mut app, "Deep notes");
    assert!(app.screen().shows("Ana Reyes <ana@example.com>"));
    assert!(app.screen().shows("Clients / Acme"));
    assert_no_label_holds(&app, &SECRETS, "details");

    press(&mut app, "label:My Drive");
    details(&mut app, "Photo.png");
    assert!(app.screen().matches("label:Preview of this file").unwrap());
    assert_no_label_holds(&app, &SECRETS, "image preview");

    app.type_text("label:Search Google Drive", "Budget")
        .expect("it types");
    app.submit("label:Search Google Drive").expect("it submits");
    settle(&mut app);
    assert_no_label_holds(&app, &SECRETS, "search");

    app.set_viewport(size(px(420.0), px(600.0)))
        .expect("the viewport sets");
    settle(&mut app);
    assert_no_label_holds(&app, &SECRETS, "narrow");
}

#[test]
fn no_label_on_a_bar_or_a_refusal_holds_a_file_name() {
    for (google, changed) in [(false, false), (false, true), (true, false)] {
        let (mut app, fake, _) =
            start_edited(1000.0, vec![edit("budget", "Budget.xlsx", google, changed)]);
        assert!(app.screen().shows("Budget.xlsx has changes here"));
        assert_no_label_holds(&app, &SECRETS, "the bar");
        fake.opens.lock().unwrap().insert(
            "notes".to_owned(),
            json!({"refused": "too_big", "message": "Notes is too big.", "link": "https://docs.google.com/document/d/notes/edit"}),
        );
        open(&mut app, "Notes");
        assert!(app.screen().shows("Notes is too big."));
        assert_no_label_holds(&app, &SECRETS, "what a press said");
    }
}

#[test]
fn an_edited_row_keeps_its_two_fixed_labels() {
    let (app, _, _) = start_edited(1000.0, vec![edit("budget", "Budget.xlsx", false, false)]);
    assert_eq!(app.screen().count("label:Open this item").unwrap(), 7);
    assert_eq!(app.screen().count("label:Show details").unwrap(), 7);
    assert!(!app.screen().matches("label:/edited/").unwrap());
}

#[test]
fn every_row_and_card_has_the_one_fixed_label() {
    let (mut app, _) = start();
    assert_eq!(app.screen().count("label:Open this item").unwrap(), 7);
    press(&mut app, "label:Grid");
    assert_eq!(app.screen().count("label:Open this item").unwrap(), 7);
}

#[test]
fn no_label_on_the_card_holds_a_name_or_an_owner() {
    let (mut app, _, _) = start_card(420.0, 400.0);
    app.wait_for_within("text:Card file 0", WAIT)
        .expect("it shows");
    assert_no_label_holds(&app, &["Card file", "Ana", "Reyes"], "card");
}

// --- The card ----------------------------------------------------------------

fn start_card(width: f32, height: f32) -> (Headless, Arc<Fake>, Frames) {
    let fake = Arc::new(Fake::default());
    *fake.inline.lock().unwrap() = true;
    *fake.props.lock().unwrap() = json!({"search": "tkt1"});
    let (app, frames) = open_app(fake.clone(), width, height);
    (app, fake, frames)
}

#[test]
fn the_card_shows_five_files_with_no_click_and_opens_full_screen() {
    let (mut app, fake, _) = start_card(420.0, 500.0);
    app.wait_for_within("text:Card file 0", WAIT)
        .expect("it shows");
    settle(&mut app);
    assert_eq!(last_of(&fake, "gws_drive_card")["ticket"], "tkt1");
    for n in 0..5 {
        assert!(app.screen().shows(&format!("Card file {n}")), "{n}");
    }
    assert!(!app.screen().shows("Card file 5"), "up to five");
    assert_eq!(app.screen().count("text:PDF").unwrap(), 5);
    assert_eq!(app.screen().count("text:Ana Reyes").unwrap(), 5);
    assert_eq!(app.screen().count("text:Sep 30, 2024").unwrap(), 5);

    press(&mut app, "label:Open full screen");
    let opened = fake.host.lock().unwrap().clone();
    let (method, data) = opened
        .iter()
        .find(|(m, _)| m == "buwiz.open")
        .expect("it opens");
    assert_eq!(method, "buwiz.open");
    assert_eq!(data["app"], "drive");
    assert_eq!(data["props"], json!({"search": "tkt1"}));
}

#[test]
fn a_card_with_no_files_says_so() {
    let fake = Arc::new(Fake::default());
    *fake.inline.lock().unwrap() = true;
    *fake.card.lock().unwrap() = Some(json!({"rows": [], "saved": false}));
    let (mut app, _) = open_app(fake, 420.0, 300.0);
    app.wait_for_within("text:No files match.", WAIT)
        .expect("it says so");
}

#[test]
fn an_old_cards_ticket_says_so() {
    let fake = Arc::new(Fake::default());
    *fake.inline.lock().unwrap() = true;
    *fake.card.lock().unwrap() =
        Some(json!({"refused": "not_found", "message": "This card is out of date."}));
    let (mut app, _) = open_app(fake, 420.0, 300.0);
    app.wait_for_within("text:This card is out of date.", WAIT)
        .expect("it says so");
    assert!(!app.screen().matches("label:Open full screen").unwrap());
}

#[test]
fn a_saved_card_says_so() {
    let fake = Arc::new(Fake::default());
    *fake.inline.lock().unwrap() = true;
    let mut answer = a_card();
    answer["saved"] = json!(true);
    *fake.card.lock().unwrap() = Some(answer);
    let (mut app, _) = open_app(fake, 420.0, 400.0);
    app.wait_for_within("text:Saved files, offline", WAIT)
        .expect("it says so");
}

#[test]
fn a_cards_presses_and_long_values_stay_whole_at_every_width() {
    let fake = Arc::new(Fake::default());
    *fake.inline.lock().unwrap() = true;
    *fake.props.lock().unwrap() = json!({"search": "tkt1"});
    *fake.card.lock().unwrap() = Some(json!({"rows": long_rows(), "saved": false}));
    let (mut app, frames) = open_app(fake, 420.0, 700.0);
    app.wait_for_within("text:Short", WAIT).expect("it shows");
    settle(&mut app);
    sweep(
        &mut app,
        &frames,
        &["label:Open full screen", "label:Open this item >> nth=0"],
    );
    let owners = frames.text_edges(LONG_OWNER);
    assert!(!owners.is_empty());
    for (_, right) in owners {
        assert!(right <= 100.5, "a long owner runs to {right}");
    }
}
