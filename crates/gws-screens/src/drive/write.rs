//! What changes Drive from here, past opening a file: a bar of presses for the
//! folder and the rows picked, a name to type, Undo, and files going up.
//!
//! Each call starts at the press itself, before anything else can run: the Bot
//! judges who pressed by the moment of the call, and an AI's press goes out as
//! the AI and is refused. A press is only offered when `capabilities` says Google
//! would take it. Nothing changes on screen until the answer is in, so a write
//! that fails has nothing to take back: it says why and the rows stay.
//!
//! Everything pressable keeps to plain flow and wraps, so a narrow screen loses
//! none of it. A file's name is a value and a text, never a label, and never in
//! what a press says afterwards.

use std::future::Future;

use buwiz_app::{Bridge, BuwizError, PickOptions, PickedFile};
use nsh_core::element::input;
use nsh_core::prelude::*;
use serde_json::{Value, json};

use super::edits::{failed_line, press, press_as, say};
use super::file::{Row, rows_in};
use super::{Drive, close_details, fetch, mover, refused, reload, share, show_details, value};
use crate::compose::{ATTACHMENTS_MAX, CHUNK, chunk};
use crate::shell::{BG, INK, LINE, MUTE, RAIL, owned};

/// The most rows one press takes, as the tool does.
const TRASH_MAX: usize = 100;

/// What's being done, past the rows.
#[derive(Clone, PartialEq)]
pub(super) enum Acting {
    NewFolder,
    Rename(Row),
    Move(Row),
    Share(Row),
}

impl Acting {
    /// It needs the rows' room.
    pub(super) fn panel(acting: &Option<Acting>) -> bool {
        matches!(acting, Some(Acting::Move(_) | Acting::Share(_)))
    }
}

/// Puts up what's being done. Each opening is its own turn, so an answer can tell
/// the one it came from from any other, even on the same file.
pub(super) fn start_acting(m: Drive, acting: Acting) {
    m.opening.ask();
    m.acting.set(Some(acting));
}

pub(super) fn stop_acting(m: Drive) {
    m.opening.ask();
    m.acting.set(None);
}

/// Closes what a press came from, if it's what is up. A press can land after the
/// person has cancelled it and gone on to something else.
pub(super) fn stop_opening(m: Drive, opening: u64) {
    if m.opening.counts(opening) {
        stop_acting(m);
    }
}

/// The presses Google would take on these rows.
#[derive(Debug, Default, PartialEq)]
struct Offer {
    rename: bool,
    shift: bool,
    share: bool,
    trash: bool,
}

/// What's in the Trash has no presses here. A rename, a move and a share are for
/// one file; the Trash goes for all that's picked or for none.
fn offer(rows: &[Row]) -> Offer {
    if rows.is_empty() || rows.iter().any(|r| r.trashed) {
        return Offer::default();
    }
    let one = rows.len() == 1;
    Offer {
        rename: one && rows[0].may("rename"),
        shift: one && rows[0].may("move"),
        share: one && rows[0].may("share"),
        trash: rows.iter().all(|r| r.may("trash")),
    }
}

/// The picked rows, in the list's order.
fn picked_rows(m: Drive) -> Vec<Row> {
    m.picked.with(|p| {
        m.rows
            .with(|rows| rows.iter().filter(|r| p.contains(&r.id)).cloned().collect())
    })
}

/// Where a new file goes: the folder that's open, or the top of My Drive.
fn open_parent(m: Drive) -> String {
    m.folder
        .get_untracked()
        .unwrap_or_else(|| "root".to_owned())
}

/// The answer to a press that changes Drive. Only a good answer touches the
/// screen. What's said for a refused press stays, for the person to act on,
/// while they're still on what they pressed it from: an answer from an opening
/// that's gone says nothing, as it could be read as about what's up now.
pub(super) fn answer_write(
    b: &Bridge,
    m: Drive,
    call: impl Future<Output = Result<Value, BuwizError>> + 'static,
    done: impl FnOnce(&Bridge, Drive, &Value) + 'static,
) {
    let opening = m.opening.now();
    m.writing.set(true);
    m.said.set(None);
    let b = b.clone();
    fetch(m, async move {
        let answer = call.await;
        m.writing.set(false);
        let here = m.opening.counts(opening);
        match &answer {
            Ok(v) => match refused(v) {
                Some(why) if here => say(m, &why),
                Some(_) => {}
                None => done(&b, m, v),
            },
            Err(e) if here => say(m, &failed_line(e)),
            Err(_) => {}
        }
    });
}

