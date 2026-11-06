//! Drive: a side column, a header and a folder as a list or a grid, with a
//! file's details beside them. A press on a file opens it in the Bot's viewer;
//! `edits.rs` has that and what comes back from it. `write.rs`, `mover.rs` and
//! `share.rs` have the presses that change Drive. The details stay a press away,
//! on each row's own Details.
//!
//! The label rule is Mail's (see the top of `mail.rs`): a press or a row has a
//! fixed `label`, and what's the person's own goes in `accessibility_value` and
//! the row's texts, so no file name, owner or path reaches `ui_look`.

mod card;
mod detail;
mod edits;
mod file;
mod list;
mod mover;
mod share;
mod write;

use std::collections::HashSet;
use std::future::Future;

use buwiz_app::{Bridge, Placement};
use nsh_core::AccessibilityValue;
use nsh_core::element::{Root, input};
use nsh_core::geometry::Percentage;
use nsh_core::keymap::When;
use nsh_core::prelude::*;
use serde_json::{Value, json};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

use crate::compose::press_text;
use crate::shell::{
    Account, BG, CHOSEN, DIM, INK, Job, LINE, Latest, MUTE, PANEL, POLL, RAIL, account_of, line_of,
    read, sleep,
};
use edits::{Said, load_edits};
use file::{Crumb, Edit, Facts, Preview, Row, path_in, rows_in, storage_line};

/// How wide the screen must be for the side column to fit beside the rows.
/// Under this its presses go in a row of their own, so none is lost.
const SIDE_FROM: f32 = 640.0;
/// How wide the screen must be for the details to fit beside the rows. Under
/// this they cover them.
const DETAILS_FROM: f32 = 900.0;
/// How near the last row a keyboard move asks for the next page.
const AHEAD: usize = 20;
const SIGN_IN_AGAIN: &str =
    "Your Google sign-in stopped working. Sign in again on this app's page.";

/// Every view, as `gws_drive_list` and the column name them.
const VIEWS: [(&str, &str); 5] = [
    ("my", "My Drive"),
    ("shared", "Shared"),
    ("recent", "Recent"),
    ("starred", "Starred"),
    ("trash", "Trash"),
];

#[derive(Clone, Copy, PartialEq)]
enum Layout {
    List,
    Grid,
}

/// What a list of results came from: typed here, or a chat's, by its ticket.
#[derive(Clone, PartialEq)]
enum Search {
    Query(String),
    Ticket(String),
}

/// What's known of the open file past its row.
#[derive(Clone, PartialEq)]
enum FactsState {
    Reading,
    Known(Facts),
    Missing(String),
}

/// What the screen is showing right now.
#[derive(Clone, Copy)]
struct Drive {
    view: Signal<&'static str>,
    folder: Signal<Option<String>>,
    search: Signal<Option<Search>>,
    query: Signal<String>,
    path: Signal<Vec<Crumb>>,
    rows: Signal<Vec<Row>>,
    more: Signal<bool>,
    saved: Signal<bool>,
    status: Signal<String>,
    loading: Signal<bool>,
    /// Why the list stopped short, in words for the person.
    problem: Signal<Option<String>>,
    list_turn: Latest,
    open_turn: Latest,
    layout: Signal<Layout>,
    /// How wide the rows really are, which says what fits in them.
    width: Signal<f32>,
    /// The first and last row on screen.
    seen: Signal<(usize, usize)>,
    /// The row the keys move, by id: rows move as files come and go.
    cursor: Signal<Option<String>>,
    /// Where a Shift move started.
    anchor: Signal<Option<String>>,
    picked: Signal<HashSet<String>>,
    /// The file in the details pane.
    open: Signal<Option<Row>>,
    /// What's open on this Mac and edited, by what `gws_file_edits` said last.
    edits: Signal<Vec<Edit>>,
    /// Files a press is working on, by id.
    pressed: Signal<HashSet<String>>,
    /// What became of the last press on a file.
    said: Signal<Option<Said>>,
    /// The file whose Discard is being asked about.
    discarding: Signal<Option<String>>,
    file_turn: Latest,
    facts: Signal<FactsState>,
    preview: Signal<Preview>,
    storage: Signal<Option<String>>,
    account: Signal<Account>,
    seq: Signal<i64>,
    /// What's being renamed, moved, shared or made.
    acting: Signal<Option<write::Acting>>,
    /// The name being typed for that.
    typed: Signal<String>,
    /// A write is on its way, so nothing presses it twice.
    writing: Signal<bool>,
    /// What can be undone, and what to say about it.
    undo: Signal<Option<(String, String)>>,
    /// The undo already pressed, while it waits for its answer.
    undoing: Signal<Option<String>>,
    /// The list is one folder, and Google lets a new file go in it.
    here_adds: Signal<bool>,
    /// That folder's id, as a file's parents say it.
    here: Signal<Option<String>>,
    /// Files picked for upload and still to land.
    sending: Signal<Vec<write::Sending>>,
    next_send: Signal<u64>,
    mover: mover::Mover,
    sharing: share::Sharing,
    /// Reads started by a press, each run on its own: what they were pressed on
    /// can be rebuilt, and a task dies with its view.
    reads: Signal<UnboundedSender<Job>>,
    search_box: ElementRef,
    list: ElementRef,
}

