//! Mail: labels, the list and the reader, on one screen of its own.
//!
//! The label rule: a row and a label carry a fixed `label`, with sender and
//! subject in `accessibility_value` and in the row's `text`s. Those are in the
//! accessibility tree, as a screen reader needs them, and `ui_look` leaves
//! them out: it drops values, and a row with several texts goes out unnamed.
//! The Bot's `agent_hub::filter` tests hold that on the bytes it sends.

use std::collections::HashSet;

use buwiz_app::{Bridge, Placement};
use chrono::{Local, TimeZone as _, Utc};
use nsh_core::AccessibilityValue;
use nsh_core::element::{Root, dark, input};
use nsh_core::prelude::*;
use serde_json::{Value, json};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

use crate::compose::{self, Compose};
use crate::reader;
use crate::shell::{
    BG, CHOSEN, Chooser, DIM, INK, Job, LINE, Latest, MUTE, PANEL, POLL, RAIL, link_bar, once,
    read, sleep,
};

mod pictures;

const ROW_HEIGHT: f32 = 52.0;

#[derive(Clone, PartialEq, Default, Debug)]
pub struct Row {
    pub id: String,
    pub sender: String,
    pub subject: String,
    pub snippet: String,
    pub at: i64,
    pub unread: bool,
    pub starred: bool,
    pub messages: i64,
    pub attachment: bool,
}

impl Row {
    pub fn of(v: &Value) -> Option<Row> {
        let text = |k: &str| {
            v.get(k)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        let flag = |k: &str| v.get(k).and_then(Value::as_bool).unwrap_or(false);
        Some(Row {
            id: v.get("id")?.as_str()?.to_owned(),
            sender: text("sender"),
            subject: text("subject"),
            snippet: text("snippet"),
            at: v.get("at").and_then(Value::as_i64).unwrap_or(0),
            unread: flag("unread"),
            starred: flag("starred"),
            messages: v.get("messages").and_then(Value::as_i64).unwrap_or(1),
            attachment: flag("attachment"),
        })
    }
}

#[derive(Clone, PartialEq, Default, Debug)]
struct LabelRow {
    id: String,
    name: String,
    unread: i64,
}

/// The mailboxes everyone has, in the order the column shows them.
const SYSTEM: [(&str, &str); 6] = [
    ("INBOX", "Inbox"),
    ("STARRED", "Starred"),
    ("IMPORTANT", "Important"),
    ("SENT", "Sent"),
    ("DRAFT", "Drafts"),
    ("TRASH", "Trash"),
];

/// The column's rows: the system boxes in their order, then the person's
/// labels by name. Gmail's other system labels stay out.
fn column(labels: &Value) -> Vec<LabelRow> {
    let list: Vec<&Value> = labels
        .get("labels")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .collect();
    let unread = |id: &str| {
        list.iter()
            .find(|l| l["id"] == id)
            .and_then(|l| l["unread"].as_i64())
            .unwrap_or(0)
    };
    let mut out: Vec<LabelRow> = SYSTEM
        .iter()
        .map(|(id, name)| LabelRow {
            id: (*id).to_owned(),
            name: (*name).to_owned(),
            unread: unread(id),
        })
        .collect();
    let mut own: Vec<LabelRow> = list
        .iter()
        .filter(|l| l["kind"] == "user")
        .filter_map(|l| {
            Some(LabelRow {
                id: l["id"].as_str()?.to_owned(),
                name: l["name"].as_str()?.to_owned(),
                unread: l["unread"].as_i64().unwrap_or(0),
            })
        })
        .collect();
    own.sort_by(|a, b| a.name.cmp(&b.name));
    out.append(&mut own);
    out
}

/// Seconds east of UTC at `ms`, on this Mac: a zone's offset moves with the
/// date, so each moment asks for its own.
fn zone_at(ms: i64) -> i64 {
    Utc.timestamp_millis_opt(ms).single().map_or(0, |at| {
        i64::from(at.with_timezone(&Local).offset().local_minus_utc())
    })
}

/// A row's time in the person's zone: today's as the clock, older as a date.
pub fn short_time(at: i64, now: i64) -> String {
    time_in(at, now, zone_at)
}

fn time_in(at: i64, now: i64, zone: impl Fn(i64) -> i64) -> String {
    let local = |ms: i64| ms + zone(ms) * 1000;
    let (at, now) = (local(at), local(now));
    let day = |ms: i64| ms.div_euclid(86_400_000);
    if day(at) == day(now) {
        let secs = at.div_euclid(1000).rem_euclid(86_400);
        return format!("{:02}:{:02}", secs / 3600, secs / 60 % 60);
    }
    let z = day(at) + 719_468;
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let (d, m) = (
        doy - (153 * mp + 2) / 5 + 1,
        if mp < 10 { mp + 3 } else { mp - 9 },
    );
    let months = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    format!("{} {d}", months[(m - 1) as usize])
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// The line under the search box, when there is something to say. With
/// nothing listed there's no saved mail to claim.
pub fn notice(status: &str, saved: bool, empty: bool) -> Option<&'static str> {
    match (status, saved) {
        (_, true) | ("offline", _) if empty => Some("Can't reach Google."),
        (_, true) | ("offline", _) => Some("Can't reach Google. Showing saved mail."),
        ("needs_access", _) => {
            Some("Your Google sign-in needs more access. Sign in again on this app's page.")
        }
        ("failed", _) if empty => Some("Couldn't update your mail."),
        ("failed", _) => Some("Couldn't update your mail. Showing what's saved."),
        _ => None,
    }
}

/// What an empty list says, and the next press where there is one. Until the
/// first sync is in, empty says nothing about the mailbox; when the last one
/// failed, the list is only what's saved.
fn empty_line(label: &str, searching: bool, status: &str, saved: bool) -> &'static str {
    if searching {
        return "No mail matches that. Try other words.";
    }
    match status {
        "" | "syncing" => return "Your mail is on its way from Google…",
        "ok" if !saved => {}
        _ => return "No mail saved here yet.",
    }
    match label {
        "INBOX" => "Your inbox is empty. Press Write to start a message.",
        "STARRED" => "Nothing starred yet. Star a conversation and it shows here.",
        "IMPORTANT" => "Nothing marked important.",
        "SENT" => "Nothing sent yet. Press Write to start a message.",
        "DRAFT" => "No drafts. Press Write to start one.",
        "TRASH" => "Trash is empty.",
        _ => "No conversations have this label.",
    }
}

/// A page's rows the list doesn't hold yet: mail moving between pages can
/// bring one back, and each row's id must stay unique.
fn new_rows(have: &[Row], page: Vec<Row>) -> Vec<Row> {
    let have: HashSet<&str> = have.iter().map(|r| r.id.as_str()).collect();
    page.into_iter()
        .filter(|r| !have.contains(r.id.as_str()))
        .collect()
}