/// What Undo can take back now, if this answer left a way.
pub(super) fn keep_undo(m: Drive, v: &Value, said: &str) {
    if let Some(undo) = v.get("undo").and_then(Value::as_str) {
        m.undo.set(Some((undo.to_owned(), said.to_owned())));
    }
}

/// A write went through. A list reads the store, which already has it. A search
/// is Google's, whose index can lag behind a write, so its rows change here.
pub(super) fn landed(b: &Bridge, m: Drive, v: &Value) {
    let files = rows_in(v, "files");
    let open = m.open.get_untracked();
    if let Some(now) = open.and_then(|o| files.iter().find(|f| f.id == o.id).cloned()) {
        if now.trashed {
            close_details(m);
        } else {
            show_details(b, m, now);
        }
    }
    if m.search.with_untracked(Option::is_none) {
        fetch(m, reload(b.clone(), m));
        return;
    }
    let trashed = |id: &String| files.iter().any(|f| f.id == *id && f.trashed);
    m.picked.update(|p| p.retain(|id| !trashed(id)));
    m.rows.update(|rows| {
        rows.retain(|r| !trashed(&r.id));
        for row in rows {
            if let Some(now) = files.iter().find(|f| f.id == row.id) {
                *row = now.clone();
            }
        }
    });
}

pub(super) fn start_rename(m: Drive) {
    let rows = m.picked_now();
    if offer(&rows).rename {
        m.typed.set(rows[0].name.clone());
        start_acting(m, Acting::Rename(rows[0].clone()));
    }
}

impl Drive {
    fn picked_now(self) -> Vec<Row> {
        self.picked.with_untracked(|p| {
            self.rows
                .with_untracked(|rows| rows.iter().filter(|r| p.contains(&r.id)).cloned().collect())
        })
    }
}

fn trash(b: &Bridge, m: Drive) {
    if m.writing.get_untracked() {
        return;
    }
    let rows = m.picked_now();
    if rows.len() > TRASH_MAX {
        say(m, "Pick 100 or fewer to move to the Trash.");
        return;
    }
    let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
    let call = b.call("gws_drive_trash", json!({"ids": ids}));
    answer_write(b, m, call, |b, m, v| {
        let done = v.get("files").and_then(Value::as_array).map_or(0, Vec::len);
        if done > 0 {
            keep_undo(m, v, &trashed_line(done));
        }
        if v.get("partial") == Some(&Value::Bool(true)) {
            say(m, "Only some of these went to the Trash. The rest didn't.");
        }
        landed(b, m, v);
    });
}

fn trashed_line(done: usize) -> String {
    if done == 1 {
        "Moved to Trash.".to_owned()
    } else {
        format!("Moved {done} items to Trash.")
    }
}

/// The name that was typed, as a rename or a new folder.
fn commit(b: &Bridge, m: Drive) {
    if m.writing.get_untracked() {
        return;
    }
    let typed = m.typed.get_untracked();
    let name = typed.trim();
    if name.is_empty() {
        say(m, "Type a name first.");
        return;
    }
    let opening = m.opening.now();
    match m.acting.get_untracked() {
        Some(Acting::Rename(row)) if row.name == name => stop_acting(m),
        Some(Acting::Rename(row)) => {
            let call = b.call("gws_drive_rename", json!({"id": row.id, "name": name}));
            answer_write(b, m, call, move |b, m, v| {
                stop_opening(m, opening);
                keep_undo(m, v, "Renamed.");
                landed(b, m, v);
            });
        }
        Some(Acting::NewFolder) => {
            let args = json!({"parent": open_parent(m), "name": name});
            let args = owned(args, &m.chooser.who.get_untracked());
            let call = b.call("gws_drive_folder", args);
            answer_write(b, m, call, move |b, m, v| {
                if m.opening.counts(opening) {
                    say(m, "Made a new folder on Google Drive.");
                }
                stop_opening(m, opening);
                landed(b, m, v);
            });
        }
        _ => {}
    }
}