fn value(text: impl Into<String>) -> Option<AccessibilityValue> {
    Some(AccessibilityValue {
        text: Some(text.into().into()),
        ..AccessibilityValue::default()
    })
}

fn fetch(m: Drive, job: impl Future<Output = ()> + 'static) {
    m.reads
        .with_untracked(|reads| drop(reads.send(Box::pin(job))));
}

/// The words of a refusal that came as an answer.
fn refused(v: &Value) -> Option<String> {
    v.get("refused")?;
    let why = v.get("message").and_then(Value::as_str);
    Some(why.unwrap_or("That didn't work.").to_owned())
}

/// A read that takes a refusal as the failure it is.
async fn ask(b: &Bridge, tool: &str, args: Value) -> Result<Value, String> {
    let v = read(b, tool, args).await?;
    refused(&v).map_or(Ok(v), Err)
}

/// The tool and arguments for what the list shows now.
fn list_call(
    view: &str,
    folder: Option<String>,
    search: Option<Search>,
    skip: usize,
) -> (&'static str, Value) {
    match search {
        Some(Search::Query(query)) => ("gws_search_drive", json!({"query": query})),
        Some(Search::Ticket(ticket)) => ("gws_drive_list", json!({"search": ticket, "skip": skip})),
        None => {
            let mut args = json!({"view": view, "skip": skip});
            if let Some(folder) = folder {
                args["folder"] = json!(folder);
            }
            ("gws_drive_list", args)
        }
    }
}

async fn read_list(b: &Bridge, m: Drive, skip: usize) -> Result<Value, String> {
    let (tool, args) = list_call(
        m.view.get_untracked(),
        m.folder.get_untracked(),
        m.search.get_untracked(),
        skip,
    );
    ask(b, tool, args).await
}

/// The rows of an answer, whichever tool gave it.
fn rows_of(v: &Value) -> Vec<Row> {
    let key = if v.get("items").is_some() {
        "items"
    } else {
        "files"
    };
    rows_in(v, key)
}

/// A page's rows the list doesn't hold yet: files moving between pages can
/// bring one back, and each row's id must stay unique.
fn new_rows(have: &[Row], page: Vec<Row>) -> Vec<Row> {
    let have: HashSet<&str> = have.iter().map(|r| r.id.as_str()).collect();
    page.into_iter()
        .filter(|r| !have.contains(r.id.as_str()))
        .collect()
}

fn show_answer(m: Drive, v: &Value) {
    m.more.set(v.get("more") == Some(&Value::Bool(true)));
    m.saved.set(v.get("saved") == Some(&Value::Bool(true)));
    if m.search.with_untracked(Option::is_none) && v.get("path").is_some() {
        m.path.set(path_in(v));
    }
    m.here_adds
        .set(v.get("can_add") == Some(&Value::Bool(true)));
    m.here
        .set(v.get("at").and_then(Value::as_str).map(str::to_owned));
    m.problem.set(None);
}

/// A read of the list failed. What's on screen stays: the person can still use
/// it, so only an empty list goes to the sign-in line.
fn list_failed(m: Drive, why: String) {
    let n = m.rows.with_untracked(Vec::len);
    if n == 0 {
        if why == "sign-in" {
            m.account.set(Account::SignedOut);
        } else {
            m.problem.set(Some(why));
        }
        return;
    }
    let why = if why == "sign-in" {
        SIGN_IN_AGAIN.to_owned()
    } else {
        why
    };
    let items = if n == 1 {
        "the 1 item".to_owned()
    } else {
        format!("the {n} items")
    };
    m.problem
        .set(Some(format!("Showing {items} already here. {why}")));
}

