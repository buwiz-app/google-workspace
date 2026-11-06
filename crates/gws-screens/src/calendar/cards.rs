//! Calendar as cards in a chat, each read live so no event sits in the chat's
//! record: the next event and the two after it, and an event the AI drafted.

use buwiz_app::Bridge;
use chrono::{Local, TimeZone};
use chrono_tz::Tz;
use nsh_core::prelude::*;
use serde_json::{Value, json};
use tokio::sync::mpsc::unbounded_channel;

use super::editor::blank_for_card;
use super::event::{Calendar, Event, Form, calendars_in, repeat_line, rows_in, saved_line};
use super::time::{self, When};
use super::{outcome, refused_read, value};
use crate::compose::{self, press_send_button, press_text, review_line};
use crate::shell::{INK, Job, LINE, MUTE, PANEL, RAIL, read};

/// How many invites a card sends without asking once more, as Mail's does.
const FEW: usize = 5;

/// Reports the card's height, so the chat can size it.
fn fitted(b: Bridge) -> View {
    view().app_class(Some(PANEL.into())).on_layout(
        move |bounds: &nsh_core::geometry::Bounds<nsh_core::geometry::Points>| {
            let _ = b.fit(bounds.size.height.0.ceil());
        },
    )
}

fn plain(line: String) -> View {
    view()
        .app_class(Some("p-3".into()))
        .child(text(line).app_class(Some(format!("text-[12px] {MUTE}").into())))
}

fn trouble(why: String) -> String {
    if why == "sign-in" {
        "Sign in to Google first.".to_owned()
    } else {
        why
    }
}

fn next_row(b: &Bridge, e: &Event, first: bool) -> View {
    let (b, key) = (b.clone(), e.key());
    let name_size = if first {
        "text-[14px] font-semibold"
    } else {
        "text-[13px]"
    };
    let row = view()
        .app_class(Some(format!("px-3 py-1.5 border-b {LINE}").into()))
        .label("Open this event")
        .accessibility_value(value(e.name()))
        .on_press(move || {
            let open = b.open("calendar", json!({"event": key.args()}));
            b.spawn(async move { drop(open.await) });
        })
        .child(
            text(e.name())
                .number_of_lines(1)
                .app_class(Some(format!("{name_size} {INK}").into())),
        )
        .child(
            text(time::when_line(&e.when, &Local))
                .number_of_lines(1)
                .app_class(Some(format!("text-[11px] {MUTE}").into())),
        );
    if e.place.is_empty() {
        row
    } else {
        row.child(
            text(e.place.clone())
                .number_of_lines(1)
                .app_class(Some(format!("text-[11px] {MUTE}").into())),
        )
    }
}

/// The next event and the two after it, showing with no click.
pub(super) fn next(b: Bridge) -> View {
    #[derive(Clone, PartialEq)]
    enum State {
        Loading,
        Rows(Vec<Event>),
        Line(String),
    }
    let state = signal(State::Loading);
    let reader = b.clone();
    b.spawn(async move {
        match read(&reader, "gws_cal_next", json!({})).await {
            Ok(v) => {
                let first = v.get("event").and_then(Event::of);
                let rows: Vec<Event> = first.into_iter().chain(rows_in(&v, "after")).collect();
                state.set(State::Rows(rows));
            }
            Err(e) => state.set(State::Line(trouble(e))),
        }
        reader.ready();
    });
    let press = b.clone();
    fitted(b).child(dynamic(move || match state.get() {
        State::Loading => plain("Looking…".to_owned()),
        State::Line(line) => plain(line),
        State::Rows(rows) if rows.is_empty() => plain("Nothing coming up.".to_owned()),
        State::Rows(rows) => rows.iter().enumerate().fold(view(), |list, (at, e)| {
            list.child(next_row(&press, e, at == 0))
        }),
    }))
}

