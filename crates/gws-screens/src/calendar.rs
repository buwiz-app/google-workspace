//! Calendar: a side column, a header and one of four views, on a screen of its
//! own, with event detail and the editor in the main area.
//!
//! The label rule is Mail's (see the top of `mail.rs`): a press or a row has a
//! fixed `label`, and what's the person's own goes in `accessibility_value` and
//! the row's texts, so no event title, guest or place reaches `ui_look`.
//!
//! Save, Delete, Reply and Undo are app-only tools. Each one's call is made in
//! the press itself and only its answer waits on the queue: the Bot takes who
//! made a call from the moment it's made, so a call made later, from a task,
//! could no longer be told from the person's when the press was the AI's.

pub mod time;

mod cards;
mod detail;
mod editor;
mod event;
mod grid;

use std::future::Future;

use buwiz_app::{Bridge, BuwizError, Placement};
use chrono::{Local, NaiveDate};
use nsh_core::AccessibilityValue;
use nsh_core::element::{Root, input};
use nsh_core::prelude::*;
use serde_json::{Value, json};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

use crate::compose::{self, press_send_button, press_text};
use crate::shell::{
    Account, BG, INK, Job, LINE, Latest, MUTE, PANEL, POLL, RAIL, account_of, line_of, link_bar,
    read, sleep,
};
use editor::Editor;
use event::{Calendar, Detail, Event, Key, calendars_in, rows_in};
use time::Span;

/// How wide the screen must be for the side column to fit beside the views.
const SIDE_FROM: f32 = 640.0;

