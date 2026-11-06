//! Drafts and sending. A draft is built here, into the message Gmail keeps, and
//! Send sends the very message the person read: a draft that changed since
//! has a new revision, and refuses until it's read again.

use std::collections::HashSet;
use std::sync::MutexGuard;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::{Value, json};

use crate::accounts::Bearer;
use crate::gmail::{self, Failure};
use crate::mail::Subject;
use crate::tools::{MAIL_DRAFT_SAVE, fail, ok};
use crate::{Core, js};

/// The most recipients one message takes.
const RECIPIENTS_MAX: usize = 100;
/// The longest body a draft holds, before attachments.
const BODY_MAX: usize = 1024 * 1024;

/// The most attachments one message carries: Gmail's own limit.
pub const ATTACHMENTS_MAX: usize = 25 * 1024 * 1024;
/// What waits in memory, across every message being written.
const UPLOADS_MAX: usize = 2 * ATTACHMENTS_MAX;
/// How many finished saves wait for a screen to ask.
const JOBS_MAX: usize = 16;

/// A file in a message.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Part {
    pub name: String,
    pub mime: String,
    pub bytes: Vec<u8>,
}

/// An attachment a saved draft already has, which a new save must carry over:
/// Gmail replaces the whole message.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Kept {
    message: String,
    id: String,
    name: String,
    mime: String,
}

/// Files the person chose, as they arrive, by upload id.
#[derive(Default)]
pub struct Uploads {
    files: std::collections::HashMap<String, Upload>,
    next: u64,
}

struct Upload {
    part: Part,
    /// The draft it went into, so sending or deleting that frees it.
    draft: Option<String>,
}

impl Uploads {
    pub fn held_bytes(&self) -> usize {
        self.files.values().map(|u| u.part.bytes.len()).sum()
    }

    fn start(&mut self, name: String, mime: String) -> String {
        self.next += 1;
        let id = format!("up{}", self.next);
        let part = Part {
            name,
            mime,
            bytes: Vec::new(),
        };
        self.files.insert(id.clone(), Upload { part, draft: None });
        id
    }

    fn free_draft(&mut self, draft: &str) {
        self.files.retain(|_, u| u.draft.as_deref() != Some(draft));
    }
}

fn uploads(core: &Core) -> MutexGuard<'_, Uploads> {
    core.uploads.lock().unwrap_or_else(|e| e.into_inner())
}

/// A file's name as a header can hold it: no path, and nothing that ends the line.
fn file_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let clean: String = base.chars().filter(|c| !c.is_control()).collect();
    if clean.trim().is_empty() {
        "attachment".to_owned()
    } else {
        js::slice_utf16(clean.trim(), 200)
    }
}

/// `type/subtype`, or what any file is.
fn mime_type(kind: &str) -> String {
    let token = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"+-._".contains(&b))
    };
    match kind.split_once('/') {
        Some((a, b)) if token(a) && token(b) => kind.to_owned(),
        _ => "application/octet-stream".to_owned(),
    }
}

/// `gws_upload_chunk`: part of a file. `at` says where it goes, and the answer
/// where to go on from, so a chunk whose answer was lost isn't kept twice.
pub fn upload_chunk(core: &Core, args: &Value) -> Result<Value, Failure> {
    let Ok(data) =
        base64::engine::general_purpose::STANDARD.decode(js::string_or_empty(args.get("data")))
    else {
        return Ok(fail("invalid_args", "That isn't a file's bytes."));
    };
    let at = args.get("at").and_then(Value::as_u64).unwrap_or(0) as usize;
    let mut held = uploads(core);
    let total = held.held_bytes();
    let mut id = js::string_or_empty(args.get("upload"));
    if id.is_empty() {
        let name = file_name(&js::string_or_empty(args.get("name")));
        id = held.start(name, mime_type(&js::string_or_empty(args.get("type"))));
    }
    let Some(upload) = held.files.get_mut(&id) else {
        return Ok(fail(
            "not_found",
            "That upload is gone. Attach the file again.",
        ));
    };
    let size = upload.part.bytes.len();
    if at == size {
        if size + data.len() > ATTACHMENTS_MAX {
            held.files.remove(&id);
            return Ok(fail("too_big", "That file is over Gmail's 25 MB limit."));
        }
        if total + data.len() > UPLOADS_MAX {
            held.files.remove(&id);
            return Ok(fail(
                "too_big",
                "That's too much to attach at once. Send or discard a message first.",
            ));
        }
        upload.part.bytes.extend_from_slice(&data);
    }
    let size = held.files.get(&id).map_or(0, |u| u.part.bytes.len());
    Ok(ok(json!({"upload": id, "size": size})))
}

