//! Recent mail on this computer ahead of time, so it opens at once and with no
//! connection: after a list sync the timer or the person started, the views of
//! the last 30 days' threads, made by the open's own [`read_view`], and their
//! attachments under 5 MB that can be opened.
//!
//! It runs on a thread of its own, so the list sync never waits for it, and it
//! holds the store only to read and write, never while Google answers, so a
//! person's open never waits for it either. It's the person's own mail for the
//! person's own screens: nothing here is the model's to read.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use gws_store::mail::BODIES_MAX;
use gws_store::prefetch::{Ahead, Saved};
use serde_json::Value;

use crate::Core;
use crate::accounts::Bearer;
use crate::gmail::{self, Failure};
use crate::mail::{Viewed, now_ms, read_view, row_of, signed_in, with_own_store, with_store};
use crate::pictures;

/// How far back mail is kept ahead.
pub const WINDOW_MS: i64 = 30 * 24 * 60 * 60 * 1000;
/// Threads read in one round; the rest wait for the next.
pub const BATCH: usize = 20;
/// Attachments this size and over are fetched at the press.
pub const ATTACHMENT_MAX: i64 = 5 * 1024 * 1024;
/// The most one round downloads, views and attachments together.
pub const ROUND_MAX: i64 = 50 * 1024 * 1024;
/// Room a view may need: what [`read_view`] makes stops short of a MiB.
const VIEW_ROOM: i64 = 1024 * 1024;
/// The wait after a round that failed, doubling to the most while they keep
/// failing.
pub const BACKOFF_MIN_MS: i64 = 60_000;
pub const BACKOFF_MAX_MS: i64 = 60 * 60_000;
/// As long as a part id or an attachment id may be, as the open checks them.
const PART_MAX: usize = 64;
const ID_MAX: usize = 4096;

/// Whether a round is on its way, and the backing off after one failed.
#[derive(Default)]
pub struct Running {
    busy: AtomicBool,
    /// Asked for while one was on its way: one more goes after it.
    again: AtomicBool,
    /// Whose the last round was: another's starts with no wait.
    account: std::sync::Mutex<String>,
    /// No round before this, by the Core's clock.
    not_before: AtomicI64,
    backoff: AtomicI64,
}

/// Lets the next round start, however this one ended.
struct Done<'a>(&'a AtomicBool);

impl Drop for Done<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Starts a round on its own thread. One still going takes it as one more to
/// run after it, so a wake the person caused isn't lost to it.
pub fn kick(core: &Arc<Core>) {
    let running = &core.prefetch;
    running.again.store(true, Ordering::Release);
    if running.busy.swap(true, Ordering::AcqRel) {
        return;
    }
    let core = core.clone();
    std::thread::spawn(move || {
        let running = &core.prefetch;
        loop {
            let done = Done(&running.busy);
            while running.again.swap(false, Ordering::AcqRel) {
                let _ = run(&core);
            }
            drop(done);
            // one asked for after the last look, and before busy went
            if !running.again.load(Ordering::Acquire) || running.busy.swap(true, Ordering::AcqRel) {
                return;
            }
        }
    });
}

/// One round: the views due, then the attachments of what's kept. It stops at
/// the first failure, a busy answer from Google among them, and then no round
/// starts until the wait after it is over.
pub fn run(core: &Core) -> Result<(), Failure> {
    let now = core.clock.now_ms();
    let running = &core.prefetch;
    // a wait is one account's: signed out or another's, it's over
    let fresh = |running: &Running| {
        running.backoff.store(0, Ordering::Release);
        running.not_before.store(0, Ordering::Release);
    };
    let Some((account, _)) = core.accounts.signed_in() else {
        fresh(running);
        return Err(Failure::AccountRequired);
    };
    {
        let mut last = running.account.lock().unwrap_or_else(|e| e.into_inner());
        if *last != account {
            fresh(running);
            *last = account;
        }
    }
    if now < running.not_before.load(Ordering::Acquire) {
        return Ok(());
    }
    let result = round(core, now);
    let wait = match &result {
        Ok(()) | Err(Failure::AccountRequired) => 0,
        Err(_) => {
            (running.backoff.load(Ordering::Acquire) * 2).clamp(BACKOFF_MIN_MS, BACKOFF_MAX_MS)
        }
    };
    running.backoff.store(wait, Ordering::Release);
    running.not_before.store(now + wait, Ordering::Release);
    result
}

