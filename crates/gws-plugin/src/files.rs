//! Opening a Drive file or a mail's attachment in the Bot's viewer, and editing
//! back. Only a person's press reaches this: nothing uploads on its own, in the
//! background or on a timer. `drive/write.rs` has Drive's other writes.
//!
//! An open file lands in `open/<id>/<name>`. Both parts come from outside, so
//! the id is checked and the name made safe before either touches a path, and
//! the bytes go straight to that file: a tool's result is limited to 1 MiB, and
//! what it carries is a path, in `_meta["buwiz/document"]`, for the Bot to check.
//!
//! The file sits unencrypted while the viewer has it. It goes after upload or
//! discard, after 24 hours, and at sign-out. A file edited here and not yet
//! answered stays through all of those, since the person's work is in it and
//! nowhere else, and a sign-out can be a failed refresh and not a choice: only
//! the person's press or an uninstall takes it. It waits for its own account to
//! sign in again. An edited attachment is the same: it has no file on Drive to
//! go back to, so its answers are a new file in My Drive, or discard.
//!
//! We never write over a Google Doc, Sheet or Slides: Google says an update
//! replaces "the full contents of the document", and doesn't say what becomes
//! of comments and suggestions. They take `copy` alone.
//!
//! Drive v3 has no atomic check, so check-then-write leaves a small race.
//! Revision history is the backstop.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{Read as _, SeekFrom, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::time::Duration;

use gws_store::Store;
use gws_store::open::Opened;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use crate::accounts::Bearer;
use crate::drive::{self, Answer, DOC, SHEET, SLIDES, id_ok, kind_of, refusal_of};
use crate::gmail::{self, Failure};
use crate::mail::{signed_in, with_own_store, with_store};
use crate::tools::{
    FILE_DISCARD, FILE_EDITS, FILE_OPEN, FILE_UPLOAD, MAIL_ATTACHMENT_OPEN, failed, ok,
};
use crate::{Core, js};

/// How long a file stays once it's been opened.
pub const LIFE_MS: i64 = 24 * 60 * 60 * 1000;
/// How old a file's stamps must be before they can vouch for it: a save in the
/// same tick as a hash leaves the same stamps, and the coarsest disks tick every
/// two seconds.
pub const SETTLE_MS: i64 = 2000;
/// Google's limit on an export.
const EXPORT_MAX: u64 = 10 * 1024 * 1024;
/// A real file: far more than a document needs, and well short of filling a
/// disk because someone shared a film. Google's word for its size isn't taken
/// for it, so the download has this stop of its own.
const FILE_MAX: u64 = 1024 * 1024 * 1024;
/// How long a transfer may run, on the tool's own thread. The Bot gives up on a
/// call after 30 seconds, and this goes on past that: the next press finds it
/// done.
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Bytes a piece of an upload holds, in Google's multiples of 256 KiB, so a big
/// file never has to fit in memory.
const PIECE: usize = 8 * 1024 * 1024;
/// A file system takes 255 bytes in a name. This leaves room for the rest.
const NAME_MAX: usize = 200;
/// The longest part after the last dot that counts as an extension.
const EXTENSION_MAX: usize = 16;
/// The longest part id taken: a path of numbers, as deep as the mail's nesting.
const PART_ID_MAX: usize = 64;
/// The longest attachment id taken: Gmail's run to hundreds of characters.
const ATTACHMENT_ID_MAX: usize = 4096;
/// Starts the folder of an attachment, and never a Drive file's.
const ATTACHMENT: &str = "att-";
/// Where a download waits to be whole. No safe name starts with a dot.
const PART: &str = ".part";
const DOCX: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
const PPTX: &str = "application/vnd.openxmlformats-officedocument.presentationml.presentation";
/// What a file says when it says nothing: a mail's parts often do.
const NO_TYPE: [&str; 2] = ["", "application/octet-stream"];
/// What can be opened, with its extensions: what the Bot's Word and Spreadsheet
/// views take, as its own check does. Anything else stays on Drive, since the Bot
/// would refuse it and the person would see nothing happen.
const TYPES: [(&str, &[&str]); 5] = [
    (DOCX, &["docx"]),
    (XLSX, &["xlsx"]),
    ("application/msword", &["doc"]),
    ("application/vnd.ms-excel", &["xls"]),
    ("text/csv", &["csv"]),
];
const OPEN_FIELDS: &str = "id,name,mimeType,size,version,md5Checksum,capabilities(canDownload)";
const CHECK_FIELDS: &str = "version,md5Checksum,parents";

/// The time, set by a test. Nothing a tool is given can reach it.
#[derive(Default)]
pub struct Clock(AtomicI64);

impl Clock {
    pub fn now_ms(&self) -> i64 {
        match self.0.load(Ordering::Relaxed) {
            0 => chrono::Utc::now().timestamp_millis(),
            set => set,
        }
    }

    /// From here on it's `ms`, and stays there.
    pub fn set(&self, ms: i64) {
        self.0.store(ms, Ordering::Relaxed);
    }
}

/// What's being downloaded or uploaded now, by folder. A second press on one
/// waits its turn, and a sweep leaves its half-made folder alone.
#[derive(Default)]
pub struct Working(std::sync::Mutex<HashSet<String>>);

impl Working {
    fn held(&self) -> std::sync::MutexGuard<'_, HashSet<String>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn holds(&self, id: &str) -> bool {
        self.held().contains(id)
    }
}

/// The turn of one file: over when this is dropped, however the call ended.
struct Turn<'a> {
    core: &'a Core,
    id: String,
}

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        self.core.working.held().remove(&self.id);
    }
}

fn turn<'a>(core: &'a Core, id: &str) -> Option<Turn<'a>> {
    core.working.held().insert(id.to_owned()).then(|| Turn {
        core,
        id: id.to_owned(),
    })
}

/// What the disk says of a file, without reading it. The change time is here
/// because nothing can set it back: a save that keeps the size and restores the
/// modified time still moves it, and a save by rename brings a new inode.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Stamp {
    size: u64,
    inode: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

impl Stamp {
    fn of(meta: &fs::Metadata) -> Self {
        Self {
            size: meta.len(),
            inode: meta.ino(),
            modified: (meta.mtime(), meta.mtime_nsec()),
            changed: (meta.ctime(), meta.ctime_nsec()),
        }
    }