/// A name to type, under the bar. It stays through a refusal, to be tried again.
fn name_bar(b: Bridge, m: Drive, renaming: bool) -> View {
    // past 48 characters on purpose: the Bot's filter leaves such a field unnamed
    // and refuses the AI, whose name would go at the person's press
    let (label, field, word, press_label, shows) = if renaming {
        (
            "New name, as it will show in Google Drive to others",
            "New name",
            "Rename",
            "Save this name",
            "Rename",
        )
    } else {
        (
            "Folder name, as it will show in Google Drive to others",
            "Folder name",
            "New folder",
            "Make this folder",
            "Make folder",
        )
    };
    let (typed, pressed) = (b.clone(), b);
    view()
        .app_class(Some(
            format!("flex-row flex-wrap items-center gap-1 px-2 py-1 border-b {LINE} {RAIL}")
                .into(),
        ))
        .child(text(word).app_class(Some(format!("text-[12px] {MUTE}").into())))
        .child(
            input()
                .label(label)
                .value(move || m.typed.get())
                .placeholder(field.to_owned())
                .auto_focus(true)
                .app_class(Some(
                    format!(
                        "flex-1 min-w-[60px] max-w-full rounded-lg border px-2.5 py-1 text-[13px] {BG} {LINE} {INK}"
                    )
                    .into(),
                ))
                .on_change(move |value: &str| m.typed.set(value.to_owned()))
                .on_submit(move |_: &str| commit(&typed, m)),
        )
        .child(press_as(press_label, shows.to_owned(), "", move || {
            commit(&pressed, m);
        }))
        .child(press_as("Cancel this", "Cancel".to_owned(), "", move || {
            stop_acting(m);
            m.said.set(None);
        }))
}

fn name_line(m: Drive, b: Bridge) -> impl IntoElement {
    let kind = computed(move || {
        m.acting.with(|a| match a {
            Some(Acting::NewFolder) => Some(false),
            Some(Acting::Rename(_)) => Some(true),
            _ => None,
        })
    });
    dynamic(move || match kind.get() {
        Some(renaming) => name_bar(b.clone(), m, renaming),
        None => view(),
    })
}

/// A file on its way up. It keeps what was picked, so a try again can read it
/// from the start: the plugin holds nothing once a press has spent it.
#[derive(Clone)]
pub(super) struct Sending {
    key: u64,
    file: PickedFile,
    parent: String,
    /// The account it was picked on: a switch mustn't send it to the other.
    who: String,
    arrived: u64,
    why: Option<String>,
    /// The same file could still go.
    again: bool,
}

impl PartialEq for Sending {
    fn eq(&self, other: &Self) -> bool {
        (self.key, self.arrived, &self.why, self.again)
            == (other.key, other.arrived, &other.why, other.again)
    }
}

fn change_send(m: Drive, key: u64, change: impl FnOnce(&mut Sending)) {
    m.sending.update(|all| {
        if let Some(item) = all.iter_mut().find(|i| i.key == key) {
            change(item);
        }
    });
}

fn send_failed(m: Drive, key: u64, why: String, again: bool) {
    change_send(m, key, |item| {
        item.why = Some(why);
        item.again = again;
    });
}

fn finish_send(m: Drive, key: u64) {
    m.sending.update(|all| all.retain(|i| i.key != key));
}

