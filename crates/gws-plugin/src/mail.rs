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
use crate::gmail::{self, Failure, Limit, Whole};
use crate::pictures::{self, Made};
use crate::tools::{
    MAIL_ARCHIVE, MAIL_CARD, MAIL_CHANGES, MAIL_DRAFT_DELETE, MAIL_DRAFT_OF, MAIL_DRAFT_READ,
    MAIL_DRAFT_SAVE, MAIL_JOB, MAIL_JOB_START, MAIL_LABEL, MAIL_LABELS, MAIL_LIST, MAIL_MARK,
    MAIL_REPLY_START, MAIL_SEND, MAIL_SEND_ARM, MAIL_SEND_DISARM, MAIL_THREAD, MAIL_TRASH,
    MAIL_UNDO, PEOPLE_SUGGEST, SEARCH_MAIL, UPLOAD_CHUNK, UPLOAD_DROP, fail, failed, ok,
};
use crate::{Core, clean, compose, fold, js};

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

/// How many cards' tickets each of the two generations held here takes.
const CARDS_HELD: usize = gws_store::tickets::CARDS_MAX as usize;
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
            let ticket = issue_card(core, &Subject::Draft(draft.to_owned()));
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
    let ticket = issue_card(core, &Subject::Search(query));
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
    core.sync.wake_by_person();
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
    core.sync.wake_by_person();
    Ok(ok(json!({"done": ids.len()})))
}

/// What every card says when its ticket stands for nothing here and nothing
/// came with it to bind it again.
pub const TICKET_LOST: &str = "This card lost its link when the plugin restarted.";

/// What a card says when its ticket is another account's.
pub const TICKET_ELSEWHERE: &str = "This card is from another account.";

/// What a card says when its ticket aged out of what's kept.
pub const TICKET_GONE: &str = "This card is out of date.";

/// Why a card's `ticket` stands for nothing here, in words.
pub(crate) fn lost_words(core: &Core, ticket: &str) -> &'static str {
    match place(core, ticket) {
        Place::Elsewhere => TICKET_ELSEWHERE,
        Place::Gone => TICKET_GONE,
        Place::Here(_) | Place::Nowhere => TICKET_LOST,
    }
}

/// [`ticket_lost`], saying why for `ticket`.
pub(crate) fn lost(core: &Core, ticket: &str) -> Value {
    fail("ticket_lost", lost_words(core, ticket))
}

impl Subject {
    /// The name its kind is kept under, and what it stands for.
    fn kept(&self) -> (&'static str, &str) {
        match self {
            Subject::Search(q) => ("search", q),
            Subject::Thread(t) => ("thread", t),
            Subject::Draft(d) => ("draft", d),
            Subject::EventDraft(f) => ("event_draft", f),
            Subject::DriveSearch(w) => ("drive_search", w),
        }
    }

    fn of_kept(kind: &str, subject: String) -> Option<Self> {
        Some(match kind {
            "search" => Subject::Search(subject),
            "thread" => Subject::Thread(subject),
            "draft" => Subject::Draft(subject),
            "event_draft" => Subject::EventDraft(subject),
            "drive_search" => Subject::DriveSearch(subject),
            _ => return None,
        })
    }

    fn to_kept(&self, ticket: &str) -> gws_store::tickets::Ticket {
        let (kind, subject) = self.kept();
        gws_store::tickets::Ticket {
            ticket: ticket.to_owned(),
            kind: kind.to_owned(),
            subject: subject.to_owned(),
        }
    }
}

/// Whose a ticket is: the account signed in when it was issued, if any.
type Whose = Option<String>;

/// Tickets and their subjects, both ways, each with whose it is.
#[derive(Default)]
struct Held {
    by_subject: HashMap<(Whose, Subject), String>,
    by_ticket: HashMap<String, (Subject, Whose)>,
}

impl Held {
    fn put(&mut self, ticket: &str, subject: &Subject, whose: &Whose) {
        self.by_subject
            .insert((whose.clone(), subject.clone()), ticket.to_owned());
        self.by_ticket
            .insert(ticket.to_owned(), (subject.clone(), whose.clone()));
    }
}

/// The tickets this process knows. The store keeps them past it
/// ([`issue_card`], [`issue_rows`], [`subject_of`]); this is what's been read
/// from there or issued here, and all there is while the store can't open.
///
/// Cards and rows each come in two generations, so neither grows without
/// end: when the newer fills, the older goes. A row's card issues it again
/// when it next reads; a card's is read from the store again.
#[derive(Default)]
pub struct Tickets {
    cards: Held,
    older_cards: Held,
    cards_issued: usize,
    newer: Held,
    older: Held,
    issued: usize,
}

impl Tickets {
    fn all(&self) -> [&Held; 4] {
        [&self.cards, &self.older_cards, &self.newer, &self.older]
    }

    fn ticket(&self, whose: &Whose, subject: &Subject) -> Option<&String> {
        let key = (whose.clone(), subject.clone());
        self.all()
            .into_iter()
            .find_map(|held| held.by_subject.get(&key))
    }

    fn entry(&self, ticket: &str) -> Option<&(Subject, Whose)> {
        self.all()
            .into_iter()
            .find_map(|held| held.by_ticket.get(ticket))
    }

    #[cfg(test)]
    fn subject(&self, ticket: &str) -> Option<&Subject> {
        self.entry(ticket).map(|(subject, _)| subject)
    }

    fn put(&mut self, ticket: &str, subject: &Subject, whose: &Whose, card: bool) {
        let a_card = |held: &Held| held.by_ticket.contains_key(ticket);
        if card || a_card(&self.cards) || a_card(&self.older_cards) {
            if self.cards_issued >= CARDS_HELD {
                self.older_cards = std::mem::take(&mut self.cards);
                self.cards_issued = 0;
            }
            self.cards_issued += 1;
            return self.cards.put(ticket, subject, whose);
        }
        if self.issued >= TICKETS_MAX {
            self.older = std::mem::take(&mut self.newer);
            self.issued = 0;
        }
        self.issued += 1;
        self.newer.put(ticket, subject, whose);
    }

    /// Forgets every ticket held here. The store's are its own to forget.
    fn clear(&mut self) {
        *self = Self::default();
    }
}

/// Whose tickets are kept: the account signed in. With nobody signed in
/// nothing is kept, as nothing later would take it away again.
fn keeper(core: &Core) -> Option<String> {
    core.accounts.signed_in().map(|(who, _)| who)
}

