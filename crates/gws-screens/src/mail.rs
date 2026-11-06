//! Mail: labels, the list and the reader, on one screen of its own.
//!
//! The label rule holds here by construction. A row and a label carry a fixed
//! `label`, and sender, subject and label names sit in `accessibility_value`,
//! which `ui_look` never sends. What a mail says lives in the web view, which
//! no tree shows.

use std::collections::HashSet;
use std::time::Duration;

use buwiz_app::{Bridge, Placement};
use nsh_core::AccessibilityValue;
use nsh_core::element::{Root, dark, input};
use nsh_core::prelude::*;
use serde_json::{Value, json};

use crate::reader;
use crate::shell::{BG, MUTE, RETRIES, backoff, sleep};

const INK: &str = "text-[#1c1c1c] dark:text-[#f0f0f0]";
const RAIL: &str = "bg-[#efefec] dark:bg-[#0f0f0f]";
const PANEL: &str = "bg-white dark:bg-[#1d1d1d]";
const LINE: &str = "border-black/10 dark:border-[#2b2b2b]";
const CHOSEN: &str = "bg-[#e1ebfb] dark:bg-[#15253f]";
const DIM: &str = "text-[#8a8a8a] dark:text-[#6e6e6e]";
/// The owner hasn't decided what a card shows without a click (G11). This is
/// the one switch: `false` leaves senders and times only.
const CARD_SUBJECTS: bool = true;
const ROW_HEIGHT: f32 = 52.0;
/// How often an open screen asks whether the store changed.
const POLL: Duration = Duration::from_secs(15);

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

/// A row's time: today's as the clock, older as a date. UTC until the Bot
/// gives a screen the person's zone.
pub fn short_time(at: i64, now: i64) -> String {
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

/// The line under the search box, when there is something to say.
pub fn notice(status: &str, saved: bool) -> Option<&'static str> {
    match (status, saved) {
        (_, true) | ("offline", _) => Some("Showing saved mail"),
        ("needs_access", _) => {
            Some("Your Google sign-in needs more access. Sign in again on this app's page.")
        }
        ("failed", _) => Some("Couldn't update your mail. Showing what's saved."),
        _ => None,
    }
}

/// Which of several answers out at once still counts: the latest asked for.
#[derive(Clone, Copy)]
struct Latest(Signal<u64>);

impl Latest {
    fn new() -> Self {
        Self(signal(0))
    }

    /// A new question: every earlier one stops counting.
    fn ask(self) -> u64 {
        let turn = self.0.get_untracked() + 1;
        self.0.set(turn);
        turn
    }

    fn counts(self, turn: u64) -> bool {
        self.0.get_untracked() == turn
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
    list_turn: Latest,
    open_turn: Latest,
    labels: Signal<Vec<LabelRow>>,
    label: Signal<String>,
    rows: Signal<Vec<Row>>,
    more: Signal<bool>,
    query: Signal<String>,
    /// A card's search, by its ticket, until the person goes elsewhere.
    search: Signal<Option<String>>,
    chosen: Signal<Option<usize>>,
    page: Signal<Option<String>>,
    links: Signal<Vec<String>>,
    images: Signal<i64>,
    status: Signal<String>,
    saved: Signal<bool>,
    signed_out: Signal<bool>,
    link: Signal<Option<String>>,
    loading: Signal<bool>,
    seq: Signal<i64>,
    help: Signal<bool>,
}

/// A read through the bridge, tried again while the answer is only busy.
async fn read(b: &Bridge, tool: &str, args: Value) -> Result<Value, String> {
    for attempt in 0.. {
        match b.call(tool, args.clone()).await {
            Ok(v) => return Ok(v),
            Err(e) if e.retryable && attempt < RETRIES => sleep(backoff(attempt)).await,
            Err(e) if e.message.contains("Sign in to Google") => return Err("sign-in".to_owned()),
            Err(e) => return Err(e.message),
        }
    }
    Err("That didn't work. Try again.".to_owned())
}

/// What the list shows now, as `gws_mail_list` asks for it.
fn list_args(query: &str, search: Option<String>, label: String, skip: usize) -> Value {
    match search {
        Some(ticket) if query.is_empty() => json!({"search": ticket}),
        _ if query.is_empty() => json!({"label": label, "skip": skip}),
        _ => json!({"query": query}),
    }
}

async fn load_list(b: Bridge, m: Mail, skip: usize) {
    let turn = m.list_turn.ask();
    let args = list_args(
        &m.query.get_untracked(),
        m.search.get_untracked(),
        m.label.get_untracked(),
        skip,
    );
    m.loading.set(true);
    let answer = read(&b, "gws_mail_list", args).await;
    // a later load owns the list and the flag now
    if !m.list_turn.counts(turn) {
        return;
    }
    m.loading.set(false);
    match answer {
        Ok(v) => {
            let mut rows: Vec<Row> = v
                .get("threads")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Row::of)
                .collect();
            if skip > 0 {
                let mut all = m.rows.get_untracked();
                all.append(&mut new_rows(&all, rows));
                rows = all;
            }
            m.rows.set(rows);
            m.more.set(v.get("more") == Some(&Value::Bool(true)));
            m.saved.set(v.get("saved") == Some(&Value::Bool(true)));
            m.signed_out.set(false);
        }
        Err(e) if e == "sign-in" => m.signed_out.set(true),
        Err(e) => m.status.set(e),
    }
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
    m.chosen.set(Some(index));
    show(b, m, id).await;
}