/// `gws_upload_drop`: a file taken off before it was sent.
pub fn upload_drop(core: &Core, args: &Value) -> Result<Value, Failure> {
    uploads(core)
        .files
        .remove(&js::string_or_empty(args.get("upload")));
    Ok(ok(json!({"dropped": true})))
}

/// A slow save's outcome, for a caller that stopped waiting: the Bot gives a
/// call 30 seconds, and a big attachment on a slow line takes longer.
pub fn finish_job(core: &Core, args: &Value, answer: &Value) {
    let job = js::string_or_empty(args.get("job"));
    if !gmail::id_ok(&job) {
        return;
    }
    let mut jobs = core.jobs.lock().unwrap_or_else(|e| e.into_inner());
    if jobs.len() == JOBS_MAX {
        jobs.pop_front();
    }
    jobs.push_back((job, answer.clone()));
}

/// `gws_mail_job`: how a save came out, or that it's still going.
pub fn job(core: &Core, args: &Value) -> Result<Value, Failure> {
    let job = js::string_or_empty(args.get("job"));
    let mut jobs = core.jobs.lock().unwrap_or_else(|e| e.into_inner());
    Ok(match jobs.iter().position(|(id, _)| *id == job) {
        Some(at) => {
            let (_, answer) = jobs.remove(at).unwrap_or_default();
            ok(json!({"done": true, "answer": answer}))
        }
        None => ok(json!({"done": false})),
    })
}

/// A message being written.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Draft {
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub body: String,
    /// The conversation a reply belongs to.
    pub thread: Option<String>,
    pub in_reply_to: String,
    pub references: String,
}

fn kept(args: &Value) -> Vec<Kept> {
    let text = |item: &Value, k: &str| js::string_or_empty(item.get(k));
    args.get("keep")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|item| Kept {
            message: text(item, "message"),
            id: text(item, "id"),
            name: file_name(&text(item, "name")),
            mime: mime_type(&text(item, "mime")),
        })
        .collect()
}

fn upload_ids(args: &Value) -> Vec<String> {
    args.get("attachments")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|i| i.as_str().map(str::to_owned))
        .collect()
}

/// Every file a save carries: what was uploaded, then what the draft already
/// had. `Err` is the person's answer: a message must not lose a file quietly.
fn gather(core: &Core, auth: &Bearer, args: &Value) -> Result<Result<Vec<Part>, Value>, Failure> {
    let mut parts = Vec::new();
    {
        let held = uploads(core);
        for id in upload_ids(args) {
            match held.files.get(&id) {
                Some(upload) => parts.push(upload.part.clone()),
                None => {
                    return Ok(Err(fail(
                        "not_found",
                        "An attachment is gone. Attach it again.",
                    )));
                }
            }
        }
    }
    for keep in kept(args) {
        if !gmail::id_ok(&keep.message) || keep.id.is_empty() {
            return Ok(Err(fail("invalid_args", "That isn't an attachment.")));
        }
        let Some(bytes) = core.gmail.attachment(auth, &keep.message, &keep.id)? else {
            return Ok(Err(fail(
                "not_found",
                "An attachment is gone. Attach it again.",
            )));
        };
        parts.push(Part {
            name: keep.name,
            mime: keep.mime,
            bytes,
        });
    }
    if parts.iter().map(|p| p.bytes.len()).sum::<usize>() > ATTACHMENTS_MAX {
        return Ok(Err(fail(
            "too_big",
            "That's over Gmail's 25 MB limit for one message.",
        )));
    }
    Ok(Ok(parts))
}