/// What the screen is showing right now.
#[derive(Clone, Copy)]
struct Mail {
    chooser: Chooser,
    list_turn: Latest,
    open_turn: Latest,
    labels: Signal<Vec<LabelRow>>,
    label: Signal<String>,
    rows: Signal<Vec<Row>>,
    more: Signal<bool>,
    query: Signal<String>,
    /// A card's search, by its ticket, until the person goes elsewhere.
    search: Signal<Option<String>>,
    /// The open conversation, by id: rows move as mail arrives and goes, and
    /// an action must land on the conversation the person chose.
    chosen: Signal<Option<String>>,
    /// Conversations picked with `x`, which actions take over the open one.
    picked: Signal<Vec<String>>,
    jobs: Signal<UnboundedSender<Job>>,
    /// What runs beside the jobs: an attachment can take half a minute to come,
    /// and an archive pressed meanwhile mustn't wait behind it.
    beside: Signal<UnboundedSender<Job>>,
    compose: Compose,
    /// The label picker is open.
    labeling: Signal<bool>,
    /// What can be undone, and what to say about it.
    undo: Signal<Option<(String, String)>>,
    /// The undo already pressed, while it waits for its answer.
    undoing: Signal<Option<String>>,
    page: Signal<Option<String>>,
    links: Signal<Vec<String>>,
    /// What the page leaves out of its pictures, and who they're from.
    images: Signal<pictures::Pictures>,
    /// The open conversation's attachments.
    attachments: Signal<Vec<Attachment>>,
    /// Why an attachment didn't open.
    attachment_said: Signal<Option<String>>,
    status: Signal<String>,
    saved: Signal<bool>,
    signed_out: Signal<bool>,
    link: Signal<Option<String>>,
    loading: Signal<bool>,
    /// What became of an action: it stays until the person dismisses it or
    /// acts again, as a reload can come at any moment.
    told: Signal<Option<Told>>,
    /// Why the list didn't load, in words for the person.
    problem: Signal<Option<String>>,
    seq: Signal<i64>,
    help: Signal<bool>,
}

/// A file on a mail in the open conversation.
#[derive(Clone, PartialEq, Debug)]
struct Attachment {
    /// The message that holds it. One that's inline in its mail has no id of
    /// its own, and so no way to open.
    message: String,
    /// Which part of the message: what opens it, as `id` can change between reads.
    part: String,
    id: String,
    name: String,
    mime: String,
}

fn attachments_in(v: &Value) -> Vec<Attachment> {
    let text = |a: &Value, key: &str| {
        a.get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    v.get("attachments")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|a| Attachment {
            message: text(a, "message"),
            part: text(a, "part"),
            id: text(a, "id"),
            name: text(a, "name"),
            mime: text(a, "mime"),
        })
        .collect()
}

/// What the list shows now, as `gws_mail_list` asks for it.
fn list_args(query: &str, search: Option<String>, label: String, skip: usize) -> Value {
    match search {
        Some(ticket) if query.is_empty() => json!({"search": ticket}),
        _ if query.is_empty() => json!({"label": label, "skip": skip}),
        _ => json!({"query": query}),
    }
}

fn list_now(m: Mail, skip: usize) -> Value {
    list_args(
        &m.query.get_untracked(),
        m.search.get_untracked(),
        m.label.get_untracked(),
        skip,
    )
}

fn rows_in(v: &Value) -> Vec<Row> {
    v.get("threads")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Row::of)
        .collect()
}

fn show_list(m: Mail, rows: Vec<Row>, v: &Value) {
    // a pick that left the list is one nobody can see
    m.picked
        .update(|p| p.retain(|id| rows.iter().any(|r| r.id == *id)));
    m.rows.set(rows);
    m.more.set(v.get("more") == Some(&Value::Bool(true)));
    m.saved.set(v.get("saved") == Some(&Value::Bool(true)));
    m.signed_out.set(false);
    m.problem.set(None);
}

fn list_failed(m: Mail, e: String) {
    if e == "sign-in" {
        m.signed_out.set(true);
    } else {
        m.problem.set(Some(e));
    }
}

async fn load_list(b: Bridge, m: Mail, skip: usize) {
    let turn = m.list_turn.ask();
    m.loading.set(true);
    let answer = read(&b, "gws_mail_list", list_now(m, skip)).await;
    // a later load owns the list and the flag now
    if !m.list_turn.counts(turn) {
        return;
    }
    m.loading.set(false);
    match answer {
        Ok(v) => {
            let mut rows = rows_in(&v);
            if skip > 0 {
                let mut all = m.rows.get_untracked();
                all.append(&mut new_rows(&all, rows));
                rows = all;
            }
            show_list(m, rows, &v);
        }
        Err(e) => list_failed(m, e),
    }
}

/// The list read again where it stands: as far down as it had got, and a page
/// further while a pick isn't found, as new mail pushes rows down. A pick not
/// found by then has left this list, and the screen says so.
async fn reload(b: Bridge, m: Mail) {
    let turn = m.list_turn.ask();
    let reach = m.rows.with_untracked(Vec::len);
    m.loading.set(true);
    let mut rows: Vec<Row> = Vec::new();
    let last = loop {
        let answer = read(&b, "gws_mail_list", list_now(m, rows.len())).await;
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
        let fresh = new_rows(&rows, rows_in(&v));
        let page = fresh.len();
        rows.extend(fresh);
        let seeking = m
            .picked
            .with_untracked(|p| p.iter().any(|id| !rows.iter().any(|r| r.id == *id)));
        let enough = reach + if seeking { page } else { 0 };
        if page == 0 || rows.len() >= enough || v.get("more") != Some(&Value::Bool(true)) {
            break v;
        }
    };
    m.loading.set(false);
    let picks = m.picked.with_untracked(Vec::len);
    show_list(m, rows, &last);
    match picks - m.picked.with_untracked(Vec::len) {
        0 => {}
        1 => tell_too(m, "A picked conversation left this list."),
        n => tell_too(m, &format!("{n} picked conversations left this list.")),
    }
}

/// A line about what became of an action, and the undo of the one it's about.
#[derive(Clone, PartialEq)]
struct Told {
    line: String,
    undo: Option<String>,
}

fn tell(m: Mail, line: String, undo: Option<String>) {
    m.told.set(Some(Told { line, undo }));
}

/// A line that doesn't replace what's being said already.
fn tell_too(m: Mail, line: &str) {
    match m.told.get_untracked() {
        Some(Told { line: said, undo }) => tell(m, format!("{said} {line}"), undo),
        None => tell(m, line.to_owned(), None),
    }
}

/// Hands `job` to the screen's own task. A press's task dies with the view it
/// was pressed on, and an action redraws what it's pressed on: an action
/// must outlive that, and two must run in the order they were asked.
fn run(m: Mail, job: impl Future<Output = ()> + 'static) {
    m.jobs.with_untracked(|jobs| drop(jobs.send(Box::pin(job))));
}

/// `args` for an action on what's picked or open right now: read at the
/// press, because the job runs later and the person may have moved on.
fn on_targets(m: Mail, mut args: Value) -> Value {
    args["threads"] = json!(targets(m));
    args
}

fn thread_ids(args: &Value) -> Vec<String> {
    args.get("threads")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|t| t.as_str().map(str::to_owned))
        .collect()
}

/// What an action takes: the picked conversations, else the open one.
fn targets(m: Mail) -> Vec<String> {
    let picked = m.picked.get_untracked();
    if !picked.is_empty() {
        return picked;
    }
    m.chosen.get_untracked().into_iter().collect()
}

/// The row to open once `gone` have left the list.
fn next_after(rows: &[Row], gone: &[String]) -> Option<String> {
    let last = rows.iter().rposition(|r| gone.contains(&r.id))?;
    let keeps = |r: &&Row| !gone.contains(&r.id);
    rows[last..]
        .iter()
        .find(keeps)
        .or_else(|| rows[..last].iter().rev().find(keeps))
        .map(|r| r.id.clone())
}

