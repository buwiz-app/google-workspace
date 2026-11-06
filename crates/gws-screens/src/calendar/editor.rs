//! The event editor, in the views' place. The presses stay outside the scroll.

use buwiz_app::Bridge;
use chrono::Local;
use nsh_core::element::input;
use nsh_core::prelude::*;
use serde_json::{Value, json};

use super::event::{Detail, Event, Form, REPEATS, home, is_address, repeat_line};
use super::time;
use super::{
    Ask, Cal, Change, fetch, outcome, refresh, refused_read, run, show_event, tell, today, value,
    write_failed,
};
use crate::compose::{SUGGEST_AFTER, press_text};
use crate::shell::{CHOSEN, INK, LINE, Latest, MUTE, PANEL, RAIL, read, sleep};

const FIELD: &str = "flex-1 min-w-[108px] rounded-md border px-2 py-1 text-[13px]";

/// A field of the form.
type Place = fn(&mut Form) -> &mut String;

#[derive(Clone, Copy)]
pub(super) struct Editor {
    pub open: Signal<bool>,
    form: Signal<Form>,
    /// From Save's press to its answer: what goes is what was showing.
    saving: Signal<bool>,
    /// Someone else's version, after a save was refused for it.
    newer: Signal<Option<Detail>>,
    /// `(name, address)` suggested for what's being typed.
    people: Signal<Vec<(String, String)>>,
    asked: Latest,
}

impl Editor {
    pub fn new() -> Editor {
        Editor {
            open: signal(false),
            form: signal(Form::default()),
            saving: signal(false),
            newer: signal(None),
            people: signal(Vec::new()),
            asked: Latest::new(),
        }
    }

    fn fill(self, form: Form) {
        self.form.set(form);
        self.newer.set(None);
        self.asked.ask();
        self.people.set(Vec::new());
    }

    fn close(self, m: Cal) {
        self.open.set(false);
        self.fill(Form::default());
        m.ask.set(None);
    }
}

fn blank(m: Cal) -> Form {
    let (start, end) = time::new_slot(m.anchor.get_untracked(), Local::now().naive_local());
    m.calendars
        .with_untracked(|c| Form::new(home(c), start, end))
}

pub(super) fn new_event(m: Cal) {
    m.ask.set(None);
    m.editor.fill(blank(m));
    m.editor.open.set(true);
}

pub(super) fn edit(m: Cal, d: &Detail) {
    m.ask.set(None);
    m.editor.fill(Form::of_detail(d, &Local));
    m.editor.open.set(true);
}

/// A draft a card named, opened here to go on with.
pub(super) async fn open_draft(b: Bridge, m: Cal, ticket: String) {
    match read(&b, "gws_event_draft_read", json!({"ticket": ticket})).await {
        Ok(v) => match refused_read(&v) {
            Some(why) => tell(m, why),
            None => {
                m.editor.fill(Form::of_draft(&v, blank(m), &Local));
                m.editor.open.set(true);
            }
        },
        Err(why) => tell(m, why),
    }
}

fn change(m: Cal, change: impl FnOnce(&mut Form)) {
    if m.editor.saving.get_untracked() {
        return;
    }
    m.editor.form.update(change);
    // what the series question was about may not be what's there now
    if m.ask
        .with_untracked(|a| *a == Some(Ask::Scope(Change::Save)))
    {
        m.ask.set(None);
    }
}

/// Save's press. Nothing goes while the form can't, or while a series hasn't
/// been asked: that press is the one the change goes out on.
pub(super) fn press_save(b: &Bridge, m: Cal) {
    let e = m.editor;
    if e.saving.get_untracked() || e.newer.with_untracked(Option::is_some) {
        return;
    }
    let form = e.form.get_untracked();
    if let Err(why) = form.save_args() {
        tell(m, why);
        return;
    }
    if form.in_series {
        m.ask.set(Some(Ask::Scope(Change::Save)));
    } else {
        save_now(b, m, None);
    }
}