/// An address alone, as `a@b.c` or `Name <a@b.c>`, if it's one a header can hold.
fn address(text: &str) -> Option<String> {
    let text = text.trim();
    let bare = text
        .rsplit_once('<')
        .and_then(|(_, rest)| rest.strip_suffix('>'))
        .unwrap_or(text);
    let (name, host) = bare.split_once('@')?;
    let clean = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| !c.is_control() && !c.is_whitespace() && !"<>,;\"".contains(c))
    };
    (clean(name) && clean(host) && host.contains('.')).then(|| bare.to_owned())
}

fn addresses(v: Option<&Value>) -> Result<Vec<String>, String> {
    let list: Vec<String> = match v {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|i| i.as_str().map(str::to_owned))
            .collect(),
        Some(Value::String(s)) => s.split(',').map(str::to_owned).collect(),
        _ => Vec::new(),
    };
    list.iter()
        .filter(|a| !a.trim().is_empty())
        .map(|a| address(a).ok_or_else(|| format!("{} isn't an address.", js::slice_utf16(a, 80))))
        .collect()
}

/// A line of a header: nothing in it may start another.
fn one_line(text: &str) -> String {
    text.replace(['\r', '\n'], " ")
}

/// A header value in words any mail system reads.
fn header_text(text: &str) -> String {
    let text = one_line(text);
    if text.is_ascii() {
        return text;
    }
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    format!("=?UTF-8?B?{encoded}?=")
}

impl Draft {
    pub fn of(args: &Value) -> Result<Draft, String> {
        let text = |k: &str| js::string_or_empty(args.get(k));
        let draft = Draft {
            to: addresses(args.get("to"))?,
            cc: addresses(args.get("cc"))?,
            bcc: addresses(args.get("bcc"))?,
            subject: text("subject"),
            body: text("body"),
            thread: Some(text("thread")).filter(|t| gmail::id_ok(t)),
            in_reply_to: one_line(&text("in_reply_to")),
            references: one_line(&text("references")),
        };
        if draft.recipients().count() > RECIPIENTS_MAX {
            return Err("That's too many recipients for one message.".to_owned());
        }
        if draft.body.len() > BODY_MAX {
            return Err("That message is too long.".to_owned());
        }
        Ok(draft)
    }

    pub fn recipients(&self) -> impl Iterator<Item = &String> {
        self.to.iter().chain(&self.cc).chain(&self.bcc)
    }

    /// The message, as Gmail's JSON door takes it.
    pub fn raw(&self, message_id: &str) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(self.message(message_id, &[]))
    }

    /// The message itself. It names no From: Gmail fills that from the sign-in,
    /// so a draft can't claim to be from anyone else.
    pub fn message(&self, message_id: &str, parts: &[Part]) -> Vec<u8> {
        let mut head = String::new();
        let mut line = |name: &str, value: &str| {
            if !value.is_empty() {
                head.push_str(&format!("{name}: {value}\r\n"));
            }
        };
        line("To", &self.to.join(", "));
        line("Cc", &self.cc.join(", "));
        line("Bcc", &self.bcc.join(", "));
        line("Subject", &header_text(&self.subject));
        line("In-Reply-To", &self.in_reply_to);
        line("References", &self.references);
        line("Message-ID", message_id);
        head.push_str("MIME-Version: 1.0\r\n");
        let text = format!(
            "Content-Type: text/plain; charset=UTF-8\r\nContent-Transfer-Encoding: base64\r\n\r\n{}\r\n",
            wrapped(self.body.as_bytes())
        );
        if parts.is_empty() {
            return format!("{head}{text}").into_bytes();
        }
        let boundary = "buwiz-gws-part-boundary";
        let mut out = format!(
            "{head}Content-Type: multipart/mixed; boundary=\"{boundary}\"\r\n\r\n--{boundary}\r\n{text}"
        );
        for part in parts {
            out.push_str(&format!(
                "--{boundary}\r\nContent-Type: {}\r\nContent-Disposition: attachment; {}\r\nContent-Transfer-Encoding: base64\r\n\r\n{}\r\n",
                part.mime,
                disposition_name(&part.name),
                wrapped(&part.bytes),
            ));
        }
        out.push_str(&format!("--{boundary}--\r\n"));
        out.into_bytes()
    }
}

