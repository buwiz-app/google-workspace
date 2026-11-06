//! The `files` card: what a chat's search found, read live through its ticket
//! so no file sits in the chat's record, showing with no click.

use buwiz_app::Bridge;
use nsh_core::prelude::*;
use serde_json::{Value, json};

use super::file::{Row, rows_in};
use super::{refused, value};
use crate::compose::press_text;
use crate::shell::{DIM, INK, LINE, MUTE, PANEL, read};

/// The most files a card shows, however many the answer holds.
const ROWS: usize = 5;

fn plain(line: String) -> View {
    view()
        .app_class(Some("p-3".into()))
        .child(text(line).app_class(Some(format!("text-[12px] {MUTE}").into())))
}

fn small(line: String) -> Text {
    text(line)
        .number_of_lines(1)
        .app_class(Some(format!("max-w-full text-[11px] {MUTE}").into()))
}

/// Opens Drive on the search, as Open full screen does: the card has no way to
/// name one file to the screen.
fn open_screen(b: &Bridge, ticket: &str) {
    let open = b.open("drive", json!({"search": ticket}));
    b.spawn(async move { drop(open.await) });
}

fn row(b: &Bridge, ticket: &str, r: &Row) -> View {
    let (b, ticket) = (b.clone(), ticket.to_owned());
    view()
        .app_class(Some(format!("px-3 py-1.5 border-b {LINE}").into()))
        .label("Open this item")
        .accessibility_value(value(r.spoken()))
        .on_press(move || open_screen(&b, &ticket))
        .child(
            text(r.name.clone())
                .number_of_lines(1)
                .app_class(Some(format!("text-[13px] {INK}").into())),
        )
        // each may wrap below the last, so a narrow card loses none of them
        .child(
            view()
                .app_class(Some("flex-row flex-wrap gap-x-2".into()))
                .child(small(r.tag().to_owned()))
                .child(small(r.owner_line()))
                .child(small(r.changed_line())),
        )
}

pub(super) fn files(b: Bridge) -> View {
    #[derive(Clone, PartialEq)]
    enum State {
        Loading,
        Rows(Vec<Row>, bool),
        Line(String),
    }
    let state = signal(State::Loading);
    let ticket = b
        .props()
        .get("search")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let (reader, asked) = (b.clone(), ticket.clone());
    b.spawn(async move {
        match read(&reader, "gws_drive_card", json!({"ticket": asked})).await {
            Ok(v) => match refused(&v) {
                Some(why) => state.set(State::Line(why)),
                None => {
                    let mut rows = rows_in(&v, "rows");
                    rows.truncate(ROWS);
                    state.set(State::Rows(
                        rows,
                        v.get("saved") == Some(&Value::Bool(true)),
                    ));
                }
            },
            Err(e) if e == "sign-in" => {
                state.set(State::Line("Sign in to Google first.".to_owned()));
            }
            Err(e) => state.set(State::Line(e)),
        }
        reader.ready();
    });
    let (fit, press) = (b.clone(), b);
    view()
        .app_class(Some(PANEL.into()))
        .on_layout(
            move |bounds: &nsh_core::geometry::Bounds<nsh_core::geometry::Points>| {
                let _ = fit.fit(bounds.size.height.0.ceil());
            },
        )
        .child(dynamic(move || match state.get() {
            State::Loading => plain("Looking…".to_owned()),
            State::Line(line) => plain(line),
            State::Rows(rows, _) if rows.is_empty() => plain("No files match.".to_owned()),
            State::Rows(rows, saved) => {
                let list = rows
                    .iter()
                    .fold(view(), |list, r| list.child(row(&press, &ticket, r)));
                let list = if saved {
                    list.child(
                        view().app_class(Some("px-3 py-1".into())).child(
                            text("Saved files, offline")
                                .app_class(Some(format!("text-[11px] {DIM}").into())),
                        ),
                    )
                } else {
                    list
                };
                let (b, ticket) = (press.clone(), ticket.clone());
                // straight in the column, where it can wrap: in a row it runs off
                list.child(press_text(
                    "Open full screen",
                    "Open full screen",
                    move || {
                        open_screen(&b, &ticket);
                    },
                ))
            }
        }))
}