    /// Whether a later save would show. A stamp from the future says nothing.
    fn settled(&self, now_ms: i64) -> bool {
        let ms = |(s, ns): (i64, i64)| s * 1000 + ns / 1_000_000;
        now_ms - ms(self.modified).max(ms(self.changed)) >= SETTLE_MS
    }
}

/// The last hash of each open file, by folder, so a poll doesn't read a big file
/// again to learn nothing. Only a guess for what shows on a screen: what acts on
/// a file hashes it for real. Gone at a restart, and the first poll reads again.
#[derive(Default)]
pub struct Hashes {
    known: std::sync::Mutex<HashMap<String, (Stamp, String)>>,
    runs: AtomicUsize,
}

impl Hashes {
    fn held(&self) -> std::sync::MutexGuard<'_, HashMap<String, (Stamp, String)>> {
        self.known.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// How many files have been read through, for a test.
    pub fn runs(&self) -> usize {
        self.runs.load(Ordering::Relaxed)
    }

    fn run(&self, path: &Path) -> std::io::Result<String> {
        self.runs.fetch_add(1, Ordering::Relaxed);
        hash_of(path)
    }
}

/// What a download came to.
enum Saved {
    /// The whole file, as `.part`, and its hash.
    Whole(String),
    TooBig,
    Refused(Answer),
}

/// What a session's start or its last piece came to.
pub(crate) enum Sent {
    Done(Value),
    Refused(Answer),
}

type Response = ureq::http::Response<ureq::Body>;

/// The long calls, Drive's: slower than [`drive::DriveApi`] allows, and it
/// follows no redirects, so the token only ever goes to the API base.
pub struct Transfers {
    agent: ureq::Agent,
    base: String,
    upload: String,
}

fn could_not_reach(e: &ureq::Error) -> Failure {
    Failure::Offline(format!(
        "could not reach Google Drive: {}",
        js::slice_utf16(&e.to_string(), 200)
    ))
}

fn cant_save() -> Failure {
    Failure::Plain("That file couldn't be saved on this Mac.".to_owned())
}

fn cant_read(e: &std::io::Error) -> Failure {
    Failure::Plain(format!(
        "Google Drive's answer could not be read: {}",
        js::slice_utf16(&e.to_string(), 200)
    ))
}

fn refusal_body(res: &mut Response) -> Vec<u8> {
    res.body_mut()
        .with_config()
        .limit(1024 * 1024)
        .read_to_vec()
        .unwrap_or_default()
}

impl Transfers {
    /// `base` is [`drive::DRIVE_API`] in production; tests pass a local server.
    pub fn new(base: &str) -> Self {
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .timeout_global(Some(TRANSFER_TIMEOUT))
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .user_agent("buwiz-gws")
            .build();
        let base = base.trim_end_matches('/').to_owned();
        Self {
            agent: config.new_agent(),
            upload: base.replacen("/drive/v3", "/upload/drive/v3", 1),
            base,
        }
    }

    /// `path` into `to`, stopping at `max` bytes whatever it said it would be.
    fn save(
        &self,
        auth: &Bearer,
        path: &str,
        query: &[(&str, String)],
        max: u64,
        to: &Path,
    ) -> Result<Saved, Failure> {
        let mut req = self
            .agent
            .get(format!("{}/{path}", self.base))
            .header("authorization", auth.header());
        for (k, v) in query {
            req = req.query(*k, v);
        }
        let mut res = req.call().map_err(|e| could_not_reach(&e))?;
        let status = res.status().as_u16();
        if (300..400).contains(&status) {
            return Ok(Saved::Refused(Answer::Forbidden("redirected".to_owned())));
        }
        if status != 200 {
            return refusal_of(status, &refusal_body(&mut res)).map(Saved::Refused);
        }
        let mut out = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(to)
            .map_err(|_| cant_save())?;
        let (mut hash, mut held) = (Sha256::new(), 0u64);
        let mut reader = res.body_mut().as_reader();
        let mut buffer = vec![0; 64 * 1024];
        loop {
            let n = reader.read(&mut buffer).map_err(|e| cant_read(&e))?;
            if n == 0 {
                return Ok(Saved::Whole(format!("{:x}", hash.finalize())));
            }
            held += n as u64;
            if held > max {
                return Ok(Saved::TooBig);
            }
            hash.update(&buffer[..n]);
            out.write_all(&buffer[..n]).map_err(|_| cant_save())?;
        }
    }

    /// Starts a resumable upload: where its pieces go. The one way in for a file
    /// of any size, and the bytes never need the token. `fields` is what the
    /// last piece's answer holds.
    pub(crate) fn session(
        &self,
        auth: &Bearer,
        id: Option<&str>,
        metadata: &Value,
        mime: &str,
        length: u64,
        fields: &str,
    ) -> Result<Result<String, Answer>, Failure> {
        let req = match id {
            Some(id) => self.agent.patch(format!("{}/files/{id}", self.upload)),
            None => self.agent.post(format!("{}/files", self.upload)),
        };
        let mut res = req
            .query("uploadType", "resumable")
            .query("fields", fields)
            .header("authorization", auth.header())
            .header("content-type", "application/json; charset=UTF-8")
            .header("x-upload-content-type", mime)
            .header("x-upload-content-length", length.to_string())
            .send(metadata.to_string().as_str())
            .map_err(|e| could_not_reach(&e))?;
        let status = res.status().as_u16();
        if status != 200 {
            return refusal_of(status, &refusal_body(&mut res)).map(Err);
        }
        let place = res
            .headers()
            .get("location")
            .and_then(|l| l.to_str().ok())
            .unwrap_or_default();
        // the person's file goes to Google and nowhere else
        if !place.starts_with(&format!("{}/", self.upload)) {
            return Err(Failure::Plain(
                "Google Drive didn't say where the file should go.".to_owned(),
            ));
        }
        Ok(Ok(place.to_owned()))
    }