/// What Save would write, as moments read back on the clock of the zone beside
/// them on the card, or the fields as typed if they can't go yet. `local` is
/// for a form naming no zone, which Save reads on this Mac's clock.
fn when_of<Z: TimeZone>(form: &Form, args: &Value, local: &Z) -> String {
    let text = |k: &str| args.get(k).and_then(Value::as_str).unwrap_or_default();
    match When::of(text("start"), text("end"), form.all_day) {
        Some(when) => match form.time_zone.trim().parse::<Tz>() {
            Ok(zone) => time::when_line(&when, &zone),
            Err(_) => time::when_line(&when, local),
        },
        None => format!("{} {}", form.start_date, form.start_time),
    }
}

fn review(form: &Form, calendars: &[Calendar]) -> View {
    let args = form.save_args().unwrap_or(Value::Null);
    let calendar = calendars
        .iter()
        .find(|c| c.id == form.calendar)
        .map_or_else(|| form.calendar.clone(), |c| c.name.clone());
    let lines = [
        ("Title", form.title.clone()),
        ("When", when_of(form, &args, &Local)),
        ("Zone", form.time_zone.clone()),
        ("Repeats", repeat_line(&form.repeat)),
        ("Guests", form.invites().join(", ")),
        ("Place", form.place.clone()),
        ("Calendar", calendar),
    ];
    let body = lines.into_iter().filter(|(_, line)| !line.is_empty()).fold(
        view().app_class(Some("py-2".into())),
        |card, (name, line)| card.child(review_line(name, line)),
    );
    let body = if form.meet {
        body.child(review_line("Video", "A Meet link will be added".to_owned()))
    } else {
        body
    };
    let body = if form.notes.is_empty() {
        body
    } else {
        body.child(
            view()
                .app_class(Some(format!("mt-1 px-3 py-2 border-t {LINE}").into()))
                .child(
                    text(form.notes.clone()).app_class(Some(format!("text-[12px] {INK}").into())),
                ),
        )
    };
    // its own scroll, so a long list of guests can't take the presses off the card
    view().child(scroll().app_class(Some("max-h-[260px]".into())).child(body))
}

/// Where a decide card has got to.
#[derive(Clone, PartialEq)]
enum Stage {
    Reading,
    Showing,
    Kept,
    Saved(String),
}

/// The press that names its invites, for the row and for a second press.
fn save_press(label: &'static str, line: String, press: impl Fn() + 'static) -> View {
    view()
        .app_class(Some("px-4 py-1 rounded-md bg-[#2563eb]".into()))
        .label(label)
        .accessibility_value(value(line.clone()))
        .on_press(press)
        .child(text(line).app_class(Some("text-[12px] font-semibold text-white".into())))
}

