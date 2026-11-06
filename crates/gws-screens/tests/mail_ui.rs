//! The Mail screen driven as a person drives it, on a headless engine: no
//! window and no plugin. A fake host answers the screen's tool calls, and can
//! hold an answer back so a test can make answers arrive out of order.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use buwiz_app::{Bridge, BuwizError, Done, Host, Listener};
use nsh_core::prelude::*;
use nsh_engine::EngineConfig;
use nsh_testing::Headless;
use serde_json::{Map, Value, json};

const WAIT: Duration = Duration::from_secs(5);
const PAGE: usize = 30;

/// What a list holds: its rows' numbers, shown as `Subject <n>`.
fn numbers(label: &str) -> Vec<usize> {
    match label {
        "INBOX" => (0..200).collect(),
        "STARRED" => (0..200).step_by(7).collect(),
        _ => Vec::new(),
    }
}

fn row(n: usize) -> Value {
    json!({"id": format!("t{n}"), "sender": format!("Sender {n}"), "subject": format!("Subject {n}"), "snippet": "", "at": 0, "unread": n == 4})
}

fn list(args: &Value) -> Value {
    let label = args["label"].as_str().unwrap_or("INBOX");
    let skip = args["skip"].as_u64().unwrap_or(0) as usize;
    let all = numbers(label);
    let rows: Vec<Value> = all.iter().skip(skip).take(PAGE).map(|n| row(*n)).collect();
    json!({"threads": rows, "more": skip + PAGE < all.len(), "saved": false})
}

/// A thread's answer: `t1` alone says it has blocked images, which is how a
/// test sees whose answer the reader took.
fn thread(id: &str) -> Value {
    json!({"page": format!("<p>{id}</p>"), "links": [], "images": i64::from(id == "t1")})
}

#[derive(Default)]
struct Fake {
    /// Mail that came in, newest first, ahead of the inbox's rows.
    newer: Mutex<Vec<usize>>,
    /// Archive answers that it's busy, and an undo that it failed.
    busy: Mutex<bool>,
    /// Trash goes through for the first conversation alone.
    partly: Mutex<bool>,
    /// What the next action answers instead of doing it.
    refuse: Mutex<Option<String>>,
    /// The next change check says the list changed.
    changed: Mutex<bool>,
    calls: Mutex<Vec<(String, Value)>>,
    /// Calls whose answer waits for [`Fake::release`], by what names them.
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

    fn hold(&self, name: &str) {
        self.hold.lock().unwrap().push(name.to_owned());
    }

    fn release(&self, name: &str) {
        let mut held = self.held.lock().unwrap();
        let at = held
            .iter()
            .position(|(n, _, _)| n == name)
            .expect("that call is held");
        let (_, answer, done) = held.remove(at);
        drop(held);
        done(Ok(answer));
    }
}

impl Host for Fake {
    fn params(&self) -> Map<String, Value> {
        let p = json!({
            "props": {}, "app": {"plugin": "app.buwiz.google-workspace", "app": "mail"},
            "granted": ["buwiz.call"],
            "viewport": {"width": 1000, "height": 600, "placement": "full"},
        });
        p.as_object().unwrap().clone()
    }

    fn post(&self, _: &str, _: Value) {}