    /// The file's bytes, a piece at a time.
    pub(crate) fn send(
        &self,
        place: &str,
        file: &mut (impl std::io::Read + std::io::Seek),
        length: u64,
    ) -> Result<Sent, Failure> {
        let mut buffer = vec![0; PIECE];
        let mut from = 0u64;
        loop {
            let want = (length - from).min(PIECE as u64) as usize;
            file.seek(SeekFrom::Start(from)).map_err(|_| cant_save())?;
            file.read_exact(&mut buffer[..want])
                .map_err(|_| cant_save())?;
            let to = from + want as u64;
            let range = format!("bytes {from}-{}/{length}", to - 1);
            let mut res = self
                .agent
                .put(place)
                .header("content-range", range)
                .send(&buffer[..want])
                .map_err(|e| could_not_reach(&e))?;
            let status = res.status().as_u16();
            match status {
                // what Drive kept can be short of what went, and it goes on
                // from there; a piece that moved nothing on would never end
                308 => {
                    let kept = kept_of(&res);
                    if kept <= from || kept > to || kept == length {
                        return Err(unfinished());
                    }
                    from = kept;
                }
                // Drive has it all now, so an answer that can't be read is no
                // failure: another press would make a second file
                200 | 201 if to == length => {
                    let body = refusal_body(&mut res);
                    return Ok(Sent::Done(
                        serde_json::from_slice(&body).unwrap_or(Value::Null),
                    ));
                }
                200 | 201 => return Err(unfinished()),
                _ => return refusal_of(status, &refusal_body(&mut res)).map(Sent::Refused),
            }
        }
    }
}

/// How many bytes a 308 says Drive has: `Range: bytes=0-<last>`, and none when
/// it doesn't say.
fn kept_of(res: &Response) -> u64 {
    res.headers()
        .get("range")
        .and_then(|range| range.to_str().ok())
        .and_then(|range| range.strip_prefix("bytes=0-"))
        .and_then(|last| last.parse::<u64>().ok())
        .map_or(0, |last| last + 1)
}

fn unfinished() -> Failure {
    Failure::Plain("Google Drive didn't finish that upload. Your file is still here.".to_owned())
}

fn refusal(code: &str, why: &str) -> Value {
    ok(json!({"refused": code, "message": why}))
}

fn bad_args(why: &str) -> Value {
    refusal("invalid_args", why)
}

fn gone() -> Value {
    refusal("not_found", "That's gone from Google Drive.")
}

fn not_here() -> Value {
    refusal("not_found", "That file isn't open here any more.")
}

fn no_access() -> Value {
    refusal("no_access", "This account can't read that in Google Drive.")
}

fn no_write() -> Value {
    refusal(
        "no_access",
        "This account can't change that in Google Drive. Your file is still here.",
    )
}

fn busy() -> Value {
    refusal(
        "busy",
        "Google Drive is still working on this file. Try again in a moment.",
    )
}

fn unsupported() -> Value {
    refusal("unsupported", "This kind of file can't be opened here.")
}

/// A refused read, in the tool's terms.
fn read_refused(answer: Answer) -> Value {
    match answer {
        Answer::NotFound => gone(),
        Answer::Invalid(why) => bad_args(&why),
        Answer::Done(_) | Answer::Forbidden(_) => no_access(),
    }
}

/// A refused write. Whatever Google said, the local file stays.
fn write_refused(answer: Answer) -> Value {
    match answer {
        Answer::NotFound => refusal(
            "not_found",
            "That's gone from Google Drive. Your file is still here.",
        ),
        Answer::Invalid(why) => bad_args(&why),
        Answer::Forbidden(reason) if reason == "storageQuotaExceeded" => refusal(
            "no_space",
            "Your Google storage is full, so nothing was uploaded. Your file is still here.",
        ),
        Answer::Done(_) | Answer::Forbidden(_) => no_write(),
    }
}

/// Where the same file opens in a browser. Made here from the id, which was
/// checked, and not taken from any answer.
fn browser_link(id: &str, mime: &str) -> String {
    match mime {
        DOC => format!("https://docs.google.com/document/d/{id}/edit"),
        SHEET => format!("https://docs.google.com/spreadsheets/d/{id}/edit"),
        SLIDES => format!("https://docs.google.com/presentation/d/{id}/edit"),
        _ => format!("https://drive.google.com/file/d/{id}/view"),
    }
}

fn too_big(id: &str, mime: &str, why: &str) -> Value {
    ok(json!({"refused": "too_big", "message": why, "link": browser_link(id, mime)}))
}

fn too_big_for(id: &str, mime: &str) -> Value {
    if export_of(mime).is_some() {
        too_big(
            id,
            mime,
            "Google won't export a file over 10 MB. You can open it in the browser.",
        )
    } else {
        too_big(
            id,
            mime,
            "This file is over 1 GB, too big to open here. You can open it in the browser.",
        )
    }
}

/// What a Google file exports as. Any other Google type has no such form.
fn export_of(mime: &str) -> Option<&'static str> {
    match mime {
        DOC => Some(DOCX),
        SHEET => Some(XLSX),
        SLIDES => Some(PPTX),
        _ => None,
    }
}

fn extension(name: &str) -> Option<&str> {
    let (stem, ext) = name.rsplit_once('.')?;
    (!stem.is_empty() && !ext.is_empty() && ext.len() <= EXTENSION_MAX).then_some(ext)
}

/// The type a file opens as, from what it says it is or, when it says nothing,
/// from its name. `None` is a kind no viewer takes.
fn openable(mime: &str, name: &str) -> Option<&'static str> {
    if NO_TYPE.contains(&mime) {
        let ext = extension(name)?.to_lowercase();
        return TYPES
            .iter()
            .find(|(_, exts)| exts.contains(&ext.as_str()))
            .map(|(mime, _)| *mime);
    }
    TYPES
        .iter()
        .find(|(m, _)| *m == mime)
        .map(|(mime, _)| *mime)
}

/// At most [`NAME_MAX`] bytes, cut in the name and not its extension.
fn capped(name: &str) -> String {
    if name.len() <= NAME_MAX {
        return name.to_owned();
    }
    let ext = extension(name).map_or(String::new(), |e| format!(".{e}"));
    let room = NAME_MAX - ext.len();
    let stem = &name[..name.len() - ext.len()];
    let cut = (0..=room)
        .rev()
        .find(|at| stem.is_char_boundary(*at))
        .unwrap_or(0);
    format!("{}{ext}", stem[..cut].trim_end_matches(['.', ' ']))
}

/// What can't be in a name. Control characters and what reverses or hides text
/// are the ones a person can't see, so a name can't pretend with them.
fn unsafe_char(c: char) -> bool {
    c.is_control()
        || matches!(c,
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
            | '\u{200b}'..='\u{200f}' | '\u{2028}'..='\u{202e}'
            | '\u{2060}'..='\u{206f}' | '\u{feff}')
}

