//! The invite card on the Mail screen, driven as a person drives it on a
//! headless engine. A fake host answers the screen's calls with an invite in
//! each state the plugin can give.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use buwiz_app::{Bridge, BuwizError, Done, Host, Listener};
use chrono::Local;
use nsh_core::prelude::*;
use nsh_engine::EngineConfig;
use nsh_testing::Headless;
use serde_json::{Map, Value, json};

#[allow(dead_code)]
mod bot_filter;

use bot_filter::Filter;

const WAIT: Duration = Duration::from_secs(30);

fn invite(method: &str) -> Value {
    json!({"message": "m20", "uid": "fake-roof-inspection@google.com", "method": method, "title": "Roof inspection", "start": "2026-11-10T02:00:00Z", "end": "2026-11-10T03:00:00Z", "all_day": false, "place": "12 Mabini St", "organizer": "Ana Reyes"})
}

#[derive(Default)]
struct Fake {
    /// What `gws_mail_invite` answers.
    invite: Mutex<Value>,
    /// What a reply answers, in place of its event.
    reply: Mutex<Option<Value>>,
    /// The next looks fail with this.
    look_fails: Mutex<Option<String>>,
    /// Replies wait for [`Fake::release`].
    hold_replies: Mutex<bool>,
    held: Mutex<Vec<(Value, Done)>>,
    calls: Mutex<Vec<(String, Value)>>,
}

impl Fake {
    fn release(&self) {
        let by = std::time::Instant::now() + WAIT;
        loop {
            if let Some((answer, done)) = self.held.lock().unwrap().pop() {
                return done(Ok(answer));
            }
            assert!(std::time::Instant::now() < by, "no reply was held");
            std::thread::yield_now();
        }
    }

    fn called(&self, tool: &str) -> Vec<Value> {
        let calls = self.calls.lock().unwrap();
        calls
            .iter()
            .filter(|(t, _)| t == tool)
            .map(|(_, a)| a.clone())
            .collect()
    }
}

impl Host for Fake {
    fn params(&self) -> Map<String, Value> {
        let p = json!({
            "props": {}, "app": {"plugin": "app.buwiz.google-workspace", "app": "mail"},
            "granted": ["buwiz.call", "buwiz.open"],
            "viewport": {"width": 1000, "height": 600, "placement": "full"},
        });
        p.as_object().unwrap().clone()
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
        let answer = match tool {
            "gws_status" => json!({"signed_in": true, "subject": "owner@example.com"}),
            "gws_accounts" => json!({"accounts": []}),
            "gws_mail_labels" => json!({"labels": []}),
            "gws_mail_changes" => json!({"status": "ok", "seq": 0, "changed": false}),
            "gws_mail_list" => {
                json!({"threads": [{"id": "t20", "sender": "Ana Reyes", "subject": "Invitation: Roof inspection", "snippet": "", "at": 0, "unread": false}, {"id": "t30", "sender": "Cara Lim", "subject": "Photos", "snippet": "", "at": 0, "unread": false}], "more": false, "saved": false})
            }
            "gws_mail_thread" => {
                json!({"page": "<p>Ana Reyes has invited you</p>", "links": [], "images": 0, "attachments": []})
            }
            "gws_mail_invite" if self.look_fails.lock().unwrap().is_some() => {
                let why = self.look_fails.lock().unwrap().clone().unwrap();
                return done(Err(BuwizError::new("busy", false, why)));
            }
            "gws_mail_invite" if args["thread"] == "t20" => self.invite.lock().unwrap().clone(),
            "gws_mail_invite" => json!({"invite": null}),
            "gws_mail_invite_reply" => {
                self.reply.lock().unwrap().clone().unwrap_or_else(
                    || json!({"sent": true, "event": {"response": args["response"]}}),
                )
            }
            other => {
                return done(Err(BuwizError::new(
                    "invalid",
                    false,
                    format!("not faked: {other}"),
                )));
            }
        };
        if tool == "gws_mail_invite_reply" && *self.hold_replies.lock().unwrap() {
            return self.held.lock().unwrap().push((answer, done));
        }
        done(Ok(answer));
    }