/// A person's action, answered and shown. `leaves` says the conversations go
/// from this list, and `change` what shows on those that stay.
async fn act(
    b: Bridge,
    m: Mail,
    tool: &str,
    args: Value,
    said: &str,
    leaves: bool,
    change: fn(&mut Row),
) {
    let ids = thread_ids(&args);
    if ids.is_empty() {
        return;
    }
    match once(&b, tool, args).await {
        Ok(v) => {
            let undo = v.get("undo").and_then(Value::as_str).map(str::to_owned);
            if let Some(undo) = &undo {
                m.undo.set(Some((undo.clone(), said.to_owned())));
            }
            let done = if v.get("threads").is_some() {
                thread_ids(&v)
            } else {
                ids.clone()
            };
            let missed: Vec<String> = ids.iter().filter(|i| !done.contains(i)).cloned().collect();
            m.told.set((!missed.is_empty()).then(|| Told {
                line: format!(
                    "{} of {} went through. The rest didn't.",
                    done.len(),
                    ids.len()
                ),
                undo,
            }));
            // what failed stays picked, ready to try again, with anything picked since
            m.picked.update(|p| p.retain(|id| !done.contains(id)));
            if leaves {
                let next = m.rows.with_untracked(|r| next_after(r, &done));
                let open_gone = m
                    .chosen
                    .with_untracked(|c| c.as_ref().is_some_and(|c| done.contains(c)));
                m.rows.update(|r| r.retain(|r| !done.contains(&r.id)));
                if open_gone {
                    m.chosen.set(next.clone());
                    match next {
                        Some(id) => show(b, m, id).await,
                        None => m.page.set(None),
                    }
                }
            } else {
                m.rows.update(|r| {
                    r.iter_mut()
                        .filter(|r| done.contains(&r.id))
                        .for_each(change)
                });
            }
        }
        Err(e) if e == "sign-in" => m.signed_out.set(true),
        Err(e) => tell(m, e, None),
    }
}

/// Undoes `undo`, named at the press: another action can finish before this
/// one's turn comes, and it mustn't be that one that goes back.
async fn undo_last(b: Bridge, m: Mail, undo: Option<String>) {
    let Some(undo) = undo else {
        return;
    };
    let answer = once(&b, "gws_mail_undo", json!({"undo": undo})).await;
    if m.undoing.with_untracked(|u| u.as_ref() == Some(&undo)) {
        m.undoing.set(None);
    }
    match answer {
        Ok(_) => {
            if m.undo
                .with_untracked(|u| u.as_ref().is_some_and(|(id, _)| *id == undo))
            {
                m.undo.set(None);
            }
            // what's said about another action still stands
            if m.told
                .with_untracked(|t| t.as_ref().is_some_and(|t| t.undo.as_ref() == Some(&undo)))
            {
                m.told.set(None);
            }
            reload(b, m).await;
        }
        // the bar stays, so it can be pressed again
        Err(e) => tell(m, e, Some(undo)),
    }
}

/// What Undo would take back right now, and nothing while that's on its way: a
/// second press would ask for what the first already took.
fn claim_undo(m: Mail) -> Option<String> {
    let (id, _) = m.undo.get_untracked()?;
    if m.undoing.with_untracked(|u| u.as_ref() == Some(&id)) {
        return None;
    }
    m.undoing.set(Some(id.clone()));
    Some(id)
}

async fn load_labels(b: Bridge, m: Mail) {
    if let Ok(v) = read(&b, "gws_mail_labels", json!({})).await {
        m.labels.set(column(&v));
    }
}

async fn open(b: Bridge, m: Mail, index: usize) {
    let Some(id) = m
        .rows
        .with_untracked(|r| r.get(index).map(|r| r.id.clone()))
    else {
        return;
    };
    // a draft goes to the panel: the reader has nothing of it to open
    if m.label.with_untracked(|l| l == "DRAFT") {
        return compose::resume(b, m.compose, id);
    }
    m.chosen.set(Some(id.clone()));
    let unread = m
        .rows
        .with_untracked(|r| r.get(index).is_some_and(|r| r.unread));
    show(b.clone(), m, id.clone()).await;
    // only a person's open reads a mail, so this is here and not in the tool
    if unread
        && once(&b, "gws_mail_mark", json!({"threads": [id], "read": true}))
            .await
            .is_ok()
    {
        m.rows.update(|r| {
            if let Some(row) = r.iter_mut().find(|r| r.id == id) {
                row.unread = false;
            }
        });
    }
}

/// A card's row, opened in the reader.
async fn open_ticket(b: Bridge, m: Mail, ticket: String) {
    show(b, m, ticket).await;
}

async fn show(b: Bridge, m: Mail, thread: String) {
    let turn = m.open_turn.ask();
    m.page.set(None);
    m.attachments.set(Vec::new());
    m.attachment_said.set(None);
    let answer = read(&b, "gws_mail_thread", json!({"thread": thread})).await;
    // the person opened another, or left: this one must not replace it
    if !m.open_turn.counts(turn) {
        return;
    }
    match answer {
        Ok(v) => {
            m.links.set(
                v.get("links")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|l| l.as_str().map(str::to_owned))
                    .collect(),
            );
            pictures::opened(&b, m, &v, turn);
            m.attachments.set(attachments_in(&v));
            m.page.set(Some(
                v.get("page")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            ));
        }
        Err(e) if e == "sign-in" => m.signed_out.set(true),
        Err(e) => m.page.set(Some(format!(
            "<p>{}</p>",
            e.replace('&', "&amp;").replace('<', "&lt;")
        ))),
    }
}

/// The account changed under this screen: what's on it is the old one's. A
/// letter being written stays, on the account it was started on.
fn switched(b: &Bridge, m: Mail) {
    m.label.set("INBOX".to_owned());
    m.query.set(String::new());
    m.search.set(None);
    m.chosen.set(None);
    m.picked.set(Vec::new());
    m.undo.set(None);
    m.open_turn.ask();
    m.page.set(None);
    m.seq.set(-1);
    b.spawn(load_list(b.clone(), m, 0));
    b.spawn(load_labels(b.clone(), m));
}

/// [`Chooser::check`], and the account a new letter starts on.
async fn check_account(b: &Bridge, m: Mail) {
    let changed = m.chooser.check(b).await;
    // one opened before the first answer is on the account that answer names
    if !m.compose.open.get_untracked() || m.compose.who.with_untracked(String::is_empty) {
        m.compose.who.set(m.chooser.who.get_untracked());
    }
    if changed {
        switched(b, m);
    }
}

async fn watch(b: Bridge, m: Mail) {
    loop {
        check_account(&b, m).await;
        if let Ok(v) = read(
            &b,
            "gws_mail_changes",
            json!({"since": m.seq.get_untracked()}),
        )
        .await
        {
            m.status.set(
                v.get("status")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            );
            m.seq.set(v.get("seq").and_then(Value::as_i64).unwrap_or(0));
            if v.get("changed") == Some(&Value::Bool(true))
                && m.query.with_untracked(String::is_empty)
            {
                reload(b.clone(), m).await;
                load_labels(b.clone(), m).await;
            }
        }
        sleep(POLL).await;
    }
}

fn press_class(on: bool) -> String {
    format!(
        "px-3 py-1 rounded-md flex-row justify-between {}",
        if on { CHOSEN } else { "" }
    )
}

