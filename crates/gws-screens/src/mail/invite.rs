//! The invite card, above the letter: the event a Calendar invite carries, read
//! by the plugin from its `.ics`, with Yes, No and Maybe answered through
//! Calendar so the organiser hears.
//!
//! Once Calendar has the event, the card shows Calendar's own title, time,
//! place and organiser, never the `.ics`'s: what a press answers is what the
//! person sees. The card's state lives in [`Mail`], by conversation, so a
//! page shown again (pictures let in) keeps an answer on its way.
//!
//! The label rule holds: the card's words are texts, read as values, and the
//! answers carry fixed labels with other words than they show, so the Bot's
//! filter names none of them and the AI can't press one. A press is the
//! person's, called at the press, as who pressed is judged by that moment.

use buwiz_app::Bridge;
use chrono::{Local, NaiveDate};
use nsh_core::AccessibilityState;
use nsh_core::prelude::*;
use serde_json::{Value, json};

use super::Mail;
use crate::calendar::time::{When, when_line};
use crate::shell::{CHOSEN, DIM, INK, LINE, MUTE, read};

/// The plugin's `invite`, as the card shows it.
#[derive(Clone, PartialEq, Debug)]
struct Invite {
    message: String,
    method: String,
    title: String,
    when: Option<When>,
    place: String,
    organizer: String,
    /// For a reply: who answered, and what.
    reply: Option<(String, String)>,
}

