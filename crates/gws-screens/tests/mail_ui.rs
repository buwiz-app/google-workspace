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
        "DRAFT" => vec![500, 501],
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
    /// What the screen opens with, and where: a card is `inline`.
    props: Mutex<Value>,
    inline: Mutex<bool>,
    /// What a draft read answers, in place of the usual draft.
    draft: Mutex<Option<Value>>,
    /// Mail that came in, newest first, ahead of the inbox's rows.
    newer: Mutex<Vec<usize>>,
    /// Rows that have left the mailbox.
    gone: Mutex<Vec<usize>>,
    /// How many actions have been answered, for each one's undo id.
    actions: Mutex<usize>,
    /// Archive answers that it's busy, and an undo that it failed.
    busy: Mutex<bool>,
    /// Trash goes through for the first conversation alone.
    partly: Mutex<bool>,
    /// What the next action answers instead of doing it.
    refuse: Mutex<Option<String>>,
    /// The next change check says the list changed.
    changed: Mutex<bool>,
    /// Why a press of Send is refused: the press was the AI's.
    refuse_press: Mutex<Option<String>>,
    refuse_send: Mutex<Option<String>>,
    drafts: Mutex<usize>,
    held_bytes: Mutex<u64>,
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
        let placement = if *self.inline.lock().unwrap() {
            "inline"
        } else {
            "full"
        };
        let props = self.props.lock().unwrap().clone();
        let props = if props.is_null() { json!({}) } else { props };
        let p = json!({
            "props": props, "app": {"plugin": "app.buwiz.google-workspace", "app": "mail"},
            "granted": ["buwiz.call", "buwiz.open"],
            "viewport": {"width": 1000, "height": 600, "placement": placement},
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
                let gone = self.gone.lock().unwrap();
                let rows = answer["threads"].as_array_mut().unwrap();
                rows.retain(|r| !gone.iter().any(|n| r["id"] == format!("t{n}")));
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
                    json!({"undo": "partly", "done": 1, "threads": [first], "partial": true}),
                )
            }
            "gws_mail_mark" | "gws_mail_archive" | "gws_mail_trash" | "gws_mail_label" => {
                let mut actions = self.actions.lock().unwrap();
                *actions += 1;
                (
                    tool.to_owned(),
                    json!({"undo": format!("u{actions}"), "done": 1}),
                )
            }
            "gws_mail_undo" => (tool.to_owned(), json!({"done": 1})),
            "gws_mail_send_arm" if self.refuse_press.lock().unwrap().is_some() => {
                let why = self.refuse_press.lock().unwrap().clone().unwrap();
                return done(Err(BuwizError::new("denied", false, why)));
            }
            "gws_mail_send_arm" => (tool.to_owned(), json!({"armed": "a1"})),
            "gws_mail_draft_save" => {
                let mut drafts = self.drafts.lock().unwrap();
                *drafts += 1;
                let id = args["draft"]
                    .as_str()
                    .map_or(format!("d{drafts}"), str::to_owned);
                (
                    tool.to_owned(),
                    json!({"draft": id, "revision": format!("r{drafts}"), "message_id": "<m@x>"}),
                )
            }
            "gws_mail_send" if self.refuse_send.lock().unwrap().is_some() => {
                let why = self.refuse_send.lock().unwrap().take().unwrap();
                return done(Err(BuwizError::new("changed", false, why)));
            }
            "gws_mail_send" => (tool.to_owned(), json!({"sent": true})),
            "gws_mail_draft_delete" => (tool.to_owned(), json!({"deleted": true})),
            "gws_mail_reply_start" => (
                tool.to_owned(),
                json!({"to": ["ana@example.com"], "cc": [], "subject": "Re: Lunch", "body": "\n\nOn Monday, Ana wrote:\n> Noon?\n", "thread": args["thread"], "in_reply_to": "<a@x>", "references": "<a@x>"}),
            ),
            "gws_mail_draft_of" => (tool.to_owned(), json!({"draft": "d9"})),
            "gws_people_suggest" => (
                tool.to_owned(),
                json!({"people": [{"name": "Ana Reyes", "address": "ana@example.com"}]}),
            ),
            "gws_mail_draft_read" if self.draft.lock().unwrap().is_some() => {
                (tool.to_owned(), self.draft.lock().unwrap().clone().unwrap())
            }
            "gws_mail_draft_read" => (
                tool.to_owned(),
                json!({"draft": args["draft"], "revision": "r9", "message_id": "<m9@x>", "to": "ben@example.com", "cc": "", "bcc": "", "subject": "Saved one", "body": "Half written", "thread": "t1", "attachments": [{"name": "a.pdf", "mime": "application/pdf", "size": 2048, "id": "att-0"}]}),
            ),
            "gws_upload_chunk" => {
                let mut held = self.held_bytes.lock().unwrap();
                *held += args["data"].as_str().unwrap().len() as u64 * 3 / 4;
                (tool.to_owned(), json!({"upload": "up1", "size": *held}))
            }
            "gws_upload_drop" => (tool.to_owned(), json!({"dropped": true})),
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