/// The change a series asks about before it goes.
#[derive(Clone, PartialEq, Debug)]
enum Change {
    Save,
    Delete(Key),
    Reply(Key, &'static str),
}

/// What's waiting on a second press.
#[derive(Clone, PartialEq, Debug)]
enum Ask {
    Delete(Key, i64),
    Scope(Change),
}

/// What the screen is showing right now.
#[derive(Clone, Copy)]
struct Cal {
    span: Signal<Span>,
    anchor: Signal<NaiveDate>,
    events: Signal<Vec<Event>>,
    calendars: Signal<Vec<Calendar>>,
    /// The list has come, so an empty one means none.
    calendars_read: Signal<bool>,
    range_turn: Latest,
    open_turn: Latest,
    loading: Signal<bool>,
    saved: Signal<bool>,
    account: Signal<Account>,
    /// Why the view didn't load, in words for the person.
    problem: Signal<Option<String>>,
    /// The query the list is showing the results of.
    searching: Signal<Option<String>>,
    query: Signal<String>,
    /// The go-to-date field is open.
    going: Signal<bool>,
    going_to: Signal<String>,
    /// The open event, and that one is on its way.
    detail: Signal<Option<Detail>>,
    opening: Signal<bool>,
    editor: Editor,
    ask: Signal<Option<Ask>>,
    /// A Delete or a reply is out: a double click is one press.
    busy: Signal<bool>,
    /// What became of an action: it stays until dismissed or replaced, as a
    /// reload can come at any moment.
    told: Signal<Option<String>>,
    /// The delete Undo would take back.
    undo: Signal<Option<Key>>,
    undoing: Signal<bool>,
    link: Signal<Option<String>>,
    seq: Signal<i64>,
    /// Moves the now line along.
    now: Signal<i64>,
    /// Writes' answers, in the order they were pressed.
    jobs: Signal<UnboundedSender<Job>>,
    /// Reads started by a press, each run on its own: the views they were
    /// pressed on are rebuilt, and a slow one mustn't hold up the next.
    reads: Signal<UnboundedSender<Job>>,
    search: ElementRef,
}

fn value(text: impl Into<String>) -> Option<AccessibilityValue> {
    Some(AccessibilityValue {
        text: Some(text.into().into()),
        ..AccessibilityValue::default()
    })
}

fn today() -> NaiveDate {
    Local::now().date_naive()
}

fn now_ms() -> i64 {
    Local::now().timestamp_millis()
}

fn run(m: Cal, job: impl Future<Output = ()> + 'static) {
    m.jobs.with_untracked(|jobs| drop(jobs.send(Box::pin(job))));
}

fn fetch(m: Cal, job: impl Future<Output = ()> + 'static) {
    m.reads
        .with_untracked(|reads| drop(reads.send(Box::pin(job))));
}

fn tell(m: Cal, line: impl Into<String>) {
    m.told.set(Some(line.into()));
}

/// A failed read that means the sign-in went, or a line about it for the person.
fn failed(m: Cal, why: String) -> Option<String> {
    if why == "sign-in" {
        m.account.set(Account::SignedOut);
        return None;
    }
    Some(why)
}

/// A write's answer: its value, or the refusal's code and words. A refusal the
/// screen acts on comes as an answer, and any other as a failure.
fn outcome(answer: Result<Value, BuwizError>) -> Result<Value, (String, String)> {
    match answer {
        Err(e) => Err((e.code, e.message)),
        Ok(v) => match v.get("refused").and_then(Value::as_str) {
            Some(code) => Err((
                code.to_owned(),
                v.get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("That didn't work.")
                    .to_owned(),
            )),
            None => Ok(v),
        },
    }
}

/// Says why a write didn't go, or that the sign-in went.
fn write_failed(m: Cal, why: String) {
    if why.contains("Sign in to Google") {
        m.account.set(Account::SignedOut);
    } else {
        tell(m, why);
    }
}

/// The words of a refusal that came as an answer, to a read.
fn refused_read(v: &Value) -> Option<String> {
    v.get("refused")?;
    let why = v.get("message").and_then(Value::as_str);
    Some(why.unwrap_or("That didn't work.").to_owned())
}

fn range_args(span: Span, anchor: NaiveDate) -> Value {
    let (first, end) = time::range(span, anchor);
    match time::range_args(first, end, &Local) {
        Some((from, to)) => json!({"from": from, "to": to, "to_date": end.to_string()}),
        None => json!({}),
    }
}

async fn load(b: Bridge, m: Cal) {
    let turn = m.range_turn.ask();
    m.loading.set(true);
    let args = range_args(m.span.get_untracked(), m.anchor.get_untracked());
    let answer = read(&b, "gws_cal_range", args).await;
    // a later view owns the rows and the flag now
    if !m.range_turn.counts(turn) {
        return;
    }
    m.loading.set(false);
    match answer {
        Ok(v) => {
            m.events.set(rows_in(&v, "events"));
            m.saved.set(v.get("saved") == Some(&Value::Bool(true)));
            m.problem.set(None);
        }
        Err(e) => m.problem.set(failed(m, e)),
    }
}

async fn search(b: Bridge, m: Cal, query: String) {
    let turn = m.range_turn.ask();
    m.loading.set(true);
    m.searching.set(Some(query.clone()));
    let answer = read(&b, "gws_search_events", json!({"query": query})).await;
    if !m.range_turn.counts(turn) {
        return;
    }
    m.loading.set(false);
    match answer {
        Ok(v) => {
            m.events.set(rows_in(&v, "items"));
            m.problem.set(None);
        }
        Err(e) => m.problem.set(failed(m, e)),
    }
}

async fn load_calendars(b: Bridge, m: Cal) {
    match read(&b, "gws_cal_list", json!({})).await {
        Ok(v) => {
            m.calendars.set(calendars_in(&v));
            m.calendars_read.set(true);
        }
        Err(e) => {
            if let Some(line) = failed(m, e) {
                m.problem.set(Some(line));
            }
        }
    }
}

/// The view read again, as it stands.
async fn refresh(b: Bridge, m: Cal) {
    match m.searching.get_untracked() {
        Some(query) => search(b, m, query).await,
        None => load(b, m).await,
    }
}

async fn watch(b: Bridge, m: Cal) {
    loop {
        if let Ok(v) = read(&b, "gws_cal_changes", json!({})).await {
            let seq = v.get("seq").and_then(Value::as_i64).unwrap_or(0);
            let last = m.seq.get_untracked();
            m.seq.set(seq);
            if last >= 0 && seq != last {
                refresh(b.clone(), m).await;
                load_calendars(b.clone(), m).await;
            }
        }
        m.now.set(now_ms());
        sleep(POLL).await;
    }
}

/// Closes the open event, and any answer on its way for it.
fn close_detail(m: Cal) {
    m.open_turn.ask();
    m.detail.set(None);
    m.opening.set(false);
    m.ask.set(None);
}

/// The view moved: what's open belongs to the old one.
fn show_view(b: &Bridge, m: Cal, span: Span, anchor: NaiveDate) {
    close_detail(m);
    m.searching.set(None);
    m.span.set(span);
    m.anchor.set(anchor);
    fetch(m, load(b.clone(), m));
}

fn open_event(b: &Bridge, m: Cal, key: Key) {
    m.ask.set(None);
    fetch(m, show_event(b.clone(), m, key));
}

async fn show_event(b: Bridge, m: Cal, key: Key) {
    let turn = m.open_turn.ask();
    m.opening.set(true);
    let answer = read(&b, "gws_event_read", key.args()).await;
    // the person opened another, or left
    if !m.open_turn.counts(turn) {
        return;
    }
    m.opening.set(false);
    match answer.map(|v| (Detail::of(&v), v)) {
        Ok((Some(detail), _)) => m.detail.set(Some(detail)),
        Ok((None, v)) => {
            m.detail.set(None);
            tell(
                m,
                refused_read(&v).unwrap_or_else(|| "That event couldn't be opened.".to_owned()),
            );
        }
        Err(e) => {
            m.detail.set(None);
            if let Some(line) = failed(m, e) {
                tell(m, line);
            }
        }
    }
}

/// A delete, called in this press.
fn delete_now(b: &Bridge, m: Cal, key: Key, scope: Option<&'static str>) {
    if m.busy.get_untracked() {
        return;
    }
    m.busy.set(true);
    m.ask.set(None);
    m.told.set(None);
    let mut args = key.args();
    if let Some(scope) = scope {
        args["scope"] = scope.into();
    }
    let call = b.call("gws_event_delete", args);
    let b = b.clone();
    run(m, async move {
        let answer = outcome(call.await);
        m.busy.set(false);
        match answer {
            Ok(v) => {
                if m.detail
                    .with_untracked(|d| d.as_ref().is_some_and(|d| d.row.key() == key))
                {
                    close_detail(m);
                }
                m.events
                    .update(|e| e.retain(|e| e.id != key.id || e.calendar != key.calendar));
                if v.get("undo") == Some(&Value::Bool(true)) {
                    m.undo.set(Some(key));
                } else {
                    m.undo.set(None);
                    tell(m, "Event deleted. Its guests were told.");
                }
                refresh(b, m).await;
            }
            Err((_, why)) => write_failed(m, why),
        }
    });
}

/// A reply, called in this press.
fn reply_now(b: &Bridge, m: Cal, key: Key, response: &'static str, scope: Option<&'static str>) {
    if m.busy.get_untracked() {
        return;
    }
    m.busy.set(true);
    m.ask.set(None);
    m.told.set(None);
    let mut args = key.args();
    args["response"] = response.into();
    if let Some(scope) = scope {
        args["scope"] = scope.into();
    }
    let call = b.call("gws_event_rsvp", args);
    let b = b.clone();
    run(m, async move {
        let answer = outcome(call.await);
        m.busy.set(false);
        match answer {
            Ok(_) => {
                tell(m, "Your reply was sent.");
                let open = m
                    .detail
                    .with_untracked(|d| d.as_ref().is_some_and(|d| d.row.key() == key));
                if open {
                    show_event(b.clone(), m, key).await;
                }
                refresh(b, m).await;
            }
            Err((_, why)) => write_failed(m, why),
        }
    });
}

/// Undo of the last delete, called in this press.
fn undo_now(b: &Bridge, m: Cal) {
    if m.undoing.get_untracked() {
        return;
    }
    let Some(key) = m.undo.get_untracked() else {
        return;
    };
    m.undoing.set(true);
    let call = b.call("gws_event_restore", key.args());
    let b = b.clone();
    run(m, async move {
        let answer = outcome(call.await);
        m.undoing.set(false);
        match answer {
            Ok(_) => {
                if m.undo.get_untracked().as_ref() == Some(&key) {
                    m.undo.set(None);
                }
                tell(m, "Event restored.");
                refresh(b, m).await;
            }
            // the bar stays, so it can be pressed again
            Err((_, why)) => write_failed(m, why),
        }
    });
}

/// Delete on the open event: one question, which for a series is which of them.
fn ask_delete(m: Cal) {
    let Some(d) = m.detail.get_untracked().filter(|d| d.can_edit) else {
        return;
    };
    let key = d.row.key();
    m.ask.set(Some(if d.in_series() {
        Ask::Scope(Change::Delete(key))
    } else {
        Ask::Delete(key, d.row.guests)
    }));
}

/// The next thing on the view, else its first: what `e` opens.
fn next_in_view(m: Cal) -> Option<Key> {
    let now = Local::now().naive_local();
    m.events.with_untracked(|rows| {
        rows.iter()
            .find(|e| e.when.starts(&Local) >= now)
            .or(rows.first())
            .map(Event::key)
    })
}

fn keys(b: Bridge, m: Cal) {
    // shortcuts are for the views, not for a form or a screen with no calendar
    let free = move || {
        !m.editor.open.get_untracked()
            && matches!(
                m.account.get_untracked(),
                Account::Covered | Account::Unknown
            )
    };
    for (key, span) in [
        ("d", Span::Day),
        ("w", Span::Week),
        ("m", Span::Month),
        ("a", Span::Agenda),
    ] {
        let b = b.clone();
        on_key(key, move |_| {
            if free() {
                show_view(&b, m, span, m.anchor.get_untracked());
            }
        });
    }
    for (key, forward) in [("j", true), ("k", false)] {
        let b = b.clone();
        on_key(key, move |_| {
            if free() {
                let span = m.span.get_untracked();
                show_view(
                    &b,
                    m,
                    span,
                    time::step(span, m.anchor.get_untracked(), forward),
                );
            }
        });
    }
    let (going, opening, undoing, saving) = (b.clone(), b.clone(), b.clone(), b);
    on_key("t", move |_| {
        if free() {
            show_view(&going, m, m.span.get_untracked(), today());
        }
    });
    on_key("c", move |_| {
        if free() {
            editor::new_event(m);
        }
    });
    on_key("e", move |_| {
        if free()
            && m.detail.with_untracked(Option::is_none)
            && let Some(key) = next_in_view(m)
        {
            open_event(&opening, m, key);
        }
    });
    on_key("delete", move |_| {
        if free() {
            ask_delete(m);
        }
    });
    on_key("z", move |_| {
        if free() {
            undo_now(&undoing, m);
        }
    });
    on_key("g", move |_| {
        if free() {
            m.going.set(true);
        }
    });
    on_key("/", move |_| {
        if free() {
            m.search.focus();
        }
    });
    on_key("mod+s", move |_| {
        if m.editor.open.get_untracked() {
            editor::press_save(&saving, m);
        }
    });
    on_key("escape", move |_| {
        if m.ask.with_untracked(Option::is_some) {
            m.ask.set(None);
        } else if m.going.get_untracked() {
            m.going.set(false);
        } else if free() && (m.opening.get_untracked() || m.detail.with_untracked(Option::is_some))
        {
            close_detail(m);
        }
    });
}

/// Previous, Next, Today, the date range, the views, search and go to date.
fn header(b: Bridge, m: Cal) -> impl IntoElement {
    let (back, on, here, go, submit) = (b.clone(), b.clone(), b.clone(), b.clone(), b.clone());
    let tab = move |span: Span, name: &'static str| {
        let b = b.clone();
        view()
            .app_class(move || {
                let on = m.span.get() == span && m.searching.with(Option::is_none);
                Some(
                    format!(
                        "px-3 py-1 rounded-md {}",
                        if on { crate::shell::CHOSEN } else { "" }
                    )
                    .into(),
                )
            })
            .label(name)
            .on_press(move || show_view(&b, m, span, m.anchor.get_untracked()))
            .child(text(name).app_class(Some(format!("text-[12px] {INK}").into())))
    };
    let title = move || match m.searching.get() {
        Some(_) => "Search results".to_owned(),
        None => time::range_title(m.span.get(), m.anchor.get()),
    };
    view()
        .app_class(Some(format!("px-2 py-1 gap-1 border-b {LINE}").into()))
        .child(
            view()
                .app_class(Some("flex-row flex-wrap items-center gap-1".into()))
                .child(press_text("Previous", "<", move || {
                    let span = m.span.get_untracked();
                    show_view(&back, m, span, time::step(span, m.anchor.get_untracked(), false));
                }))
                .child(press_text("Next", ">", move || {
                    let span = m.span.get_untracked();
                    show_view(&on, m, span, time::step(span, m.anchor.get_untracked(), true));
                }))
                .child(press_text("Today", "Today", move || {
                    show_view(&here, m, m.span.get_untracked(), today());
                }))
                .child(
                    text(title)
                        .number_of_lines(1)
                        .app_class(Some(format!("px-2 text-[14px] font-semibold {INK}").into())),
                )
                .child(tab(Span::Day, "Day"))
                .child(tab(Span::Week, "Week"))
                .child(tab(Span::Month, "Month"))
                .child(tab(Span::Agenda, "List")),
        )
        .child(
            view()
                .app_class(Some("flex-row flex-wrap items-center gap-2".into()))
                .child(
                    input()
                        .element_ref(&m.search)
                        .label("Search events")
                        .value(move || m.query.get())
                        .placeholder("Search events".to_owned())
                        .app_class(Some(
                            format!("w-[200px] rounded-lg border px-2.5 py-1 text-[13px] {BG} {LINE} {INK}")
                                .into(),
                        ))
                        .on_change(move |value: &str| m.query.set(value.to_owned()))
                        .on_submit(move |query: &str| {
                            close_detail(m);
                            if query.trim().is_empty() {
                                m.searching.set(None);
                                fetch(m, load(submit.clone(), m));
                            } else {
                                fetch(m, search(submit.clone(), m, query.trim().to_owned()));
                            }
                        }),
                )
                .child(dynamic(move || {
                    if !m.going.get() {
                        return view();
                    }
                    let go = go.clone();
                    view().child(
                        input()
                            .label("Go to date")
                            .auto_focus(true)
                            .value(move || m.going_to.get())
                            .placeholder("2026-11-06".to_owned())
                            .app_class(Some(
                                format!("w-[120px] rounded-lg border px-2.5 py-1 text-[13px] {BG} {LINE} {INK}")
                                    .into(),
                            ))
                            .on_change(move |value: &str| m.going_to.set(value.to_owned()))
                            .on_submit(move |date: &str| match time::parse_date(date) {
                                Some(day) => {
                                    m.going.set(false);
                                    m.going_to.set(String::new());
                                    show_view(&go, m, m.span.get_untracked(), day);
                                }
                                None => tell(m, "Type a date like 2026-11-06."),
                            }),
                    )
                })),
        )
}

