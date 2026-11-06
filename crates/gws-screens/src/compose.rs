//! Writing mail, in the reader's place. The message lives in one [`Letter`], so
//! what's held during Send's five seconds can come back whole on Undo.
//!
//! Every action here runs on this screen's own queue, for the reason Mail's do:
//! a press's task dies with the view it was pressed on.
//!
//! Send's press makes its first call at once, before any wait: the Bot judges a
//! call by the moment it's made, so that's the call that can tell a person's
//! press from the AI's. The plugin won't send without its answer.

use std::time::Duration;

use base64::Engine as _;
use buwiz_app::{Bridge, BuwizError, PickOptions};
use nsh_core::AccessibilityValue;
use nsh_core::element::input;
use nsh_core::prelude::*;
use serde_json::{Value, json};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::shell::{Job, Latest, MUTE, RETRIES, backoff, sleep};

const INK: &str = "text-[#1c1c1c] dark:text-[#f0f0f0]";
const LINE: &str = "border-black/10 dark:border-[#2b2b2b]";
const RAIL: &str = "bg-[#efefec] dark:bg-[#0f0f0f]";
const FIELD: &str = "border-b px-3 py-1.5 text-[13px]";

/// How long Send can still be taken back.
const HOLD: Duration = Duration::from_secs(5);
/// How often the hold looks to see if it was undone.
const TICK: Duration = Duration::from_millis(100);
/// How long typing rests before a draft saves.
const AUTOSAVE: Duration = Duration::from_secs(2);
/// A file's bytes per call: under 256 KiB of arguments once in base64.
const CHUNK: usize = 180 * 1024;
const ATTACHMENTS_MAX: u64 = 25 * 1024 * 1024;
/// How long typing rests before asking who it could be.
const SUGGEST_AFTER: Duration = Duration::from_millis(200);

/// A file in the message: uploaded here, or already in the saved draft.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct File {
    key: u64,
    upload: Option<String>,
    keep: Option<Value>,
    name: String,
    size: u64,
    /// How much has arrived, while it's on its way.
    arrived: u64,
}

impl File {
    fn ready(&self) -> bool {
        self.keep.is_some() || (self.upload.is_some() && self.arrived >= self.size)
    }
}

/// The message as written.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct Letter {
    to: String,
    cc: String,
    bcc: String,
    subject: String,
    body: String,
    draft: Option<String>,
    revision: Option<String>,
    message_id: Option<String>,
    thread: Option<String>,
    in_reply_to: String,
    references: String,
    files: Vec<File>,
}

impl Letter {
    fn blank(&self) -> bool {
        self.draft.is_none()
            && self.files.is_empty()
            && [&self.to, &self.cc, &self.bcc, &self.subject]
                .iter()
                .all(|f| f.trim().is_empty())
            && self.body.trim().is_empty()
    }

    fn has_recipient(&self) -> bool {
        [&self.to, &self.cc, &self.bcc]
            .iter()
            .any(|f| !f.trim().is_empty())
    }

    /// What `gws_mail_draft_save` takes.
    fn args(&self) -> Value {
        let mut args = json!({
            "to": self.to, "cc": self.cc, "bcc": self.bcc,
            "subject": self.subject, "body": self.body,
            "in_reply_to": self.in_reply_to, "references": self.references,
            "attachments": self.files.iter().filter_map(|f| f.upload.clone()).collect::<Vec<_>>(),
            "keep": self.files.iter().filter_map(|f| f.keep.clone()).collect::<Vec<_>>(),
        });
        for (key, value) in [
            ("draft", &self.draft),
            ("thread", &self.thread),
            ("message_id", &self.message_id),
        ] {
            if let Some(value) = value {
                args[key] = value.clone().into();
            }
        }
        args
    }