/// The save, called in this press.
pub(super) fn save_now(b: &Bridge, m: Cal, scope: Option<&'static str>) {
    let e = m.editor;
    if e.saving.get_untracked() {
        return;
    }
    let form = e.form.get_untracked();
    let mut args = match form.save_args() {
        Ok(args) => args,
        Err(why) => return tell(m, why),
    };
    if let Some(scope) = scope {
        args["scope"] = scope.into();
    }
    e.saving.set(true);
    m.ask.set(None);
    m.told.set(None);
    let call = b.call("gws_event_save", args);
    let b = b.clone();
    run(m, async move {
        let answer = outcome(call.await);
        e.saving.set(false);
        match answer {
            Ok(v) => saved(b, m, &form, &v).await,
            Err((code, _)) if code == "changed" => show_newer(b, m, &form).await,
            Err((_, why)) => write_failed(m, why),
        }
    });
}

async fn saved(b: Bridge, m: Cal, form: &Form, v: &Value) {
    m.editor.close(m);
    tell(m, form.saved_line());
    let row = v.get("event").and_then(Event::of);
    if let Some(row) = &row {
        // the person should see what they just saved
        let day = row.when.starts(&Local).date();
        let (first, end) = time::range(m.span.get_untracked(), m.anchor.get_untracked());
        if day < first || day >= end {
            m.anchor.set(day);
        }
    }
    let open = row.filter(|_| m.detail.with_untracked(Option::is_some));
    if let Some(row) = open {
        fetch(m, show_event(b.clone(), m, row.key()));
    }
    refresh(b, m).await;
}

/// Someone else's change won: nothing more is written, and the person chooses.
async fn show_newer(b: Bridge, m: Cal, form: &Form) {
    let Some(key) = &form.event else {
        return;
    };
    let lost = "Someone else changed this event, so yours wasn't saved.";
    let read = read(&b, "gws_event_read", key.args()).await;
    match read.as_ref().ok().and_then(Detail::of) {
        Some(newer) => {
            m.editor.newer.set(Some(newer));
            tell(m, format!("{lost} The newer one is shown here."));
        }
        // the etag is still the old one, so a second Save can't overwrite it
        None => tell(m, format!("{lost} The newer one couldn't be read.")),
    }
}

/// Who the word being typed could be, once typing rests for a moment.
async fn suggest(b: Bridge, m: Cal, word: String) {
    let e = m.editor;
    let turn = e.asked.ask();
    sleep(SUGGEST_AFTER).await;
    if !e.asked.counts(turn) {
        return;
    }
    if word.len() < 2 {
        e.people.set(Vec::new());
        return;
    }
    let found = b.call("gws_cal_people", json!({"query": word})).await;
    if !e.asked.counts(turn) {
        return;
    }
    let text = |p: &Value, k: &str| {
        p.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let people = found.ok().and_then(|v| v.get("people").cloned());
    e.people.set(
        people
            .as_ref()
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|p| (text(p, "name"), text(p, "address")))
            .collect(),
    );
}

fn add_guest(form: &mut Form, address: &str) {
    if !form.guests.iter().any(|g| g.eq_ignore_ascii_case(address)) {
        form.guests.push(address.to_owned());
    }
}

/// Takes in what's typed: a comma ends an address, and what's left is the word
/// still being typed. A word that isn't an address stays there to be fixed.
fn typed_guests(form: &mut Form, value: &str) {
    let mut words: Vec<&str> = value.split([',', ';']).map(str::trim).collect();
    let rest = words.pop().unwrap_or_default().to_owned();
    let mut kept = Vec::new();
    for word in words.into_iter().filter(|w| !w.is_empty()) {
        if is_address(word) {
            add_guest(form, word);
        } else {
            kept.push(word.to_owned());
        }
    }
    kept.push(rest);
    form.typing = kept.join(", ");
}

fn field(m: Cal, name: &'static str, at: Place) -> Input {
    input()
        .label(name)
        .value(move || {
            let mut form = m.editor.form.get();
            at(&mut form).clone()
        })
        .placeholder(name.to_owned())
        .readonly(move || m.editor.saving.get())
        .app_class(Some(format!("{FIELD} {LINE} {INK}").into()))
        // setting a field's value answers with a change of its own
        .on_change(move |value: &str| {
            let mut form = m.editor.form.get_untracked();
            if at(&mut form) != value {
                let value = value.to_owned();
                self::change(m, |f| *at(f) = value);
            }
        })
}

fn row(name: &'static str, body: impl IntoElement) -> View {
    view()
        .app_class(Some(
            format!("flex-row items-center gap-2 px-3 py-1 border-b {LINE}").into(),
        ))
        .child(text(name).app_class(Some(format!("w-[64px] text-[12px] {MUTE}").into())))
        .child(
            view()
                .app_class(Some("flex-1 flex-row flex-wrap items-center gap-1".into()))
                .child(body),
        )
}