/// A name from Drive or a mail, which is a stranger's, as a safe file name: no
/// path separators, no `..`, no leading dot, nothing unseen, and short enough.
pub fn safe_name(name: &str) -> String {
    let mut clean: String = name
        .chars()
        .map(|c| if unsafe_char(c) { '_' } else { c })
        .collect();
    while clean.contains("..") {
        clean = clean.replace("..", ".");
    }
    let clean = clean
        .trim_start_matches(['.', ' ', '_', '-'])
        .trim_end_matches(['.', ' ']);
    let clean = capped(clean);
    if clean.is_empty() {
        "file".to_owned()
    } else {
        clean
    }
}

/// [`safe_name`], ending as `mime`'s files end: what's on disk says what it is,
/// and a Doc's export gets its `.docx`.
fn file_name(name: &str, mime: &str) -> String {
    let name = safe_name(name);
    let Some((_, exts)) = TYPES.iter().find(|(m, _)| *m == mime) else {
        return name;
    };
    let fits = extension(&name).is_some_and(|e| exts.contains(&e.to_lowercase().as_str()));
    if fits {
        name
    } else {
        capped(&format!("{name}.{}", exts[0]))
    }
}

/// `<name> (copy)`, with the extension kept last: what Drive holds is still a
/// .docx, and a name that didn't end so wouldn't open as one.
pub fn copy_name(name: &str) -> String {
    match extension(name) {
        Some(ext) => format!("{} (copy).{ext}", &name[..name.len() - ext.len() - 1]),
        None => format!("{name} (copy)"),
    }
}

/// This install's folder of open files.
pub fn open_root(core: &Core) -> Result<PathBuf, Failure> {
    let key = core
        .key()
        .ok_or_else(|| Failure::Plain("The plugin isn't ready yet.".to_owned()))?;
    core.paths
        .as_ref()
        .map(|paths| paths.open_dir(&key))
        .map_err(|e| Failure::Plain(e.message.clone()))
}

fn folder_of(core: &Core, id: &str) -> Result<PathBuf, Failure> {
    // whatever called this, an id is checked before it's a folder's name
    if !id_ok(id) {
        return Err(Failure::Plain("That isn't a file.".to_owned()));
    }
    Ok(open_root(core)?.join(id))
}

/// `open/<id>/`, private to this user, and really a folder.
fn make_folder(root: &Path, dir: &Path) -> Result<(), Failure> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|_| cant_save())?;
    for place in [root, dir] {
        let real = fs::symlink_metadata(place).map_err(|_| cant_save())?;
        if !real.is_dir() {
            return Err(cant_save());
        }
    }
    Ok(())
}

/// A regular file at `path`, and no link to one: an upload sends what's here.
fn file_here(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_file())
}

fn hash_of(path: &Path) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let (mut hash, mut buffer) = (Sha256::new(), vec![0; 64 * 1024]);
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            return Ok(format!("{:x}", hash.finalize()));
        }
        hash.update(&buffer[..n]);
    }
}

fn path_of(core: &Core, row: &Opened) -> Result<PathBuf, Failure> {
    Ok(folder_of(core, &row.id)?.join(&row.name))
}

/// Whether the file holds what came. Only a hash can say: the viewer's save
/// can leave the size alone, and a reopened file its time. One that's here but
/// can't be read counts as edited: nothing goes on a guess.
fn is_edited(core: &Core, path: &Path, row: &Opened) -> bool {
    file_here(path) && core.hashes.run(path).map_or(true, |hash| hash != row.hash)
}

/// [`is_edited`] for a poll: the file is read only if its stamps moved. They're
/// taken before the read, so a save during it shows next time, and kept only
/// once settled, so a save in the same tick can't hide behind them.
fn seems_edited(core: &Core, path: &Path, row: &Opened) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    let stamp = Stamp::of(&meta);
    if let Some((seen, hash)) = core.hashes.held().get(&row.id)
        && *seen == stamp
    {
        return *hash != row.hash;
    }
    let Ok(hash) = core.hashes.run(path) else {
        return false;
    };
    let edited = hash != row.hash;
    if stamp.settled(core.clock.now_ms()) {
        core.hashes.held().insert(row.id.clone(), (stamp, hash));
    }
    edited
}

/// The whole `open/<id>/`, and the row that goes with it. The row goes first and
/// whatever the folder does: a folder with no row is the sweep's to finish, and
/// a file with its row still looks like work waiting for an answer.
fn remove(core: &Core, id: &str) -> Result<(), Failure> {
    let dir = folder_of(core, id)?;
    let forgotten = with_store(core, |store| store.forget_opened(id));
    core.hashes.held().remove(id);
    match fs::remove_dir_all(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => {
            return Err(Failure::Plain(
                "That file couldn't be removed from this Mac.".to_owned(),
            ));
        }
    }
    forgotten
}

/// The downloaded `.part` as `open/<id>/<name>`, and all that was there before
/// gone. The answer is the real path: the Bot checks it has no link on the way,
/// and a folder reached through one could never pass.
fn land(part: &Path, dir: &Path, name: &str) -> Result<PathBuf, Failure> {
    if Path::new(name).file_name() != Some(OsStr::new(name)) {
        return Err(cant_save());
    }
    let target = dir.join(name);
    fs::rename(part, &target).map_err(|_| cant_save())?;
    for entry in fs::read_dir(dir).map_err(|_| cant_save())?.flatten() {
        if entry.file_name() != OsStr::new(name) {
            let was = entry.path();
            let _ = fs::remove_file(&was).or_else(|_| fs::remove_dir_all(&was));
        }
    }
    let real = fs::canonicalize(&target).map_err(|_| cant_save())?;
    let inside = fs::canonicalize(dir).map_err(|_| cant_save())?;
    if real.parent() != Some(inside.as_path()) {
        let _ = fs::remove_file(&target);
        return Err(cant_save());
    }
    Ok(real)
}

/// The answer that opens the viewer. Only `_meta` has the path, so it stays out
/// of anything a screen or a log gets, and it's all the Bot needs: this doesn't
/// depend on who asked, or on the chat's own way of adding a card.
fn opened(path: &Path, name: &str, mime: &str) -> Value {
    let mut answer = ok(json!({"opened": true, "name": name, "kind": kind_of(mime)}));
    answer["_meta"] = json!({"buwiz/document": {"path": path, "name": name, "mime": mime}});
    answer
}

/// A file already here, opened again. Its 24 hours start over: the person has
/// it in front of them.
fn reopen(core: &Core, row: &Opened) -> Result<Value, Failure> {
    let again = Opened {
        opened_ms: core.clock.now_ms(),
        ..row.clone()
    };
    with_own_store(core, &row.account, |store| store.put_opened(&again))?;
    let path = fs::canonicalize(path_of(core, row)?).map_err(|_| cant_save())?;
    Ok(opened(&path, &row.name, &row.mime))
}

