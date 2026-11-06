//! Drafts and sending. A draft is built here, into the message Gmail keeps, and
//! Send sends the very message the person read: a draft that changed since
//! has a new revision, and refuses until it's read again.

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::{Value, json};

use crate::accounts::Bearer;
use crate::gmail::{self, Failure};
use crate::mail::Subject;
use crate::tools::{fail, failed, ok};
use crate::{Core, js};

/// The most recipients one message takes.
const RECIPIENTS_MAX: usize = 100;
/// The longest body a draft holds, before attachments.
const BODY_MAX: usize = 1024 * 1024;

/// The most attachments one message carries: Gmail's own limit.
pub const ATTACHMENTS_MAX: usize = 25 * 1024 * 1024;
/// The whole message as it goes up, in its base64: the most Google's page gives
/// for this upload. Whether Gmail then sends a file near the 25 MB line is not
/// proven, only that it arrives whole at the stub; a refusal shows at Send.
const MESSAGE_MAX: usize = 36_000_000;
/// What waits in memory, across every message being written.
const UPLOADS_MAX: usize = 2 * ATTACHMENTS_MAX;
/// How many finished saves wait for a screen to ask.
const JOBS_MAX: usize = 16;

/// A refusal the screen must act on, not only show. The Bot hands a screen a
/// failed tool's words and no code, so this comes as an answer.
fn refusal(code: &str, why: &str) -> Value {
    ok(json!({"refused": code, "message": why}))
}

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

    /// A whole file for Drive, which takes it from here: the press spends it,
    /// so a second press can't send it twice and nothing stays in memory.
    pub(crate) fn take(&mut self, id: &str) -> Option<Part> {
        self.files.remove(id).map(|u| u.part)
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

/// Saves a screen may ask after: the Bot gives a call 30 seconds, and a big
/// attachment on a slow line takes longer. Only this module makes a job's id,
/// so nothing a model passes can plant an answer or hold a screen waiting.
#[derive(Default)]
pub struct Jobs {
    running: VecDeque<String>,
    done: VecDeque<(String, Value)>,
    next: u64,
}

fn jobs(core: &Core) -> MutexGuard<'_, Jobs> {
    core.jobs.lock().unwrap_or_else(|e| e.into_inner())
}

/// What can't be guessed from what came before.
fn nonce() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos())
}

/// `gws_mail_job_start`: an id for the save that's about to be asked for.
pub fn job_start(core: &Core) -> Result<Value, Failure> {
    let mut held = jobs(core);
    // one that's running is someone waiting: the newcomer waits its turn instead
    if held.running.len() == JOBS_MAX {
        return Ok(fail(
            "rate_limited",
            "Too many saves are under way. Try again in a moment.",
        ));
    }
    held.next += 1;
    let job = format!("job{}x{:x}", held.next, nonce());
    held.running.push_back(job.clone());
    Ok(ok(json!({"job": job})))
}

/// Keeps a save's outcome, if it was one a job was started for.
pub fn finish_job(core: &Core, args: &Value, answer: &Value) {
    let job = js::string_or_empty(args.get("job"));
    let mut held = jobs(core);
    let Some(at) = held.running.iter().position(|j| *j == job) else {
        return;
    };
    held.running.remove(at);
    if held.done.len() == JOBS_MAX {
        held.done.pop_front();
    }
    held.done.push_back((job, answer.clone()));
}