fn pill(on: bool) -> Option<SharedString> {
    Some(format!("px-2 py-1 rounded-md {}", if on { CHOSEN } else { "" }).into())
}

/// The date and times: all day has dates only, and shows the last day it covers.
fn when_fields(m: Cal) -> impl IntoElement {
    let all_day = computed(move || m.editor.form.with(|f| f.all_day));
    dynamic(move || {
        let on = all_day.get();
        let line = view()
            .app_class(Some("flex-row flex-wrap items-center gap-1".into()))
            .child(field(m, "Start date", |f| &mut f.start_date));
        let line = if on {
            line
        } else {
            line.child(field(m, "Start time", |f| &mut f.start_time))
        };
        let line = line.child(field(m, "End date", |f| &mut f.end_date));
        let line = if on {
            line
        } else {
            line.child(field(m, "End time", |f| &mut f.end_time))
        };
        view().child(row("When", line)).child(row(
            "All day",
            view()
                .app_class(pill(on))
                .label("All day")
                .accessibility_value(value(if on { "on" } else { "off" }))
                .on_press(move || self::change(m, |f| f.all_day = !f.all_day))
                .child(
                    text(if on { "All day: on" } else { "All day: off" })
                        .app_class(Some(format!("text-[12px] {INK}").into())),
                ),
        ))
    })
}

/// A change to one event of a series can't change its rule, so that's shown and
/// not offered.
fn repeat_field(m: Cal) -> impl IntoElement {
    let fixed = computed(move || m.editor.form.with(|f| f.in_series));
    let rule = computed(move || m.editor.form.with(|f| f.repeat.clone()));
    dynamic(move || {
        if fixed.get() {
            return row(
                "Repeats",
                text(repeat_line(&rule.get()))
                    .number_of_lines(1)
                    .app_class(Some(format!("text-[12px] {INK}").into())),
            );
        }
        let now = rule.get();
        let mut choices = REPEATS.iter().fold(
            view().app_class(Some("flex-row flex-wrap gap-1".into())),
            |list, (name, rule)| {
                list.child(
                    view()
                        .app_class(pill(now == *rule))
                        .label(*name)
                        .on_press(move || self::change(m, |f| f.repeat = (*rule).to_owned()))
                        .child(text(*name).app_class(Some(format!("text-[12px] {INK}").into()))),
                )
            },
        );
        if !REPEATS.iter().any(|(_, r)| *r == now) {
            choices = choices.child(
                text(now.clone())
                    .number_of_lines(1)
                    .app_class(Some(format!("px-2 text-[12px] {INK}").into())),
            );
        }
        row("Repeats", choices)
    })
}