fn moved(row: &Opened, live: &Value) -> bool {
    let text = |key: &str| live.get(key).and_then(Value::as_str).unwrap_or_default();
    text("version") != row.version || text("md5Checksum") != row.md5
}

/// Takes a `.part` that's whole to its place and records it. A sign-out while
/// it came refuses the record, and then nothing stays.
fn keep(core: &Core, dir: &Path, row: &Opened) -> Result<Value, Failure> {
    let done = land(&dir.join(PART), dir, &row.name).and_then(|path| {
        with_own_store(core, &row.account, |store| store.put_opened(row))?;
        Ok(path)
    });
    match done {
        Ok(path) => Ok(opened(&path, &row.name, &row.mime)),
        Err(e) => {
            let _ = remove(core, &row.id);
            Err(e)
        }
    }
}

fn start_part(core: &Core, id: &str) -> Result<PathBuf, Failure> {
    let dir = folder_of(core, id)?;
    make_folder(&open_root(core)?, &dir)?;
    let _ = fs::remove_file(dir.join(PART));
    Ok(dir)
}

/// `gws_file_open`.
fn open(core: &Core, args: &Value) -> Result<Value, Failure> {
    let (auth, account) = signed_in(core)?;
    let id = match drive::id_of(args) {
        Ok(id) if !id.starts_with(ATTACHMENT) => id,
        Ok(_) => return Ok(bad_args("That isn't a file.")),
        Err(answer) => return Ok(answer),
    };
    let Some(_turn) = turn(core, &id) else {
        return Ok(busy());
    };
    let held = with_store(core, |store| store.opened(&id))?;
    // a row is the file's only way of being told from an edit, so one with no
    // file is no copy at all
    let here = match held {
        Some(row) if file_here(&path_of(core, &row)?) => Some(row),
        _ => None,
    };
    // what's edited here stays as it is, whatever Drive holds now: a download
    // would lose the person's work
    if let Some(row) = &here
        && is_edited(core, &path_of(core, row)?, row)
    {
        if row.account != account {
            return Ok(refusal(
                "in_use",
                "Another of your Google accounts has edits to this file here.",
            ));
        }
        return reopen(core, row);
    }
    let mine = here.as_ref().filter(|row| row.account == account);
    let live = match (core.drive.file(&auth, &id, OPEN_FIELDS), mine) {
        (Ok(Answer::Done(v)), _) => v,
        (Ok(answer), _) => return Ok(read_refused(answer)),
        // without a connection, what's here is what there is
        (Err(Failure::Offline(_)), Some(row)) => return reopen(core, row),
        (Err(e), _) => return Err(e),
    };
    if let Some(row) = mine
        && !moved(row, &live)
    {
        return reopen(core, row);
    }
    download(core, &auth, &account, &id, &live)
}

fn download(
    core: &Core,
    auth: &Bearer,
    account: &str,
    id: &str,
    live: &Value,
) -> Result<Value, Failure> {
    let text = |key: &str| live.get(key).and_then(Value::as_str).unwrap_or_default();
    let (mime, title) = (text("mimeType"), text("name"));
    // nothing here shows slides yet, and Google's own page does
    if mime == SLIDES {
        return Ok(ok(
            json!({"refused": "in_browser", "message": "Slides open in the browser for now.", "link": browser_link(id, mime)}),
        ));
    }
    let export = export_of(mime);
    let Some(kind) = export.or_else(|| openable(mime, title)) else {
        return Ok(unsupported());
    };
    if live.pointer("/capabilities/canDownload") == Some(&Value::Bool(false)) {
        return Ok(refusal(
            "no_access",
            "The owner of this file has turned off downloads.",
        ));
    }
    // Google's word for the size only saves a wasted download
    let size = text("size").parse::<u64>().unwrap_or(0);
    if export.is_none() && size > FILE_MAX {
        return Ok(too_big_for(id, mime));
    }
    let dir = start_part(core, id)?;
    let part = dir.join(PART);
    let saved = match export {
        Some(wanted) => core.transfers.save(
            auth,
            &format!("files/{id}/export"),
            &[("mimeType", wanted.to_owned())],
            EXPORT_MAX,
            &part,
        ),
        None => core.transfers.save(
            auth,
            &format!("files/{id}"),
            &[("alt", "media".to_owned())],
            FILE_MAX,
            &part,
        ),
    };
    let hash = match saved {
        Ok(Saved::Whole(hash)) => hash,
        failed => {
            let _ = fs::remove_file(&part);
            let _ = fs::remove_dir(&dir);
            return match failed? {
                Saved::TooBig => Ok(too_big_for(id, mime)),
                Saved::Refused(Answer::Forbidden(reason))
                    if reason == "exportSizeLimitExceeded" =>
                {
                    Ok(too_big_for(id, mime))
                }
                Saved::Refused(answer) => Ok(read_refused(answer)),
                Saved::Whole(_) => unreachable!("taken above"),
            };
        }
    };
    let row = Opened {
        id: id.to_owned(),
        account: account.to_owned(),
        name: file_name(title, kind),
        mime: kind.to_owned(),
        google: export.is_some(),
        attachment: false,
        hash,
        version: text("version").to_owned(),
        md5: text("md5Checksum").to_owned(),
        opened_ms: core.clock.now_ms(),
    };
    keep(core, &dir, &row)
}

/// A part of a mail, as [`gmail::attachments`] names it: `1`, `0.2`, `root`.
pub(crate) fn part_ok(part: &str) -> bool {
    !part.is_empty()
        && part.len() <= PART_ID_MAX
        && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.')
}