fn label_rail(b: Bridge, m: Mail) -> impl IntoElement {
    let writes = b.clone();
    let chooses = b.clone();
    // the highlight is a binding on each row, not read here: a rebuild on the
    // chosen label would drop the pressed row, and the load it started with it
    let list = dynamic(move || {
        m.labels
            .get()
            .into_iter()
            .fold(view().app_class(Some("gap-[1px]".into())), |column, l| {
                let (id, row, b) = (l.id.clone(), l.id.clone(), b.clone());
                let count = if l.unread > 0 {
                    l.unread.to_string()
                } else {
                    String::new()
                };
                column.child(
                    view()
                        .app_class(move || Some(press_class(m.label.with(|c| *c == row)).into()))
                        .label("Open this label")
                        .accessibility_value(Some(AccessibilityValue {
                            text: Some(l.name.clone().into()),
                            ..AccessibilityValue::default()
                        }))
                        .on_press(move || {
                            m.label.set(id.clone());
                            m.query.set(String::new());
                            m.search.set(None);
                            m.chosen.set(None);
                            m.picked.set(Vec::new());
                            m.open_turn.ask();
                            m.page.set(None);
                            b.spawn(load_list(b.clone(), m, 0));
                        })
                        .child(text(l.name).app_class(Some(format!("text-[13px] {INK}").into())))
                        .child(text(count).app_class(Some(format!("text-[12px] {MUTE}").into()))),
                )
            })
    });
    let accounts = dynamic(move || {
        // a letter stays on its account, so a switch waits for it to close. One
        // left on another account by a switch elsewhere can only be saved from
        // there, so then it's the way back.
        if m.compose.open.get() && m.compose.who.get() == m.chooser.who.get() {
            return view();
        }
        let b = chooses.clone();
        view().child(crate::shell::chooser(
            b,
            m.chooser,
            move |job| m.beside.with_untracked(|beside| drop(beside.send(job))),
            move |b| {
                if !m.compose.open.get_untracked() {
                    m.compose.who.set(m.chooser.who.get_untracked());
                }
                switched(b, m);
            },
        ))
    });
    let column = scroll()
        .app_class(Some("flex-1 pt-3".into()))
        .child(
            view().app_class(Some("px-2 py-2".into())).child(
                view()
                    .app_class(Some(
                        "items-center px-3 py-1.5 rounded-md bg-[#2563eb]".into(),
                    ))
                    .label("Write a message")
                    .on_press(move || compose::start(writes.clone(), m.compose))
                    .child(
                        text("Write")
                            .app_class(Some("text-[13px] font-semibold text-white".into())),
                    ),
            ),
        )
        .child(list);
    view()
        .app_class(Some(format!("w-[190px] border-r {LINE} {RAIL}").into()))
        .child(column)
        .child(accounts)
}

/// One cell of the list. Cells are recycled, so every read goes through
/// `at`: a position read once would keep showing, and opening, its first row.
fn row_view(b: Bridge, m: Mail, at: Position) -> impl IntoElement {
    let field = move |f: fn(&Row) -> String| {
        move || {
            m.rows
                .with(|r| r.get(at.index()).map(f).unwrap_or_default())
        }
    };
    let chosen = move || {
        m.chosen.with(|c| {
            m.rows
                .with(|r| c.is_some() && r.get(at.index()).map(|r| &r.id) == c.as_ref())
        })
    };
    let picked = move || {
        m.picked.with(|p| {
            m.rows
                .with(|r| r.get(at.index()).is_some_and(|r| p.contains(&r.id)))
        })
    };
    let weight = move || m.rows.with(|r| r.get(at.index()).is_some_and(|r| r.unread));
    view()
        .app_class(move || {
            Some(
                format!(
                    "h-[{ROW_HEIGHT}px] px-3 py-1.5 border-b {LINE} {}",
                    if chosen() || picked() { CHOSEN } else { "" }
                )
                .into(),
            )
        })
        .label("Open conversation")
        .accessibility_value(move || {
            m.rows.with(|r| {
                r.get(at.index()).map(|r| AccessibilityValue {
                    text: Some(format!("{}, {}", r.sender, r.subject).into()),
                    ..AccessibilityValue::default()
                })
            })
        })
        .on_press(move || {
            b.spawn(open(b.clone(), m, at.index()));
        })
        .child(
            view()
                .app_class(Some("flex-row justify-between gap-2".into()))
                .child(
                    text(field(|r| r.sender.clone()))
                        .number_of_lines(1)
                        .app_class(move || {
                            Some(
                                format!(
                                    "flex-1 text-[13px] {INK} {}",
                                    if weight() { "font-semibold" } else { "" }
                                )
                                .into(),
                            )
                        }),
                )
                .child(
                    text(field(|r| short_time(r.at, now_ms())))
                        .app_class(Some(format!("text-[11px] {DIM}").into())),
                ),
        )
        .child(
            text(field(|r| {
                if r.messages > 1 {
                    format!("{} ({})", r.subject, r.messages)
                } else {
                    r.subject.clone()
                }
            }))
            .number_of_lines(1)
            .app_class(Some(format!("text-[12px] {INK}").into())),
        )
        .child(
            text(field(|r| r.snippet.clone()))
                .number_of_lines(1)
                .app_class(Some(format!("text-[11px] {MUTE}").into())),
        )
}

fn list_pane(b: Bridge, m: Mail) -> impl IntoElement {
    let search = element_ref();
    on_key("/", move |_| search.focus());
    let submit = b.clone();
    let more = b.clone();
    let row_bridge = b.clone();
    let empty = dynamic(move || {
        let searching = !m.query.with(String::is_empty) || m.search.with(Option::is_some);
        let line = if m.loading.get() {
            "Loading your mail…"
        } else if !searching && m.problem.with(Option::is_some) {
            ""
        } else {
            empty_line(&m.label.get(), searching, &m.status.get(), m.saved.get())
        };
        view()
            .app_class(Some("p-6 items-center".into()))
            .child(text(line).app_class(Some(format!("text-[13px] {MUTE}").into())))
    });
    view()
        .app_class(Some(format!("w-[330px] border-r {LINE} {PANEL}").into()))
        .child(
            view()
                .app_class(Some(format!("p-2 border-b {LINE}").into()))
                .child(
                    input()
                        .element_ref(&search)
                        .value(move || m.query.get())
                        .placeholder("Search mail".to_owned())
                        .app_class(Some(
                            format!("rounded-lg border px-2.5 py-1 text-[13px] {BG} {LINE} {INK}")
                                .into(),
                        ))
                        .on_change(move |value: &str| m.query.set(value.to_owned()))
                        .on_submit(move |_: &str| {
                            m.chosen.set(None);
                            m.picked.set(Vec::new());
                            submit.spawn(load_list(submit.clone(), m, 0));
                        }),
                ),
        )
        .child(dynamic(move || match m.told.get() {
            Some(Told { line, .. }) => view()
                .app_class(Some(
                    format!("flex-row items-center gap-2 px-3 py-1 border-b {LINE}").into(),
                ))
                .child(text(line).app_class(Some(format!("flex-1 text-[11px] {INK}").into())))
                .child(
                    view()
                        .label("Dismiss this")
                        .on_press(move || m.told.set(None))
                        .child(
                            text("Dismiss").app_class(Some(format!("text-[11px] {MUTE}").into())),
                        ),
                ),
            None => view(),
        }))
        .child(dynamic(move || {
            let line = m.problem.get().or_else(|| {
                notice(&m.status.get(), m.saved.get(), m.rows.with(Vec::is_empty))
                    .map(str::to_owned)
            });
            match line {
                Some(line) => view()
                    .app_class(Some(format!("px-3 py-1 border-b {LINE}").into()))
                    .child(text(line).app_class(Some(format!("text-[11px] {MUTE}").into()))),
                None => view(),
            }
        }))
        .child(
            virtual_list(
                move || m.rows.with(Vec::len),
                move |at| row_view(row_bridge.clone(), m, at),
            )
            // rows come and go anywhere when the list changes, not just at its end
            .keys(move || {
                m.rows
                    .with(|r| r.iter().map(|r| r.id.clone()).collect::<Vec<_>>())
            })
            .estimated_item_size(px(ROW_HEIGHT))
            .empty(empty)
            .on_load_more(move || {
                if m.more.get_untracked() && !m.loading.get_untracked() {
                    let skip = m.rows.with_untracked(Vec::len);
                    more.spawn(load_list(more.clone(), m, skip));
                }
            })
            .app_class(Some("flex-1".into())),
        )
}

