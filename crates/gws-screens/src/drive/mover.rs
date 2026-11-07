//! The folder chooser for a move: My Drive and what's under it, one folder at a
//! time, in the rows' room. Its Move here stays outside the scroll, so a folder
//! of hundreds can't take it away. What's being moved isn't on the list, so a
//! folder can't be sent inside itself.

use buwiz_app::Bridge;
use nsh_core::prelude::*;
use serde_json::{Value, json};

use super::edits::{press, press_as};
use super::file::{Crumb, Row, rows_in};
use super::write::{
    Acting, answer_write, keep_undo, landed, start_acting, stop_acting, stop_opening,
};
use super::{Drive, ask, fetch, value};
use crate::shell::{INK, LINE, Latest, MUTE, PANEL};

/// Pages read for one folder's folders. They come first, so this is only ever
/// reached by a folder that really holds this many.
const PAGES_MAX: usize = 10;

/// What's in the chooser now.
#[derive(Clone, Copy)]
pub(super) struct Mover {
    /// The way down from My Drive.
    trail: Signal<Vec<Crumb>>,
    folders: Signal<Vec<Row>>,
    /// The folder shown's id, as a file's parents say it.
    here: Signal<Option<String>>,
    /// Google lets a file go in the folder shown.
    adds: Signal<bool>,
    reading: Signal<bool>,
    problem: Signal<Option<String>>,
    turn: Latest,
}

impl Mover {
    pub(super) fn new() -> Self {
        Self {
            trail: signal(Vec::new()),
            folders: signal(Vec::new()),
            here: signal(None),
            adds: signal(false),
            reading: signal(false),
            problem: signal(None),
            turn: Latest::new(),
        }
    }
}

fn folders_in(v: &Value, moving: &str) -> Vec<Row> {
    let rows = rows_in(v, "files");
    let folders = rows.into_iter().filter(|r| r.is_folder() && !r.trashed);
    folders.filter(|r| r.id != moving).collect()
}

/// A folder's folders. A list has them first, so a page that isn't all folders
/// is the last that's needed.
async fn read_folders(b: Bridge, m: Drive, moving: String) {
    let c = m.mover;
    let turn = c.turn.ask();
    c.reading.set(true);
    c.problem.set(None);
    c.folders.set(Vec::new());
    let folder = c.trail.with_untracked(|t| t.last().map(|c| c.id.clone()));
    let (mut found, mut skip) = (Vec::new(), 0);
    for _ in 0..PAGES_MAX {
        let mut args = json!({"view": "my", "skip": skip});
        if let Some(folder) = &folder {
            args["folder"] = json!(folder);
        }
        let answer = ask(&b, "gws_drive_list", args).await;
        if !c.turn.counts(turn) {
            return;
        }
        let v = match answer {
            Ok(v) => v,
            Err(why) => {
                c.reading.set(false);
                c.problem.set(Some(why));
                return;
            }
        };
        let all = v.get("files").and_then(Value::as_array).map_or(0, Vec::len);
        let page = folders_in(&v, &moving);
        let folders_only = rows_in(&v, "files").iter().all(Row::is_folder);
        found.extend(page);
        skip += all;
        c.here
            .set(v.get("at").and_then(Value::as_str).map(str::to_owned));
        c.adds.set(v.get("can_add") == Some(&Value::Bool(true)));
        if !folders_only || v.get("more") != Some(&Value::Bool(true)) {
            break;
        }
    }
    c.reading.set(false);
    c.folders.set(found);
}

/// Opens the chooser on My Drive.
pub(super) fn open(b: &Bridge, m: Drive, row: Row) {
    m.said.set(None);
    m.mover.trail.set(Vec::new());
    m.mover.here.set(None);
    m.mover.adds.set(false);
    fetch(m, read_folders(b.clone(), m, row.id.clone()));
    start_acting(m, Acting::Move(row));
}

fn go(b: &Bridge, m: Drive, moving: &str, trail: Vec<Crumb>) {
    m.mover.trail.set(trail);
    m.mover.adds.set(false);
    fetch(m, read_folders(b.clone(), m, moving.to_owned()));
}

fn move_here(b: &Bridge, m: Drive, row: &Row) {
    if m.writing.get_untracked() {
        return;
    }
    let to = m
        .mover
        .trail
        .with_untracked(|t| t.last().map_or_else(|| "root".to_owned(), |c| c.id.clone()));
    let call = b.call("gws_drive_move", json!({"id": row.id, "to": to}));
    let opening = m.opening.now();
    answer_write(b, m, call, move |b, m, v| {
        // the same as a rename: what's up now may be another file
        let said = if m.opening.counts(opening) {
            "Moved."
        } else {
            "Your earlier move went through."
        };
        stop_opening(m, opening);
        keep_undo(m, v, said);
        landed(b, m, v);
    });
}