/// An attachment id: Gmail's, which can be longer than any file name.
pub(crate) fn attachment_ok(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= ATTACHMENT_ID_MAX
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// An attachment's folder, by its message and part. Gmail's attachment id can't
/// be it: nothing promises that id stays the same from one read to the next, and
/// a new one would be a second folder, with the person's edit left in the first.
/// The part goes in as a hash so that its dots stay out of the name.
fn attachment_folder(message: &str, part: &str) -> String {
    let hash = format!("{:x}", Sha256::digest(part.as_bytes()));
    format!("{ATTACHMENT}{message}-{}", &hash[..32])
}

/// `gws_mail_attachment_open`. The name, the type and the id to fetch by all
/// come from a fresh read of the message, so what lands is that part's. The name
/// is still a stranger's.
fn open_attachment(core: &Core, args: &Value) -> Result<Value, Failure> {
    let (auth, account) = signed_in(core)?;
    let message = js::trim(&js::string_or_empty(args.get("message"))).to_owned();
    let part = js::trim(&js::string_or_empty(args.get("part"))).to_owned();
    if !gmail::id_ok(&message) || !part_ok(&part) {
        return Ok(bad_args("That isn't an attachment."));
    }
    let id = attachment_folder(&message, &part);
    let Some(_turn) = turn(core, &id) else {
        return Ok(busy());
    };
    let held = with_store(core, |store| store.opened(&id))?;
    let here = match held {
        Some(row) if file_here(&path_of(core, &row)?) => Some(row),
        _ => None,
    };
    if let Some(row) = &here {
        if row.account == account {
            return reopen(core, row);
        }
        // a download would land on another account's work
        if is_edited(core, &path_of(core, row)?, row) {
            return Ok(refusal(
                "in_use",
                "Another of your Google accounts has edits to this attachment here.",
            ));
        }
    }
    let gone = || refusal("not_found", "That attachment is gone from the mail.");
    let Some(mail) = core.gmail.full(&auth, &message)? else {
        return Ok(gone());
    };
    let listed = gmail::attachments(&mail);
    let Some(found) = listed
        .iter()
        .find(|a| a.get("part").and_then(Value::as_str) == Some(&part))
    else {
        return Ok(gone());
    };
    let text = |key: &str| found.get(key).and_then(Value::as_str).unwrap_or_default();
    let (title, mime, attachment) = (text("name"), text("mime"), text("id"));
    let Some(kind) = openable(mime, title) else {
        return Ok(unsupported());
    };
    // a small file can come in its own part, with no id to fetch it by
    let bytes = if attachment.is_empty() {
        gmail::part_bytes(&mail, &part)
    } else if attachment_ok(attachment) {
        core.gmail.attachment(&auth, &message, attachment)?
    } else {
        None
    };
    let Some(bytes) = bytes else {
        return Ok(gone());
    };
    let dir = start_part(core, &id)?;
    let write = || -> std::io::Result<()> {
        let mut part = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(dir.join(PART))?;
        part.write_all(&bytes)
    };
    write().map_err(|_| cant_save())?;
    let row = Opened {
        id,
        account,
        name: file_name(title, kind),
        mime: kind.to_owned(),
        google: false,
        attachment: true,
        hash: format!("{:x}", Sha256::digest(&bytes)),
        version: String::new(),
        md5: String::new(),
        opened_ms: core.clock.now_ms(),
    };
    keep(core, &dir, &row)
}

/// `gws_file_edits`: what's here and edited, with whether Drive moved too. Found
/// by the hash, so it's what the file holds that counts. Google not answering
/// says nothing either way, so the file shows as it was: the check that matters
/// comes again at the press. An attachment has nothing on Drive to have moved.
fn edits(core: &Core) -> Result<Value, Failure> {
    sweep(core);
    let Some((auth, account)) = core.accounts.session() else {
        return Ok(ok(json!({"files": []})));
    };
    let rows = with_store(core, Store::all_opened)?;
    let mut files = Vec::new();
    for row in rows.iter().filter(|row| row.account == account) {
        if !seems_edited(core, &path_of(core, row)?, row) {
            continue;
        }
        let changed = !row.attachment
            && match core.drive.file(&auth, &row.id, CHECK_FIELDS) {
                Ok(Answer::Done(live)) => moved(row, &live),
                Ok(_) => true,
                Err(_) => false,
            };
        files.push(json!({
            "id": row.id, "name": row.name, "state": "edited", "google": row.google,
            "attachment": row.attachment, "changed_on_drive": changed,
            "working": core.working.holds(&row.id),
        }));
    }
    Ok(ok(json!({"files": files})))
}

/// `gws_file_upload`. A Google original is refused before anything else
/// happens, and `update` writes only when Drive still holds what came. An
/// attachment takes `copy` alone, which makes it a new file in My Drive.
fn upload(core: &Core, args: &Value) -> Result<Value, Failure> {
    let (auth, account) = signed_in(core)?;
    let id = match drive::id_of(args) {
        Ok(id) => id,
        Err(answer) => return Ok(answer),
    };
    let mode = js::string_or_empty(args.get("mode"));
    if !["update", "copy", "replace"].contains(&mode.as_str()) {
        return Ok(bad_args("That isn't a way to upload."));
    }
    let Some(_turn) = turn(core, &id) else {
        return Ok(busy());
    };
    let Some(row) =
        with_store(core, |store| store.opened(&id))?.filter(|row| row.account == account)
    else {
        return Ok(not_here());
    };
    if row.attachment && mode != "copy" {
        return Ok(refusal(
            "unsupported",
            "An attachment has no file on Google Drive to write over. Save it as a new one instead.",
        ));
    }
    if row.google && mode != "copy" {
        return Ok(refusal(
            "google_original",
            "A Google file is never written over, so nothing was uploaded. Save a copy instead.",
        ));
    }
    let path = path_of(core, &row)?;
    if !file_here(&path) {
        return Ok(not_here());
    }
    if !is_edited(core, &path, &row) {
        return Ok(refusal(
            "not_edited",
            "This file has no changes here, so there's nothing to upload.",
        ));
    }
    let mut file = File::open(&path).map_err(|_| cant_save())?;
    let length = file.metadata().map_err(|_| cant_save())?.len();
    // a save that was cut off mustn't become a new revision
    if length == 0 {
        return Ok(refusal(
            "empty",
            "This file is empty here, so nothing was uploaded.",
        ));
    }
    // an attachment's folder is no Drive id, and there's nothing there to ask
    let live = if row.attachment {
        Answer::NotFound
    } else {
        core.drive.file(&auth, &id, CHECK_FIELDS)?
    };
    let metadata = match (mode.as_str(), live) {
        ("copy", live) => {
            // beside the original, or in My Drive once it's gone
            let parents = match &live {
                Answer::Done(v) => v.get("parents").cloned(),
                _ => None,
            };
            let mut metadata = json!({"name": copy_name(&row.name), "mimeType": row.mime});
            if let Some(parents) = parents {
                metadata["parents"] = parents;
            }
            metadata
        }
        ("update", Answer::Done(v)) if moved(&row, &v) => {
            return Ok(refusal(
                "changed",
                "Google Drive changed this file since you opened it, so nothing was uploaded.",
            ));
        }
        (_, Answer::Done(_)) => json!({}),
        (_, answer) => return Ok(write_refused(answer)),
    };
    let target = (mode != "copy").then_some(id.as_str());
    let place = match core
        .transfers
        .session(&auth, target, &metadata, &row.mime, length, "id")?
    {
        Ok(place) => place,
        Err(answer) => return Ok(write_refused(answer)),
    };
    let done = match core.transfers.send(&place, &mut file, length)? {
        Sent::Done(v) => v,
        Sent::Refused(answer) => return Ok(write_refused(answer)),
    };
    drop(file);
    // Drive has it, and that's the answer. What won't go from here has lost its
    // row, so it can't be sent twice, and the sweep finishes it
    let _ = remove(core, &id);
    // the new revision or copy shows when the changes sync brings it
    core.sync.wake();
    let mut answer = json!({"uploaded": true, "mode": mode});
    // Drive has it either way: an answer it gave that can't be read has no id to pass on
    if let Some(id) = done.get("id").and_then(Value::as_str) {
        answer["id"] = id.into();
    }
    Ok(ok(answer))
}

/// `gws_file_discard`: this Mac's copy goes, and Drive hears nothing of it. Another
/// account can't take it. With nobody signed in there's no other account, and an
/// edit shouldn't have to wait for a sign-in to be thrown away.
fn discard(core: &Core, args: &Value) -> Result<Value, Failure> {
    let account = core.accounts.signed_in().map(|(who, _)| who);
    let id = match drive::id_of(args) {
        Ok(id) => id,
        Err(answer) => return Ok(answer),
    };
    let Some(_turn) = turn(core, &id) else {
        return Ok(busy());
    };
    let held = with_store(core, |store| store.opened(&id))?;
    if !held.is_some_and(|row| account.as_ref().is_none_or(|who| row.account == *who)) {
        return Ok(not_here());
    }
    remove(core, &id)?;
    Ok(ok(json!({"discarded": true})))
}

/// Takes what's had its time: a file past its 24 hours, unless it's edited and
/// unanswered; a row whose file went; a folder nothing here knows of, which is
/// what a crash leaves. What's being worked on now is left alone. Each file is
/// seen to on its own, so one that won't go doesn't keep the rest.
pub fn sweep(core: &Core) {
    let Ok(rows) = with_store(core, Store::all_opened) else {
        return;
    };
    let now = core.clock.now_ms();
    for row in &rows {
        let Ok(path) = path_of(core, row) else {
            continue;
        };
        if core.working.holds(&row.id) {
            continue;
        }
        let over = now - row.opened_ms >= LIFE_MS;
        // the stamps can keep a file and never take one: what would go is read
        let done = !file_here(&path)
            || (over && !seems_edited(core, &path, row) && !is_edited(core, &path, row));
        if done && let Some(_turn) = turn(core, &row.id) {
            let _ = remove(core, &row.id);
        }
    }
    let Ok(root) = open_root(core) else {
        return;
    };
    let Ok(folders) = fs::read_dir(&root) else {
        return;
    };
    for entry in folders.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if rows.iter().any(|row| row.id == name) {
            continue;
        }
        // asked again under its turn: an open may have finished since the list
        // above, and its folder is then no stray
        let Some(_turn) = turn(core, &name) else {
            continue;
        };
        if matches!(with_store(core, |store| store.opened(&name)), Ok(None)) {
            let was = entry.path();
            let _ = fs::remove_dir_all(&was).or_else(|_| fs::remove_file(&was));
        }
    }
}

