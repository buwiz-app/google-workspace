//! The Mail screen driven as a person drives it, on a headless engine: no
//! window and no plugin. A fake host answers the screen's tool calls, and can
//! hold an answer back so a test can make answers arrive out of order.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use buwiz_app::{Bridge, BuwizError, Done, Host, Listener};
use nsh_core::prelude::*;
use nsh_core::{MountItem, NodeId};
use nsh_engine::EngineConfig;
use nsh_testing::Headless;
use serde_json::{Map, Value, json};

mod bot_filter;

use bot_filter::Filter;

/// Room for a slow machine, and for Send's five seconds on one, which are fifty
/// ticks that each run late there. It ends on what's looked for, so it costs a
/// passing test nothing.
const WAIT: Duration = Duration::from_secs(30);
const PAGE: usize = 30;
/// Past the next autosave, however the screen's clock falls.
const PAST_AN_AUTOSAVE: Duration = Duration::from_millis(2500);

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
    let attachments = if id == "t2" {
        json!([
            {"name": "Roof quote.docx", "mime": "application/vnd.openxmlformats-officedocument.wordprocessingml.document", "size": 2048, "id": "att-1", "part": "1", "message": "m7"},
            {"name": "Inline logo.png", "mime": "image/png", "size": 80, "id": null, "message": "m7"},
        ])
    } else {
        json!([])
    };
    json!({"page": format!("<p>{id}</p>"), "links": [], "images": i64::from(id == "t1"), "attachments": attachments})
}

#[derive(Default)]
struct Fake {
    /// The active account and every label on the list. Empty is a plugin with
    /// no list, as with one account from an older Bot.
    accounts: Mutex<Vec<(String, bool)>>,
    /// What the sync says of itself.
    status: Mutex<String>,
    /// In place of the person's own labels.
    labels: Mutex<Option<Value>>,
    /// A search finds nothing.
    no_match: Mutex<bool>,
    /// What a mail card reads.
    card: Mutex<Value>,
    /// Google's sign-in went: lists fail as the plugin says so.
    signed_out: Mutex<bool>,
    /// In place of an attachment opening.
    attachment_answer: Mutex<Option<Value>>,
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
    /// Sends get no answer, so no one knows if they went.
    send_lost: Mutex<bool>,
    /// The first send gets no answer, and the second finds its press spent.
    send_lost_then_spent: Mutex<bool>,
    /// Draft saves refused with this, as an answer: a tool's failure has no code
    /// by the time it reaches a screen.
    save_refused: Mutex<Option<String>>,
    /// The plugin asks for a switch and the Bot can't make it.
    switch_fails: Mutex<bool>,
    /// A failed switch's code, when it isn't a sign-in that's gone.
    switch_code: Mutex<Option<&'static str>>,
    save_timeout: Mutex<bool>,
    /// What a save says it now holds as uploads.
    kept: Mutex<Value>,
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

    fn switch_to(&self, label: &str) {
        for row in self.accounts.lock().unwrap().iter_mut() {
            row.1 = row.0 == label;
        }
    }

