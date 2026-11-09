//! The invite card, above the letter: the event a Calendar invite carries, read
//! by the plugin from its `.ics`, with Yes, No and Maybe answered through
//! Calendar so the organiser hears.
//!
//! Once Calendar has the event, the card shows Calendar's own title, time,
//! place and organiser, never the `.ics`'s: what a press answers is what the
//! person sees. Until then, the `.ics`'s words show as the mail's, not
//! checked. The card's state lives in [`Mail`], by conversation, so a page
//! shown again (pictures let in) keeps an answer on its way, and a look for
//! one conversation never lands on another's card.
//!
//! The label rule holds: the card's words are texts, read as values, and the
//! answers carry fixed labels with other words than they show, so the Bot's
//! filter names none of them and the AI can't press one. A press is the
//! person's, called at the press, as who pressed is judged by that moment.

use std::collections::BTreeMap;

use buwiz_app::Bridge;
use chrono::{Local, NaiveDate};
use nsh_core::AccessibilityState;
use nsh_core::prelude::*;
use serde_json::{Value, json};

use super::Mail;
use crate::calendar::event::text as text_in;
use crate::calendar::time::{When, when_line};
use crate::shell::{CHOSEN, DIM, INK, LINE, MUTE, RETRIES, backoff, read, sleep};

const SENT: &str = "Your reply was sent.";
const NOT_SHOWN: &str = "Sent. Calendar hasn't shown it yet.";
const NOT_YET: &str = "Not in your Calendar yet.";
const DIFFERS: &str = "This invite doesn't match your Calendar.";
const UNCHECKED: &str = "From the mail, not checked with Calendar.";
const UNVERIFIED: &str = "Gmail couldn't confirm this invite came from its organiser.";
const DAMAGED: &str = "Can't read this invite.";
const SIGNED_OUT: &str = "Sign in to Google to see this invite.";

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
    /// Not there yet: the buttons wait for Calendar's sync, and the card can
    /// look again.
    Missing,
    /// There, and cancelled.
    Cancelled,
    /// There, but the person isn't on its guest list.
    NotAGuest,
    /// An event with the invite's UID that doesn't read as the invite does:
    /// a forged or stale `.ics`, which can't answer it, and whose words
    /// don't show.
    Differs,
    /// Gmail couldn't vouch that the organiser sent it: not asked.
    Unverified,
    /// Calendar didn't answer, in its words: the invite shows as Gmail sent it.
    Unanswered(String),
    /// There, with the person's answer so far.
    Guest(String),
}

