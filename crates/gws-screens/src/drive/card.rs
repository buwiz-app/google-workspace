//! The `files` card: what a chat's search found, read live through its ticket
//! so no file sits in the chat's record, showing with no click.

use buwiz_app::Bridge;
use nsh_core::prelude::*;
use serde_json::{Value, json};

use super::file::{Row, rows_in};
use super::{refused, value};
use crate::compose::press_text;
use crate::shell::{
    INK, Kept, LINE, MUTE, PANEL, RUN_AGAIN, kept_footer, link_lost, lost_card, origin, read,
};

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

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

#[derive(Clone, PartialEq)]
enum State {
    Loading,
    Rows(Vec<Row>, Option<Kept>),
    Line(String),
    /// Its ticket stands for nothing.
    Lost,
}

/// Reads the card by its ticket. `query` is the search the chat made it with:
/// it goes along, so a plugin that kept no tickets past a restart binds the
/// ticket again.
async fn read_card(b: &Bridge, ticket: &str, query: Option<&str>) -> State {
    let mut args = json!({"ticket": ticket});
    if let Some(query) = query {
        args["query"] = json!(query);
    }
    match read(b, "gws_drive_card", args).await {
        Ok(v) => match refused(&v) {
            Some(why) if link_lost(&why) => State::Lost,
            Some(why) => State::Line(why),
            None => {
                let mut rows = rows_in(&v, "rows");
                rows.truncate(ROWS);
                State::Rows(rows, Kept::of(&v))
            }
        },
        Err(e) if e == "sign-in" => State::Line("Sign in to Google first.".to_owned()),
        Err(e) => State::Line(e),
    }
}

pub(super) fn files(b: Bridge) -> View {
    let state = signal(State::Loading);
    let ticket = b
        .props()
        .get("search")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let query = origin(&b, "gws_search_drive")
        .and_then(|args| Some(args.get("query")?.as_str()?.trim().to_owned()))
        .filter(|q| !q.is_empty());
    // a read asked for by a press outlives the press's own part of the card
    let (reads, waiting) = tokio::sync::mpsc::unbounded_channel::<crate::shell::Job>();
    b.spawn(crate::compose::work(waiting));
    let load = {
        let (b, ticket, query) = (b.clone(), ticket.clone(), query.clone());
        move || {
            let (reader, ticket, query) = (b.clone(), ticket.clone(), query.clone());
            state.set(State::Loading);
            drop(reads.send(Box::pin(async move {
                state.set(read_card(&reader, &ticket, query.as_deref()).await);
                reader.ready();
            })));
        }
    };
    load();
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
            State::Lost if query.is_some() => lost_card(RUN_AGAIN, load.clone()),
            // nothing says what the search was: Drive is where to make it again
            State::Lost => {
                let b = press.clone();
                lost_card("Open Drive", move || {
                    let open = b.open("drive", json!({}));
                    b.spawn(async move { drop(open.await) });
                })
            }
            State::Rows(rows, _) if rows.is_empty() => plain("No files match.".to_owned()),
            State::Rows(rows, kept) => {
                let list = rows
                    .iter()
                    .fold(view(), |list, r| list.child(row(&press, &ticket, r)));
                let list = match kept {
                    Some(kept) => {
                        // Drive's own screen is where a sign-in that went is put right
                        let b = press.clone();
                        list.child(kept_footer(
                            &kept,
                            "files",
                            now_ms(),
                            load.clone(),
                            move || {
                                let open = b.open("drive", json!({}));
                                b.spawn(async move { drop(open.await) });
                            },
                        ))
                    }
                    None => list,
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
