//! Mail for the screens: a background sync into the store, and the tools that
//! read it. The screens read what's stored, so a slow or missing connection
//! shows as old mail, not as a blank window. Google is asked only by the sync,
//! by a search, and for a thread not yet opened.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::{Local, TimeZone as _, Utc};
use gws_store::Store;
use gws_store::mail::{Label, Thread};
use serde_json::{Value, json};

use crate::accounts::Bearer;
use crate::clean;
use crate::gmail::{self, Failure};
use crate::tools::{
    MAIL_ARCHIVE, MAIL_CARD, MAIL_CHANGES, MAIL_DRAFT_DELETE, MAIL_DRAFT_OF, MAIL_DRAFT_READ,
    MAIL_DRAFT_SAVE, MAIL_JOB, MAIL_JOB_START, MAIL_LABEL, MAIL_LABELS, MAIL_LIST, MAIL_MARK,
    MAIL_REPLY_START, MAIL_SEND, MAIL_SEND_ARM, MAIL_SEND_DISARM, MAIL_THREAD, MAIL_TRASH,
    MAIL_UNDO, PEOPLE_SUGGEST, SEARCH_MAIL, UPLOAD_CHUNK, UPLOAD_DROP, fail, failed, ok,
};
use crate::{Core, compose, js};

/// Rows a list page holds.
pub const PAGE: i64 = 50;
/// What a thread's view may come to: a result stops at 1 MiB.
const VIEW_MAX: usize = 900 * 1024;
/// The most conversations one action takes.
const ACT_MAX: usize = 100;
/// How many actions can still be undone.
const UNDOS_MAX: usize = 20;
/// A generation of [`Tickets`].
const TICKETS_MAX: usize = 512;
const TOO_LONG: &str = "<p>This message is too long to show here.</p>";
/// How often the sync runs with a screen open, and with none.
const ACTIVE: Duration = Duration::from_secs(30);
const IDLE: Duration = Duration::from_secs(300);
/// A screen counts as open this long after its last call.
const LINGER: Duration = Duration::from_secs(120);
const SEQ: &str = "mail_seq";
const HISTORY: &str = "mail_history";
const STATUS: &str = "mail_status";

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// What a card's ticket stands for.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Subject {
    Search(String),
    Thread(String),
    Draft(String),
    /// An event draft's fields, as JSON: it's nowhere else until it's saved.
    EventDraft(String),
    /// The words of a search of Drive.
    DriveSearch(String),
}

/// How many rows a card shows.
const CARD_ROWS: usize = 5;

/// Offers a card with a chat's mail search. Nothing about the mail goes in it:
/// props hold a ticket, and the card reads its rows itself.
pub fn offer_card(core: &Core, tool: &str, args: &Value, result: &mut Value) {
    if tool == MAIL_DRAFT_SAVE {
        let saved = result.pointer("/structuredContent/draft");
        if let Some(draft) = saved.and_then(Value::as_str) {
            let ticket = core
                .tickets
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .issue(&Subject::Draft(draft.to_owned()));
            result["_meta"] = json!({"buwiz/open": {"app": "mail", "props": {"draft": ticket}, "title": "Mail", "placement": "inline"}});
        }
        return;
    }
    let found = result
        .pointer("/structuredContent/items")
        .and_then(Value::as_array)
        .is_some_and(|i| !i.is_empty());
    if tool != SEARCH_MAIL || !found {
        return;
    }
    let query = js::trim(&js::string_or_empty(args.get("query"))).to_owned();
    let ticket = core
        .tickets
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .issue(&Subject::Search(query));
    result["_meta"] = json!({"buwiz/open": {"app": "mail", "props": {"search": ticket}, "title": "Mail", "placement": "inline"}});
}

/// What one action changed, kept so Undo can put it back exactly: each
/// message's own labels, since a thread's messages can differ.
#[derive(Default)]
pub struct Undos {
    next: u64,
    kept: VecDeque<(String, Undo)>,
}

struct Undo {
    account: String,
    threads: Vec<(String, gmail::Messages)>,
    trashed: bool,
}

impl Undos {
    fn keep(&mut self, undo: Undo) -> String {
        self.next += 1;
        let id = format!("u{}", self.next);
        self.kept.push_back((id.clone(), undo));
        if self.kept.len() > UNDOS_MAX {
            self.kept.pop_front();
        }
        id
    }

    fn put_back(&mut self, id: String, undo: Undo) {
        self.kept.push_back((id, undo));
    }

    fn take(&mut self, id: &str) -> Option<Undo> {
        let at = self.kept.iter().position(|(k, _)| k == id)?;
        self.kept.remove(at).map(|(_, undo)| undo)
    }
}

fn strings(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|s| s.as_str().map(str::to_owned))
        .collect()
}

/// A write's labels: what to add, what to take off.
fn change(tool: &str, args: &Value) -> (Vec<String>, Vec<String>) {
    let (mut add, mut remove) = (Vec::new(), Vec::new());
    let mut flag = |on: Option<bool>, label: &str| match on {
        Some(true) => add.push(label.to_owned()),
        Some(false) => remove.push(label.to_owned()),
        None => {}
    };
    match tool {
        MAIL_MARK => {
            flag(
                args.get("read").and_then(Value::as_bool).map(|r| !r),
                "UNREAD",
            );
            flag(args.get("starred").and_then(Value::as_bool), "STARRED");
        }
        MAIL_ARCHIVE => flag(Some(false), "INBOX"),
        MAIL_LABEL => {
            add = strings(args.get("add"));
            remove = strings(args.get("remove"));
        }
        _ => {}
    }
    (add, remove)
}

/// A person's action on conversations. It stops at the first that fails, and
/// what went before it can still be undone.
fn act(core: &Core, tool: &str, args: &Value) -> Result<Value, Failure> {
    let (auth, account) = signed_in(core)?;
    let ids = strings(args.get("threads"));
    let (add, remove) = change(tool, args);
    let bad = ids.is_empty()
        || ids.len() > ACT_MAX
        || !ids
            .iter()
            .chain(&add)
            .chain(&remove)
            .all(|i| gmail::id_ok(i));
    if bad {
        return Ok(fail("invalid_args", "That isn't a set of conversations."));
    }
    let trashed = tool == MAIL_TRASH;
    let mut threads = Vec::new();
    let mut failure = None;
    for id in &ids {
        let done = core.gmail.thread_labels(&auth, id).and_then(|before| {
            if trashed {
                core.gmail.trash_thread(&auth, id)?;
            } else {
                core.gmail.modify_thread(&auth, id, &add, &remove)?;
            }
            Ok(before)
        });
        match done {
            Ok(before) => threads.push((id.clone(), before.unwrap_or_default())),
            Err(e) => {
                failure = Some(e);
                break;
            }
        }
    }
    let changed: Vec<String> = threads.iter().map(|(id, _)| id.clone()).collect();
    // the writes have happened, so the way back is kept before anything else
    let undo = (!threads.is_empty()).then(|| {
        core.undos
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keep(Undo {
                account: account.clone(),
                threads,
                trashed,
            })
    });
    if !changed.is_empty() {
        // a failed read here is only a stale row: the sync below mends it
        let _ = fetch_rows(core, &auth, &account, &changed);
    }
    core.sync.wake();
    match failure {
        Some(e) if undo.is_none() => Err(e),
        Some(_) => Ok(ok(
            json!({"undo": undo, "done": changed.len(), "threads": changed, "partial": true}),
        )),
        None => Ok(ok(
            json!({"undo": undo, "done": changed.len(), "threads": changed}),
        )),
    }
}