/// A ticket for each subject: the one it has, here or in the store, else a new
/// one. All are kept in the store as the newest, a `card`'s for good. With no
/// store they live as long as this process, as they always did.
fn issue(core: &Core, subjects: &[Subject], card: bool) -> Vec<String> {
    let account = keeper(core);
    let known: Vec<Option<String>> = {
        let held = core.tickets.lock().unwrap_or_else(|e| e.into_inner());
        subjects
            .iter()
            .map(|s| held.ticket(&account, s).cloned())
            .collect()
    };
    let kept = account.as_ref().and_then(|account| {
        with_own_store(core, account, |store| {
            let mut tickets = Vec::with_capacity(subjects.len());
            for (subject, known) in subjects.iter().zip(&known) {
                let (kind, words) = subject.kept();
                tickets.push(match known {
                    Some(ticket) => ticket.clone(),
                    None => store
                        .ticket_for(account, kind, words)?
                        .unwrap_or_else(buwiz_plugin::new_ticket),
                });
            }
            let rows: Vec<_> = subjects
                .iter()
                .zip(&tickets)
                .map(|(subject, ticket)| subject.to_kept(ticket))
                .collect();
            store.keep_tickets(account, &rows, card)?;
            Ok(tickets)
        })
        .inspect_err(|e| eprintln!("buwiz-gws: tickets weren't kept: {e:?}"))
        .ok()
    });
    let tickets = kept.unwrap_or_else(|| {
        known
            .into_iter()
            .map(|known| known.unwrap_or_else(buwiz_plugin::new_ticket))
            .collect()
    });
    let mut held = core.tickets.lock().unwrap_or_else(|e| e.into_inner());
    for (subject, ticket) in subjects.iter().zip(&tickets) {
        held.put(ticket, subject, &account, card);
    }
    tickets
}

/// The ticket of a card in a chat. The chat keeps the card, so the ticket is
/// kept for as long as the account is: a card must never find its own gone.
pub fn issue_card(core: &Core, subject: &Subject) -> String {
    issue(core, std::slice::from_ref(subject), true).remove(0)
}

/// The tickets of what a card or a list shows, which it issues again whenever
/// it reads: kept too, so rows shown before a restart still open after it, but
/// only among the newest.
pub fn issue_rows(core: &Core, subjects: &[Subject]) -> Vec<String> {
    issue(core, subjects, false)
}

/// [`issue_rows`] for one.
pub fn issue_row(core: &Core, subject: &Subject) -> String {
    issue_rows(core, std::slice::from_ref(subject)).remove(0)
}

/// What a ticket is to whoever is signed in now.
pub(crate) enum Place {
    /// Theirs, standing for this.
    Here(Subject),
    /// Another account's.
    Elsewhere,
    /// Theirs, but it aged out of what's kept: it stands for nothing.
    Gone,
    /// Nobody's that this process or the store knows.
    Nowhere,
}

/// Where `ticket` stands: as this process knows it, else as the store kept it.
/// A ticket stands for its subject only under the account it was issued to.
pub(crate) fn place(core: &Core, ticket: &str) -> Place {
    let whose = keeper(core);
    let held = core
        .tickets
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(ticket)
        .cloned();
    if let Some((subject, of)) = held {
        return if of == whose {
            Place::Here(subject)
        } else {
            Place::Elsewhere
        };
    }
    if !buwiz_plugin::is_ticket(ticket) {
        return Place::Nowhere;
    }
    let kept = match with_store(core, |store| store.ticket(ticket)) {
        Ok(Some(kept)) => kept,
        Ok(None) => return Place::Nowhere,
        Err(e) => {
            eprintln!("buwiz-gws: a card's link couldn't be read: {e:?}");
            return Place::Nowhere;
        }
    };
    if whose.as_deref() != Some(kept.account.as_str()) {
        return Place::Elsewhere;
    }
    if kept.ticket.kind == gws_store::tickets::GONE {
        return Place::Gone;
    }
    let Some(subject) = Subject::of_kept(&kept.ticket.kind, kept.ticket.subject) else {
        return Place::Gone;
    };
    core.tickets
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .put(ticket, &subject, &whose, kept.card);
    Place::Here(subject)
}

/// What a ticket stands for under the account signed in now.
pub fn subject_of(core: &Core, ticket: &str) -> Option<Subject> {
    match place(core, ticket) {
        Place::Here(subject) => Some(subject),
        _ => None,
    }
}

/// Gives a card's ticket that stands for nothing its subject back: the chat
/// still has the card and what made it, though the ticket was issued by a
/// process that kept none. False for anything that isn't a ticket, one that
/// stands for something already, another account's, or one aged out. Two
/// binds at once: the first kept wins, and the other is false unless it said
/// the same.
pub(crate) fn bind(core: &Core, ticket: &str, subject: &Subject) -> bool {
    if !buwiz_plugin::is_ticket(ticket) || !matches!(place(core, ticket), Place::Nowhere) {
        return false;
    }
    let whose = keeper(core);
    if let Some(account) = &whose {
        let kept = subject.to_kept(ticket);
        // unkept, it still works until this process ends
        if let Err(e) = with_own_store(core, account, |store| {
            store.keep_tickets(account, std::slice::from_ref(&kept), true)
        }) {
            eprintln!("buwiz-gws: a card's link wasn't kept: {e:?}");
        }
    }
    {
        let mut held = core.tickets.lock().unwrap_or_else(|e| e.into_inner());
        if held.entry(ticket).is_none() {
            held.put(ticket, subject, &whose, true);
        }
    }
    matches!(place(core, ticket), Place::Here(now) if now == *subject)
}

/// Forgets the tickets this process holds, after an account's went from the
/// store: whatever is still kept there is read from it again.
pub(crate) fn forget_tickets(core: &Core) {
    core.tickets
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
}

/// The search a card's ticket stands for. One that stands for nothing is
/// bound to `query`, the search the chat made the card with, when that came
/// too: a card from before tickets were kept has only that to go on.
fn card_search(core: &Core, args: &Value) -> Option<String> {
    let ticket = js::string_or_empty(args.get("ticket"));
    match subject_of(core, &ticket) {
        Some(Subject::Search(query)) => Some(query),
        Some(_) => None,
        None => {
            let query = js::trim(&js::string_or_empty(args.get("query"))).to_owned();
            (!query.is_empty() && bind(core, &ticket, &Subject::Search(query.clone())))
                .then_some(query)
        }
    }
}

/// A card's rows, each with a ticket to open it by.
fn card(core: &Core, args: &Value) -> Result<Value, Failure> {
    let (auth, account) = reader(core)?;
    let Some(query) = card_search(core, args) else {
        return Ok(lost(core, &js::string_or_empty(args.get("ticket"))));
    };
    let found = match &auth {
        Some(auth) => search(core, auth, &account, &query),
        None => Err(unreached()),
    };
    let (rows, saved) = match found {
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
    let shown: Vec<Subject> = rows
        .iter()
        .take(CARD_ROWS)
        .map(|t| Subject::Thread(t.id.clone()))
        .collect();
    let rows: Vec<Value> = rows
        .iter()
        .zip(issue_rows(core, &shown))
        .map(|(t, ticket)| json!({"sender": t.sender, "subject": t.subject, "at": t.at, "unread": t.unread, "ticket": ticket}))
        .collect();
    Ok(ok(json!({"rows": rows, "more": more, "saved": saved})))
}

/// What a sync woke for. Mail is kept ahead only after the timer's or a
/// person's: a wake from anything else, a write the model or a routine made
/// among them, must not start a round of downloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Woken {
    Timer,
    Person,
    Other,
}