/// A press on the reader's toolbar: a fixed label, and what it does.
fn tool(label: &'static str, shows: &'static str, press: impl Fn() + 'static) -> View {
    view()
        .app_class(Some("px-2 py-1 rounded-md".into()))
        .label(label)
        .on_press(press)
        .child(text(shows).app_class(Some(format!("text-[12px] {INK}").into())))
}

/// What can be done to the open conversation, or to the picked ones.
fn toolbar(b: Bridge, m: Mail) -> impl IntoElement {
    let writes = b.clone();
    let answer = move |mode: &'static str| {
        let b = writes.clone();
        move || {
            if let Some(thread) = m.chosen.get_untracked() {
                compose::reply(b.clone(), m.compose, thread, mode);
            }
        }
    };
    let go = move |tool: &'static str,
                   args: Value,
                   said: &'static str,
                   leaves: bool,
                   change: fn(&mut Row)| {
        let b = b.clone();
        let args = args.clone();
        move || {
            run(
                m,
                act(
                    b.clone(),
                    m,
                    tool,
                    on_targets(m, args.clone()),
                    said,
                    leaves,
                    change,
                ),
            );
        }
    };
    view()
        .app_class(Some(
            format!("flex-row flex-wrap gap-1 px-2 py-1 border-b {LINE}").into(),
        ))
        .child(tool("Reply to this", "Reply", answer("reply")))
        .child(tool("Reply to everyone", "Reply all", answer("all")))
        .child(tool("Forward this", "Forward", answer("forward")))
        .child(tool(
            "Archive this",
            "Archive",
            go("gws_mail_archive", json!({}), "Archived.", true, |_| {}),
        ))
        .child(tool(
            "Delete this",
            "Delete",
            go("gws_mail_trash", json!({}), "Moved to Trash.", true, |_| {}),
        ))
        .child(tool(
            "Star this",
            "Star",
            go(
                "gws_mail_mark",
                json!({"starred": true}),
                "Starred.",
                false,
                |r| r.starred = true,
            ),
        ))
        .child(tool(
            "Mark this unread",
            "Mark unread",
            go(
                "gws_mail_mark",
                json!({"read": false}),
                "Marked unread.",
                false,
                |r| r.unread = true,
            ),
        ))
        .child(tool("Choose a label", "Label", move || {
            m.labeling.set(!m.labeling.get_untracked())
        }))
}

/// The person's own labels: add one, or move the conversation there.
fn label_picker(b: Bridge, m: Mail) -> impl IntoElement {
    dynamic(move || {
        if !m.labeling.get() {
            return view();
        }
        let own = m.labels.with(|l| {
            l.iter()
                .filter(|l| !SYSTEM.iter().any(|(id, _)| *id == l.id))
                .cloned()
                .collect::<Vec<_>>()
        });
        if own.is_empty() {
            return view()
                .app_class(Some(format!("px-3 py-1 border-b {LINE}").into()))
                .child(
                    text("No labels of your own yet. Make one in Gmail and it shows here.")
                        .app_class(Some(format!("text-[12px] {MUTE}").into())),
                );
        }
        own.into_iter().fold(
            view().app_class(Some(format!("px-2 py-1 gap-1 border-b {LINE}").into())),
            |list, l| {
                let value = || AccessibilityValue {
                    text: Some(l.name.clone().into()),
                    ..AccessibilityValue::default()
                };
                let (add, moved, id) = (b.clone(), b.clone(), l.id.clone());
                let labelled = id.clone();
                list.child(
                    view()
                        .app_class(Some("flex-row items-center gap-2".into()))
                        .child(
                            text(l.name.clone())
                                .app_class(Some(format!("flex-1 text-[12px] {INK}").into())),
                        )
                        .child(
                            tool("Add this label", "Add", move || {
                                m.labeling.set(false);
                                run(
                                    m,
                                    act(
                                        add.clone(),
                                        m,
                                        "gws_mail_label",
                                        on_targets(m, json!({"add": [labelled.clone()]})),
                                        "Label added.",
                                        false,
                                        |_| {},
                                    ),
                                );
                            })
                            .accessibility_value(Some(value())),
                        )
                        .child(
                            tool("Move here", "Move", move || {
                                m.labeling.set(false);
                                run(
                                    m,
                                    act(
                                        moved.clone(),
                                        m,
                                        "gws_mail_label",
                                        on_targets(
                                            m,
                                            json!({"add": [id.clone()], "remove": ["INBOX"]}),
                                        ),
                                        "Moved.",
                                        true,
                                        |_| {},
                                    ),
                                );
                            })
                            .accessibility_value(Some(value())),
                        ),
                )
            },
        )
    })
}

fn reader_pane(b: Bridge, m: Mail) -> impl IntoElement {
    view()
        .app_class(Some(format!("flex-1 {PANEL}").into()))
        .child(dynamic(move || {
            let pane = view().app_class(Some("flex-1".into()));
            if m.compose.open.get() {
                pane.child(compose::panel(b.clone(), m.compose))
            } else {
                pane.child(reading(b.clone(), m))
            }
        }))
}

/// A press on an attachment: it opens in the Bot's viewer, from the answer's
/// `_meta`, which never reaches here. The call starts at the press, as who
/// pressed is judged by that moment.
fn open_attachment(b: &Bridge, m: Mail, a: &Attachment) {
    let call = b.call(
        "gws_mail_attachment_open",
        json!({"message": a.message, "part": a.part}),
    );
    m.attachment_said.set(Some("Opening…".to_owned()));
    let job = Box::pin(async move {
        m.attachment_said.set(match call.await {
            Ok(v) if v.get("refused").is_some() => Some(
                v.get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("That didn't work.")
                    .to_owned(),
            ),
            Ok(_) => None,
            Err(e) if e.code == "timeout" => {
                Some("This attachment is taking a while. Press it again in a moment.".to_owned())
            }
            Err(e) => Some(e.message),
        });
    });
    m.beside.with_untracked(|beside| drop(beside.send(job)));
}

fn attachment_row(b: Bridge, m: Mail, a: Attachment) -> View {
    let row = view()
        .app_class(Some(
            "flex-row flex-wrap items-center gap-2 px-3 py-1".into(),
        ))
        .child(text(a.name.clone()).number_of_lines(1).app_class(Some(
            format!("flex-1 min-w-[80px] text-[12px] {INK}").into(),
        )));
    if a.message.is_empty() || a.id.is_empty() {
        return row;
    }
    row.child(
        view()
            .app_class(Some("px-3 py-1".into()))
            .label("Open this attachment")
            .accessibility_value(Some(AccessibilityValue {
                text: Some(a.name.clone().into()),
                ..AccessibilityValue::default()
            }))
            .on_press(move || open_attachment(&b, m, &a))
            .child(text("Open").app_class(Some(format!("text-[12px] {INK}").into()))),
    )
}