/// The line under the header, when there is something to say.
fn notice(m: Cal) -> impl IntoElement {
    dynamic(move || {
        let line = m
            .problem
            .get()
            .or_else(|| m.saved.get().then(|| "Showing saved events".to_owned()));
        match line {
            Some(line) => view()
                .app_class(Some(format!("px-3 py-1 border-b {LINE}").into()))
                .child(text(line).app_class(Some(format!("text-[11px] {MUTE}").into()))),
            None => view(),
        }
    })
}

/// What the main area holds. A rebuild drops what it holds, so it follows this
/// and not the rows.
#[derive(Clone, Copy, PartialEq)]
enum Pane {
    Event,
    List,
    Days,
    Month,
}

fn main_area(b: Bridge, m: Cal) -> impl IntoElement {
    let pane = computed(move || {
        if m.opening.get() || m.detail.with(Option::is_some) {
            Pane::Event
        } else if m.searching.with(Option::is_some) {
            Pane::List
        } else {
            match m.span.get() {
                Span::Day | Span::Week => Pane::Days,
                Span::Month => Pane::Month,
                Span::Agenda => Pane::List,
            }
        }
    });
    let pick = b.clone();
    view()
        .app_class(Some(format!("flex-1 min-w-0 {PANEL}").into()))
        .child(header(b, m))
        .child(notice(m))
        .child(dynamic(move || {
            // the list says so itself, and an open event isn't an empty view
            if matches!(pane.get(), Pane::Days | Pane::Month) {
                view().child(grid::nothing(m))
            } else {
                view()
            }
        }))
        .child(dynamic(move || {
            let b = pick.clone();
            let body = view().app_class(Some("flex-1".into()));
            match pane.get() {
                Pane::Event => body.child(detail::panel(b, m)),
                Pane::List => body.child(grid::agenda(b, m)),
                Pane::Days => body.child(grid::days(b, m)),
                Pane::Month => body.child(grid::month(b, m)),
            }
        }))
}