/// `gws_mail_job`: how a save came out, that it's still going, or that this
/// process knows nothing of it.
pub fn job(core: &Core, args: &Value) -> Result<Value, Failure> {
    let job = js::string_or_empty(args.get("job"));
    let mut held = jobs(core);
    if let Some(at) = held.done.iter().position(|(id, _)| *id == job)
        && let Some((_, answer)) = held.done.remove(at)
    {
        return Ok(ok(json!({"done": true, "answer": answer})));
    }
    let running = held.running.contains(&job);
    Ok(ok(json!({"done": false, "gone": !running})))
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

/// What a save carries: every file, and the uploads made of files the draft had.
#[derive(Default)]
struct Gathered {
    parts: Vec<Part>,
    /// `(the attachment's id, the upload that holds it now)`.
    kept: Vec<(String, String)>,
}

/// Every file a save carries: what was uploaded, then what the draft already
/// had. Those are held as uploads from here on, as saving gives the draft a new
/// message and leaves their ids behind. `Err` is the person's answer: a message
/// must not lose a file quietly.
fn gather(core: &Core, auth: &Bearer, args: &Value) -> Result<Result<Gathered, Value>, Failure> {
    let gone = || fail("attachment_gone", "An attachment is gone. Attach it again.");
    let mut got = Gathered::default();
    {
        let held = uploads(core);
        for id in upload_ids(args) {
            match held.files.get(&id) {
                Some(upload) => got.parts.push(upload.part.clone()),
                None => return Ok(Err(gone())),
            }
        }
    }
    for keep in kept(args) {
        if !gmail::id_ok(&keep.message) || keep.id.is_empty() {
            drop_kept(core, &got);
            return Ok(Err(fail("invalid_args", "That isn't an attachment.")));
        }
        let bytes = core.gmail.attachment(auth, &keep.message, &keep.id);
        if !matches!(bytes, Ok(Some(_))) {
            drop_kept(core, &got);
        }
        let Some(bytes) = bytes? else {
            return Ok(Err(gone()));
        };
        let part = Part {
            name: keep.name,
            mime: keep.mime,
            bytes,
        };
        let mut held = uploads(core);
        if held.held_bytes() + part.bytes.len() > UPLOADS_MAX {
            drop(held);
            drop_kept(core, &got);
            return Ok(Err(fail(
                "too_big",
                "That's too much to attach at once. Send or discard a message first.",
            )));
        }
        let upload = held.start(part.name.clone(), part.mime.clone());
        if let Some(file) = held.files.get_mut(&upload) {
            file.part.bytes.clone_from(&part.bytes);
        }
        got.kept.push((keep.id, upload));
        got.parts.push(part);
    }
    if got.parts.iter().map(|p| p.bytes.len()).sum::<usize>() > ATTACHMENTS_MAX {
        drop_kept(core, &got);
        return Ok(Err(fail(
            "too_big",
            "That's over Gmail's 25 MB limit for one message.",
        )));
    }
    Ok(Ok(got))
}

fn drop_kept(core: &Core, got: &Gathered) {
    let mut held = uploads(core);
    for (_, upload) in &got.kept {
        held.files.remove(upload);
    }
}

/// An address alone, from `a@b.c` or `Name <a@b.c>`, if it's a plain one: what
/// passes here is safe in a header and between quotes in a Gmail query.
fn address(text: &str) -> Option<String> {
    let text = text.trim();
    let bare = match text.rsplit_once('<') {
        Some((_, rest)) => rest.strip_suffix('>')?,
        None => text,
    };
    let (local, host) = bare.split_once('@')?;
    let local_ok = !local.is_empty()
        && local.len() <= 64
        && !local.starts_with('.')
        && !local.ends_with('.')
        && !local.contains("..")
        && local
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".!#$%&'*+-/=?^_~".contains(&b));
    let label_ok = |l: &str| {
        !l.is_empty()
            && !l.starts_with('-')
            && !l.ends_with('-')
            && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    };
    let host_ok = host.len() <= 253 && host.contains('.') && host.split('.').all(label_ok);
    (local_ok && host_ok).then(|| bare.to_owned())
}

/// A `Message-ID` plain enough to go in a Gmail query as it is.
fn message_id_ok(id: &str) -> bool {
    id.strip_prefix('<')
        .and_then(|i| i.strip_suffix('>'))
        .is_some_and(|i| address(i).is_some())
}