fn text_in(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn invite_of(v: &Value) -> Option<Invite> {
    let method = text_in(v, "method");
    if !["request", "cancel", "reply"].contains(&method.as_str()) {
        return None;
    }
    let reply = match v.get("reply") {
        Some(r) if method == "reply" => Some((text_in(r, "who"), text_in(r, "response"))),
        _ if method == "reply" => return None,
        _ => None,
    };
    Some(Invite {
        message: text_in(v, "message"),
        when: When::of(
            &text_in(v, "start"),
            &text_in(v, "end"),
            v.get("all_day") == Some(&Value::Bool(true)),
        ),
        title: text_in(v, "title"),
        place: text_in(v, "place"),
        organizer: text_in(v, "organizer"),
        reply,
        method,
    })
}

/// The invite with Calendar's own words for its event in place of the
/// `.ics`'s, when Calendar sent them.
fn as_calendar_has_it(mut invite: Invite, event: Option<&Value>) -> Invite {
    if let Some(e) = event.filter(|e| e.is_object()) {
        invite.title = text_in(e, "title");
        invite.when = When::of(
            &text_in(e, "start"),
            &text_in(e, "end"),
            e.get("all_day") == Some(&Value::Bool(true)),
        );
        invite.place = text_in(e, "place");
        invite.organizer = text_in(e, "organizer");
    }
    invite
}

/// What Calendar says of an invitation's event.
#[derive(Clone, PartialEq, Debug)]
enum Calendar {
    /// Not there yet: the buttons wait for Calendar's sync.
    Missing,
    /// There, and cancelled.
    Cancelled,
    /// There, but the person isn't on its guest list.
    NotAGuest,
    /// An event with the invite's UID that doesn't read as the invite does:
    /// a forged or stale `.ics`, which can't answer it.
    Differs,
    /// There, with the person's answer so far.
    Guest(String),
}

fn calendar_of(v: &Value) -> Calendar {
    let yes = |key: &str| v.get(key) == Some(&Value::Bool(true));
    if yes("found") && yes("cancelled") {
        return Calendar::Cancelled;
    }
    match (yes("found"), yes("matches"), yes("guest")) {
        (false, ..) => Calendar::Missing,
        (true, false, _) => Calendar::Differs,
        (true, true, true) => Calendar::Guest(text_in(v, "response")),
        (true, true, false) => Calendar::NotAGuest,
    }
}

/// What the card shows for one conversation, kept in [`Mail`].
#[derive(Clone, PartialEq, Debug)]
pub(super) struct Held {
    thread: String,
    shows: Shows,
}

#[derive(Clone, PartialEq, Debug)]
enum Shows {
    Nothing,
    /// Why there's no card yet, as the plugin said it.
    Line(String),
    Card(Box<Card>),
}

#[derive(Clone, PartialEq, Debug)]
struct Card {
    invite: Invite,
    calendar: Calendar,
    /// An answer is on its way.
    busy: bool,
    said: Option<String>,
}

/// The day a date tile shows, on this Mac's clock as Calendar's screen has it.
fn day_of(when: &When) -> NaiveDate {
    match when {
        When::AllDay { first, .. } => *first,
        When::Timed { start, .. } => start.with_timezone(&Local).date_naive(),
    }
}

/// The line under the title that says what this is.
fn about(invite: &Invite, calendar: &Calendar) -> String {
    if *calendar == Calendar::Cancelled {
        return "This event was cancelled.".to_owned();
    }
    match (invite.method.as_str(), &invite.reply) {
        ("cancel", _) => "This event was cancelled.".to_owned(),
        ("reply", Some((who, response))) => match response.as_str() {
            "accepted" => format!("{who} accepted."),
            "declined" => format!("{who} declined."),
            _ => format!("{who} said maybe."),
        },
        _ if invite.organizer.is_empty() => "You're invited".to_owned(),
        _ => format!("{} invited you", invite.organizer),
    }
}

fn line(words: String, class: &str) -> Text {
    text(words).app_class(Some(format!("text-[12px] {class}").into()))
}

/// Whether a reply's answer says it went: a tool's failure (`isError`, as
/// compose reads one) or a refusal is why it didn't, and only an answer
/// that says it was sent is a reply sent. The event it wrote may not be
/// there: the answer is written all the same.
fn sent(v: &Value) -> Result<(), String> {
    let why = |v: &Value| {
        let said = v
            .get("message")
            .or_else(|| v.pointer("/structuredContent/message"))
            .or_else(|| v.pointer("/content/0/text"));
        said.and_then(Value::as_str)
            .unwrap_or("That didn't work.")
            .to_owned()
    };
    if v.get("isError") == Some(&Value::Bool(true)) || v.get("refused").is_some() {
        return Err(why(v));
    }
    let said = v
        .get("sent")
        .or_else(|| v.pointer("/structuredContent/sent"));
    match said {
        Some(Value::Bool(true)) => Ok(()),
        _ => Err(why(v)),
    }
}

/// The card for `thread`, changed by `f`, if it's still the one held.
fn change(m: Mail, thread: &str, f: impl FnOnce(&mut Card)) {
    m.invite.update(|held| {
        if let Some(Held {
            thread: at,
            shows: Shows::Card(card),
        }) = held
            && at == thread
        {
            f(card);
        }
    });
}

/// The person's answer, called in this press.
fn answer_now(b: &Bridge, m: Mail, response: &'static str) {
    let held = m.invite.get_untracked();
    let Some(Held {
        thread,
        shows: Shows::Card(card),
    }) = held
    else {
        return;
    };
    if card.busy {
        return;
    }
    let call = b.call(
        "gws_mail_invite_reply",
        json!({"message": card.invite.message, "response": response}),
    );
    change(m, &thread, |card| {
        card.busy = true;
        card.said = None;
    });
    // nothing else changes the card meanwhile: a press is refused while it's
    // busy, however often the page is shown again. It runs beside Mail's
    // jobs, as an attachment's open does
    let job = Box::pin(async move {
        let answer = call.await.map(|v| sent(&v));
        change(m, &thread, |card| {
            card.busy = false;
            card.said = Some(match answer {
                Ok(Ok(())) => {
                    card.calendar = Calendar::Guest(response.to_owned());
                    "Your reply was sent.".to_owned()
                }
                Ok(Err(why)) => why,
                Err(e) => e.message,
            });
        });
    });
    m.beside.with_untracked(|beside| drop(beside.send(job)));
}

fn answers(b: &Bridge, m: Mail, card: &Card) -> View {
    let (on, mine) = match &card.calendar {
        Calendar::Guest(said) => (!card.busy, said.as_str()),
        _ => (false, ""),
    };
    let press = |label: &'static str, name: &'static str, response: &'static str| {
        let b = b.clone();
        let chosen = mine == response;
        view()
            .app_class(Some(
                format!(
                    "px-3 py-1 rounded-md border {LINE} {}",
                    if chosen { CHOSEN } else { "" }
                )
                .into(),
            ))
            .label(label)
            .accessibility_state(AccessibilityState {
                selected: chosen,
                ..AccessibilityState::default()
            })
            .disabled(!on)
            .on_press(move || answer_now(&b, m, response))
            .child(text(name).app_class(Some(format!("text-[12px] {INK}").into())))
    };
    view()
        .app_class(Some(
            "flex-row flex-wrap items-center gap-1.5 pt-1.5".into(),
        ))
        .child(press("Answer yes", "Yes", "accepted"))
        .child(press("Answer no", "No", "declined"))
        .child(press("Answer maybe", "Maybe", "tentative"))
}