/// The conversation's files, under the page. Past a few they scroll, so the
/// page keeps its room.
fn attachments(b: Bridge, m: Mail) -> impl IntoElement {
    dynamic(move || {
        let files = m.attachments.get();
        if files.is_empty() {
            return view();
        }
        let list = files.into_iter().fold(view(), |list, a| {
            list.child(attachment_row(b.clone(), m, a))
        });
        let said = m.attachment_said.get().map_or_else(view, |line| {
            view()
                .app_class(Some("px-3 py-1".into()))
                .child(text(line).app_class(Some(format!("text-[11px] {MUTE}").into())))
        });
        view()
            .app_class(Some(format!("border-t {LINE}").into()))
            .child(said)
            .child(scroll().app_class(Some("max-h-[120px]".into())).child(list))
    })
}

fn reading(b: Bridge, m: Mail) -> impl IntoElement {
    let picking = b.clone();
    let files = b.clone();
    let bar = dynamic(move || {
        if m.chosen.with(Option::is_none) && m.picked.with(Vec::is_empty) {
            return view();
        }
        view()
            .child(toolbar(b.clone(), m))
            .child(label_picker(picking.clone(), m))
    });
    view()
        .app_class(Some("flex-1".into()))
        .child(bar)
        .child(dynamic(move || match m.page.get() {
            Some(page) => view()
                .app_class(Some("flex-1".into()))
                .child(pictures::bar(files.clone(), m))
                .child(reader::view(
                    move || reader::seal(&page, dark()),
                    move |n| m.link.set(m.links.with_untracked(|l| l.get(n).cloned())),
                ))
                .child(attachments(files.clone(), m)),
            None if m.chosen.get().is_some() => view()
                .app_class(Some("flex-1 items-center justify-center".into()))
                .child(text("Opening…").app_class(Some(format!("text-[13px] {MUTE}").into()))),
            // only this line follows the rows: a page must not rebuild with them
            None => view()
                .app_class(Some("flex-1 items-center justify-center".into()))
                .child(
                    text(move || {
                        if !m.rows.with(Vec::is_empty) {
                            "Pick a conversation to read it"
                        } else if m.loading.get() {
                            "Loading…"
                        } else {
                            "Nothing to read here."
                        }
                    })
                    .app_class(Some(format!("text-[13px] {MUTE}").into())),
                ),
        }))
}

/// What was just done, with the way back.
fn undo_bar(b: Bridge, m: Mail) -> impl IntoElement {
    dynamic(move || match m.undo.get() {
        None => view(),
        Some((_, said)) => {
            let b = b.clone();
            view()
                .app_class(Some(
                    format!("flex-row items-center gap-3 px-3 py-2 border-t {LINE} {RAIL}").into(),
                ))
                .child(text(said).app_class(Some(format!("flex-1 text-[12px] {INK}").into())))
                .child(
                    view()
                        .app_class(Some("px-3 py-1".into()))
                        .label("Undo that")
                        .on_press(move || {
                            run(m, undo_last(b.clone(), m, claim_undo(m)));
                        })
                        .child(text("Undo").app_class(Some(format!("text-[12px] {INK}").into()))),
                )
        }
    })
}

fn keys(b: Bridge, m: Mail) {
    let here = move || {
        let chosen = m.chosen.get_untracked()?;
        m.rows
            .with_untracked(|r| r.iter().position(|r| r.id == chosen))
    };
    let step = move |by: isize| {
        let len = m.rows.with_untracked(Vec::len) as isize;
        let at = here().map_or(-1, |i| i as isize);
        ((at + by).clamp(0, (len - 1).max(0))) as usize
    };
    let acts = b.clone();
    let (down, up, enter, oh) = (b.clone(), b.clone(), b.clone(), b);
    on_key("j", move |_| {
        down.spawn(open(down.clone(), m, step(1)));
    });
    on_key("k", move |_| {
        up.spawn(open(up.clone(), m, step(-1)));
    });
    on_key("enter", move |_| {
        enter.spawn(open(enter.clone(), m, here().unwrap_or(0)));
    });
    on_key("o", move |_| {
        oh.spawn(open(oh.clone(), m, here().unwrap_or(0)));
    });
    on_key("u", move |_| {
        m.chosen.set(None);
        m.page.set(None);
    });
    on_key("shift+/", move |_| m.help.set(!m.help.get_untracked()));
    let (archive, trash, star) = (acts.clone(), acts.clone(), acts.clone());
    let (read_it, unread_it, back) = (acts.clone(), acts.clone(), acts.clone());
    on_key("e", move |_| {
        run(
            m,
            act(
                archive.clone(),
                m,
                "gws_mail_archive",
                on_targets(m, json!({})),
                "Archived.",
                true,
                |_| {},
            ),
        );
    });
    on_key("shift+3", move |_| {
        run(
            m,
            act(
                trash.clone(),
                m,
                "gws_mail_trash",
                on_targets(m, json!({})),
                "Moved to Trash.",
                true,
                |_| {},
            ),
        );
    });
    on_key("s", move |_| {
        let on = m.chosen.get_untracked().is_some_and(|c| {
            m.rows
                .with_untracked(|r| r.iter().any(|r| r.id == c && r.starred))
        });
        let change: fn(&mut Row) = if on {
            |r| r.starred = false
        } else {
            |r| r.starred = true
        };
        run(
            m,
            act(
                star.clone(),
                m,
                "gws_mail_mark",
                on_targets(m, json!({"starred": !on})),
                if on { "Star removed." } else { "Starred." },
                false,
                change,
            ),
        );
    });
    on_key("shift+i", move |_| {
        run(
            m,
            act(
                read_it.clone(),
                m,
                "gws_mail_mark",
                on_targets(m, json!({"read": true})),
                "Marked read.",
                false,
                |r| r.unread = false,
            ),
        );
    });
    on_key("shift+u", move |_| {
        run(
            m,
            act(
                unread_it.clone(),
                m,
                "gws_mail_mark",
                on_targets(m, json!({"read": false})),
                "Marked unread.",
                false,
                |r| r.unread = true,
            ),
        );
    });
    let (new, answer) = (acts.clone(), acts.clone());
    on_key("c", move |_| compose::start(new.clone(), m.compose));
    for (key, mode) in [("r", "reply"), ("a", "all"), ("f", "forward")] {
        let b = answer.clone();
        on_key(key, move |_| {
            if let Some(thread) = m.chosen.get_untracked() {
                compose::reply(b.clone(), m.compose, thread, mode);
            }
        });
    }
    let (sending, keeping) = (acts.clone(), acts);
    on_key("cmd+enter", move |_| {
        if m.compose.open.get_untracked() {
            compose::send(sending.clone(), m.compose);
        }
    });
    on_key("escape", move |_| {
        if m.compose.open.get_untracked() {
            compose::keep(keeping.clone(), m.compose);
        }
    });
    on_key("x", move |_| {
        if let Some(id) = m.chosen.get_untracked() {
            m.picked.update(|p| match p.iter().position(|x| *x == id) {
                Some(at) => drop(p.remove(at)),
                None => p.push(id),
            });
        }
    });
    on_key("z", move |_| {
        run(m, undo_last(back.clone(), m, claim_undo(m)));
    });
}

/// Each person brings their own Google client, so this is the way in: the
/// steps, each with the page it happens on.
pub const SETUP: [(&str, &str, Option<&str>); 5] = [
    (
        "Make a Google Cloud project",
        "It's free, and it's yours.",
        Some("https://console.cloud.google.com/projectcreate"),
    ),
    (
        "Turn on the APIs",
        "Gmail, Calendar, Drive and People.",
        Some(
            "https://console.cloud.google.com/flows/enableapi?apiid=gmail.googleapis.com,calendar-json.googleapis.com,drive.googleapis.com,people.googleapis.com",
        ),
    ),
    (
        "Set up the consent screen",
        "Choose External and add your own address as a test user. Publish it, or you'll be asked to sign in again every 7 days.",
        Some("https://console.cloud.google.com/auth/overview"),
    ),
    (
        "Create a client",
        "Choose Desktop app, then copy its Client ID and Client secret.",
        Some("https://console.cloud.google.com/auth/clients/create"),
    ),
    (
        "Paste them and sign in",
        "On this app's page in Connect apps: Setup, then Sign in. Google will warn the app isn't verified. It's your own client, so you can go on.",
        None,
    ),
];