/// Guests, as chips that wrap, then the field for the next one.
fn guest_field(b: Bridge, m: Cal) -> impl IntoElement {
    let chips = computed(move || m.editor.form.with(|f| f.guests.clone()));
    let submit = move |_: &str| {
        let word = m.editor.form.with_untracked(|f| f.typing.trim().to_owned());
        if is_address(&word) {
            self::change(m, |f| {
                add_guest(f, &word);
                f.typing.clear();
            });
            m.editor.asked.ask();
            m.editor.people.set(Vec::new());
        } else if !word.is_empty() {
            tell(m, format!("Check this guest's address: {word}"));
        }
    };
    view()
        .child(dynamic(move || {
            let list = chips.get();
            if list.is_empty() {
                return view();
            }
            list.into_iter().fold(
                view().app_class(Some(format!("flex-row flex-wrap gap-1 px-3 py-1 border-b {LINE}").into())),
                |all, address| {
                    let gone = address.clone();
                    all.child(
                        view()
                            .app_class(Some(format!("flex-row items-center gap-1 px-2 py-[2px] rounded-md max-w-full {RAIL}").into()))
                            .label("Remove this guest")
                            .accessibility_value(value(address.clone()))
                            .on_press(move || {
                                self::change(m, |f| f.guests.retain(|g| *g != gone));
                            })
                            .child(
                                text(address)
                                    .number_of_lines(1)
                                    .app_class(Some(format!("text-[12px] {INK}").into())),
                            )
                            .child(text("x").app_class(Some(format!("text-[11px] {MUTE}").into()))),
                    )
                },
            )
        }))
        .child(row(
            "Guests",
            input()
                // the @ keeps the AI out: what it staged here would go at the person's press
                .label("Guest's address, like name@company.com")
                .value(move || m.editor.form.with(|f| f.typing.clone()))
                .placeholder("Add a guest".to_owned())
                .readonly(move || m.editor.saving.get())
                .app_class(Some(format!("{FIELD} {LINE} {INK}").into()))
                .on_change(move |value: &str| {
                    if m.editor.form.with_untracked(|f| f.typing == value) {
                        return;
                    }
                    self::change(m, |f| typed_guests(f, value));
                    let word = m.editor.form.with_untracked(|f| f.typing.trim().to_owned());
                    fetch(m, suggest(b.clone(), m, word));
                })
                .on_submit(submit),
        ))
        .child(dynamic(move || {
            m.editor.people.get().into_iter().fold(
                view(),
                move |column, (name, address)| {
                    let shown = if name.is_empty() || name == address {
                        address.clone()
                    } else {
                        format!("{name} <{address}>")
                    };
                    let chosen = address.clone();
                    column.child(
                        view()
                            .app_class(Some(format!("px-3 py-1 border-b {LINE} {RAIL}").into()))
                            .label("Use this address")
                            .accessibility_value(value(address))
                            .on_press(move || {
                                self::change(m, |f| {
                                    add_guest(f, &chosen);
                                    f.typing.clear();
                                });
                                m.editor.asked.ask();
                                m.editor.people.set(Vec::new());
                            })
                            .child(text(shown).app_class(Some(format!("text-[12px] {INK}").into()))),
                    )
                },
            )
        }))
}

fn meet_field(m: Cal) -> impl IntoElement {
    let state = computed(move || m.editor.form.with(|f| (f.has_meet, f.meet)));
    dynamic(move || {
        let (has, adding) = state.get();
        let body = if has {
            view().child(
                text("This event has a Meet link")
                    .app_class(Some(format!("text-[12px] {INK}").into())),
            )
        } else {
            view()
                .app_class(pill(adding))
                .label("Add a Meet link")
                .accessibility_value(value(if adding { "on" } else { "off" }))
                .on_press(move || self::change(m, |f| f.meet = !f.meet))
                .child(
                    text(if adding {
                        "A Meet link will be added"
                    } else {
                        "+ Add a Meet link"
                    })
                    .app_class(Some(format!("text-[12px] {INK}").into())),
                )
        };
        row("Video", body)
    })
}

/// An event stays on its calendar: the tools have no move.
fn calendar_field(m: Cal) -> impl IntoElement {
    let state = computed(move || {
        m.editor
            .form
            .with(|f| (f.event.is_some(), f.calendar.clone()))
    });
    dynamic(move || {
        let (existing, chosen) = state.get();
        let calendars = m.calendars.get();
        if existing {
            let name = calendars
                .iter()
                .find(|c| c.id == chosen)
                .map(|c| c.name.clone());
            return row(
                "Calendar",
                text(name.unwrap_or_default())
                    .number_of_lines(1)
                    .app_class(Some(format!("text-[12px] {INK}").into())),
            );
        }
        let list = calendars.iter().filter(|c| c.writable).fold(
            view().app_class(Some("flex-row flex-wrap gap-1".into())),
            |list, c| {
                let (id, zone) = (c.id.clone(), c.time_zone.clone());
                list.child(
                    view()
                        .app_class(pill(c.id == chosen))
                        .label("Use this calendar")
                        .accessibility_value(value(c.name.clone()))
                        .on_press(move || {
                            self::change(m, |f| {
                                f.calendar = id.clone();
                                if f.time_zone.is_empty() {
                                    f.time_zone = zone.clone();
                                }
                            });
                        })
                        .child(
                            text(c.name.clone())
                                .number_of_lines(1)
                                .app_class(Some(format!("text-[12px] {INK}").into())),
                        ),
                )
            },
        );
        row("Calendar", list)
    })
}