/// What became of an action, for as long as it matters.
fn told_bar(m: Cal) -> impl IntoElement {
    dynamic(move || match m.told.get() {
        None => view(),
        Some(line) => view()
            .app_class(Some(
                format!("flex-row flex-wrap items-center gap-2 px-3 py-1 border-t {LINE} {RAIL}")
                    .into(),
            ))
            .child(text(line).app_class(Some(
                format!("flex-1 min-w-[160px] text-[12px] {INK}").into(),
            )))
            .child(press_text("Dismiss this", "Dismiss", move || {
                m.told.set(None)
            })),
    })
}

/// The question, on a line of its own: beside the presses it pushes them off a
/// narrow screen.
fn ask_bar(b: Bridge, m: Cal) -> impl IntoElement {
    dynamic(move || {
        let Some(ask) = m.ask.get() else {
            return view();
        };
        let (b, line) = (b.clone(), ask_line(&ask));
        let presses = view()
            .app_class(Some(
                "flex-row flex-wrap items-center justify-end gap-1".into(),
            ))
            .child(press_text("Cancel this", "Cancel", move || m.ask.set(None)));
        let presses = match ask {
            Ask::Delete(key, _) => {
                presses.child(press_send_button("Delete it", "Delete", move || {
                    delete_now(&b, m, key.clone(), None)
                }))
            }
            Ask::Scope(change) => {
                let (one, all, other) = (change.clone(), change, b.clone());
                presses
                    .child(press_text("This event", "This event", move || {
                        apply(&b, m, one.clone(), "this");
                    }))
                    .child(press_text("All events", "All events", move || {
                        apply(&other, m, all.clone(), "all");
                    }))
            }
        };
        view()
            .app_class(Some(
                format!("px-3 py-1 gap-1 border-t {LINE} {RAIL}").into(),
            ))
            .child(text(line).app_class(Some(format!("text-[12px] {INK}").into())))
            .child(presses)
    })
}