    /// A draft or a reply as the plugin answered it.
    fn of(v: &Value) -> Letter {
        let text = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default();
        let list = |k: &str| match v.get(k) {
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", "),
            _ => text(k).to_owned(),
        };
        let some = |k: &str| Some(text(k).to_owned()).filter(|t| !t.is_empty());
        let revision = some("revision");
        let files = v
            .get("attachments")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
            .map(|(key, a)| {
                let name = a.get("name").and_then(Value::as_str).unwrap_or_default();
                let size = a.get("size").and_then(Value::as_u64).unwrap_or(0);
                File {
                    key: key as u64,
                    keep: Some(json!({"message": revision, "id": a.get("id"), "name": name, "mime": a.get("mime")})),
                    name: name.to_owned(),
                    size,
                    arrived: size,
                    upload: None,
                }
            })
            .collect();
        Letter {
            to: list("to"),
            cc: list("cc"),
            bcc: list("bcc"),
            subject: text("subject").to_owned(),
            body: text("body").to_owned(),
            draft: some("draft"),
            revision,
            message_id: some("message_id"),
            thread: some("thread"),
            in_reply_to: text("in_reply_to").to_owned(),
            references: text("references").to_owned(),
            files,
        }
    }
}

/// Where a Send that left the panel has got to.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Stage {
    /// It can still be undone.
    Waiting,
    Sending,
    Sent,
}

#[derive(Clone, Copy)]
pub struct Compose {
    pub open: Signal<bool>,
    letter: Signal<Letter>,
    /// Cc and Bcc are showing.
    extra: Signal<bool>,
    /// Counts edits, and which of them the saved draft holds.
    edits: Signal<u64>,
    saved: Signal<u64>,
    saving: Signal<bool>,
    /// What went wrong, for the person.
    said: Signal<Option<String>>,
    /// Deleting the draft waits on a second press.
    asking: Signal<bool>,
    stage: Signal<Option<Stage>>,
    /// The letter a Send took, until it's gone or undone.
    held: Signal<Option<Letter>>,
    hold: Latest,
    jobs: Signal<UnboundedSender<Job>>,
    next: Signal<u64>,
    /// `(name, address)` suggested for what's being typed, and where it goes.
    people: Signal<Vec<(String, String)>>,
    naming: Signal<Option<fn(&mut Letter) -> &mut String>>,
    asked: Latest,
}

impl Compose {
    /// The screen, and what its task must drain: see [`work`].
    pub fn new() -> (Compose, UnboundedReceiver<Job>) {
        let (jobs, waiting) = unbounded_channel::<Job>();
        let c = Compose {
            open: signal(false),
            letter: signal(Letter::default()),
            extra: signal(false),
            edits: signal(0),
            saved: signal(0),
            saving: signal(false),
            said: signal(None),
            asking: signal(false),
            stage: signal(None),
            held: signal(None),
            hold: Latest::new(),
            jobs: signal_with(jobs, SignalEquals::Never),
            next: signal(0),
            people: signal(Vec::new()),
            naming: signal(None),
            asked: Latest::new(),
        };
        (c, waiting)
    }

    fn run(self, job: impl Future<Output = ()> + 'static) {
        self.jobs
            .with_untracked(|jobs| drop(jobs.send(Box::pin(job))));
    }

    fn fresh(self) -> u64 {
        let n = self.next.get_untracked() + 1;
        self.next.set(n);
        n
    }

    fn dirty(self) -> bool {
        self.edits.get_untracked() != self.saved.get_untracked()
    }

    fn edit(self, change: impl FnOnce(&mut Letter)) {
        self.letter.update(change);
        self.edits.set(self.edits.get_untracked() + 1);
    }

    fn fill(self, letter: Letter) {
        let extra = !letter.cc.is_empty() || !letter.bcc.is_empty();
        self.letter.set(letter);
        self.saved.set(self.edits.get_untracked());
        self.extra.set(extra);
        self.said.set(None);
        self.asking.set(false);
    }

    fn close(self) {
        self.fill(Letter::default());
        self.open.set(false);
    }
}

/// Runs what's queued, for as long as the screen lives. When it goes, so does a
/// Send still in its hold: nothing goes out.
pub async fn work(mut waiting: UnboundedReceiver<Job>) {
    while let Some(job) = waiting.recv().await {
        job.await;
    }
}