async fn load(b: Bridge, m: Drive, skip: usize) {
    let turn = m.list_turn.ask();
    m.loading.set(true);
    let answer = read_list(&b, m, skip).await;
    // a later load owns the list and the flag now
    if !m.list_turn.counts(turn) {
        return;
    }
    m.loading.set(false);
    match answer {
        Ok(v) => {
            let rows = rows_of(&v);
            if skip > 0 {
                m.rows.update(|all| {
                    let mut fresh = new_rows(all, rows);
                    all.append(&mut fresh);
                });
            } else {
                m.rows.set(rows);
            }
            show_answer(m, &v);
        }
        Err(e) => list_failed(m, e),
    }
}

/// The next page, unless one is on its way. The flag goes up here: two calls
/// in one moment would both ask for the same rows.
fn next_page(b: &Bridge, m: Drive) {
    if m.more.get_untracked() && !m.loading.get_untracked() {
        m.loading.set(true);
        fetch(m, load(b.clone(), m, m.rows.with_untracked(Vec::len)));
    }
}

/// The list read again where it stands, as far down as it had got.
async fn reload(b: Bridge, m: Drive) {
    let turn = m.list_turn.ask();
    let reach = m.rows.with_untracked(Vec::len);
    m.loading.set(true);
    let mut rows: Vec<Row> = Vec::new();
    let last = loop {
        let answer = read_list(&b, m, rows.len()).await;
        if !m.list_turn.counts(turn) {
            return;
        }
        let v = match answer {
            Ok(v) => v,
            Err(e) => {
                m.loading.set(false);
                list_failed(m, e);
                return;
            }
        };
        let fresh = new_rows(&rows, rows_of(&v));
        let page = fresh.len();
        rows.extend(fresh);
        if page == 0 || rows.len() >= reach || v.get("more") != Some(&Value::Bool(true)) {
            break v;
        }
    };
    m.loading.set(false);
    // a pick that left the list is one nobody can see
    m.picked
        .update(|p| p.retain(|id| rows.iter().any(|r| r.id == *id)));
    if m.cursor
        .with_untracked(|c| c.as_ref().is_some_and(|c| !rows.iter().any(|r| r.id == *c)))
    {
        m.cursor.set(None);
        m.anchor.set(None);
    }
    m.rows.set(rows);
    show_answer(m, &last);
}

/// Picks up where a failed read stopped.
fn try_again(b: &Bridge, m: Drive) {
    m.problem.set(None);
    if m.more.get_untracked() {
        next_page(b, m);
    } else {
        fetch(m, reload(b.clone(), m));
    }
}

async fn load_about(b: Bridge, m: Drive) {
    if let Ok(v) = ask(&b, "gws_drive_about", json!({})).await {
        m.storage.set(storage_line(&v));
    }
}

async fn watch(b: Bridge, m: Drive) {
    loop {
        if let Ok(v) = read(&b, "gws_drive_changes", json!({})).await {
            m.status.set(
                v.get("status")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            );
            let seq = v.get("seq").and_then(Value::as_i64).unwrap_or(0);
            let last = m.seq.get_untracked();
            m.seq.set(seq);
            if last >= 0 && seq != last {
                reload(b.clone(), m).await;
                load_about(b.clone(), m).await;
            }
        }
        // the viewer saves in place and says nothing, so this is how an edit shows
        load_edits(b.clone(), m).await;
        sleep(POLL).await;
    }
}

/// Closes the open file, and any answer on its way for it.
fn close_details(m: Drive) {
    m.open_turn.ask();
    m.open.set(None);
}

/// A new list: what's open and picked belongs to the old one, and its rows
/// mustn't sit under the new one's path while the answer comes.
fn start_over(b: &Bridge, m: Drive) {
    // now, not when the load's turn comes: an old answer could land between
    m.list_turn.ask();
    close_details(m);
    // a name being typed for a folder is for the folder that's going
    m.acting.set(None);
    m.here_adds.set(false);
    m.here.set(None);
    m.rows.set(Vec::new());
    m.more.set(false);
    m.saved.set(false);
    m.problem.set(None);
    m.picked.set(HashSet::new());
    m.cursor.set(None);
    m.anchor.set(None);
    m.seen.set((0, 0));
    // an emptied list keeps its offset, which would leave the new rows above it
    m.list.scroll_to_offset(px(0.0), false);
    m.loading.set(true);
    fetch(m, load(b.clone(), m, 0));
}

fn go_to(b: &Bridge, m: Drive, view: &'static str, folder: Option<String>, path: Vec<Crumb>) {
    m.view.set(view);
    m.folder.set(folder);
    m.path.set(path);
    m.search.set(None);
    m.query.set(String::new());
    start_over(b, m);
}