/// Puts back one thread's messages, each to the labels it had.
fn restore(
    core: &Core,
    auth: &Bearer,
    thread: &str,
    messages: &gmail::Messages,
    trashed: bool,
) -> Result<(), Failure> {
    if trashed {
        core.gmail.untrash_thread(auth, thread)?;
    }
    let now = core.gmail.thread_labels(auth, thread)?.unwrap_or_default();
    for (message, before) in messages {
        let Some((_, labels)) = now.iter().find(|(m, _)| m == message) else {
            continue;
        };
        let add: Vec<String> = before
            .iter()
            .filter(|l| !labels.contains(l))
            .cloned()
            .collect();
        let remove: Vec<String> = labels
            .iter()
            .filter(|l| !before.contains(l))
            .cloned()
            .collect();
        if !add.is_empty() || !remove.is_empty() {
            core.gmail.modify_message(auth, message, &add, &remove)?;
        }
    }
    Ok(())
}

fn undo(core: &Core, args: &Value) -> Result<Value, Failure> {
    let (auth, account) = signed_in(core)?;
    let id = js::string_or_empty(args.get("undo"));
    let held = core
        .undos
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take(&id);
    let Some(mut undo) = held.filter(|u| u.account == account) else {
        return Ok(fail("not_found", "That can't be undone any more."));
    };
    let ids: Vec<String> = undo.threads.iter().map(|(id, _)| id.clone()).collect();
    while let Some((thread, messages)) = undo.threads.first() {
        if let Err(e) = restore(core, &auth, thread, messages, undo.trashed) {
            // what's left can be tried again, under the same id
            core.undos
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .put_back(id, undo);
            return Err(e);
        }
        undo.threads.remove(0);
    }
    let _ = fetch_rows(core, &auth, &account, &ids);
    core.sync.wake();
    Ok(ok(json!({"done": ids.len()})))
}

/// Tickets in two generations, so they can't grow without end: when the newer
/// fills, the older goes, and a card that old says it's out of date.
#[derive(Default)]
pub struct Tickets {
    newer: buwiz_plugin::Tickets<Subject>,
    older: buwiz_plugin::Tickets<Subject>,
    issued: usize,
}

impl Tickets {
    pub fn issue(&mut self, subject: &Subject) -> String {
        if self.issued >= TICKETS_MAX {
            self.older = std::mem::take(&mut self.newer);
            self.issued = 0;
        }
        self.issued += 1;
        self.newer.issue(subject)
    }

    pub fn subject(&self, ticket: &str) -> Option<&Subject> {
        self.newer
            .subject(ticket)
            .or_else(|| self.older.subject(ticket))
    }
}

/// A card's rows, each with a ticket to open it by.
fn card(core: &Core, args: &Value) -> Result<Value, Failure> {
    let (auth, account) = signed_in(core)?;
    let held = core
        .tickets
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .subject(&js::string_or_empty(args.get("ticket")))
        .cloned();
    let Some(Subject::Search(query)) = held else {
        return Ok(fail("not_found", "This card is out of date."));
    };
    let (rows, saved) = match search(core, &auth, &account, &query) {
        Ok(rows) => (rows, false),
        Err(Failure::Offline(_)) => (
            with_store(core, |store| {
                store.search_threads(&account, &query, CARD_ROWS as i64)
            })?,
            true,
        ),
        Err(e) => return Err(e),
    };
    let more = rows.len() > CARD_ROWS;
    let mut tickets = core.tickets.lock().unwrap_or_else(|e| e.into_inner());
    let rows: Vec<Value> = rows
        .iter()
        .take(CARD_ROWS)
        .map(|t| json!({"sender": t.sender, "subject": t.subject, "at": t.at, "unread": t.unread, "ticket": tickets.issue(&Subject::Thread(t.id.clone()))}))
        .collect();
    Ok(ok(json!({"rows": rows, "more": more, "saved": saved})))
}

/// Wakes the sync, remembers when a screen last asked for something, and when
/// it next has to look whatever else happens.
#[derive(Default)]
pub struct Waker {
    state: Mutex<(bool, Option<Instant>, Option<Instant>)>,
    bell: Condvar,
    stop: AtomicBool,
}

impl Waker {
    /// Asks the sync thread to end after what it's doing.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        self.wake();
    }

    fn stopping(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    pub fn wake(&self) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).0 = true;
        self.bell.notify_all();
    }

    pub(crate) fn seen(&self) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).1 = Some(Instant::now());
    }

    /// Wakes the sync `after` this long, in place of any such time set before.
    pub(crate) fn wake_after(&self, after: Duration) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).2 = Some(Instant::now() + after);
        self.bell.notify_all();
    }

    fn wait(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let every = if state.1.is_some_and(|at| at.elapsed() < LINGER) {
            ACTIVE
        } else {
            IDLE
        };
        let every = state.2.map_or(every, |at| {
            every.min(at.saturating_duration_since(Instant::now()))
        });
        if !state.0 {
            state = self
                .bell
                .wait_timeout(state, every)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        state.0 = false;
        if state.2.is_some_and(|at| at <= Instant::now()) {
            state.2 = None;
        }
    }
}

/// How long the Bot's pushes must have stopped before they count as finished.
/// It sends them one at a time, and can refresh a token between two.
pub const SETTLE_MS: i64 = 60_000;
/// Past [`SETTLE_MS`], so the wake lands after the wait is over and not on it.
const SETTLE_PAD: Duration = Duration::from_millis(50);

/// Whether the Bot's pushes have finished. Only then does an account nobody
/// pushed count as signed out: until the last one, it may just be next in line.
pub struct Settle {
    wait_ms: AtomicI64,
    last_ms: AtomicI64,
    /// A push came, and what nobody holds hasn't been cleared since.
    owed: AtomicBool,
}

impl Default for Settle {
    fn default() -> Self {
        Self {
            wait_ms: AtomicI64::new(SETTLE_MS),
            last_ms: AtomicI64::new(0),
            owed: AtomicBool::new(false),
        }
    }
}

impl Settle {
    /// A test's shorter wait.
    pub fn wait_for(&self, ms: i64) {
        self.wait_ms.store(ms, Ordering::Relaxed);
    }

    /// A push came now. Says how long until they may have finished.
    pub(crate) fn pushed(&self, now_ms: i64) -> Duration {
        self.last_ms.store(now_ms, Ordering::Relaxed);
        self.owed.store(true, Ordering::Relaxed);
        Duration::from_millis(self.wait_ms.load(Ordering::Relaxed).max(0) as u64) + SETTLE_PAD
    }

    fn quiet(&self, now_ms: i64) -> bool {
        now_ms - self.last_ms.load(Ordering::Relaxed) >= self.wait_ms.load(Ordering::Relaxed)
    }
}

/// Starts the background sync.
pub fn start(core: Arc<Core>) -> JoinHandle<()> {
    std::thread::spawn(move || {
        loop {
            core.sync.wait();
            if core.sync.stopping() {
                return;
            }
            if core.provisioned() {
                let _ = sync(&core);
                crate::files::sweep(&core);
            }
        }
    })
}

/// Writes down whose each label on the Bot's list is, and → those accounts: a
/// listed account keeps its mail, though after a restart only the active one is
/// pushed. A label off the list goes from the record too. With no list, as from
/// an older Bot, nothing is written and nobody is kept by it.
pub(crate) fn file_sign_ins(core: &Core, store: &Store) -> gws_store::Result<HashSet<String>> {
    let Some(roster) = core.accounts.roster() else {
        return Ok(HashSet::new());
    };
    let filed: HashMap<String, String> = store.sign_ins()?.into_iter().collect();
    let rows: Vec<(String, String)> = roster
        .into_iter()
        .filter_map(|(listed, who)| {
            let who = who.or_else(|| filed.get(&listed.label).cloned())?;
            Some((listed.label, who))
        })
        .collect();
    store.set_sign_ins(&rows)?;
    Ok(rows.into_iter().map(|(_, who)| who).collect())
}

/// Whose sign-in `args` name, as last filed.
pub(crate) fn filed_subject(core: &Core, args: &Value) -> Option<String> {
    let label = args.get("label").and_then(Value::as_str)?;
    let filed = with_store(core, |store| store.sign_ins()).ok()?;
    let row = filed.into_iter().find(|(filed, _)| filed == label)?;
    Some(row.1)
}