/// Saves a draft when typing rests. A file makes every save a whole upload, so
/// a message with one saves on leaving and on Send.
pub async fn autosave(b: Bridge, c: Compose) {
    loop {
        sleep(AUTOSAVE).await;
        let idle = c.open.get_untracked() && c.dirty() && !c.saving.get_untracked();
        if idle && c.letter.with_untracked(|l| !l.blank() && l.files.is_empty()) {
            c.saving.set(true);
            let b = b.clone();
            c.run(async move {
                // an address half typed isn't one yet: leaving says so, not this
                let _ = save(&b, c).await;
                c.saving.set(false);
            });
        }
    }
}

/// A tool's answer as a screen gets it, from a result that came by way of a job.
fn answered(result: &Value) -> Result<Value, String> {
    if result.get("isError") == Some(&Value::Bool(true)) {
        let why = result.pointer("/content/0/text").and_then(Value::as_str);
        return Err(why.unwrap_or("That didn't work.").to_owned());
    }
    Ok(result
        .get("structuredContent")
        .cloned()
        .unwrap_or(Value::Null))
}

/// How a save too slow for its call came out: it goes on in the plugin.
async fn wait_for(b: &Bridge, job: &str) -> Result<Value, String> {
    loop {
        sleep(AUTOSAVE).await;
        let v = b
            .call("gws_mail_job", json!({"job": job}))
            .await
            .map_err(|e| e.message)?;
        if v.get("done") == Some(&Value::Bool(true)) {
            return answered(v.get("answer").unwrap_or(&Value::Null));
        }
    }
}

async fn write(b: &Bridge, c: Compose, letter: &Letter) -> Result<Value, BuwizError> {
    let job = format!("job{}", c.fresh());
    let mut args = letter.args();
    args["job"] = job.clone().into();
    match b.call("gws_mail_draft_save", args).await {
        Err(e) if e.code == "timeout" => wait_for(b, &job)
            .await
            .map_err(|why| BuwizError::new("failed", false, why)),
        answer => answer,
    }
}

/// Writes the letter to its draft. One deleted elsewhere is written anew.
async fn save(b: &Bridge, c: Compose) -> Result<(), String> {
    let seen = c.edits.get_untracked();
    let mut letter = c.letter.get_untracked();
    let mut answer = write(b, c, &letter).await;
    if matches!(&answer, Err(e) if e.code == "not_found") && letter.draft.is_some() {
        letter.draft = None;
        answer = write(b, c, &letter).await;
    }
    let v = answer.map_err(|e| e.message)?;
    let text = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
    c.letter.update(|l| {
        l.draft = text("draft");
        l.revision = text("revision");
        l.message_id = text("message_id");
    });
    c.saved.set(seen);
    Ok(())
}

/// What's open is saved before something else takes its place. `false` leaves
/// it open, with the reason showing.
async fn put_away(b: &Bridge, c: Compose) -> bool {
    if !c.open.get_untracked() || c.letter.with_untracked(Letter::blank) || !c.dirty() {
        return true;
    }
    match save(b, c).await {
        Ok(()) => true,
        Err(why) => {
            c.said.set(Some(why));
            false
        }
    }
}

/// A new message.
pub fn start(b: Bridge, c: Compose) {
    c.run(async move {
        if put_away(&b, c).await {
            c.fill(Letter::default());
            c.open.set(true);
        }
    });
}

/// A reply, reply-all or forward of `thread`.
pub fn reply(b: Bridge, c: Compose, thread: String, mode: &'static str) {
    c.run(async move {
        if !put_away(&b, c).await {
            return;
        }
        let args = json!({"thread": thread, "mode": mode});
        match b.call("gws_mail_reply_start", args).await {
            Ok(v) => {
                c.fill(Letter::of(&v));
                c.open.set(true);
            }
            Err(e) => {
                c.fill(Letter::default());
                c.said.set(Some(e.message));
                c.open.set(true);
            }
        }
    });
}

/// The draft in `thread`, to go on with, if it still has one.
pub fn resume(b: Bridge, c: Compose, thread: String) {
    c.run(async move {
        if !put_away(&b, c).await {
            return;
        }
        let found = b.call("gws_mail_draft_of", json!({"thread": thread})).await;
        let draft = found.map(|v| v.get("draft").cloned().unwrap_or(Value::Null));
        let read = match draft {
            Ok(Value::Null) => return,
            Ok(draft) => b.call("gws_mail_draft_read", json!({"draft": draft})).await,
            Err(e) => Err(e),
        };
        match read {
            Ok(v) => {
                c.fill(Letter::of(&v));
                c.open.set(true);
            }
            Err(e) => {
                c.said.set(Some(e.message));
                c.open.set(true);
            }
        }
    });
}