/// The bytes into the plugin, a chunk at a time: an answer says how far it got,
/// and a chunk it has already is never kept twice.
async fn chunks(
    b: &Bridge,
    m: Drive,
    key: u64,
    file: &PickedFile,
    bytes: &[u8],
) -> Result<String, BuwizError> {
    use base64::Engine as _;
    let (mut at, mut id) = (0usize, Value::Null);
    while at < bytes.len() || id.is_null() {
        let end = (at + CHUNK).min(bytes.len());
        let data = base64::engine::general_purpose::STANDARD.encode(&bytes[at..end]);
        let mut args = json!({"name": file.name, "type": file.mime, "at": at, "data": data});
        if !id.is_null() {
            args["upload"] = id.clone();
        }
        let got = match chunk(b, args).await {
            Ok(v) => v,
            Err(e) => {
                if !id.is_null() {
                    let _ = b.call("gws_upload_drop", json!({"upload": id})).await;
                }
                return Err(e);
            }
        };
        id = got.get("upload").cloned().unwrap_or(Value::Null);
        at = got.get("size").and_then(Value::as_u64).unwrap_or(0) as usize;
        change_send(m, key, |item| item.arrived = at as u64);
    }
    Ok(id.as_str().unwrap_or_default().to_owned())
}

/// One file, from the person's disk to the folder chosen at the press.
async fn send(b: Bridge, m: Drive, key: u64) {
    let item = m
        .sending
        .with_untracked(|all| all.iter().find(|i| i.key == key).cloned());
    let Some(item) = item else {
        return;
    };
    change_send(m, key, |item| {
        item.why = None;
        item.arrived = 0;
    });
    if item.file.size > ATTACHMENTS_MAX {
        let why = "This file is over the 25 MB this screen can upload.".to_owned();
        return send_failed(m, key, why, false);
    }
    let bytes = match item.file.read().await {
        Ok(bytes) => bytes,
        Err(e) => return send_failed(m, key, failed_line(&e), true),
    };
    let upload = match chunks(&b, m, key, &item.file, &bytes).await {
        Ok(upload) => upload,
        Err(e) => return send_failed(m, key, failed_line(&e), true),
    };
    let args = json!({"upload": upload, "parent": item.parent});
    let args = owned(args, &item.who);
    match b.call("gws_drive_upload", args).await {
        Ok(v) => match refused(&v) {
            Some(why) => send_failed(m, key, why, true),
            None => {
                finish_send(m, key);
                say(m, "Uploaded to Google Drive.");
                landed(&b, m, &v);
            }
        },
        // the plugin goes on past what the Bot waits for, and the list shows it
        Err(e) if e.code == "timeout" => {
            finish_send(m, key);
            say(m, &failed_line(&e));
        }
        Err(e) => send_failed(m, key, failed_line(&e), true),
    }
}

/// Asks for files, for the folder that's open at the press.
fn pick(b: &Bridge, m: Drive) {
    let parent = open_parent(m);
    let options = PickOptions {
        extensions: Vec::new(),
        multiple: true,
    };
    // as `parent` is: the picker can stay open through a switch
    let who = m.chooser.who.get_untracked();
    let Some(pick) = b.pick_file(options) else {
        say(m, "This screen can't reach your files.");
        return;
    };
    let b = b.clone();
    fetch(m, async move {
        let files = match pick.await {
            Ok(files) => files,
            Err(e) => return say(m, &failed_line(&e)),
        };
        let mut keys = Vec::new();
        for file in files {
            let key = m.next_send.get_untracked() + 1;
            m.next_send.set(key);
            keys.push(key);
            m.sending.update(|all| {
                all.push(Sending {
                    key,
                    file,
                    parent: parent.clone(),
                    who: who.clone(),
                    arrived: 0,
                    why: None,
                    again: false,
                });
            });
        }
        // one at a time: each is held whole in memory until it's gone
        for key in keys {
            send(b.clone(), m, key).await;
        }
    });
}

fn sending_row(b: &Bridge, m: Drive, item: &Sending) -> View {
    let name = item.file.name.clone();
    let line = item.why.clone().unwrap_or_else(|| {
        format!(
            "{} of {}",
            super::file::size_text(item.arrived),
            super::file::size_text(item.file.size)
        )
    });
    let spoken = format!("{name}, {line}");
    let row = view()
        .app_class(Some(
            format!("flex-row flex-wrap items-center gap-1 px-3 py-1 border-b {LINE}").into(),
        ))
        .child(
            view()
                .app_class(Some("flex-1 min-w-[80px]".into()))
                .label("A file going to Google Drive")
                .accessibility_value(value(spoken))
                .child(
                    text(name.clone())
                        .number_of_lines(1)
                        .app_class(Some(format!("text-[12px] {INK}").into())),
                )
                .child(text(line).app_class(Some(format!("text-[11px] {MUTE}").into()))),
        );
    if item.why.is_none() {
        return row;
    }
    let key = item.key;
    let row = row.child(press("Forget this upload", &name, move || {
        finish_send(m, key);
    }));
    if !item.again {
        return row;
    }
    let b = b.clone();
    // fewer words than its label: its calls come after the press, where the Bot
    // can't tell them from a person's, so the AI must never make this one
    row.child(press_as(
        "Try this upload again",
        "Try again".to_owned(),
        &name,
        move || fetch(m, send(b.clone(), m, key)),
    ))
}

