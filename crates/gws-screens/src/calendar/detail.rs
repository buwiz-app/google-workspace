//! One event, open in the main area. The presses stay outside the scroll, so a
//! long list of guests can't take them away.

use buwiz_app::Bridge;
use chrono::Local;
use nsh_core::prelude::*;

use super::event::Detail;
use super::time;
use super::{Ask, Cal, Change, ask_delete, close_detail, editor, reply_now};
use crate::compose::press_text;
use crate::shell::{CHOSEN, DIM, INK, LINE, MUTE};

fn line(title: &'static str, body: String) -> View {
    view()
        .app_class(Some("flex-row gap-2 px-4 py-1".into()))
        .child(text(title).app_class(Some(format!("w-[64px] text-[12px] {MUTE}").into())))
        .child(text(body).app_class(Some(format!("flex-1 text-[13px] {INK}").into())))
}

fn guests(d: &Detail) -> View {
    d.attendees.iter().fold(
        view().app_class(Some("px-4 py-1 gap-1".into())).child(
            text(match d.attendees.len() {
                1 => "1 guest".to_owned(),
                n => format!("{n} guests"),
            })
            .app_class(Some(format!("text-[12px] {MUTE}").into())),
        ),
        |list, g| {
            let who = if g.name.is_empty() {
                g.email.clone()
            } else {
                format!("{} <{}>", g.name, g.email)
            };
            let reply = if g.organizer {
                format!("{}, organizer", g.reply())
            } else {
                g.reply().to_owned()
            };
            list.child(
                view()
                    .app_class(Some("flex-row flex-wrap gap-2".into()))
                    .child(text(who).app_class(Some(
                        format!("flex-1 min-w-[120px] text-[12px] {INK}").into(),
                    )))
                    .child(text(reply).app_class(Some(format!("text-[11px] {DIM}").into()))),
            )
        },
    )
}

/// Yes, Maybe or No. A series asks which first, and that's the press it goes on.
fn replies(b: &Bridge, m: Cal, d: &Detail) -> View {
    let press = |name: &'static str, response: &'static str| {
        let (b, key, series) = (b.clone(), d.row.key(), d.in_series());
        let on = if d.row.response == response {
            CHOSEN
        } else {
            ""
        };
        view()
            .app_class(Some(format!("px-3 py-1 rounded-md {on}").into()))
            .label(name)
            .on_press(move || {
                if series {
                    m.ask
                        .set(Some(Ask::Scope(Change::Reply(key.clone(), response))));
                } else {
                    reply_now(&b, m, key.clone(), response, None);
                }
            })
            .child(text(name).app_class(Some(format!("text-[12px] {INK}").into())))
    };
    view()
        .app_class(Some(
            format!("flex-row flex-wrap items-center gap-1 px-2 py-1 border-t {LINE}").into(),
        ))
        .child(
            view()
                .app_class(Some("px-2".into()))
                .child(text("Going?").app_class(Some(format!("text-[12px] {MUTE}").into()))),
        )
        .child(press("Yes", "accepted"))
        .child(press("Maybe", "tentative"))
        .child(press("No", "declined"))
}

pub(super) fn panel(b: Bridge, m: Cal) -> impl IntoElement {
    dynamic(move || {
        let Some(d) = m.detail.get() else {
            // a slow answer mustn't leave the person with no way back
            return view()
                .app_class(Some("flex-1".into()))
                .child(
                    view()
                        .app_class(Some("flex-1 items-center justify-center".into()))
                        .child(
                            text("Opening…").app_class(Some(format!("text-[13px] {MUTE}").into())),
                        ),
                )
                .child(
                    view()
                        .app_class(Some(format!("flex-row px-2 py-1 border-t {LINE}").into()))
                        .child(press_text("Back to the calendar", "Back", move || {
                            close_detail(m)
                        })),
                );
        };
        let calendar = m.calendars.with(|c| {
            c.iter()
                .find(|c| c.id == d.row.calendar)
                .map(|c| c.name.clone())
        });
        let mut body = view()
            .app_class(Some("py-3 gap-1".into()))
            .child(
                view().app_class(Some("px-4 pb-1".into())).child(
                    text(d.row.name())
                        .app_class(Some(format!("text-[16px] font-semibold {INK}").into())),
                ),
            )
            .child(line("When", time::when_line(&d.row.when, &Local)));
        if !d.row.place.is_empty() {
            body = body.child(line("Where", d.row.place.clone()));
        }
        if let Some(calendar) = calendar {
            body = body.child(line("Calendar", calendar));
        }
        if !d.organizer.is_empty() {
            body = body.child(line("Organizer", d.organizer.clone()));
        }
        if !d.attendees.is_empty() {
            body = body.child(guests(&d));
        }
        if !d.meet_url.is_empty() {
            let url = d.meet_url.clone();
            body = body.child(
                view()
                    .app_class(Some("flex-row px-1".into()))
                    .child(press_text(
                        "Open the Meet link",
                        "Open the Meet link",
                        move || {
                            m.link.set(Some(url.clone()));
                        },
                    )),
            );
        }
        if !d.notes.is_empty() {
            body =
                body.child(view().app_class(Some("px-4 py-2".into())).child(
                    text(d.notes.clone()).app_class(Some(format!("text-[13px] {INK}").into())),
                ));
        }
        let (editing, can_edit) = (d.clone(), d.can_edit);
        let mut actions = view()
            .app_class(Some(
                format!("flex-row flex-wrap items-center gap-1 px-2 py-1 border-t {LINE}").into(),
            ))
            .child(press_text("Back to the calendar", "Back", move || {
                close_detail(m)
            }));
        if can_edit {
            actions = actions
                .child(press_text("Edit this event", "Edit", move || {
                    editor::edit(m, &editing);
                }))
                .child(press_text("Delete this event", "Delete", move || {
                    ask_delete(m)
                }));
        }
        let pane = view()
            .app_class(Some("flex-1".into()))
            .child(scroll().app_class(Some("flex-1".into())).child(body));
        let pane = if d.invite() {
            pane.child(replies(&b, m, &d))
        } else {
            pane
        };
        pane.child(actions)
    })
}