/// Base64 in the lines mail takes. Nothing in it can look like a boundary, as
/// base64 has no `-`.
fn wrapped(bytes: &[u8]) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    let lines: Vec<&str> = encoded
        .as_bytes()
        .chunks(76)
        .map(|c| std::str::from_utf8(c).unwrap_or_default())
        .collect();
    lines.join("\r\n")
}

/// `filename=`, with anything past plain letters percent-encoded (RFC 2231).
fn disposition_name(name: &str) -> String {
    let plain = |b: u8| b.is_ascii_alphanumeric() || b"-._".contains(&b);
    let encoded: String = name
        .bytes()
        .map(|b| {
            if plain(b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    format!("filename*=UTF-8''{encoded}")
}

/// The message id a draft goes out under, made here so a Send that lost its
/// answer can look in Sent for it.
fn new_message_id() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("<{now:x}.{:x}@buwiz.local>", std::process::id())
}

fn session(core: &Core) -> Result<Bearer, Failure> {
    core.accounts
        .session()
        .map(|(auth, _)| auth)
        .ok_or(Failure::AccountRequired)
}

fn model_drafts(core: &Core) -> MutexGuard<'_, HashSet<String>> {
    core.model_drafts.lock().unwrap_or_else(|e| e.into_inner())
}

/// `gws_mail_draft_save` from the model: a new draft, or one it wrote and the
/// person hasn't touched. Every other draft is the person's work.
pub fn save_for_model(core: &Core, args: &Value) -> Value {
    if !upload_ids(args).is_empty() || !kept(args).is_empty() {
        return fail("not_allowed", "Only a person attaches files.");
    }
    let held = js::string_or_empty(args.get("draft"));
    if !held.is_empty() && !model_drafts(core).contains(&held) {
        return fail(
            "not_allowed",
            "That draft is the person's. Write a new one instead.",
        );
    }
    let result = core.run(MAIL_DRAFT_SAVE, args);
    if let Some(id) = result
        .pointer("/structuredContent/draft")
        .and_then(Value::as_str)
    {
        model_drafts(core).insert(id.to_owned());
    }
    result
}

/// `gws_mail_draft_save`: writes a draft, or replaces one. Sends nothing.
pub fn save(core: &Core, args: &Value) -> Result<Value, Failure> {
    let auth = session(core)?;
    let draft = match Draft::of(args) {
        Ok(draft) => draft,
        Err(why) => return Ok(fail("invalid_args", &why)),
    };
    let held = js::string_or_empty(args.get("draft"));
    if !held.is_empty() && !gmail::id_ok(&held) {
        return Ok(fail("invalid_args", "That isn't a draft."));
    }
    let message_id = Some(js::string_or_empty(args.get("message_id")))
        .filter(|m| m.starts_with('<') && m.ends_with('>') && !m.contains(['\r', '\n']))
        .unwrap_or_else(new_message_id);
    let parts = match gather(core, &auth, args)? {
        Ok(parts) => parts,
        Err(refused) => return Ok(refused),
    };
    let thread = draft.thread.as_deref();
    let saved = if !parts.is_empty() {
        let replaces = Some(held.as_str()).filter(|h| !h.is_empty());
        let message = draft.message(&message_id, &parts);
        core.gmail.draft_upload(&auth, replaces, &message, thread)?
    } else if held.is_empty() {
        core.gmail
            .draft_create(&auth, &draft.raw(&message_id), thread)?
    } else {
        core.gmail
            .draft_update(&auth, &held, &draft.raw(&message_id), thread)?
    };
    let Some(saved) = saved else {
        return Ok(fail("not_found", "That draft is gone."));
    };
    // a person's edit makes it theirs: `save_for_model` claims it back only for itself
    model_drafts(core).remove(&held);
    let id = saved
        .pointer("/id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut files = uploads(core);
    for upload in upload_ids(args) {
        if let Some(file) = files.files.get_mut(&upload) {
            file.draft = Some(id.to_owned());
        }
    }
    drop(files);
    Ok(ok(json!({
        "draft": saved.pointer("/id"),
        "revision": saved.pointer("/message/id"),
        "message_id": message_id,
    })))
}

/// The most recipients looked up in Sent: past that, all count as new.
const LOOKED_UP_MAX: usize = 20;

/// Who in `to`, `cc` and `bcc` the person has never written to.
fn new_to(core: &Core, auth: &Bearer, lists: &[String]) -> Result<Vec<String>, Failure> {
    let all: Vec<String> = lists
        .iter()
        .flat_map(|l| l.split(','))
        .filter_map(address)
        .collect();
    let mut new = Vec::new();
    for (at, who) in all.iter().enumerate() {
        let known = at < LOOKED_UP_MAX
            && !core
                .gmail
                .search(auth, &format!("in:sent to:{who}"), 1)?
                .is_empty();
        if !known {
            new.push(who.clone());
        }
    }
    Ok(new)
}

/// `gws_mail_draft_read`: a draft as it stands, with the revision Send needs. By
/// a card's ticket it's a review: it also says who it's from and who's new.
pub fn read(core: &Core, args: &Value) -> Result<Value, Failure> {
    let (auth, me) = core.accounts.session().ok_or(Failure::AccountRequired)?;
    let ticket = js::string_or_empty(args.get("ticket"));
    let review = !ticket.is_empty();
    let id = if review {
        let tickets = core.tickets.lock().unwrap_or_else(|e| e.into_inner());
        match tickets.subject(&ticket) {
            Some(Subject::Draft(id)) => id.clone(),
            _ => return Ok(fail("not_found", "This card is out of date.")),
        }
    } else {
        js::string_or_empty(args.get("draft"))
    };
    if !gmail::id_ok(&id) {
        return Ok(fail("invalid_args", "That isn't a draft."));
    }
    let Some(held) = core.gmail.draft(&auth, &id)? else {
        return Ok(fail("not_found", "That draft is gone."));
    };
    let message = held.get("message").cloned().unwrap_or(Value::Null);
    let h = |name| gmail::short_header(&message, name);
    let new = if review {
        new_to(core, &auth, &[h("To"), h("Cc"), h("Bcc")])?
    } else {
        Vec::new()
    };
    Ok(ok(json!({
        "draft": id,
        "from": me,
        "new": new,
        "revision": message.get("id"),
        "message_id": h("Message-ID"),
        "to": h("To"),
        "cc": h("Cc"),
        "bcc": h("Bcc"),
        "subject": h("Subject"),
        "body": gmail::body(&message),
        "thread": message.get("threadId"),
        "attachments": gmail::attachments(&message),
    })))
}

/// `gws_mail_draft_delete`: the person's, and it can't be undone.
pub fn delete(core: &Core, args: &Value) -> Result<Value, Failure> {
    let auth = session(core)?;
    let id = js::string_or_empty(args.get("draft"));
    if !gmail::id_ok(&id) {
        return Ok(fail("invalid_args", "That isn't a draft."));
    }
    core.gmail.draft_delete(&auth, &id)?;
    uploads(core).free_draft(&id);
    Ok(ok(json!({"deleted": true})))
}

/// Who may be written to, when a test against a real account says: nobody but
/// this address, checked before a request leaves.
fn only_to() -> Option<String> {
    if cfg!(feature = "stub") {
        std::env::var("GWS_ONLY_TO").ok().filter(|a| !a.is_empty())
    } else {
        None
    }
}

fn outside<'a>(allowed: &str, recipients: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    let mut recipients = recipients;
    recipients.find(|r| !r.eq_ignore_ascii_case(allowed))
}

/// How long a press allows its send: long enough to save a big attachment.
const ARMED_FOR: Duration = Duration::from_secs(900);

/// Presses of Send, by what each was given. The Bot judges a call by the moment
/// it's made, not by the press behind it: a send held five seconds could go out
/// as the person's after the AI pressed, if the person pressed anything in the
/// gap. So the call that decides comes at the press, and the send needs its answer.
#[derive(Default)]
pub struct Armed {
    presses: Vec<(String, Instant)>,
    next: u64,
}

impl Armed {
    fn take(&mut self, token: &str) -> bool {
        self.presses.retain(|(_, at)| at.elapsed() < ARMED_FOR);
        match self.presses.iter().position(|(t, _)| t == token) {
            Some(at) => {
                self.presses.remove(at);
                true
            }
            None => false,
        }
    }
}

fn armed(core: &Core) -> MutexGuard<'_, Armed> {
    core.armed.lock().unwrap_or_else(|e| e.into_inner())
}