/// Leaves the message for later: saved, unless there's nothing to keep.
pub fn keep(b: Bridge, c: Compose) {
    c.run(async move {
        if put_away(&b, c).await {
            c.close();
        }
    });
}

fn discard(b: Bridge, c: Compose) {
    c.run(async move {
        let letter = c.letter.get_untracked();
        if let Some(draft) = &letter.draft {
            let args = json!({"draft": draft});
            if let Err(e) = b.call("gws_mail_draft_delete", args).await {
                c.said.set(Some(e.message));
                return;
            }
        }
        for file in &letter.files {
            if let Some(upload) = &file.upload {
                let _ = b.call("gws_upload_drop", json!({"upload": upload})).await;
            }
        }
        c.close();
    });
}

/// One chunk, tried again while the answer is only busy: `at` keeps a chunk
/// from being kept twice, so trying again is safe here.
async fn chunk(b: &Bridge, args: Value) -> Result<Value, BuwizError> {
    let mut attempt = 0;
    loop {
        match b.call("gws_upload_chunk", args.clone()).await {
            Err(e) if e.retryable && attempt < RETRIES => {
                sleep(backoff(attempt)).await;
                attempt += 1;
            }
            answer => return answer,
        }
    }
}

async fn upload(b: &Bridge, c: Compose, key: u64, name: &str, mime: &str, bytes: &[u8]) -> bool {
    let mut at = 0usize;
    let mut id = Value::Null;
    while at < bytes.len() || id.is_null() {
        let end = (at + CHUNK).min(bytes.len());
        let data = base64::engine::general_purpose::STANDARD.encode(&bytes[at..end]);
        let mut args = json!({"name": name, "type": mime, "at": at, "data": data});
        if !id.is_null() {
            args["upload"] = id.clone();
        }
        let v = match chunk(b, args).await {
            Ok(v) => v,
            Err(e) => {
                c.said.set(Some(e.message));
                return false;
            }
        };
        id = v.get("upload").cloned().unwrap_or(Value::Null);
        at = v.get("size").and_then(Value::as_u64).unwrap_or(0) as usize;
        let gone = !c.letter.with_untracked(|l| l.files.iter().any(|f| f.key == key));
        if gone {
            // taken off while it came: nothing should hold on to it
            let _ = b.call("gws_upload_drop", json!({"upload": id})).await;
            return false;
        }
        let upload = id.as_str().map(str::to_owned);
        c.edit(|l| {
            if let Some(file) = l.files.iter_mut().find(|f| f.key == key) {
                file.upload = upload;
                file.arrived = at as u64;
            }
        });
    }
    true
}

/// Asks for files and brings them in.
fn attach(b: Bridge, c: Compose) {
    let options = PickOptions {
        extensions: Vec::new(),
        multiple: true,
    };
    let Some(pick) = b.pick_file(options) else {
        c.said
            .set(Some("This screen can't reach your files.".to_owned()));
        return;
    };
    c.run(async move {
        let files = match pick.await {
            Ok(files) => files,
            Err(e) => {
                c.said.set(Some(e.message));
                return;
            }
        };
        for file in files {
            let held = c.letter.with_untracked(|l| l.files.iter().map(|f| f.size).sum::<u64>());
            if held + file.size > ATTACHMENTS_MAX {
                c.said
                    .set(Some("That's over Gmail's 25 MB limit for one message.".to_owned()));
                continue;
            }
            let key = 1_000 + c.fresh();
            let (name, size) = (file.name.clone(), file.size);
            c.edit(|l| {
                l.files.push(File {
                    key,
                    name,
                    size,
                    ..File::default()
                });
            });
            let bytes = match file.read().await {
                Ok(bytes) => bytes,
                Err(e) => {
                    c.said.set(Some(e.message));
                    c.edit(|l| l.files.retain(|f| f.key != key));
                    continue;
                }
            };
            if !upload(&b, c, key, &file.name, &file.mime, &bytes).await {
                c.edit(|l| l.files.retain(|f| f.key != key));
            }
        }
    });
}