/// A row for each file on its way. Past a few it scrolls, so the rows keep
/// their room.
fn sending_list(b: Bridge, m: Drive) -> impl IntoElement {
    dynamic(move || {
        let all = m.sending.get();
        if all.is_empty() {
            return view();
        }
        let list = all
            .iter()
            .fold(view(), |list, item| list.child(sending_row(&b, m, item)));
        view().child(scroll().app_class(Some("max-h-[160px]".into())).child(list))
    })
}

/// What was just done, with the way back.
fn undo_bar(b: Bridge, m: Drive) -> impl IntoElement {
    dynamic(move || match m.undo.get() {
        None => view(),
        Some((_, said)) => {
            let b = b.clone();
            view()
                .app_class(Some(
                    format!(
                        "flex-row flex-wrap items-center gap-1 px-3 py-1 border-b {LINE} {RAIL}"
                    )
                    .into(),
                ))
                .child(text(said).app_class(Some(
                    format!("flex-1 min-w-[80px] text-[12px] {INK}").into(),
                )))
                .child(press_as("Undo that", "Undo".to_owned(), "", move || {
                    undo(&b, m);
                }))
        }
    })
}

/// What Undo would take back now, and nothing while that's on its way: a second
/// press would ask for what the first already took.
fn claim_undo(m: Drive) -> Option<String> {
    let (id, _) = m.undo.get_untracked()?;
    if m.undoing.with_untracked(|u| u.as_ref() == Some(&id)) {
        return None;
    }
    m.undoing.set(Some(id.clone()));
    Some(id)
}

/// Undoes the last write: named at the press, as another can finish before this
/// one's answer comes. A refusal keeps the bar, so it can be pressed again.
pub(super) fn undo(b: &Bridge, m: Drive) {
    let Some(id) = claim_undo(m) else {
        return;
    };
    let call = b.call("gws_drive_undo", json!({"undo": id}));
    let b = b.clone();
    m.said.set(None);
    fetch(m, async move {
        let answer = call.await;
        if m.undoing.with_untracked(|u| u.as_ref() == Some(&id)) {
            m.undoing.set(None);
        }
        match answer {
            Ok(v) => match refused(&v) {
                Some(why) => say(m, &why),
                None => {
                    if m.undo
                        .with_untracked(|u| u.as_ref().is_some_and(|(now, _)| *now == id))
                    {
                        m.undo.set(None);
                    }
                    landed(&b, m, &v);
                    // what comes back has no row in a search to change
                    if m.search.with_untracked(Option::is_some) {
                        fetch(m, reload(b.clone(), m));
                    }
                }
            },
            Err(e) => say(m, &failed_line(&e)),
        }
    });
}