fn trail_line(b: Bridge, m: Drive, row: Row) -> impl IntoElement {
    dynamic(move || {
        let top = (b.clone(), row.id.clone());
        let first = press_as("Go to My Drive", "My Drive".to_owned(), "", move || {
            go(&top.0, m, &top.1, Vec::new());
        });
        let line = view()
            .app_class(Some(
                format!("flex-row flex-wrap items-center gap-1 px-2 py-1 border-b {LINE}").into(),
            ))
            .child(first);
        let mut way = Vec::new();
        m.mover.trail.get().into_iter().fold(line, |line, part| {
            way.push(part.clone());
            let (b, kept, id) = (b.clone(), way.clone(), row.id.clone());
            line.child(press_as(
                "Back to this folder",
                part.name.clone(),
                &part.name,
                move || go(&b, m, &id, kept.clone()),
            ))
        })
    })
}

fn folder_row(b: &Bridge, m: Drive, moving: &str, folder: Row) -> View {
    let (b, id) = (b.clone(), moving.to_owned());
    let name = folder.name.clone();
    view()
        .app_class(Some(format!("px-3 py-2 border-b {LINE}").into()))
        .label("Go into this folder")
        .accessibility_value(value(name.clone()))
        .on_press(move || {
            let mut trail = m.mover.trail.get_untracked();
            trail.push(Crumb {
                id: folder.id.clone(),
                name: folder.name.clone(),
            });
            go(&b, m, &id, trail);
        })
        .child(
            text(name)
                .number_of_lines(1)
                .app_class(Some(format!("text-[13px] {INK}").into())),
        )
}

fn folder_list(b: Bridge, m: Drive, row: Row) -> impl IntoElement {
    dynamic(move || {
        let c = m.mover;
        if let Some(why) = c.problem.get() {
            return view()
                .app_class(Some("p-3".into()))
                .child(text(why).app_class(Some(format!("text-[12px] {MUTE}").into())));
        }
        let folders = c.folders.get();
        if folders.is_empty() {
            let line = if c.reading.get() {
                "Reading your folders…"
            } else {
                "No folders in here."
            };
            return view()
                .app_class(Some("p-3".into()))
                .child(text(line).app_class(Some(format!("text-[12px] {MUTE}").into())));
        }
        folders.into_iter().fold(view(), |list, folder| {
            list.child(folder_row(&b, m, &row.id, folder))
        })
    })
}

/// The press that moves it, when Google would take it: not where it already is,
/// and not into a folder that takes no new files.
fn footer(b: Bridge, m: Drive, row: Row) -> impl IntoElement {
    let name = row.name.clone();
    let (place, shut) = (row, name.clone());
    view()
        .app_class(Some(
            format!("flex-row flex-wrap items-center gap-1 px-2 py-1 border-t {LINE}").into(),
        ))
        .child(dynamic(move || {
            let c = m.mover;
            let there = c.here.with(|here| {
                here.as_ref()
                    .is_some_and(|here| place.parents.contains(here))
            });
            if c.reading.get() || c.problem.with(Option::is_some) {
                return view();
            }
            if there {
                return view().child(
                    text("It's in this folder now.")
                        .app_class(Some(format!("text-[11px] {MUTE}").into())),
                );
            }
            if !c.adds.get() {
                return view().child(
                    text("You can't add to this folder.")
                        .app_class(Some(format!("text-[11px] {MUTE}").into())),
                );
            }
            let (b, row) = (b.clone(), place.clone());
            view().app_class(Some("max-w-full".into())).child(press(
                "Move here",
                &name,
                move || move_here(&b, m, &row),
            ))
        }))
        .child(press_as(
            "Cancel this move",
            "Cancel".to_owned(),
            &shut,
            move || {
                stop_acting(m);
                m.said.set(None);
            },
        ))
}

pub(super) fn pane(b: Bridge, m: Drive, row: Row) -> View {
    let (trail, list) = (b.clone(), b.clone());
    view()
        .app_class(Some(format!("flex-1 min-h-0 {PANEL}").into()))
        .child(
            view()
                .app_class(Some(format!("px-3 py-1 border-b {LINE}").into()))
                .child(
                    text("Move to")
                        .app_class(Some(format!("text-[13px] font-semibold {INK}").into())),
                ),
        )
        .child(
            scroll()
                .app_class(Some("max-h-[64px]".into()))
                .child(trail_line(trail, m, row.clone())),
        )
        .child(
            scroll()
                .app_class(Some("flex-1 min-h-0".into()))
                .child(folder_list(list, m, row.clone())),
        )
        .child(footer(b, m, row))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_is_being_moved_is_never_a_place_to_move_it() {
        let v = json!({"files": [
            {"id": "a", "name": "A", "kind": "folder"},
            {"id": "b", "name": "B", "kind": "folder"},
            {"id": "c", "name": "C", "kind": "doc"},
            {"id": "d", "name": "D", "kind": "folder", "trashed": true},
        ]});
        let ids: Vec<String> = folders_in(&v, "a").into_iter().map(|r| r.id).collect();
        assert_eq!(ids, ["b"]);
    }
}