fn remove(b: Bridge, c: Compose, key: u64) {
    let upload = c.letter.with_untracked(|l| {
        l.files
            .iter()
            .find(|f| f.key == key)
            .and_then(|f| f.upload.clone())
    });
    c.edit(|l| l.files.retain(|f| f.key != key));
    if let Some(upload) = upload {
        c.run(async move {
            let _ = b.call("gws_upload_drop", json!({"upload": upload})).await;
        });
    }
}

/// Why this letter can't go yet.
fn not_ready(l: &Letter) -> Option<&'static str> {
    if !l.has_recipient() {
        return Some("Add someone to send this to.");
    }
    if l.files.iter().any(|f| !f.ready()) {
        return Some("A file is still coming in.");
    }
    None
}

/// Send: the press's own call, the save, the hold, then the send.
pub fn send(b: Bridge, c: Compose) {
    if let Some(why) = c.letter.with_untracked(not_ready) {
        c.said.set(Some(why.to_owned()));
        return;
    }
    c.said.set(None);
    let press = b.call("gws_mail_send_arm", json!({}));
    c.run(async move {
        let armed = match press.await {
            Ok(v) => v.get("armed").cloned().unwrap_or(Value::Null),
            Err(e) => {
                c.said.set(Some(e.message));
                return;
            }
        };
        if let Err(why) = save(&b, c).await {
            c.said.set(Some(why));
            return;
        }
        let letter = c.letter.get_untracked();
        let args = json!({
            "draft": letter.draft, "revision": letter.revision,
            "message_id": letter.message_id, "armed": armed,
        });
        c.held.set(Some(letter));
        c.close();
        c.stage.set(Some(Stage::Waiting));
        let turn = c.hold.ask();
        let mut waited = Duration::ZERO;
        while waited < HOLD {
            sleep(TICK).await;
            waited += TICK;
            if !c.hold.counts(turn) {
                return;
            }
        }
        c.stage.set(Some(Stage::Sending));
        let mut answer = b.call("gws_mail_send", args.clone()).await;
        // an answer lost on the way: asking again finds it in Sent, and it can't
        // go twice, as the press it needs is spent
        if matches!(&answer, Err(e) if e.retryable) {
            answer = b.call("gws_mail_send", args).await;
        }
        match answer {
            Ok(_) => {
                c.held.set(None);
                c.stage.set(Some(Stage::Sent));
            }
            Err(e) => {
                c.stage.set(None);
                bring_back(&b, c).await;
                c.said.set(Some(e.message));
            }
        }
    });
}

/// The held letter, back in the panel. What was written since is kept as a draft.
async fn bring_back(b: &Bridge, c: Compose) {
    if !put_away(b, c).await {
        return;
    }
    if let Some(letter) = c.held.get_untracked() {
        c.held.set(None);
        c.fill(letter);
        c.open.set(true);
    }
}

fn undo(b: Bridge, c: Compose) {
    c.hold.ask();
    c.stage.set(None);
    c.run(async move { bring_back(&b, c).await });
}

/// The address being typed: what's after the last comma.
fn last_word(list: &str) -> &str {
    list.rsplit(',').next().unwrap_or_default().trim()
}

/// `list` with the address being typed replaced by `address`, ready for the next.
fn completed(list: &str, address: &str) -> String {
    match list.rsplit_once(',') {
        Some((before, _)) => format!("{before}, {address}, "),
        None => format!("{address}, "),
    }
}

/// Who the word being typed could be, once typing rests for a moment.
async fn suggest(b: Bridge, c: Compose, word: String) {
    let turn = c.asked.ask();
    sleep(SUGGEST_AFTER).await;
    if !c.asked.counts(turn) {
        return;
    }
    if word.len() < 2 {
        c.people.set(Vec::new());
        return;
    }
    let found = b.call("gws_people_suggest", json!({"query": word})).await;
    if !c.asked.counts(turn) {
        return;
    }
    let text = |p: &Value, k: &str| {
        p.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let people = found.ok().and_then(|v| v.get("people").cloned());
    c.people.set(
        people
            .as_ref()
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|p| (text(p, "name"), text(p, "address")))
            .collect(),
    );
}

