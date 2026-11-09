//! Recent mail on this computer ahead of time, so it opens at once and with no
//! connection: after each list sync, the views of the last 30 days' threads,
//! made by the open's own [`read_view`], and their attachments under 5 MB.
//!
//! It runs on a thread of its own, so the list sync never waits for it, and it
//! holds the store only to read and write, never while Google answers, so a
//! person's open never waits for it either. It's the person's own mail for the
//! person's own screens: nothing here is the model's to read.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use gws_store::prefetch::{Ahead, Saved};
use serde_json::Value;

use crate::Core;
use crate::accounts::Bearer;
use crate::gmail::{self, Failure};
use crate::mail::{now_ms, read_view, row_of, signed_in, with_own_store, with_store};

/// How far back mail is kept ahead.
pub const WINDOW_MS: i64 = 30 * 24 * 60 * 60 * 1000;
/// Threads read in one round; the rest wait for the next.
pub const BATCH: usize = 20;
/// Attachments this size and over are fetched at the press.
pub const ATTACHMENT_MAX: i64 = 5 * 1024 * 1024;
/// As long as a part id or an attachment id may be, as the open checks them.
const PART_MAX: usize = 64;
const ID_MAX: usize = 4096;

/// Whether a round is on its way.
#[derive(Default)]
pub struct Running(AtomicBool);

/// Lets the next round start, however this one ended.
struct Done<'a>(&'a AtomicBool);

impl Drop for Done<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Starts a round on its own thread, unless one is still going.
pub fn kick(core: &Arc<Core>) {
    if core.prefetch.0.swap(true, Ordering::AcqRel) {
        return;
    }
    let core = core.clone();
    std::thread::spawn(move || {
        let _done = Done(&core.prefetch.0);
        let _ = run(&core);
    });
}

/// One round: the views due, then the attachments of what's kept. It stops at
/// the first failure, a busy answer from Google among them, and the rest wait.
pub fn run(core: &Core) -> Result<(), Failure> {
    let (auth, account) = signed_in(core)?;
    let since = core.clock.now_ms() - WINDOW_MS;
    bodies(core, &auth, &account, since)?;
    attachments(core, &auth, &account, since)
}

/// Views of recent threads that have none, newest first.
pub fn bodies(core: &Core, auth: &Bearer, account: &str, since: i64) -> Result<(), Failure> {
    let due = with_store(core, |store| store.unviewed_since(account, since, BATCH))?;
    for id in due {
        if core.sync.stopping() {
            break;
        }
        // made as an open makes it, so what's kept is what an open would show
        let Some((full, view)) = read_view(core, auth, &id)? else {
            continue;
        };
        // what was read is what the row says, or the next sync has news of it
        let Some(row) = row_of(&full) else {
            continue;
        };
        let view = view.to_string();
        let kept = with_own_store(core, account, |store| {
            store.put_view_ahead(account, &id, (row.at, row.messages), &view, now_ms(), since)
        })?;
        if kept == Ahead::Full {
            break;
        }
    }
    Ok(())
}

/// The attachments under [`ATTACHMENT_MAX`] of kept recent views, inline
/// pictures among them.
pub fn attachments(core: &Core, auth: &Bearer, account: &str, since: i64) -> Result<(), Failure> {
    let due = with_store(core, |store| {
        store.views_unfetched_since(account, since, BATCH)
    })?;
    for (thread, view) in due {
        if core.sync.stopping() {
            break;
        }
        let view: Value = serde_json::from_str(&view).unwrap_or_default();
        let listed = view.get("attachments").and_then(Value::as_array);
        for a in listed.into_iter().flatten() {
            let text = |key: &str| a.get(key).and_then(Value::as_str).unwrap_or_default();
            let (message, part, id) = (text("message"), text("part"), text("id"));
            let size = a.get("size").and_then(Value::as_i64).unwrap_or(-1);
            if !gmail::id_ok(message) || !part_ok(part) || !id_ok(id) {
                continue;
            }
            if !(0..ATTACHMENT_MAX).contains(&size)
                || with_store(core, |store| store.has_attachment(account, message, part))?
            {
                continue;
            }
            let Some(bytes) = core.gmail.attachment(auth, message, id)? else {
                continue;
            };
            // the size was Google's word; what counts is what came
            if bytes.len() as i64 >= ATTACHMENT_MAX {
                continue;
            }
            let saved = Saved {
                thread: thread.clone(),
                message: message.to_owned(),
                part: part.to_owned(),
                name: text("name").to_owned(),
                mime: text("mime").to_owned(),
                bytes,
            };
            with_own_store(core, account, |store| {
                store.put_attachment(account, &saved, now_ms())
            })?;
        }
        with_own_store(core, account, |store| store.mark_fetched(account, &thread))?;
    }
    Ok(())
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