/// A card's row, opened in the reader.
async fn open_ticket(b: Bridge, m: Mail, ticket: String) {
    show(b, m, ticket).await;
}

async fn show(b: Bridge, m: Mail, thread: String) {
    let turn = m.open_turn.ask();
    m.page.set(None);
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
            m.images
                .set(v.get("images").and_then(Value::as_i64).unwrap_or(0));
            m.page.set(Some(
                v.get("page")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            ));
        }
        Err(e) => m
            .page
            .set(Some(format!("<p>{}</p>", e.replace('<', "&lt;")))),
    }
}

async fn watch(b: Bridge, m: Mail) {
    loop {
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
                load_list(b.clone(), m, 0).await;
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
    let list = dynamic(move || {
        let chosen = m.label.get();
        m.labels
            .get()
            .into_iter()
            .fold(view().app_class(Some("gap-[1px]".into())), |column, l| {
                let (id, on, b) = (l.id.clone(), l.id == chosen, b.clone());
                let count = if l.unread > 0 {
                    l.unread.to_string()
                } else {
                    String::new()
                };
                column.child(
                    view()
                        .app_class(Some(press_class(on).into()))
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
                            m.open_turn.ask();
                            m.page.set(None);
                            b.spawn(load_list(b.clone(), m, 0));
                        })
                        .child(text(l.name).app_class(Some(format!("text-[13px] {INK}").into())))
                        .child(text(count).app_class(Some(format!("text-[12px] {MUTE}").into()))),
                )
            })
    });
    scroll()
        .app_class(Some(
            format!("w-[190px] pt-3 border-r {LINE} {RAIL}").into(),
        ))
        .child(list)
}