fn line(c: Compose, name: &'static str, at: fn(&mut Letter) -> &mut String) -> Input {
    input()
        .label(name)
        .value(move || {
            let mut letter = c.letter.get();
            at(&mut letter).clone()
        })
        .placeholder(name.to_owned())
        .app_class(Some(format!("{FIELD} {LINE} {INK}").into()))
}

/// Takes in what was typed: `false` when that's what the letter holds already,
/// as setting a field's value answers with a change of its own.
fn typed(c: Compose, at: fn(&mut Letter) -> &mut String, value: &str) -> bool {
    let mut letter = c.letter.get_untracked();
    if at(&mut letter) == value {
        return false;
    }
    let value = value.to_owned();
    c.edit(|l| *at(l) = value);
    true
}

fn field(c: Compose, name: &'static str, at: fn(&mut Letter) -> &mut String) -> Input {
    line(c, name, at).on_change(move |value: &str| {
        typed(c, at, value);
    })
}

/// A field that takes people, with suggestions as they're typed.
fn people_field(
    b: Bridge,
    c: Compose,
    name: &'static str,
    at: fn(&mut Letter) -> &mut String,
) -> impl IntoElement {
    line(c, name, at).on_change(move |value: &str| {
        if typed(c, at, value) {
            c.naming.set(Some(at));
            b.spawn(suggest(b.clone(), c, last_word(value).to_owned()));
        }
    })
}

fn suggestions(c: Compose) -> impl IntoElement {
    dynamic(move || {
        c.people
            .get()
            .into_iter()
            .fold(view(), move |column, (name, address)| {
                let shown = if name.is_empty() || name == address {
                    address.clone()
                } else {
                    format!("{name} <{address}>")
                };
                let chosen = address.clone();
                column.child(
                    view()
                        .app_class(Some(format!("px-3 py-1 border-b {LINE} {RAIL}").into()))
                        .label("Use this address")
                        .accessibility_value(Some(AccessibilityValue {
                            text: Some(address.into()),
                            ..AccessibilityValue::default()
                        }))
                        .on_press(move || {
                            if let Some(at) = c.naming.get_untracked() {
                                let chosen = chosen.clone();
                                c.edit(|l| *at(l) = completed(at(l), &chosen));
                            }
                            c.asked.ask();
                            c.people.set(Vec::new());
                        })
                        .child(text(shown).app_class(Some(format!("text-[12px] {INK}").into()))),
                )
            })
    })
}

fn press_text(label: &'static str, shows: &'static str, press: impl Fn() + 'static) -> View {
    view()
        .app_class(Some("px-3 py-1".into()))
        .label(label)
        .on_press(press)
        .child(text(shows).app_class(Some(format!("text-[12px] {INK}").into())))
}

fn size_text(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{} KB", bytes.div_ceil(1024))
    }
}

fn file_row(b: Bridge, c: Compose, file: File) -> View {
    let line = if file.ready() {
        size_text(file.size)
    } else {
        format!("{} of {}", size_text(file.arrived), size_text(file.size))
    };
    let key = file.key;
    view()
        .app_class(Some(format!("flex-row items-center gap-2 px-3 py-1 border-b {LINE}").into()))
        .child(
            text(file.name.clone())
                .number_of_lines(1)
                .app_class(Some(format!("flex-1 text-[12px] {INK}").into())),
        )
        .child(text(line).app_class(Some(format!("text-[11px] {MUTE}").into())))
        .child(
            view()
                .label("Remove this attachment")
                .accessibility_value(Some(AccessibilityValue {
                    text: Some(file.name.into()),
                    ..AccessibilityValue::default()
                }))
                .on_press(move || remove(b.clone(), c, key))
                .child(text("Remove").app_class(Some(format!("text-[11px] {MUTE}").into()))),
        )
}