/// `gws_mail_send_arm`: the person pressed Send. The model's call never gets
/// here: this tool is the app's alone, and a press of the AI's goes out as the AI.
pub fn arm(core: &Core) -> Result<Value, Failure> {
    let mut held = armed(core);
    held.next += 1;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let token = format!("a{}-{nanos:x}", held.next);
    held.presses.push((token.clone(), Instant::now()));
    Ok(ok(json!({"armed": token})))
}

/// `gws_mail_draft_of`: the draft in a conversation, for the Drafts list, whose
/// rows are conversations.
pub fn draft_of(core: &Core, args: &Value) -> Result<Value, Failure> {
    let auth = session(core)?;
    let thread = js::string_or_empty(args.get("thread"));
    if !gmail::id_ok(&thread) {
        return Ok(fail("invalid_args", "That isn't a conversation."));
    }
    Ok(ok(
        json!({"draft": core.gmail.draft_in_thread(&auth, &thread)?}),
    ))
}

/// How many people a suggestion list shows.
const SUGGESTIONS_MAX: usize = 8;

/// `gws_people_suggest`: who the person has heard from, kept here and there for
/// any sign-in, then their Google contacts if the sign-in covers those.
pub fn suggest(core: &Core, args: &Value) -> Result<Value, Failure> {
    let query = js::string_or_empty(args.get("query"));
    let query = query.trim();
    let Some((auth, me)) = core.accounts.session() else {
        return Err(Failure::AccountRequired);
    };
    if query.is_empty() {
        return Ok(ok(json!({"people": []})));
    }
    let take = SUGGESTIONS_MAX as i64;
    let mut found =
        crate::mail::with_store(core, |s| s.senders(&me, query, take)).unwrap_or_default();
    // no contacts is no reason to lose what's here: a sign-in from before this
    // asked for them, or being offline, answers with the rest
    if !core.warm.swap(true, Ordering::Relaxed) {
        let _ = core.gmail.contacts(&auth, "");
    }
    found.extend(core.gmail.contacts(&auth, query).unwrap_or_default());
    let mut seen = HashSet::new();
    let people: Vec<Value> = found
        .into_iter()
        .filter(|(_, address)| {
            !address.eq_ignore_ascii_case(&me) && seen.insert(address.to_lowercase())
        })
        .take(SUGGESTIONS_MAX)
        .map(|(name, address)| json!({"name": name, "address": address}))
        .collect();
    Ok(ok(json!({"people": people})))
}