fn round(core: &Core, now: i64) -> Result<(), Failure> {
    round_within(core, now, ROUND_MAX, gmail::FULL_MAX)
}

/// [`round`], downloading no more than `max`, and no thread past `read_max`.
fn round_within(core: &Core, now: i64, max: i64, read_max: u64) -> Result<(), Failure> {
    let (_, account) = signed_in(core)?;
    let since = now - WINDOW_MS;
    let mut spent = 0;
    bodies(core, &account, since, &mut spent, max, read_max)?;
    attachments(core, &account, since, &mut spent, max)
}

/// The token to ask Google with, read again before each call: a sign-out or a
/// switch between two calls ends the round, and no call goes out after it.
fn still(core: &Core, account: &str) -> Result<Bearer, Failure> {
    match core.accounts.session() {
        Some((auth, who)) if who == account && !core.sync.stopping() => Ok(auth),
        _ => Err(Failure::AccountRequired),
    }
}

/// Views of recent threads that have none, newest first.
fn bodies(
    core: &Core,
    account: &str,
    since: i64,
    spent: &mut i64,
    max: i64,
    read_max: u64,
) -> Result<(), Failure> {
    let due = with_store(core, |store| store.unviewed_since(account, since, BATCH))?;
    for id in due {
        if *spent >= max {
            break;
        }
        // a full budget is known before a download, not after it
        let held = with_store(core, |store| store.recent_view_bytes(account, since))?;
        if held + VIEW_ROOM > BODIES_MAX {
            break;
        }
        let auth = still(core, account)?;
        let before = *spent;
        // every byte that comes counts, the pictures inside the mail among them,
        // and none is asked for past the round's share or after a sign-out
        let (mut over, mut gone) = (false, false);
        let read = read_view(core, &auth, account, &id, read_max, &mut |bytes| {
            let bytes = bytes as i64;
            *spent += bytes;
            if still(core, account).is_err() {
                gone = true;
            } else if *spent > max {
                over = true;
            }
            !(gone || over)
        })?;
        if gone {
            return Err(Failure::AccountRequired);
        }
        let (full, view, short) = match read {
            Viewed::Read { full, view, short } => (full, view, short),
            Viewed::Gone => {
                // gone at Google, as a list sync would find
                with_own_store(core, account, |store| store.remove_thread(account, &id))?;
                continue;
            }
            // past what one read takes, it would fail each round and hold up
            // everything older: an open says why. What came of it counts.
            Viewed::TooBig(_) => {
                *spent += read_max as i64;
                with_own_store(core, account, |store| store.pass_thread(account, &id))?;
                continue;
            }
        };
        // a thread that alone passes a round's share would come again each
        // round and never be kept, and nothing older after it: an open
        // fetches it. Nor is a view kept that Google said too little of a
        // picture for, as if it were whole: the open reads it again.
        if (over && before == 0) || short {
            with_own_store(core, account, |store| store.pass_thread(account, &id))?;
            continue;
        }
        // a view short of its pictures isn't what an open makes: next round
        if over {
            break;
        }
        let view = view.to_string();
        // what was read is what the row says, or the next sync has news of it
        let Some(row) = row_of(&full) else {
            with_own_store(core, account, |store| store.pass_thread(account, &id))?;
            continue;
        };
        let kept = with_own_store(core, account, |store| {
            store.put_view_ahead(account, &id, (row.at, row.messages), &view, now_ms(), since)
        })?;
        if kept == Ahead::Full {
            break;
        }
    }
    Ok(())
}

/// What a kept view lists that could be kept ahead: `(message, part)`.
fn wanted(core: &Core, account: &str, view: &str) -> Result<Vec<(String, String)>, Failure> {
    let view: Value = serde_json::from_str(view).unwrap_or_default();
    let listed = view.get("attachments").and_then(Value::as_array);
    let mut out = Vec::new();
    for a in listed.into_iter().flatten() {
        let text = |key: &str| a.get(key).and_then(Value::as_str).unwrap_or_default();
        let (message, part) = (text("message"), text("part"));
        let size = a.get("size").and_then(Value::as_i64).unwrap_or(-1);
        if gmail::id_ok(message)
            && part_ok(part)
            // no size said is no reason to leave it: the fresh read may say one
            && size < ATTACHMENT_MAX
            && opens(text("mime"), text("name"))
            && !with_store(core, |store| store.has_attachment(account, message, part))?
        {
            out.push((message.to_owned(), part.to_owned()));
        }
    }
    Ok(out)
}