/// The panel, in the reader's place.
pub fn panel(b: Bridge, c: Compose) -> impl IntoElement {
    let (sending, attaching, keeping, files, ask, naming) =
        (b.clone(), b.clone(), b.clone(), b.clone(), b.clone(), b.clone());
    view()
        .app_class(Some("flex-1".into()))
        .child(people_field(b, c, "To", |l| &mut l.to))
        .child(dynamic(move || {
            if c.extra.get() {
                view()
                    .child(people_field(naming.clone(), c, "Cc", |l| &mut l.cc))
                    .child(people_field(naming.clone(), c, "Bcc", |l| &mut l.bcc))
            } else {
                view()
                    .app_class(Some("flex-row".into()))
                    .child(press_text("Add Cc or Bcc", "Cc, Bcc", move || c.extra.set(true)))
            }
        }))
        .child(suggestions(c))
        .child(field(c, "Subject", |l| &mut l.subject))
        .child(
            view().app_class(Some("flex-1".into())).child(
                input()
                    .label("Message")
                    .multiline(true)
                    .value(move || c.letter.with(|l| l.body.clone()))
                    .placeholder("Message".to_owned())
                    .app_class(Some(format!("flex-1 px-3 py-2 text-[13px] {INK}").into()))
                    .on_change(move |value: &str| {
                        if c.letter.with_untracked(|l| l.body != value) {
                            let value = value.to_owned();
                            c.edit(|l| l.body = value);
                        }
                    }),
            ),
        )
        .child(dynamic(move || {
            let rows = c.letter.with(|l| l.files.clone());
            let b = files.clone();
            rows.into_iter().fold(view(), move |v, file| {
                v.child(file_row(b.clone(), c, file))
            })
        }))
        .child(dynamic(move || match c.said.get() {
            Some(line) => view()
                .app_class(Some(format!("px-3 py-1 border-t {LINE}").into()))
                .child(text(line).app_class(Some(format!("text-[12px] {INK}").into()))),
            None => view(),
        }))
        .child(dynamic(move || {
            let ask = ask.clone();
            if c.asking.get() {
                return view()
                    .app_class(Some(format!("flex-row items-center gap-2 px-2 py-1 border-t {LINE}").into()))
                    .child(
                        text("Delete this draft for good?")
                            .app_class(Some(format!("flex-1 px-1 text-[12px] {INK}").into())),
                    )
                    .child(press_text("Keep writing", "Keep writing", move || {
                        c.asking.set(false);
                    }))
                    .child(press_text("Delete it for good", "Delete it", move || {
                        discard(ask.clone(), c);
                    }));
            }
            let (sending, attaching, keeping) =
                (sending.clone(), attaching.clone(), keeping.clone());
            view()
                .app_class(Some(format!("flex-row items-center gap-1 px-2 py-1 border-t {LINE} {RAIL}").into()))
                .child(press_text("Send", "Send", move || send(sending.clone(), c)))
                .child(press_text("Attach a file", "Attach", move || {
                    attach(attaching.clone(), c);
                }))
                .child(view().app_class(Some("flex-1".into())))
                .child(press_text("Keep draft", "Keep draft", move || {
                    keep(keeping.clone(), c);
                }))
                .child(press_text("Delete this draft", "Delete", move || {
                    if c.letter.with_untracked(Letter::blank) {
                        c.close();
                    } else {
                        c.asking.set(true);
                    }
                }))
        }))
}

/// Send's five seconds, and how they ended.
pub fn hold_bar(b: Bridge, c: Compose) -> impl IntoElement {
    dynamic(move || {
        let Some(stage) = c.stage.get() else {
            return view();
        };
        let line = match stage {
            Stage::Waiting => "Sending in a moment.",
            Stage::Sending => "Sending…",
            Stage::Sent => "Message sent.",
        };
        let bar = view()
            .app_class(Some(format!("flex-row items-center gap-3 px-3 py-2 border-t {LINE} {RAIL}").into()))
            .child(text(line).app_class(Some(format!("flex-1 text-[12px] {INK}").into())));
        let b = b.clone();
        match stage {
            Stage::Waiting => bar.child(press_text("Take this back", "Undo", move || {
                undo(b.clone(), c);
            })),
            Stage::Sending => bar,
            Stage::Sent => bar.child(press_text("Dismiss this", "Dismiss", move || {
                c.stage.set(None);
            })),
        }
    })
}