/// A header's list, split where a comma really separates two: a name in quotes
/// or an address in brackets can hold one.
pub(crate) fn split_list(list: &str) -> Vec<&str> {
    let (mut parts, mut from) = (Vec::new(), 0);
    let (mut quoted, mut bracket, mut escaped) = (false, false, false);
    for (at, c) in list.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            '<' if !quoted => bracket = true,
            '>' if !quoted => bracket = false,
            ',' if !quoted && !bracket => {
                parts.push(&list[from..at]);
                from = at + 1;
            }
            _ => {}
        }
    }
    parts.push(&list[from..]);
    parts.retain(|p| !p.trim().is_empty());
    parts
}

/// Everyone in a header, whole: an address where it's one, else as written, so
/// that a person sees it and a save refuses it rather than it going missing.
fn listed(list: &str) -> Vec<String> {
    split_list(list)
        .iter()
        .map(|p| address(p).unwrap_or_else(|| p.trim().to_owned()))
        .collect()
}

fn addresses(v: Option<&Value>) -> Result<Vec<String>, String> {
    let list: Vec<String> = match v {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|i| i.as_str().map(str::to_owned))
            .collect(),
        Some(Value::String(s)) => split_list(s).iter().map(|p| (*p).to_owned()).collect(),
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

/// What makes one draft's writes and its send take turns: a send's look at the
/// draft and the send itself must have nothing land between them.
fn gate(core: &Core, draft: &str) -> Arc<Mutex<()>> {
    let mut gates = core.gates.lock().unwrap_or_else(|e| e.into_inner());
    gates.retain(|_, gate| Arc::strong_count(gate) > 1);
    gates.entry(draft.to_owned()).or_default().clone()
}

/// The turn, if nobody has it now.
fn try_turn(gate: &Mutex<()>) -> Option<MutexGuard<'_, ()>> {
    match gate.try_lock() {
        Ok(turn) => Some(turn),
        Err(TryLockError::Poisoned(e)) => Some(e.into_inner()),
        Err(TryLockError::WouldBlock) => None,
    }
}

/// `gws_mail_draft_save` from the model: a new draft, or one it wrote and the
/// person hasn't touched. Every other draft is the person's work, and so is one
/// they've pressed Send on: their yes is to what they saw.
pub fn save_for_model(core: &Core, args: &Value) -> Value {
    if !upload_ids(args).is_empty() || !kept(args).is_empty() {
        return fail("not_allowed", "Only a person attaches files.");
    }
    let held = js::string_or_empty(args.get("draft"));
    if held.is_empty() {
        return save_as_model(core, args);
    }
    let in_hand = || {
        fail(
            "not_allowed",
            "The person has that draft in hand. Leave it as it is.",
        )
    };
    // the turn is held to the end of the PUT: a send can't start under it
    let gate = gate(core, &held);
    let Some(_turn) = try_turn(&gate) else {
        return in_hand();
    };
    if armed(core).holds(&held) {
        return in_hand();
    }
    if !model_drafts(core).contains(&held) {
        return fail(
            "not_allowed",
            "That draft is the person's. Write a new one instead.",
        );
    }
    save_as_model(core, args)
}

fn save_as_model(core: &Core, args: &Value) -> Value {
    // what ties a save to a screen and a press is the app's alone
    let mut args = args.clone();
    if let Some(args) = args.as_object_mut() {
        for key in ["job", "armed", "message_id"] {
            args.remove(key);
        }
    }
    core.sync.seen();
    let result = write_draft(core, &args).unwrap_or_else(failed);
    if let Some(code) = result
        .pointer("/structuredContent/refused")
        .and_then(Value::as_str)
    {
        let why = result.pointer("/structuredContent/message");
        return fail(
            code,
            why.and_then(Value::as_str).unwrap_or("That didn't work."),
        );
    }
    if let Some(id) = result
        .pointer("/structuredContent/draft")
        .and_then(Value::as_str)
    {
        model_drafts(core).insert(id.to_owned());
    }
    result
}

