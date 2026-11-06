//! Mail for the screens: a background sync into the store, and the tools that
//! read it. The screens read what's stored, so a slow or missing connection
//! shows as old mail, not as a blank window. Google is asked only by the sync,
//! by a search, and for a thread not yet opened.

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gws_store::Store;
use gws_store::mail::{Label, Thread};
use serde_json::{Value, json};

use crate::accounts::Bearer;
use crate::clean;
use crate::gmail::{self, Failure};
use crate::tools::{
    MAIL_CARD, MAIL_CHANGES, MAIL_LABELS, MAIL_LIST, MAIL_THREAD, SEARCH_MAIL, fail, failed, ok,
};
use crate::{Core, js};

/// Rows a list page holds.
pub const PAGE: i64 = 50;
/// What a thread's view may come to: a result stops at 1 MiB.
const VIEW_MAX: usize = 900 * 1024;
/// How often the sync runs with a screen open, and with none.
const ACTIVE: Duration = Duration::from_secs(30);
const IDLE: Duration = Duration::from_secs(300);
/// A screen counts as open this long after its last call.
const LINGER: Duration = Duration::from_secs(120);
const SEQ: &str = "mail_seq";
const HISTORY: &str = "mail_history";
const STATUS: &str = "mail_status";

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// What a card's ticket stands for.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Subject {
    Search(String),
    Thread(String),
}

/// How many rows a card shows.
const CARD_ROWS: usize = 5;

/// Offers a card with a chat's mail search. Nothing about the mail goes in it:
/// props hold a ticket, and the card reads its rows itself.
pub fn offer_card(core: &Core, tool: &str, args: &Value, result: &mut Value) {
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

/// Wakes the sync, and remembers when a screen last asked for something.
#[derive(Default)]
pub struct Waker {
    state: Mutex<(bool, Option<Instant>)>,
    bell: Condvar,
}

impl Waker {
    pub fn wake(&self) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).0 = true;
        self.bell.notify_all();
    }

    fn seen(&self) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).1 = Some(Instant::now());
    }

    fn wait(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let every = if state.1.is_some_and(|at| at.elapsed() < LINGER) {
            ACTIVE
        } else {
            IDLE
        };
        if !state.0 {
            state = self
                .bell
                .wait_timeout(state, every)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        state.0 = false;
    }
}

/// Starts the background sync.
pub fn start(core: Arc<Core>) {
    std::thread::spawn(move || {
        loop {
            core.sync.wait();
            if core.provisioned() {
                let _ = sync(&core);
            }
        }
    });
}

fn with_store<T>(
    core: &Core,
    f: impl FnOnce(&Store) -> gws_store::Result<T>,
) -> Result<T, Failure> {
    let held = core.store().map_err(|e| Failure::Plain(e.message))?;
    let store = held
        .as_ref()
        .ok_or_else(|| Failure::Plain("The local mail store isn't open.".to_owned()))?;
    f(store).map_err(|e| Failure::Plain(e.message))
}