/// The view a folder opens in: `folder` narrows only My Drive and Shared.
fn home_of(view: &'static str, row: &Row) -> &'static str {
    match view {
        "my" | "shared" => view,
        _ if row.mine => "my",
        _ => "shared",
    }
}

fn enter_folder(b: &Bridge, m: Drive, row: &Row) {
    let mut path = if m.search.with_untracked(Option::is_none) {
        m.path.get_untracked()
    } else {
        Vec::new()
    };
    path.push(Crumb {
        id: row.id.clone(),
        name: row.name.clone(),
    });
    let view = home_of(m.view.get_untracked(), row);
    go_to(b, m, view, Some(row.id.clone()), path);
}

async fn read_facts(b: Bridge, m: Drive, id: String, turn: u64) {
    let answer = ask(&b, "gws_drive_file", json!({"id": id})).await;
    // the person opened another, or left: this one must not replace it
    if !m.open_turn.counts(turn) {
        return;
    }
    m.facts.set(match answer {
        Ok(v) => FactsState::Known(Facts::of(&v)),
        Err(e) if e == "sign-in" => FactsState::Missing(SIGN_IN_AGAIN.to_owned()),
        Err(e) => FactsState::Missing(e),
    });
}

async fn read_preview(b: Bridge, m: Drive, id: String, turn: u64) {
    let answer = ask(&b, "gws_drive_preview", json!({"id": id})).await;
    if !m.open_turn.counts(turn) {
        return;
    }
    m.preview.set(match answer {
        Ok(v) => Preview::of(&v),
        Err(e) if e == "sign-in" => Preview::Reason(SIGN_IN_AGAIN.to_owned()),
        Err(e) => Preview::Reason(e),
    });
}

fn show_details(b: &Bridge, m: Drive, row: Row) {
    let turn = m.open_turn.ask();
    let id = row.id.clone();
    m.facts.set(FactsState::Reading);
    m.preview.set(Preview::Reading);
    m.open.set(Some(row));
    // each on its own: a slow preview mustn't hold up who has access
    fetch(m, read_facts(b.clone(), m, id.clone(), turn));
    fetch(m, read_preview(b.clone(), m, id, turn));
}

/// A folder goes in and a file opens. What's in Trash has no way in until it's
/// restored, so it shows its details.
fn open_row(b: &Bridge, m: Drive, row: Row) {
    if row.trashed {
        show_details(b, m, row);
    } else if row.is_folder() {
        enter_folder(b, m, &row);
    } else {
        edits::open_file(b, m, row);
    }
}

fn pick_only(m: Drive, id: String) {
    m.cursor.set(Some(id.clone()));
    m.anchor.set(Some(id.clone()));
    m.picked.set(HashSet::from([id]));
}

fn open_at(b: &Bridge, m: Drive, index: usize) {
    let Some(row) = m.rows.with_untracked(|r| r.get(index).cloned()) else {
        return;
    };
    pick_only(m, row.id.clone());
    open_row(b, m, row);
}

fn details_at(b: &Bridge, m: Drive, index: usize) {
    let Some(row) = m.rows.with_untracked(|r| r.get(index).cloned()) else {
        return;
    };
    pick_only(m, row.id.clone());
    show_details(b, m, row);
}

/// Where a move of `by` from `at` lands, `across` to a line. A move down from
/// the line before a short last one lands on its end, and off the list is no
/// move at all.
fn moved(at: usize, by: isize, len: usize, across: usize) -> usize {
    match at.checked_add_signed(by) {
        None => at,
        Some(to) if to < len => to,
        Some(_) if by > 1 && (len - 1) / across > at / across => len - 1,
        Some(_) => at,
    }
}

/// Keeps the row on screen, moving the list only when it must: the last row
/// counted may be showing just a sliver.
fn reveal(m: Drive, at: usize) {
    let (first, last) = m.seen.get_untracked();
    if at <= first {
        m.list
            .scroll_to_index(at as u32, Percentage(0.0), px(0.0), false);
    } else if at >= last {
        m.list
            .scroll_to_index(at as u32, Percentage(1.0), px(0.0), false);
    }
}

fn step(b: &Bridge, m: Drive, by: isize, extend: bool) {
    let len = m.rows.with_untracked(Vec::len);
    if len == 0 {
        return;
    }
    let index_of = |id: Option<String>| {
        let id = id?;
        m.rows.with_untracked(|r| r.iter().position(|r| r.id == id))
    };
    let here = index_of(m.cursor.get_untracked());
    let across = across(m);
    let to = here.map_or(0, |at| moved(at, by, len, across));
    let id = m.rows.with_untracked(|r| r[to].id.clone());
    if extend {
        let from = index_of(m.anchor.get_untracked()).or(here).unwrap_or(to);
        let (low, high) = (from.min(to), from.max(to));
        let range = m
            .rows
            .with_untracked(|r| r[low..=high].iter().map(|r| r.id.clone()).collect());
        let start = m.rows.with_untracked(|r| r[from].id.clone());
        m.anchor.set(Some(start));
        m.cursor.set(Some(id));
        m.picked.set(range);
    } else {
        pick_only(m, id);
    }
    reveal(m, to);
    if to + AHEAD >= len {
        next_page(b, m);
    }
}

/// How many rows a move up or down crosses.
fn across(m: Drive) -> usize {
    match m.layout.get_untracked() {
        Layout::List => 1,
        Layout::Grid => list::cards_across(m.width.get_untracked()),
    }
}

/// The details are over the rows, or something is being done to them, so the
/// keys have nothing to move.
fn rows_covered(m: Drive) -> bool {
    m.acting.with_untracked(Option::is_some)
        || (m.open.with_untracked(Option::is_some) && window().width.0 < DETAILS_FROM)
}

fn keys(b: Bridge, m: Drive) {
    // never while typing in a field, where these keys are the field's own
    let options = KeyOptions {
        when: When::NotEditing,
        repeat: true,
    };
    let free = move || {
        matches!(
            m.account.get_untracked(),
            Account::Covered | Account::Unknown
        ) && !rows_covered(m)
    };
    for (key, sideways, sign) in [
        ("up", false, -1),
        ("down", false, 1),
        ("left", true, -1),
        ("right", true, 1),
    ] {
        for extend in [false, true] {
            let b = b.clone();
            let stroke = if extend {
                format!("shift+{key}")
            } else {
                key.to_owned()
            };
            on_key_with(&stroke, options, move |_| {
                let grid = m.layout.get_untracked() == Layout::Grid;
                if !free() || (sideways && !grid) {
                    return;
                }
                let by = if sideways { 1 } else { across(m) as isize };
                step(&b, m, by * sign, extend);
            });
        }
    }
    let chosen = move || {
        m.cursor.get_untracked().and_then(|id| {
            m.rows
                .with_untracked(|r| r.iter().find(|r| r.id == id).cloned())
        })
    };
    let (details, back) = (b.clone(), b.clone());
    on_key_with("enter", options, move |_| {
        if let Some(row) = chosen().filter(|_| free()) {
            open_row(&b, m, row);
        }
    });
    on_key_with("i", options, move |_| {
        if let Some(row) = chosen().filter(|_| free()) {
            show_details(&details, m, row);
        }
    });
    on_key_with("/", options, move |_| {
        if free() {
            m.search_box.focus();
        }
    });
    let once = KeyOptions {
        when: When::NotEditing,
        repeat: false,
    };
    on_key_with("f2", once, move |_| {
        if free() {
            write::start_rename(m);
        }
    });
    on_key_with("cmd+z", once, move |_| {
        if free() {
            write::undo(&back, m);
        }
    });
}

fn view_press(b: Bridge, m: Drive, id: &'static str, name: &'static str) -> View {
    view()
        .app_class(move || {
            let on = m.view.get() == id && m.search.with(Option::is_none);
            Some(format!("px-3 py-1 rounded-md {}", if on { CHOSEN } else { "" }).into())
        })
        .label(name)
        .on_press(move || go_to(&b, m, id, None, Vec::new()))
        .child(text(name).app_class(Some(format!("text-[13px] {INK}").into())))
}

fn storage_text(m: Drive) -> impl IntoElement {
    text(move || m.storage.get().unwrap_or_default())
        .app_class(Some(format!("text-[12px] {MUTE}").into()))
}

fn side(b: Bridge, m: Drive) -> View {
    let list = VIEWS.iter().fold(
        view().app_class(Some("gap-[1px] p-2".into())),
        |list, (id, name)| list.child(view_press(b.clone(), m, id, name)),
    );
    view()
        .app_class(Some(format!("w-[224px] border-r {LINE} {RAIL}").into()))
        .child(scroll().app_class(Some("flex-1".into())).child(list))
        .child(
            view()
                .app_class(Some(format!("px-4 py-2 border-t {LINE}").into()))
                .child(storage_text(m)),
        )
}

/// The column's presses when it doesn't fit, each free to wrap below the last.
fn side_row(b: Bridge, m: Drive) -> View {
    let list = VIEWS.iter().fold(
        view().app_class(Some(
            format!("flex-row flex-wrap items-center gap-[1px] px-1 py-1 border-b {LINE} {RAIL}")
                .into(),
        )),
        |list, (id, name)| list.child(view_press(b.clone(), m, id, name)),
    );
    list.child(view().app_class(Some("px-3".into())).child(storage_text(m)))
}

fn crumb(line: String, press: impl Fn() + 'static) -> View {
    view()
        .app_class(Some("max-w-full px-1 py-[2px]".into()))
        .label("Open this folder")
        .accessibility_value(value(line.clone()))
        .on_press(press)
        .child(
            text(line)
                .number_of_lines(1)
                .app_class(Some(format!("text-[12px] {INK}").into())),
        )
}

fn divider() -> Text {
    text(">").app_class(Some(format!("text-[12px] {DIM}").into()))
}

/// The way here, every part a press. A deep path wraps, and past a few lines
/// scrolls, so it can't take the rows' room.
fn breadcrumb(b: Bridge, m: Drive) -> impl IntoElement {
    let parts = dynamic(move || {
        let view_id = m.view.get();
        let name = VIEWS
            .iter()
            .find(|(id, _)| *id == view_id)
            .map_or("My Drive", |(_, name)| name);
        let path = m.path.get();
        let root = b.clone();
        let line = view()
            .app_class(Some(
                "flex-row flex-wrap items-center gap-x-1 px-2 py-1".into(),
            ))
            .child(crumb(name.to_owned(), move || {
                go_to(&root, m, view_id, None, Vec::new());
            }));
        if m.search.with(Option::is_some) {
            return line.child(divider()).child(
                text("Search results").app_class(Some(format!("text-[12px] {MUTE}").into())),
            );
        }
        let mut way = Vec::new();
        path.into_iter().fold(line, |line, part| {
            way.push(part.clone());
            let (b, kept) = (b.clone(), way.clone());
            let (id, name) = (part.id, part.name);
            line.child(divider()).child(crumb(name, move || {
                go_to(&b, m, view_id, Some(id.clone()), kept.clone());
            }))
        })
    });
    view()
        .app_class(Some(format!("border-b {LINE}").into()))
        .child(scroll().app_class(Some("max-h-[64px]".into())).child(parts))
}

fn layout_press(m: Drive, layout: Layout, name: &'static str) -> View {
    view()
        .app_class(move || {
            let on = m.layout.get() == layout;
            Some(format!("px-3 py-1 rounded-md {}", if on { CHOSEN } else { "" }).into())
        })
        .label(name)
        .on_press(move || m.layout.set(layout))
        .child(text(name).app_class(Some(format!("text-[12px] {INK}").into())))
}

/// Search, then List and Grid, which wrap below it on a narrow screen.
fn header(b: Bridge, m: Drive) -> impl IntoElement {
    view()
        .app_class(Some(
            format!("flex-row flex-wrap items-center gap-2 px-2 py-1 border-b {LINE}").into(),
        ))
        .child(
            input()
                .element_ref(&m.search_box)
                .label("Search Google Drive")
                .value(move || m.query.get())
                .placeholder("Search Google Drive".to_owned())
                .app_class(Some(
                    format!(
                        "flex-1 min-w-[80px] rounded-lg border px-2.5 py-1 text-[13px] {BG} {LINE} {INK}"
                    )
                    .into(),
                ))
                .on_change(move |value: &str| m.query.set(value.to_owned()))
                .on_submit(move |query: &str| {
                    let query = query.trim();
                    // Return in an empty field with no search on isn't a request
                    if query.is_empty() && m.search.with_untracked(Option::is_none) {
                        return;
                    }
                    m.search
                        .set((!query.is_empty()).then(|| Search::Query(query.to_owned())));
                    start_over(&b, m);
                }),
        )
        .child(layout_press(m, Layout::List, "List"))
        .child(layout_press(m, Layout::Grid, "Grid"))
}

/// The line under the header when the list isn't simply live.
fn notice_line(status: &str, saved: bool) -> Option<&'static str> {
    match (status, saved) {
        (_, true) | ("offline", _) => Some("Offline. Showing your saved files."),
        ("needs_access", _) => {
            Some("Your Google sign-in needs more access. Sign in again on this app's page.")
        }
        ("failed", _) => Some("Couldn't update your files. Showing what's saved."),
        _ => None,
    }
}