fn ask_line(ask: &Ask) -> String {
    match ask {
        Ask::Delete(_, 0) => "Delete this event?".to_owned(),
        Ask::Delete(_, 1) => "Delete this event? Its guest will be told.".to_owned(),
        Ask::Delete(_, n) => format!("Delete this event? Its {n} guests will be told."),
        Ask::Scope(Change::Save) => "Save this change to which events?".to_owned(),
        Ask::Scope(Change::Delete(_)) => "Delete which events?".to_owned(),
        Ask::Scope(Change::Reply(..)) => "Reply to which events?".to_owned(),
    }
}

/// The answer to a series' question, which is the press the change goes out on.
fn apply(b: &Bridge, m: Cal, change: Change, scope: &'static str) {
    match change {
        Change::Save => editor::save_now(b, m, Some(scope)),
        Change::Delete(key) => delete_now(b, m, key, Some(scope)),
        Change::Reply(key, response) => reply_now(b, m, key, response, Some(scope)),
    }
}

fn undo_bar(b: Bridge, m: Cal) -> impl IntoElement {
    dynamic(move || {
        if m.undo.with(Option::is_none) {
            return view();
        }
        let b = b.clone();
        view()
            .app_class(Some(
                format!("flex-row flex-wrap items-center gap-3 px-3 py-2 border-t {LINE} {RAIL}")
                    .into(),
            ))
            .child(
                text("Event deleted.").app_class(Some(format!("flex-1 text-[12px] {INK}").into())),
            )
            .child(press_text("Undo that", "Undo", move || undo_now(&b, m)))
    })
}