fn row_view(b: Bridge, m: Mail, index: usize) -> impl IntoElement {
    let at = move |f: fn(&Row) -> String| {
        move || m.rows.with(|r| r.get(index).map(f).unwrap_or_default())
    };
    let chosen = move || m.chosen.get() == Some(index);
    let weight = move || m.rows.with(|r| r.get(index).is_some_and(|r| r.unread));
    view()
        .app_class(move || {
            Some(
                format!(
                    "h-[{ROW_HEIGHT}px] px-3 py-1.5 border-b {LINE} {}",
                    if chosen() { CHOSEN } else { "" }
                )
                .into(),
            )
        })
        .label("Open conversation")
        .accessibility_value(move || {
            m.rows.with(|r| {
                r.get(index).map(|r| AccessibilityValue {
                    text: Some(format!("{}, {}", r.sender, r.subject).into()),
                    ..AccessibilityValue::default()
                })
            })
        })
        .on_press(move || {
            b.spawn(open(b.clone(), m, index));
        })
        .child(
            view()
                .app_class(Some("flex-row justify-between gap-2".into()))
                .child(
                    text(at(|r| r.sender.clone()))
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
                    text(at(|r| short_time(r.at, now_ms())))
                        .app_class(Some(format!("text-[11px] {DIM}").into())),
                ),
        )
        .child(
            text(at(|r| {
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
            text(at(|r| r.snippet.clone()))
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
        let line = match (m.loading.get(), m.query.with(String::is_empty)) {
            (true, _) => "Loading your mail…",
            (false, true) => "Nothing here",
            (false, false) => "No mail matches that.",
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
                            submit.spawn(load_list(submit.clone(), m, 0));
                        }),
                ),
        )
        .child(dynamic(move || {
            match notice(&m.status.get(), m.saved.get()) {
                Some(line) => view()
                    .app_class(Some(format!("px-3 py-1 border-b {LINE}").into()))
                    .child(text(line).app_class(Some(format!("text-[11px] {MUTE}").into()))),
                None => view(),
            }
        }))
        .child(
            virtual_list(
                move || m.rows.with(Vec::len),
                move |at| row_view(row_bridge.clone(), m, at.index()),
            )
            // cells are recycled: without identities a list that changes keeps
            // each cell where the last list left it, out of order on screen
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

fn reader_pane(m: Mail) -> impl IntoElement {
    view()
        .app_class(Some(format!("flex-1 {PANEL}").into()))
        .child(dynamic(move || match m.page.get() {
            Some(page) => view()
                .app_class(Some("flex-1".into()))
                .child(dynamic(move || {
                    let n = m.images.get();
                    if n > 0 {
                        view()
                            .app_class(Some(format!("px-3 py-1.5 border-b {LINE}").into()))
                            .child(
                                text(format!(
                                    "{n} image{} blocked",
                                    if n == 1 { "" } else { "s" }
                                ))
                                .app_class(Some(format!("text-[12px] {MUTE}").into())),
                            )
                    } else {
                        view()
                    }
                }))
                .child(reader::view(
                    move || reader::seal(&page, dark()),
                    move |n| m.link.set(m.links.with_untracked(|l| l.get(n).cloned())),
                )),
            None if m.chosen.get().is_some() => view()
                .app_class(Some("flex-1 items-center justify-center".into()))
                .child(text("Opening…").app_class(Some(format!("text-[13px] {MUTE}").into()))),
            None => view()
                .app_class(Some("flex-1 items-center justify-center".into()))
                .child(
                    text("Pick a conversation to read it")
                        .app_class(Some(format!("text-[13px] {MUTE}").into())),
                ),
        }))
}

/// The confirm a link needs: the address in full, before anything opens.
fn link_bar(m: Mail) -> impl IntoElement {
    dynamic(move || match m.link.get() {
        None => view(),
        Some(url) => {
            let target = url.clone();
            view()
                .app_class(Some(
                    format!("flex-row items-center gap-3 px-3 py-2 border-t {LINE} {RAIL}").into(),
                ))
                .child(
                    text(format!("Open {url}?"))
                        .number_of_lines(2)
                        .app_class(Some(format!("flex-1 text-[12px] {INK}").into())),
                )
                .child(
                    view()
                        .app_class(Some(
                            "px-3 py-1 rounded-md bg-[#2f6fd6] dark:bg-[#7fb2ff]".into(),
                        ))
                        .label("Open this link")
                        .on_press(move || {
                            m.link.set(None);
                            drop(open_url(target.clone()));
                        })
                        .child(
                            text("Open").app_class(Some(
                                "text-[12px] text-white dark:text-[#10151c]".into(),
                            )),
                        ),
                )
                .child(
                    view()
                        .app_class(Some("px-3 py-1".into()))
                        .label("Don't open this link")
                        .on_press(move || m.link.set(None))
                        .child(text("Cancel").app_class(Some(format!("text-[12px] {INK}").into()))),
                )
        }
    })
}

fn keys(b: Bridge, m: Mail) {
    let step = move |by: isize| {
        let len = m.rows.with_untracked(Vec::len) as isize;
        let at = m.chosen.get_untracked().map_or(-1, |i| i as isize);
        ((at + by).clamp(0, (len - 1).max(0))) as usize
    };
    let (down, up, enter, oh) = (b.clone(), b.clone(), b.clone(), b);
    on_key("j", move |_| {
        down.spawn(open(down.clone(), m, step(1)));
    });
    on_key("k", move |_| {
        up.spawn(open(up.clone(), m, step(-1)));
    });
    on_key("enter", move |_| {
        enter.spawn(open(
            enter.clone(),
            m,
            m.chosen.get_untracked().unwrap_or(0),
        ));
    });
    on_key("o", move |_| {
        oh.spawn(open(oh.clone(), m, m.chosen.get_untracked().unwrap_or(0)));
    });
    on_key("u", move |_| {
        m.chosen.set(None);
        m.page.set(None);
    });
    on_key("shift+/", move |_| m.help.set(!m.help.get_untracked()));
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
                                .child(if CARD_SUBJECTS {
                                    text(subject)
                                        .number_of_lines(1)
                                        .app_class(Some(format!("text-[12px] {MUTE}").into()))
                                } else {
                                    text(String::new())
                                }),
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
        if b.viewport_now().placement == Placement::Inline {
            return view().child(card(b));
        }
        let props = b.props();
        let prop = |k: &str| props.get(k).and_then(Value::as_str).map(str::to_owned);
        let m = Mail {
            list_turn: Latest::new(),
            open_turn: Latest::new(),
            labels: signal(column(&Value::Null)),
            label: signal("INBOX".to_owned()),
            rows: signal(Vec::new()),
            more: signal(false),
            query: signal(String::new()),
            search: signal(prop("search")),
            chosen: signal(None),
            page: signal(None),
            links: signal(Vec::new()),
            images: signal(0),
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
        if let Some(ticket) = prop("thread") {
            b.spawn(open_ticket(b.clone(), m, ticket));
        }
        let (rail, list, guide) = (b.clone(), b.clone(), b.clone());
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
                            .child(reader_pane(m)),
                    )
                    .child(link_bar(m))
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
        assert_eq!(short_time(now - 3_600_000, now), "08:00");
        assert_eq!(short_time(now - 2 * 86_400_000, now), "Nov 4");
    }

    #[test]
    fn the_notice_says_what_a_person_can_use() {
        assert_eq!(notice("ok", false), None);
        assert_eq!(notice("syncing", false), None);
        assert_eq!(notice("ok", true), Some("Showing saved mail"));
        assert_eq!(notice("offline", false), Some("Showing saved mail"));
        assert!(
            notice("needs_access", false)
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
    fn a_page_adds_only_rows_the_list_lacks() {
        let row = |id: &str| Row::of(&json!({"id": id})).expect("a row");
        let page = new_rows(&[row("a"), row("b")], vec![row("b"), row("c")]);
        assert_eq!(
            page.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["c"]
        );
    }
}
