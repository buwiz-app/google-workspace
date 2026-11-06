//! A file opened from here. A press opens it in the Bot's viewer; a bar then
//! says what to do with one edited here, with the buttons of the plan's table.
//! Nothing here uploads by itself: `gws_file_upload` is called from a press and
//! nowhere else, and the plugin refuses what would overwrite. `write.rs` has the
//! rest of what changes Drive.
//!
//! Each call starts at the press itself, before anything else can run: the Bot
//! judges who pressed by the moment of the call, and only a person's press
//! opens the viewer. The bars keep to plain flow and wrap, so a narrow screen
//! loses none of their presses. A file's name is a value and a text, never a
//! label.

use std::future::Future;

use buwiz_app::{Bridge, BuwizError};
use nsh_core::prelude::*;
use serde_json::{Value, json};

use super::file::{Edit, Row, edits_in};
use super::{Drive, SIGN_IN_AGAIN, fetch, refused, show_details, value};
use crate::shell::{INK, LINE, MUTE, RAIL, read};

/// What became of a press on a file.
#[derive(Clone, PartialEq)]
pub(super) struct Said {
    line: String,
    /// Where the file opens instead.
    link: Option<String>,
    /// It's over, so the person can put it away.
    done: bool,
}

pub(super) fn say(m: Drive, line: &str) {
    m.said.set(Some(Said {
        line: line.to_owned(),
        link: None,
        done: true,
    }));
}

/// A failed call, as the person hears it. The Bot gives a call 30 seconds, and
/// the plugin goes on past that, so a timeout isn't a failure.
pub(super) fn failed_line(e: &BuwizError) -> String {
    if e.code == "timeout" {
        "Google Drive is taking a while. It's still going, so try again in a moment.".to_owned()
    } else if e.message.contains("Sign in to Google") {
        SIGN_IN_AGAIN.to_owned()
    } else {
        e.message.clone()
    }
}

pub(super) async fn load_edits(b: Bridge, m: Drive) {
    if let Ok(v) = read(&b, "gws_file_edits", json!({})).await {
        m.edits.set(edits_in(&v));
    }
}

fn opened(b: &Bridge, m: Drive, row: Row, v: &Value) {
    match v.get("refused").and_then(Value::as_str) {
        None => m.said.set(None),
        // no viewer takes it, and the details still have something to show
        Some("unsupported") => {
            m.said.set(None);
            show_details(b, m, row);
        }
        Some(_) => m.said.set(Some(Said {
            line: refused(v).unwrap_or_default(),
            link: v.get("link").and_then(Value::as_str).map(str::to_owned),
            done: true,
        })),
    }
}

/// A press on a file. If it worked, the Bot has already put the viewer over the
/// screen, from the answer's `_meta`, which never reaches here.
pub(super) fn open_file(b: &Bridge, m: Drive, row: Row) {
    let call = b.call("gws_file_open", json!({"id": row.id}));
    let turn = m.file_turn.ask();
    m.said.set(Some(Said {
        line: "Opening…".to_owned(),
        link: None,
        done: false,
    }));
    let b = b.clone();
    fetch(m, async move {
        let answer = call.await;
        // the person pressed something since: this one has nothing left to say
        if m.file_turn.counts(turn) {
            match &answer {
                Ok(v) => opened(&b, m, row, v),
                Err(e) => say(m, &failed_line(e)),
            }
        }
        load_edits(b, m).await;
    });
}

/// A press on a bar: the file shows as under way until the answer's in, so
/// nothing can be pressed twice. A refusal says why, and reading the edits again
/// brings the buttons that fit what Drive holds now.
fn answer(
    b: &Bridge,
    m: Drive,
    id: String,
    call: impl Future<Output = Result<Value, BuwizError>> + 'static,
    done: &'static str,
) {
    let turn = m.file_turn.ask();
    m.said.set(None);
    m.pressed.update(|p| {
        p.insert(id.clone());
    });
    let b = b.clone();
    fetch(m, async move {
        let answer = call.await;
        if m.file_turn.counts(turn) {
            match &answer {
                Ok(v) => say(m, &refused(v).unwrap_or_else(|| done.to_owned())),
                Err(e) => say(m, &failed_line(e)),
            }
        }
        load_edits(b, m).await;
        m.pressed.update(|p| {
            p.remove(&id);
        });
    });
}

fn upload(b: &Bridge, m: Drive, id: &str, mode: &str, done: &'static str) {
    let call = b.call("gws_file_upload", json!({"id": id, "mode": mode}));
    answer(b, m, id.to_owned(), call, done);
}

fn discard(b: &Bridge, m: Drive, id: &str) {
    let call = b.call("gws_file_discard", json!({"id": id}));
    answer(
        b,
        m,
        id.to_owned(),
        call,
        "Discarded. Nothing changed on Google Drive.",
    );
}

/// A press in a bar: as wide as what's around it at most, so its words wrap
/// and it never runs off the edge.
pub(super) fn press(label: &'static str, name: &str, on: impl Fn() + 'static) -> View {
    press_as(label, label.to_owned(), name, on)
}