/// What can be opened here: the kinds an open takes, and pictures and PDFs.
/// Nothing else is worth a byte of the budget.
fn opens(mime: &str, name: &str) -> bool {
    crate::files::openable(mime, name).is_some()
        || mime.starts_with("image/")
        || mime == "application/pdf"
}

/// The attachments under [`ATTACHMENT_MAX`] of kept recent views. The thread is
/// read fresh for them: attachment ids change between reads, and only the mail
/// says which parts are pictures shown inside it, which the page has already.
fn attachments(
    core: &Core,
    account: &str,
    since: i64,
    spent: &mut i64,
    max: i64,
) -> Result<(), Failure> {
    let due = with_store(core, |store| {
        store.views_unfetched_since(account, since, BATCH)
    })?;
    for (thread, at, view) in due {
        let wanted = wanted(core, account, &view)?;
        if wanted.is_empty() {
            with_own_store(core, account, |store| store.mark_fetched(account, &thread))?;
            continue;
        }
        if *spent >= max {
            return Ok(());
        }
        let auth = still(core, account)?;
        let Some(full) = core.gmail.thread_full(&auth, &thread)? else {
            with_own_store(core, account, |store| store.tried(account, &thread))?;
            continue;
        };
        *spent += came(&full);
        let inline = inline_parts(&full);
        let messages = full.get("messages").and_then(Value::as_array);
        let mut whole = true;
        for (message, part) in wanted {
            let found = messages
                .into_iter()
                .flatten()
                .filter(|m| m.get("id").and_then(Value::as_str) == Some(&message))
                .flat_map(gmail::attachments)
                .find(|a| a.get("part").and_then(Value::as_str) == Some(&part));
            let Some(found) = found else {
                whole = false;
                continue;
            };
            if inline.contains(&(message.clone(), part.clone())) {
                continue;
            }
            let text = |key: &str| found.get(key).and_then(Value::as_str).unwrap_or_default();
            let (id, mime, name) = (text("id"), text("mime"), text("name"));
            let size = found.get("size").and_then(Value::as_i64).unwrap_or(-1);
            if size >= ATTACHMENT_MAX || !opens(mime, name) {
                continue;
            }
            // Google said too little to fetch it by: another read may say more
            if !id_ok(id) || size < 0 {
                whole = false;
                continue;
            }
            if *spent + encoded(size) > max {
                return Ok(());
            }
            // full of newer mail's: nothing older is worth asking for
            if !with_store(core, |store| store.attachment_room(account, at, size))? {
                return Ok(());
            }
            let auth = still(core, account)?;
            let Some(bytes) = core.gmail.attachment(&auth, &message, id)? else {
                whole = false;
                continue;
            };
            *spent += encoded(bytes.len() as i64);
            // the size was Google's word; what counts is what came
            if bytes.len() as i64 >= ATTACHMENT_MAX {
                continue;
            }
            let saved = Saved {
                thread: thread.clone(),
                at,
                message,
                part,
                name: name.to_owned(),
                mime: mime.to_owned(),
                bytes,
            };
            let kept = with_own_store(core, account, |store| {
                store.put_attachment(account, &saved, now_ms())
            })?;
            if !kept {
                return Ok(());
            }
        }
        with_own_store(core, account, |store| {
            if whole {
                store.mark_fetched(account, &thread)
            } else {
                store.tried(account, &thread)
            }
        })?;
    }
    Ok(())
}