/// Clears every account in the store that no sign-in holds, whether or not
/// this process ever saw it signed in. False if it couldn't, and then a marker
/// on disk has the next sync try again, even after a restart.
pub fn reconcile(core: &Core) -> bool {
    let cleared = with_store(core, |store| {
        let listed = file_sign_ins(core, store)?;
        for account in store.accounts()? {
            if !core.accounts.holds(&account) && !listed.contains(&account) {
                crate::files::forget_account(core, store, &account)?;
                store.forget_account(&account)?;
            }
        }
        Ok(())
    })
    .is_ok();
    if let Ok(paths) = &core.paths {
        let marker = paths.wipe_pending();
        let marked = if cleared {
            std::fs::remove_file(&marker).or_else(|e| match e.kind() {
                std::io::ErrorKind::NotFound => Ok(()),
                _ => Err(e),
            })
        } else {
            std::fs::write(&marker, b"")
        };
        if marked.is_err() {
            eprintln!("buwiz-gws: couldn't keep track of a mail wipe");
        }
    }
    cleared
}

/// A sign-in of `account` ended: unless another holds it, its own mail goes now,
/// and no one else's, as the Bot may have pushes still to come. False if it
/// couldn't, and then the marker has it cleared with the rest.
pub(crate) fn forget(core: &Core, account: &str) -> bool {
    let cleared = with_store(core, |store| {
        let listed = file_sign_ins(core, store)?;
        if !core.accounts.holds(account) && !listed.contains(account) {
            crate::files::forget_account(core, store, account)?;
            store.forget_account(account)?;
        }
        Ok(())
    })
    .is_ok();
    if !cleared
        && let Ok(paths) = &core.paths
        && std::fs::write(paths.wipe_pending(), b"").is_err()
    {
        eprintln!("buwiz-gws: couldn't keep track of a mail wipe");
    }
    cleared
}

/// Clears what no sign-in holds, once the Bot's pushes have finished, and with
/// it a wipe that failed earlier, in this run or a past one. Not before the
/// first push: until then nobody looks signed in, and everyone's mail would go.
pub fn finish_wipes(core: &Core) {
    if !core.pushed.load(Ordering::Relaxed) || !core.settle.quiet(core.clock.now_ms()) {
        return;
    }
    let failed = core.paths.as_ref().is_ok_and(|p| p.wipe_pending().exists());
    // taken first: a push from here on is owed a clear of its own
    if core.settle.owed.swap(false, Ordering::Relaxed) || failed {
        reconcile(core);
    }
}

/// [`with_store`] for a write, which only goes in while `account` is signed
/// in. Checked under the store's lock, which a sign-out's clear takes too, so
/// a sync still in flight can't put back what was cleared.
pub(crate) fn with_own_store<T>(
    core: &Core,
    account: &str,
    f: impl FnOnce(&Store) -> gws_store::Result<T>,
) -> Result<T, Failure> {
    let held = core.store().map_err(|e| Failure::Plain(e.message))?;
    if !core.accounts.holds(account) {
        return Err(Failure::AccountRequired);
    }
    let store = held
        .as_ref()
        .ok_or_else(|| Failure::Plain("The local mail store isn't open.".to_owned()))?;
    f(store).map_err(|e| Failure::Plain(e.message))
}

pub(crate) fn with_store<T>(
    core: &Core,
    f: impl FnOnce(&Store) -> gws_store::Result<T>,
) -> Result<T, Failure> {
    let held = core.store().map_err(|e| Failure::Plain(e.message))?;
    let store = held
        .as_ref()
        .ok_or_else(|| Failure::Plain("The local mail store isn't open.".to_owned()))?;
    f(store).map_err(|e| Failure::Plain(e.message))
}

pub(crate) fn signed_in(core: &Core) -> Result<(Bearer, String), Failure> {
    core.accounts.session().ok_or(Failure::AccountRequired)
}

/// `Ana Reyes <ana@example.com>` as (name, address); a bare address is both.
pub fn sender_of(from: &str) -> (String, String) {
    match from.rsplit_once('<') {
        Some((name, rest)) => {
            let address = rest.trim_end_matches('>').trim().to_owned();
            let name = name.trim().trim_matches('"').trim();
            (
                if name.is_empty() {
                    address.clone()
                } else {
                    name.to_owned()
                },
                address,
            )
        }
        None => (from.trim().to_owned(), from.trim().to_owned()),
    }
}

fn millis(message: &Value) -> i64 {
    message
        .get("internalDate")
        .and_then(Value::as_str)
        .and_then(|d| d.parse().ok())
        .unwrap_or(0)
}

/// Who the person's latest message in a thread went to: "Ana Reyes", or "Ana
/// Reyes and 2 more". Empty when the person wrote nothing there.
fn recipient_of(messages: &[Value]) -> String {
    let theirs = messages.iter().rev().find(|m| {
        m.get("labelIds")
            .and_then(Value::as_array)
            .is_some_and(|l| l.iter().any(|l| l == "SENT" || l == "DRAFT"))
    });
    let to = theirs.map(|m| gmail::header(m, "To")).unwrap_or_default();
    let people = compose::split_list(&to);
    match people.as_slice() {
        [] => String::new(),
        [one] => sender_of(one).0,
        [first, rest @ ..] => format!("{} and {} more", sender_of(first).0, rest.len()),
    }
}

/// A thread's list row from Gmail's answer.
pub fn row_of(thread: &Value) -> Option<Thread> {
    let messages = thread
        .get("messages")
        .and_then(Value::as_array)
        .filter(|m| !m.is_empty())?;
    let (first, last) = (messages.first()?, messages.last()?);
    let (sender, address) = sender_of(&gmail::header(last, "From"));
    let mut labels: Vec<String> = messages
        .iter()
        .flat_map(|m| {
            m.get("labelIds")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        })
        .filter_map(|l| l.as_str().map(str::to_owned))
        .collect();
    labels.sort();
    labels.dedup();
    Some(Thread {
        id: thread.get("id")?.as_str()?.to_owned(),
        subject: gmail::header(first, "Subject"),
        snippet: gmail::snippet(last),
        sender,
        address,
        recipient: recipient_of(messages),
        at: millis(last),
        unread: labels.iter().any(|l| l == "UNREAD"),
        starred: labels.iter().any(|l| l == "STARRED"),
        messages: messages.len() as i64,
        attachment: messages.iter().any(|m| {
            m.pointer("/payload/mimeType").and_then(Value::as_str) == Some("multipart/mixed")
        }),
        labels,
    })
}

/// Stores threads by id; one Gmail no longer has is removed.
fn fetch_rows(
    core: &Core,
    auth: &Bearer,
    account: &str,
    ids: &[String],
) -> Result<Vec<Thread>, Failure> {
    let found = core.gmail.threads_metadata(auth, ids)?;
    let mut rows = Vec::with_capacity(ids.len());
    with_own_store(core, account, |store| {
        for (id, thread) in ids.iter().zip(found) {
            match thread.as_ref().and_then(row_of) {
                Some(row) => {
                    store.put_thread(account, &row)?;
                    rows.push(row);
                }
                None => store.remove_thread(account, id)?,
            }
        }
        Ok(())
    })?;
    Ok(rows)
}

fn sync_labels(core: &Core, auth: &Bearer, account: &str) -> Result<(), Failure> {
    let labels: Vec<Label> = core
        .gmail
        .labels(auth)?
        .iter()
        .filter_map(|l| {
            let text = |k: &str| l.get(k).and_then(Value::as_str).map(str::to_owned);
            let count = |k: &str| l.get(k).and_then(Value::as_i64).unwrap_or(0);
            Some(Label {
                id: text("id")?,
                name: text("name")?,
                kind: text("type").unwrap_or_else(|| "user".to_owned()),
                unread: count("threadsUnread"),
                total: count("threadsTotal"),
            })
        })
        .collect();
    with_own_store(core, account, |store| store.put_labels(account, &labels))
}