fn calendar_of(v: &Value) -> Calendar {
    let yes = |key: &str| v.get(key) == Some(&Value::Bool(true));
    if yes("unverified") {
        return Calendar::Unverified;
    }
    if let Some(why) = v.get("error").and_then(Value::as_str) {
        return Calendar::Unanswered(why.to_owned());
    }
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

/// Every conversation's card this session, by thread.
pub(super) type Cards = BTreeMap<String, Held>;

/// One conversation's card.
#[derive(Clone, PartialEq, Debug, Default)]
pub(super) struct Held {
    /// What the last look said; `None` before one has.
    shows: Option<Shows>,
    /// A look is on its way: a rebuild or a second press starts no other.
    looking: bool,
}

impl Held {
    /// An answer is on its way: nothing but its own look changes the card.
    fn busy(&self) -> bool {
        matches!(&self.shows, Some(Shows::Card(c)) if c.busy)
    }

    /// Something of this card must outlast leaving the conversation.
    fn kept(&self) -> bool {
        self.looking
            || self.busy()
            || matches!(&self.shows, Some(Shows::Card(c)) if c.unshown.is_some())
    }
}

#[derive(Clone, PartialEq, Debug)]
enum Shows {
    Nothing,
    /// Why there's no card yet, as the plugin said it, and a try again.
    Line(String),
    /// Why there's no card, for good.
    Said(String),
    Card(Box<Card>),
}

#[derive(Clone, PartialEq, Debug)]
struct Card {
    invite: Invite,
    calendar: Calendar,
    /// An answer is on its way, or its look.
    busy: bool,
    said: Option<String>,
    /// An answer that may have reached Calendar, which Calendar hasn't
    /// shown yet: the buttons stay off, whatever the time or an error, until
    /// a look shows an answer there.
    unshown: Option<String>,
}

/// Claims a look for `thread`: false while one is on its way or an answer
/// is, so a rebuild or a second press starts no other.
fn begin(cards: &mut Cards, thread: &str) -> bool {
    let held = cards.entry(thread.to_owned()).or_default();
    if held.looking || held.busy() {
        return false;
    }
    held.looking = true;
    true
}

/// A look's answer, onto its own conversation's card only.
fn landed(cards: &mut Cards, thread: &str, shows: Shows) {
    let held = cards.entry(thread.to_owned()).or_default();
    held.looking = false;
    if held.busy() {
        return;
    }
    let unshown = match &held.shows {
        Some(Shows::Card(c)) => c.unshown.clone(),
        _ => None,
    };
    held.shows = Some(match (unshown, shows) {
        (Some(response), Shows::Card(fresh)) => Shows::Card(Box::new(settle(*fresh, &response))),
        // a look that failed takes nothing back of an answer that may have gone
        (Some(_), _) => return,
        (None, shows) => shows,
    });
}

/// Whether Calendar shows an answer from the person: theirs, or one since.
fn shows_an_answer(card: &Card) -> bool {
    matches!(&card.calendar, Calendar::Guest(r) if r != "needsAction")
}

/// A fresh card for an answer that may have gone as `response`.
fn settle(mut fresh: Card, response: &str) -> Card {
    if shows_an_answer(&fresh) {
        fresh.unshown = None;
        fresh.said =
            (fresh.calendar == Calendar::Guest(response.to_owned())).then(|| SENT.to_owned());
    } else {
        fresh.unshown = Some(response.to_owned());
        fresh.said = Some(NOT_SHOWN.to_owned());
    }
    fresh
}

/// The card for `thread`, changed by `f`.
fn change(m: Mail, thread: &str, f: impl FnOnce(&mut Card)) {
    m.invite.update(|cards| {
        if let Some(Held {
            shows: Some(Shows::Card(card)),
            ..
        }) = cards.get_mut(thread)
        {
            f(card);
        }
    });
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

/// What a card says, top to bottom, and whether it can look in Calendar
/// again. An invite unlike its Calendar event says only that: neither its
/// words nor Calendar's show under the other's name.
#[derive(Debug, PartialEq)]
struct Words {
    /// The event: title, time, place, and what this is.
    event: Option<(String, Option<String>, String, String)>,
    /// Lines under it, dimmed.
    notes: Vec<&'static str>,
    /// Calendar's own reason it didn't answer.
    why: Option<String>,
    /// The buttons show; whether they light is [`lit`]'s.
    buttons: bool,
    look_again: bool,
}

fn words(card: &Card) -> Words {
    let invite = &card.invite;
    let request = invite.method == "request"
        && card.calendar != Calendar::Cancelled
        && card.calendar != Calendar::Unverified;
    if request && card.calendar == Calendar::Differs {
        return Words {
            event: None,
            notes: vec![DIFFERS],
            why: None,
            buttons: false,
            look_again: false,
        };
    }
    let title = if invite.title.is_empty() {
        "Untitled event".to_owned()
    } else {
        invite.title.clone()
    };
    let when = invite.when.as_ref().map(|w| when_line(w, &Local));
    let event = Some((
        title,
        when,
        invite.place.clone(),
        about(invite, &card.calendar),
    ));
    let (mut notes, mut why, mut look_again) = (Vec::new(), None, card.unshown.is_some());
    // a cancel or a reply from a sender Gmail can't vouch for is the mail's word
    if card.calendar == Calendar::Unverified {
        notes = vec![UNCHECKED, UNVERIFIED];
    }
    if request {
        match &card.calendar {
            Calendar::Missing => {
                notes = vec![UNCHECKED, NOT_YET];
                look_again = true;
            }
            Calendar::Unanswered(e) => {
                notes = vec![UNCHECKED];
                why = Some(e.clone());
                look_again = true;
            }
            _ => {}
        }
    }
    Words {
        event,
        notes,
        why,
        // an unverified invitation shows its buttons, off
        buttons: request || (invite.method == "request" && card.calendar == Calendar::Unverified),
        look_again,
    }
}

/// Whether the buttons light: only for a guest of Calendar's own event that
/// reads as the invite does, from a sender Gmail vouched for, and never
/// while an answer or its look is on its way.
fn lit(card: &Card) -> bool {
    matches!(card.calendar, Calendar::Guest(_)) && !card.busy && card.unshown.is_none()
}

/// Whether a reply's answer leaves it open that the PATCH landed: it said
/// it went, or the plugin says it may have, or the answer was lost on the way.
fn may_have_gone(answer: &Result<Value, buwiz_app::BuwizError>) -> bool {
    match answer {
        Ok(v) => {
            sent(v).is_ok()
                || v.pointer("/structuredContent/code").and_then(Value::as_str)
                    == Some("unconfirmed")
        }
        Err(_) => true,
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

/// Whether a fresh look shows `response` as the person's answer.
fn confirms(shows: &Shows, response: &str) -> bool {
    matches!(shows, Shows::Card(c) if c.calendar == Calendar::Guest(response.to_owned()))
}

/// The card once an answer's look is in. A fresh card is Calendar's word;
/// a look that failed keeps the card as it was. An answer that may have
/// gone stays unshown until Calendar shows one: no timer and no error
/// lights the buttons again.
fn after_answer(old: &Card, fresh: Shows, response: &str, said: String, maybe: bool) -> Shows {
    let mut card = match fresh {
        Shows::Card(card) => *card,
        _ => old.clone(),
    };
    card.busy = false;
    if maybe {
        return Shows::Card(Box::new(settle(card, response)));
    }
    card.unshown = None;
    card.said = Some(said);
    Shows::Card(Box::new(card))
}

/// The person's answer, called in this press.
fn answer_now(b: &Bridge, m: Mail, thread: String, response: &'static str) {
    let card = m.invite.with_untracked(|cards| match cards.get(&thread) {
        Some(Held {
            shows: Some(Shows::Card(card)),
            ..
        }) => Some(card.clone()),
        _ => None,
    });
    let Some(card) = card.filter(|c| lit(c)) else {
        return;
    };
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
    let b = b.clone();
    let job = Box::pin(async move {
        let answer = call.await;
        let maybe = may_have_gone(&answer);
        let why = match answer.map(|v| sent(&v)) {
            Ok(Ok(())) => SENT.to_owned(),
            Ok(Err(why)) => why,
            Err(e) => e.message,
        };
        // the buttons stay busy until Calendar shows what happened: a reply
        // whose answer was lost on the way may still have gone
        let mut attempt = 0;
        let fresh = loop {
            let fresh = shows_of(read(&b, "gws_mail_invite", json!({"thread": thread})).await);
            let shown = matches!(&fresh, Shows::Card(c) if shows_an_answer(c));
            if shown || !maybe || attempt >= RETRIES {
                break fresh;
            }
            sleep(backoff(attempt)).await;
            attempt += 1;
        };
        let said = if confirms(&fresh, response) {
            SENT.to_owned()
        } else {
            why
        };
        m.invite.update(|cards| {
            if let Some(held) = cards.get_mut(&thread)
                && let Some(Shows::Card(old)) = &held.shows
            {
                held.shows = Some(after_answer(old, fresh, response, said, maybe));
            }
        });
    });
    m.beside.with_untracked(|beside| drop(beside.send(job)));
}

fn answers(b: &Bridge, m: Mail, thread: &str, card: &Card) -> View {
    let on = lit(card);
    let mine = match &card.calendar {
        Calendar::Guest(said) => said.as_str(),
        _ => "",
    };
    let press = |label: &'static str, name: &'static str, response: &'static str| {
        let b = b.clone();
        let thread = thread.to_owned();
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
            .on_press(move || answer_now(&b, m, thread.clone(), response))
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

/// A small button that asks the plugin again, under a fixed label.
fn again(b: &Bridge, m: Mail, thread: &str, label: &'static str, name: &'static str) -> View {
    let (b, thread) = (b.clone(), thread.to_owned());
    view()
        .app_class(Some(format!("px-3 py-1 rounded-md border {LINE}").into()))
        .label(label)
        .on_press(move || start(&b, m, &thread))
        .child(text(name).app_class(Some(format!("text-[12px] {INK}").into())))
}

fn shown(b: &Bridge, m: Mail, thread: &str, card: &Card) -> View {
    let said = words(card);
    let tile = match (&said.event, card.invite.when.as_ref()) {
        (Some(_), Some(when)) => {
            let day = day_of(when);
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
        }
        _ => view(),
    };
    let mut words = view().app_class(Some("flex-1 min-w-0 gap-0.5".into()));
    if let Some((title, when, place, about)) = said.event {
        words = words.child(
            text(title)
                .number_of_lines(2)
                .app_class(Some(format!("text-[13px] font-semibold {INK}").into())),
        );
        if let Some(when) = when {
            words = words.child(line(when, MUTE));
        }
        if !place.is_empty() {
            words = words.child(line(place, MUTE).number_of_lines(1));
        }
        words = words.child(line(about, MUTE));
    }
    for note in said.notes {
        words = words.child(line(note.to_owned(), DIM));
    }
    if let Some(why) = said.why {
        words = words.child(line(why, MUTE));
    }
    if said.buttons {
        let mut row = answers(b, m, thread, card);
        if said.look_again {
            row = row.child(again(b, m, thread, "Look in Calendar again", "Check again"));
        }
        words = words.child(row);
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

/// Starts a look for `thread`, unless one or an answer is on its way.
fn start(b: &Bridge, m: Mail, thread: &str) {
    let mut claimed = false;
    m.invite.update(|cards| claimed = begin(cards, thread));
    if claimed {
        let job: crate::shell::Job = Box::pin(look(b.clone(), m, thread.to_owned()));
        m.beside.with_untracked(|beside| drop(beside.send(job)));
    }
}

/// Asks the plugin for `thread`'s invite and holds what it says, for that
/// conversation, wherever the person has gone since.
async fn look(b: Bridge, m: Mail, thread: String) {
    let answer = read(&b, "gws_mail_invite", json!({"thread": thread})).await;
    if answer.as_ref().err().is_some_and(|e| e == "sign-in") {
        m.signed_out.set(true);
    }
    let shows = shows_of(answer);
    m.invite.update(|cards| landed(cards, &thread, shows));
}

/// Why there's no card, and a way to ask again.
fn failed(b: &Bridge, m: Mail, thread: &str, why: String) -> View {
    view()
        .app_class(Some(
            format!("flex-row flex-wrap items-center gap-2 px-3 py-1.5 border-b {LINE}").into(),
        ))
        .child(line(why, MUTE))
        .child(again(b, m, thread, "Read this invite again", "Try again"))
}

/// What the plugin's answer shows.
fn shows_of(answer: Result<Value, String>) -> Shows {
    let v = match answer {
        Err(e) if e == "sign-in" => return Shows::Said(SIGNED_OUT.to_owned()),
        Err(e) if e.is_empty() => {
            return Shows::Line("Couldn't read this invite. Try again.".to_owned());
        }
        Err(e) => return Shows::Line(e),
        Ok(v) => v,
    };
    if v.get("refused").is_some() {
        let why = v.get("message").and_then(Value::as_str);
        return Shows::Line(why.unwrap_or("That didn't work.").to_owned());
    }
    if v.pointer("/invite/damaged") == Some(&Value::Bool(true)) {
        return Shows::Said(DAMAGED.to_owned());
    }
    let Some(invite) = v.get("invite").and_then(invite_of) else {
        return Shows::Nothing;
    };
    let calendar = v.get("calendar").unwrap_or(&Value::Null);
    Shows::Card(Box::new(Card {
        invite: as_calendar_has_it(invite, calendar.get("event")),
        calendar: calendar_of(calendar),
        busy: false,
        said: None,
        unshown: None,
    }))
}

/// The conversation `thread` was opened, or opened again: its card looks
/// afresh, as Calendar may have its event by now. Cards of conversations
/// left behind go, unless something of theirs is on its way.
pub(super) fn opened(b: &Bridge, m: Mail, thread: &str) {
    if m.chosen.with_untracked(|c| c.as_deref() != Some(thread)) {
        return;
    }
    m.invite
        .update(|cards| cards.retain(|t, h| t == thread || h.kept()));
    start(b, m, thread);
}

/// The open conversation's invite, if it has one: asked of the plugin as the
/// conversation opens ([`opened`]), and here only if it never was. Nothing
/// shows for a mail with none.
pub(super) fn card(b: Bridge, m: Mail) -> impl IntoElement {
    if let Some(thread) = m.chosen.get_untracked()
        && m.invite
            .with_untracked(|cards| !cards.contains_key(&thread))
    {
        start(&b, m, &thread);
    }
    dynamic(move || {
        let cards = m.invite.get();
        let Some(thread) = m.chosen.get_untracked() else {
            return view();
        };
        match cards.get(&thread).and_then(|h| h.shows.clone()) {
            Some(Shows::Card(card)) => shown(&b, m, &thread, &card),
            Some(Shows::Line(why)) => failed(&b, m, &thread, why),
            Some(Shows::Said(why)) => view()
                .app_class(Some(format!("px-3 py-1.5 border-b {LINE}").into()))
                .child(line(why, MUTE)),
            Some(Shows::Nothing) | None => view(),
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
            calendar_of(&json!({"found": true, "matches": false, "cancelled": false})),
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
            Shows::Line(
                "Gmail didn't send this invite just now. Try again in a moment.".to_owned()
            )
        );
        assert_eq!(
            shows_of(Ok(
                json!({"refused": "invalid_args", "message": "That isn't a mail."})
            )),
            Shows::Line("That isn't a mail.".to_owned())
        );
        assert_eq!(shows_of(Ok(json!({"invite": null}))), Shows::Nothing);
        // a sign-in that lapsed is said, not a silent blank
        assert_eq!(
            shows_of(Err("sign-in".to_owned())),
            Shows::Said(SIGNED_OUT.to_owned())
        );
        // a damaged one for good, with no try again
        assert_eq!(
            shows_of(Ok(json!({"invite": {"damaged": true}, "calendar": null}))),
            Shows::Said(DAMAGED.to_owned())
        );
    }

    fn card(calendar: Calendar) -> Card {
        Card {
            invite: invite_of(&invite("request")).unwrap(),
            calendar,
            busy: false,
            said: None,
            unshown: None,
        }
    }

    #[test]
    fn an_invite_unlike_its_calendar_event_shows_none_of_its_words() {
        let said = words(&card(Calendar::Differs));
        assert_eq!(said.event, None);
        assert_eq!(said.notes, [DIFFERS]);
        assert!(!said.buttons);
    }

    #[test]
    fn what_calendar_hasnt_checked_is_said_to_be_the_mails() {
        let missing = words(&card(Calendar::Missing));
        assert_eq!(missing.event.unwrap().0, "Roof inspection");
        assert_eq!(missing.notes, [UNCHECKED, NOT_YET]);
        assert!(missing.look_again, "it can check again");
        let down = words(&card(Calendar::Unanswered("Calendar is busy.".to_owned())));
        assert_eq!(
            down.event.unwrap().0,
            "Roof inspection",
            "kept, not replaced"
        );
        assert_eq!(down.notes, [UNCHECKED]);
        assert_eq!(down.why.as_deref(), Some("Calendar is busy."));
        let unverified = words(&card(Calendar::Unverified));
        assert_eq!(unverified.notes, [UNCHECKED, UNVERIFIED]);
        assert!(!unverified.look_again);
        let guest = words(&card(Calendar::Guest("needsAction".to_owned())));
        assert!(guest.notes.is_empty() && !guest.look_again);
        for c in [
            Calendar::Missing,
            Calendar::Differs,
            Calendar::Unverified,
            Calendar::Unanswered(String::new()),
            Calendar::NotAGuest,
        ] {
            assert!(!lit(&card(c.clone())), "{c:?}");
        }
        assert!(lit(&card(Calendar::Guest("needsAction".to_owned()))));
    }

    #[test]
    fn a_cancel_gmail_cant_vouch_for_is_said_to_be_the_mails() {
        let cancel = Card {
            invite: invite_of(&invite("cancel")).unwrap(),
            ..card(Calendar::Unverified)
        };
        let said = words(&cancel);
        assert_eq!(said.notes, [UNCHECKED, UNVERIFIED]);
        assert!(!said.buttons);
        assert_eq!(
            calendar_of(&json!({"unverified": true})),
            Calendar::Unverified
        );
    }

    #[test]
    fn calendar_says_why_it_wasnt_asked_or_didnt_answer() {
        assert_eq!(
            calendar_of(&json!({"unverified": true})),
            Calendar::Unverified
        );
        assert_eq!(
            calendar_of(&json!({"error": "Calendar is busy."})),
            Calendar::Unanswered("Calendar is busy.".to_owned())
        );
    }

    #[test]
    fn each_conversation_holds_its_own_card_and_one_look_at_a_time() {
        let mut cards = Cards::new();
        assert!(begin(&mut cards, "a"));
        assert!(!begin(&mut cards, "a"), "a second press while it looks");
        landed(
            &mut cards,
            "a",
            Shows::Card(Box::new(card(Calendar::Missing))),
        );
        assert!(begin(&mut cards, "a"), "opened again, it looks again");
        landed(
            &mut cards,
            "a",
            Shows::Card(Box::new(card(Calendar::Guest("needsAction".to_owned())))),
        );
        if let Some(Shows::Card(c)) = &mut cards.get_mut("a").unwrap().shows {
            c.busy = true;
        }
        // another's look lands on its own card, and A's answer stays on its way
        assert!(begin(&mut cards, "b"));
        landed(&mut cards, "b", Shows::Nothing);
        assert!(cards["a"].busy());
        assert!(!begin(&mut cards, "a"), "no look over an answer on its way");
        landed(&mut cards, "a", Shows::Nothing);
        assert!(cards["a"].busy(), "nor does one that was already out");
    }

    #[test]
    fn an_answer_is_settled_by_what_calendar_shows_after_it() {
        let asked = card(Calendar::Guest("needsAction".to_owned()));
        let fresh = |r: &str| Shows::Card(Box::new(card(Calendar::Guest(r.to_owned()))));
        assert!(confirms(&fresh("accepted"), "accepted"));
        assert!(!confirms(&fresh("needsAction"), "accepted"));
        assert!(!confirms(&Shows::Line("x".to_owned()), "accepted"));
        // a look that failed after a reply that may have gone: off, unshown
        let Shows::Card(after) = after_answer(
            &asked,
            Shows::Line("x".to_owned()),
            "accepted",
            SENT.to_owned(),
            true,
        ) else {
            panic!("a card");
        };
        assert!(!after.busy);
        assert!(!lit(&after), "no timer or error lights it");
        assert_eq!(after.said.as_deref(), Some(NOT_SHOWN));
        assert!(words(&after).look_again);
        // a look that still shows no answer keeps it so; one that shows one lights it
        let mut cards = Cards::new();
        cards.insert(
            "a".to_owned(),
            Held {
                shows: Some(Shows::Card(after)),
                looking: false,
            },
        );
        landed(&mut cards, "a", fresh("needsAction"));
        landed(&mut cards, "a", Shows::Line("Gmail is busy.".to_owned()));
        let Some(Shows::Card(c)) = &cards["a"].shows else {
            panic!("a card");
        };
        assert!(!lit(c), "still unshown");
        landed(&mut cards, "a", fresh("accepted"));
        let Some(Shows::Card(c)) = &cards["a"].shows else {
            panic!("a card");
        };
        assert!(lit(c));
        assert_eq!(c.said.as_deref(), Some(SENT));
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