#[test]
fn a_queued_undo_takes_back_what_it_was_pressed_on() {
    let (mut app, fake) = start();
    app.press("text:Subject 3").expect("the row presses");
    settle(&mut app);
    app.press_key("e").expect("the key presses");
    settle(&mut app);
    let first = fake.actions.lock().unwrap().to_string();
    fake.hold("gws_mail_mark");
    app.press_key("s").expect("the key presses");
    settle(&mut app);
    app.press("label:Undo that").expect("the bar offers it");
    settle(&mut app);
    fake.release("gws_mail_mark");
    settle(&mut app);
    let undone = fake.called("gws_mail_undo");
    assert_eq!(undone.last().expect("an undo")["undo"], format!("u{first}"));
}

#[test]
fn a_second_undo_press_is_the_same_undo() {
    let (mut app, fake) = start();
    app.press("text:Subject 3").expect("the row presses");
    settle(&mut app);
    app.press_key("e").expect("the key presses");
    settle(&mut app);
    fake.hold("gws_mail_undo");
    app.press("label:Undo that").expect("the bar offers it");
    settle(&mut app);
    app.press_key("z").expect("the key presses");
    settle(&mut app);
    fake.release("gws_mail_undo");
    settle(&mut app);
    assert_eq!(fake.called("gws_mail_undo").len(), 1, "asked for once");
    assert!(app.screen().shows("Subject 3"));
    assert!(
        !app.screen().matches("label:Dismiss this").unwrap(),
        "no error after it worked"
    );
}

#[test]
fn a_good_undo_leaves_what_was_said_about_another_action() {
    let (mut app, fake) = start();
    app.press("text:Subject 3").expect("the row presses");
    settle(&mut app);
    app.press_key("e").expect("the key presses");
    settle(&mut app);
    fake.hold("gws_mail_trash");
    app.press("text:Subject 2").expect("the row presses");
    settle(&mut app);
    app.press_key("x").expect("the key presses");
    app.press("text:Subject 5").expect("the row presses");
    settle(&mut app);
    app.press_key("x").expect("the key presses");
    *fake.partly.lock().unwrap() = true;
    app.press_key("shift+3").expect("the key presses");
    settle(&mut app);
    // pressed while the bar still offers the archive, so it runs after the trash
    app.press("label:Undo that").expect("the bar offers it");
    settle(&mut app);
    fake.release("gws_mail_trash");
    settle(&mut app);
    assert_eq!(
        fake.called("gws_mail_undo").len(),
        1,
        "the archive came back"
    );
    assert!(app.screen().shows("1 of 2 went through. The rest didn't."));
}