    fn listen(&self, _: Listener) {}

    fn fail(&self, message: &str) {
        panic!("the screen failed: {message}");
    }
}

/// The Mail screen with the invite open, its card answered with `answer`.
fn opened(answer: Value) -> (Headless, Filter, Arc<Fake>) {
    let fake = Arc::new(Fake::default());
    *fake.invite.lock().unwrap() = answer;
    let def = gws_screens::screens()
        .iter()
        .find(|s| s.id == "gws.mail")
        .expect("Mail");
    let root = (def.build)(Bridge::new(fake.clone()));
    let mut app = Headless::start(EngineConfig::new(root)).expect("the engine starts");
    let filter = Filter::watch(&mut app);
    app.set_viewport(size(px(1000.0), px(600.0)))
        .expect("the viewport sets");
    app.wait_for_within("text:Invitation: Roof inspection", WAIT)
        .expect("the inbox shows");
    app.press("text:Invitation: Roof inspection")
        .expect("the row presses");
    settle(&mut app);
    (app, filter, fake)
}

fn settle(app: &mut Headless) {
    app.wait(Duration::from_millis(300)).expect("time passes");
}

fn in_calendar(response: &str) -> Value {
    json!({"invite": invite("request"), "calendar": {"found": true, "matches": true, "guest": true, "response": response}})
}

#[test]
fn an_invitation_shows_its_event_and_who_sent_it() {
    let (app, _, _) = opened(in_calendar("needsAction"));
    let screen = app.screen();
    assert!(screen.shows("Roof inspection"));
    assert!(screen.shows("12 Mabini St"));
    assert!(screen.shows("Ana Reyes invited you"));
    // the time as Calendar's screen shows it, on this Mac's clock
    let when = gws_screens::calendar::time::When::of(
        "2026-11-10T02:00:00Z",
        "2026-11-10T03:00:00Z",
        false,
    )
    .unwrap();
    assert!(screen.shows(&gws_screens::calendar::time::when_line(&when, &Local)));
    for label in ["Answer yes", "Answer no", "Answer maybe"] {
        assert!(
            screen.enabled(&format!("label:{label}")).unwrap(),
            "{label}"
        );
        assert!(
            !screen.selected(&format!("label:{label}")).unwrap(),
            "{label}"
        );
    }
    assert!(!screen.shows("Not in your Calendar yet."));
}

#[test]
fn the_persons_answer_so_far_is_marked() {
    let (app, _, _) = opened(in_calendar("tentative"));
    assert!(app.screen().selected("label:Answer maybe").unwrap());
    assert!(!app.screen().selected("label:Answer yes").unwrap());
}

#[test]
fn yes_answers_through_calendar_once_and_marks_it() {
    let (mut app, _, fake) = opened(in_calendar("needsAction"));
    assert!(fake.called("gws_mail_invite_reply").is_empty());
    app.press("label:Answer yes").expect("Yes presses");
    app.wait_for_within("text:Your reply was sent.", WAIT)
        .expect("it says so");
    assert_eq!(
        fake.called("gws_mail_invite_reply"),
        [json!({"message": "m20", "response": "accepted"})]
    );
    assert!(app.screen().selected("label:Answer yes").unwrap());
}

#[test]
fn a_refused_answer_says_why_and_keeps_what_was_answered() {
    let (mut app, _, fake) = opened(in_calendar("declined"));
    *fake.reply.lock().unwrap() =
        Some(json!({"refused": "changed", "message": "Someone else changed this event."}));
    app.press("label:Answer yes").expect("Yes presses");
    app.wait_for_within("text:Someone else changed this event.", WAIT)
        .expect("it says why");
    assert!(app.screen().selected("label:Answer no").unwrap());
}