fn notice(b: Bridge, m: Drive) -> impl IntoElement {
    dynamic(move || {
        if let Some(line) = m.problem.get() {
            let b = b.clone();
            return view()
                .app_class(Some(
                    format!("flex-row flex-wrap items-center gap-1 px-3 py-1 border-b {LINE}")
                        .into(),
                ))
                .child(text(line).app_class(Some(
                    format!("flex-1 min-w-[80px] text-[11px] {INK}").into(),
                )))
                .child(press_text("Try again", "Try again", move || {
                    try_again(&b, m);
                }));
        }
        match notice_line(&m.status.get(), m.saved.get()) {
            Some(line) => view()
                .app_class(Some(format!("px-3 py-1 border-b {LINE}").into()))
                .child(text(line).app_class(Some(format!("text-[11px] {MUTE}").into()))),
            None => view(),
        }
    })
}

/// The rows and, when a file is open, its details: beside them, or over them.
/// Over them the rows are dropped, not hidden: `hidden` leaves a list's cells
/// where they were. `seen` brings the person back to the same place.
fn rows_and_details(b: Bridge, m: Drive) -> impl IntoElement {
    let beside = computed(|| window().width.0 >= DETAILS_FROM);
    let open = computed(move || m.open.with(Option::is_some));
    let panel = computed(move || m.acting.with(write::Acting::panel));
    let covered = computed(move || panel.get() || (open.get() && !beside.get()));
    let over = b.clone();
    view()
        .app_class(Some("flex-1 flex-row min-h-0".into()))
        .child(dynamic(move || {
            if !panel.get() {
                return view();
            }
            view()
                .app_class(Some("flex-1 min-w-0".into()))
                .child(write::panel(over.clone(), m))
        }))
        .child(dynamic(move || {
            if covered.get() {
                return view();
            }
            view()
                .app_class(Some("flex-1 min-w-0".into()))
                .on_layout(
                    move |bounds: &nsh_core::geometry::Bounds<nsh_core::geometry::Points>| {
                        m.width.set(bounds.size.width.0);
                    },
                )
                .child(list::body(b.clone(), m))
        }))
        .child(dynamic(move || {
            if !open.get() || panel.get() {
                return view();
            }
            let place = if beside.get() {
                format!("w-[320px] border-l {LINE}")
            } else {
                "flex-1 min-w-0".to_owned()
            };
            view().app_class(Some(place.into())).child(detail::pane(m))
        }))
}