/// `gws_mail_draft_save`: writes a draft, or replaces one. Sends nothing. It
/// waits its turn: a person's save behind a send finds what became of it.
pub fn save(core: &Core, args: &Value) -> Result<Value, Failure> {
    let held = js::string_or_empty(args.get("draft"));
    if held.is_empty() {
        return write_draft(core, args);
    }
    let gate = gate(core, &held);
    let _turn = gate.lock().unwrap_or_else(|e| e.into_inner());
    write_draft(core, args)
}

fn write_draft(core: &Core, args: &Value) -> Result<Value, Failure> {
    let auth = session(core)?;
    let draft = match Draft::of(args) {
        Ok(draft) => draft,
        Err(why) => return Ok(fail("invalid_args", &why)),
    };
    let held = js::string_or_empty(args.get("draft"));
    if !held.is_empty() && !gmail::id_ok(&held) {
        return Ok(fail("invalid_args", "That isn't a draft."));
    }
    let named = js::string_or_empty(args.get("message_id"));
    let message_id = Some(named.clone())
        .filter(|m| message_id_ok(m))
        .unwrap_or_else(new_message_id);
    let got = match gather(core, &auth, args)? {
        Ok(got) => got,
        Err(refused) => return Ok(refused),
    };
    let thread = draft.thread.as_deref();
    let saved = if got.parts.is_empty() {
        if held.is_empty() {
            core.gmail
                .draft_create(&auth, &draft.raw(&message_id), thread)
        } else {
            core.gmail
                .draft_update(&auth, &held, &draft.raw(&message_id), thread)
        }
    } else {
        let message = draft.message(&message_id, &got.parts);
        if message.len() > MESSAGE_MAX {
            drop_kept(core, &got);
            return Ok(fail(
                "too_big",
                "That's over what Gmail takes for one message, once it's encoded.",
            ));
        }
        let replaces = Some(held.as_str()).filter(|h| !h.is_empty());
        core.gmail.draft_upload(&auth, replaces, &message, thread)
    };
    if !matches!(saved, Ok(Some(_))) {
        drop_kept(core, &got);
    }
    let Some(saved) = saved? else {
        // a draft that's gone may be one that went: writing it anew would send it twice
        if in_sent(core, &auth, &named)? {
            return Ok(refusal(
                "already_sent",
                "This message has already gone. It's in Sent.",
            ));
        }
        return Ok(refusal("not_found", "That draft is gone."));
    };
    // a person's edit makes it theirs: `save_for_model` claims it back only for itself
    model_drafts(core).remove(&held);
    let text = |path: &str| saved.pointer(path).and_then(Value::as_str);
    let id = text("/id").unwrap_or_default();
    let mut files = uploads(core);
    let kept_ids = got.kept.iter().map(|(_, upload)| upload.clone());
    for upload in upload_ids(args).into_iter().chain(kept_ids) {
        if let Some(file) = files.files.get_mut(&upload) {
            file.draft = Some(id.to_owned());
        }
    }
    drop(files);
    let armed_by = js::string_or_empty(args.get("armed"));
    if let Some(revision) = text("/message/id") {
        armed(core).bind(&armed_by, id, revision);
    }
    let kept: Vec<Value> = got
        .kept
        .iter()
        .map(|(attachment, upload)| json!({"id": attachment, "upload": upload}))
        .collect();
    Ok(ok(json!({
        "draft": saved.pointer("/id"),
        "revision": saved.pointer("/message/id"),
        "message_id": message_id,
        "kept": kept,
    })))
}

/// Whether the message with this `Message-ID` is in Sent.
fn in_sent(core: &Core, auth: &Bearer, message_id: &str) -> Result<bool, Failure> {
    if !message_id_ok(message_id) {
        return Ok(false);
    }
    let found = core
        .gmail
        .search(auth, &format!("rfc822msgid:{message_id}"), 1)?;
    Ok(!found.is_empty())
}

/// The most recipients looked up in Sent: past that, all count as new.
const LOOKED_UP_MAX: usize = 20;