fn signed_out_view(b: Bridge) -> impl IntoElement {
    let steps = SETUP.iter().enumerate().fold(
        view().app_class(Some("gap-3 w-full max-w-[520px]".into())),
        |list, (n, (title, detail, url))| {
            let mut row = view()
                .app_class(Some("flex-row gap-3 items-start".into()))
                .child(
                    text(format!("{}", n + 1))
                        .app_class(Some(format!("text-[13px] w-[16px] {MUTE}").into())),
                )
                .child(
                    view()
                        .app_class(Some("flex-1 gap-[2px]".into()))
                        .child(
                            text(*title)
                                .app_class(Some(format!("text-[13px] font-semibold {INK}").into())),
                        )
                        .child(text(*detail).app_class(Some(format!("text-[12px] {MUTE}").into()))),
                );
            if let Some(url) = url {
                let b = b.clone();
                row = row.child(
                    view()
                        .app_class(Some(
                            "px-3 py-1 rounded-md bg-[#e9e9e6] dark:bg-[#2d2d2d]".into(),
                        ))
                        .label("Open this step")
                        .on_press(move || {
                            let open = b.open_url(url);
                            b.spawn(async move { drop(open.await) });
                        })
                        .child(text("Open").app_class(Some(format!("text-[12px] {INK}").into()))),
                );
            }
            list.child(row)
        },
    );
    scroll().app_class(Some("flex-1".into())).child(
        view().app_class(Some("p-6 gap-4 items-center".into())).child(text("Connect your Google account").app_class(Some(format!("text-[16px] font-semibold {INK}").into()))).child(text("Mail uses your own Google client, so your mail goes straight from Google to this Mac. It takes a few minutes, once.").app_class(Some(format!("text-[13px] {MUTE}").into()))).child(steps),
    )
}

/// Mail as a card in a chat: its own rows, read live, so no mail sits in the
/// chat's record. A row or the card's maximize opens Mail.
fn card(b: Bridge) -> View {
    #[derive(Clone, PartialEq)]
    enum State {
        Loading,
        Rows(Vec<(String, String, i64, String)>, bool),
        Line(String),
    }
    let state = signal(State::Loading);
    let ticket = b
        .props()
        .get("search")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let reader = b.clone();
    b.spawn(async move {
        match read(&reader, "gws_mail_card", json!({"ticket": ticket})).await {
            Ok(v) => {
                let rows = v
                    .get("rows")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|r| {
                        (
                            r["sender"].as_str().unwrap_or_default().to_owned(),
                            r["subject"].as_str().unwrap_or_default().to_owned(),
                            r["at"].as_i64().unwrap_or(0),
                            r["ticket"].as_str().unwrap_or_default().to_owned(),
                        )
                    })
                    .collect();
                state.set(State::Rows(
                    rows,
                    v.get("saved") == Some(&Value::Bool(true)),
                ));
            }
            Err(e) if e == "sign-in" => {
                state.set(State::Line("Sign in to Google first.".to_owned()))
            }
            Err(e) => state.set(State::Line(e)),
        }
        reader.ready();
    });
    let fit = b.clone();
    let press = b;
    view()
        .app_class(Some(PANEL.into()))
        .on_layout(
            move |bounds: &nsh_core::geometry::Bounds<nsh_core::geometry::Points>| {
                let _ = fit.fit(bounds.size.height.0.ceil());
            },
        )
        .child(dynamic(move || match state.get() {
            State::Loading => view()
                .app_class(Some("p-3".into()))
                .child(text("Looking…").app_class(Some(format!("text-[12px] {MUTE}").into()))),
            State::Line(line) => view()
                .app_class(Some("p-3".into()))
                .child(text(line).app_class(Some(format!("text-[12px] {MUTE}").into()))),
            // the card is read live, and what it found can have gone since
            State::Rows(rows, _) if rows.is_empty() => view().app_class(Some("p-3".into())).child(
                text("No mail matches this now.")
                    .app_class(Some(format!("text-[12px] {MUTE}").into())),
            ),
            State::Rows(rows, saved) => {
                let now = now_ms();
                let list = rows
                    .into_iter()
                    .fold(view(), |list, (sender, subject, at, ticket)| {
                        let b = press.clone();
                        list.child(
                            view()
                                .app_class(Some(format!("px-3 py-1.5 border-b {LINE}").into()))
                                .label("Open this mail")
                                .accessibility_value(Some(AccessibilityValue {
                                    text: Some(sender.clone().into()),
                                    ..AccessibilityValue::default()
                                }))
                                .on_press(move || {
                                    let open = b.open("mail", json!({"thread": ticket}));
                                    b.spawn(async move { drop(open.await) });
                                })
                                .child(
                                    view()
                                        .app_class(Some("flex-row justify-between gap-2".into()))
                                        .child(text(sender).number_of_lines(1).app_class(Some(
                                            format!("flex-1 text-[13px] {INK}").into(),
                                        )))
                                        .child(
                                            text(short_time(at, now)).app_class(Some(
                                                format!("text-[11px] {DIM}").into(),
                                            )),
                                        ),
                                )
                                .child(
                                    text(subject)
                                        .number_of_lines(1)
                                        .app_class(Some(format!("text-[12px] {MUTE}").into())),
                                ),
                        )
                    });
                if saved {
                    list.child(
                        view().app_class(Some("px-3 py-1".into())).child(
                            text("Saved mail, offline")
                                .app_class(Some(format!("text-[11px] {DIM}").into())),
                        ),
                    )
                } else {
                    list
                }
            }
        }))
}

fn help_view() -> impl IntoElement {
    let lines = [
        "j  next",
        "k  previous",
        "o or Enter  open",
        "u  close",
        "e  archive",
        "#  delete",
        "s  star",
        "Shift+I, Shift+U  read, unread",
        "x  pick",
        "z  undo",
        "c  write a message",
        "r, a, f  reply, reply all, forward",
        "Cmd+Enter  send",
        "/  search",
        "?  this help",
    ];
    lines.iter().fold(
        view().app_class(Some(format!("px-3 py-2 border-t {LINE} {RAIL}").into())),
        |v, l| v.child(text(*l).app_class(Some(format!("text-[12px] {INK}").into()))),
    )
}

