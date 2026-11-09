//! The invite card, above the letter: the event a Calendar invite carries, read
//! by the plugin from its `.ics`, with Yes, No and Maybe answered through
//! Calendar so the organiser hears.
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

/// What Calendar says of an invitation's event.
#[derive(Clone, PartialEq, Debug)]
enum Calendar {
    /// Not there yet: the buttons wait for Calendar's sync.
    Missing,
    /// There, but the person isn't on its guest list.
    NotAGuest,
    /// An event with the invite's UID whose organiser or start isn't the
    /// invite's: a forged or stale `.ics`, which can't answer it.
    Differs,
    /// There, with the person's answer so far.
    Guest(String),
}

fn calendar_of(v: &Value) -> Calendar {
    let yes = |key: &str| v.get(key) == Some(&Value::Bool(true));
    match (yes("found"), yes("matches"), yes("guest")) {
        (false, ..) => Calendar::Missing,
        (true, false, _) => Calendar::Differs,
        (true, true, true) => Calendar::Guest(text_in(v, "response")),
        (true, true, false) => Calendar::NotAGuest,
    }
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
fn about(invite: &Invite) -> String {
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

/// The person's answer, called in this press.
fn answer_now(b: &Bridge, m: Mail, state: Signal<Option<Card>>, response: &'static str) {
    // the press is on the card, so the card is there
    let Some(mut card) = state.get_untracked() else {
        return;
    };
    if card.busy {
        return;
    }
    let call = b.call(
        "gws_mail_invite_reply",
        json!({"message": card.invite.message, "response": response}),
    );
    card.busy = true;
    card.said = None;
    state.try_set(Some(card.clone()));
    // nothing else changes the card meanwhile: a press is refused while it's
    // busy. It runs beside Mail's jobs, as an attachment's open does
    let job = Box::pin(async move {
        card.busy = false;
        card.said = Some(match call.await {
            Ok(v) if v.get("refused").is_some() => v
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("That didn't work.")
                .to_owned(),
            Ok(_) => {
                card.calendar = Calendar::Guest(response.to_owned());
                "Your reply was sent.".to_owned()
            }
            Err(e) => e.message,
        });
        state.try_set(Some(card));
    });
    m.beside.with_untracked(|beside| drop(beside.send(job)));
}

fn answers(b: &Bridge, m: Mail, state: Signal<Option<Card>>, card: &Card) -> View {
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
            .on_press(move || answer_now(&b, m, state, response))
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

fn shown(b: &Bridge, m: Mail, state: Signal<Option<Card>>, card: &Card) -> View {
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
    words = words.child(line(about(invite), MUTE));
    if invite.method == "request" {
        match card.calendar {
            Calendar::Missing => {
                words = words.child(line("Not in your Calendar yet.".to_owned(), DIM));
            }
            Calendar::Differs => {
                words = words.child(line(
                    "This invite doesn't match the event in your Calendar.".to_owned(),
                    DIM,
                ));
            }
            _ => {}
        }
        words = words.child(answers(b, m, state, card));
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

/// The open conversation's invite, if it has one: asked of the plugin, which
/// reads it and looks in Calendar. Nothing shows for a mail with none.
pub(super) fn card(b: Bridge, m: Mail) -> impl IntoElement {
    let state: Signal<Option<Card>> = signal(None);
    if let Some(thread) = m.chosen.get_untracked() {
        let reader = b.clone();
        b.spawn(async move {
            let Ok(v) = read(&reader, "gws_mail_invite", json!({"thread": thread})).await else {
                return;
            };
            let Some(invite) = v.get("invite").and_then(invite_of) else {
                return;
            };
            let calendar = calendar_of(v.get("calendar").unwrap_or(&Value::Null));
            state.try_set(Some(Card {
                invite,
                calendar,
                busy: false,
                said: None,
            }));
        });
    }
    dynamic(move || {
        state
            .get()
            .map_or_else(view, |card| shown(&b, m, state, &card))
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
        assert_eq!(about(&request), "Ana Reyes invited you");
        let cancel = invite_of(&invite("cancel")).unwrap();
        assert_eq!(about(&cancel), "This event was cancelled.");
        let mut reply = invite("reply");
        assert_eq!(invite_of(&reply), None, "a reply names who answered");
        for (response, said) in [
            ("accepted", "Ben Cruz accepted."),
            ("declined", "Ben Cruz declined."),
            ("tentative", "Ben Cruz said maybe."),
        ] {
            reply["reply"] = json!({"who": "Ben Cruz", "response": response});
            assert_eq!(about(&invite_of(&reply).unwrap()), said);
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