/// An event the AI drafted: everything Save would write, and the person's press.
/// The model filled it and can press nothing here.
pub(super) fn decide(b: Bridge, ticket: String) -> View {
    let form = signal(None::<Form>);
    let calendars = signal(Vec::<Calendar>::new());
    let stage = signal(Stage::Reading);
    let said = signal(None::<String>);
    let asking = signal(false);
    let saving = signal(false);
    let (jobs, waiting) = unbounded_channel::<Job>();
    let jobs = signal_with(jobs, SignalEquals::Never);
    b.spawn(compose::work(waiting));
    let (loading, name) = (b.clone(), ticket.clone());
    b.spawn(async move {
        let list = read(&loading, "gws_cal_list", json!({})).await;
        let list = list.map(|v| calendars_in(&v)).unwrap_or_default();
        match read(&loading, "gws_event_draft_read", json!({"ticket": name})).await {
            Ok(v) => match refused_read(&v) {
                Some(why) => said.set(Some(why)),
                None => {
                    form.set(Some(Form::of_draft(&v, blank_for_card(&list), &Local)));
                    stage.set(Stage::Showing);
                }
            },
            Err(why) => said.set(Some(trouble(why))),
        }
        calendars.set(list);
        loading.ready();
    });
    // the call is made in the press, as on the screen: see the top of `calendar.rs`
    let save = {
        let b = b.clone();
        move || {
            let Some(shown) = form.get_untracked() else {
                return;
            };
            if saving.get_untracked() {
                return;
            }
            let args = match shown.save_args() {
                Ok(args) => args,
                Err(why) => return said.set(Some(why)),
            };
            saving.set(true);
            asking.set(false);
            said.set(None);
            let call = b.call("gws_event_save", args);
            let b = b.clone();
            jobs.with_untracked(|jobs| {
                drop(jobs.send(Box::pin(async move {
                    let answer = outcome(call.await);
                    saving.set(false);
                    match answer {
                        Ok(_) => {
                            let line = saved_line(shown.invites().len());
                            stage.set(Stage::Saved(line.clone()));
                            let _ = b.settle(&line).await;
                        }
                        Err((_, why)) => said.set(Some(why)),
                    }
                })))
            });
        }
    };
    let opening = b.clone();
    fitted(b).child(dynamic(move || {
        let (shown, list) = (form.get(), calendars.get());
        match stage.get() {
            Stage::Reading => plain(said.get().unwrap_or_else(|| "Reading the draft…".to_owned())),
            Stage::Kept => plain("Kept. Nothing was saved.".to_owned()),
            Stage::Saved(line) => plain(line),
            Stage::Showing => {
                let Some(shown) = shown else {
                    return view();
                };
                let n = shown.invites().len();
                let mut card = view().child(review(&shown, &list));
                if let Some(line) = said.get() {
                    card = card.child(
                        view()
                            .app_class(Some(format!("px-3 py-1 border-t {LINE}").into()))
                            .child(text(line).app_class(Some(format!("text-[12px] {INK}").into()))),
                    );
                }
                // on a line of its own: beside the buttons it pushes them off the card
                if asking.get() {
                    card = card.child(
                        view()
                            .app_class(Some(format!("px-3 py-1 border-t {LINE}").into()))
                            .child(
                                text(format!("Invite {n} people?"))
                                    .app_class(Some(format!("text-[12px] {INK}").into())),
                            ),
                    );
                }
                let (name, open) = (ticket.clone(), opening.clone());
                let bar = view()
                    .app_class(Some(
                        format!(
                            "flex-row flex-wrap items-center justify-end gap-1 px-2 py-1 border-t {LINE} {RAIL}"
                        )
                        .into(),
                    ))
                    .child(press_text("Keep draft", "Keep", move || stage.set(Stage::Kept)))
                    .child(press_text("Edit this event", "Edit", move || {
                        let go = open.open("calendar", json!({"draft": name}));
                        open.spawn(async move { drop(go.await) });
                    }));
                let save = save.clone();
                let bar = if asking.get() {
                    bar.child(press_text("Not yet", "Not yet", move || asking.set(false)))
                        .child(press_send_button("Invite them", "Invite them", save))
                } else {
                    bar.child(save_press(
                        "Save this event",
                        shown.save_line(),
                        move || {
                            if n > FEW {
                                asking.set(true);
                            } else {
                                save();
                            }
                        },
                    ))
                };
                card.child(bar)
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use chrono_tz::America::New_York;
    use chrono_tz::Asia::Manila;

    use super::*;

    fn nine_oclock(zone: &str) -> Form {
        Form {
            calendar: "mine".to_owned(),
            start_date: "2026-11-10".to_owned(),
            end_date: "2026-11-10".to_owned(),
            start_time: "09:00".to_owned(),
            end_time: "10:00".to_owned(),
            time_zone: zone.to_owned(),
            ..Form::default()
        }
    }

    #[test]
    fn the_card_reads_on_the_clock_of_the_zone_it_names_not_the_macs() {
        let form = nine_oclock("America/New_York");
        let args = form.save_args().unwrap();
        assert_eq!(args["start"], "2026-11-10T09:00:00-05:00");
        assert_eq!(
            when_of(&form, &args, &Manila),
            "Tue Nov 10, 09:00 - 10:00",
            "this Mac is in Manila, where that's 22:00"
        );
    }

    #[test]
    fn a_form_naming_no_zone_reads_on_the_macs_clock_as_save_does() {
        let form = nine_oclock("");
        let args = form.args(&New_York).unwrap();
        assert_eq!(
            when_of(&form, &args, &Manila),
            "Tue Nov 10, 22:00 - 23:00",
            "the moment, on the clock it's shown on"
        );
    }

    #[test]
    fn a_form_that_cant_go_shows_what_was_typed() {
        let form = nine_oclock("Not/AZone");
        assert_eq!(when_of(&form, &Value::Null, &Manila), "2026-11-10 09:00");
    }
}
