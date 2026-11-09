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
    /// Looks wait for [`Fake::release_looks`].
    hold_looks: Mutex<bool>,
    held_looks: Mutex<Vec<Done>>,
    /// A reply whose answer says it failed still reaches Calendar.
    went_anyway: Mutex<bool>,
    /// Calendar lags: a reply doesn't show there until [`Fake::answered`].
    lags: Mutex<bool>,
    /// A reply's answer is lost on the bridge.
    reply_lost: Mutex<bool>,
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

    /// Answers every held look with what `gws_mail_invite` answers now.
    fn release_looks(&self) {
        *self.hold_looks.lock().unwrap() = false;
        let by = std::time::Instant::now() + WAIT;
        while self.held_looks.lock().unwrap().is_empty() {
            assert!(std::time::Instant::now() < by, "no look was held");
            std::thread::yield_now();
        }
        let held: Vec<Done> = self.held_looks.lock().unwrap().drain(..).collect();
        let answer = self.invite.lock().unwrap().clone();
        for done in held {
            done(Ok(answer.clone()));
        }
    }

    /// Calendar now holds `response` as the person's answer.
    fn answered(&self, response: &Value) {
        let mut invite = self.invite.lock().unwrap();
        if invite["calendar"]["guest"] == true {
            invite["calendar"]["response"] = response.clone();
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
            "gws_mail_invite" if args["thread"] == "t20" && *self.hold_looks.lock().unwrap() => {
                return self.held_looks.lock().unwrap().push(done);
            }
            "gws_mail_invite" if args["thread"] == "t20" => self.invite.lock().unwrap().clone(),
            "gws_mail_invite" => json!({"invite": null}),
            "gws_mail_invite_reply" if *self.reply_lost.lock().unwrap() => {
                return done(Err(BuwizError::new(
                    "timeout",
                    false,
                    "The plugin took too long.",
                )));
            }
            "gws_mail_invite_reply" if *self.lags.lock().unwrap() => {
                json!({"sent": true, "event": null})
            }
            "gws_mail_invite_reply" => {
                let said = self.reply.lock().unwrap().clone();
                if said.is_none() || *self.went_anyway.lock().unwrap() {
                    self.answered(&args["response"]);
                }
                said.unwrap_or_else(
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
fn an_invite_unlike_its_calendar_event_shows_only_that_and_no_buttons() {
    let (mut app, _, fake) = opened(
        json!({"invite": invite("request"), "calendar": {"found": true, "cancelled": false, "matches": false}}),
    );
    assert!(!app.screen().shows("Roof inspection"), "none of its words");
    assert!(!app.screen().shows("Ana Reyes invited you"));
    assert!(
        app.screen()
            .shows("This invite doesn't match your Calendar.")
    );
    for label in ["Answer yes", "Answer no", "Answer maybe"] {
        assert_eq!(
            app.screen().count(&format!("label:{label}")).unwrap(),
            0,
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

fn not_yet() -> Value {
    json!({"invite": invite("request"), "calendar": {"found": false}})
}

fn lit(app: &Headless) -> bool {
    app.screen().enabled("label:Answer yes").unwrap()
}

#[test]
fn not_in_calendar_yet_is_the_mails_words_and_can_check_again() {
    let (mut app, _, fake) = opened(not_yet());
    assert!(
        app.screen()
            .shows("From the mail, not checked with Calendar.")
    );
    assert!(!lit(&app));
    *fake.invite.lock().unwrap() = in_calendar("needsAction");
    app.press("label:Look in Calendar again")
        .expect("it presses");
    settle(&mut app);
    assert!(lit(&app), "Calendar has it now");
    assert!(!app.screen().shows("Not in your Calendar yet."));
}

#[test]
fn a_conversation_opened_again_looks_again() {
    let (mut app, _, fake) = opened(not_yet());
    assert!(!lit(&app));
    *fake.invite.lock().unwrap() = in_calendar("needsAction");
    app.press("text:Invitation: Roof inspection")
        .expect("the row presses");
    settle(&mut app);
    assert!(lit(&app), "Calendar has it now");
}

#[test]
fn a_try_again_pressed_twice_while_it_looks_asks_once() {
    let fake = Arc::new(Fake::default());
    *fake.invite.lock().unwrap() = in_calendar("needsAction");
    *fake.look_fails.lock().unwrap() = Some("Gmail didn't send this invite just now.".to_owned());
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
    app.wait_for_within("label:Read this invite again", WAIT)
        .expect("it says why");
    *fake.look_fails.lock().unwrap() = None;
    *fake.hold_looks.lock().unwrap() = true;
    let before = fake.called("gws_mail_invite").len();
    app.press("label:Read this invite again")
        .expect("it presses");
    settle(&mut app);
    let _ = app.press("label:Read this invite again");
    settle(&mut app);
    assert_eq!(fake.called("gws_mail_invite").len(), before + 1);
    fake.release_looks();
    app.wait_for_within("text:Ana Reyes invited you", WAIT)
        .expect("the card comes");
}

#[test]
fn the_buttons_stay_busy_until_a_fresh_look_shows_the_answer() {
    let (mut app, _, fake) = opened(in_calendar("needsAction"));
    *fake.hold_looks.lock().unwrap() = true;
    app.press("label:Answer yes").expect("Yes presses");
    settle(&mut app);
    assert_eq!(fake.called("gws_mail_invite_reply").len(), 1);
    assert!(!lit(&app), "busy while Calendar is asked");
    let _ = app.press("label:Answer yes");
    settle(&mut app);
    assert_eq!(fake.called("gws_mail_invite_reply").len(), 1, "not twice");
    fake.release_looks();
    app.wait_for_within("text:Your reply was sent.", WAIT)
        .expect("it says so");
    assert!(lit(&app));
    assert!(app.screen().selected("label:Answer yes").unwrap());
}

#[test]
fn a_reply_whose_answer_was_lost_but_went_is_said_to_be_sent() {
    let (mut app, _, fake) = opened(in_calendar("needsAction"));
    *fake.reply.lock().unwrap() = Some(
        json!({"isError": true, "content": [{"type": "text", "text": "The plugin took too long to answer."}]}),
    );
    *fake.went_anyway.lock().unwrap() = true;
    app.press("label:Answer yes").expect("Yes presses");
    app.wait_for_within("text:Your reply was sent.", WAIT)
        .expect("Calendar shows it went");
    assert!(app.screen().selected("label:Answer yes").unwrap());
    assert!(!app.screen().shows("The plugin took too long to answer."));
}

#[test]
fn another_conversations_look_never_lands_on_an_answer_on_its_way() {
    let (mut app, _, fake) = opened(in_calendar("needsAction"));
    *fake.hold_replies.lock().unwrap() = true;
    app.press("label:Answer yes").expect("Yes presses");
    settle(&mut app);
    app.press("text:Photos").expect("the row presses");
    settle(&mut app);
    app.press("text:Invitation: Roof inspection")
        .expect("the row presses");
    settle(&mut app);
    assert!(!lit(&app), "still on its way");
    let _ = app.press("label:Answer yes");
    settle(&mut app);
    fake.release();
    app.wait_for_within("text:Your reply was sent.", WAIT)
        .expect("it says so");
    assert_eq!(fake.called("gws_mail_invite_reply").len(), 1);
}

#[test]
fn calendar_not_answering_keeps_the_invite_with_the_reason_and_no_answer() {
    let (app, _, _) = opened(
        json!({"invite": invite("request"), "calendar": {"error": "Calendar is busy right now. Try again in a moment."}}),
    );
    assert!(app.screen().shows("Roof inspection"));
    assert!(
        app.screen()
            .shows("Calendar is busy right now. Try again in a moment.")
    );
    assert!(!lit(&app));
}

#[test]
fn an_invite_gmail_couldnt_vouch_for_answers_nothing() {
    let (app, _, _) =
        opened(json!({"invite": invite("request"), "calendar": {"unverified": true}}));
    assert!(
        app.screen()
            .shows("Gmail couldn't confirm this invite came from its organiser.")
    );
    assert!(!lit(&app));
}

#[test]
fn a_damaged_invite_says_so_for_good() {
    let (app, _, _) = opened(json!({"invite": {"damaged": true}, "calendar": null}));
    assert!(app.screen().shows("Can't read this invite."));
    assert_eq!(
        app.screen().count("label:Read this invite again").unwrap(),
        0
    );
}

#[test]
fn a_lapsed_sign_in_shows_the_sign_in_steps_not_a_blank() {
    let fake = Arc::new(Fake::default());
    *fake.invite.lock().unwrap() = in_calendar("needsAction");
    *fake.look_fails.lock().unwrap() = Some("Sign in to Google to use this.".to_owned());
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
    app.wait_for_within("text:Make a Google Cloud project", WAIT)
        .expect("the sign-in steps show");
}

/// Past the screen's own retries, which end in under eight seconds.
const PAST_RETRIES: Duration = Duration::from_secs(20);

#[test]
fn a_reply_calendar_lags_on_never_lights_again_until_it_shows() {
    let (mut app, _, fake) = opened(in_calendar("needsAction"));
    *fake.lags.lock().unwrap() = true;
    app.press("label:Answer yes").expect("Yes presses");
    app.wait_for_within("text:Sent. Calendar hasn't shown it yet.", PAST_RETRIES)
        .expect("it says so");
    app.wait(Duration::from_secs(2)).expect("time passes");
    assert!(!lit(&app), "no timer lights it");
    let _ = app.press("label:Answer yes");
    settle(&mut app);
    assert_eq!(fake.called("gws_mail_invite_reply").len(), 1, "not twice");
    // a look that still finds nothing keeps it off
    app.press("label:Look in Calendar again")
        .expect("it presses");
    settle(&mut app);
    assert!(!lit(&app));
    fake.answered(&json!("accepted"));
    app.press("label:Look in Calendar again")
        .expect("it presses");
    app.wait_for_within("text:Your reply was sent.", WAIT)
        .expect("Calendar shows it");
    assert!(lit(&app));
    assert!(app.screen().selected("label:Answer yes").unwrap());
}

#[test]
fn a_reply_lost_on_its_way_never_lights_the_buttons_at_once() {
    let (mut app, _, fake) = opened(in_calendar("needsAction"));
    *fake.reply_lost.lock().unwrap() = true;
    app.press("label:Answer yes").expect("Yes presses");
    settle(&mut app);
    assert!(!lit(&app), "it may have gone");
    app.wait_for_within("text:Sent. Calendar hasn't shown it yet.", PAST_RETRIES)
        .expect("it says so");
    assert!(!lit(&app));
    let _ = app.press("label:Answer yes");
    settle(&mut app);
    assert_eq!(fake.called("gws_mail_invite_reply").len(), 1, "not twice");
}

#[test]
fn a_cancel_gmail_cant_vouch_for_is_labelled_the_mails() {
    let (app, _, _) = opened(json!({"invite": invite("cancel"), "calendar": {"unverified": true}}));
    assert!(
        app.screen()
            .shows("From the mail, not checked with Calendar.")
    );
    assert!(
        app.screen()
            .shows("Gmail couldn't confirm this invite came from its organiser.")
    );
    assert_eq!(app.screen().count("label:Answer yes").unwrap(), 0);
}