/// The wake waiting, when a screen last asked, the next time set to look, and
/// when the timer last had its turn.
type Wakes = (
    Option<Woken>,
    Option<Instant>,
    Option<Instant>,
    Option<Instant>,
);

/// Wakes the sync, remembers when a screen last asked for something, and when
/// it next has to look whatever else happens.
#[derive(Default)]
pub struct Waker {
    state: Mutex<Wakes>,
    bell: Condvar,
    stop: AtomicBool,
}

impl Waker {
    /// Asks the sync thread to end after what it's doing.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        self.wake();
    }

    pub(crate) fn stopping(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    pub fn wake(&self) {
        self.woken(Woken::Other);
    }

    /// [`Self::wake`] for what the person did on a screen.
    pub fn wake_by_person(&self) {
        self.woken(Woken::Person);
    }

    fn woken(&self, by: Woken) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        // a person's wake still waiting isn't lost to another
        if state.0 != Some(Woken::Person) {
            state.0 = Some(by);
        }
        drop(state);
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

    /// Until a wake or the time to look again, and which it was.
    pub(crate) fn wait(&self) -> Woken {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let start = state.3.unwrap_or_else(Instant::now);
        loop {
            let every = if state.1.is_some_and(|at| at.elapsed() < LINGER) {
                ACTIVE
            } else {
                IDLE
            };
            let due = state.2.map_or(start + every, |at| at.min(start + every));
            let now = Instant::now();
            // a condvar can wake for nothing: only a time that's come is the
            // timer's, and a time that's come is, whatever else woke it
            let woken = match state.0.take() {
                Some(Woken::Person) => Some(Woken::Person),
                _ if now >= due => Some(Woken::Timer),
                other => other,
            };
            if let Some(by) = woken {
                if state.2.is_some_and(|at| at <= now) {
                    state.2 = None;
                }
                if by != Woken::Other {
                    state.3 = Some(now);
                }
                return by;
            }
            state = self
                .bell
                .wait_timeout(state, due - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
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
            let woken = core.sync.wait();
            if core.sync.stopping() {
                return;
            }
            if core.provisioned() {
                if sync(&core).is_ok() && woken != Woken::Other {
                    crate::prefetch::kick(&core);
                }
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
    // the tickets held here go under the same lock as the store's, so no read
    // in between finds one the store no longer has
    let mut held = core.tickets.lock().unwrap_or_else(|e| e.into_inner());
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
    if cleared {
        held.clear();
    }
    drop(held);
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
    let mut held = core.tickets.lock().unwrap_or_else(|e| e.into_inner());
    let cleared = with_store(core, |store| {
        let listed = file_sign_ins(core, store)?;
        if !core.accounts.holds(account) && !listed.contains(account) {
            crate::files::forget_account(core, store, account)?;
            store.forget_account(account)?;
        }
        Ok(())
    })
    .is_ok();
    if cleared {
        held.clear();
    }
    drop(held);
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

/// Whose saved mail a screen reads, with their token while it's live. A lapsed
/// one is still their sign-in: with no connection the Bot can't refresh it, and
/// what's saved is still theirs to read. No token means Google can't be asked.
/// Only the active sign-in's mail: with no active one, nobody's.
pub(crate) fn reader(core: &Core) -> Result<(Option<Bearer>, String), Failure> {
    if let Some((auth, account)) = core.accounts.session() {
        return Ok((Some(auth), account));
    }
    let (account, _) = core.accounts.signed_in().ok_or(Failure::AccountRequired)?;
    Ok((None, account))
}

/// What a call to Google comes to with no live token.
pub(crate) fn unreached() -> Failure {
    Failure::Offline(crate::tools::GOOGLE_UNREACHED.to_owned())
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
    let (_, account) = reader(core)?;
    let list = with_store(core, |store| store.labels(&account))?;
    let rows: Vec<Value> = list.iter().map(|l| json!({"id": l.id, "name": l.name, "kind": l.kind, "unread": l.unread, "total": l.total})).collect();
    Ok(ok(json!({"labels": rows})))
}

fn list(core: &Core, args: &Value) -> Result<Value, Failure> {
    let (auth, account) = reader(core)?;
    let mut query = js::trim(&js::string_or_empty(args.get("query"))).to_owned();
    // a card's search, carried to the full screen by its ticket
    if let Some(Subject::Search(q)) = subject_of(core, &js::string_or_empty(args.get("search"))) {
        query = q;
    }
    if !query.is_empty() {
        let found = match &auth {
            Some(auth) => search(core, auth, &account, &query),
            None => Err(unreached()),
        };
        return Ok(match found {
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
    let mut saved = false;
    if (stored.len() as i64) < PAGE {
        // the store has less than a page here: ask Google for the next one
        let filled = match &auth {
            Some(auth) => fill(core, auth, &account, &label),
            None => Err(unreached()),
        };
        match filled {
            Ok(_) => {}
            Err(Failure::Offline(_)) => saved = true,
            Err(e) => return Err(e),
        }
    }
    let rows = with_store(core, |store| store.threads(&account, &label, skip, PAGE))?;
    let rows: Vec<Thread> = rows.into_iter().map(|t| addressed(t, &label)).collect();
    // nothing kept is nothing saved to show
    let saved = saved && !rows.is_empty();
    let total = with_store(core, |store| store.thread_count(&account, &label))?;
    // no token on record yet is not the end: only an empty one is
    let more =
        with_store(core, |store| store.state(&account, &page_key(&label)))?.as_deref() != Some("");
    Ok(ok(
        json!({"threads": rows.iter().map(row_json).collect::<Vec<_>>(), "more": more || skip + PAGE < total, "saved": saved}),
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

pub(crate) fn escape(text: &str) -> String {
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
/// page, so each message's numbers start after the last one's. Messages fold
/// as Gmail folds them ([`fold`]); a folded one is still on the page, so the
/// links and the images it counts are the same.
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
    // the newest and the unread open; the rest fold, a long run behind one row
    let open: Vec<bool> = list
        .iter()
        .enumerate()
        .map(|(i, m)| i + 1 == list.len() || unread(m))
        .collect();
    let hidden = fold::hidden(&open);
    // each message, its history folded, and what its row says: the words
    // before the fold
    // and the Content-IDs of the pictures that went in, whose cards go
    let mut shown: Vec<HashSet<String>> = Vec::with_capacity(list.len());
    let (mut parts, snippets): (Vec<Part>, Vec<String>) = list
        .iter()
        .map(|m| {
            let text = gmail::body(m);
            match gmail::html_body(m) {
                Some(h) => {
                    // the pictures picked for it go in first, so what folds
                    // folds with them
                    let put = std::cell::RefCell::new(HashSet::new());
                    let c = clean::clean_with(&h, &|url| {
                        let data = pictures::inline(m, url);
                        if data.is_some() {
                            put.borrow_mut().insert(url.to_owned());
                        }
                        data
                    });
                    // what the cleaner turned down stays out
                    let put = put.into_inner();
                    shown.push(
                        put.iter()
                            .filter(|url| !c.images.contains(url))
                            .filter_map(|url| gmail::cid_of(url))
                            .collect(),
                    );
                    let quoted = fold::quotes(&c.html).unwrap_or_default();
                    let snippet = gmail::snippet_of(if quoted.regions.is_empty() {
                        &text
                    } else {
                        &quoted.fresh
                    });
                    (Part::Html(c, quoted.regions, text), snippet)
                }
                None => {
                    shown.push(HashSet::new());
                    let cut = fold::plain_cut(&text);
                    let snippet = gmail::snippet_of(&text[..cut.unwrap_or(text.len())]);
                    (Part::Text(text, cut), snippet)
                }
            }
        })
        .unzip();
    // each message's markup before its body and after it
    let meta: Vec<(String, String)> = list
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let (name, address) = sender_of(&gmail::header(m, "From"));
            let name = gmail::cap_bytes(&name, gmail::HEADER_MAX);
            let at = when(millis(m));
            let run = hidden.iter().find(|r| r.contains(&i));
            let mut before = match run {
                Some(r) if r.start == i => fold::more(r.len()),
                _ => String::new(),
            };
            before.push_str(&fold::message(&name, &snippets[i], &day(&at), open[i]));
            before.push_str(&format!(
                "<div class=\"gws-meta\">&lt;{}&gt; · {at}<br>to {}</div>",
                escape(&gmail::cap_bytes(&address, gmail::HEADER_MAX)),
                escape(&gmail::short_header(m, "To"))
            ));
            let mut after = fold::END.to_owned();
            if run.is_some_and(|r| r.end == i + 1) {
                after.push_str(fold::END);
            }
            (before, after)
        })
        .collect();
    let title = format!(
        "<h2 style=\"font-size:18px;margin:0 0 12px\">{}</h2>",
        escape(&gmail::cap_bytes(&subject, gmail::HEADER_MAX))
    );
    // an attachment is fetched from its own message, so it says which that is
    let files = |shown: &[HashSet<String>]| -> Vec<Value> {
        list.iter()
            .zip(shown)
            .flat_map(|(m, shown)| {
                gmail::attachments(m, shown).into_iter().map(|mut a| {
                    a["message"] = m.get("id").cloned().unwrap_or(Value::Null);
                    a
                })
            })
            .collect()
    };
    let mut attachments = files(&shown);
    // what's counted is what's sent: the markup, as JSON
    let mut size = sent(&title)
        + meta.iter().map(|(b, a)| sent(b) + sent(a)).sum::<usize>()
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
    // a message given way to text shows no pictures, so its cards come back
    let mut lost = false;
    for (part, shown) in parts.iter().zip(&mut shown) {
        if !matches!(part, Part::Html(..)) && !shown.is_empty() {
            shown.clear();
            lost = true;
        }
    }
    if lost {
        attachments = files(&shown);
    }
    let (mut page, mut links, mut images) = (title, Vec::new(), 0);
    for ((before, after), part) in meta.iter().zip(parts) {
        page.push_str(before);
        match part {
            Part::Html(c, regions, _) => {
                let (base, count) = (links.len(), c.links.len());
                let html = fold::wrap(&c.html, &regions);
                page.push_str(&clean::rewrite(&html, "gws-link:", |n, out| {
                    let known = n < count;
                    if known {
                        out.push_str(&format!("\"gws-link:{}\"", n + base));
                    }
                    known
                }));
                images += c.images.len();
                links.extend(c.links);
            }
            Part::Text(t, cut) => page.push_str(&fold::split(&t, cut)),
            Part::TooLong => page.push_str(TOO_LONG),
        }
        page.push_str(after);
    }
    json!({"id": thread.get("id"), "page": page, "links": links, "images": images, "attachments": attachments})
}

/// One message of a thread, as much of it as fits.
/// A text keeps where its fold starts, so its row and its fold agree.
enum Part {
    /// The cleaned HTML, what of it folds, and the message's plain text.
    Html(clean::Cleaned, Vec<fold::Region>, String),
    Text(String, Option<usize>),
    TooLong,
}

impl Part {
    fn size(&self) -> usize {
        match self {
            // the fold's markup measured where it goes, nothing built
            Part::Html(c, regions, _) => {
                let mut html = 2;
                fold::pieces(&c.html, regions, |p| html += sent(p) - 2);
                let links = c.links.iter().map(|l| sent(l) + 1).sum::<usize>();
                html + links.max(1) + 1
            }
            Part::Text(t, cut) => sent(&fold::split(t, *cut)),
            Part::TooLong => TOO_LONG.len(),
        }
    }

    fn shrink(&mut self) {
        *self = match std::mem::replace(self, Part::TooLong) {
            // as plain text it keeps the fold where the HTML had it, worked
            // out only now that it's needed
            Part::Html(c, regions, text) => {
                match regions.first().and_then(|r| fold::shrunk(&c.html, r.start)) {
                    Some((shown, cut)) => Part::Text(shown, Some(cut)),
                    None => {
                        let cut = fold::plain_cut(&text);
                        Part::Text(text, cut)
                    }
                }
            }
            _ => Part::TooLong,
        };
    }
}

/// `text`'s size in a JSON result, as serde_json writes it, counted without
/// writing it.
fn sent(text: &str) -> usize {
    let escapes: usize = text
        .bytes()
        .map(|b| match b {
            b'"' | b'\\' | b'\x08' | b'\x0c' | b'\n' | b'\r' | b'\t' => 1,
            0..=0x1f => 5,
            _ => 0,
        })
        .sum();
    text.len() + escapes + 2
}

/// Whether Gmail marks this one message unread.
fn unread(message: &Value) -> bool {
    message
        .get("labelIds")
        .and_then(Value::as_array)
        .is_some_and(|l| l.iter().any(|l| l == "UNREAD"))
}

/// `2026-11-06 09:00`, as a folded row shows it: `Nov 6`.
fn day(at: &str) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let month = at.get(5..7).and_then(|m| m.parse::<usize>().ok());
    let date = at.get(8..10).and_then(|d| d.parse::<u32>().ok());
    match (month.and_then(|m| MONTHS.get(m.wrapping_sub(1))), date) {
        (Some(month), Some(date)) => format!("{month} {date}"),
        _ => at.to_owned(),
    }
}

/// What a saved view was made by. A view of an older shape is made again: a
/// migration that clears them all (as 6, 7 and 10 did) runs once, and this
/// holds for each change after. Raise it whenever `view_of` or what it lists
/// changes.
pub const VIEW_VERSION: i64 = 3;

/// What [`read_view`] came to.
pub(crate) enum Viewed {
    /// Gone at Google.
    Gone,
    /// Past the read's cap, which another read comes to as well.
    TooBig(Failure),
    /// Begun, and outlasting the read's time.
    TooSlow(Failure),
    /// Refused by Gmail's answer, as [`Whole::Failed`].
    Failed(Failure),
    /// What Google sent, the page made of it, and what its pictures and its
    /// invite's twin check came to: a view kept is only a whole one.
    Read {
        full: Value,
        view: Value,
        made: Made,
    },
}

/// A thread read whole from Google, within `limit`, and made into the page
/// an open shows, with what Google sent: an open and a read ahead both keep
/// what this makes, its pictures inside the mail and what they need to show
/// among it.
///
/// `gate` hears of every byte that comes for it, as Gmail sends it: the thread
/// once it's here, and each picture inside it before it's asked for, which a
/// false answer stops.
pub(crate) fn read_view(
    core: &Core,
    auth: &Bearer,
    account: &str,
    id: &str,
    limit: Limit,
    gate: &mut dyn FnMut(u64) -> bool,
) -> Result<Viewed, Failure> {
    let mut full = match core.gmail.thread_whole(auth, id, limit)? {
        Whole::Read(full) => full,
        Whole::Gone => return Ok(Viewed::Gone),
        Whole::TooBig(why) => return Ok(Viewed::TooBig(why)),
        Whole::TooSlow(why) => return Ok(Viewed::TooSlow(why)),
        Whole::Failed(why) => return Ok(Viewed::Failed(why)),
    };
    // past the round's share or after a sign-out, the twin check asks nothing
    let go_on = gate(full.to_string().len() as u64);
    let told = go_on && core.gmail.mark_twins(auth, &mut full);
    // a picture comes as base64 inside JSON
    let mut asks = |size: u64| gate(size.saturating_mul(4).div_ceil(3));
    let (mut view, made) = pictures::view_within(core, auth, account, &mut full, &mut asks);
    view["view_version"] = VIEW_VERSION.into();
    // an `.ics` Gmail sends apart is the invite card's to fetch, so a slow or
    // refused one never holds up the thread: until then it's pending, never
    // none. A read ahead keeps the same
    let invite = crate::invite::fill(None, &mut full)?;
    view["invite"] = invite.unwrap_or_else(crate::invite::pending);
    // a twin check cut short is a picture missed: the next read may finish it
    let made = if told { made } else { Made::Missed };
    Ok(Viewed::Read { full, view, made })
}

fn thread(core: &Core, args: &Value) -> Result<Value, Failure> {
    let (auth, account) = reader(core)?;
    let id = js::trim(&js::string_or_empty(args.get("thread"))).to_owned();
    // a card's row opens by ticket
    let id = match subject_of(core, &id) {
        Some(Subject::Thread(t)) => t,
        _ => id,
    };
    if !gmail::id_ok(&id) {
        return Ok(fail("invalid_args", "That isn't a thread."));
    }
    if let Some(saved) = with_store(core, |store| store.view(&account, &id, now_ms()))?
        && let Ok(view) = serde_json::from_str::<Value>(&saved)
        && view.get("view_version").and_then(Value::as_i64) == Some(VIEW_VERSION)
    {
        return Ok(pictures::answer(core, &account, view));
    }
    // the sign-in is there, and the Bot can't refresh its token just now
    let auth = auth.ok_or_else(unreached)?;
    let (view, made) = match read_view(core, &auth, &account, &id, Limit::FULL, &mut |_| true)? {
        Viewed::Read { view, made, .. } => (view, made),
        Viewed::Gone => return Ok(fail("not_found", "That conversation is gone.")),
        Viewed::TooBig(why) | Viewed::TooSlow(why) | Viewed::Failed(why) => return Err(why),
    };
    // a twin check or a picture cut short isn't kept, so the next open can
    // finish it
    if made == Made::Whole {
        with_own_store(core, &account, |store| {
            store.put_view(&account, &id, &view.to_string(), now_ms())
        })?;
    }
    Ok(pictures::answer(core, &account, view))
}

fn changes(core: &Core, args: &Value) -> Result<Value, Failure> {
    let (auth, account) = reader(core)?;
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
    // with no live token the sync can't run, and what it last said is old news
    let status = match auth {
        Some(_) => status.unwrap_or_else(|| "syncing".to_owned()),
        None => "offline".to_owned(),
    };
    Ok(ok(
        json!({"seq": seq, "changed": seq != since, "status": status}),
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
    fn a_wake_says_what_caused_it_and_a_persons_isnt_lost() {
        let waker = Waker::default();
        waker.wake();
        assert_eq!(waker.wait(), Woken::Other);
        waker.wake_by_person();
        waker.wake();
        assert_eq!(waker.wait(), Woken::Person);
        waker.wake_after(Duration::from_millis(10));
        assert_eq!(waker.wait(), Woken::Timer);
    }

    #[test]
    fn a_wake_after_the_time_to_look_still_counts_as_the_timer() {
        let waker = Waker::default();
        waker.wake_after(Duration::from_millis(20));
        std::thread::sleep(Duration::from_millis(40));
        waker.wake();
        assert_eq!(waker.wait(), Woken::Timer);
    }

    #[test]
    fn a_wake_for_nothing_isnt_the_timer() {
        let waker = Waker::default();
        waker.wake_after(Duration::from_millis(400));
        let start = Instant::now();
        std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(50));
                waker.bell.notify_all();
            });
            assert_eq!(waker.wait(), Woken::Timer);
        });
        assert!(
            start.elapsed() >= Duration::from_millis(400),
            "woke at {:?}",
            start.elapsed()
        );
    }

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

    mod rich_thread {
        use super::*;
        use crate::apis::Apis;
        use gws_fake_google::{FakeGoogle, Seed};

        /// The rich stub's "Site visit schedule", opened, with only the
        /// messages that start as `unread` says still unread.
        fn opened(name: &str, unread: &[&str]) -> Value {
            let mut seed = Seed::default().with_rich_mail("http://127.0.0.1:1");
            let thread = format!("{:016x}", 0x19f0_0000_0000_u64 + 10);
            for m in seed.mails.iter_mut().filter(|m| m.thread == thread) {
                if !unread.iter().any(|u| m.text.starts_with(u)) {
                    m.labels.retain(|l| l != "UNREAD");
                }
            }
            let google = FakeGoogle::start(seed);
            let dir = std::env::temp_dir().join(format!("gws-rich-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let core = Core::new(
                &Apis::at(&google.base()),
                Ok(gws_store::Paths::in_dir(&dir)),
            );
            core.unlock("seed");
            core.account("buwiz.account.set", &json!({"account": "google", "label": "default", "provider": "google", "subject": "owner@example.com", "scopes": [], "access_token": "fake-access-1", "token_type": "Bearer", "expires_at": 4_000_000_000u64}));
            let view =
                core.run(MAIL_THREAD, &json!({"thread": thread}))["structuredContent"].clone();
            let _ = std::fs::remove_dir_all(&dir);
            view
        }

        #[test]
        fn the_site_visit_thread_folds_as_gmail_folds_it() {
            let view = opened("read", &[]);
            let page = view["page"].as_str().unwrap();
            assert_eq!(folds(page), [false, false, false, false, true], "{page}");
            assert!(
                page.contains(r#"<details class="gws-more"><summary><span class="gws-n">3 more messages</span><span class="gws-less">Fewer messages</span></summary>"#),
                "{page}"
            );
            // each reply's history is behind its pill, and its row shows the new words
            assert_eq!(
                page.matches(r#"<details class="gws-quote">"#).count(),
                4,
                "{page}"
            );
            assert!(
                page.contains(r#"<span class="gws-from">You</span><span class="gws-snip">Thursday works. Morning or afternoon?</span>"#),
                "{page}"
            );
            let newest = page.rfind(r#"<details class="gws-msg" open>"#).unwrap();
            let pill = newest
                + page[newest..]
                    .find(r#"<details class="gws-quote">"#)
                    .unwrap();
            assert!(page[newest..pill].contains("See you all at 9"));
            assert!(
                page[pill..].contains("join too"),
                "the history is in the pill"
            );
        }

        #[test]
        fn an_unread_reply_in_the_thread_stays_open() {
            let page = opened("unread", &["Morning, 9 am"])["page"]
                .as_str()
                .unwrap()
                .to_owned();
            assert_eq!(folds(&page), [false, false, true, false, true], "{page}");
            assert!(!page.contains("gws-more"), "no run of more than three");
        }
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

            // a card whose ticket stands for nothing, read with the chat's
            // fields after a switch: never bound to the account signed in now
            switch(&core, A, B);
            let with = |ticket: &str, fields: Value| {
                answer(&core.run(
                    crate::tools::EVENT_DRAFT_READ,
                    &json!({"ticket": ticket, "fields": fields}),
                ))
                .clone()
            };
            let old = buwiz_plugin::new_ticket();
            let said = with(&old, args.clone());
            assert_eq!(
                (said["refused"].clone(), said["message"].clone()),
                (json!("ticket_lost"), json!(TICKET_GONE)),
                "no account recorded: out of date"
            );
            let mut drafted_on_a = args.clone();
            drafted_on_a["as_account"] = json!(A);
            assert_eq!(with(&old, drafted_on_a.clone())["refused"], "not_found");
            assert_eq!(subject_of(&core, &old), None, "nothing was bound");
            // the old card's own ticket, fields or not, stays A's
            assert_eq!(
                with(ticket.as_str().unwrap(), drafted_on_a.clone())["refused"],
                "not_found"
            );
            switch(&core, B, A);
            assert_eq!(
                with(&old, drafted_on_a)["title"],
                "Lunch",
                "back on A it binds to A"
            );
        }

        #[test]
        fn a_card_ticket_reads_only_under_its_own_account() {
            let google = FakeGoogle::start(Seed::demo(1));
            let (core, _) = two_accounts(&google, "two-card");
            let ticket = issue_card(&core, &Subject::Search("from:ana".to_owned()));
            assert_eq!(
                subject_of(&core, &ticket),
                Some(Subject::Search("from:ana".to_owned()))
            );
            switch(&core, A, B);
            assert_eq!(subject_of(&core, &ticket), None);
            assert_eq!(lost_words(&core, &ticket), TICKET_ELSEWHERE);
            assert!(
                !bind(&core, &ticket, &Subject::Search("from:bo".to_owned())),
                "another account's ticket is never bound again"
            );
            let card = core.run(
                crate::tools::MAIL_CARD,
                &json!({"ticket": ticket, "query": "from:bo"}),
            );
            assert_eq!(answer(&card)["message"], TICKET_ELSEWHERE);
            switch(&core, B, A);
            assert_eq!(
                subject_of(&core, &ticket),
                Some(Subject::Search("from:ana".to_owned()))
            );
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

    /// [`message`] with a picture by Content-ID that the view put in.
    fn with_picture(id: &str, html: &str) -> Value {
        let mut m = message(id, &format!("<img src=\"cid:dot\">{html}"));
        let alternative = m["payload"].take();
        m["payload"] = json!({"mimeType": "multipart/related", "headers": alternative["headers"].clone(), "parts": [
            alternative,
            {"partId": "1", "mimeType": "image/png", "filename": "dot.png", "headers": [{"name": "Content-ID", "value": "<dot>"}], "body": {"attachmentId": "x", "size": 70}},
        ]});
        m["gwsPictures"] = json!({"cid:dot": "data:image/png;base64,iVBORw0KGgo="});
        m
    }

    #[test]
    fn a_message_given_way_to_text_gets_its_pictures_cards_back() {
        let small = view_of(&json!({"id": "t", "messages": [with_picture("one", "<p>Hi</p>")]}));
        assert!(small["page"].as_str().unwrap().contains("data:image/png"));
        assert_eq!(small["attachments"], json!([]), "shown, it's the letter's");
        // markup enough to give way, and words few enough to fit then
        let big = "<b>.</b>".repeat(120_000);
        let view = view_of(&json!({"id": "t", "messages": [
            with_picture("old", &big),
            with_picture("new", "<p>Hi</p>"),
        ]}));
        let page = view["page"].as_str().unwrap();
        assert_eq!(
            page.matches("data:image/png").count(),
            1,
            "the newest shows it"
        );
        let cards: Vec<&Value> = view["attachments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| &a["message"])
            .collect();
        assert_eq!(cards, [&json!("old")], "the oldest went to text");
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
        let me = Some("me".to_owned());
        tickets.put("card", &Subject::Search("a card's".to_owned()), &me, true);
        tickets.put("first", &Subject::Thread("first".to_owned()), &me, false);
        for n in 0..TICKETS_MAX * 3 {
            tickets.put(
                &format!("row-{n}"),
                &Subject::Thread(n.to_string()),
                &me,
                false,
            );
        }
        assert_eq!(tickets.subject("first"), None, "the oldest row went");
        let last = format!("row-{}", TICKETS_MAX * 3 - 1);
        assert!(tickets.subject(&last).is_some());
        assert!(tickets.subject("card").is_some(), "a card's never does");
        // a row a card then takes for its own stays one
        tickets.put(&last, &Subject::Thread("x".to_owned()), &me, true);
        tickets.put(&last, &Subject::Thread("x".to_owned()), &me, false);
        for n in 0..TICKETS_MAX * 3 {
            tickets.put(
                &format!("more-{n}"),
                &Subject::Thread(format!("m{n}")),
                &me,
                false,
            );
        }
        assert!(tickets.subject(&last).is_some());
        // cards come in generations too
        for n in 0..CARDS_HELD * 2 {
            tickets.put(
                &format!("card-{n}"),
                &Subject::Search(n.to_string()),
                &me,
                true,
            );
        }
        assert!(
            tickets.subject("card").is_none(),
            "the oldest card here went; the store still has it"
        );
        assert!(
            tickets
                .subject(&format!("card-{}", CARDS_HELD * 2 - 1))
                .is_some()
        );
        // a subject's ticket is whoever's it was issued to
        let you = Some("you".to_owned());
        assert_eq!(tickets.ticket(&you, &Subject::Search("0".to_owned())), None);
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

    /// A message with its HTML and Gmail's labels for it.
    fn labelled(id: &str, html: &str, labels: &[&str]) -> Value {
        let mut m = message(id, html);
        m["labelIds"] = json!(labels);
        m
    }

    /// Whether each message's fold is open, in page order.
    fn folds(page: &str) -> Vec<bool> {
        page.match_indices("<details class=\"gws-msg\"")
            .map(|(at, found)| page[at + found.len()..].starts_with(" open>"))
            .collect()
    }

    #[test]
    fn the_newest_and_the_unread_stay_open_and_the_rest_fold() {
        let view = view_of(&json!({"id": "t", "messages": [
            labelled("a", "<p>one</p>", &["INBOX"]),
            labelled("b", "<p>two</p>", &["INBOX", "UNREAD"]),
            labelled("c", "<p>three</p>", &["INBOX"]),
            labelled("d", "<p>four</p>", &["INBOX"]),
        ]}));
        let page = view["page"].as_str().unwrap();
        assert_eq!(folds(page), [false, true, false, true], "{page}");
        assert!(!page.contains("gws-more"), "two in a row stay rows");
        // each row names its sender and starts what they said
        assert!(
            page.contains(r#"<span class="gws-date">Nov 6</span><span class="gws-from">Ana</span><span class="gws-snip">one</span></summary>"#),
            "{page}"
        );
        assert_eq!(
            page.matches("<details").count(),
            page.matches("</details>").count()
        );
    }

    #[test]
    fn a_long_run_of_read_messages_folds_behind_one_row() {
        let messages: Vec<Value> = (0..7)
            .map(|n| labelled(&n.to_string(), &format!("<p>said {n}</p>"), &["INBOX"]))
            .collect();
        let view = view_of(&json!({"id": "t", "messages": messages}));
        let page = view["page"].as_str().unwrap();
        assert_eq!(
            folds(page),
            [false, false, false, false, false, false, true]
        );
        let more = page
            .find(r#"<details class="gws-more"><summary><span class="gws-n">5 more messages</span><span class="gws-less">Fewer messages</span></summary>"#)
            .expect("the middle collapses");
        assert!(page.find("said 0").unwrap() < more, "the first stays a row");
        let close = page
            .find("said 5</p></details></details>")
            .expect("the run ends inside it");
        for n in 1..6 {
            let at = page.find(&format!("said {n}")).unwrap();
            assert!(more < at && at <= close, "{n} is behind the row");
        }
        assert!(
            page.find("said 6").unwrap() > close,
            "the newest is out of it"
        );
    }

    #[test]
    fn links_and_images_count_on_through_folded_messages_and_quotes() {
        let quoted = r#"<p><a href="https://one.example/">1</a></p><div class="gmail_quote"><a href="https://two.example/">2</a><img src="https://pics.example/a.png"></div>"#;
        let view = view_of(&json!({"id": "t", "messages": [
            labelled("a", quoted, &["INBOX"]),
            labelled("b", r#"<a href="https://three.example/">3</a><img src="https://pics.example/b.png">"#, &["INBOX"]),
            labelled("c", r#"<a href="https://four.example/">4</a>"#, &["INBOX"]),
        ]}));
        let page = view["page"].as_str().unwrap();
        assert_eq!(folds(page), [false, false, true]);
        assert_eq!(
            view["links"],
            json!([
                "https://one.example/",
                "https://two.example/",
                "https://three.example/",
                "https://four.example/"
            ])
        );
        for n in 0..4 {
            assert!(
                page.contains(&format!(r#"<a href="gws-link:{n}">{}</a>"#, n + 1)),
                "{page}"
            );
        }
        let fold = page.find(r#"<details class="gws-quote">"#).unwrap();
        assert!(page[fold..].starts_with(r#"<details class="gws-quote"><summary>•••</summary><div class="gmail_quote"><a href="gws-link:1">"#), "{page}");
        assert_eq!(view["images"], 2);
    }

    #[test]
    fn a_plain_message_folds_its_quoted_lines() {
        use base64::Engine as _;
        let data = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode("Yes.\n\nOn Nov 6, 2026, Ana <ana@example.com> wrote:\n> Can we?\n");
        let plain = json!({"id": "p", "internalDate": "1793955600000", "payload": {"mimeType": "text/plain", "headers": [], "body": {"data": data}}});
        let page = view_of(&json!({"id": "t", "messages": [plain]}))["page"]
            .as_str()
            .unwrap()
            .to_owned();
        let fold = page
            .find(r#"<details class="gws-quote"><summary>•••</summary><pre"#)
            .expect("folded");
        assert!(
            page[..fold].contains(r#"<span class="gws-snip">Yes.</span>"#),
            "{page}"
        );
        assert!(
            page[fold..].contains("On Nov 6, 2026, Ana &lt;ana@example.com&gt; wrote:"),
            "{page}"
        );
    }

    #[test]
    fn a_message_shrunk_to_text_keeps_its_fold() {
        let heavy = format!(
            r#"<p>Fresh words</p><div class="gmail_quote">{}</div>"#,
            "<b>x</b>".repeat(120_000)
        );
        let view = view_of(&json!({"id": "t", "messages": [
            labelled("a", &heavy, &["INBOX"]),
            labelled("b", "<p>last</p>", &["INBOX"]),
        ]}));
        let page = view["page"].as_str().unwrap();
        assert!(!page.contains("<b>x</b>"), "the oldest gave way to text");
        let fold = page
            .find(r#"<details class="gws-quote"><summary>•••</summary><pre"#)
            .expect("the fold stays");
        assert!(page[..fold].contains(r#"<span class="gws-snip">Fresh words</span>"#));
        assert!(page[..fold].contains(">Fresh words</pre>"));
        assert!(page[fold..].starts_with(
            r#"<details class="gws-quote"><summary>•••</summary><pre style="white-space:pre-wrap;font:inherit;margin:0">xxx"#
        ));
    }

    /// The page of a thread whose first message is too big for HTML.
    fn shrunk_page(html: &str) -> String {
        let heavy = format!("<p>Fresh words</p>{}{html}", "<b>x</b>".repeat(120_000));
        let view = view_of(&json!({"id": "t", "messages": [
            labelled("a", &heavy, &["INBOX"]),
            labelled("b", "<p>last</p>", &["INBOX"]),
        ]}));
        let page = view["page"].as_str().unwrap().to_owned();
        assert!(!page.contains("<b>x</b>"), "the oldest gave way to text");
        page
    }

    #[test]
    fn a_shrunk_message_with_a_plain_reply_still_folds_it() {
        let page = shrunk_page("<p>On Nov 6, 2026, Ana wrote:</p><p>&gt; Can we?</p>");
        let fold = page
            .find(r#"<details class="gws-quote"><summary>•••</summary><pre"#)
            .expect("the fold stays");
        assert!(
            page[fold..].contains(">On Nov 6, 2026, Ana wrote:"),
            "{page}"
        );
    }

    #[test]
    fn a_shrunk_message_whose_first_quote_is_hidden_still_folds_the_rest() {
        let page = shrunk_page(
            r#"<div style="display:none"><blockquote type="cite">INJECT</blockquote></div><blockquote type="cite">Old words</blockquote>"#,
        );
        assert!(!page.contains("INJECT"), "{page}");
        let fold = page
            .find(r#"<details class="gws-quote"><summary>•••</summary><pre"#)
            .expect("the fold stays");
        assert!(page[fold..].contains(">Old words</pre>"), "{page}");
    }

    #[test]
    fn a_parts_size_is_what_it_sends_counted_without_building_it() {
        let c = clean::clean(
            r#"<div><a href="https://a.example/?q=&quot;">Sounds</a> "good".<br><i>On Nov 6, 2026, Ana wrote:</i></div><blockquote type="cite">Old	\</blockquote>"#,
        );
        let regions = fold::quotes(&c.html).unwrap().regions;
        assert!(!regions[0].reopen.is_empty(), "the fold reopens an element");
        let expected = sent(&fold::wrap(&c.html, &regions)) + json!(c.links).to_string().len();
        let part = Part::Html(c, regions, String::new());
        let (size, made) = fold::allocations(|| part.size());
        assert_eq!((size, made), (expected, 0));
        let empty = Part::Html(clean::clean("<p>x</p>"), Vec::new(), String::new());
        assert_eq!(empty.size(), sent("<p>x</p>") + 2);
        for text in ["", "plain", "\"\\\n\r\t\u{8}\u{c}\u{1}\u{1f}\u{7f}é😀</>"] {
            assert_eq!(sent(text), Value::from(text).to_string().len(), "{text:?}");
        }
    }

    #[test]
    fn a_shrunk_message_shows_none_of_what_its_page_hides() {
        let page = shrunk_page(
            r#"<div style="display:none"><blockquote type="cite">INJECT</blockquote></div>"#,
        );
        assert!(!page.contains("INJECT"), "{page}");
    }

    #[test]
    fn a_bare_attribution_and_a_quote_is_not_a_row_of_its_own() {
        let view = view_of(&json!({"id": "t", "messages": [
            labelled("a", r#"<div>On Nov 6, 2026, Ana wrote:</div><blockquote type="cite"><div>Old words</div></blockquote>"#, &["INBOX"]),
            labelled("b", "<p>last</p>", &["INBOX"]),
        ]}));
        let page = view["page"].as_str().unwrap();
        assert!(!page.contains("gws-quote"), "{page}");
        assert!(
            page.contains(r#"<span class="gws-snip">On Nov 6, 2026, Ana wrote: Old words</span>"#),
            "{page}"
        );
    }

    #[test]
    fn a_folded_row_shows_none_of_what_the_fold_hides() {
        // Apple's reply has no "wrote:" line, only the cited quote
        let view = view_of(&json!({"id": "t", "messages": [
            labelled("a", r#"<div>Sounds good.</div><blockquote type="cite"><div>Old words</div></blockquote>"#, &["INBOX"]),
            labelled("b", "<p>last</p>", &["INBOX"]),
        ]}));
        let page = view["page"].as_str().unwrap();
        assert!(
            page.contains(r#"<span class="gws-snip">Sounds good.</span>"#),
            "{page}"
        );
        assert!(
            page.contains(
                r#"<details class="gws-quote"><summary>•••</summary><blockquote type="cite">"#
            ),
            "{page}"
        );
    }

    #[test]
    fn a_mail_that_is_only_a_quote_is_not_hidden() {
        let view = view_of(&json!({"id": "t", "messages": [
            labelled("a", r#"<div class="gmail_quote"><p>Forwarded words</p></div>"#, &["INBOX"]),
        ]}));
        let page = view["page"].as_str().unwrap();
        assert!(!page.contains("gws-quote"), "{page}");
        assert!(
            page.contains(r#"<span class="gws-snip">Forwarded words</span>"#),
            "{page}"
        );
    }

    #[test]
    fn a_mail_that_writes_its_own_folds_gets_none() {
        let hostile = r#"<p>Hi</p></details></details><details open class="gws-msg"><summary class="gws-quote">•••</summary>fake</details><div class="gws-more">x</div>"#;
        let view = view_of(&json!({"id": "t", "messages": [
            labelled("a", hostile, &["INBOX"]),
            labelled("b", "<p>last</p>", &["INBOX"]),
        ]}));
        let page = view["page"].as_str().unwrap();
        assert_eq!(
            page.matches("<details").count(),
            2,
            "only the two messages': {page}"
        );
        assert_eq!(page.matches("</details>").count(), 2, "{page}");
        assert_eq!(page.matches("<summary>").count(), 2, "{page}");
        assert_eq!(folds(page), [false, true]);
        // the first message's words stay inside its own fold
        let first_end = page.find("</details>").unwrap();
        assert!(page.find("fake").unwrap() < first_end, "{page}");
    }

    #[test]
    fn what_the_model_reads_of_a_quoted_mail_is_unchanged() {
        // the model's tools read `gmail::body`, which folding never touches
        let quoted = r#"<div>Sounds good.</div><div>On Nov 6, Ana wrote:</div><blockquote type="cite"><div>Can we?</div></blockquote>"#;
        assert_eq!(
            clean::visible_text(quoted),
            "Sounds good.\n\nOn Nov 6, Ana wrote:\n\nCan we?"
        );
    }

    #[test]
    fn dates_read_plainly() {
        assert_eq!(day("2026-11-06 09:00"), "Nov 6");
        assert_eq!(day("2026-01-30 09:00"), "Jan 30");
        assert_eq!(when_in(1_793_955_600_000, 0), "2026-11-06 09:00");
        assert_eq!(when_in(1_793_955_600_000, 8 * 3600), "2026-11-06 17:00");
    }
}