/// A row on the second page, picked, and which it was.
fn pick_far_down(app: &mut Headless) -> usize {
    app.scroll("text:Subject 0", px(2500.0))
        .expect("the list scrolls");
    settle(app);
    let n = *shown(app).last().expect("rows show");
    assert!(n >= PAGE, "row {n} is on the first page");
    app.press(&format!("text:Subject {n}"))
        .expect("the row presses");
    settle(app);
    app.press_key("x").expect("the key presses");
    n
}

/// Until the change check has been answered and the list read again.
fn reloaded(app: &mut Headless, fake: &Fake) {
    let by = std::time::Instant::now() + Duration::from_secs(20);
    while *fake.changed.lock().unwrap() {
        assert!(std::time::Instant::now() < by, "no change check came");
        settle(app);
    }
    settle(app);
}

#[test]
fn a_reload_keeps_the_rows_and_picks_further_down() {
    let (mut app, fake) = start();
    let n = pick_far_down(&mut app);
    // the open one is another row, so an action shows if the pick survived
    let open = *shown(&app).first().expect("rows show");
    assert_ne!(open, n);
    app.press(&format!("text:Subject {open}"))
        .expect("the row presses");
    settle(&mut app);
    fake.newer.lock().unwrap().push(900);
    *fake.changed.lock().unwrap() = true;
    reloaded(&mut app, &fake);
    app.press_key("e").expect("the key presses");
    settle(&mut app);
    let archived = fake.called("gws_mail_archive");
    assert_eq!(
        archived.last().expect("an archive")["threads"],
        json!([format!("t{n}")])
    );
    assert!(!app.screen().shows("left this list"));
}

#[test]
fn a_pick_gone_from_further_down_is_said_on_screen() {
    let (mut app, fake) = start();
    let n = pick_far_down(&mut app);
    fake.gone.lock().unwrap().push(n);
    *fake.changed.lock().unwrap() = true;
    app.wait_for_within(
        "text:A picked conversation left this list.",
        Duration::from_secs(20),
    )
    .expect("the screen says so");
}

#[test]
fn a_pick_that_left_the_list_gets_no_action() {
    let (mut app, fake) = start();
    app.press("text:Subject 2").expect("the row presses");
    settle(&mut app);
    app.press_key("x").expect("the key presses");
    app.press("text:Subject 5").expect("the row presses");
    settle(&mut app);
    fake.gone.lock().unwrap().push(2);
    *fake.changed.lock().unwrap() = true;
    app.wait_for_gone("text:Subject 2", Duration::from_secs(20))
        .expect("it leaves");
    app.press_key("e").expect("the key presses");
    settle(&mut app);
    let archived = fake.called("gws_mail_archive");
    assert_eq!(
        archived.last().expect("an archive")["threads"],
        json!(["t5"])
    );
}

#[test]
fn what_an_action_says_outlasts_a_reload_until_dismissed() {
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
    fake.newer.lock().unwrap().push(900);
    *fake.changed.lock().unwrap() = true;
    app.wait_for_within("text:Subject 900", Duration::from_secs(20))
        .expect("the list reloads");
    assert!(app.screen().shows("1 of 2 went through. The rest didn't."));
    app.press("label:Dismiss this")
        .expect("it can be dismissed");
    settle(&mut app);
    assert!(!app.screen().shows("1 of 2 went through. The rest didn't."));
}

#[test]
fn a_pick_made_during_an_action_survives_it() {
    let (mut app, fake) = start();
    app.press("text:Subject 2").expect("the row presses");
    settle(&mut app);
    app.press_key("x").expect("the key presses");
    fake.hold("gws_mail_trash");
    app.press_key("shift+3").expect("the key presses");
    settle(&mut app);
    app.press("text:Subject 5").expect("the row presses");
    settle(&mut app);
    app.press_key("x").expect("the key presses");
    // another row open, so a lost pick can't hide behind the open one
    app.press("text:Subject 6").expect("the row presses");
    settle(&mut app);
    fake.release("gws_mail_trash");
    settle(&mut app);
    app.press_key("e").expect("the key presses");
    settle(&mut app);
    let archived = fake.called("gws_mail_archive");
    assert_eq!(
        archived.last().expect("an archive")["threads"],
        json!(["t5"])
    );
}