fn shown(b: &Bridge, m: Mail, card: &Card) -> View {
    let invite = &card.invite;
    let tile = invite.when.as_ref().map(day_of).map_or_else(view, |day| {
        view()
            .app_class(Some(
                format!("w-[44px] items-center rounded-md border {LINE} py-1").into(),
            ))
            .child(
                text(day.format("%b").to_string().to_uppercase())
                    .app_class(Some(format!("text-[10px] {MUTE}").into())),
            )
            .child(
                text(day.format("%-d").to_string())
                    .app_class(Some(format!("text-[18px] font-semibold {INK}").into())),
            )
    });
    let title = if invite.title.is_empty() {
        "Untitled event".to_owned()
    } else {
        invite.title.clone()
    };
    let mut words = view()
        .app_class(Some("flex-1 min-w-0 gap-0.5".into()))
        .child(
            text(title)
                .number_of_lines(2)
                .app_class(Some(format!("text-[13px] font-semibold {INK}").into())),
        );
    if let Some(when) = &invite.when {
        words = words.child(line(when_line(when, &Local), MUTE));
    }
    if !invite.place.is_empty() {
        words = words.child(line(invite.place.clone(), MUTE).number_of_lines(1));
    }
    words = words.child(line(about(invite, &card.calendar), MUTE));
    if invite.method == "request" && card.calendar != Calendar::Cancelled {
        match card.calendar {
            Calendar::Missing => {
                words = words.child(line("Not in your Calendar yet.".to_owned(), DIM));
            }
            Calendar::Differs => {
                words = words.child(line(
                    "This invite doesn't match your Calendar.".to_owned(),
                    DIM,
                ));
            }
            _ => {}
        }
        words = words.child(answers(b, m, card));
    }
    if let Some(said) = &card.said {
        words = words.child(line(said.clone(), MUTE));
    }
    view()
        .app_class(Some(
            format!("flex-row items-start gap-3 px-3 py-2.5 border-b {LINE}").into(),
        ))
        .child(tile)
        .child(words)
}

/// Asks the plugin for `thread`'s invite and holds what it says.
async fn look(b: Bridge, m: Mail, thread: String) {
    let answer = read(&b, "gws_mail_invite", json!({"thread": thread})).await;
    // the person has gone on to another
    if m.chosen.get_untracked().as_ref() != Some(&thread) {
        return;
    }
    if let Some(shows) = shows_of(answer) {
        m.invite.set(Some(Held { thread, shows }));
    }
}