/// `gws_mail_send`: the person's press, on the revision they read. A Send whose
/// answer was lost is safe to press again: the draft has gone by then, and the
/// message is found in Sent instead of going out twice.
pub fn send(core: &Core, args: &Value) -> Result<Value, Failure> {
    let auth = session(core)?;
    let id = js::string_or_empty(args.get("draft"));
    let revision = js::string_or_empty(args.get("revision"));
    let message_id = js::string_or_empty(args.get("message_id"));
    if !gmail::id_ok(&id) || revision.is_empty() {
        return Ok(fail("invalid_args", "That isn't a draft."));
    }
    let Some(held) = core.gmail.draft(&auth, &id)? else {
        let sent = !message_id.is_empty()
            && !core
                .gmail
                .search(&auth, &format!("rfc822msgid:{message_id}"), 1)?
                .is_empty();
        return Ok(if sent {
            ok(json!({"sent": true, "already": true}))
        } else {
            fail("not_found", "That draft is gone.")
        });
    };
    let message = held.get("message").cloned().unwrap_or(Value::Null);
    if message.get("id").and_then(Value::as_str) != Some(revision.as_str()) {
        return Ok(fail(
            "changed",
            "This draft changed after you read it. Read it again before sending.",
        ));
    }
    if let Some(allowed) = only_to() {
        let all = ["To", "Cc", "Bcc"].map(|h| gmail::header(&message, h));
        let names = all.iter().flat_map(|h| h.split(',')).map(str::trim);
        if let Some(other) = outside(&allowed, names.filter(|n| !n.is_empty())) {
            return Ok(fail(
                "not_allowed",
                &format!("This test run writes only to {allowed}, not {other}."),
            ));
        }
    }
    if !armed(core).take(&js::string_or_empty(args.get("armed"))) {
        return Ok(fail("not_allowed", "Only a person's press sends mail."));
    }
    core.gmail.draft_send(&auth, &id)?;
    uploads(core).free_draft(&id);
    Ok(ok(json!({"sent": true})))
}