/// A message written from nothing, ready to send.
fn write_one(app: &mut Headless) {
    app.press("label:Write a message")
        .expect("the rail offers it");
    settle(app);
    app.type_text("label:To", "ana@example.com")
        .expect("To takes text");
    app.type_text("label:Subject", "Lunch")
        .expect("Subject takes text");
    app.type_text("label:Message", "Noon?")
        .expect("Message takes text");
}

fn wait_for_send(app: &mut Headless, fake: &Fake) {
    let by = std::time::Instant::now() + Duration::from_secs(12);
    while fake.called("gws_mail_send").is_empty() {
        assert!(std::time::Instant::now() < by, "nothing sent");
        settle(app);
    }
}

#[test]
fn send_saves_what_was_written_holds_five_seconds_and_sends_that() {
    let (mut app, fake) = start();
    write_one(&mut app);
    let pressed = std::time::Instant::now();
    app.press("label:Send").expect("Send presses");
    app.wait_for_within("text:Sending in a moment.", WAIT)
        .expect("the hold shows");
    assert!(fake.called("gws_mail_send").is_empty());
    wait_for_send(&mut app, &fake);
    assert!(
        pressed.elapsed() >= Duration::from_secs(5),
        "held the whole time"
    );

    let saved = fake.called("gws_mail_draft_save");
    let last = saved.last().expect("a save");
    assert_eq!(last["to"], "ana@example.com");
    assert_eq!(last["subject"], "Lunch");
    assert_eq!(last["body"], "Noon?");
    let sent = &fake.called("gws_mail_send")[0];
    assert_eq!(sent["armed"], "a1", "the press's own answer");
    assert_eq!(
        sent["revision"],
        format!("r{}", fake.drafts.lock().unwrap())
    );
    app.wait_for_within("text:Message sent.", WAIT)
        .expect("the bar says so");
    assert!(
        !app.screen().matches("label:Message").unwrap(),
        "the panel's gone"
    );
}

#[test]
fn undo_inside_the_hold_sends_nothing_and_brings_the_message_back() {
    let (mut app, fake) = start();
    write_one(&mut app);
    app.press("label:Send").expect("Send presses");
    app.wait_for_within("label:Take this back", WAIT)
        .expect("the bar offers it");
    app.press("label:Take this back").expect("Undo presses");
    app.wait_for_within("label:Message", WAIT)
        .expect("the panel's back");
    app.wait(Duration::from_secs(6)).expect("time passes");
    assert!(fake.called("gws_mail_send").is_empty(), "nothing went");
    assert_eq!(
        app.screen().input_text("label:To").unwrap().unwrap(),
        "ana@example.com"
    );
    assert_eq!(
        app.screen().input_text("label:Message").unwrap().unwrap(),
        "Noon?"
    );
}

#[test]
fn a_press_refused_at_send_is_never_held_and_nothing_goes() {
    let (mut app, fake) = start();
    write_one(&mut app);
    *fake.refuse_press.lock().unwrap() = Some("Only a person can do this.".to_owned());
    app.press("label:Send").expect("Send presses");
    app.wait_for_within("text:Only a person can do this.", WAIT)
        .expect("it says so");
    assert!(!app.screen().shows("Sending in a moment."));
    app.wait(Duration::from_secs(6)).expect("time passes");
    assert!(fake.called("gws_mail_send").is_empty());
    assert!(
        app.screen().matches("label:Message").unwrap(),
        "it stays to be read"
    );
}

#[test]
fn closing_the_screen_in_the_hold_sends_nothing() {
    let (mut app, fake) = start();
    write_one(&mut app);
    app.press("label:Send").expect("Send presses");
    app.wait_for_within("label:Take this back", WAIT)
        .expect("the bar offers it");
    drop(app);
    std::thread::sleep(Duration::from_secs(6));
    assert!(fake.called("gws_mail_send").is_empty());
}