fn screen(b: Bridge, m: Drive) -> impl IntoElement {
    let wide = computed(|| window().width.0 >= SIDE_FROM);
    let (top, trail, head, line, body) = (b.clone(), b.clone(), b.clone(), b.clone(), b.clone());
    let (said, edited, writes) = (b.clone(), b.clone(), b.clone());
    let main = view()
        .app_class(Some(format!("flex-1 min-w-0 {PANEL}").into()))
        .child(dynamic(move || {
            if wide.get() {
                view()
            } else {
                side_row(top.clone(), m)
            }
        }))
        .child(header(head, m))
        .child(breadcrumb(trail, m))
        .child(notice(line, m))
        .child(edits::said_line(said, m))
        .child(edits::bars(edited, m))
        .child(write::bars(writes, m))
        .child(rows_and_details(body, m));
    view()
        .app_class(Some("flex-1 flex-row".into()))
        .child(dynamic(move || {
            if wide.get() {
                side(b.clone(), m)
            } else {
                view()
            }
        }))
        .child(main)
}

/// The account first. Only then does anything load.
async fn begin(b: Bridge, m: Drive) {
    match read(&b, "gws_status", json!({})).await {
        Ok(status) => m.account.set(account_of(&status, "Drive")),
        Err(e) if e == "sign-in" => m.account.set(Account::SignedOut),
        Err(e) => m.account.set(Account::Failed(e)),
    }
    if !matches!(
        m.account.get_untracked(),
        Account::Covered | Account::Unknown
    ) {
        return;
    }
    fetch(m, load(b.clone(), m, 0));
    fetch(m, load_about(b.clone(), m));
    fetch(m, watch(b, m));
}