    /// Answers a held call, once it has come: the screen's thread may be behind.
    /// Calls after it go through: which save comes first is the autosave clock's.
    fn release(&self, name: &str) {
        self.hold.lock().unwrap().retain(|n| n != name);
        let by = std::time::Instant::now() + WAIT;
        loop {
            let mut held = self.held.lock().unwrap();
            if let Some(at) = held.iter().position(|(n, _, _)| n == name) {
                let (_, answer, done) = held.remove(at);
                drop(held);
                return done(Ok(answer));
            }
            drop(held);
            assert!(std::time::Instant::now() < by, "that call is held");
            std::thread::yield_now();
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
            "gws_status" => {
                let rows = self.accounts.lock().unwrap();
                let who = rows.iter().find(|r| r.1).map(|r| r.0.clone());
                (String::new(), json!({"signed_in": true, "subject": who}))
            }
            "gws_accounts" => {
                let rows = self.accounts.lock().unwrap();
                let view =
                    |r: &(String, bool)| json!({"label": r.0, "address": r.0, "active": r.1});
                let rows: Vec<Value> = rows.iter().map(view).collect();
                (String::new(), json!({"accounts": rows}))
            }
            "gws_account_use" if *self.switch_fails.lock().unwrap() => (
                tool.to_owned(),
                json!({"switched": true, "account_use": {"code": self.switch_code.lock().unwrap().unwrap_or("needs_signin"), "message": "Buwiz Bot couldn't switch to that account."}}),
            ),
            "gws_account_use" => {
                self.switch_to(args["label"].as_str().unwrap());
                (tool.to_owned(), json!({"switched": true}))
            }
            "gws_mail_labels" => (
                String::new(),
                self.labels.lock().unwrap().clone().unwrap_or_else(
                    || json!({"labels": [{"id": "Label_1", "name": "Work", "kind": "user"}]}),
                ),
            ),
            "gws_mail_changes" => {
                let changed = std::mem::take(&mut *self.changed.lock().unwrap());
                (
                    String::new(),
                    json!({"status": *self.status.lock().unwrap(), "seq": 0, "changed": changed}),
                )
            }
            "gws_mail_list" if *self.signed_out.lock().unwrap() => {
                return done(Err(BuwizError::new(
                    "tool_error",
                    false,
                    "Sign in to Google first.",
                )));
            }
            "gws_mail_list" if args["query"].is_string() && *self.no_match.lock().unwrap() => (
                String::new(),
                json!({"threads": [], "more": false, "saved": false}),
            ),
            "gws_mail_card" => (String::new(), self.card.lock().unwrap().clone()),
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
            "gws_mail_send_disarm" => (tool.to_owned(), json!({"disarmed": true})),
            "gws_mail_job_start" => (tool.to_owned(), json!({"job": "job1"})),
            "gws_mail_job" => (tool.to_owned(), json!({"done": false, "gone": true})),
            "gws_mail_draft_save" if self.save_timeout.lock().unwrap().to_owned() => {
                return done(Err(BuwizError::new("timeout", true, "no answer")));
            }
            "gws_mail_draft_save" if self.save_refused.lock().unwrap().is_some() => {
                let code = self.save_refused.lock().unwrap().clone().unwrap();
                (tool.to_owned(), json!({"refused": code, "message": "No."}))
            }
            "gws_mail_draft_save" => {
                let mut drafts = self.drafts.lock().unwrap();
                *drafts += 1;
                let id = args["draft"]
                    .as_str()
                    .map_or(format!("d{drafts}"), str::to_owned);
                (
                    tool.to_owned(),
                    json!({"draft": id, "revision": format!("r{drafts}"), "message_id": "<m@x>", "kept": self.kept.lock().unwrap().clone()}),
                )
            }
            "gws_mail_send" if self.refuse_send.lock().unwrap().is_some() => {
                let why = self.refuse_send.lock().unwrap().take().unwrap();
                (
                    tool.to_owned(),
                    json!({"refused": "changed", "message": why}),
                )
            }
            "gws_mail_send"
                if *self.send_lost_then_spent.lock().unwrap() && self.called(tool).len() > 1 =>
            {
                (
                    tool.to_owned(),
                    json!({"refused": "not_allowed", "message": "Only a person's press sends mail."}),
                )
            }
            "gws_mail_send"
                if *self.send_lost.lock().unwrap()
                    || *self.send_lost_then_spent.lock().unwrap() =>
            {
                return done(Err(BuwizError::new("timeout", true, "no answer")));
            }
            "gws_mail_send" => (tool.to_owned(), json!({"sent": true})),
            "gws_mail_draft_delete" => (tool.to_owned(), json!({"deleted": true})),
            "gws_mail_reply_start" => (
                tool.to_owned(),
                json!({"to": ["ana@example.com"], "cc": [], "note": "Replies go to eve@y.co, not the sender, ana@example.com.", "subject": "Re: Lunch", "body": "\n\nOn Monday, Ana wrote:\n> Noon?\n", "thread": args["thread"], "in_reply_to": "<a@x>", "references": "<a@x>"}),
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
            "gws_mail_attachment_open" => (
                tool.to_owned(),
                self.attachment_answer
                    .lock()
                    .unwrap()
                    .clone()
                    .unwrap_or_else(
                        || json!({"opened": true, "name": "Roof quote.docx", "kind": "word"}),
                    ),
            ),
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
    app.type_text("label:To, like name@company.com", "ana@example.com")
        .expect("To takes text");
    app.type_text(
        "label:Subject, the line people see first in their inbox",
        "Lunch",
    )
    .expect("Subject takes text");
    app.type_text(
        "label:Message, the words of this mail as they will be sent",
        "Noon?",
    )
    .expect("Message takes text");
}

fn wait_for_send(app: &mut Headless, fake: &Fake) {
    let by = std::time::Instant::now() + WAIT;
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
        !app.screen()
            .matches("label:Message, the words of this mail as they will be sent")
            .unwrap(),
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
    app.wait_for_within(
        "label:Message, the words of this mail as they will be sent",
        WAIT,
    )
    .expect("the panel's back");
    app.wait(Duration::from_secs(6)).expect("time passes");
    assert!(fake.called("gws_mail_send").is_empty(), "nothing went");
    assert_eq!(
        app.screen()
            .input_text("label:To, like name@company.com")
            .unwrap()
            .unwrap(),
        "ana@example.com"
    );
    assert_eq!(
        app.screen()
            .input_text("label:Message, the words of this mail as they will be sent")
            .unwrap()
            .unwrap(),
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
        app.screen()
            .matches("label:Message, the words of this mail as they will be sent")
            .unwrap(),
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
    app.type_text(
        "label:Message, the words of this mail as they will be sent",
        "Hello",
    )
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
    app.wait_for_within(
        "label:Message, the words of this mail as they will be sent",
        WAIT,
    )
    .expect("the panel opens");
    assert_eq!(
        app.screen()
            .input_text("label:To, like name@company.com")
            .unwrap()
            .unwrap(),
        "ana@example.com"
    );
    assert_eq!(
        app.screen()
            .input_text("label:Subject, the line people see first in their inbox")
            .unwrap()
            .unwrap(),
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
    let by = std::time::Instant::now() + WAIT;
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
    app.wait_for_gone(
        "label:Message, the words of this mail as they will be sent",
        WAIT,
    )
    .expect("the panel closes");
    assert!(!fake.called("gws_mail_draft_save").is_empty());

    write_one(&mut app);
    app.press("label:Delete this draft").expect("it presses");
    settle(&mut app);
    assert!(fake.called("gws_mail_draft_delete").is_empty(), "not yet");
    app.press("label:Keep writing").expect("it presses");
    settle(&mut app);
    app.press("label:Keep draft").expect("it presses");
    app.wait_for_gone(
        "label:Message, the words of this mail as they will be sent",
        WAIT,
    )
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
    app.wait_for_within(
        "label:Message, the words of this mail as they will be sent",
        WAIT,
    )
    .expect("the panel opens");
    assert_eq!(
        app.screen()
            .input_text("label:Message, the words of this mail as they will be sent")
            .unwrap()
            .unwrap(),
        "Half written"
    );
    assert!(app.screen().shows("a.pdf"));
    app.type_text(
        "label:Message, the words of this mail as they will be sent",
        " and more",
    )
    .expect("Message takes text");
    app.press("label:Keep draft").expect("it presses");
    app.wait_for_gone(
        "label:Message, the words of this mail as they will be sent",
        WAIT,
    )
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
    app.type_text("label:To, like name@company.com", "ben@example.com, an")
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
        app.screen()
            .input_text("label:To, like name@company.com")
            .unwrap()
            .unwrap(),
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
    let armed = &fake.called("gws_mail_send_arm")[0];
    assert_eq!(
        (&armed["draft"], &armed["revision"]),
        (&json!("d7"), &json!("r7"))
    );
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
    app.wait_for_within("text:This draft changed after you read it.", WAIT)
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
fn a_cards_second_press_stays_on_the_card() {
    let many = "a@x.co, b@x.co, c@x.co, d@x.co, e@x.co, f@x.co";
    let (mut app, _fake) = start_card(draft(many, Value::Null, json!([])));
    app.set_viewport(size(px(300.0), px(400.0)))
        .expect("the viewport sets");
    app.press("label:Send").expect("Send presses");
    app.wait_for_within("label:Send it", WAIT).expect("it asks");
    settle(&mut app);
    assert!(app.screen().shows("Send it"), "{:?}", app.screen().texts());
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

const UNSURE: &str =
    "text:There's no word on whether this went. Look in Sent before sending it again.";

#[test]
fn a_send_with_no_answer_is_never_written_and_sent_again() {
    let (mut app, fake) = start();
    write_one(&mut app);
    *fake.send_lost.lock().unwrap() = true;
    app.press("label:Send").expect("Send presses");
    app.wait_for_within(UNSURE, WAIT).expect("it says so");
    assert_eq!(
        fake.called("gws_mail_send").len(),
        2,
        "asked once more, for the Sent check"
    );
    *fake.send_lost.lock().unwrap() = false;
    *fake.save_refused.lock().unwrap() = Some("not_found".to_owned());
    let saves = fake.called("gws_mail_draft_save").len();
    app.press("label:Send").expect("Send presses");
    app.wait_for_within(
        "text:That draft is gone, and the message may have gone out. Look in Sent, and press Send again only if it isn't there.",
        WAIT,
    )
        .expect("it says so");
    assert_eq!(
        fake.called("gws_mail_draft_save").len(),
        saves + 1,
        "not written anew"
    );
    app.wait(Duration::from_secs(6)).expect("time passes");
    assert_eq!(fake.called("gws_mail_send").len(), 2, "and not sent again");
}

#[test]
fn a_draft_that_had_already_gone_is_reported_sent() {
    let (mut app, fake) = start();
    write_one(&mut app);
    *fake.save_refused.lock().unwrap() = Some("already_sent".to_owned());
    app.press("label:Send").expect("Send presses");
    app.wait_for_within("text:Message sent.", WAIT)
        .expect("it says so");
    assert!(fake.called("gws_mail_send").is_empty());
}

#[test]
fn what_is_typed_after_send_is_pressed_is_not_shown_as_sent() {
    let (mut app, fake) = start();
    write_one(&mut app);
    fake.hold("gws_mail_draft_save");
    app.press("label:Send").expect("Send presses");
    settle(&mut app);
    let _ = app.type_text(
        "label:Message, the words of this mail as they will be sent",
        " and more",
    );
    settle(&mut app);
    fake.release("gws_mail_draft_save");
    app.wait_for_within("label:Take this back", WAIT)
        .expect("the hold shows");
    app.press("label:Take this back").expect("Undo presses");
    app.wait_for_within(
        "label:Message, the words of this mail as they will be sent",
        WAIT,
    )
    .expect("the panel's back");
    assert_eq!(
        app.screen()
            .input_text("label:Message, the words of this mail as they will be sent")
            .unwrap()
            .unwrap(),
        "Noon?",
        "what's held is what went to the draft"
    );
}

#[test]
fn someone_new_on_a_send_from_the_panel_takes_a_second_press() {
    let (mut app, fake) = start();
    write_one(&mut app);
    *fake.draft.lock().unwrap() = Some(draft(
        "eve@example.com",
        Value::Null,
        json!(["eve@example.com"]),
    ));
    app.press("label:Send").expect("Send presses");
    app.wait_for_within("text:New to you: eve@example.com. Send it?", WAIT)
        .expect("it asks");
    assert!(!app.screen().shows("Sending in a moment."));
    let arms = fake.called("gws_mail_send_arm").len();
    app.press("label:Send it").expect("it presses");
    wait_for_send(&mut app, &fake);
    assert_eq!(
        fake.called("gws_mail_send_arm").len(),
        arms + 1,
        "a press of its own"
    );
}

/// While a press stands the AI is off the draft and no account can be switched
/// to: not even the way back the refusal names.
#[test]
fn a_send_refused_at_its_save_gives_its_press_back() {
    let (mut app, fake) = start();
    write_one(&mut app);
    *fake.save_refused.lock().unwrap() = Some("account_changed".to_owned());
    app.press("label:Send").expect("Send presses");
    settle(&mut app);
    assert_eq!(
        fake.called("gws_mail_send_disarm"),
        [json!({"armed": "a1"})]
    );
}

#[test]
fn a_question_about_someone_new_gives_the_press_back_while_it_asks() {
    let (mut app, fake) = start();
    write_one(&mut app);
    *fake.draft.lock().unwrap() = Some(draft(
        "eve@example.com",
        Value::Null,
        json!(["eve@example.com"]),
    ));
    app.press("label:Send").expect("Send presses");
    app.wait_for_within("text:New to you: eve@example.com. Send it?", WAIT)
        .expect("it asks");
    settle(&mut app);
    assert_eq!(
        fake.called("gws_mail_send_disarm"),
        [json!({"armed": "a1"})]
    );
}

/// Its press stops any older one on that draft before its own save comes back.
#[test]
fn a_send_of_a_saved_letter_names_its_draft_as_it_arms() {
    let (mut app, fake) = start();
    write_one(&mut app);
    app.wait(PAST_AN_AUTOSAVE).expect("time passes");
    settle(&mut app);
    assert!(!fake.called("gws_mail_draft_save").is_empty(), "it's saved");
    app.press("label:Send").expect("Send presses");
    settle(&mut app);
    assert_eq!(fake.called("gws_mail_send_arm")[0]["draft"], "d1");
}

#[test]
fn a_change_after_the_question_asks_again() {
    let (mut app, fake) = start();
    write_one(&mut app);
    *fake.draft.lock().unwrap() = Some(draft(
        "eve@example.com",
        Value::Null,
        json!(["eve@example.com"]),
    ));
    app.press("label:Send").expect("Send presses");
    app.wait_for_within("label:Send it", WAIT).expect("it asks");
    app.type_text("label:To, like name@company.com", ", mallory@example.com")
        .expect("To takes text");
    settle(&mut app);
    assert!(!app.screen().matches("label:Send it").unwrap());
}

#[test]
fn a_reply_says_when_it_wont_reach_the_sender() {
    let (mut app, _) = start();
    app.press("text:Subject 3").expect("the row presses");
    settle(&mut app);
    app.press("label:Reply to this")
        .expect("the toolbar offers it");
    app.wait_for_within(
        "text:Replies go to eve@y.co, not the sender, ana@example.com.",
        WAIT,
    )
    .expect("it says so");
}

fn open_a_draft(app: &mut Headless) {
    pick_label(app, "Drafts");
    let n = *shown(app).first().expect("rows show");
    app.press(&format!("text:Subject {n}"))
        .expect("the row presses");
    app.wait_for_within(
        "label:Message, the words of this mail as they will be sent",
        WAIT,
    )
    .expect("the panel opens");
}

#[test]
fn a_draft_put_away_leaves_the_reader_empty_not_opening() {
    let (mut app, _fake) = start();
    open_a_draft(&mut app);
    app.press("label:Keep draft").expect("it presses");
    app.wait_for_within("text:Pick a conversation to read it", WAIT)
        .expect("nothing is opening");
    assert!(!app.screen().shows("Opening…"));
}

#[test]
fn a_slow_save_the_plugin_lost_says_so_and_stops_waiting() {
    let (mut app, fake) = start();
    open_a_draft(&mut app);
    *fake.save_timeout.lock().unwrap() = true;
    app.press("label:Send").expect("Send presses");
    app.wait_for_within("text:That save was lost. Try again.", WAIT)
        .expect("it says so");
    assert!(fake.called("gws_mail_send").is_empty());
}

#[test]
fn a_kept_file_is_an_upload_after_the_save_that_moved_it() {
    let (mut app, fake) = start();
    open_a_draft(&mut app);
    *fake.kept.lock().unwrap() = json!([{"id": "att-0", "upload": "up7"}]);
    app.type_text(
        "label:Message, the words of this mail as they will be sent",
        " one",
    )
    .expect("Message takes text");
    app.press("label:Send").expect("Send presses");
    app.wait_for_within("label:Take this back", WAIT)
        .expect("the hold shows");
    app.press("label:Take this back").expect("Undo presses");
    app.wait_for_within(
        "label:Message, the words of this mail as they will be sent",
        WAIT,
    )
    .expect("the panel's back");
    app.type_text(
        "label:Message, the words of this mail as they will be sent",
        " two",
    )
    .expect("Message takes text");
    app.press("label:Keep draft").expect("it presses");
    app.wait_for_gone(
        "label:Message, the words of this mail as they will be sent",
        WAIT,
    )
    .expect("the panel closes");
    let saved = fake.called("gws_mail_draft_save");
    let last = saved.last().unwrap();
    assert_eq!(last["attachments"], json!(["up7"]));
    assert_eq!(last["keep"], json!([]), "not the old message's id");
}

#[test]
fn a_double_click_on_send_is_one_press() {
    let (mut app, fake) = start();
    write_one(&mut app);
    fake.hold("gws_mail_draft_save");
    app.press("label:Send").expect("Send presses");
    settle(&mut app);
    let _ = app.press("label:Send");
    settle(&mut app);
    assert_eq!(fake.called("gws_mail_send_arm").len(), 1, "one press");
    fake.release("gws_mail_draft_save");
    wait_for_send(&mut app, &fake);
    assert_eq!(fake.called("gws_mail_send").len(), 1);
}

#[test]
fn nothing_can_be_attached_once_send_is_pressed() {
    let (mut app, fake) = start();
    write_one(&mut app);
    fake.hold("gws_mail_draft_save");
    app.press("label:Send").expect("Send presses");
    settle(&mut app);
    let _ = app.press("label:Attach a file");
    settle(&mut app);
    assert!(
        !app.screen().shows("This screen can't reach your files."),
        "the press does nothing at all"
    );
    fake.release("gws_mail_draft_save");
}

// --- Attachments -------------------------------------------------------------

/// Opens the conversation that has files on it.
fn with_attachments() -> (Headless, Arc<Fake>) {
    let (mut app, fake) = start();
    app.press("text:Subject 2").expect("the row presses");
    app.wait_for_within("text:Roof quote.docx", WAIT)
        .expect("its files show");
    settle(&mut app);
    (app, fake)
}

#[test]
fn an_attachment_row_opens_with_a_press_and_says_what_it_needs_to() {
    let (mut app, fake) = with_attachments();
    assert!(fake.called("gws_mail_attachment_open").is_empty());
    app.press("label:Open this attachment").expect("it presses");
    settle(&mut app);
    assert_eq!(
        fake.called("gws_mail_attachment_open"),
        [json!({"message": "m7", "part": "1"})]
    );
    assert!(!app.screen().shows("Opening…"), "the answer put it away");
}

#[test]
fn an_inline_file_with_no_id_shows_with_no_press() {
    let (app, _) = with_attachments();
    assert!(app.screen().shows("Inline logo.png"));
    assert_eq!(app.screen().count("label:Open this attachment").unwrap(), 1);
}

#[test]
fn a_refused_attachment_says_why() {
    let (mut app, fake) = with_attachments();
    *fake.attachment_answer.lock().unwrap() = Some(
        json!({"refused": "unsupported", "message": "This kind of file can't be opened here."}),
    );
    app.press("label:Open this attachment").expect("it presses");
    settle(&mut app);
    assert!(
        app.screen()
            .shows("This kind of file can't be opened here.")
    );
}

#[test]
fn no_label_on_an_attachment_holds_its_name() {
    let (app, _) = with_attachments();
    for secret in ["Roof", "quote", "docx", "logo"] {
        assert!(
            !app.screen()
                .matches(&format!("label:/(?i){secret}/"))
                .unwrap(),
            "a label holds {secret}"
        );
    }
}

#[test]
fn another_conversation_has_no_files_left_from_the_last() {
    let (mut app, _) = with_attachments();
    app.press("text:Subject 3").expect("the row presses");
    settle(&mut app);
    assert!(!app.screen().shows("Roof quote.docx"));
}

/// A view's parent, its left edge there and its width, as the engine gave them.
type Placed = (Option<NodeId>, f32, f32);

/// Mail's two columns are fixed and it has no narrow form, so this stops where
/// the reader still has room: the press has to stay whole in what's left of it.
#[test]
fn the_open_press_stays_whole_as_the_reader_narrows() {
    let views: Arc<Mutex<HashMap<NodeId, Placed>>> = Arc::default();
    let def = gws_screens::screens()
        .iter()
        .find(|s| s.id == "gws.mail")
        .expect("Mail");
    let root = (def.build)(Bridge::new(Arc::new(Fake::default())));
    let mut app = Headless::start(EngineConfig::new(root)).expect("the engine starts");
    // from the first view on, so each one's way up to the screen is known
    let seen = views.clone();
    app.observe(move |_, batch| {
        let mut views = seen.lock().unwrap();
        for item in &batch.items {
            match item {
                MountItem::Insert { parent, child, .. } => {
                    views.entry(*child).or_default().0 = Some(*parent);
                }
                MountItem::Frame { id, frame } => {
                    let view = views.entry(*id).or_default();
                    (view.1, view.2) = (frame.origin.x.0, frame.size.width.0);
                }
                _ => {}
            }
        }
    });
    app.set_viewport(size(px(1000.0), px(600.0)))
        .expect("the viewport sets");
    app.wait_for_within("text:Subject 2", WAIT)
        .expect("the inbox");
    app.press("text:Subject 2").expect("the row presses");
    app.wait_for_within("text:Roof quote.docx", WAIT)
        .expect("its files show");
    settle(&mut app);
    for width in [900.0, 800.0, 700.0, 640.0, 620.0] {
        app.set_viewport(size(px(width), px(600.0)))
            .expect("the viewport sets");
        settle(&mut app);
        let id = app
            .screen()
            .press_target("label:Open this attachment")
            .unwrap()
            .unwrap_or_else(|| panic!("it went at {width}"));
        let views = views.lock().unwrap();
        let mut at = id;
        let mut left = 0.0;
        while let Some((parent, x, _)) = views.get(&at) {
            left += x;
            let Some(parent) = parent else { break };
            at = *parent;
        }
        let right = left + views[&id].2;
        assert!(
            left >= -0.5 && right <= width + 0.5,
            "at {width} it runs from {left} to {right}"
        );
    }
}

// --- Empty states ------------------------------------------------------------

fn start_ok() -> (Headless, Arc<Fake>) {
    let (mut app, fake) = start();
    *fake.status.lock().unwrap() = "ok".to_owned();
    *fake.changed.lock().unwrap() = true;
    app.wait(Duration::from_secs(16)).expect("the watch looks");
    (app, fake)
}

#[test]
fn every_empty_list_says_which_it_is_and_the_reader_asks_for_nothing() {
    let (mut app, _) = start_ok();
    assert!(app.screen().shows("Pick a conversation to read it"));
    for (name, line) in [
        ("Trash", "Trash is empty."),
        ("Sent", "Nothing sent yet. Press Write to start a message."),
        ("Important", "Nothing marked important."),
        ("Work", "No conversations have this label."),
    ] {
        pick_label(&mut app, name);
        assert!(
            app.screen().shows(line),
            "{name}: {:?}",
            app.screen().texts()
        );
        assert!(
            !app.screen().shows("Pick a conversation to read it"),
            "{name} has nothing to pick"
        );
        assert!(app.screen().shows("Nothing to read here."), "{name}");
    }
    pick_label(&mut app, "Inbox");
    app.wait_for_within("text:Subject 0", WAIT).expect("back");
    settle(&mut app);
    assert!(app.screen().shows("Pick a conversation to read it"));
}

#[test]
fn an_empty_list_before_the_first_sync_says_it_is_coming() {
    let (mut app, _) = start();
    pick_label(&mut app, "Trash");
    assert!(app.screen().shows("Your mail is on its way from Google…"));
    assert!(!app.screen().shows("Trash is empty."));
}

#[test]
fn a_search_that_finds_nothing_says_so_and_offers_nothing_to_pick() {
    let (mut app, fake) = start();
    *fake.no_match.lock().unwrap() = true;
    app.type_text("text:Search mail", "zzz").expect("it types");
    app.submit("text:zzz").expect("it submits");
    settle(&mut app);
    assert!(app.screen().shows("No mail matches that. Try other words."));
    assert!(app.screen().shows("Nothing to read here."));
}

#[test]
fn a_person_with_no_labels_of_their_own_is_told_so() {
    let (mut app, fake) = start();
    *fake.labels.lock().unwrap() = Some(json!({"labels": []}));
    *fake.changed.lock().unwrap() = true;
    app.wait(Duration::from_secs(16)).expect("the watch looks");
    press_row(&mut app);
    app.press("label:Choose a label").expect("it presses");
    settle(&mut app);
    assert!(
        app.screen()
            .shows("No labels of your own yet. Make one in Gmail and it shows here.")
    );
}

fn press_row(app: &mut Headless) {
    app.press("text:Subject 2").expect("the row presses");
    settle(app);
}

fn card_of(fake: Arc<Fake>) -> (Headless, Filter) {
    *fake.inline.lock().unwrap() = true;
    *fake.props.lock().unwrap() = json!({"search": "tk1"});
    audited(&fake, 420.0, 400.0)
}

#[test]
fn a_card_with_nothing_left_in_it_says_so() {
    let fake = Arc::new(Fake::default());
    *fake.card.lock().unwrap() = json!({"rows": [], "more": false, "saved": false});
    let (mut app, _) = card_of(fake);
    app.wait_for_within("text:No mail matches this now.", WAIT)
        .expect("it says so");
}

/// Nothing new is wider than the reader's room: Mail has no narrow form, so
/// this goes as far as its reader does.
#[test]
fn the_empty_lines_stay_in_their_columns_as_the_reader_narrows() {
    let fake = Arc::new(Fake::default());
    *fake.status.lock().unwrap() = "ok".to_owned();
    let (mut app, filter) = audited(&fake, 1000.0, 600.0);
    app.wait_for_within("text:Subject 2", WAIT)
        .expect("the inbox");
    pick_label(&mut app, "Sent");
    app.wait(Duration::from_secs(16)).expect("the watch looks");
    let lines = [
        "Nothing sent yet. Press Write to start a message.",
        "Nothing to read here.",
    ];
    for width in [1000.0, 900.0, 800.0, 700.0, 640.0, 620.0] {
        app.set_viewport(size(px(width), px(600.0)))
            .expect("the viewport sets");
        settle(&mut app);
        for line in lines {
            filter.assert_inside(line, width);
        }
    }
}

// --- What the AI can reach, and what labels hold ------------------------------

/// What the Bot's filter names on any Mail screen, and why each is safe. Everything
/// else, and every field, stays unnamed: a new control has to be added here, and
/// so has to say why.
const MAY_REACH: [&str; 5] = [
    // the first thing it does is `gws_mail_send_arm`, which the Bot refuses the AI
    "Send",
    "Send it",
    // puts the person's letter away as it stands
    "Keep draft",
    // takes back a question and changes nothing in the letter
    "Keep writing",
    "Not yet",
];

const SECRETS: [&str; 13] = [
    "Sender 2",
    "Subject 2",
    "Sender 0",
    "Subject 0",
    "Work",
    "Roof quote",
    "a.pdf",
    "ana@example.com",
    "Ana Reyes",
    "ben@example.com",
    "Half written",
    "Saved one",
    "Noon?",
];

fn audited(fake: &Arc<Fake>, width: f32, height: f32) -> (Headless, Filter) {
    let def = gws_screens::screens()
        .iter()
        .find(|s| s.id == "gws.mail")
        .expect("Mail");
    let root = (def.build)(Bridge::new(fake.clone()));
    let mut app = Headless::start(EngineConfig::new(root)).expect("the engine starts");
    let filter = Filter::watch(&mut app);
    app.set_viewport(size(px(width), px(height)))
        .expect("the viewport sets");
    (app, filter)
}

fn card_with(reads: Value) -> (Headless, Filter) {
    let fake = Arc::new(Fake::default());
    *fake.inline.lock().unwrap() = true;
    *fake.props.lock().unwrap() = json!({"draft": "tk1"});
    *fake.draft.lock().unwrap() = Some(reads);
    let (mut app, filter) = audited(&fake, 420.0, 400.0);
    app.wait_for_within("label:Keep draft", WAIT)
        .expect("the card shows");
    (app, filter)
}

/// Every state of every Mail screen and card, each handed to `look`.
fn every_state(mut look: impl FnMut(&Headless, &Filter, &str)) {
    let fake = Arc::new(Fake::default());
    let (mut app, filter) = audited(&fake, 1000.0, 600.0);
    app.wait_for_within("text:Subject 0", WAIT)
        .expect("the inbox");
    settle(&mut app);
    look(&app, &filter, "the inbox");

    press_row(&mut app);
    app.wait_for_within("text:Roof quote.docx", WAIT)
        .expect("its files");
    look(&app, &filter, "a conversation");
    app.press("label:Choose a label").expect("it presses");
    settle(&mut app);
    assert!(app.screen().shows("Work"), "the audit sees the labels");
    look(&app, &filter, "the label picker");
    app.press("label:Archive this").expect("it presses");
    app.wait_for_within("label:Undo that", WAIT).expect("undo");
    look(&app, &filter, "what was done, and its undo");
    app.press_key("shift+/").expect("the key presses");
    settle(&mut app);
    look(&app, &filter, "the keys");
    app.press_key("shift+/").expect("the key presses");

    app.press("label:Write a message").expect("it presses");
    settle(&mut app);
    assert!(filter.unnamed_fields() >= 4, "the audit sees the fields");
    look(&app, &filter, "a new message");
    app.press("label:Add Cc or Bcc").expect("it presses");
    settle(&mut app);
    app.type_text("label:To, like name@company.com", "an")
        .expect("it types");
    app.wait_for_within("label:Use this address", WAIT)
        .expect("it suggests");
    assert!(filter.unnamed_fields() >= 6, "the audit sees the fields");
    look(&app, &filter, "a new message, with suggestions");
    app.press("label:Use this address").expect("it presses");
    app.type_text(
        "label:Subject, the line people see first in their inbox",
        "Lunch",
    )
    .expect("it types");
    app.type_text(
        "label:Message, the words of this mail as they will be sent",
        "Noon?",
    )
    .expect("it types");
    settle(&mut app);
    app.press("label:Delete this draft").expect("it presses");
    settle(&mut app);
    assert!(app.screen().shows("Delete this draft for good?"));
    look(&app, &filter, "asked to delete");
    app.press("label:Keep writing").expect("it presses");
    settle(&mut app);

    *fake.draft.lock().unwrap() = Some(draft(
        "ana@example.com",
        Value::Null,
        json!(["ana@example.com"]),
    ));
    app.press("label:Send").expect("it presses");
    app.wait_for_within("label:Send it", WAIT).expect("it asks");
    look(&app, &filter, "asked about someone new");
    app.press("label:Send it").expect("it presses");
    app.wait_for_within("label:Take this back", WAIT)
        .expect("the hold");
    look(&app, &filter, "the hold");
    app.press("label:Take this back").expect("it presses");
    settle(&mut app);
    look(&app, &filter, "taken back");
    app.press("label:Keep draft").expect("it presses");
    settle(&mut app);
    *fake.draft.lock().unwrap() = None;

    open_a_draft(&mut app);
    assert!(app.screen().shows("a.pdf"), "the audit sees the attachment");
    look(&app, &filter, "a saved draft");
    app.press("label:Keep draft").expect("it presses");
    settle(&mut app);

    for name in ["Trash", "Sent", "Starred", "Important", "Work"] {
        pick_label(&mut app, name);
        look(&app, &filter, name);
    }
    *fake.no_match.lock().unwrap() = true;
    app.type_text("text:Search mail", "zzz").expect("it types");
    app.submit("text:zzz").expect("it submits");
    settle(&mut app);
    look(&app, &filter, "a search with nothing");

    *fake.signed_out.lock().unwrap() = true;
    pick_label(&mut app, "Drafts");
    app.wait_for_within("text:Connect your Google account", WAIT)
        .expect("the way in");
    look(&app, &filter, "signed out");

    let fake = Arc::new(Fake::default());
    *fake.card.lock().unwrap() = json!({"rows": [{"sender": "Sender 2", "subject": "Subject 2", "at": 0, "ticket": "tk2"}], "more": false, "saved": true});
    let (mut app, filter) = card_of(fake);
    app.wait_for_within("text:Sender 2", WAIT)
        .expect("the card");
    look(&app, &filter, "a mail card");
    let fake = Arc::new(Fake::default());
    *fake.card.lock().unwrap() = json!({"rows": [], "more": false, "saved": false});
    let (mut app, filter) = card_of(fake);
    app.wait_for_within("text:No mail matches this now.", WAIT)
        .expect("the card");
    look(&app, &filter, "an empty mail card");

    let (app, filter) = card_with(draft("ana@example.com", Value::Null, json!([])));
    look(&app, &filter, "a draft card");
    let (mut app, filter) = card_with(draft(
        "ana@example.com",
        Value::Null,
        json!(["ana@example.com"]),
    ));
    app.press("label:Send").expect("it presses");
    app.wait_for_within("label:Send it", WAIT).expect("it asks");
    look(&app, &filter, "a draft card that asks");
    let (app, filter) = card_with(draft(
        "ana@example.com",
        json!("t1"),
        json!(["ana@example.com"]),
    ));
    assert!(app.screen().shows("Open full review"));
    look(&app, &filter, "a draft card to a stranger");
}

#[test]
fn no_label_in_any_state_holds_a_persons_data() {
    let mut states = 0;
    every_state(|_, filter, at| {
        filter.assert_no_data_in_labels(&SECRETS, at);
        states += 1;
    });
    assert!(states >= 20, "{states}");
}

/// The AI's way in is the draft: `gws_mail_draft_save`, which the person reads
/// before Send. Nothing on a screen may be its way in as well.
#[test]
fn nothing_a_press_would_send_can_be_reached() {
    bot_filter::assert_the_copy_works();
    let mut reached = BTreeSet::new();
    every_state(|_, filter, at| {
        filter.assert_stages_nothing(&MAY_REACH, at);
        reached.extend(filter.reachable().into_iter().map(|(_, name)| name));
    });
    // or the audit is looking at nothing
    assert!(reached.contains("Send"), "{reached:?}");
    assert!(reached.contains("Keep draft"), "{reached:?}");
}

/// The first try may still be going, so a spent press says nothing about the mail.
#[test]
fn a_second_try_that_finds_its_press_spent_stays_unsure() {
    let (mut app, fake) = start();
    write_one(&mut app);
    *fake.send_lost_then_spent.lock().unwrap() = true;
    app.press("label:Send").expect("Send presses");
    app.wait_for_within(UNSURE, WAIT).expect("it says so");
    assert!(!app.screen().shows("Only a person's press sends mail."));
    assert_eq!(fake.called("gws_mail_send").len(), 2);

    *fake.send_lost_then_spent.lock().unwrap() = false;
    *fake.save_refused.lock().unwrap() = Some("not_found".to_owned());
    app.press("label:Send").expect("Send presses");
    app.wait_for_within(
        "text:That draft is gone, and the message may have gone out. Look in Sent, and press Send again only if it isn't there.",
        WAIT,
    )
    .expect("the letter came back as unsure");
}

#[test]
fn undo_gives_the_press_back_so_the_draft_is_free_again() {
    let (mut app, fake) = start_card(draft("ana@example.com", Value::Null, json!([])));
    app.press("label:Send").expect("Send presses");
    app.wait_for_within("label:Take this back", WAIT)
        .expect("the hold shows");
    assert!(fake.called("gws_mail_send_disarm").is_empty());
    app.press("label:Take this back").expect("Undo presses");
    app.wait_for_within("label:Keep draft", WAIT)
        .expect("the card's back");
    assert_eq!(
        fake.called("gws_mail_send_disarm"),
        [json!({"armed": "a1"})]
    );
}

#[test]
fn a_double_click_on_a_cards_send_is_one_press() {
    let (mut app, fake) = start_card(draft("ana@example.com", Value::Null, json!([])));
    fake.hold("gws_mail_send_arm");
    app.press("label:Send").expect("Send presses");
    settle(&mut app);
    let _ = app.press("label:Send");
    settle(&mut app);
    assert_eq!(fake.called("gws_mail_send_arm").len(), 1, "one press");
    fake.release("gws_mail_send_arm");
    wait_for_send(&mut app, &fake);
    app.wait_for_within("text:Message sent.", WAIT)
        .expect("the bar says so");
    settle(&mut app);
    assert_eq!(fake.called("gws_mail_send").len(), 1);
    assert!(app.screen().shows("Message sent."), "nothing came after it");
}

#[test]
fn a_card_whose_press_failed_can_be_pressed_again() {
    let (mut app, fake) = start_card(draft("ana@example.com", Value::Null, json!([])));
    *fake.refuse_press.lock().unwrap() = Some("Only a person can do this.".to_owned());
    app.press("label:Send").expect("Send presses");
    app.wait_for_within("text:Only a person can do this.", WAIT)
        .expect("it says so");
    *fake.refuse_press.lock().unwrap() = None;
    app.press("label:Send").expect("Send presses");
    app.wait_for_within("label:Take this back", WAIT)
        .expect("the hold shows");
}

/// The tick that used to queue a save behind Send, whose save then waited for an
/// answer that never comes: the panel never came back.
#[test]
fn no_autosave_is_left_behind_a_send() {
    let (mut app, fake) = start();
    write_one(&mut app);
    fake.hold("gws_mail_draft_save");
    app.press("label:Send").expect("Send presses");
    app.wait(PAST_AN_AUTOSAVE).expect("time passes");
    fake.release("gws_mail_draft_save");
    app.wait_for_within("label:Take this back", WAIT)
        .expect("the hold shows");
    app.press("label:Take this back").expect("Undo presses");
    app.wait_for_within(
        "label:Message, the words of this mail as they will be sent",
        WAIT,
    )
    .expect("the panel's back");
    assert_eq!(fake.called("gws_mail_draft_save").len(), 1);
}

/// One queued before the letter left: the check that counts is when it runs.
#[test]
fn an_autosave_queued_behind_keep_draft_saves_no_blank_draft() {
    let (mut app, fake) = start();
    write_one(&mut app);
    fake.hold("gws_mail_draft_save");
    app.press("label:Keep draft").expect("it presses");
    app.wait(PAST_AN_AUTOSAVE).expect("time passes");
    fake.release("gws_mail_draft_save");
    app.wait_for_gone(
        "label:Message, the words of this mail as they will be sent",
        WAIT,
    )
    .expect("the panel closes");
    settle(&mut app);
    let saved = fake.called("gws_mail_draft_save");
    assert_eq!(saved.len(), 1, "{saved:?}");
}

/// One queued ahead of Send: the letter is Send's by the time it runs.
#[test]
fn an_autosave_queued_before_send_leaves_the_letter_to_it() {
    let (mut app, fake) = start();
    write_one(&mut app);
    *fake.save_refused.lock().unwrap() = Some("failed".to_owned());
    fake.hold("gws_mail_draft_save");
    app.press("label:Keep draft").expect("it presses");
    app.wait(PAST_AN_AUTOSAVE).expect("time passes");
    app.press("label:Send").expect("Send presses");
    *fake.save_refused.lock().unwrap() = None;
    fake.release("gws_mail_draft_save");
    app.wait_for_within("label:Take this back", WAIT)
        .expect("the hold shows");
    let saved = fake.called("gws_mail_draft_save");
    assert_eq!(saved.len(), 2, "{saved:?}");
    assert_eq!(saved[1]["armed"], "a1", "the next save was Send's own");
}

/// Send pressed with an autosave out: its own save follows, with the press on it.
#[test]
fn a_send_pressed_during_an_autosave_saves_after_it() {
    let (mut app, fake) = start();
    write_one(&mut app);
    fake.hold("gws_mail_draft_save");
    let by = std::time::Instant::now() + WAIT;
    while fake.called("gws_mail_draft_save").is_empty() {
        assert!(std::time::Instant::now() < by, "no save came");
        settle(&mut app);
    }
    app.press("label:Send").expect("Send presses");
    fake.release("gws_mail_draft_save");
    app.wait_for_within("label:Take this back", WAIT)
        .expect("the hold shows");
    let saved = fake.called("gws_mail_draft_save");
    assert_eq!(saved.len(), 2, "{saved:?}");
    assert_eq!(saved[1]["armed"], "a1");
}

const ONE: &str = "one@example.com";
const TWO: &str = "two@example.com";

/// Mail on two accounts, with the first active and its address read.
fn two_accounts() -> (Headless, Arc<Fake>) {
    let (mut app, fake) = start();
    *fake.accounts.lock().unwrap() = vec![(ONE.to_owned(), true), (TWO.to_owned(), false)];
    app.wait_for_within(&format!("text:{ONE}"), Duration::from_secs(20))
        .expect("the next check finds the list");
    (app, fake)
}

#[test]
fn one_account_has_nothing_to_choose() {
    let (mut app, fake) = start();
    *fake.accounts.lock().unwrap() = vec![(ONE.to_owned(), true)];
    let by = std::time::Instant::now() + Duration::from_secs(20);
    while fake.called("gws_accounts").len() < 2 && std::time::Instant::now() < by {
        settle(&mut app);
    }
    assert!(fake.called("gws_accounts").len() >= 2, "it was asked");
    assert!(!app.screen().matches("label:Switch account").unwrap());
}

#[test]
fn pressing_another_account_switches_and_starts_over_on_its_inbox() {
    let (mut app, fake) = two_accounts();
    pick_label(&mut app, "Work");
    settle(&mut app);
    assert!(!app.screen().shows(TWO), "not until the row is pressed");
    let before = fake.called("gws_mail_list").len();
    app.press(&format!("text:{ONE}"))
        .expect("the account row opens");
    app.wait_for_within(&format!("text:{TWO}"), WAIT)
        .expect("the other account shows");
    app.press(&format!("text:{TWO}")).expect("it presses");
    app.wait_for_gone(&format!("text:{ONE}"), WAIT)
        .expect("the row names the new one");
    assert_eq!(fake.called("gws_account_use"), vec![json!({"label": TWO})]);
    settle(&mut app);
    let lists = fake.called("gws_mail_list");
    assert!(lists.len() > before, "the list is read again");
    assert_eq!(lists.last().unwrap()["label"], "INBOX");
}

#[test]
fn the_ai_can_neither_read_nor_press_an_account_row() {
    let (mut app, _fake) = two_accounts();
    let filter = Filter::watch(&mut app);
    app.press(&format!("text:{ONE}"))
        .expect("the account row opens");
    app.wait_for_within(&format!("text:{TWO}"), WAIT)
        .expect("the other account shows");
    filter.assert_no_data_in_labels(&[ONE, TWO], "the account rows");
    assert!(
        !filter
            .reachable()
            .iter()
            .any(|(_, name)| name == "Switch account"),
        "the row's text isn't its label, so the Bot leaves it unnamed"
    );
}

#[test]
fn a_switch_the_bot_could_not_make_says_so_and_stays_where_it_was() {
    let (mut app, fake) = two_accounts();
    *fake.switch_fails.lock().unwrap() = true;
    app.press(&format!("text:{ONE}"))
        .expect("the account row opens");
    app.wait_for_within(&format!("text:{TWO}"), WAIT)
        .expect("the other account shows");
    app.press(&format!("text:{TWO}")).expect("it presses");
    app.wait_for_within("text:Buwiz Bot couldn't switch to that account.", WAIT)
        .expect("it says so");
    assert!(app.screen().shows(ONE), "still this one");
}

#[test]
fn a_failed_switch_still_says_why_once_that_account_leaves_the_list() {
    let (mut app, fake) = two_accounts();
    *fake.switch_fails.lock().unwrap() = true;
    app.press(&format!("text:{ONE}"))
        .expect("the account row opens");
    app.wait_for_within(&format!("text:{TWO}"), WAIT)
        .expect("the other account shows");
    app.press(&format!("text:{TWO}")).expect("it presses");
    app.wait_for_within("text:Buwiz Bot couldn't switch to that account.", WAIT)
        .expect("it says so");
    // the account check finds the sign-in gone, so one account is left
    *fake.accounts.lock().unwrap() = vec![(ONE.to_owned(), true)];
    app.wait_for_within(
        "text:Buwiz Bot couldn't switch to that account. Sign in to it again on this app's page.",
        Duration::from_secs(20),
    )
    .expect("the person still hears why, and what to do");
    assert!(!app.screen().matches("label:Switch account").unwrap());
}

#[test]
fn a_switch_the_bot_couldnt_make_now_doesnt_ask_for_a_sign_in() {
    let (mut app, fake) = two_accounts();
    *fake.switch_fails.lock().unwrap() = true;
    *fake.switch_code.lock().unwrap() = Some("busy");
    app.press(&format!("text:{ONE}"))
        .expect("the account row opens");
    app.wait_for_within(&format!("text:{TWO}"), WAIT)
        .expect("the other account shows");
    app.press(&format!("text:{TWO}")).expect("it presses");
    app.wait_for_within("text:Buwiz Bot couldn't switch to that account.", WAIT)
        .expect("it says so");
    *fake.accounts.lock().unwrap() = vec![(ONE.to_owned(), true)];
    let by = std::time::Instant::now() + Duration::from_secs(20);
    while app.screen().matches("label:Switch account").unwrap() && std::time::Instant::now() < by {
        settle(&mut app);
    }
    assert!(!app.screen().matches("label:Switch account").unwrap());
    assert!(
        app.screen()
            .shows("Buwiz Bot couldn't switch to that account.")
    );
    assert!(
        !app.screen().shows("Sign in to it again"),
        "nothing says the sign-in went"
    );
}

#[test]
fn a_letter_names_its_account_and_a_switch_waits_for_it() {
    let (mut app, fake) = two_accounts();
    write_one(&mut app);
    assert!(
        !app.screen().matches("label:Switch account").unwrap(),
        "a letter stays on its account"
    );
    app.press("label:Keep draft").expect("it keeps");
    settle(&mut app);
    let saves = fake.called("gws_mail_draft_save");
    assert_eq!(saves.last().unwrap()["as_account"], ONE);
    assert!(app.screen().matches("label:Switch account").unwrap());
}

#[test]
fn a_switch_made_elsewhere_reaches_an_open_letter_as_nothing_but_its_account() {
    let (mut app, fake) = two_accounts();
    write_one(&mut app);
    fake.switch_to(TWO);
    let by = std::time::Instant::now() + Duration::from_secs(20);
    let lists = fake.called("gws_mail_list").len();
    while fake.called("gws_mail_list").len() == lists && std::time::Instant::now() < by {
        settle(&mut app);
    }
    assert!(
        fake.called("gws_mail_list").len() > lists,
        "the list moved on"
    );
    app.press("label:Keep draft").expect("it keeps");
    settle(&mut app);
    let saves = fake.called("gws_mail_draft_save");
    assert_eq!(
        saves.last().unwrap()["as_account"],
        ONE,
        "the plugin refuses this one, so it can't land on the other account"
    );
}

#[test]
fn a_letter_left_on_the_other_account_can_switch_back_and_stays_its_own() {
    let (mut app, fake) = two_accounts();
    write_one(&mut app);
    fake.switch_to(TWO);
    app.wait_for_within("label:Switch account", Duration::from_secs(20))
        .expect("the way back shows");
    app.press(&format!("text:{TWO}")).expect("the row opens");
    app.wait_for_within(&format!("text:{ONE}"), WAIT)
        .expect("the letter's account shows");
    app.press(&format!("text:{ONE}")).expect("it presses");
    app.wait_for_gone("label:Switch account", WAIT)
        .expect("back on its account, the chooser waits for the letter");
    assert_eq!(fake.called("gws_account_use"), vec![json!({"label": ONE})]);
    app.press("label:Keep draft").expect("it keeps");
    settle(&mut app);
    let saves = fake.called("gws_mail_draft_save");
    assert_eq!(saves.last().unwrap()["as_account"], ONE);
}