#[test]
fn send_with_nobody_to_send_to_says_so() {
    let (mut app, fake) = start();
    app.press("label:Write a message")
        .expect("the rail offers it");
    settle(&mut app);
    app.type_text("label:Message", "Hello")
        .expect("Message takes text");
    app.press("label:Send").expect("Send presses");
    settle(&mut app);
    assert!(app.screen().shows("Add someone to send this to."));
    assert!(fake.called("gws_mail_send_arm").is_empty());
}

#[test]
fn a_reply_starts_from_the_conversation_and_stays_in_it() {
    let (mut app, fake) = start();
    app.press("text:Subject 3").expect("the row presses");
    settle(&mut app);
    app.press("label:Reply to this")
        .expect("the toolbar offers it");
    app.wait_for_within("label:Message", WAIT)
        .expect("the panel opens");
    assert_eq!(
        app.screen().input_text("label:To").unwrap().unwrap(),
        "ana@example.com"
    );
    assert_eq!(
        app.screen().input_text("label:Subject").unwrap().unwrap(),
        "Re: Lunch"
    );
    app.press("label:Send").expect("Send presses");
    wait_for_send(&mut app, &fake);
    let saved = fake.called("gws_mail_draft_save");
    assert_eq!(saved.last().unwrap()["thread"], "t3");
    assert_eq!(saved.last().unwrap()["in_reply_to"], "<a@x>");
}

#[test]
fn typing_saves_a_draft_when_it_rests() {
    let (mut app, fake) = start();
    write_one(&mut app);
    let by = std::time::Instant::now() + Duration::from_secs(8);
    while fake.called("gws_mail_draft_save").is_empty() {
        assert!(std::time::Instant::now() < by, "no save came");
        settle(&mut app);
    }
    assert_eq!(fake.called("gws_mail_draft_save")[0]["body"], "Noon?");
    assert!(fake.called("gws_mail_send").is_empty());
}

#[test]
fn keep_draft_saves_and_delete_asks_first() {
    let (mut app, fake) = start();
    write_one(&mut app);
    app.press("label:Keep draft").expect("it presses");
    app.wait_for_gone("label:Message", WAIT)
        .expect("the panel closes");
    assert!(!fake.called("gws_mail_draft_save").is_empty());

    write_one(&mut app);
    app.press("label:Delete this draft").expect("it presses");
    settle(&mut app);
    assert!(fake.called("gws_mail_draft_delete").is_empty(), "not yet");
    app.press("label:Keep writing").expect("it presses");
    settle(&mut app);
    app.press("label:Keep draft").expect("it presses");
    app.wait_for_gone("label:Message", WAIT)
        .expect("the panel closes");
    assert!(fake.called("gws_mail_draft_delete").is_empty());
}

#[test]
fn a_row_in_drafts_goes_on_from_the_draft_with_its_files() {
    let (mut app, fake) = start();
    pick_label(&mut app, "Drafts");
    fake.newer.lock().unwrap().clear();
    let n = *shown(&app).first().expect("rows show");
    app.press(&format!("text:Subject {n}"))
        .expect("the row presses");
    app.wait_for_within("label:Message", WAIT)
        .expect("the panel opens");
    assert_eq!(
        app.screen().input_text("label:Message").unwrap().unwrap(),
        "Half written"
    );
    assert!(app.screen().shows("a.pdf"));
    app.type_text("label:Message", " and more")
        .expect("Message takes text");
    app.press("label:Keep draft").expect("it presses");
    app.wait_for_gone("label:Message", WAIT)
        .expect("the panel closes");
    let saved = fake.called("gws_mail_draft_save");
    let last = saved.last().expect("a save");
    assert_eq!(last["draft"], "d9");
    assert_eq!(last["keep"][0]["id"], "att-0", "its file goes with it");
    assert_eq!(last["keep"][0]["message"], "r9");
}