/// `(message, part)` of each part shown inside its mail by Content-ID, not
/// attached to it, and of each attached one its page shows all the same: the
/// page shows those itself.
fn inline_parts(thread: &Value) -> HashSet<(String, String)> {
    fn walk(message: &str, part: &Value, out: &mut HashSet<(String, String)>) {
        let header = |name: &str| {
            part.get("headers")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .find(|h| {
                    h.get("name")
                        .and_then(Value::as_str)
                        .is_some_and(|n| n.eq_ignore_ascii_case(name))
                })
                .and_then(|h| h.get("value"))
                .and_then(Value::as_str)
        };
        let attached = header("Content-Disposition").is_some_and(|d| {
            d.trim_start()
                .to_ascii_lowercase()
                .starts_with("attachment")
        });
        if header("Content-ID").is_some() && !attached {
            let id = match part.get("partId").and_then(Value::as_str) {
                Some("") | None => "root",
                Some(id) => id,
            };
            out.insert((message.to_owned(), id.to_owned()));
        }
        for p in part
            .get("parts")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            walk(message, p, out);
        }
    }
    let mut out = HashSet::new();
    for m in thread
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let (Some(id), Some(payload)) = (m.get("id").and_then(Value::as_str), m.get("payload")) {
            walk(id, payload, &mut out);
            // an attached part the page shows is a picture of the view's,
            // which came with it: not to be fetched again
            for part in pictures::shown_inside(m) {
                out.insert((id.to_owned(), part));
            }
        }
    }
    out
}

/// How much of the round a whole read took: its JSON as Gmail sends it, not
/// the view kept of it.
fn came(full: &Value) -> i64 {
    full.to_string().len() as i64
}

/// How much of the round `n` bytes of attachment take: Gmail sends them as
/// base64 inside JSON.
fn encoded(n: i64) -> i64 {
    (n * 4 + 2) / 3
}

/// An attachment kept ahead, for an open to use before it asks Google. Its name
/// and type are as Google gave them, a stranger's still, and the open checks
/// them as it checks a fresh read's.
pub fn saved_attachment(core: &Core, account: &str, message: &str, part: &str) -> Option<Saved> {
    with_store(core, |store| {
        store.attachment(account, message, part, now_ms())
    })
    .ok()
    .flatten()
}

fn part_ok(part: &str) -> bool {
    !part.is_empty()
        && part.len() <= PART_MAX
        && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.')
}