/// How many sent mails are read for who they went to.
const SENT_READ_MAX: usize = 50;

/// Who of `people` the person has never written to, in one search. A person
/// past the first few, or crowded out of the mails read, counts as new: the
/// mistake that costs a second look, not one that hides a stranger.
fn new_to(core: &Core, auth: &Bearer, people: &[String]) -> Result<Vec<String>, Failure> {
    let wanted: Vec<String> = people
        .iter()
        .take(LOOKED_UP_MAX)
        .filter_map(|p| address(p))
        .map(|p| format!("to:\"{p}\""))
        .collect();
    if wanted.is_empty() {
        return Ok(people.to_vec());
    }
    let query = format!("in:sent {{{}}}", wanted.join(" "));
    let ids = core.gmail.search(auth, &query, SENT_READ_MAX)?;
    let mails = core.gmail.metadata_with(auth, &ids, &["To", "Cc", "Bcc"])?;
    let known: HashSet<String> = mails
        .iter()
        .flatten()
        .flat_map(|m| ["To", "Cc", "Bcc"].map(|h| gmail::header(m, h)))
        .flat_map(|h| listed(&h))
        .map(|a| a.to_lowercase())
        .collect();
    Ok(people
        .iter()
        .filter(|p| !known.contains(&p.to_lowercase()))
        .cloned()
        .collect())
}