pub fn build(bridge: Bridge) -> Root {
    buwiz_app::root(bridge, move |b| {
        if b.viewport_now().placement == Placement::Inline {
            return crate::shell::card_frame().child(card::files(b));
        }
        let props = b.props();
        let prop = |k: &str| props.get(k).and_then(Value::as_str).map(str::to_owned);
        let (reads, mut asked) = unbounded_channel::<Job>();
        let opened = prop("view");
        let start = VIEWS
            .iter()
            .find(|(id, _)| Some(*id) == opened.as_deref())
            .map_or("my", |(id, _)| id);
        let m = Drive {
            view: signal(start),
            folder: signal(prop("folder")),
            search: signal(prop("search").map(Search::Ticket)),
            query: signal(String::new()),
            path: signal(Vec::new()),
            rows: signal(Vec::new()),
            more: signal(false),
            saved: signal(false),
            status: signal(String::new()),
            loading: signal(true),
            problem: signal(None),
            list_turn: Latest::new(),
            open_turn: Latest::new(),
            layout: signal(Layout::List),
            width: signal(0.0),
            seen: signal((0, 0)),
            cursor: signal(None),
            anchor: signal(None),
            picked: signal(HashSet::new()),
            open: signal(None),
            edits: signal(Vec::new()),
            pressed: signal(HashSet::new()),
            said: signal(None),
            discarding: signal(None),
            acting: signal(None),
            typed: signal(String::new()),
            writing: signal(false),
            undo: signal(None),
            undoing: signal(None),
            here_adds: signal(false),
            here: signal(None),
            sending: signal(Vec::new()),
            next_send: signal(0),
            mover: mover::Mover::new(),
            sharing: share::Sharing::new(),
            file_turn: Latest::new(),
            facts: signal(FactsState::Reading),
            preview: signal(Preview::Reading),
            storage: signal(None),
            account: signal(Account::Loading),
            seq: signal(-1),
            reads: signal_with(reads, SignalEquals::Never),
            search_box: element_ref(),
            list: element_ref(),
        };
        keys(b.clone(), m);
        let spawner = b.clone();
        b.spawn(async move {
            while let Some(job) = asked.recv().await {
                spawner.spawn(job);
            }
        });
        b.spawn(begin(b.clone(), m));
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
                        text(line_of(account, "Drive"))
                            .app_class(Some(format!("text-[14px] {MUTE}").into())),
                    )
            }))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_move_stays_on_the_list_and_down_finds_a_short_last_line() {
        assert_eq!(moved(0, -1, 10, 1), 0);
        assert_eq!(moved(9, 1, 10, 1), 9);
        assert_eq!(moved(4, 1, 10, 1), 5);
        // four across, ten cards: the last line holds two
        assert_eq!(moved(1, 4, 10, 4), 5);
        assert_eq!(moved(7, 4, 10, 4), 9, "to the end of the short line");
        assert_eq!(moved(9, 4, 10, 4), 9, "already on the last line");
        assert_eq!(moved(2, -4, 10, 4), 2, "no line above");
    }

    #[test]
    fn a_folder_opens_in_a_view_that_can_hold_it() {
        let row = |mine| Row {
            mine,
            ..Row::default()
        };
        assert_eq!(home_of("my", &row(true)), "my");
        assert_eq!(home_of("shared", &row(false)), "shared");
        assert_eq!(home_of("recent", &row(true)), "my");
        assert_eq!(home_of("starred", &row(false)), "shared");
    }

    #[test]
    fn a_search_outranks_a_view_and_only_a_view_has_a_folder() {
        let (tool, args) = list_call("recent", None, None, 100);
        assert_eq!(tool, "gws_drive_list");
        assert_eq!(args, json!({"view": "recent", "skip": 100}));
        let (_, args) = list_call("my", Some("f1".to_owned()), None, 0);
        assert_eq!(args, json!({"view": "my", "folder": "f1", "skip": 0}));
        let (tool, args) = list_call(
            "my",
            Some("f1".to_owned()),
            Some(Search::Query("plan".to_owned())),
            0,
        );
        assert_eq!(tool, "gws_search_drive");
        assert_eq!(args, json!({"query": "plan"}));
        let (tool, args) = list_call("my", None, Some(Search::Ticket("t".to_owned())), 100);
        assert_eq!(tool, "gws_drive_list");
        assert_eq!(args, json!({"search": "t", "skip": 100}));
    }

    #[test]
    fn the_notice_says_what_a_person_can_use() {
        assert_eq!(notice_line("ok", false), None);
        assert_eq!(notice_line("", false), None);
        assert!(notice_line("ok", true).unwrap().contains("saved files"));
        assert!(
            notice_line("offline", false)
                .unwrap()
                .contains("saved files")
        );
        assert!(
            notice_line("needs_access", false)
                .unwrap()
                .contains("Sign in again")
        );
    }

    #[test]
    fn a_page_adds_only_rows_the_list_lacks() {
        let row = |id: &str| Row {
            id: id.to_owned(),
            ..Row::default()
        };
        let page = new_rows(&[row("a"), row("b")], vec![row("b"), row("c")]);
        assert_eq!(
            page.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["c"]
        );
    }
}