#[test]
fn a_failed_answer_is_never_said_to_be_sent() {
    let (mut app, _, fake) = opened(in_calendar("needsAction"));
    *fake.reply.lock().unwrap() = Some(
        json!({"isError": true, "content": [{"type": "text", "text": "Calendar had a problem just now. Try again."}]}),
    );
    app.press("label:Answer yes").expect("Yes presses");
    app.wait_for_within("text:Calendar had a problem just now. Try again.", WAIT)
        .expect("it says why");
    assert!(!app.screen().shows("Your reply was sent."));
    assert!(!app.screen().selected("label:Answer yes").unwrap());
    assert!(
        app.screen().enabled("label:Answer yes").unwrap(),
        "it can be pressed again"
    );
}

#[test]
fn an_invite_not_in_calendar_shows_with_its_buttons_off() {
    let (mut app, _, fake) = opened(
        json!({"invite": invite("request"), "calendar": {"found": false, "guest": false, "response": null}}),
    );
    assert!(app.screen().shows("Roof inspection"));
    assert!(app.screen().shows("Not in your Calendar yet."));
    for label in ["Answer yes", "Answer no", "Answer maybe"] {
        assert!(
            !app.screen().enabled(&format!("label:{label}")).unwrap(),
            "{label}"
        );
    }
    let _ = app.press("label:Answer yes");
    settle(&mut app);
    assert!(fake.called("gws_mail_invite_reply").is_empty());
}

#[test]
fn an_invite_unlike_its_calendar_event_shows_with_its_buttons_off() {
    let (mut app, _, fake) = opened(
        json!({"invite": invite("request"), "calendar": {"found": true, "matches": false, "guest": true, "response": "needsAction"}}),
    );
    assert!(app.screen().shows("Roof inspection"));
    assert!(
        app.screen()
            .shows("This invite doesn't match your Calendar.")
    );
    for label in ["Answer yes", "Answer no", "Answer maybe"] {
        assert!(
            !app.screen().enabled(&format!("label:{label}")).unwrap(),
            "{label}"
        );
    }
    let _ = app.press("label:Answer yes");
    settle(&mut app);
    assert!(fake.called("gws_mail_invite_reply").is_empty());
}

#[test]
fn a_cancelled_event_says_so_with_no_buttons() {
    let (app, _, _) = opened(json!({"invite": invite("cancel"), "calendar": null}));
    assert!(app.screen().shows("This event was cancelled."));
    assert_eq!(app.screen().count("label:Answer yes").unwrap(), 0);
}

#[test]
fn a_reply_says_who_answered_what_with_no_buttons() {
    let mut reply = invite("reply");
    reply["reply"] = json!({"who": "Ben Cruz", "response": "accepted"});
    let (app, _, _) = opened(json!({"invite": reply, "calendar": null}));
    assert!(app.screen().shows("Ben Cruz accepted."));
    assert_eq!(app.screen().count("label:Answer yes").unwrap(), 0);
}

#[test]
fn a_mail_with_no_invite_has_no_card() {
    let (mut app, _, fake) = opened(in_calendar("needsAction"));
    assert!(app.screen().shows("Ana Reyes invited you"));
    app.press("text:Photos").expect("the row presses");
    settle(&mut app);
    assert!(!app.screen().shows("Ana Reyes invited you"));
    assert_eq!(app.screen().count("label:Answer yes").unwrap(), 0);
    assert_eq!(fake.called("gws_mail_invite").len(), 2);
}