fn signed_in(core: &Core) -> Result<(Bearer, String), Failure> {
    let auth = core.accounts.google().ok_or(Failure::AccountRequired)?;
    let subject = core
        .accounts
        .signed_in()
        .map(|(s, _)| s)
        .ok_or(Failure::AccountRequired)?;
    Ok((auth, subject))
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
    with_store(core, |store| {
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
    with_store(core, |store| store.put_labels(account, &labels))
}

fn bump(core: &Core, account: &str) -> Result<i64, Failure> {
    with_store(core, |store| {
        let next = store
            .state(account, SEQ)?
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0)
            + 1;
        store.set_state(account, SEQ, &next.to_string())?;
        Ok(next)
    })
}

fn sync_once(core: &Core) -> Result<bool, Failure> {
    let (auth, account) = signed_in(core)?;
    let cursor = with_store(core, |store| store.state(&account, HISTORY))?;
    let changed = match cursor
        .as_deref()
        .map(|c| core.gmail.history(&auth, c))
        .transpose()?
        .flatten()
    {
        Some((touched, newest)) => {
            fetch_rows(core, &auth, &account, &touched)?;
            with_store(core, |store| store.set_state(&account, HISTORY, &newest))?;
            !touched.is_empty()
        }
        // no cursor, or one Google has forgotten: start from the inbox again
        None => {
            let newest = core.gmail.history_now(&auth)?.unwrap_or_default();
            let (ids, next) =
                core.gmail
                    .thread_ids(&auth, Some("INBOX"), None, None, PAGE as usize)?;
            fetch_rows(core, &auth, &account, &ids)?;
            with_store(core, |store| {
                store.set_state(&account, HISTORY, &newest)?;
                store.set_state(&account, &page_key("INBOX"), next.as_deref().unwrap_or(""))
            })?;
            true
        }
    };
    // counts move only with the mail, and each label costs a request
    if changed {
        sync_labels(core, &auth, &account)?;
    }
    Ok(changed)
}

/// One sync, and its outcome kept for the screens. A change bumps the sequence.
pub fn sync(core: &Core) -> Result<(), Failure> {
    let result = sync_once(core);
    let status = match &result {
        Ok(_) => "ok",
        Err(Failure::Offline(_)) => "offline",
        Err(Failure::NeedsAccess) => "needs_access",
        Err(Failure::AccountRequired) => return Err(Failure::AccountRequired),
        Err(Failure::Plain(_)) => "failed",
    };
    let account = core
        .accounts
        .signed_in()
        .map(|(s, _)| s)
        .unwrap_or_default();
    with_store(core, |store| store.set_state(&account, STATUS, status))?;
    if matches!(result, Ok(true)) {
        bump(core, &account)?;
    }
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
    let total = with_store(core, |store| store.thread_count(&account, &label))?;
    // no token on record yet is not the end: only an empty one is
    let more =
        with_store(core, |store| store.state(&account, &page_key(&label)))?.as_deref() != Some("");
    Ok(ok(
        json!({"threads": rows.iter().map(row_json).collect::<Vec<_>>(), "more": more || skip + PAGE < total, "saved": false}),
    ))
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
    with_store(core, |store| {
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

/// `millis` in the form a mail's header shows, UTC: the screen has no clock of
/// the person's zone until the Bot gives it one.
fn when(ms: i64) -> String {
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
    format!(
        "{y}-{m:02}-{d:02} {:02}:{:02} UTC",
        secs / 3600,
        secs / 60 % 60
    )
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
    let mut cleaned: Vec<Option<clean::Cleaned>> = list
        .iter()
        .map(|m| gmail::html_body(m).map(|h| clean::clean(&h)))
        .collect();
    let text: Vec<String> = list.iter().map(gmail::body).collect();
    // plain text is sent escaped, which is what counts against the limit
    let weight =
        |c: &Option<clean::Cleaned>, t: &str| c.as_ref().map_or(escape(t).len(), |c| c.html.len());
    let mut size: usize = cleaned.iter().zip(&text).map(|(c, t)| weight(c, t)).sum();
    // past what a result can carry, the oldest fall back to plain text
    for (c, t) in cleaned.iter_mut().zip(&text) {
        if size <= VIEW_MAX {
            break;
        }
        size -= weight(c, t);
        *c = None;
        size += weight(c, t);
    }
    let (mut page, mut links, mut images, mut attachments) = (
        format!(
            "<h2 style=\"font-size:18px;margin:0 0 12px\">{}</h2>",
            escape(&subject)
        ),
        Vec::new(),
        0,
        Vec::new(),
    );
    for ((m, c), t) in list.iter().zip(cleaned).zip(&text) {
        let (name, address) = sender_of(&gmail::header(m, "From"));
        page.push_str(&format!(
            "<div class=\"gws-meta\"><b>{}</b> &lt;{}&gt; · {}<br>to {}</div>",
            escape(&name),
            escape(&address),
            when(millis(m)),
            escape(&gmail::header(m, "To"))
        ));
        match c {
            Some(mut c) => {
                for n in (0..c.links.len()).rev() {
                    c.html = c.html.replace(
                        &format!("\"gws-link:{n}\""),
                        &format!("\"gws-link:{}\"", n + links.len()),
                    );
                }
                images += c.images.len();
                links.append(&mut c.links);
                page.push_str(&c.html);
            }
            None if size > VIEW_MAX && t.len() > VIEW_MAX / 2 => {
                page.push_str("<p>This message is too long to show here.</p>")
            }
            None => page.push_str(&format!(
                "<pre style=\"white-space:pre-wrap;font:inherit;margin:0\">{}</pre>",
                escape(t)
            )),
        }
        attachments.extend(gmail::attachments(m));
    }
    json!({"id": thread.get("id"), "page": page, "links": links, "images": images, "attachments": attachments})
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
    let Some(full) = core.gmail.thread_full(&auth, &id)? else {
        return Ok(fail("not_found", "That conversation is gone."));
    };
    let view = view_of(&full);
    with_store(core, |store| {
        store.put_view(&account, &id, &view.to_string(), now_ms())
    })?;
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
        _ => return None,
    };
    core.sync.seen();
    Some(result.unwrap_or_else(failed))
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn an_oversized_thread_gives_up_html_oldest_first() {
        let big = format!("<p>{}</p>", "x".repeat(500 * 1024));
        let view =
            view_of(&json!({"id": "t", "messages": [message("old", &big), message("new", &big)]}));
        let page = view["page"].as_str().unwrap();
        assert!(
            page.contains("plain old") && !page.contains("plain new"),
            "the oldest gives way"
        );
        assert!(
            page.len() < 900 * 1024 + 4096 && view.to_string().len() < 1024 * 1024,
            "fits a result"
        );
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
        assert_eq!(when(1_793_955_600_000), "2026-11-06 09:00 UTC");
    }
}
