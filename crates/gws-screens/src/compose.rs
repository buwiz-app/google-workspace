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
/// How many times a slow save is asked after: past the plugin's own ten minutes.
const JOB_ASKS: u32 = 330;
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
    /// A Send of this letter ended with no word of whether it went.
    unsure: bool,
    /// What the person should know about who a reply goes to.
    note: String,
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
        // a forward's files are still on the mail it came from
        let holder = some("source").or_else(|| revision.clone());
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
                    keep: Some(json!({"message": holder, "id": a.get("id"), "name": name, "mime": a.get("mime")})),
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
            unsure: false,
            note: text("note").to_owned(),
        }
    }
}

/// A field of the letter.
type Place = fn(&mut Letter) -> &mut String;

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
    /// From Send's press until it's held or refused: what's sent is what was
    /// showing at the press, so nothing typed after it can be left out.
    frozen: Signal<bool>,
    /// A picker or a file's arrival is under way.
    attaching: Signal<bool>,
    /// Send waits on a second press: these people are new to the person.
    confirming: Signal<Option<Vec<String>>>,
    stage: Signal<Option<Stage>>,
    /// The letter a Send took, until it's gone or undone.
    held: Signal<Option<Letter>>,
    hold: Latest,
    jobs: Signal<UnboundedSender<Job>>,
    next: Signal<u64>,
    /// `(name, address)` suggested for what's being typed, and where it goes.
    people: Signal<Vec<(String, String)>>,
    naming: Signal<Option<Place>>,
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
            frozen: signal(false),
            attaching: signal(false),
            confirming: signal(None),
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
        if self.frozen.get_untracked() {
            return;
        }
        self.letter.update(change);
        self.edits.set(self.edits.get_untracked() + 1);
        // what was asked about may not be what's there now
        self.confirming.set(None);
    }

    fn fill(self, letter: Letter) {
        let extra = !letter.cc.is_empty() || !letter.bcc.is_empty();
        self.letter.set(letter);
        self.saved.set(self.edits.get_untracked());
        self.extra.set(extra);
        self.said.set(None);
        self.asking.set(false);
        self.confirming.set(None);
        self.frozen.set(false);
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
        if idle
            && c.letter
                .with_untracked(|l| !l.blank() && l.files.is_empty())
        {
            c.saving.set(true);
            let b = b.clone();
            c.run(async move {
                // an address half typed isn't one yet: leaving says so, not this
                let _ = save(&b, c, &Value::Null).await;
                c.saving.set(false);
            });
        }
    }
}

/// A tool's answer as a screen gets it, from a result that came by way of a job.
fn answered(result: &Value) -> Result<Value, BuwizError> {
    let content = result.get("structuredContent").cloned();
    if result.get("isError") == Some(&Value::Bool(true)) {
        let text = |path: &str| result.pointer(path).and_then(Value::as_str);
        return Err(BuwizError::new(
            text("/structuredContent/code").unwrap_or("failed"),
            false,
            text("/content/0/text").unwrap_or("That didn't work."),
        ));
    }
    Ok(content.unwrap_or(Value::Null))
}

/// How a save too slow for its call came out: it goes on in the plugin. An
/// upload gives up after ten minutes, so this needn't wait longer than that.
async fn wait_for(b: &Bridge, job: &str) -> Result<Value, BuwizError> {
    let lost = || BuwizError::new("failed", false, "That save was lost. Try again.");
    for _ in 0..JOB_ASKS {
        sleep(AUTOSAVE).await;
        let v = b.call("gws_mail_job", json!({"job": job})).await?;
        if v.get("done") == Some(&Value::Bool(true)) {
            return answered(v.get("answer").unwrap_or(&Value::Null));
        }
        if v.get("gone") == Some(&Value::Bool(true)) {
            return Err(lost());
        }
    }
    Err(lost())
}

/// One save. Only a message with files can outlast its call, so only that gets
/// a job to ask after.
async fn write(b: &Bridge, letter: &Letter, armed: &Value) -> Result<Value, BuwizError> {
    let mut args = letter.args();
    if !armed.is_null() {
        args["armed"] = armed.clone();
    }
    if letter.files.is_empty() {
        return b.call("gws_mail_draft_save", args).await;
    }
    let job = b.call("gws_mail_job_start", json!({})).await?;
    let job = job.get("job").and_then(Value::as_str).unwrap_or_default();
    args["job"] = job.into();
    match b.call("gws_mail_draft_save", args).await {
        Err(e) if e.code == "timeout" => wait_for(b, job).await,
        answer => answer,
    }
}