fn side_or_nothing(b: Bridge, m: Cal) -> impl IntoElement {
    let wide = computed(|| window().width.0 >= SIDE_FROM);
    dynamic(move || {
        if wide.get() {
            view().child(grid::side(b.clone(), m))
        } else {
            view()
        }
    })
}

fn screen(b: Bridge, m: Cal) -> impl IntoElement {
    let (side, area, form, asking, undoing) =
        (b.clone(), b.clone(), b.clone(), b.clone(), b.clone());
    let writing = computed(move || m.editor.open.get());
    view()
        .app_class(Some("flex-1".into()))
        .child(dynamic(move || {
            if writing.get() {
                return view()
                    .app_class(Some("flex-1".into()))
                    .child(editor::panel(form.clone(), m));
            }
            view()
                .app_class(Some("flex-1 flex-row".into()))
                .child(side_or_nothing(side.clone(), m))
                .child(main_area(area.clone(), m))
        }))
        .child(told_bar(m))
        .child(ask_bar(asking, m))
        .child(undo_bar(undoing, m))
        .child(link_bar(m.link))
}

/// The account first, as the empty shell did. Only then does anything load.
async fn begin(b: Bridge, m: Cal, event: Option<Key>, draft: Option<String>, new: bool) {
    match read(&b, "gws_status", json!({})).await {
        Ok(status) => m.account.set(account_of(&status, "Calendar")),
        Err(e) if e == "sign-in" => m.account.set(Account::SignedOut),
        Err(e) => m.account.set(Account::Failed(e)),
    }
    if !matches!(
        m.account.get_untracked(),
        Account::Covered | Account::Unknown
    ) {
        return;
    }
    fetch(m, load(b.clone(), m));
    fetch(m, watch(b.clone(), m));
    load_calendars(b.clone(), m).await;
    if let Some(key) = event {
        fetch(m, show_event(b.clone(), m, key));
    }
    if let Some(ticket) = draft {
        editor::open_draft(b, m, ticket).await;
    }
    // after the calendars: a new event starts in the person's own
    if new {
        editor::new_event(m);
    }
}