/// The version that won, with what it says.
fn newer_view(m: Cal) -> impl IntoElement {
    dynamic(move || {
        let Some(d) = m.editor.newer.get() else {
            return view();
        };
        let mut block = view()
            .app_class(Some(
                format!("px-3 py-2 gap-1 border-b {LINE} {RAIL}").into(),
            ))
            .child(text("The newer one").app_class(Some(format!("text-[11px] {MUTE}").into())))
            .child(
                text(d.row.name())
                    .app_class(Some(format!("text-[13px] font-semibold {INK}").into())),
            )
            .child(
                text(time::when_line(&d.row.when, &Local))
                    .app_class(Some(format!("text-[12px] {INK}").into())),
            );
        if !d.row.place.is_empty() {
            block = block.child(
                text(d.row.place.clone()).app_class(Some(format!("text-[12px] {INK}").into())),
            );
        }
        if !d.attendees.is_empty() {
            let who: Vec<&str> = d.attendees.iter().map(|a| a.email.as_str()).collect();
            block = block
                .child(text(who.join(", ")).app_class(Some(format!("text-[12px] {INK}").into())));
        }
        block
    })
}

/// The presses. Save reads what it will do, and goes once the person has chosen
/// between their version and a newer one.
fn presses(b: Bridge, m: Cal) -> impl IntoElement {
    let e = m.editor;
    let deciding = computed(move || e.newer.with(Option::is_some));
    dynamic(move || {
        let bar = view().app_class(Some(
            format!(
                "flex-row flex-wrap items-center justify-end gap-1 px-2 py-1 border-t {LINE} {RAIL}"
            )
            .into(),
        ));
        let bar = bar.child(press_text("Cancel", "Cancel", move || e.close(m)));
        if deciding.get() {
            return bar
                .child(press_text(
                    "Use the newer one",
                    "Use the newer one",
                    move || {
                        if let Some(d) = e.newer.get_untracked() {
                            e.fill(Form::of_detail(&d, &Local));
                            m.told.set(None);
                        }
                    },
                ))
                .child(press_text(
                    "Keep my changes",
                    "Keep my changes",
                    move || {
                        if let Some(d) = e.newer.get_untracked() {
                            e.form.update(|f| f.etag = d.etag.clone());
                            e.newer.set(None);
                            m.told.set(None);
                        }
                    },
                ));
        }
        let b = b.clone();
        bar.child(
            view()
                .app_class(Some("px-4 py-1 rounded-md bg-[#2563eb]".into()))
                .label("Save this event")
                .accessibility_value(move || value(e.form.with(Form::save_line)))
                .on_press(move || press_save(&b, m))
                .child(
                    text(move || e.form.with(Form::save_line))
                        .app_class(Some("text-[12px] font-semibold text-white".into())),
                ),
        )
    })
}

pub(super) fn panel(b: Bridge, m: Cal) -> impl IntoElement {
    let guests = b.clone();
    view()
        .app_class(Some(format!("flex-1 {PANEL}").into()))
        .child(newer_view(m))
        .child(
            scroll().app_class(Some("flex-1".into())).child(
                view()
                    .child(
                        view()
                            .app_class(Some(format!("px-3 py-2 border-b {LINE}").into()))
                            .child(field(m, "Title", |f| &mut f.title)),
                    )
                    .child(when_fields(m))
                    .child(row("Zone", field(m, "Time zone", |f| &mut f.time_zone)))
                    .child(repeat_field(m))
                    .child(guest_field(guests, m))
                    .child(row("Place", field(m, "Place", |f| &mut f.place)))
                    .child(meet_field(m))
                    .child(
                        view()
                            .app_class(Some(format!("px-3 py-2 border-b {LINE}").into()))
                            .child(
                                input()
                                    .label("Notes")
                                    .multiline(true)
                                    .readonly(move || m.editor.saving.get())
                                    .value(move || m.editor.form.with(|f| f.notes.clone()))
                                    .placeholder("Notes".to_owned())
                                    .app_class(Some(format!("h-[96px] text-[13px] {INK}").into()))
                                    .on_change(move |value: &str| {
                                        if m.editor.form.with_untracked(|f| f.notes != value) {
                                            let value = value.to_owned();
                                            self::change(m, |f| f.notes = value);
                                        }
                                    }),
                            ),
                    )
                    .child(calendar_field(m)),
            ),
        )
        .child(presses(b, m))
}

/// Today, in the person's zone, for a form with nothing else to go on.
pub(super) fn blank_for_card(calendars: &[super::event::Calendar]) -> Form {
    let (start, end) = time::new_slot(today(), Local::now().naive_local());
    Form::new(home(calendars), start, end)
}