fn bump(core: &Core, account: &str) -> Result<i64, Failure> {
    with_own_store(core, account, |store| {
        let next = store
            .state(account, SEQ)?
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0)
            + 1;
        store.set_state(account, SEQ, &next.to_string())?;
        Ok(next)
    })
}

fn sync_once(core: &Core, auth: &Bearer, account: &str) -> Result<bool, Failure> {
    let cursor = with_store(core, |store| store.state(account, HISTORY))?;
    let changed = match cursor
        .as_deref()
        .map(|c| core.gmail.history(auth, c))
        .transpose()?
        .flatten()
    {
        Some((touched, newest)) => {
            fetch_rows(core, auth, account, &touched)?;
            with_own_store(core, account, |store| {
                store.set_state(account, HISTORY, &newest)
            })?;
            !touched.is_empty()
        }
        // no cursor, or one Google has forgotten: start from the inbox again
        None => {
            let newest = core.gmail.history_now(auth)?.unwrap_or_default();
            let (ids, next) =
                core.gmail
                    .thread_ids(auth, Some("INBOX"), None, None, PAGE as usize)?;
            fetch_rows(core, auth, account, &ids)?;
            with_own_store(core, account, |store| {
                store.set_state(account, HISTORY, &newest)?;
                store.set_state(account, &page_key("INBOX"), next.as_deref().unwrap_or(""))
            })?;
            true
        }
    };
    // counts move only with the mail, and each label costs a request
    if changed {
        sync_labels(core, auth, account)?;
    }
    Ok(changed)
}

/// One sync, and its outcome kept for the screens. A change bumps the sequence.
pub fn sync(core: &Core) -> Result<(), Failure> {
    finish_wipes(core);
    // who this sync is for, from here to its last write
    let (auth, account) = signed_in(core)?;
    let result = sync_once(core, &auth, &account);
    let status = match &result {
        Ok(_) => "ok",
        Err(Failure::Offline(_)) => "offline",
        Err(Failure::NeedsAccess) => "needs_access",
        Err(Failure::AccountRequired) => return Err(Failure::AccountRequired),
        Err(Failure::Plain(_)) => "failed",
    };
    with_own_store(core, &account, |store| {
        store.set_state(&account, STATUS, status)
    })?;
    if matches!(result, Ok(true)) {
        bump(core, &account)?;
    }
    // the same check covers events and files; how those go is Calendar's and
    // Drive's to report
    let _ = crate::calendar::sync(core, &auth, &account);
    let _ = crate::drive::sync(core, &auth, &account);
    result.map(drop)
}

fn row_json(t: &Thread) -> Value {
    json!({"id": t.id, "subject": t.subject, "snippet": t.snippet, "sender": t.sender, "at": t.at, "unread": t.unread, "starred": t.starred, "messages": t.messages, "attachment": t.attachment})
}

fn labels(core: &Core) -> Result<Value, Failure> {
    let (_, account) = signed_in(core)?;
    let list = with_store(core, |store| store.labels(&account))?;
    let rows: Vec<Value> = list.iter().map(|l| json!({"id": l.id, "name": l.name, "kind": l.kind, "unread": l.unread, "total": l.total})).collect();
    Ok(ok(json!({"labels": rows})))
}

fn list(core: &Core, args: &Value) -> Result<Value, Failure> {
    let (auth, account) = signed_in(core)?;
    let mut query = js::trim(&js::string_or_empty(args.get("query"))).to_owned();
    // a card's search, carried to the full screen by its ticket
    if let Some(Subject::Search(q)) = core
        .tickets
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .subject(&js::string_or_empty(args.get("search")))
    {
        query = q.clone();
    }
    if !query.is_empty() {
        return Ok(match search(core, &auth, &account, &query) {
            Ok(rows) => ok(
                json!({"threads": rows.iter().map(row_json).collect::<Vec<_>>(), "more": false, "saved": false}),
            ),
            Err(Failure::Offline(_)) => {
                let rows = with_store(core, |store| store.search_threads(&account, &query, PAGE))?;
                ok(
                    json!({"threads": rows.iter().map(row_json).collect::<Vec<_>>(), "more": false, "saved": true}),
                )
            }
            Err(e) => return Err(e),
        });
    }
    let label = Some(js::string_or_empty(args.get("label")))
        .filter(|l| !l.is_empty())
        .unwrap_or_else(|| "INBOX".to_owned());
    if !gmail::id_ok(&label) {
        return Ok(fail("invalid_args", "That isn't a label."));
    }
    let skip = args.get("skip").and_then(Value::as_i64).unwrap_or(0).max(0);
    let stored = with_store(core, |store| store.threads(&account, &label, skip, PAGE))?;
    if (stored.len() as i64) < PAGE {
        // the store has less than a page here: ask Google for the next one
        match fill(core, &auth, &account, &label) {
            Ok(_) | Err(Failure::Offline(_)) => {}
            Err(e) => return Err(e),
        }
    }
    let rows = with_store(core, |store| store.threads(&account, &label, skip, PAGE))?;
    let rows: Vec<Thread> = rows.into_iter().map(|t| addressed(t, &label)).collect();
    let total = with_store(core, |store| store.thread_count(&account, &label))?;
    // no token on record yet is not the end: only an empty one is
    let more =
        with_store(core, |store| store.state(&account, &page_key(&label)))?.as_deref() != Some("");
    Ok(ok(
        json!({"threads": rows.iter().map(row_json).collect::<Vec<_>>(), "more": more || skip + PAGE < total, "saved": false}),
    ))
}

/// In Sent and Drafts a row names who it's for: the sender there is the person.
fn addressed(mut t: Thread, label: &str) -> Thread {
    if matches!(label, "SENT" | "DRAFT") && !t.recipient.is_empty() {
        t.sender = format!("To: {}", t.recipient);
    }
    t
}

/// Where a label's next page token is kept; empty means no more pages.
fn page_key(label: &str) -> String {
    format!("mail_page:{label}")
}

/// Brings in the next page of `label` from Google.
fn fill(core: &Core, auth: &Bearer, account: &str, label: &str) -> Result<(), Failure> {
    let key = page_key(label);
    let held = with_store(core, |store| store.state(account, &key))?;
    if held.as_deref() == Some("") {
        return Ok(());
    }
    let (ids, next) =
        core.gmail
            .thread_ids(auth, Some(label), None, held.as_deref(), PAGE as usize)?;
    fetch_rows(core, auth, account, &ids)?;
    with_own_store(core, account, |store| {
        store.set_state(account, &key, next.as_deref().unwrap_or(""))
    })
}

fn search(core: &Core, auth: &Bearer, account: &str, query: &str) -> Result<Vec<Thread>, Failure> {
    let (ids, _) = core
        .gmail
        .thread_ids(auth, None, Some(query), None, PAGE as usize)?;
    fetch_rows(core, auth, account, &ids)
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// `millis` as a mail's header shows it, in the person's zone.
fn when(ms: i64) -> String {
    let zone = Utc.timestamp_millis_opt(ms).single().map_or(0, |at| {
        i64::from(at.with_timezone(&Local).offset().local_minus_utc())
    });
    when_in(ms, zone)
}

fn when_in(ms: i64, zone: i64) -> String {
    let ms = ms + zone * 1000;
    let (days, secs) = (
        ms.div_euclid(86_400_000),
        ms.div_euclid(1000).rem_euclid(86_400),
    );
    let z = days + 719_468;
    let (era, doe) = (z.div_euclid(146_097), z.rem_euclid(146_097));
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let (d, m) = (
        doy - (153 * mp + 2) / 5 + 1,
        if mp < 10 { mp + 3 } else { mp - 9 },
    );
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y}-{m:02}-{d:02} {:02}:{:02}", secs / 3600, secs / 60 % 60)
}