/// The presses for what's picked, and for the folder that's open.
fn action_bar(b: Bridge, m: Drive) -> impl IntoElement {
    dynamic(move || {
        if m.acting.with(Option::is_some) {
            return view();
        }
        let adds = m.here_adds.get() && m.search.with(Option::is_none);
        let rows = picked_rows(m);
        let offered = offer(&rows);
        let nothing = offered == Offer::default();
        if !adds && nothing && rows.iter().all(|r| r.trashed) {
            return view();
        }
        let name = match rows.as_slice() {
            [one] => one.name.clone(),
            many => format!("{} items", many.len()),
        };
        let mut bar = view().app_class(Some(
            "flex-row flex-wrap items-center gap-1 px-2 py-1".into(),
        ));
        if adds {
            let file = b.clone();
            bar = bar
                .child(press("New folder", "", move || {
                    m.typed.set(String::new());
                    m.said.set(None);
                    start_acting(m, Acting::NewFolder);
                }))
                // fewer words than its label: the AI mustn't open the picker, as
                // what's chosen there goes up at the person's press
                .child(press_as(
                    "Upload a file to Google Drive",
                    "Upload".to_owned(),
                    "",
                    move || pick(&file, m),
                ));
        }
        if offered.rename {
            bar = bar.child(press("Rename", &name, move || start_rename(m)));
        }
        if offered.shift {
            let (b, row) = (b.clone(), rows[0].clone());
            bar = bar.child(press("Move", &name, move || {
                mover::open(&b, m, row.clone());
            }));
        }
        if offered.share {
            let (b, row) = (b.clone(), rows[0].clone());
            bar = bar.child(press("Share", &name, move || {
                share::open(&b, m, row.clone());
            }));
        }
        if offered.trash {
            let b = b.clone();
            let shows = if rows.len() == 1 {
                "Move to trash".to_owned()
            } else {
                format!("Move {} to trash", rows.len())
            };
            bar = bar.child(press_as("Move to trash", shows, &name, move || {
                trash(&b, m);
            }));
        }
        if nothing && !rows.is_empty() {
            bar = bar.child(
                text("You can't change this here.")
                    .app_class(Some(format!("text-[11px] {MUTE}").into())),
            );
        }
        view()
            .app_class(Some(format!("border-b {LINE}").into()))
            .child(scroll().app_class(Some("max-h-[96px]".into())).child(bar))
    })
}

/// Everything above the rows that changes Drive.
pub(super) fn bars(b: Bridge, m: Drive) -> impl IntoElement {
    let (named, going, back) = (b.clone(), b.clone(), b.clone());
    view()
        .child(action_bar(b, m))
        .child(name_line(m, named))
        .child(sending_list(going, m))
        .child(undo_bar(back, m))
}

/// What takes the rows' room: a folder to choose, or who's to have the file.
pub(super) fn panel(b: Bridge, m: Drive) -> impl IntoElement {
    let which = computed(move || m.acting.get());
    dynamic(move || match which.get() {
        Some(Acting::Move(row)) => mover::pane(b.clone(), m, row),
        Some(Acting::Share(row)) => share::pane(b.clone(), m, row),
        _ => view(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, can: &[&str]) -> Row {
        Row {
            id: id.to_owned(),
            can: Some(can.iter().map(|c| (*c).to_owned()).collect()),
            ..Row::default()
        }
    }

    const ALL: [&str; 4] = ["rename", "move", "share", "trash"];

    #[test]
    fn a_file_read_only_offers_nothing_and_one_of_many_stops_the_trash() {
        assert_eq!(offer(&[]), Offer::default());
        assert_eq!(offer(&[row("a", &[])]), Offer::default());
        let mine = offer(&[row("a", &ALL)]);
        assert!(mine.rename && mine.shift && mine.share && mine.trash);
        let both = offer(&[row("a", &ALL), row("b", &ALL)]);
        assert_eq!(
            both,
            Offer {
                trash: true,
                ..Offer::default()
            },
            "a rename or a move is for one file"
        );
        assert_eq!(offer(&[row("a", &ALL), row("b", &[])]), Offer::default());
    }

    #[test]
    fn each_press_goes_by_its_own_capability() {
        let only = |what: &str| offer(&[row("a", &[what])]);
        assert!(only("rename").rename && !only("rename").trash);
        assert!(only("move").shift && !only("move").rename);
        assert!(only("share").share && !only("share").shift);
        assert!(only("trash").trash && !only("trash").share);
    }

    #[test]
    fn what_is_in_the_trash_has_no_presses() {
        let mut gone = row("a", &ALL);
        gone.trashed = true;
        assert_eq!(offer(&[gone]), Offer::default());
    }

    #[test]
    fn trashing_says_how_many() {
        assert_eq!(trashed_line(1), "Moved to Trash.");
        assert_eq!(trashed_line(3), "Moved 3 items to Trash.");
    }
}