/// `gws_mail_draft_read`: a draft as it stands, with the revision Send needs. By
/// a card's ticket it's a review: it also says who it's from and who's new.
pub fn read(core: &Core, args: &Value) -> Result<Value, Failure> {
    let (auth, me) = core.accounts.session().ok_or(Failure::AccountRequired)?;
    let ticket = js::string_or_empty(args.get("ticket"));
    let review = !ticket.is_empty() || args.get("review") == Some(&Value::Bool(true));
    let id = if !ticket.is_empty() {
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
    // recipients are read whole: `short_header` is for showing, and would cut a
    // list, or cut an address into a different one
    let people = |name| listed(&gmail::header(&message, name));
    let (to, cc, bcc) = (people("To"), people("Cc"), people("Bcc"));
    let new = if review {
        let all: Vec<String> = to.iter().chain(&cc).chain(&bcc).cloned().collect();
        new_to(core, &auth, &all)?
    } else {
        Vec::new()
    };
    Ok(ok(json!({
        "draft": id,
        "from": me,
        "new": new,
        "revision": message.get("id"),
        "message_id": h("Message-ID"),
        "to": to,
        "cc": cc,
        "bcc": bcc,
        "subject": gmail::header(&message, "Subject"),
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
/// this address, checked before a request leaves. It's in every build, as that
/// test needs a real one, and all it can do is narrow who mail goes to.
pub(crate) fn only_to() -> Option<String> {
    std::env::var("GWS_ONLY_TO").ok().filter(|a| !a.is_empty())
}

pub(crate) fn outside<'a>(
    allowed: &str,
    recipients: impl Iterator<Item = &'a str>,
) -> Option<&'a str> {
    let mut recipients = recipients;
    recipients.find(|r| !r.eq_ignore_ascii_case(allowed))
}

/// How long a press has to reach its draft: long enough to save a big attachment.
const ARMED_FOR: Duration = Duration::from_secs(900);

/// How long it stands once it knows its draft, which is the hold and the send.
/// A screen that closes in the hold can't say so: this is what frees the draft
/// for the AI, and the account for a switch.
const BOUND_FOR: Duration = Duration::from_secs(60);

/// Presses of Send, by what each was given. The Bot judges a call by the moment
/// it's made, not by the press behind it: a send held five seconds could go out
/// as the person's after the AI pressed, if the person pressed anything in the
/// gap. So the call that decides comes at the press, and the send needs its answer.
#[derive(Default)]
pub struct Armed {
    presses: Vec<Press>,
    next: u64,
}

struct Press {
    token: String,
    until: Instant,
    /// The draft and revision it sends: known at once on a card, and after the
    /// save in the panel. Nothing else goes out on it.
    sends: Option<(String, String)>,
}

impl Armed {
    fn live(&mut self) {
        let now = Instant::now();
        self.presses.retain(|p| now < p.until);
    }

    /// The press now sends this. A newer press on a draft replaces any older one.
    fn bind(&mut self, token: &str, draft: &str, revision: &str) {
        if !self.presses.iter().any(|p| p.token == token) {
            return;
        }
        let older =
            |p: &Press| p.token != token && p.sends.as_ref().is_some_and(|sends| sends.0 == draft);
        self.presses.retain(|p| !older(p));
        if let Some(press) = self.presses.iter_mut().find(|p| p.token == token) {
            press.sends = Some((draft.to_owned(), revision.to_owned()));
            press.until = Instant::now() + BOUND_FOR;
        }
    }

    /// Whether a press still in force names this draft.
    fn holds(&mut self, draft: &str) -> bool {
        self.live();
        let names = |sends: &(String, String)| sends.0 == draft;
        self.presses
            .iter()
            .any(|p| p.sends.as_ref().is_some_and(names))
    }

    /// Whether any Send is pressed and not yet gone or taken back.
    fn any(&mut self) -> bool {
        self.live();
        !self.presses.is_empty()
    }

    /// Spends the press, whatever comes of it: what it sends, if it's still good.
    fn take(&mut self, token: &str) -> Option<(String, String)> {
        self.live();
        let at = self.presses.iter().position(|p| p.token == token)?;
        self.presses.remove(at).sends
    }
}

fn armed(core: &Core) -> MutexGuard<'_, Armed> {
    core.armed.lock().unwrap_or_else(|e| e.into_inner())
}

/// Whether a Send is in its hold.
pub(crate) fn in_hold(core: &Core) -> bool {
    armed(core).any()
}

/// `gws_mail_send_arm`: the person pressed Send. The model's call never gets
/// here: this tool is the app's alone, and a press of the AI's goes out as the AI.
pub fn arm(core: &Core, args: &Value) -> Result<Value, Failure> {
    let mut held = armed(core);
    held.next += 1;
    let token = format!("a{}x{:x}", held.next, nonce());
    held.presses.push(Press {
        token: token.clone(),
        until: Instant::now() + ARMED_FOR,
        sends: None,
    });
    let text = |k: &str| args.get(k).and_then(Value::as_str);
    if let Some((draft, revision)) = text("draft").zip(text("revision")) {
        held.bind(&token, draft, revision);
    }
    Ok(ok(json!({"armed": token})))
}

/// `gws_mail_send_disarm`: Undo. The press is spent, so the draft is free again.
pub fn disarm(core: &Core, args: &Value) -> Result<Value, Failure> {
    armed(core).take(&js::string_or_empty(args.get("armed")));
    Ok(ok(json!({"disarmed": true})))
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
    found.extend(contacts_of(core, &auth, query));
    Ok(ok(people_json(found, &me)))
}

/// `gws_cal_people`: suggest's contacts alone. Calendar's approval grants no
/// read of Gmail, so the senders of stored mail stay out of it.
pub fn contacts(core: &Core, args: &Value) -> Result<Value, Failure> {
    let query = js::string_or_empty(args.get("query"));
    let query = query.trim();
    let Some((auth, me)) = core.accounts.session() else {
        return Err(Failure::AccountRequired);
    };
    if query.is_empty() {
        return Ok(ok(json!({"people": []})));
    }
    Ok(ok(people_json(contacts_of(core, &auth, query), &me)))
}

/// No contacts is no reason to lose the rest: a sign-in from before this asked
/// for them, or being offline, answers with nothing here.
fn contacts_of(core: &Core, auth: &Bearer, query: &str) -> Vec<(String, String)> {
    if !core.warm.swap(true, Ordering::Relaxed) {
        let _ = core.gmail.contacts(auth, "");
    }
    core.gmail.contacts(auth, query).unwrap_or_default()
}

fn people_json(found: Vec<(String, String)>, me: &str) -> Value {
    let mut seen = HashSet::new();
    let people: Vec<Value> = found
        .into_iter()
        .filter(|(_, address)| {
            !address.eq_ignore_ascii_case(me) && seen.insert(address.to_lowercase())
        })
        .take(SUGGESTIONS_MAX)
        .map(|(name, address)| json!({"name": name, "address": address}))
        .collect();
    json!({"people": people})
}

/// `gws_mail_send`: the person's press, on the revision they read. A Send whose
/// answer was lost is safe to press again: the draft has gone by then, and the
/// message is found in Sent instead of going out twice.
pub fn send(core: &Core, args: &Value) -> Result<Value, Failure> {
    let token = js::string_or_empty(args.get("armed"));
    let id = js::string_or_empty(args.get("draft"));
    let revision = js::string_or_empty(args.get("revision"));
    let message_id = js::string_or_empty(args.get("message_id"));
    if !gmail::id_ok(&id) || revision.is_empty() {
        armed(core).take(&token);
        return Ok(fail("invalid_args", "That isn't a draft."));
    }
    // held to the end, so what's read here is what goes. A save under way is about
    // to change it, and a first try still going may yet send it: neither is a
    // refusal that says the mail stayed
    let gate = gate(core, &id);
    let Some(_turn) = try_turn(&gate) else {
        armed(core).take(&token);
        return Ok(refusal(
            "busy",
            "This draft is being changed or sent. Read it again before sending.",
        ));
    };
    // spent here, whatever comes of it: a refused send needs a new press, and
    // one left standing would close the draft and every switch of account
    let press = armed(core).take(&token);
    let auth = session(core)?;
    let Some(held) = core.gmail.draft(&auth, &id)? else {
        return Ok(if in_sent(core, &auth, &message_id)? {
            ok(json!({"sent": true, "already": true}))
        } else {
            refusal("not_found", "That draft is gone.")
        });
    };
    if press != Some((id.clone(), revision.clone())) {
        return Ok(refusal("not_allowed", "Only a person's press sends mail."));
    }
    let message = held.get("message").cloned().unwrap_or(Value::Null);
    if message.get("id").and_then(Value::as_str) != Some(revision.as_str()) {
        return Ok(refusal(
            "changed",
            "This draft changed after you read it. Read it again before sending.",
        ));
    }
    if let Some(allowed) = only_to() {
        let all = ["To", "Cc", "Bcc"].map(|h| gmail::header(&message, h));
        let names: Vec<String> = all.iter().flat_map(|h| listed(h)).collect();
        if let Some(other) = outside(&allowed, names.iter().map(String::as_str)) {
            return Ok(refusal(
                "not_allowed",
                &format!("This test run writes only to {allowed}, not {other}."),
            ));
        }
    }
    if !core.gmail.draft_send(&auth, &id)? {
        return Ok(refusal("not_found", "That draft is gone."));
    }
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
    let others = |name: &str| {
        let mut people = listed(&gmail::header(last, name));
        people.retain(|a| !a.eq_ignore_ascii_case(&me));
        people
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
            "source": last.get("id"),
            "attachments": gmail::attachments(last),
            "subject": prefixed("Fwd: ", &subject),
            "body": format!("\n\n---------- Forwarded message ----------\nFrom: {}\nDate: {}\nSubject: {subject}\nTo: {}\n\n{}", h("From"), h("Date"), h("To"), gmail::body(last)),
        }),
        _ => {
            let reply_to = others("Reply-To");
            let note = reply_note(&listed(&gmail::header(last, "From")), &reply_to);
            let mut to = if reply_to.is_empty() {
                others("From")
            } else {
                reply_to
            };
            // the last word was the person's own: it goes on to whoever that went to
            if to.is_empty() {
                to = others("To");
            }
            let mut cc = Vec::new();
            if mode == "all" {
                to.extend(others("To"));
                cc = others("Cc");
                // a name on both lists, or twice on one, is written to once
                let mut seen = HashSet::new();
                to.retain(|a| seen.insert(a.to_lowercase()));
                cc.retain(|a| seen.insert(a.to_lowercase()));
            }
            let message_id = h("Message-ID");
            let references = format!("{} {message_id}", h("References"));
            json!({
                "to": to, "cc": cc, "note": note,
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

/// What to say when a reply won't go to the sender: anyone can set Reply-To.
fn reply_note(from: &[String], reply_to: &[String]) -> String {
    let same = |a: &String| from.iter().any(|f| f.eq_ignore_ascii_case(a));
    if reply_to.iter().all(same) {
        return String::new();
    }
    let (goes_to, sender) = (reply_to.join(", "), from.join(", "));
    format!(
        "Replies go to {}, not the sender, {}.",
        js::slice_utf16(&goes_to, 200),
        js::slice_utf16(&sender, 200)
    )
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

    #[test]
    fn a_press_that_knows_its_draft_stands_only_for_the_hold_and_the_send() {
        let mut held = Armed::default();
        let press = |token: &str| Press {
            token: token.to_owned(),
            until: Instant::now() + ARMED_FOR,
            sends: None,
        };
        held.presses.push(press("saving"));
        held.presses.push(press("bound"));
        held.bind("bound", "d", "r");
        let left = |p: &Press| p.until.saturating_duration_since(Instant::now());
        assert!(
            left(&held.presses[0]) > BOUND_FOR,
            "a big save needs its time"
        );
        assert!(left(&held.presses[1]) <= BOUND_FOR);
        held.presses[1].until = Instant::now();
        assert!(
            !held.holds("d"),
            "a screen that closed can't lock the draft"
        );
        assert!(held.any(), "the one still saving stands");
    }

    #[test]
    fn a_save_with_no_press_behind_it_leaves_the_presses_alone() {
        let mut held = Armed::default();
        held.presses.push(Press {
            token: "a".to_owned(),
            until: Instant::now() + BOUND_FOR,
            sends: Some(("d".to_owned(), "r".to_owned())),
        });
        held.bind("", "d", "r2");
        assert!(held.holds("d"));
    }

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
            "a(b)@c.d",
            "a:b@c.d",
            "a@b@c.d",
            "a@b.c OR from:x",
            "a@-b.c",
            ".a@b.c",
            "a..b@c.d",
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
    fn a_list_splits_only_where_a_comma_separates_two() {
        assert_eq!(
            listed("\"Doe, Jane\" <jane@x.co>, ben@x.co,, \"a\\\", b\" <c@x.co>"),
            ["jane@x.co", "ben@x.co", "c@x.co"]
        );
        assert_eq!(listed("not one, ben@x.co"), ["not one", "ben@x.co"]);
    }

    #[test]
    fn a_reply_to_someone_other_than_the_sender_is_said() {
        let of = |a: &str| vec![a.to_owned()];
        assert_eq!(reply_note(&of("a@x.co"), &[]), "");
        assert_eq!(reply_note(&of("a@x.co"), &of("A@x.co")), "");
        assert_eq!(
            reply_note(&of("a@x.co"), &of("eve@y.co")),
            "Replies go to eve@y.co, not the sender, a@x.co."
        );
    }

    #[test]
    fn only_a_plain_message_id_reaches_a_query() {
        assert!(message_id_ok("<a1.b2@buwiz.local>"));
        for bad in ["a@b.c", "<a b@c.d>", "<a@b.c> OR in:sent", "<a\"@b.c>"] {
            assert!(!message_id_ok(bad), "{bad}");
        }
    }

    #[test]
    fn a_subject_is_prefixed_once() {
        assert_eq!(prefixed("Re: ", "Lunch"), "Re: Lunch");
        assert_eq!(prefixed("Re: ", "RE: Lunch"), "RE: Lunch");
    }
}