/// Why there's no card, and a way to ask again.
fn failed(b: &Bridge, m: Mail, thread: String, why: String) -> View {
    let b = b.clone();
    view()
        .app_class(Some(
            format!("flex-row flex-wrap items-center gap-2 px-3 py-1.5 border-b {LINE}").into(),
        ))
        .child(line(why, MUTE))
        .child(
            view()
                .app_class(Some(format!("px-3 py-1 rounded-md border {LINE}").into()))
                .label("Read this invite again")
                .on_press(move || {
                    let job: crate::shell::Job = Box::pin(look(b.clone(), m, thread.clone()));
                    m.beside.with_untracked(|beside| drop(beside.send(job)));
                })
                .child(text("Try again").app_class(Some(format!("text-[12px] {INK}").into()))),
        )
}

/// What the plugin's answer shows.
fn shows_of(answer: Result<Value, String>) -> Option<Shows> {
    let v = match answer {
        Err(e) if e == "sign-in" => return None,
        Err(e) if e.is_empty() => {
            return Some(Shows::Line(
                "Couldn't read this invite. Try again.".to_owned(),
            ));
        }
        Err(e) => return Some(Shows::Line(e)),
        Ok(v) => v,
    };
    if v.get("refused").is_some() {
        let why = v.get("message").and_then(Value::as_str);
        return Some(Shows::Line(why.unwrap_or("That didn't work.").to_owned()));
    }
    let Some(invite) = v.get("invite").and_then(invite_of) else {
        return Some(Shows::Nothing);
    };
    let calendar = v.get("calendar").unwrap_or(&Value::Null);
    Some(Shows::Card(Box::new(Card {
        invite: as_calendar_has_it(invite, calendar.get("event")),
        calendar: calendar_of(calendar),
        busy: false,
        said: None,
    })))
}