/// The AI reaches none of it: no label holds the invite's words, and the
/// filter names no answer, so its press can't stage one.
#[test]
fn the_ai_can_neither_read_a_label_nor_press_an_answer() {
    bot_filter::assert_the_copy_works();
    for answer in [
        in_calendar("needsAction"),
        json!({"invite": invite("request"), "calendar": {"found": false, "guest": false, "response": null}}),
        json!({"invite": invite("cancel"), "calendar": null}),
    ] {
        let (app, filter, _) = opened(answer);
        assert!(app.screen().shows("Roof inspection"));
        filter.assert_no_data_in_labels(
            &["Roof", "inspection", "Mabini", "Ana Reyes", "invited"],
            "the invite card",
        );
        for (_, name) in filter.reachable() {
            assert!(
                ![
                    "Yes",
                    "No",
                    "Maybe",
                    "Answer yes",
                    "Answer no",
                    "Answer maybe"
                ]
                .contains(&name.as_str()),
                "the AI can reach {name:?}"
            );
        }
    }
}

#[test]
fn once_calendar_has_the_event_the_card_shows_calendars_words() {
    let mut answer = in_calendar("needsAction");
    answer["calendar"]["event"] = json!({"title": "Roof check", "start": "2026-11-10T02:00:00Z", "end": "2026-11-10T03:00:00Z", "all_day": false, "place": "Site office", "organizer": "Ana R."});
    let (app, _, _) = opened(answer);
    assert!(app.screen().shows("Roof check"));
    assert!(app.screen().shows("Site office"));
    assert!(app.screen().shows("Ana R. invited you"));
    assert!(!app.screen().shows("Roof inspection"), "the letter's words");
    assert!(!app.screen().shows("12 Mabini St"));
}

#[test]
fn an_event_cancelled_in_calendar_says_so_with_no_buttons() {
    let (app, _, _) = opened(
        json!({"invite": invite("request"), "calendar": {"found": true, "cancelled": true}}),
    );
    assert!(app.screen().shows("This event was cancelled."));
    assert!(!app.screen().shows("Not in your Calendar yet."));
    assert_eq!(app.screen().count("label:Answer yes").unwrap(), 0);
}

#[test]
fn a_look_that_failed_says_why_and_can_be_asked_again() {
    let fake = Arc::new(Fake::default());
    *fake.invite.lock().unwrap() = in_calendar("needsAction");
    *fake.look_fails.lock().unwrap() =
        Some("Gmail didn't send this invite just now. Try again in a moment.".to_owned());
    let def = gws_screens::screens()
        .iter()
        .find(|s| s.id == "gws.mail")
        .expect("Mail");
    let root = (def.build)(Bridge::new(fake.clone()));
    let mut app = Headless::start(EngineConfig::new(root)).expect("the engine starts");
    app.set_viewport(size(px(1000.0), px(600.0)))
        .expect("the viewport sets");
    app.wait_for_within("text:Invitation: Roof inspection", WAIT)
        .expect("the inbox shows");
    app.press("text:Invitation: Roof inspection")
        .expect("the row presses");
    app.wait_for_within(
        "text:Gmail didn't send this invite just now. Try again in a moment.",
        WAIT,
    )
    .expect("it says why");
    *fake.look_fails.lock().unwrap() = None;
    app.press("label:Read this invite again")
        .expect("it presses");
    app.wait_for_within("text:Ana Reyes invited you", WAIT)
        .expect("the card comes");
}

#[test]
fn a_page_shown_again_mid_reply_keeps_the_reply_and_sends_it_once() {
    let (mut app, _, fake) = opened(in_calendar("needsAction"));
    *fake.hold_replies.lock().unwrap() = true;
    app.press("label:Answer yes").expect("Yes presses");
    settle(&mut app);
    // the page goes and comes back, as letting pictures in does
    app.press("text:Invitation: Roof inspection")
        .expect("the row presses");
    settle(&mut app);
    assert!(app.screen().shows("Roof inspection"));
    assert!(
        !app.screen().enabled("label:Answer yes").unwrap(),
        "still on its way"
    );
    let _ = app.press("label:Answer yes");
    settle(&mut app);
    fake.release();
    app.wait_for_within("text:Your reply was sent.", WAIT)
        .expect("it says so");
    assert_eq!(fake.called("gws_mail_invite_reply").len(), 1);
    assert!(app.screen().selected("label:Answer yes").unwrap());
}