/// What a refusal the screen must act on says. A failed tool reaches a screen as
/// words with no code, so these come as answers.
fn refused_text(v: &Value) -> Option<&str> {
    v.get("refused").and_then(Value::as_str)
}

fn refused(answer: &Result<Value, BuwizError>) -> Option<&str> {
    answer.as_ref().ok().and_then(refused_text)
}

/// Writes the letter to its draft, with the press that will send it, if any.
/// A draft deleted elsewhere is written anew, but never one that may have gone:
/// sending that again is the worst thing this screen could do. `false` says the
/// message had already gone.
async fn save(b: &Bridge, c: Compose, armed: &Value) -> Result<bool, String> {
    let seen = c.edits.get_untracked();
    let mut letter = c.letter.get_untracked();
    let mut answer = write(b, &letter, armed).await;
    if refused(&answer) == Some("not_found") && letter.draft.is_some() {
        if letter.unsure {
            c.letter.update(|l| {
                l.unsure = false;
                l.draft = None;
                l.revision = None;
                l.message_id = None;
            });
            return Err(
                "That draft is gone, and the message may have gone out. Look in Sent, and press Send again only if it isn't there."
                    .to_owned(),
            );
        }
        letter.draft = None;
        answer = write(b, &letter, armed).await;
    }
    if refused(&answer) == Some("already_sent") {
        c.close();
        c.stage.set(Some(Stage::Sent));
        return Ok(false);
    }
    let v = answer.map_err(|e| e.message)?;
    let text = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
    if refused_text(&v).is_some() {
        return Err(text("message").unwrap_or_else(|| "That didn't work.".to_owned()));
    }
    let kept = v.get("kept").and_then(Value::as_array);
    c.letter.update(|l| {
        l.draft = text("draft");
        l.revision = text("revision");
        l.message_id = text("message_id");
        // those attachments' ids left with the old message: the plugin holds them now
        for held in kept.into_iter().flatten() {
            let id = held.get("id");
            let was = |f: &&mut File| f.keep.as_ref().and_then(|k| k.get("id")) == id;
            if let Some(file) = l.files.iter_mut().find(was) {
                file.keep = None;
                file.upload = held
                    .get("upload")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
        }
    });
    c.saved.set(seen);
    Ok(true)
}

/// What's open is saved before something else takes its place. `false` leaves
/// it open, with the reason showing.
async fn put_away(b: &Bridge, c: Compose) -> bool {
    if !c.open.get_untracked() || c.letter.with_untracked(Letter::blank) || !c.dirty() {
        return true;
    }
    match save(b, c, &Value::Null).await {
        Ok(_) => true,
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

/// The draft a card named, opened here to go on with.
pub fn resume_ticket(b: Bridge, c: Compose, ticket: String) {
    c.run(async move {
        match b
            .call("gws_mail_draft_read", json!({"ticket": ticket}))
            .await
        {
            Ok(v) => c.fill(Letter::of(&v)),
            Err(e) => c.said.set(Some(e.message)),
        }
        c.open.set(true);
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
        let gone = !c
            .letter
            .with_untracked(|l| l.files.iter().any(|f| f.key == key));
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
    c.attaching.set(true);
    c.run(async move {
        let files = match pick.await {
            Ok(files) => files,
            Err(e) => {
                c.attaching.set(false);
                c.said.set(Some(e.message));
                return;
            }
        };
        for file in files {
            let held = c
                .letter
                .with_untracked(|l| l.files.iter().map(|f| f.size).sum::<u64>());
            if held + file.size > ATTACHMENTS_MAX {
                c.said.set(Some(
                    "That's over Gmail's 25 MB limit for one message.".to_owned(),
                ));
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
        c.attaching.set(false);
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

/// Send: the press's own call, the save, a look at who it goes to, the hold,
/// then the send.
pub fn send(b: Bridge, c: Compose) {
    press_send(b, c, false);
}

fn press_send(b: Bridge, c: Compose, confirmed: bool) {
    let why = if c.attaching.get_untracked() {
        Some("A file is still coming in.")
    } else {
        c.letter.with_untracked(not_ready)
    };
    if let Some(why) = why {
        c.said.set(Some(why.to_owned()));
        return;
    }
    c.said.set(None);
    c.confirming.set(None);
    c.frozen.set(true);
    let press = b.call("gws_mail_send_arm", json!({}));
    c.run(async move {
        if let Err(why) = send_pressed(&b, c, press, confirmed).await {
            c.said.set(Some(why));
        }
        c.frozen.set(false);
    });
}

async fn send_pressed(
    b: &Bridge,
    c: Compose,
    press: impl Future<Output = Result<Value, BuwizError>>,
    confirmed: bool,
) -> Result<(), String> {
    let press = press.await.map_err(|e| e.message)?;
    let armed = press.get("armed").cloned().unwrap_or(Value::Null);
    if !save(b, c, &armed).await? {
        return Ok(());
    }
    if !confirmed {
        let draft = c.letter.with_untracked(|l| l.draft.clone());
        let args = json!({"draft": draft, "review": true});
        let read = b.call("gws_mail_draft_read", args).await;
        let new = people_new(&read.map_err(|e| e.message)?);
        if !new.is_empty() {
            c.confirming.set(Some(new));
            return Ok(());
        }
    }
    hold_and_send(b, c, armed).await
}

fn people_new(review: &Value) -> Vec<String> {
    let new = review.get("new").and_then(Value::as_array);
    new.into_iter()
        .flatten()
        .filter_map(|n| n.as_str().map(str::to_owned))
        .collect()
}

/// The five seconds, then the send of the letter as it stands, which must be
/// the revision the person read. `Err` has brought the letter back.
async fn hold_and_send(b: &Bridge, c: Compose, armed: Value) -> Result<(), String> {
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
            return Ok(());
        }
    }
    c.stage.set(Some(Stage::Sending));
    let mut answer = b.call("gws_mail_send", args.clone()).await;
    // an answer lost on the way: asking again finds it in Sent, and it can't go
    // twice, as the press it needs is spent
    let lost = matches!(&answer, Err(e) if e.retryable);
    if lost {
        answer = b.call("gws_mail_send", args).await;
    }
    let why = match (&answer, refused(&answer)) {
        (Ok(_), None) => {
            c.held.set(None);
            c.stage.set(Some(Stage::Sent));
            return Ok(());
        }
        (Ok(v), Some(_)) => v
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("That didn't work."),
        (Err(e), _) => e.message.as_str(),
    };
    // only a refusal to a first try is sure it didn't go
    let sure = !lost && matches!(refused(&answer), Some("changed" | "not_allowed"));
    c.stage.set(None);
    c.held.update(|held| {
        if let Some(held) = held {
            held.unsure = !sure;
        }
    });
    bring_back(b, c).await;
    Err(why.to_owned())
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

fn line(c: Compose, name: &'static str, at: Place) -> Input {
    input()
        .label(name)
        .value(move || {
            let mut letter = c.letter.get();
            at(&mut letter).clone()
        })
        .placeholder(name.to_owned())
        .readonly(move || c.frozen.get())
        .app_class(Some(format!("{FIELD} {LINE} {INK}").into()))
}

/// Takes in what was typed: `false` when that's what the letter holds already,
/// as setting a field's value answers with a change of its own.
fn typed(c: Compose, at: Place, value: &str) -> bool {
    let mut letter = c.letter.get_untracked();
    if at(&mut letter) == value || c.frozen.get_untracked() {
        return false;
    }
    let value = value.to_owned();
    c.edit(|l| *at(l) = value);
    true
}

fn field(c: Compose, name: &'static str, at: Place) -> Input {
    line(c, name, at).on_change(move |value: &str| {
        typed(c, at, value);
    })
}

/// A field that takes people, with suggestions as they're typed.
fn people_field(b: Bridge, c: Compose, name: &'static str, at: Place) -> impl IntoElement {
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
        .app_class(Some(
            format!("flex-row items-center gap-2 px-3 py-1 border-b {LINE}").into(),
        ))
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
    let (sending, attaching, keeping, files, ask, naming) = (
        b.clone(),
        b.clone(),
        b.clone(),
        b.clone(),
        b.clone(),
        b.clone(),
    );
    view()
        .app_class(Some("flex-1".into()))
        .child(people_field(b, c, "To", |l| &mut l.to))
        .child(dynamic(move || {
            if c.extra.get() {
                view()
                    .child(people_field(naming.clone(), c, "Cc", |l| &mut l.cc))
                    .child(people_field(naming.clone(), c, "Bcc", |l| &mut l.bcc))
            } else {
                view().app_class(Some("flex-row".into())).child(press_text(
                    "Add Cc or Bcc",
                    "Cc, Bcc",
                    move || c.extra.set(true),
                ))
            }
        }))
        .child(suggestions(c))
        .child(field(c, "Subject", |l| &mut l.subject))
        .child(
            view().app_class(Some("flex-1".into())).child(
                input()
                    .label("Message")
                    .multiline(true)
                    .readonly(move || c.frozen.get())
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
            rows.into_iter()
                .fold(view(), move |v, file| v.child(file_row(b.clone(), c, file)))
        }))
        .child(dynamic(move || {
            let note = c.letter.with(|l| l.note.clone());
            let lines = [Some(note).filter(|n| !n.is_empty()), c.said.get()];
            lines.into_iter().flatten().fold(view(), |column, line| {
                column.child(
                    view()
                        .app_class(Some(format!("px-3 py-1 border-t {LINE}").into()))
                        .child(text(line).app_class(Some(format!("text-[12px] {INK}").into()))),
                )
            })
        }))
        .child(dynamic(move || {
            let ask = ask.clone();
            if let Some(new) = c.confirming.get() {
                let line = format!("New to you: {}. Send it?", new.join(", "));
                return view()
                    .app_class(Some(
                        format!("flex-row items-center gap-2 px-2 py-1 border-t {LINE}").into(),
                    ))
                    .child(
                        text(line).app_class(Some(format!("flex-1 px-1 text-[12px] {INK}").into())),
                    )
                    .child(press_text("Keep writing", "Keep writing", move || {
                        c.confirming.set(None);
                    }))
                    .child(press_text("Send it", "Send it", move || {
                        press_send(ask.clone(), c, true);
                    }));
            }
            if c.asking.get() {
                return view()
                    .app_class(Some(
                        format!("flex-row items-center gap-2 px-2 py-1 border-t {LINE}").into(),
                    ))
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
                .app_class(Some(
                    format!("flex-row items-center gap-1 px-2 py-1 border-t {LINE} {RAIL}").into(),
                ))
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
            .app_class(Some(
                format!("flex-row items-center gap-3 px-3 py-2 border-t {LINE} {RAIL}").into(),
            ))
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

/// How many recipients a card sends to without asking once more.
const FEW: usize = 5;

/// What the card has to show.
#[derive(Clone, PartialEq, Default)]
struct Review {
    from: String,
    /// Recipients the person has never written to.
    new: Vec<String>,
}

fn recipients(l: &Letter) -> usize {
    [&l.to, &l.cc, &l.bcc]
        .iter()
        .flat_map(|f| f.split(','))
        .filter(|a| !a.trim().is_empty())
        .count()
}

/// Reads the draft again: what's shown is always what Send would send.
async fn read_card(
    b: &Bridge,
    c: Compose,
    ticket: &str,
    review: Signal<Option<Review>>,
) -> Result<(), String> {
    let v = b
        .call("gws_mail_draft_read", json!({"ticket": ticket}))
        .await
        .map_err(|e| e.message)?;
    c.fill(Letter::of(&v));
    let new = v.get("new").and_then(Value::as_array).into_iter().flatten();
    review.set(Some(Review {
        from: v
            .get("from")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        new: new.filter_map(|n| n.as_str().map(str::to_owned)).collect(),
    }));
    Ok(())
}

fn review_line(label: &'static str, value: String) -> View {
    view()
        .app_class(Some("flex-row gap-2 px-3 py-[2px]".into()))
        .child(text(label).app_class(Some(format!("w-[52px] text-[11px] {MUTE}").into())))
        .child(text(value).app_class(Some(format!("flex-1 text-[12px] {INK}").into())))
}

/// The Send press on a card: its own call first, as in the panel. A draft that
/// changed since it showed is refused by revision, and shown again as it is.
fn send_reviewed(b: Bridge, c: Compose, ticket: String, review: Signal<Option<Review>>) {
    let shown = c
        .letter
        .with_untracked(|l| json!({"draft": l.draft, "revision": l.revision}));
    let press = b.call("gws_mail_send_arm", shown);
    c.run(async move {
        let armed = match press.await {
            Ok(v) => v.get("armed").cloned().unwrap_or(Value::Null),
            Err(e) => {
                c.said.set(Some(e.message));
                return;
            }
        };
        match hold_and_send(&b, c, armed).await {
            Ok(()) if c.stage.get_untracked() == Some(Stage::Sent) => {
                let _ = b.settle("Message sent.").await;
            }
            Ok(()) => {}
            Err(why) => {
                let _ = read_card(&b, c, &ticket, review).await;
                c.said.set(Some(why));
            }
        }
    });
}

/// The draft card under a chat's answer: everything Send would send, and the
/// person's press. The model filled it and can press nothing here.
pub fn card(b: Bridge, ticket: String) -> View {
    let (c, waiting) = Compose::new();
    let review = signal(None::<Review>);
    let kept = signal(false);
    b.spawn(work(waiting));
    let (loading, name) = (b.clone(), ticket.clone());
    c.run(async move {
        if let Err(why) = read_card(&loading, c, &name, review).await {
            c.said.set(Some(why));
        }
        loading.ready();
    });
    let (fit, bar) = (b.clone(), b.clone());
    view()
        .app_class(Some("bg-white dark:bg-[#1d1d1d]".into()))
        .on_layout(
            move |bounds: &nsh_core::geometry::Bounds<nsh_core::geometry::Points>| {
                let _ = fit.fit(bounds.size.height.0.ceil());
            },
        )
        .child(hold_bar(bar, c))
        .child(dynamic(move || {
            let shown = review.get();
            if c.stage.get().is_some() {
                return view();
            }
            if kept.get() {
                return view().app_class(Some("p-3".into())).child(
                    text("Kept in Drafts.").app_class(Some(format!("text-[12px] {MUTE}").into())),
                );
            }
            let Some(shown) = shown else {
                let line = c
                    .said
                    .get()
                    .unwrap_or_else(|| "Reading the draft…".to_owned());
                return view()
                    .app_class(Some("p-3".into()))
                    .child(text(line).app_class(Some(format!("text-[12px] {MUTE}").into())));
            };
            let letter = c.letter.get();
            let stranger = letter.thread.is_some() && !shown.new.is_empty();
            let mut card = view()
                .app_class(Some("py-2".into()))
                .label("This draft")
                .child(review_line("From", shown.from.clone()))
                .child(review_line("To", letter.to.clone()));
            if !letter.cc.is_empty() {
                card = card.child(review_line("Cc", letter.cc.clone()));
            }
            if !letter.bcc.is_empty() {
                card = card.child(review_line("Bcc", letter.bcc.clone()));
            }
            if !shown.new.is_empty() {
                card = card.child(review_line("New", shown.new.join(", ")));
            }
            card = card
                .child(review_line("Subject", letter.subject.clone()))
                .child(
                    scroll()
                        .app_class(Some(
                            format!("max-h-[220px] mt-1 px-3 py-2 border-t {LINE}").into(),
                        ))
                        .child(
                            text(letter.body.clone())
                                .app_class(Some(format!("text-[12px] {INK}").into())),
                        ),
                );
            if let Some(line) = c.said.get() {
                card = card.child(
                    view()
                        .app_class(Some(format!("px-3 py-1 border-t {LINE}").into()))
                        .child(text(line).app_class(Some(format!("text-[12px] {INK}").into()))),
                );
            }
            if stranger {
                card = card.child(
                    view()
                        .app_class(Some(format!("px-3 py-1 border-t {LINE}").into()))
                        .child(
                            text("Replies to someone new. Open the full review to send it.")
                                .app_class(Some(format!("text-[12px] {INK}").into())),
                        ),
                );
            }
            let (sending, opening, name) = (b.clone(), b.clone(), ticket.clone());
            let open = press_text(
                if stranger {
                    "Open the full review"
                } else {
                    "Edit this draft"
                },
                if stranger { "Open full review" } else { "Edit" },
                move || {
                    let open = opening.open("mail", json!({"draft": name}));
                    opening.spawn(async move { drop(open.await) });
                },
            );
            let keep = press_text("Keep draft", "Keep draft", move || kept.set(true));
            let mut row = view()
                .app_class(Some(
                    format!("flex-row items-center gap-1 px-2 py-1 border-t {LINE} {RAIL}").into(),
                ))
                .child(keep)
                .child(open)
                .child(view().app_class(Some("flex-1".into())));
            if !stranger {
                let needs_asking =
                    recipients(&letter) > FEW || !letter.bcc.is_empty() || !shown.new.is_empty();
                let name = ticket.clone();
                if c.asking.get() {
                    row = row
                        .child(
                            text(format!("Send to {}?", recipients(&letter)))
                                .app_class(Some(format!("text-[12px] {INK}").into())),
                        )
                        .child(press_text("Not yet", "Not yet", move || {
                            c.asking.set(false)
                        }))
                        .child(press_text("Send it", "Send it", move || {
                            c.asking.set(false);
                            send_reviewed(sending.clone(), c, name.clone(), review);
                        }));
                } else {
                    row = row.child(press_text("Send", "Send", move || {
                        if needs_asking {
                            c.asking.set(true);
                        } else {
                            send_reviewed(sending.clone(), c, name.clone(), review);
                        }
                    }));
                }
            }
            view().child(card).child(row)
        }))
}