#[test]
fn a_suggestion_finishes_the_address_being_typed_and_keeps_the_rest() {
    let (mut app, fake) = start();
    app.press("label:Write a message")
        .expect("the rail offers it");
    settle(&mut app);
    app.type_text("label:To", "ben@example.com, an")
        .expect("To takes text");
    app.wait_for_within("label:Use this address", WAIT)
        .expect("it suggests");
    let asked = fake.called("gws_people_suggest");
    assert_eq!(
        asked.last().unwrap()["query"],
        "an",
        "only the word being typed"
    );
    app.press("label:Use this address").expect("it presses");
    settle(&mut app);
    assert_eq!(
        app.screen().input_text("label:To").unwrap().unwrap(),
        "ben@example.com, ana@example.com, "
    );
    assert!(!app.screen().matches("label:Use this address").unwrap());
}

fn draft(to: &str, thread: Value, new: Value) -> Value {
    json!({"draft": "d7", "revision": "r7", "message_id": "<m7@x>", "from": "owner@example.com", "new": new, "to": to, "cc": "", "bcc": "", "subject": "Lunch", "body": "Noon?", "thread": thread, "attachments": []})
}

fn start_card(reads: Value) -> (Headless, Arc<Fake>) {
    let fake = Arc::new(Fake::default());
    *fake.inline.lock().unwrap() = true;
    *fake.props.lock().unwrap() = json!({"draft": "tk1"});
    *fake.draft.lock().unwrap() = Some(reads);
    let def = gws_screens::screens()
        .iter()
        .find(|s| s.id == "gws.mail")
        .expect("Mail");
    let root = (def.build)(Bridge::new(fake.clone()));
    let mut app = Headless::start(EngineConfig::new(root)).expect("the engine starts");
    app.set_viewport(size(px(420.0), px(400.0)))
        .expect("the viewport sets");
    app.wait_for_within("label:Keep draft", WAIT)
        .expect("the card shows");
    (app, fake)
}

#[test]
fn a_card_shows_everything_send_would_send_and_sends_that_revision() {
    let (mut app, fake) = start_card(draft("ana@example.com", Value::Null, json!([])));
    for line in ["owner@example.com", "ana@example.com", "Lunch", "Noon?"] {
        assert!(app.screen().shows(line), "{line}");
    }
    app.press("label:Send").expect("Send presses");
    app.wait_for_within("label:Take this back", WAIT)
        .expect("the hold shows");
    wait_for_send(&mut app, &fake);
    let sent = &fake.called("gws_mail_send")[0];
    assert_eq!(sent["draft"], "d7");
    assert_eq!(sent["revision"], "r7", "the one that was shown");
    assert_eq!(sent["armed"], "a1");
    assert!(
        fake.called("gws_mail_draft_save").is_empty(),
        "a card writes nothing"
    );
}

#[test]
fn undo_on_a_card_sends_nothing_and_shows_it_again() {
    let (mut app, fake) = start_card(draft("ana@example.com", Value::Null, json!([])));
    app.press("label:Send").expect("Send presses");
    app.wait_for_within("label:Take this back", WAIT)
        .expect("the hold shows");
    app.press("label:Take this back").expect("Undo presses");
    app.wait_for_within("label:Keep draft", WAIT)
        .expect("the card's back");
    app.wait(Duration::from_secs(6)).expect("time passes");
    assert!(fake.called("gws_mail_send").is_empty());
    assert!(app.screen().shows("Noon?"));
}

#[test]
fn the_ais_press_on_a_cards_send_stops_at_the_press() {
    let (mut app, fake) = start_card(draft("ana@example.com", Value::Null, json!([])));
    *fake.refuse_press.lock().unwrap() = Some("Only a person can do this.".to_owned());
    app.press("label:Send").expect("Send presses");
    app.wait_for_within("text:Only a person can do this.", WAIT)
        .expect("it says so");
    app.wait(Duration::from_secs(6)).expect("time passes");
    assert!(fake.called("gws_mail_send").is_empty());
}