/// `gws_mail_reply_start`: a reply, reply-all or forward, filled in from the
/// conversation, for the person to go on from. It saves nothing.
pub fn reply_start(core: &Core, args: &Value) -> Result<Value, Failure> {
    let auth = session(core)?;
    let id = js::string_or_empty(args.get("thread"));
    let mode = js::string_or_empty(args.get("mode"));
    if !gmail::id_ok(&id) {
        return Ok(fail("invalid_args", "That isn't a thread."));
    }
    let Some(thread) = core.gmail.thread_full(&auth, &id)? else {
        return Ok(fail("not_found", "That conversation is gone."));
    };
    let list = thread.get("messages").and_then(Value::as_array);
    let Some(last) = list.and_then(|l| l.last()) else {
        return Ok(fail("not_found", "That conversation is gone."));
    };
    let h = |name| gmail::short_header(last, name);
    let me = core
        .accounts
        .signed_in()
        .map(|(who, _)| who)
        .unwrap_or_default();
    let others = |list: &str| {
        list.split(',')
            .filter_map(address)
            .filter(|a| !a.eq_ignore_ascii_case(&me))
            .collect::<Vec<_>>()
    };
    let subject = h("Subject");
    let quoted: String = gmail::body(last)
        .lines()
        .map(|l| format!("> {l}\n"))
        .collect();
    let said = format!("\n\nOn {}, {} wrote:\n{quoted}", h("Date"), h("From"));
    let answer = match mode.as_str() {
        "forward" => json!({
            "to": [], "cc": [],
            "subject": prefixed("Fwd: ", &subject),
            "body": format!("\n\n---------- Forwarded message ----------\nFrom: {}\nDate: {}\nSubject: {subject}\nTo: {}\n\n{}", h("From"), h("Date"), h("To"), gmail::body(last)),
        }),
        _ => {
            let from = h("Reply-To");
            let from = if from.is_empty() { h("From") } else { from };
            let mut to = others(&from);
            // the last word was the person's own: it goes on to whoever that went to
            if to.is_empty() {
                to = others(&h("To"));
            }
            let mut cc = Vec::new();
            if mode == "all" {
                to.extend(others(&h("To")));
                cc = others(&h("Cc"));
            }
            let message_id = h("Message-ID");
            let references = format!("{} {message_id}", h("References"));
            json!({
                "to": to, "cc": cc,
                "subject": prefixed("Re: ", &subject),
                "body": said,
                "thread": id,
                "in_reply_to": message_id,
                "references": references.trim(),
            })
        }
    };
    Ok(ok(answer))
}