    fn call(&self, method: &str, data: Value, done: Done) {
        if method != "buwiz.call" {
            return done(Err(BuwizError::new(
                "invalid",
                false,
                format!("not faked: {method}"),
            )));
        }
        let (tool, args) = (data["tool"].as_str().unwrap_or_default(), &data["args"]);
        self.calls
            .lock()
            .unwrap()
            .push((tool.to_owned(), args.clone()));
        let (name, answer) = match tool {
            "gws_mail_labels" => (
                String::new(),
                json!({"labels": [{"id": "Label_1", "name": "Work", "kind": "user"}]}),
            ),
            "gws_mail_changes" => {
                let changed = std::mem::take(&mut *self.changed.lock().unwrap());
                (
                    String::new(),
                    json!({"status": "", "seq": 0, "changed": changed}),
                )
            }
            "gws_mail_list" => {
                let mut answer = list(args);
                if args["label"] == "INBOX" && args["skip"] == 0 {
                    let new = self.newer.lock().unwrap();
                    let rows = answer["threads"].as_array_mut().unwrap();
                    for n in new.iter().rev() {
                        rows.insert(0, row(*n));
                    }
                }
                (
                    args["label"].as_str().unwrap_or_default().to_owned(),
                    answer,
                )
            }
            "gws_mail_archive" if self.refuse.lock().unwrap().is_some() => {
                let why = self.refuse.lock().unwrap().take().unwrap();
                return done(Err(BuwizError::new("needs_access", false, why)));
            }
            "gws_mail_archive" if *self.busy.lock().unwrap() => {
                return done(Err(BuwizError::new("busy", true, "busy".to_owned())));
            }
            "gws_mail_undo" if *self.busy.lock().unwrap() => {
                return done(Err(BuwizError::new(
                    "failed",
                    false,
                    "Gmail didn't answer.".to_owned(),
                )));
            }
            "gws_mail_trash" if *self.partly.lock().unwrap() => {
                let first = args["threads"][0].clone();
                (
                    tool.to_owned(),
                    json!({"undo": "u1", "done": 1, "threads": [first], "partial": true}),
                )
            }
            "gws_mail_mark" | "gws_mail_archive" | "gws_mail_trash" | "gws_mail_label" => {
                (tool.to_owned(), json!({"undo": "u1", "done": 1}))
            }
            "gws_mail_undo" => (String::new(), json!({"done": 1})),
            "gws_mail_thread" => {
                let id = args["thread"].as_str().unwrap_or_default();
                (id.to_owned(), thread(id))
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

fn start() -> (Headless, Arc<Fake>) {
    let fake = Arc::new(Fake::default());
    let def = gws_screens::screens()
        .iter()
        .find(|s| s.id == "gws.mail")
        .expect("Mail");
    let root = (def.build)(Bridge::new(fake.clone()));
    let mut app = Headless::start(EngineConfig::new(root)).expect("the engine starts");
    app.set_viewport(size(px(1000.0), px(600.0)))
        .expect("the viewport sets");
    app.wait_for_within("text:Subject 0", WAIT)
        .expect("the inbox shows");
    (app, fake)
}

/// The row numbers on screen.
fn shown(app: &Headless) -> BTreeSet<usize> {
    let texts = app.screen().texts();
    texts
        .iter()
        .filter_map(|t| t.strip_prefix("Subject ")?.parse().ok())
        .collect()
}

/// A list on screen is one unbroken run of `all`: a cell that kept an old row
/// shows up as a number from somewhere else.
fn assert_unbroken(app: &Headless, all: &[usize]) {
    let shown = shown(app);
    let at = |n: &usize| {
        all.iter()
            .position(|a| a == n)
            .unwrap_or_else(|| panic!("{n} isn't in this list: {shown:?}"))
    };
    let (first, last) = (
        at(shown.first().expect("rows")),
        at(shown.last().expect("rows")),
    );
    assert_eq!(last - first + 1, shown.len(), "a gap in {shown:?}");
}

fn settle(app: &mut Headless) {
    app.wait(Duration::from_millis(300)).expect("time passes");
}

fn pick_label(app: &mut Headless, name: &str) {
    app.press(&format!("label:Open this label >> text:{name}"))
        .expect("the label is there");
    settle(app);
}

#[test]
fn a_label_press_loads_that_label() {
    let (mut app, fake) = start();
    pick_label(&mut app, "Starred");
    let asked: Vec<Value> = fake
        .called("gws_mail_list")
        .iter()
        .map(|a| a["label"].clone())
        .collect();
    assert!(asked.contains(&json!("STARRED")), "asked for {asked:?}");
    assert_unbroken(&app, &numbers("STARRED"));
    assert!(app.screen().shows("Subject 7"));
    assert!(!app.screen().shows("Subject 1"), "the inbox's rows went");
}

#[test]
fn rows_stay_in_order_through_a_scroll_and_a_label_switch() {
    let (mut app, _) = start();
    app.scroll("text:Subject 0", px(2500.0))
        .expect("the list scrolls");
    settle(&mut app);
    assert_unbroken(&app, &numbers("INBOX"));
    assert!(!app.screen().shows("Subject 0"), "scrolled past the top");

    pick_label(&mut app, "Starred");
    assert_unbroken(&app, &numbers("STARRED"));
    pick_label(&mut app, "Inbox");
    assert_unbroken(&app, &numbers("INBOX"));
}

#[test]
fn a_press_after_a_scroll_opens_the_row_it_shows() {
    let (mut app, fake) = start();
    app.scroll("text:Subject 0", px(2500.0))
        .expect("the list scrolls");
    settle(&mut app);
    let n = *shown(&app).iter().nth(3).expect("rows on screen");
    app.press(&format!("text:Subject {n}"))
        .expect("the row presses");
    settle(&mut app);
    let opened = fake.called("gws_mail_thread");
    assert_eq!(
        opened.last().expect("a thread opened")["thread"],
        format!("t{n}")
    );
}

#[test]
fn a_late_list_answer_does_not_replace_a_newer_list() {
    let (mut app, fake) = start();
    fake.hold("STARRED");
    pick_label(&mut app, "Starred");
    pick_label(&mut app, "Sent");
    assert!(shown(&app).is_empty(), "Sent has no mail");

    fake.release("STARRED");
    settle(&mut app);
    assert!(
        shown(&app).is_empty(),
        "Starred came late: {:?}",
        shown(&app)
    );
}

#[test]
fn a_late_thread_answer_does_not_replace_the_open_one() {
    let (mut app, fake) = start();
    fake.hold("t1");
    app.press("text:Subject 1").expect("the row presses");
    settle(&mut app);
    app.press("text:Subject 2").expect("the row presses");
    settle(&mut app);

    fake.release("t1");
    settle(&mut app);
    assert!(
        !app.screen().shows("1 image blocked"),
        "t1's answer landed after t2"
    );
}

fn opened(fake: &Fake) -> Value {
    fake.called("gws_mail_thread")
        .last()
        .expect("a thread opened")["thread"]
        .clone()
}

/// Mail arrives above the list, moving every row down.
fn mail_arrives(app: &mut Headless, fake: &Fake) {
    fake.newer.lock().unwrap().extend([900, 901]);
    *fake.changed.lock().unwrap() = true;
    app.wait_for_within("text:Subject 900", Duration::from_secs(20))
        .expect("the new mail shows");
}

#[test]
fn new_mail_on_top_leaves_an_action_on_the_conversation_chosen() {
    let (mut app, fake) = start();
    app.press("text:Subject 3").expect("the row presses");
    settle(&mut app);
    mail_arrives(&mut app, &fake);
    app.press_key("e").expect("the key presses");
    settle(&mut app);
    let archived = fake.called("gws_mail_archive");
    assert_eq!(
        archived.last().expect("an archive")["threads"],
        json!(["t3"])
    );
}

#[test]
fn j_after_new_mail_goes_on_from_the_conversation_chosen() {
    let (mut app, fake) = start();
    app.press("text:Subject 3").expect("the row presses");
    settle(&mut app);
    mail_arrives(&mut app, &fake);
    app.press_key("j").expect("the key presses");
    settle(&mut app);
    assert_eq!(opened(&fake), "t4");
}

#[test]
fn archive_moves_on_to_the_next_and_undo_brings_the_list_back() {
    let (mut app, fake) = start();
    app.press("text:Subject 3").expect("the row presses");
    settle(&mut app);
    app.press_key("e").expect("the key presses");
    settle(&mut app);
    assert!(!app.screen().shows("Subject 3"), "it left the list");
    assert_eq!(opened(&fake), "t4", "the next one opens");
    assert!(app.screen().shows("Archived."));

    app.press("label:Undo that").expect("the bar offers it");
    settle(&mut app);
    assert_eq!(
        fake.called("gws_mail_undo").last().expect("an undo")["undo"],
        "u1"
    );
    assert!(app.screen().shows("Subject 3"), "the list came back");
}

#[test]
fn picked_conversations_go_together() {
    let (mut app, fake) = start();
    app.press("text:Subject 2").expect("the row presses");
    settle(&mut app);
    app.press_key("x").expect("the key presses");
    app.press("text:Subject 5").expect("the row presses");
    settle(&mut app);
    app.press_key("x").expect("the key presses");
    app.press_key("shift+3").expect("the key presses");
    settle(&mut app);
    let trashed = fake.called("gws_mail_trash");
    assert_eq!(
        trashed.last().expect("a trash")["threads"],
        json!(["t2", "t5"])
    );
}

#[test]
fn with_nothing_open_an_action_does_nothing() {
    let (mut app, fake) = start();
    app.press_key("e").expect("the key presses");
    app.press_key("shift+3").expect("the key presses");
    settle(&mut app);
    assert!(fake.called("gws_mail_archive").is_empty());
    assert!(fake.called("gws_mail_trash").is_empty());
}

#[test]
fn typing_in_search_is_no_action() {
    let (mut app, fake) = start();
    app.press("text:Subject 3").expect("the row presses");
    settle(&mut app);
    app.press_key("/").expect("the key presses");
    settle(&mut app);
    app.press_key("e").expect("the key presses");
    app.press_key("shift+3").expect("the key presses");
    settle(&mut app);
    assert!(
        fake.called("gws_mail_archive").is_empty(),
        "e went to the field"
    );
    assert!(
        fake.called("gws_mail_trash").is_empty(),
        "# went to the field"
    );
}

#[test]
fn opening_an_unread_mail_marks_it_read_and_nothing_else_does() {
    let (mut app, fake) = start();
    assert!(
        fake.called("gws_mail_mark").is_empty(),
        "the list reads nothing"
    );
    app.press("text:Subject 3").expect("the row presses");
    settle(&mut app);
    assert!(
        fake.called("gws_mail_mark").is_empty(),
        "that one was read already"
    );
    app.press("text:Subject 4").expect("the row presses");
    settle(&mut app);
    let marked = fake.called("gws_mail_mark");
    assert_eq!(marked, vec![json!({"threads": ["t4"], "read": true})]);
}

#[test]
fn the_toolbar_acts_on_the_open_conversation() {
    let (mut app, fake) = start();
    assert!(!app.screen().shows("Archive"), "nothing open, no toolbar");
    app.press("text:Subject 3").expect("the row presses");
    settle(&mut app);
    app.press("label:Star this").expect("the toolbar shows");
    settle(&mut app);
    let marked = fake.called("gws_mail_mark");
    assert_eq!(
        marked.last().expect("a mark"),
        &json!({"threads": ["t3"], "starred": true})
    );

    app.press("label:Delete this").expect("the toolbar shows");
    settle(&mut app);
    assert_eq!(
        fake.called("gws_mail_trash").last().expect("a trash")["threads"],
        json!(["t3"])
    );
    assert!(!app.screen().shows("Subject 3"));
    assert_eq!(
        opened(&fake),
        "t4",
        "the next one opens, though the bar redrew"
    );
}

#[test]
fn a_label_can_be_added_or_moved_to() {
    let (mut app, fake) = start();
    app.press("text:Subject 3").expect("the row presses");
    settle(&mut app);
    app.press("label:Choose a label")
        .expect("the toolbar shows");
    settle(&mut app);
    app.press("label:Add this label").expect("the picker shows");
    settle(&mut app);
    let labelled = fake.called("gws_mail_label");
    assert_eq!(
        labelled.last().expect("a label"),
        &json!({"threads": ["t3"], "add": ["Label_1"]})
    );
    assert!(app.screen().shows("Subject 3"), "adding keeps it here");

    app.press("label:Choose a label")
        .expect("the toolbar shows");
    settle(&mut app);
    app.press("label:Move here").expect("the picker shows");
    settle(&mut app);
    let moved = fake.called("gws_mail_label");
    assert_eq!(
        moved.last().expect("a move"),
        &json!({"threads": ["t3"], "add": ["Label_1"], "remove": ["INBOX"]})
    );
    assert!(!app.screen().shows("Subject 3"), "a move takes it out");
}

#[test]
fn an_action_the_sign_in_does_not_cover_says_so_and_changes_nothing_here() {
    let (mut app, fake) = start();
    app.press("text:Subject 3").expect("the row presses");
    settle(&mut app);
    *fake.refuse.lock().unwrap() = Some("Sign in again to turn it on.".to_owned());
    app.press_key("e").expect("the key presses");
    settle(&mut app);
    assert!(app.screen().shows("Sign in again to turn it on."));
    assert!(app.screen().shows("Subject 3"), "it stays in the list");
    assert!(!app.screen().shows("Archived."));
}

#[test]
fn queued_actions_keep_the_conversation_they_were_pressed_on() {
    let (mut app, fake) = start();
    app.press("text:Subject 3").expect("the row presses");
    settle(&mut app);
    fake.hold("gws_mail_mark");
    app.press_key("s").expect("the key presses");
    app.press_key("e").expect("the key presses");
    app.press_key("j").expect("the key presses");
    settle(&mut app);
    assert_eq!(opened(&fake), "t4", "j moved on while the star was out");
    fake.release("gws_mail_mark");
    settle(&mut app);
    let archived = fake.called("gws_mail_archive");
    assert_eq!(
        archived.last().expect("an archive")["threads"],
        json!(["t3"])
    );
}

#[test]
fn a_new_list_drops_the_picks_nobody_can_see() {
    let (mut app, fake) = start();
    app.press("text:Subject 2").expect("the row presses");
    settle(&mut app);
    app.press_key("x").expect("the key presses");
    pick_label(&mut app, "Starred");
    app.press_key("e").expect("the key presses");
    settle(&mut app);
    assert!(
        fake.called("gws_mail_archive").is_empty(),
        "no pick, nothing open"
    );

    pick_label(&mut app, "Inbox");
    app.press("text:Subject 2").expect("the row presses");
    settle(&mut app);
    app.press_key("x").expect("the key presses");
    app.submit("text:Search mail").expect("the search goes");
    settle(&mut app);
    app.press_key("e").expect("the key presses");
    settle(&mut app);
    assert!(
        fake.called("gws_mail_archive").is_empty(),
        "a search drops them too"
    );
}

#[test]
fn what_failed_stays_and_the_screen_says_how_many_went() {
    let (mut app, fake) = start();
    app.press("text:Subject 2").expect("the row presses");
    settle(&mut app);
    app.press_key("x").expect("the key presses");
    app.press("text:Subject 5").expect("the row presses");
    settle(&mut app);
    app.press_key("x").expect("the key presses");
    *fake.partly.lock().unwrap() = true;
    app.press_key("shift+3").expect("the key presses");
    settle(&mut app);
    assert!(!app.screen().shows("Subject 2"), "that one went");
    assert!(app.screen().shows("Subject 5"), "that one stayed");
    assert!(app.screen().shows("1 of 2 went through. The rest didn't."));
}

#[test]
fn a_write_is_never_tried_twice_on_its_own() {
    let (mut app, fake) = start();
    app.press("text:Subject 3").expect("the row presses");
    settle(&mut app);
    *fake.busy.lock().unwrap() = true;
    app.press_key("e").expect("the key presses");
    app.wait(Duration::from_secs(2)).expect("time passes");
    assert_eq!(fake.called("gws_mail_archive").len(), 1);
    assert!(app.screen().shows("Subject 3"));
}

#[test]
fn an_undo_that_failed_can_be_pressed_again() {
    let (mut app, fake) = start();
    app.press("text:Subject 3").expect("the row presses");
    settle(&mut app);
    app.press_key("e").expect("the key presses");
    settle(&mut app);
    *fake.busy.lock().unwrap() = true;
    app.press("label:Undo that").expect("the bar offers it");
    settle(&mut app);
    assert!(app.screen().shows("Gmail didn't answer."));
    assert!(
        app.screen().matches("label:Undo that").unwrap(),
        "the bar stays"
    );
    *fake.busy.lock().unwrap() = false;
    app.press("label:Undo that").expect("the bar offers it");
    settle(&mut app);
    assert!(app.screen().shows("Subject 3"));
}