pub fn build(bridge: Bridge) -> Root {
    buwiz_app::root(bridge, move |b| {
        let props = b.props();
        let prop = |k: &str| props.get(k).and_then(Value::as_str).map(str::to_owned);
        if b.viewport_now().placement == Placement::Inline {
            return match prop("draft") {
                Some(ticket) => crate::shell::card_frame().child(compose::card(b, ticket)),
                None => crate::shell::card_frame().child(card(b)),
            };
        }
        let (jobs, mut waiting) = unbounded_channel::<Job>();
        let (beside, mut alongside) = unbounded_channel::<Job>();
        let (writer, writing) = Compose::new();
        let m = Mail {
            chooser: Chooser::new(),
            compose: writer,
            problem: signal(None),
            told: signal(None),
            undoing: signal(None),
            list_turn: Latest::new(),
            open_turn: Latest::new(),
            labels: signal(column(&Value::Null)),
            label: signal(prop("view").unwrap_or_else(|| "INBOX".to_owned())),
            rows: signal(Vec::new()),
            more: signal(false),
            query: signal(String::new()),
            search: signal(prop("search")),
            chosen: signal(None),
            picked: signal(Vec::new()),
            undo: signal(None),
            labeling: signal(false),
            jobs: signal_with(jobs, SignalEquals::Never),
            beside: signal_with(beside, SignalEquals::Never),
            page: signal(None),
            links: signal(Vec::new()),
            images: signal(pictures::Pictures::default()),
            attachments: signal(Vec::new()),
            attachment_said: signal(None),
            status: signal(String::new()),
            saved: signal(false),
            signed_out: signal(false),
            link: signal(None),
            loading: signal(true),
            seq: signal(-1),
            help: signal(false),
        };
        keys(b.clone(), m);
        b.spawn(load_list(b.clone(), m, 0));
        b.spawn(load_labels(b.clone(), m));
        b.spawn(watch(b.clone(), m));
        b.spawn(compose::work(writing));
        b.spawn(compose::autosave(b.clone(), writer));
        b.spawn(async move {
            while let Some(job) = waiting.recv().await {
                job.await;
            }
        });
        let spawner = b.clone();
        b.spawn(async move {
            while let Some(job) = alongside.recv().await {
                spawner.spawn(job);
            }
        });
        if let Some(ticket) = prop("thread") {
            b.spawn(open_ticket(b.clone(), m, ticket));
        }
        if let Some(ticket) = prop("draft") {
            compose::resume_ticket(b.clone(), writer, ticket);
        }
        let (rail, list, guide) = (b.clone(), b.clone(), b.clone());
        let (undoing, pane, holding) = (b.clone(), b.clone(), b.clone());
        view()
            .app_class(Some(format!("flex-1 {BG}").into()))
            .child(dynamic(move || {
                if m.signed_out.get() {
                    return view()
                        .app_class(Some("flex-1".into()))
                        .child(signed_out_view(guide.clone()));
                }
                view()
                    .app_class(Some("flex-1".into()))
                    .child(
                        view()
                            .app_class(Some("flex-1 flex-row".into()))
                            .child(label_rail(rail.clone(), m))
                            .child(list_pane(list.clone(), m))
                            .child(reader_pane(pane.clone(), m)),
                    )
                    .child(compose::hold_bar(holding.clone(), writer))
                    .child(undo_bar(undoing.clone(), m))
                    .child(link_bar(m.link))
                    .child(dynamic(move || {
                        if m.help.get() {
                            view().child(help_view())
                        } else {
                            view()
                        }
                    }))
            }))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_column_is_the_system_boxes_then_the_persons_labels() {
        let v = json!({"labels": [{"id": "Label_2", "name": "Zebra", "kind": "user", "unread": 0}, {"id": "Label_1", "name": "Alpha", "kind": "user", "unread": 3}, {"id": "INBOX", "name": "INBOX", "kind": "system", "unread": 12}, {"id": "CATEGORY_SOCIAL", "name": "x", "kind": "system", "unread": 9}]});
        let c = column(&v);
        assert_eq!(
            c.iter().map(|l| l.name.as_str()).collect::<Vec<_>>(),
            [
                "Inbox",
                "Starred",
                "Important",
                "Sent",
                "Drafts",
                "Trash",
                "Alpha",
                "Zebra"
            ]
        );
        assert_eq!((c[0].unread, c[6].unread), (12, 3));
        assert_eq!(
            column(&Value::Null).len(),
            6,
            "before any sync, the boxes are there"
        );
    }

    #[test]
    fn every_setup_step_but_the_last_opens_a_page_on_googles_console() {
        let (last, rest) = SETUP.split_last().unwrap();
        assert!(last.2.is_none());
        assert!(rest.iter().all(|s| {
            s.2.is_some_and(|u| u.starts_with("https://console.cloud.google.com/"))
        }));
    }

    #[test]
    fn a_row_reads_what_it_can() {
        let row = Row::of(&json!({"id": "t", "sender": "Ana", "subject": "Hi", "at": 5, "unread": true, "messages": 2})).unwrap();
        assert_eq!(
            (
                row.sender.as_str(),
                row.at,
                row.unread,
                row.messages,
                row.starred
            ),
            ("Ana", 5, true, 2, false)
        );
        assert_eq!(Row::of(&json!({"sender": "no id"})), None);
    }

    #[test]
    fn times_read_like_a_mail_list() {
        let now = 1_793_955_600_000;
        let utc = |_| 0;
        assert_eq!(time_in(now - 3_600_000, now, utc), "08:00");
        assert_eq!(time_in(now - 2 * 86_400_000, now, utc), "Nov 4");
        // eight hours east: the clock moves, and so can the day
        let manila = |_| 8 * 3600;
        assert_eq!(time_in(now - 3_600_000, now, manila), "16:00");
        assert_eq!(time_in(now + 12 * 3_600_000, now, manila), "Nov 7");
    }

    #[test]
    fn the_notice_says_what_a_person_can_use() {
        assert_eq!(notice("ok", false, false), None);
        assert_eq!(notice("syncing", false, false), None);
        assert_eq!(
            notice("ok", true, false),
            Some("Can't reach Google. Showing saved mail.")
        );
        assert_eq!(
            notice("offline", false, false),
            Some("Can't reach Google. Showing saved mail.")
        );
        assert_eq!(
            notice("offline", false, true),
            Some("Can't reach Google."),
            "no saved mail claimed over none"
        );
        assert_eq!(
            notice("failed", false, false),
            Some("Couldn't update your mail. Showing what's saved.")
        );
        assert_eq!(
            notice("failed", false, true),
            Some("Couldn't update your mail."),
            "no saved mail claimed over none"
        );
        assert!(
            notice("needs_access", false, false)
                .unwrap()
                .contains("Sign in again")
        );
    }

    #[test]
    fn a_search_outranks_a_label_and_a_query_outranks_a_card() {
        let card = || Some("tkt".to_owned());
        assert_eq!(
            list_args("", card(), "INBOX".into(), 40),
            json!({"search": "tkt"})
        );
        assert_eq!(
            list_args("q", card(), "INBOX".into(), 40),
            json!({"query": "q"})
        );
    }

    #[test]
    fn an_empty_list_says_which_list_and_never_before_the_first_sync() {
        let line = |label, status| empty_line(label, false, status, false);
        assert_eq!(
            line("INBOX", "ok"),
            "Your inbox is empty. Press Write to start a message."
        );
        assert_eq!(line("TRASH", "ok"), "Trash is empty.");
        assert_eq!(line("Label_7", "ok"), "No conversations have this label.");
        for early in ["", "syncing"] {
            assert_eq!(line("INBOX", early), "Your mail is on its way from Google…");
        }
        for trouble in ["offline", "failed", "needs_access"] {
            assert_eq!(line("INBOX", trouble), "No mail saved here yet.");
        }
        assert_eq!(
            empty_line("INBOX", false, "ok", true),
            "No mail saved here yet."
        );
        assert_eq!(
            empty_line("INBOX", true, "syncing", false),
            "No mail matches that. Try other words."
        );
    }

    #[test]
    fn a_page_adds_only_rows_the_list_lacks() {
        let row = |id: &str| Row::of(&json!({"id": id})).expect("a row");
        let page = new_rows(&[row("a"), row("b")], vec![row("b"), row("c")]);
        assert_eq!(
            page.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["c"]
        );
    }
}