fn prefixed(prefix: &str, subject: &str) -> String {
    if subject.to_lowercase().starts_with(&prefix.to_lowercase()) {
        subject.to_owned()
    } else {
        format!("{prefix}{subject}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decoded(raw: &str) -> String {
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(raw)
            .unwrap();
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn a_header_cannot_be_smuggled_in() {
        let draft = Draft::of(&json!({
            "to": "ana@example.com",
            "subject": "hi\r\nBcc: spy@example.com",
            "in_reply_to": "<a>\r\nBcc: spy@example.com",
        }))
        .unwrap();
        let message = decoded(&draft.raw("<id@x>"));
        let head = message.split("\r\n\r\n").next().unwrap();
        assert!(!head.lines().any(|l| l.starts_with("Bcc:")), "{head}");
        for bad in [
            "a\r\nBcc: spy@example.com",
            "not an address",
            "a@b",
            "<a@b.c",
            "a b@c.d",
            "\"a\"@b.c",
        ] {
            assert!(Draft::of(&json!({"to": [bad]})).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_message_names_no_sender_and_says_what_was_written() {
        let draft = Draft::of(&json!({
            "to": ["Ana <ana@example.com>", "ben@example.com"],
            "cc": "cara@example.com",
            "subject": "Lunch ñ",
            "body": "Hello é",
        }))
        .unwrap();
        assert_eq!(draft.to, ["ana@example.com", "ben@example.com"]);
        let message = decoded(&draft.raw("<id@x>"));
        assert!(!message.contains("\nFrom:") && !message.starts_with("From:"));
        assert!(message.contains("To: ana@example.com, ben@example.com\r\n"));
        assert!(message.contains("Cc: cara@example.com\r\n"));
        assert!(message.contains("Subject: =?UTF-8?B?"));
        let body = message
            .split("\r\n\r\n")
            .nth(1)
            .unwrap()
            .replace("\r\n", "");
        let sent = base64::engine::general_purpose::STANDARD
            .decode(body)
            .unwrap();
        assert_eq!(String::from_utf8(sent).unwrap(), "Hello é");
    }

    #[test]
    fn too_many_or_too_long_is_refused() {
        let many: Vec<String> = (0..101).map(|n| format!("a{n}@example.com")).collect();
        assert!(Draft::of(&json!({"to": many})).is_err());
        assert!(Draft::of(&json!({"body": "x".repeat(BODY_MAX + 1)})).is_err());
    }

    #[test]
    fn the_test_guard_finds_anyone_else() {
        let allowed = "support@buwiz.com";
        assert_eq!(outside(allowed, ["SUPPORT@buwiz.com"].into_iter()), None);
        assert_eq!(
            outside(allowed, ["support@buwiz.com", "x@example.com"].into_iter()),
            Some("x@example.com")
        );
    }

    #[test]
    fn a_subject_is_prefixed_once() {
        assert_eq!(prefixed("Re: ", "Lunch"), "Re: Lunch");
        assert_eq!(prefixed("Re: ", "RE: Lunch"), "RE: Lunch");
    }
}