pub fn build(bridge: Bridge) -> Root {
    buwiz_app::root(bridge, move |b| {
        let props = b.props();
        let prop = |k: &str| props.get(k).and_then(Value::as_str).map(str::to_owned);
        if b.viewport_now().placement == Placement::Inline {
            return match prop("draft") {
                Some(ticket) => crate::shell::card_frame().child(cards::decide(b, ticket)),
                None => crate::shell::card_frame().child(cards::next(b)),
            };
        }
        let (jobs, waiting) = unbounded_channel::<Job>();
        let (reads, mut asked) = unbounded_channel::<Job>();
        let m = Cal {
            span: signal(
                prop("view")
                    .as_deref()
                    .and_then(Span::of)
                    .unwrap_or(Span::Week),
            ),
            anchor: signal(today()),
            events: signal(Vec::new()),
            calendars: signal(Vec::new()),
            calendars_read: signal(false),
            range_turn: Latest::new(),
            open_turn: Latest::new(),
            loading: signal(true),
            saved: signal(false),
            account: signal(Account::Loading),
            problem: signal(None),
            searching: signal(None),
            query: signal(String::new()),
            going: signal(false),
            going_to: signal(String::new()),
            detail: signal(None),
            opening: signal(false),
            editor: Editor::new(),
            ask: signal(None),
            busy: signal(false),
            told: signal(None),
            undo: signal(None),
            undoing: signal(false),
            link: signal(None),
            seq: signal(-1),
            now: signal(now_ms()),
            jobs: signal_with(jobs, SignalEquals::Never),
            reads: signal_with(reads, SignalEquals::Never),
            search: element_ref(),
        };
        keys(b.clone(), m);
        b.spawn(compose::work(waiting));
        let spawner = b.clone();
        b.spawn(async move {
            while let Some(job) = asked.recv().await {
                spawner.spawn(job);
            }
        });
        let event = props.get("event").and_then(|e| {
            Some(Key {
                calendar: e.get("calendar")?.as_str()?.to_owned(),
                id: e.get("id")?.as_str()?.to_owned(),
            })
        });
        let new = prop("view").as_deref() == Some("new");
        b.spawn(begin(b.clone(), m, event, prop("draft"), new));
        let view_of = b;
        view()
            .app_class(Some(format!("flex-1 {BG}").into()))
            .child(dynamic(move || {
                let account = m.account.get();
                if matches!(account, Account::Covered | Account::Unknown) {
                    return view()
                        .app_class(Some("flex-1".into()))
                        .child(screen(view_of.clone(), m));
                }
                view()
                    .app_class(Some("flex-1 items-center justify-center p-6".into()))
                    .child(
                        text(line_of(account, "Calendar"))
                            .app_class(Some(format!("text-[14px] {MUTE}").into())),
                    )
            }))
    })
}