#[test]
fn a_draft_changed_after_it_showed_waits_for_a_fresh_read() {
    let (mut app, fake) = start_card(draft("ana@example.com", Value::Null, json!([])));
    *fake.refuse_send.lock().unwrap() = Some("This draft changed after you read it.".to_owned());
    let mut changed = draft("eve@example.com", Value::Null, json!(["eve@example.com"]));
    changed["revision"] = json!("r8");
    *fake.draft.lock().unwrap() = Some(changed);
    app.press("label:Send").expect("Send presses");
    app.wait_for_within(
        "text:This draft changed after you read it.",
        Duration::from_secs(9),
    )
    .expect("it says so");
    assert!(
        app.screen().shows("eve@example.com"),
        "what it says now shows"
    );
    assert_eq!(
        fake.called("gws_mail_send").len(),
        1,
        "and it waits for a press"
    );
}

#[test]
fn many_recipients_or_a_bcc_asks_once_more() {
    let many = "a@x.co, b@x.co, c@x.co, d@x.co, e@x.co, f@x.co";
    let (mut app, fake) = start_card(draft(many, Value::Null, json!([])));
    app.press("label:Send").expect("Send presses");
    settle(&mut app);
    assert!(fake.called("gws_mail_send_arm").is_empty(), "not yet");
    app.press("label:Send it").expect("it asks");
    app.wait_for_within("label:Take this back", WAIT)
        .expect("the hold shows");
    assert_eq!(fake.called("gws_mail_send_arm").len(), 1);

    let mut bcc = draft("ana@example.com", Value::Null, json!([]));
    bcc["bcc"] = json!("spy@example.com");
    let (mut app, fake) = start_card(bcc);
    assert!(app.screen().shows("spy@example.com"), "a Bcc shows");
    app.press("label:Send").expect("Send presses");
    settle(&mut app);
    assert!(fake.called("gws_mail_send_arm").is_empty());
    assert!(app.screen().matches("label:Send it").unwrap());
}

#[test]
fn a_reply_to_someone_new_cant_be_sent_from_the_card() {
    let (app, _) = start_card(draft(
        "new@example.com",
        json!("t1"),
        json!(["new@example.com"]),
    ));
    assert!(
        !app.screen().matches("label:Send").unwrap(),
        "Send stays off"
    );
    assert!(app.screen().matches("label:Open the full review").unwrap());
    assert!(
        app.screen()
            .shows("Replies to someone new. Open the full review to send it.")
    );
}

#[test]
fn keep_draft_on_a_card_deletes_nothing() {
    let (mut app, fake) = start_card(draft("ana@example.com", Value::Null, json!([])));
    app.press("label:Keep draft").expect("it presses");
    app.wait_for_within("text:Kept in Drafts.", WAIT)
        .expect("it settles");
    assert!(fake.called("gws_mail_draft_delete").is_empty());
    assert!(fake.called("gws_mail_send").is_empty());
}

#[test]
fn a_screen_opened_on_a_view_shows_that_view() {
    let fake = Arc::new(Fake::default());
    *fake.props.lock().unwrap() = json!({"view": "STARRED"});
    let def = gws_screens::screens()
        .iter()
        .find(|s| s.id == "gws.mail")
        .expect("Mail");
    let root = (def.build)(Bridge::new(fake.clone()));
    let mut app = Headless::start(EngineConfig::new(root)).expect("the engine starts");
    app.set_viewport(size(px(1000.0), px(600.0)))
        .expect("the viewport sets");
    app.wait_for_within("text:Subject 7", WAIT)
        .expect("the view shows");
    assert_eq!(fake.called("gws_mail_list")[0]["label"], "STARRED");
}
