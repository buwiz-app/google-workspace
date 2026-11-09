//! The pictures bar over a mail, driven as a person drives it, on a headless
//! engine with a fake host: a stranger's web pictures wait for a press, and a
//! trusted sender's are asked for as the person opens the mail.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use buwiz_app::{Bridge, BuwizError, Done, Host, Listener};
use nsh_core::prelude::*;
use nsh_engine::EngineConfig;
use nsh_testing::Headless;
use serde_json::{Map, Value, json};

mod bot_filter;

use bot_filter::Filter;

const WAIT: Duration = Duration::from_secs(30);
const NEWS: &str = "news@digest.example";
const FRIEND: &str = "ana@example.com";

#[derive(Default)]
struct Fake {
    /// Senders whose pictures always show.
    trusted: Mutex<Vec<String>>,
    calls: Mutex<Vec<(String, Value)>>,
    /// A pictures answer waits here until released.
    hold_pictures: Mutex<bool>,
    held: Mutex<Vec<(Value, Done)>>,
}

impl Fake {
    fn called(&self, tool: &str) -> Vec<Value> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(t, _)| t == tool)
            .map(|(_, a)| a.clone())
            .collect()
    }

    fn from(id: &str) -> &'static str {
        if id == "t2" { FRIEND } else { NEWS }
    }

    fn view(&self, id: &str, shown: bool) -> Value {
        let from = Self::from(id);
        let trusted = self.trusted.lock().unwrap().iter().any(|t| t == from);
        let page = if shown {
            "<p>with pictures</p>"
        } else {
            "<p>without</p>"
        };
        json!({"id": id, "page": format!("{page}<p>{id}</p>"), "links": [], "images": if shown { 0 } else { 2 }, "attachments": [], "pictures": {"from": from, "trusted": trusted, "asked": shown}})
    }
}

impl Host for Fake {
    fn params(&self) -> Map<String, Value> {
        json!({
            "props": {}, "app": {"plugin": "app.buwiz.google-workspace", "app": "mail"},
            "granted": ["buwiz.call", "buwiz.open"],
            "viewport": {"width": 1000, "height": 600, "placement": "full"},
        })
        .as_object()
        .unwrap()
        .clone()
    }

    fn post(&self, _: &str, _: Value) {}

    fn call(&self, method: &str, data: Value, done: Done) {
        if method != "buwiz.call" {
            return done(Err(BuwizError::new("invalid", false, "not faked")));
        }
        let (tool, args) = (data["tool"].as_str().unwrap_or_default(), &data["args"]);
        self.calls
            .lock()
            .unwrap()
            .push((tool.to_owned(), args.clone()));
        let thread = args["thread"].as_str().unwrap_or_default();
        let answer = match tool {
            "gws_status" => json!({"signed_in": true, "subject": null}),
            "gws_accounts" => json!({"accounts": []}),
            "gws_mail_labels" => json!({"labels": []}),
            "gws_mail_changes" => json!({"status": "ok", "seq": 0, "changed": false}),
            "gws_mail_list" => json!({"threads": [
                {"id": "t1", "sender": "Weekly Digest", "subject": "Subject 1", "snippet": "", "at": 0},
                {"id": "t2", "sender": "Ana", "subject": "Subject 2", "snippet": "", "at": 0},
            ], "more": false, "saved": false}),
            "gws_mail_mark" => json!({"done": 1}),
            "gws_mail_thread" => self.view(thread, false),
            "gws_mail_pictures" => {
                let view = self.view(thread, true);
                if *self.hold_pictures.lock().unwrap() {
                    self.held.lock().unwrap().push((view, done));
                    return;
                }
                view
            }
            "gws_mail_picture_sender_add" => {
                let address = args["address"].as_str().unwrap().to_owned();
                self.trusted.lock().unwrap().push(address);
                json!({"trusted": true})
            }
            "gws_mail_picture_sender_remove" => {
                let address = args["address"].as_str().unwrap();
                self.trusted.lock().unwrap().retain(|t| t != address);
                json!({"trusted": false})
            }
            other => {
                return done(Err(BuwizError::new(
                    "invalid",
                    false,
                    format!("not faked: {other}"),
                )));
            }
        };
        done(Ok(answer));
    }

    fn listen(&self, _: Listener) {}

    fn fail(&self, message: &str) {
        panic!("the screen failed: {message}");
    }
}

fn start(fake: &Arc<Fake>) -> (Headless, Filter) {
    let def = gws_screens::screens()
        .iter()
        .find(|s| s.id == "gws.mail")
        .expect("Mail");
    let root = (def.build)(Bridge::new(fake.clone()));
    let mut app = Headless::start(EngineConfig::new(root)).expect("the engine starts");
    let filter = Filter::watch(&mut app);
    app.set_viewport(size(px(1000.0), px(600.0)))
        .expect("the viewport sets");
    app.wait_for_within("text:Subject 1", WAIT)
        .expect("the inbox shows");
    (app, filter)
}