fn id_ok(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= ID_MAX
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apis::Apis;
    use gws_fake_google::{FAKE_EMAIL, FakeGoogle, Mail, Seed};
    use serde_json::json;

    const DAY: i64 = 86_400_000;
    const NOW: i64 = 1_800_000_000_000;

    fn mail(n: u64, days_ago: i64) -> Mail {
        let id = format!("1a{n:014x}");
        Mail {
            id: id.clone(),
            thread: id,
            labels: vec!["INBOX".to_owned()],
            from: "Ana Reyes <ana@example.com>".to_owned(),
            to: FAKE_EMAIL.to_owned(),
            subject: format!("Mail {n}"),
            at: NOW - days_ago * DAY,
            text: format!("Fake mail number {n}."),
            html: None,
            attachments: Vec::new(),
            message_id: String::new(),
            inline: Vec::new(),
        }
    }

    /// A mail with `count` pictures inside it of `size` bytes each.
    fn with_pictures(n: u64, days_ago: i64, count: usize, size: usize) -> Mail {
        let mut m = mail(n, days_ago);
        m.html = Some(
            (0..count)
                .map(|i| format!("<img src=\"cid:p{i}\">"))
                .collect(),
        );
        m.inline = (0..count)
            .map(|i| {
                let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
                png.resize(size, i as u8);
                (
                    format!("p{i}"),
                    format!("p{i}.png"),
                    "image/png".to_owned(),
                    png,
                )
            })
            .collect();
        m
    }

    fn synced(google: &FakeGoogle, name: &str) -> Core {
        let dir =
            std::env::temp_dir().join(format!("gws-ahead-unit-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let core = Core::new(&Apis::at(&google.base()), Ok(gws_store::Paths::in_dir(dir)));
        core.unlock("seed");
        let set = core.account("buwiz.account.set", &json!({"account": "google", "label": "default", "provider": "google", "subject": FAKE_EMAIL, "scopes": ["https://www.googleapis.com/auth/gmail.modify"], "access_token": "fake-access-1", "token_type": "Bearer", "expires_at": 4_000_000_000u64}));
        assert_ne!(set["isError"], true, "{set}");
        core.clock.set(NOW);
        crate::mail::sync(&core).unwrap();
        core
    }

    fn has_view(core: &Core, n: u64) -> bool {
        with_store(core, |s| s.view(FAKE_EMAIL, &format!("1a{n:014x}"), 0))
            .unwrap()
            .is_some()
    }

    fn reads_of(google: &FakeGoogle, n: u64) -> usize {
        let id = format!("1a{n:014x}");
        google
            .log()
            .iter()
            .filter(|s| {
                s.target.contains(&format!("/threads/{id}")) && s.target.contains("format=full")
            })
            .count()
    }

    fn attachment_reads(google: &FakeGoogle) -> usize {
        google
            .log()
            .iter()
            .filter(|s| s.target.contains("/attachments/"))
            .count()
    }

    #[test]
    fn a_thread_bigger_than_a_round_is_left_to_its_open_and_older_mail_comes() {
        let mut big = mail(1, 1);
        big.text = "x".repeat(200 * 1024);
        let google = FakeGoogle::start(Seed {
            mails: vec![big, mail(2, 2)],
            ..Seed::default()
        });
        let core = synced(&google, "too-big");
        for _ in 0..3 {
            round_within(&core, NOW, 100 * 1024, gmail::FULL_MAX).unwrap();
        }
        assert!(has_view(&core, 2), "older mail isn't held up by it");
        assert!(!has_view(&core, 1), "it waits for an open");
        assert_eq!(reads_of(&google, 1), 1, "downloaded once, not each round");
    }

    #[test]
    fn a_thread_past_what_a_read_takes_is_left_to_its_open_and_older_mail_comes() {
        let mut big = mail(1, 1);
        big.text = "x".repeat(200 * 1024);
        let google = FakeGoogle::start(Seed {
            mails: vec![big, mail(2, 2)],
            ..Seed::default()
        });
        let core = synced(&google, "past-read");
        // one round, with no backing off: the next thread comes in it
        round_within(&core, NOW, ROUND_MAX, 100 * 1024).unwrap();
        assert!(has_view(&core, 2), "older mail isn't held up by it");
        assert!(!has_view(&core, 1), "it waits for an open");
        round_within(&core, NOW, ROUND_MAX, 100 * 1024).unwrap();
        assert_eq!(reads_of(&google, 1), 1, "asked for once, not each round");
    }

    #[test]
    fn the_pictures_inside_a_mail_count_against_the_round() {
        let google = FakeGoogle::start(Seed {
            mails: vec![
                with_pictures(1, 1, 3, 50 * 1024),
                with_pictures(2, 2, 3, 50 * 1024),
            ],
            ..Seed::default()
        });
        let core = synced(&google, "pictures-share");
        // each picture comes as 67 KiB of base64: the first mail's three make
        // 200, and the second's stop when the next would pass 300
        round_within(&core, NOW, 300 * 1024, gmail::FULL_MAX).unwrap();
        assert!(has_view(&core, 1));
        assert!(!has_view(&core, 2), "short of its pictures, it waits");
        assert!(
            attachment_reads(&google) < 6,
            "{}",
            attachment_reads(&google)
        );
        round_within(&core, NOW, 300 * 1024, gmail::FULL_MAX).unwrap();
        assert!(has_view(&core, 2), "the next round takes it");
    }

    fn part(id: &str, cid: &str, disposition: &str) -> Value {
        json!({"partId": id, "mimeType": "image/png", "filename": format!("{cid}.png"), "headers": [{"name": "Content-ID", "value": format!("<{cid}>")}, {"name": "Content-Disposition", "value": disposition}], "body": {"attachmentId": "att", "size": 1024}})
    }

    #[test]
    fn an_attached_picture_the_page_shows_is_the_pages_and_an_unshown_one_isnt() {
        use base64::Engine as _;
        let html = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode("<p><img src=\"cid:logo\"></p>");
        let thread = json!({"messages": [{"id": "m1", "payload": {"partId": "", "mimeType": "multipart/related", "parts": [
            {"partId": "0", "mimeType": "text/html", "body": {"data": html, "size": 10}},
            part("1", "logo", "attachment; filename=\"logo.png\""),
            part("2", "outlook", "attachment; filename=\"quote.png\""),
            part("3", "kept", "inline"),
        ]}}]});
        let inline = inline_parts(&thread);
        let has = |p: &str| inline.contains(&("m1".to_owned(), p.to_owned()));
        assert!(has("1"), "shown in the page: not fetched twice");
        assert!(!has("2"), "an attachment named by nothing is an attachment");
        assert!(has("3"));
    }
}