/// `account` is no longer signed in: what can be fetched again goes with it. A
/// file edited here and unanswered stays, with its row, for that account to
/// come back to.
pub fn forget_account(core: &Core, store: &Store, account: &str) -> gws_store::Result<()> {
    for row in store
        .all_opened()?
        .iter()
        .filter(|row| row.account == account)
    {
        if path_of(core, row).is_ok_and(|path| is_edited(core, &path, row)) {
            continue;
        }
        if let Ok(dir) = folder_of(core, &row.id) {
            let _ = fs::remove_dir_all(dir);
        }
        store.forget_opened(&row.id)?;
    }
    Ok(())
}

/// Runs a file tool; `None` for any other tool.
pub fn run(core: &Core, tool: &str, args: &Value) -> Option<Value> {
    let result = match tool {
        FILE_OPEN => open(core, args),
        FILE_EDITS => edits(core),
        FILE_UPLOAD => upload(core, args),
        FILE_DISCARD => discard(core, args),
        MAIL_ATTACHMENT_OPEN => open_attachment(core, args),
        _ => return None,
    };
    core.sync.seen();
    Some(result.unwrap_or_else(failed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_one_safe_part(name: &str) {
        assert_eq!(
            Path::new(name).file_name(),
            Some(OsStr::new(name)),
            "{name:?}"
        );
        assert!(!name.starts_with('.') && !name.contains(".."), "{name:?}");
        assert!(!name.is_empty() && name.len() <= NAME_MAX, "{name:?}");
        assert!(!name.chars().any(unsafe_char), "{name:?}");
    }

    #[test]
    fn a_hostile_name_becomes_one_safe_part() {
        let long = format!("{}.docx", "é".repeat(500));
        for hostile in [
            "../../x",
            "..",
            ".",
            "",
            " ",
            "/etc/passwd",
            "/",
            "\\..\\..\\windows\\x",
            ".hidden",
            "...hidden",
            "a/../../b",
            "..\u{0}/x",
            "nul\u{0}byte.docx",
            "bell\u{7}\r\n.docx",
            "annexe\u{202e}xcod.exe",
            "\u{feff}.env",
            "file....",
            "- rf",
            long.as_str(),
        ] {
            is_one_safe_part(&safe_name(hostile));
            is_one_safe_part(&file_name(hostile, DOCX));
        }
    }

    #[test]
    fn a_plain_name_stays_as_it_is_and_a_long_one_keeps_its_end() {
        assert_eq!(safe_name("Roof quote v2.docx"), "Roof quote v2.docx");
        assert_eq!(safe_name("Résumé 2026.pdf"), "Résumé 2026.pdf");
        assert_eq!(safe_name("Q3/Q4 plan.docx"), "Q3_Q4 plan.docx");
        assert_eq!(safe_name("../../x"), "x");
        assert_eq!(safe_name(".."), "file");
        let long = safe_name(&format!("{}.xlsx", "a".repeat(500)));
        assert!(long.ends_with(".xlsx") && long.len() <= NAME_MAX, "{long}");
    }

    #[test]
    fn what_is_on_disk_ends_as_its_type_does() {
        assert_eq!(file_name("Notes", DOCX), "Notes.docx");
        assert_eq!(file_name("Notes.docx", DOCX), "Notes.docx");
        assert_eq!(file_name("NOTES.DOCX", DOCX), "NOTES.DOCX");
        assert_eq!(file_name("Budget.2026", XLSX), "Budget.2026.xlsx");
        assert_eq!(file_name("invoice.html", DOCX), "invoice.html.docx");
        assert_eq!(file_name("Photo.jpeg", "image/jpeg"), "Photo.jpeg");
    }

    #[test]
    fn a_copy_keeps_its_extension_last() {
        assert_eq!(copy_name("Budget.xlsx"), "Budget (copy).xlsx");
        assert_eq!(copy_name("a.b.docx"), "a.b (copy).docx");
        assert_eq!(copy_name("Plan"), "Plan (copy)");
    }

    #[test]
    fn only_what_a_viewer_takes_opens_and_silence_goes_by_the_name() {
        assert_eq!(openable(DOCX, "x"), Some(DOCX));
        assert_eq!(openable("application/octet-stream", "x.DOCX"), Some(DOCX));
        assert_eq!(openable("application/pdf", "x.pdf"), None, "the Bot won't");
        assert_eq!(openable("", "x.xlsx"), Some(XLSX));
        assert_eq!(
            openable("application/zip", "x.docx"),
            None,
            "it says what it is"
        );
        assert_eq!(openable("video/mp4", "film.mp4"), None);
        assert_eq!(openable("application/octet-stream", "x.exe"), None);
        assert_eq!(openable("application/vnd.google-apps.form", "x"), None);
    }

    #[test]
    fn an_attachment_folder_is_short_whatever_its_id_and_stays_one_part() {
        let part = "1.".repeat(PART_ID_MAX / 2);
        assert!(part_ok(&part) && part_ok("0.2") && part_ok("root"));
        let folder = attachment_folder(&"m".repeat(64), &part);
        assert!(id_ok(&folder) && folder.len() < 255 && folder.starts_with(ATTACHMENT));
        assert_ne!(folder, attachment_folder(&"m".repeat(64), "1"));
        for bad in ["", "a/b", "a b", &"x".repeat(PART_ID_MAX + 1)] {
            assert!(!part_ok(bad), "{bad}");
        }
        for bad in ["", "a/b", "..", "a b", &"x".repeat(ATTACHMENT_ID_MAX + 1)] {
            assert!(!attachment_ok(bad), "{bad}");
        }
    }

    #[test]
    fn a_link_is_made_here_for_the_kind_of_file() {
        assert_eq!(
            browser_link("a1", DOC),
            "https://docs.google.com/document/d/a1/edit"
        );
        assert_eq!(
            browser_link("a1", SHEET),
            "https://docs.google.com/spreadsheets/d/a1/edit"
        );
        assert_eq!(
            browser_link("a1", SLIDES),
            "https://docs.google.com/presentation/d/a1/edit"
        );
        assert_eq!(
            browser_link("a1", "video/mp4"),
            "https://drive.google.com/file/d/a1/view"
        );
    }

    #[test]
    fn a_files_second_press_waits_for_the_first_to_end() {
        let core = Core::new(
            &crate::apis::Apis::at("http://127.0.0.1:9"),
            Err(gws_store::Error {
                code: "none",
                message: "no store".to_owned(),
            }),
        );
        let first = turn(&core, "a1").unwrap();
        assert!(
            turn(&core, "a1").is_none(),
            "no second copy, no second download"
        );
        assert!(turn(&core, "b2").is_some(), "another file goes on");
        drop(first);
        assert!(turn(&core, "a1").is_some());
    }

    /// Stamps that lie can only keep a file. No save can make them lie from
    /// outside, so this one is told to, and the sweep has to read for itself.
    #[test]
    fn a_stale_hash_can_keep_a_file_but_never_take_one() {
        let dir = std::env::temp_dir().join(format!("gws-stale-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let core = Core::new(
            &crate::apis::Apis::at("http://127.0.0.1:9"),
            Ok(gws_store::Paths::in_dir(&dir)),
        );
        core.unlock("seed");
        let row = Opened {
            id: "a1".to_owned(),
            account: "owner@example.com".to_owned(),
            name: "Plan.docx".to_owned(),
            hash: "what came".to_owned(),
            opened_ms: 1,
            ..Opened::default()
        };
        with_store(&core, |store| store.put_opened(&row)).unwrap();
        let root = open_root(&core).unwrap();
        make_folder(&root, &root.join("a1")).unwrap();
        let path = path_of(&core, &row).unwrap();
        fs::write(&path, b"PK mine").unwrap();
        let stamp = Stamp::of(&fs::metadata(&path).unwrap());
        core.hashes
            .held()
            .insert("a1".to_owned(), (stamp, row.hash.clone()));
        core.clock.set(i64::MAX / 2);
        assert!(!seems_edited(&core, &path, &row), "it was told so");
        sweep(&core);
        assert_eq!(fs::read(&path).unwrap(), b"PK mine");
        let _ = fs::remove_dir_all(&dir);
    }

    /// A download stops at its cap whatever was promised: a real file's cap is
    /// too big to send here, so this is the same code with a smaller one.
    #[test]
    fn a_download_stops_at_its_cap_however_it_was_described() {
        let dir = std::env::temp_dir().join(format!("gws-cap-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let fake = gws_fake_google::serve(|_| (200, "x".repeat(5000)));
        let transfers = Transfers::new(&fake.base);
        let accounts = crate::accounts::Accounts::default();
        accounts.set(&json!({"account": "google", "label": "default", "provider": "google", "subject": "owner@example.com", "scopes": [], "access_token": "t", "token_type": "Bearer", "expires_at": 4_000_000_000u64}));
        let auth = accounts.google().unwrap();
        let saved = transfers.save(&auth, "files/a", &[], 1000, &dir.join("over"));
        assert!(matches!(saved, Ok(Saved::TooBig)));
        let saved = transfers.save(&auth, "files/a", &[], 5000, &dir.join("fits"));
        assert!(matches!(saved, Ok(Saved::Whole(_))));
        assert_eq!(fs::read(dir.join("fits")).unwrap().len(), 5000);
        let _ = fs::remove_dir_all(&dir);
    }
}