/// [`press`] showing words of its own: its label stays fixed whatever they say.
pub(super) fn press_as(
    label: &'static str,
    shows: String,
    name: &str,
    on: impl Fn() + 'static,
) -> View {
    view()
        .app_class(Some(
            format!("max-w-full px-2 py-1 rounded-md border {LINE}").into(),
        ))
        .label(label)
        .accessibility_value(value(name))
        .on_press(on)
        .child(text(shows).app_class(Some(format!("text-[12px] {INK}").into())))
}

/// The buttons of the plan's table, by what's on Drive.
fn buttons(b: &Bridge, m: Drive, e: &Edit) -> View {
    let row = view().app_class(Some("flex-row flex-wrap items-center gap-1".into()));
    let (bb, id) = (b.clone(), e.id.clone());
    let row = if e.attachment {
        row.child(press("Save to Google Drive", &e.name, move || {
            upload(&bb, m, &id, "copy", "Saved to Google Drive as a new file.");
        }))
    } else if e.google {
        row.child(press("Save a copy to Google Drive", &e.name, move || {
            upload(&bb, m, &id, "copy", "Saved a copy to Google Drive.");
        }))
    } else if e.changed {
        let (bc, copied) = (b.clone(), e.id.clone());
        row.child(press("Save as a copy", &e.name, move || {
            upload(&bc, m, &copied, "copy", "Saved as a copy on Google Drive.");
        }))
        .child(press("Replace the one on Drive", &e.name, move || {
            upload(&bb, m, &id, "replace", "Replaced the one on Google Drive.");
        }))
    } else {
        row.child(press("Upload to Google Drive", &e.name, move || {
            upload(&bb, m, &id, "update", "Uploaded to Google Drive.");
        }))
    };
    let id = e.id.clone();
    // fewer words than it shows: the AI mustn't raise a question whose yes can't
    // be taken back, for the person to press
    row.child(press_as(
        "Discard",
        "Discard edits".to_owned(),
        &e.name,
        move || {
            m.discarding.set(Some(id.clone()));
        },
    ))
}

/// Discard can't be undone: the edits exist nowhere else, so it asks once.
fn asking(b: &Bridge, m: Drive, e: &Edit) -> View {
    let (b, id) = (b.clone(), e.id.clone());
    view()
        .app_class(Some("flex-row flex-wrap items-center gap-1".into()))
        .child(line(
            "Discard your edits? They can't be brought back.".to_owned(),
            INK,
        ))
        .child(press("Keep my edits", &e.name, move || {
            m.discarding.set(None)
        }))
        .child(press("Discard my edits", &e.name, move || {
            m.discarding.set(None);
            discard(&b, m, &id);
        }))
}

fn line(words: String, tone: &str) -> Text {
    text(words).app_class(Some(format!("text-[12px] {tone}").into()))
}

fn bar(b: &Bridge, m: Drive, e: &Edit, under_way: bool) -> View {
    let top = view()
        .app_class(Some(
            format!("px-3 py-2 gap-1 border-b {LINE} {RAIL}").into(),
        ))
        .child(
            text(format!("{} has changes here", e.name))
                .number_of_lines(2)
                .app_class(Some(format!("text-[13px] {INK}").into())),
        );
    let top = if e.attachment {
        top.child(line(
            "This came from a mail. It stays here until you save it as a new file or discard it."
                .to_owned(),
            MUTE,
        ))
    } else if e.google {
        top.child(line(
            "Google's own file stays as it is. This saves a new one beside it.".to_owned(),
            MUTE,
        ))
    } else if e.changed {
        top.child(line(
            "Google Drive also changed since you opened it.".to_owned(),
            MUTE,
        ))
    } else {
        top
    };
    if under_way {
        top.child(line("Working on it…".to_owned(), MUTE))
    } else if m.discarding.get().as_deref() == Some(e.id.as_str()) {
        top.child(asking(b, m, e))
    } else {
        top.child(buttons(b, m, e))
    }
}

/// A bar for each file edited here, wherever in Drive the person is now: one
/// that's in another folder still has to be answered. Past a few it scrolls, so
/// the rows keep their room.
pub(super) fn bars(b: Bridge, m: Drive) -> impl IntoElement {
    dynamic(move || {
        let edits = m.edits.get();
        if edits.is_empty() {
            return view();
        }
        let pressed = m.pressed.get();
        let list = edits.iter().fold(view(), |list, e| {
            list.child(bar(&b, m, e, e.working || pressed.contains(&e.id)))
        });
        view().child(scroll().app_class(Some("max-h-[240px]".into())).child(list))
    })
}

/// What became of the last press, and the way to the browser if there is one.
pub(super) fn said_line(b: Bridge, m: Drive) -> impl IntoElement {
    dynamic(move || {
        let Some(said) = m.said.get() else {
            return view();
        };
        let mut row = view()
            .app_class(Some(
                format!("flex-row flex-wrap items-center gap-1 px-3 py-1 border-b {LINE}").into(),
            ))
            .child(text(said.line).app_class(Some(
                format!("flex-1 min-w-[80px] text-[11px] {INK}").into(),
            )));
        if let Some(link) = said.link {
            let b = b.clone();
            row = row.child(press("Open in the browser", "", move || {
                let open = b.open_url(&link);
                fetch(m, async move { drop(open.await) });
            }));
        }
        if said.done {
            row = row.child(press("Dismiss", "", move || m.said.set(None)));
        }
        row
    })
}
