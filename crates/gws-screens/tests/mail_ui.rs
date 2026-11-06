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
    json!({"id": format!("t{n}"), "sender": format!("Sender {n}"), "subject": format!("Subject {n}"), "snippet": "", "at": 0})
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
            "gws_mail_labels" => (String::new(), json!({"labels": []})),
            "gws_mail_changes" => (
                String::new(),
                json!({"status": "", "seq": 0, "changed": false}),
            ),
            "gws_mail_list" => (
                args["label"].as_str().unwrap_or_default().to_owned(),
                list(args),
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