/// The open conversation's invite, if it has one: asked of the plugin, which
/// reads it and looks in Calendar, once for each conversation opened. Nothing
/// shows for a mail with none.
pub(super) fn card(b: Bridge, m: Mail) -> impl IntoElement {
    let thread = m.chosen.get_untracked();
    let held = m
        .invite
        .with_untracked(|h| h.as_ref().map(|h| h.thread.clone()));
    if let Some(thread) = thread
        && held.as_ref() != Some(&thread)
    {
        b.spawn(look(b.clone(), m, thread));
    }
    dynamic(move || {
        let held = m.invite.get();
        let open = m.chosen.get_untracked();
        match held {
            Some(Held { thread, shows }) if Some(&thread) == open.as_ref() => match shows {
                Shows::Card(card) => shown(&b, m, &card),
                Shows::Line(why) => failed(&b, m, thread, why),
                Shows::Nothing => view(),
            },
            _ => view(),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invite(method: &str) -> Value {
        json!({"message": "m1", "uid": "u1", "method": method, "title": "Roof inspection", "start": "2026-11-10T02:00:00Z", "end": "2026-11-10T03:00:00Z", "all_day": false, "place": "12 Mabini St", "organizer": "Ana Reyes"})
    }

    #[test]
    fn each_kind_of_invite_says_what_it_is() {
        let request = invite_of(&invite("request")).unwrap();
        assert_eq!(about(&request, &Calendar::Missing), "Ana Reyes invited you");
        assert_eq!(
            about(&request, &Calendar::Cancelled),
            "This event was cancelled."
        );
        let cancel = invite_of(&invite("cancel")).unwrap();
        assert_eq!(
            about(&cancel, &Calendar::Missing),
            "This event was cancelled."
        );
        let mut reply = invite("reply");
        assert_eq!(invite_of(&reply), None, "a reply names who answered");
        for (response, said) in [
            ("accepted", "Ben Cruz accepted."),
            ("declined", "Ben Cruz declined."),
            ("tentative", "Ben Cruz said maybe."),
        ] {
            reply["reply"] = json!({"who": "Ben Cruz", "response": response});
            assert_eq!(about(&invite_of(&reply).unwrap(), &Calendar::Missing), said);
        }
        assert_eq!(invite_of(&invite("publish")), None);
        assert_eq!(invite_of(&Value::Null), None);
    }

    #[test]
    fn calendar_says_whether_the_buttons_can_answer() {
        assert_eq!(calendar_of(&Value::Null), Calendar::Missing);
        assert_eq!(
            calendar_of(&json!({"found": false, "guest": false})),
            Calendar::Missing
        );
        assert_eq!(
            calendar_of(&json!({"found": true, "matches": true, "guest": false})),
            Calendar::NotAGuest
        );
        assert_eq!(
            calendar_of(
                &json!({"found": true, "matches": true, "guest": true, "response": "tentative"})
            ),
            Calendar::Guest("tentative".to_owned())
        );
        // a guest of an event the invite doesn't describe answers nothing
        assert_eq!(
            calendar_of(
                &json!({"found": true, "matches": false, "guest": true, "response": "needsAction"})
            ),
            Calendar::Differs
        );
    }

    #[test]
    fn only_an_answer_that_says_it_was_sent_is_a_reply_sent() {
        assert_eq!(
            sent(&json!({"sent": true, "event": {"response": "accepted"}})),
            Ok(())
        );
        // written, though the read after it found nothing
        assert_eq!(sent(&json!({"sent": true, "event": null})), Ok(()));
        assert_eq!(
            sent(&json!({"structuredContent": {"sent": true, "event": null}})),
            Ok(())
        );
        assert_eq!(
            sent(
                &json!({"isError": true, "content": [{"type": "text", "text": "Calendar had a problem just now. Try again."}]})
            ),
            Err("Calendar had a problem just now. Try again.".to_owned())
        );
        assert_eq!(
            sent(&json!({"code": "busy", "message": "Gmail didn't send this invite just now."})),
            Err("Gmail didn't send this invite just now.".to_owned())
        );
        assert_eq!(
            sent(&json!({"refused": "changed", "message": "No."})),
            Err("No.".to_owned())
        );
        assert!(sent(&json!({"event": {"response": "accepted"}})).is_err());
    }

    #[test]
    fn calendars_own_words_stand_in_for_the_invites() {
        let ics = invite_of(&invite("request")).unwrap();
        let ours = as_calendar_has_it(
            ics.clone(),
            Some(
                &json!({"title": "Roof check", "start": "2026-11-12T02:00:00Z", "end": "2026-11-12T03:00:00Z", "all_day": false, "place": "Site office", "organizer": "Ana R."}),
            ),
        );
        assert_eq!(
            (
                ours.title.as_str(),
                ours.place.as_str(),
                ours.organizer.as_str()
            ),
            ("Roof check", "Site office", "Ana R.")
        );
        assert_ne!(ours.when, ics.when);
        assert_eq!(
            ours.message, ics.message,
            "the answer goes from the same mail"
        );
        assert_eq!(as_calendar_has_it(ics.clone(), None), ics);
        assert_eq!(
            calendar_of(&json!({"found": true, "cancelled": true})),
            Calendar::Cancelled
        );
    }

    #[test]
    fn a_look_that_failed_says_why_in_the_cards_place() {
        assert_eq!(
            shows_of(Err(
                "Gmail didn't send this invite just now. Try again in a moment.".to_owned()
            )),
            Some(Shows::Line(
                "Gmail didn't send this invite just now. Try again in a moment.".to_owned()
            ))
        );
        assert_eq!(
            shows_of(Ok(
                json!({"refused": "invalid_args", "message": "That isn't a mail."})
            )),
            Some(Shows::Line("That isn't a mail.".to_owned()))
        );
        assert_eq!(shows_of(Ok(json!({"invite": null}))), Some(Shows::Nothing));
        assert_eq!(shows_of(Err("sign-in".to_owned())), None);
    }

    #[test]
    fn an_all_day_invite_tiles_its_own_date() {
        let mut v = invite("request");
        v["start"] = json!("2026-11-24");
        v["end"] = json!("2026-11-26");
        v["all_day"] = json!(true);
        let when = invite_of(&v).unwrap().when.unwrap();
        assert_eq!(
            day_of(&when),
            NaiveDate::from_ymd_opt(2026, 11, 24).unwrap()
        );
    }
}