fn settle(app: &mut Headless) {
    app.wait(Duration::from_millis(300)).expect("time passes");
}

fn open(app: &mut Headless, subject: &str) {
    app.press(&format!("text:{subject}"))
        .expect("the row presses");
    settle(app);
}

const PRESSES: [&str; 3] = [
    "Show the pictures in this mail",
    "Always show this sender's pictures",
    "Stop always showing this sender's pictures",
];

#[test]
fn a_strangers_pictures_wait_for_the_press() {
    let fake = Arc::new(Fake::default());
    let (mut app, _) = start(&fake);
    open(&mut app, "Subject 1");
    assert!(app.screen().shows("Pictures are hidden."));
    assert!(app.screen().shows(&format!("Always from {NEWS}")));
    assert!(!app.screen().shows("1 image blocked") && !app.screen().shows("2 images blocked"));
    assert!(
        fake.called("gws_mail_pictures").is_empty(),
        "nothing before the press"
    );
    app.press(&format!("label:{}", PRESSES[0]))
        .expect("Show pictures presses");
    settle(&mut app);
    assert_eq!(
        fake.called("gws_mail_pictures"),
        [json!({"thread": "t1", "trusted": false})]
    );
    assert!(!app.screen().shows("Pictures are hidden."), "the bar went");
    assert!(fake.called("gws_mail_picture_sender_add").is_empty());
}

#[test]
fn always_from_a_sender_trusts_them_shows_their_pictures_and_can_stop() {
    let fake = Arc::new(Fake::default());
    let (mut app, _) = start(&fake);
    open(&mut app, "Subject 1");
    app.press(&format!("label:{}", PRESSES[1]))
        .expect("Always presses");
    settle(&mut app);
    assert_eq!(
        fake.called("gws_mail_picture_sender_add"),
        [json!({"address": NEWS})]
    );
    assert_eq!(fake.called("gws_mail_pictures").len(), 1);
    assert!(
        app.screen()
            .shows(&format!("Pictures from {NEWS} always show."))
    );
    app.press(&format!("label:{}", PRESSES[2]))
        .expect("Stop presses");
    settle(&mut app);
    assert_eq!(
        fake.called("gws_mail_picture_sender_remove"),
        [json!({"address": NEWS})]
    );
    assert!(
        !app.screen()
            .shows(&format!("Pictures from {NEWS} always show."))
    );
}

#[test]
fn a_trusted_senders_mail_asks_for_its_pictures_as_it_opens() {
    let fake = Arc::new(Fake::default());
    fake.trusted.lock().unwrap().push(FRIEND.to_owned());
    let (mut app, _) = start(&fake);
    open(&mut app, "Subject 1");
    assert!(
        fake.called("gws_mail_pictures").is_empty(),
        "a stranger waits"
    );
    open(&mut app, "Subject 2");
    assert_eq!(
        fake.called("gws_mail_pictures"),
        [json!({"thread": "t2", "trusted": true})]
    );
    assert!(
        app.screen()
            .shows(&format!("Pictures from {FRIEND} always show."))
    );
}

#[test]
fn pictures_for_a_mail_left_behind_dont_land_on_the_next() {
    let fake = Arc::new(Fake::default());
    *fake.hold_pictures.lock().unwrap() = true;
    let (mut app, _) = start(&fake);
    open(&mut app, "Subject 1");
    app.press(&format!("label:{}", PRESSES[0]))
        .expect("Show pictures presses");
    settle(&mut app);
    assert!(app.screen().shows("Getting pictures…"));
    open(&mut app, "Subject 2");
    let (view, done) = fake.held.lock().unwrap().pop().expect("held");
    done(Ok(view));
    settle(&mut app);
    assert!(
        app.screen().shows("Pictures are hidden."),
        "t2's bar is still its own"
    );
    assert!(app.screen().shows(&format!("Always from {FRIEND}")));
}

/// The AI can't press any of it, and no label holds the sender.
#[test]
fn the_ai_reaches_no_press_of_the_bar_and_no_label_holds_the_sender() {
    let fake = Arc::new(Fake::default());
    let (mut app, filter) = start(&fake);
    bot_filter::assert_the_copy_works();
    open(&mut app, "Subject 1");
    assert!(
        app.screen().shows("Pictures are hidden."),
        "the audit sees the bar"
    );
    assert!(
        filter.unnamed_fields() >= 1,
        "the audit sees the search field"
    );
    let audit = |at: &str| {
        filter.assert_stages_nothing(&[], at);
        filter.assert_no_data_in_labels(&[NEWS, "digest"], at);
    };
    audit("pictures hidden");
    assert!(filter.labels().iter().any(|l| l == PRESSES[0]));
    assert!(filter.labels().iter().any(|l| l == PRESSES[1]));
    app.press(&format!("label:{}", PRESSES[1]))
        .expect("Always presses");
    settle(&mut app);
    audit("a trusted sender");
    assert!(
        filter.labels().iter().any(|l| l == PRESSES[2]),
        "the audit sees Stop"
    );
}