/// A thread as one page, every word of it inside the web view: names and
/// subjects never become text a screen tree shows. One link list covers the
/// page, so each message's numbers start after the last one's.
pub fn view_of(thread: &Value) -> Value {
    let list = thread
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let subject = list
        .first()
        .map(|m| gmail::header(m, "Subject"))
        .unwrap_or_default();
    let meta: Vec<String> = list
        .iter()
        .map(|m| {
            let (name, address) = sender_of(&gmail::header(m, "From"));
            format!(
                "<div class=\"gws-meta\"><b>{}</b> &lt;{}&gt; · {}<br>to {}</div>",
                escape(&gmail::cap_bytes(&name, gmail::HEADER_MAX)),
                escape(&gmail::cap_bytes(&address, gmail::HEADER_MAX)),
                when(millis(m)),
                escape(&gmail::short_header(m, "To"))
            )
        })
        .collect();
    let title = format!(
        "<h2 style=\"font-size:18px;margin:0 0 12px\">{}</h2>",
        escape(&gmail::cap_bytes(&subject, gmail::HEADER_MAX))
    );
    // an attachment is fetched from its own message, so it says which that is
    let attachments: Vec<Value> = list
        .iter()
        .flat_map(|m| {
            gmail::attachments(m).into_iter().map(|mut a| {
                a["message"] = m.get("id").cloned().unwrap_or(Value::Null);
                a
            })
        })
        .collect();
    let mut parts: Vec<Part> = list
        .iter()
        .map(|m| match gmail::html_body(m) {
            Some(h) => Part::Html(clean::clean(&h), gmail::body(m)),
            None => Part::Text(gmail::body(m)),
        })
        .collect();
    // what's counted is what's sent: the markup, as JSON
    let mut size = sent(&title)
        + meta.iter().map(|m| sent(m)).sum::<usize>()
        + json!(attachments).to_string().len()
        + parts.iter().map(Part::size).sum::<usize>();
    // past what a result can carry, the oldest give way first: HTML to plain
    // text, then plain text to a line saying so
    for pass in 0..2 {
        for part in &mut parts {
            if size <= VIEW_MAX {
                break;
            }
            if pass == 1 || matches!(part, Part::Html(..)) {
                size -= part.size();
                part.shrink();
                size += part.size();
            }
        }
    }
    let (mut page, mut links, mut images) = (title, Vec::new(), 0);
    for (meta, part) in meta.iter().zip(parts) {
        page.push_str(meta);
        match part {
            Part::Html(c, _) => {
                let (base, count) = (links.len(), c.links.len());
                page.push_str(&clean::rewrite(&c.html, "gws-link:", |n, out| {
                    let known = n < count;
                    if known {
                        out.push_str(&format!("\"gws-link:{}\"", n + base));
                    }
                    known
                }));
                images += c.images.len();
                links.extend(c.links);
            }
            Part::Text(t) => page.push_str(&pre(&t)),
            Part::TooLong => page.push_str(TOO_LONG),
        }
    }
    json!({"id": thread.get("id"), "page": page, "links": links, "images": images, "attachments": attachments, "invite": crate::invite::of(&list)})
}

/// One message of a thread, as much of it as fits.
enum Part {
    Html(clean::Cleaned, String),
    Text(String),
    TooLong,
}

impl Part {
    fn size(&self) -> usize {
        match self {
            Part::Html(c, _) => sent(&c.html) + json!(c.links).to_string().len(),
            Part::Text(t) => sent(&pre(t)),
            Part::TooLong => TOO_LONG.len(),
        }
    }

    fn shrink(&mut self) {
        *self = match std::mem::replace(self, Part::TooLong) {
            Part::Html(_, text) => Part::Text(text),
            _ => Part::TooLong,
        };
    }
}

/// `text`'s size in a JSON result.
fn sent(text: &str) -> usize {
    Value::from(text).to_string().len()
}

fn pre(text: &str) -> String {
    format!(
        "<pre style=\"white-space:pre-wrap;font:inherit;margin:0\">{}</pre>",
        escape(text)
    )
}

fn thread(core: &Core, args: &Value) -> Result<Value, Failure> {
    let (auth, account) = signed_in(core)?;
    let id = js::trim(&js::string_or_empty(args.get("thread"))).to_owned();
    // a card's row opens by ticket
    let id = match core
        .tickets
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .subject(&id)
    {
        Some(Subject::Thread(t)) => t.clone(),
        _ => id,
    };
    if !gmail::id_ok(&id) {
        return Ok(fail("invalid_args", "That isn't a thread."));
    }
    if let Some(saved) = with_store(core, |store| store.view(&account, &id, now_ms()))?
        && let Ok(view) = serde_json::from_str::<Value>(&saved)
    {
        return Ok(ok(view));
    }
    let Some(mut full) = core.gmail.thread_full(&auth, &id)? else {
        return Ok(fail("not_found", "That conversation is gone."));
    };
    // an invite that didn't come must be asked for again, not kept as none
    let whole = crate::invite::fill(core, &auth, &mut full);
    let view = view_of(&full);
    if whole {
        with_own_store(core, &account, |store| {
            store.put_view(&account, &id, &view.to_string(), now_ms())
        })?;
    }
    Ok(ok(view))
}

fn changes(core: &Core, args: &Value) -> Result<Value, Failure> {
    let (_, account) = signed_in(core)?;
    let since = args.get("since").and_then(Value::as_i64).unwrap_or(-1);
    let (seq, status) = with_store(core, |store| {
        Ok((
            store
                .state(&account, SEQ)?
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(0),
            store.state(&account, STATUS)?,
        ))
    })?;
    Ok(ok(
        json!({"seq": seq, "changed": seq != since, "status": status.unwrap_or_else(|| "syncing".to_owned())}),
    ))
}

/// Runs a mail screen's tool; `None` for any other tool. Every call says a
/// screen is open, so the sync keeps up with it.
pub fn run(core: &Core, tool: &str, args: &Value) -> Option<Value> {
    let result = match tool {
        MAIL_LABELS => labels(core),
        MAIL_LIST => list(core, args),
        MAIL_THREAD => thread(core, args),
        MAIL_CHANGES => changes(core, args),
        MAIL_CARD => card(core, args),
        MAIL_MARK | MAIL_LABEL | MAIL_ARCHIVE | MAIL_TRASH => act(core, tool, args),
        MAIL_UNDO => undo(core, args),
        MAIL_DRAFT_SAVE => compose::save(core, args),
        MAIL_DRAFT_READ => compose::read(core, args),
        MAIL_DRAFT_DELETE => compose::delete(core, args),
        MAIL_SEND => compose::send(core, args),
        MAIL_REPLY_START => compose::reply_start(core, args),
        MAIL_JOB => compose::job(core, args),
        MAIL_JOB_START => compose::job_start(core),
        MAIL_SEND_ARM => compose::arm(core, args),
        MAIL_SEND_DISARM => compose::disarm(core, args),
        PEOPLE_SUGGEST => compose::suggest(core, args),
        MAIL_DRAFT_OF => compose::draft_of(core, args),
        UPLOAD_CHUNK => compose::upload_chunk(core, args),
        UPLOAD_DROP => compose::upload_drop(core, args),
        _ => return None,
    };
    core.sync.seen();
    let answer = result.unwrap_or_else(failed);
    if tool == MAIL_DRAFT_SAVE {
        compose::finish_job(core, args, &answer);
    }
    Some(answer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_row_names_who_the_persons_last_message_went_to() {
        let message = |labels: Value, to: &str| json!({"labelIds": labels, "payload": {"headers": [{"name": "To", "value": to}]}});
        let them = message(json!(["INBOX"]), "me@example.com");
        let one = message(json!(["SENT"]), "\"Reyes, Ana\" <ana@example.com>");
        let three = message(
            json!(["DRAFT"]),
            "Ana <a@example.com>, b@example.com, c@example.com",
        );
        assert_eq!(recipient_of(std::slice::from_ref(&them)), "");
        assert_eq!(recipient_of(&[one.clone(), them]), "Reyes, Ana");
        assert_eq!(recipient_of(&[one, three]), "Ana and 2 more");
    }

    mod signing_out {
        use super::*;
        use crate::apis::Apis;
        use gws_fake_google::{FakeGoogle, Seed};

        const OWNER: &str = "owner@example.com";

        fn sign_in(subject: &str) -> Value {
            json!({"account": "google", "label": "default", "provider": "google", "subject": subject, "scopes": [], "access_token": "fake-access-1", "token_type": "Bearer", "expires_at": 4_000_000_000u64})
        }

        fn dir(name: &str) -> gws_store::Paths {
            let dir = std::env::temp_dir().join(format!("gws-out-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            gws_store::Paths::in_dir(dir)
        }

        fn core(google: &FakeGoogle, paths: &gws_store::Paths) -> Core {
            let core = Core::new(&Apis::at(&google.base()), Ok(paths.clone()));
            core.unlock("seed");
            core
        }

        fn rows(core: &Core) -> usize {
            with_store(core, |s| s.threads(OWNER, "INBOX", 0, 100))
                .unwrap_or_default()
                .len()
        }

        fn out() -> Value {
            json!({"account": "google", "label": "default"})
        }

        /// As long after the last push as it takes to count as the last.
        fn pushes_stop(core: &Core) {
            core.clock.set(core.clock.now_ms() + SETTLE_MS);
            finish_wipes(core);
        }

        const AT: i64 = 1_800_000_000_000;

        fn push(core: &Core, who: &str) {
            let mut args = sign_in(who);
            args["label"] = json!(who);
            let said = core.account("buwiz.account.set", &args);
            assert_eq!(said["structuredContent"], json!({"ok": true}), "{said}");
        }

        /// A store that was left holding `who`, each with a mark to look for.
        fn left_with(paths: &gws_store::Paths, who: &[&str]) {
            let core = Core::new(&Apis::at("http://127.0.0.1:1"), Ok(paths.clone()));
            core.unlock("seed");
            with_store(&core, |s| {
                who.iter()
                    .try_for_each(|account| s.set_state(account, "mark", "kept"))
            })
            .unwrap();
        }

        fn marked(core: &Core, who: &str) -> bool {
            with_store(core, |s| s.state(who, "mark"))
                .unwrap()
                .is_some()
        }

        /// One at a time, with a sync after each as the push wakes one: nothing
        /// goes while one is still to come, however long a push takes to arrive.
        fn pushed_one_at_a_time(who: &[&str], tag: &str) {
            let google = FakeGoogle::start(Seed::demo(1));
            let paths = dir(tag);
            left_with(&paths, who);
            let core = core(&google, &paths);
            core.clock.set(AT);
            for (at, account) in who.iter().enumerate() {
                push(&core, account);
                let _ = sync(&core);
                for each in who {
                    assert!(marked(&core, each), "{each} went at push {at}");
                }
                core.clock.set(core.clock.now_ms() + SETTLE_MS - 1);
                finish_wipes(&core);
                for each in who {
                    assert!(marked(&core, each), "{each} went after push {at}");
                }
            }
            pushes_stop(&core);
            for each in who {
                assert!(marked(&core, each), "{each} is signed in");
            }
        }

        #[test]
        fn two_accounts_pushed_one_at_a_time_both_stay() {
            pushed_one_at_a_time(&["a@example.com", "b@example.com"], "two");
        }

        #[test]
        fn three_accounts_pushed_one_at_a_time_all_stay() {
            pushed_one_at_a_time(
                &["a@example.com", "b@example.com", "c@example.com"],
                "three",
            );
        }

        #[test]
        fn an_account_nobody_pushed_goes_only_after_the_last_push() {
            let google = FakeGoogle::start(Seed::demo(1));
            let paths = dir("after-last");
            let (a, b, gone) = ("a@example.com", "b@example.com", "gone@example.com");
            left_with(&paths, &[a, b, gone]);
            let core = core(&google, &paths);
            core.clock.set(AT);
            push(&core, a);
            let _ = sync(&core);
            assert!(marked(&core, gone) && marked(&core, b));
            core.clock.set(AT + SETTLE_MS - 1);
            push(&core, b);
            core.clock.set(AT + SETTLE_MS);
            let _ = sync(&core);
            assert!(marked(&core, gone), "the wait starts over at each push");
            pushes_stop(&core);
            assert!(marked(&core, a) && marked(&core, b));
            assert!(!marked(&core, gone), "nobody pushed it");
        }

        #[test]
        fn a_sign_out_takes_its_own_mail_at_once_and_nobody_elses() {
            let google = FakeGoogle::start(Seed::demo(1));
            let paths = dir("own-only");
            let (a, b, later) = ("a@example.com", "b@example.com", "later@example.com");
            left_with(&paths, &[a, b, later]);
            let core = core(&google, &paths);
            core.clock.set(AT);
            push(&core, a);
            push(&core, b);
            core.account(
                "buwiz.account.clear",
                &json!({"account": "google", "label": a}),
            );
            assert!(!marked(&core, a), "it doesn't outlive the sign-in");
            assert!(marked(&core, b) && marked(&core, later));
        }

        #[test]
        fn a_second_sign_in_of_one_account_keeps_it_through_a_sign_out() {
            let google = FakeGoogle::start(Seed::demo(1));
            let paths = dir("two-labels");
            left_with(&paths, &[OWNER]);
            let core = core(&google, &paths);
            let mut other = sign_in(OWNER);
            other["label"] = json!("work");
            core.account("buwiz.account.set", &sign_in(OWNER));
            core.account("buwiz.account.set", &other);
            core.account("buwiz.account.clear", &out());
            assert!(marked(&core, OWNER));
        }

        /// Nothing else may have to come along for the last clear to happen.
        #[test]
        fn the_sync_thread_clears_by_itself_once_the_pushes_stop() {
            let google = FakeGoogle::start(Seed::demo(1));
            let paths = dir("thread");
            let (a, gone) = ("a@example.com", "gone@example.com");
            left_with(&paths, &[a, gone]);
            let core = Arc::new(core(&google, &paths));
            core.settle.wait_for(300);
            let thread = start(core.clone());
            push(&core, a);
            assert!(marked(&core, gone), "not yet");
            let by = Instant::now() + Duration::from_secs(10);
            while marked(&core, gone) && Instant::now() < by {
                std::thread::sleep(Duration::from_millis(20));
            }
            let cleared = !marked(&core, gone);
            core.sync.stop();
            thread.join().unwrap();
            assert!(cleared, "nothing woke the clear");
            assert!(marked(&core, a));
        }

        #[test]
        fn nothing_lands_after_a_sign_out_however_late_it_arrives() {
            let google = FakeGoogle::start(Seed::demo(6));
            let core = core(&google, &dir("late"));
            core.account("buwiz.account.set", &sign_in(OWNER));
            sync(&core).unwrap();
            core.account("buwiz.account.clear", &out());
            let late = with_own_store(&core, OWNER, |s| s.set_state(OWNER, "late", "1"));
            assert!(late.is_err(), "the account is gone");
            let state = |account: &str, key: &str| {
                with_store(&core, |s| s.state(account, key)).ok().flatten()
            };
            assert_eq!(state(OWNER, "late"), None);
            assert!(sync(&core).is_err(), "no sync starts for nobody");
            assert_eq!(state("", STATUS), None);
        }

        #[test]
        fn a_wipe_that_failed_says_so_and_survives_a_restart() {
            let google = FakeGoogle::start(Seed::demo(6));
            let paths = dir("restart");
            let first = core(&google, &paths);
            first.account("buwiz.account.set", &sign_in(OWNER));
            sync(&first).unwrap();
            assert!(rows(&first) > 0);
            // the store can't be reached for this one sign-out
            first.store_key.lock().unwrap().take();
            first.store.lock().unwrap().take();
            let said = first.account("buwiz.account.clear", &out());
            assert_eq!(said["structuredContent"]["mail_kept"], true, "{said}");
            assert!(paths.wipe_pending().exists());
            drop(first);

            let second = core(&google, &paths);
            assert!(rows(&second) > 0, "still there after the restart");
            let _ = sync(&second);
            assert!(rows(&second) > 0, "the Bot hasn't pushed anything yet");
            second.account("buwiz.account.set", &sign_in("other@example.com"));
            let _ = sync(&second);
            assert!(rows(&second) > 0, "the Bot may have more pushes to come");
            pushes_stop(&second);
            assert_eq!(rows(&second), 0, "the last push cleared it");
            assert!(!paths.wipe_pending().exists());
        }

        #[test]
        fn a_pending_wipe_waits_for_the_first_push_and_then_keeps_who_is_signed_in() {
            let google = FakeGoogle::start(Seed::demo(6));
            let paths = dir("marker");
            let first = core(&google, &paths);
            first.account("buwiz.account.set", &sign_in(OWNER));
            sync(&first).unwrap();
            drop(first);
            std::fs::write(paths.wipe_pending(), b"").unwrap();

            let second = core(&google, &paths);
            let _ = sync(&second);
            assert!(rows(&second) > 0, "no push yet, so nobody's mail goes");
            second.account("buwiz.account.set", &sign_in(OWNER));
            pushes_stop(&second);
            assert!(rows(&second) > 0, "the owner is signed in");
            assert!(!paths.wipe_pending().exists());
        }

        #[test]
        fn a_refused_push_is_no_push() {
            let google = FakeGoogle::start(Seed::demo(6));
            let paths = dir("refused");
            let first = core(&google, &paths);
            first.account("buwiz.account.set", &sign_in(OWNER));
            sync(&first).unwrap();
            drop(first);
            std::fs::write(paths.wipe_pending(), b"").unwrap();

            let second = core(&google, &paths);
            let refused = second.account("buwiz.account.set", &json!({"account": "google"}));
            assert_eq!(refused["isError"], true, "{refused}");
            pushes_stop(&second);
            assert!(paths.wipe_pending().exists(), "the wipe still waits");
        }

        #[test]
        fn a_clear_for_a_sign_in_this_process_never_saw_still_wipes() {
            let google = FakeGoogle::start(Seed::demo(6));
            let paths = dir("unseen");
            let first = core(&google, &paths);
            first.account("buwiz.account.set", &sign_in(OWNER));
            sync(&first).unwrap();
            drop(first);

            let second = core(&google, &paths);
            assert!(rows(&second) > 0);
            second.account("buwiz.account.clear", &out());
            pushes_stop(&second);
            assert_eq!(rows(&second), 0);
        }

        /// A push as the Bot sends one, with its list: `(label, active)`.
        fn with_list(mut args: Value, list: &[(&str, bool)]) -> Value {
            let rows: Vec<Value> = list
                .iter()
                .map(|(label, active)| json!({"account": "google", "label": label, "active": active}))
                .collect();
            args["accounts"] = json!(rows);
            args
        }

        fn push_listed(core: &Core, who: &str, list: &[(&str, bool)]) {
            let mut args = sign_in(who);
            args["label"] = json!(who);
            let said = core.account("buwiz.account.set", &with_list(args, list));
            assert_eq!(said["structuredContent"], json!({"ok": true}), "{said}");
        }

        fn clear_listed(core: &Core, who: &str, list: &[(&str, bool)]) {
            let args = json!({"account": "google", "label": who});
            core.account("buwiz.account.clear", &with_list(args, list));
        }

        /// The Bot's switch from `from` to `to`: the old one cleared, the new
        /// one pushed, both with the list as it is by then.
        fn switch(core: &Core, from: &str, to: &str) {
            let list = [(from, false), (to, true)];
            clear_listed(core, from, &list);
            push_listed(core, to, &list);
        }

        const A: &str = "a@example.com";
        const B: &str = "b@example.com";

        /// Both signed in and a on top, on a store holding both.
        fn two_accounts(google: &FakeGoogle, tag: &str) -> (Core, gws_store::Paths) {
            let paths = dir(tag);
            left_with(&paths, &[A, B]);
            let core = core(google, &paths);
            core.clock.set(AT);
            push_listed(&core, B, &[(A, true), (B, false)]);
            switch(&core, B, A);
            (core, paths)
        }

        fn answer(result: &Value) -> &Value {
            &result["structuredContent"]
        }

        #[test]
        fn a_switch_keeps_both_accounts_and_a_sign_out_still_takes_its_own() {
            let google = FakeGoogle::start(Seed::demo(1));
            let (core, _) = two_accounts(&google, "two-switch");
            switch(&core, A, B);
            let _ = sync(&core);
            pushes_stop(&core);
            assert!(marked(&core, A), "a switch is no sign-out");
            assert!(marked(&core, B));
            assert_eq!(core.accounts.session().unwrap().1, B);
            clear_listed(&core, A, &[(B, true)]);
            assert!(!marked(&core, A), "off the list, so it's gone at once");
            assert!(marked(&core, B));
        }

        #[test]
        fn an_account_nobody_pushed_since_a_restart_is_still_kept_and_named() {
            let google = FakeGoogle::start(Seed::demo(1));
            let (first, paths) = two_accounts(&google, "two-restart");
            drop(first);
            let second = core(&google, &paths);
            second.clock.set(AT);
            push_listed(&second, B, &[(A, false), (B, true)]);
            let _ = sync(&second);
            pushes_stop(&second);
            assert!(marked(&second, A), "only the active one is pushed");
            assert!(marked(&second, B));
            let rows = second.run(crate::tools::ACCOUNTS, &json!({}));
            assert_eq!(
                answer(&rows)["accounts"],
                json!([
                    {"label": A, "address": A, "active": false},
                    {"label": B, "address": B, "active": true},
                ])
            );
        }

        #[test]
        fn a_label_off_the_list_after_a_restart_goes_with_its_record() {
            let google = FakeGoogle::start(Seed::demo(1));
            let (first, paths) = two_accounts(&google, "two-off-list");
            drop(first);
            let second = core(&google, &paths);
            second.clock.set(AT);
            push_listed(&second, B, &[(B, true)]);
            pushes_stop(&second);
            assert!(!marked(&second, A));
            assert!(marked(&second, B));
            let filed = with_store(&second, |s| s.sign_ins()).unwrap();
            assert_eq!(filed, vec![(B.to_owned(), B.to_owned())]);
        }

        #[test]
        fn a_sign_out_of_an_account_nobody_pushed_since_a_restart_goes_at_once() {
            let google = FakeGoogle::start(Seed::demo(1));
            let (first, paths) = two_accounts(&google, "two-out-restart");
            drop(first);
            let second = core(&google, &paths);
            second.clock.set(AT);
            push_listed(&second, B, &[(A, false), (B, true)]);
            clear_listed(&second, A, &[(B, true)]);
            assert!(!marked(&second, A), "not at the next settle: now");
            assert!(marked(&second, B));
        }

        /// Not what B7 sends. A Bot that stopped listing can't vouch for a label.
        #[test]
        fn a_clear_with_no_list_is_a_sign_out_whatever_the_last_list_said() {
            let google = FakeGoogle::start(Seed::demo(1));
            let (core, _) = two_accounts(&google, "two-no-list");
            let out = json!({"account": "google", "label": B});
            core.account("buwiz.account.clear", &out);
            assert!(!marked(&core, B), "gone at once");
            assert!(marked(&core, A));
            let rows = core.run(crate::tools::ACCOUNTS, &json!({}));
            assert_eq!(
                answer(&rows)["accounts"],
                json!([{"label": A, "address": A, "active": true}])
            );
        }

        #[test]
        fn a_clear_whose_list_is_null_has_no_list() {
            let google = FakeGoogle::start(Seed::demo(1));
            let (core, _) = two_accounts(&google, "two-null-list");
            let out = json!({"account": "google", "label": B, "accounts": null});
            core.account("buwiz.account.clear", &out);
            assert!(!marked(&core, B), "gone at once");
        }

        #[test]
        fn only_a_listed_other_account_asks_the_bot_to_switch() {
            let google = FakeGoogle::start(Seed::demo(1));
            let (core, _) = two_accounts(&google, "two-use");
            let ask = |label: &str| core.run(crate::tools::ACCOUNT_USE, &json!({"label": label}));
            let asked = ask(B);
            assert_eq!(
                asked["_meta"],
                json!({"buwiz/account_use": {"account": "google", "label": B}})
            );
            assert_eq!(core.accounts.session().unwrap().1, A, "the Bot switches");
            let same = ask(A);
            assert_eq!(same.get("_meta"), None);
            assert_eq!(answer(&same)["switched"], false);
            let nobody = ask("nobody");
            assert_eq!(nobody.get("_meta"), None);
            assert_eq!(answer(&nobody)["code"], "not_found");
        }

        #[test]
        fn a_send_in_its_hold_keeps_the_account_until_it_goes() {
            let google = FakeGoogle::start(Seed::demo(1));
            let (core, _) = two_accounts(&google, "two-hold");
            let press = core.run(crate::tools::MAIL_SEND_ARM, &json!({}));
            let ask = || core.run(crate::tools::ACCOUNT_USE, &json!({"label": B}));
            let held = ask();
            assert_eq!(held.get("_meta"), None);
            assert_eq!(answer(&held)["code"], "busy");
            let armed = answer(&press)["armed"].clone();
            core.run(crate::tools::MAIL_SEND_DISARM, &json!({"armed": armed}));
            assert!(ask().get("_meta").is_some(), "Undo frees it");
        }

        #[test]
        fn a_write_for_the_account_before_a_switch_is_refused_before_google() {
            let google = FakeGoogle::start(Seed::demo(1));
            let (core, _) = two_accounts(&google, "two-stale");
            let save = |who: &str| {
                let args = json!({"as_account": who, "to": "ana@example.com", "subject": "Hi", "body": "x"});
                core.run(crate::tools::MAIL_DRAFT_SAVE, &args)
            };
            assert!(answer(&save(A)).get("draft").is_some(), "its own account");
            switch(&core, A, B);
            let refused = save(A);
            assert_eq!(answer(&refused)["code"], "account_changed");
            assert!(answer(&save(B)).get("draft").is_some());
        }

        #[test]
        fn an_event_draft_stays_on_the_account_it_was_made_on() {
            let google = FakeGoogle::start(Seed::demo(1));
            let (core, _) = two_accounts(&google, "two-event");
            let args = json!({"title": "Lunch", "start": "2026-11-09T12:00:00+08:00", "end": "2026-11-09T13:00:00+08:00"});
            let made = core.run(crate::tools::EVENT_DRAFT, &args);
            let read = |ticket: &Value| {
                core.run(crate::tools::EVENT_DRAFT_READ, &json!({"ticket": ticket}))
            };
            let ticket = answer(&made)["ticket"].clone();
            assert_eq!(answer(&read(&ticket))["as_account"], A);
            switch(&core, A, B);
            assert_eq!(answer(&read(&ticket))["refused"], "not_found");
            switch(&core, B, A);
            assert_eq!(answer(&read(&ticket))["title"], "Lunch");
        }

        #[test]
        fn a_start_with_no_push_yet_keeps_the_mail() {
            let google = FakeGoogle::start(Seed::demo(6));
            let paths = dir("start");
            let first = core(&google, &paths);
            first.account("buwiz.account.set", &sign_in(OWNER));
            sync(&first).unwrap();
            drop(first);
            let second = core(&google, &paths);
            let _ = sync(&second);
            assert!(rows(&second) > 0, "the Bot hasn't pushed its token yet");
        }
    }

    #[test]
    fn a_sender_splits_into_name_and_address() {
        assert_eq!(
            sender_of("Ana Reyes <ana@example.com>"),
            ("Ana Reyes".to_owned(), "ana@example.com".to_owned())
        );
        assert_eq!(
            sender_of("\"Reyes, Ana\" <ana@example.com>").0,
            "Reyes, Ana"
        );
        assert_eq!(
            sender_of("ana@example.com"),
            ("ana@example.com".to_owned(), "ana@example.com".to_owned())
        );
        assert_eq!(sender_of("<ana@example.com>").0, "ana@example.com");
    }

    fn message(id: &str, html: &str) -> Value {
        use base64::Engine as _;
        let b64 = |s: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(s);
        json!({"id": id, "internalDate": "1793955600000", "payload": {"mimeType": "multipart/alternative", "headers": [{"name": "Subject", "value": "A <b>subject</b>"}, {"name": "From", "value": "Ana <ana@example.com>"}], "parts": [
            {"mimeType": "text/plain", "body": {"data": b64(&format!("plain {id}"))}},
            {"mimeType": "text/html", "body": {"data": b64(html)}},
        ]}})
    }

    #[test]
    fn an_oversized_thread_gives_up_the_oldest_first() {
        let big = format!("<p>{}</p>", "x".repeat(500 * 1024));
        let view =
            view_of(&json!({"id": "t", "messages": [message("old", &big), message("new", &big)]}));
        let page = view["page"].as_str().unwrap();
        let cut = page.find("too long").expect("the oldest gives way");
        assert!(page[cut..].contains("xxxx"), "the newest stays");
        assert!(view.to_string().len() < 1024 * 1024, "fits a result");
    }

    #[test]
    fn what_is_sent_stays_under_a_result_however_it_escapes() {
        use base64::Engine as _;
        // a newline costs two bytes as JSON, a control character six
        let text = "\n\u{1}".repeat(300 * 1024);
        let data = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&text);
        let plain = json!({"id": "p", "internalDate": "1793955600000", "payload": {"mimeType": "text/plain", "headers": [], "body": {"data": data}}});
        let view = view_of(&json!({"id": "t", "messages": [plain]}));
        assert!(
            view.to_string().len() < 1024 * 1024,
            "{}",
            view.to_string().len()
        );
    }

    #[test]
    fn links_number_on_through_a_thread() {
        let view = view_of(&json!({"id": "t", "messages": [
            message("a", r#"<a href="https://one.example/">1</a><a href="https://two.example/">2</a>"#),
            message("b", r#"<a href="https://three.example/">3</a>"#),
        ]}));
        let page = view["page"].as_str().unwrap();
        assert_eq!(
            view["links"],
            json!([
                "https://one.example/",
                "https://two.example/",
                "https://three.example/"
            ])
        );
        for n in 0..3 {
            assert!(page.contains(&format!("\"gws-link:{n}\"")), "{page}");
        }
    }

    #[test]
    fn tickets_stay_bounded_and_the_recent_ones_work() {
        let mut tickets = Tickets::default();
        let first = tickets.issue(&Subject::Search("first".to_owned()));
        let mut last = String::new();
        for n in 0..TICKETS_MAX * 3 {
            last = tickets.issue(&Subject::Search(n.to_string()));
        }
        assert_eq!(tickets.subject(&first), None, "the oldest went");
        assert!(tickets.subject(&last).is_some());
    }

    #[test]
    fn headers_are_escaped_and_links_number_across_the_thread() {
        let one = message("1", r#"<a href="https://one.example/">one</a>"#);
        let two = message(
            "2",
            r#"<a href="https://two.example/">two</a><a href="https://three.example/">three</a>"#,
        );
        let view = view_of(&json!({"id": "t", "messages": [one, two]}));
        let page = view["page"].as_str().unwrap();
        assert!(
            page.contains("A &lt;b&gt;subject&lt;/b&gt;") && !page.contains("<b>subject"),
            "{page}"
        );
        assert_eq!(
            view["links"],
            json!([
                "https://one.example/",
                "https://two.example/",
                "https://three.example/"
            ])
        );
        for (n, word) in [(0, "one"), (1, "two"), (2, "three")] {
            assert!(
                page.contains(&format!(r#"<a href="gws-link:{n}">{word}</a>"#)),
                "{page}"
            );
        }
    }

    #[test]
    fn dates_read_plainly() {
        assert_eq!(when_in(1_793_955_600_000, 0), "2026-11-06 09:00");
        assert_eq!(when_in(1_793_955_600_000, 8 * 3600), "2026-11-06 17:00");
    }
}
