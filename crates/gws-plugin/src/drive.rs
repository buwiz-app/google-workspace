//! Drive: Google's API (`drive/v3`), a changes sync that rides on Mail's, and
//! the tools. It reads and nothing else: this file makes no call that writes.
//! What writes is in `drive/write.rs` and `files.rs`, and only a person's press
//! gets there.
//!
//! A list comes from the store, and reaches Google for the next page when the
//! store runs out, so a lost connection shows the files already seen. A page
//! and the token after it go in together, so a list that stops partway, for
//! whatever reason, picks up where it stopped.
//!
//! A file's name and words are a stranger's. What the model reads carries a
//! warning, and what a screen shows is text or checked image bytes: no HTML
//! and no remote URL ever leaves here.

use std::io::Read as _;
use std::time::Duration;

use base64::Engine as _;
use chrono::DateTime;
use gws_store::drive::{CHANGES_TOKEN, File, RECENT};
use serde_json::{Value, json};

use crate::accounts::Bearer;
use crate::gmail::{BODY_MAX, Failure, HEADER_MAX, cap_bytes};
use crate::mail::{Subject, signed_in, with_own_store, with_store};
use crate::tools::{
    DRIVE_ABOUT, DRIVE_CARD, DRIVE_CHANGES, DRIVE_FILE, DRIVE_LIST, DRIVE_PREVIEW, READ_DOC_TEXT,
    RESOLVE, SEARCH_DRIVE, UNTRUSTED_DRIVE, failed, ok, plain_error,
};
use crate::{Core, calendar, js};

pub mod write;

/// The production API base.
pub const DRIVE_API: &str = "https://www.googleapis.com/drive/v3";
const TIMEOUT: Duration = Duration::from_secs(30);
/// An answer in JSON: a page of a list, with room to spare.
const ANSWER_MAX: u64 = 32 * 1024 * 1024;
/// Rows a list page holds, here and from Google.
const PAGE: i64 = 100;
/// Pages one call may fetch: Google can answer with fewer rows than it was
/// asked for, but a wild `skip` mustn't walk a whole Drive inside one call.
const FILL_MAX: usize = 20;
/// The most rows a search answers.
const SEARCH_MAX: i64 = 50;
/// How many of a page of changes Google gives at once.
const CHANGES_PAGE: &str = "1000";
/// The most folders a path climbs, against a loop.
const PATH_MAX: usize = 32;
/// The most people a file's details list.
const PEOPLE_MAX: usize = 100;
/// How many rows a card shows.
const CARD_ROWS: usize = 5;
/// The most choices `/file` offers while typing.
const CHOICES_MAX: i64 = 8;
/// The most text a preview or a read gives.
const TEXT_MAX: usize = BODY_MAX;
/// The largest image a preview takes.
const IMAGE_MAX: i64 = 8 * 1024 * 1024;
/// What a tool's whole result can hold (protocol 13.2: past 1 MiB the Bot
/// rejects it), less room for the rest of the answer. In base64 this is the
/// real limit on an image today, well under [`IMAGE_MAX`].
const RESULT_MAX: usize = 1024 * 1024 - 1024;
const IMAGE_READ_MAX: u64 = (RESULT_MAX / 4 * 3) as u64;
const SEQ: &str = "drive_seq";
const STATUS: &str = "drive_status";
const ROOT: &str = "drive_root";
const ABOUT: &str = "drive_about";
pub(crate) const FOLDER: &str = "application/vnd.google-apps.folder";
pub(crate) const DOC: &str = "application/vnd.google-apps.document";
pub(crate) const SHEET: &str = "application/vnd.google-apps.spreadsheet";
pub(crate) const SLIDES: &str = "application/vnd.google-apps.presentation";
const TEXT: &str = "text/plain";
const IMAGES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];
/// What a row needs. With no `fields`, Google gives four of these and no more.
const FILE_FIELDS: &str = "id,name,mimeType,owners(displayName),ownedByMe,modifiedTime,size,starred,shared,sharedWithMeTime,trashed,parents,capabilities(canDownload,canRename,canTrash,canMoveItemWithinDrive,canShare,canAddChildren)";
const DETAIL_FIELDS: &str = "createdTime,description,lastModifyingUser(displayName),permissions(type,displayName,emailAddress,role,domain)";
/// Google's word for each thing a screen may offer, and ours.
const CAPABILITIES: [(&str, &str); 5] = [
    ("canRename", "rename"),
    ("canTrash", "trash"),
    ("canMoveItemWithinDrive", "move"),
    ("canShare", "share"),
    ("canAddChildren", "add"),
];
/// Folders first and then by name, in Google's own order across its pages.
const BY_NAME: &str = "folder,name";
const NEWEST: &str = "modifiedTime desc";

/// What Google said to a read.
#[derive(Debug, PartialEq)]
pub enum Answer {
    Done(Value),
    NotFound,
    /// 403 about this file, not the sign-in: the person can't read it, or
    /// Google won't hand it over. With Google's reason.
    Forbidden(String),
    /// 400, with why.
    Invalid(String),
}

/// A download: the bytes, as many as were wanted, or why not.
enum Download {
    Bytes(Vec<u8>),
    Refused(Answer),
}

/// The Drive API at a base URL. Like Gmail's, it follows no redirects, so the
/// token only ever goes to the API base.
#[derive(Clone)]
pub struct DriveApi {
    agent: ureq::Agent,
    base: String,
}

type Response = ureq::http::Response<ureq::Body>;

/// `text` as a string in Drive's `q`: inside one, a quote or a backslash needs
/// a backslash, and nothing else is special. A query goes through here or it
/// could end its string and add terms of its own.
pub fn quoted(text: &str) -> String {
    format!("'{}'", text.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// A Drive id: what the API hands out, so it's safe in a path as it is.
pub(crate) fn id_ok(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// What Google's `q` and order are for a view.
fn listing(view: &str) -> (String, &'static str) {
    match view {
        "my" => ("'root' in parents and trashed = false".to_owned(), BY_NAME),
        "shared" => (
            "sharedWithMe = true and trashed = false".to_owned(),
            BY_NAME,
        ),
        RECENT => ("trashed = false".to_owned(), NEWEST),
        "starred" => ("starred = true and trashed = false".to_owned(), BY_NAME),
        "trash" => ("trashed = true".to_owned(), BY_NAME),
        _ => {
            let folder = view.strip_prefix("folder:").unwrap_or_default();
            let q = format!("{} in parents and trashed = false", quoted(folder));
            (q, BY_NAME)
        }
    }
}

fn reason_of(body: &[u8]) -> (String, String) {
    let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let text = |at: &str| v.pointer(at).and_then(Value::as_str).unwrap_or_default();
    let why = Some(text("/error/message"))
        .filter(|m| !m.is_empty())
        .unwrap_or("no reason given");
    (text("/error/errors/0/reason").to_owned(), why.to_owned())
}

/// What a status that isn't a success means. A 403 can be the sign-in's scope,
/// Google asking us to slow down, or this one file: only the first two say
/// anything about Drive as a whole.
pub(crate) fn refusal_of(status: u16, body: &[u8]) -> Result<Answer, Failure> {
    let (reason, why) = reason_of(body);
    match status {
        400 => Ok(Answer::Invalid(js::slice_utf16(&why, 300))),
        401 => Err(Failure::AccountRequired),
        404 => Ok(Answer::NotFound),
        403 if reason == "insufficientPermissions"
            || why.contains("insufficient authentication") =>
        {
            Err(Failure::NeedsAccess)
        }
        403 if reason.to_lowercase().contains("ratelimit") => Err(busy()),
        403 => Ok(Answer::Forbidden(reason)),
        429 => Err(busy()),
        500.. => Err(Failure::Plain(
            "Google Drive had a problem just now. Try again.".to_owned(),
        )),
        _ => Err(Failure::Plain(format!(
            "Google Drive didn't accept that: {}",
            js::slice_utf16(&why, 300)
        ))),
    }
}

fn busy() -> Failure {
    Failure::Plain("Google Drive is busy right now. Try again in a moment.".to_owned())
}

impl DriveApi {
    /// `base` is [`DRIVE_API`] in production; tests pass a local server.
    pub fn new(base: &str) -> Self {
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .timeout_global(Some(TIMEOUT))
            .user_agent("buwiz-gws")
            .build();
        Self {
            agent: config.new_agent(),
            base: base.trim_end_matches('/').to_owned(),
        }
    }

    fn call(
        &self,
        auth: &Bearer,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<Response, Failure> {
        let mut req = self
            .agent
            .get(format!("{}/{path}", self.base))
            .header("authorization", auth.header());
        for (k, v) in query {
            req = req.query(*k, v);
        }
        req.call().map_err(|e| {
            Failure::Offline(format!(
                "could not reach Google Drive: {}",
                js::slice_utf16(&e.to_string(), 200)
            ))
        })
    }

    fn body(res: &mut Response) -> Result<Vec<u8>, Failure> {
        res.body_mut()
            .with_config()
            .limit(ANSWER_MAX)
            .read_to_vec()
            .map_err(|e| {
                Failure::Plain(format!(
                    "Google Drive's answer could not be read: {}",
                    js::slice_utf16(&e.to_string(), 200)
                ))
            })
    }

    fn get(&self, auth: &Bearer, path: &str, query: &[(&str, String)]) -> Result<Answer, Failure> {
        let mut res = self.call(auth, path, query)?;
        let status = res.status().as_u16();
        let body = Self::body(&mut res)?;
        if status != 200 {
            return refusal_of(status, &body);
        }
        serde_json::from_slice(&body)
            .map(Answer::Done)
            .map_err(|_| Failure::Plain("Google Drive's answer is not JSON".to_owned()))
    }

    /// At most `max` bytes of a file, however long it says it is. A redirect
    /// isn't followed, as the token would go with it: it's a refusal here.
    fn download(
        &self,
        auth: &Bearer,
        path: &str,
        query: &[(&str, String)],
        max: u64,
    ) -> Result<Download, Failure> {
        let mut res = self.call(auth, path, query)?;
        let status = res.status().as_u16();
        if (300..400).contains(&status) {
            return Ok(Download::Refused(Answer::Forbidden(
                "redirected".to_owned(),
            )));
        }
        if status != 200 {
            let body = Self::body(&mut res)?;
            return refusal_of(status, &body).map(Download::Refused);
        }
        let mut bytes = Vec::new();
        res.body_mut()
            .as_reader()
            .take(max)
            .read_to_end(&mut bytes)
            .map_err(|e| {
                Failure::Plain(format!(
                    "Google Drive's answer could not be read: {}",
                    js::slice_utf16(&e.to_string(), 200)
                ))
            })?;
        Ok(Download::Bytes(bytes))
    }

    /// One page of a view, from the page `token` names.
    pub fn list(&self, auth: &Bearer, view: &str, token: Option<&str>) -> Result<Answer, Failure> {
        let (q, order) = listing(view);
        let mut query = vec![
            ("q", q),
            ("orderBy", order.to_owned()),
            ("pageSize", PAGE.to_string()),
            ("fields", format!("nextPageToken,files({FILE_FIELDS})")),
        ];
        query.extend(token.map(|t| ("pageToken", t.to_owned())));
        self.get(auth, "files", &query)
    }

    /// Files holding `words` in their name or text. Google won't sort this.
    pub fn search(&self, auth: &Bearer, words: &str, size: i64) -> Result<Answer, Failure> {
        let q = format!("fullText contains {} and trashed = false", quoted(words));
        self.get(
            auth,
            "files",
            &[
                ("q", q),
                ("pageSize", size.to_string()),
                ("fields", format!("files({FILE_FIELDS})")),
            ],
        )
    }

    pub fn file(&self, auth: &Bearer, id: &str, fields: &str) -> Result<Answer, Failure> {
        self.get(
            auth,
            &format!("files/{id}"),
            &[("fields", fields.to_owned())],
        )
    }

    pub fn about(&self, auth: &Bearer) -> Result<Answer, Failure> {
        let fields = "user(displayName,emailAddress),storageQuota(limit,usage)".to_owned();
        self.get(auth, "about", &[("fields", fields)])
    }

    pub fn start_token(&self, auth: &Bearer) -> Result<Answer, Failure> {
        self.get(auth, "changes/startPageToken", &[])
    }

    pub fn changes(&self, auth: &Bearer, token: &str) -> Result<Answer, Failure> {
        self.get(
            auth,
            "changes",
            &[
                ("pageToken", token.to_owned()),
                ("pageSize", CHANGES_PAGE.to_owned()),
                ("includeRemoved", "true".to_owned()),
                (
                    "fields",
                    format!(
                        "nextPageToken,newStartPageToken,changes(fileId,removed,file({FILE_FIELDS}))"
                    ),
                ),
            ],
        )
    }

    fn media(&self, auth: &Bearer, id: &str, max: u64) -> Result<Download, Failure> {
        let query = [("alt", "media".to_owned())];
        self.download(auth, &format!("files/{id}"), &query, max)
    }

    fn export(&self, auth: &Bearer, id: &str, mime: &str, max: u64) -> Result<Download, Failure> {
        let query = [("mimeType", mime.to_owned())];
        self.download(auth, &format!("files/{id}/export"), &query, max)
    }
}

fn text(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn flag(v: &Value, key: &str) -> bool {
    v.get(key) == Some(&Value::Bool(true))
}

/// What a file is, for the screen's icon and for what can be done with it.
pub(crate) fn kind_of(mime: &str) -> &'static str {
    match mime {
        FOLDER => "folder",
        DOC => "doc",
        SHEET => "sheet",
        SLIDES => "slides",
        "application/vnd.ms-powerpoint"
        | "application/vnd.openxmlformats-officedocument.presentationml.presentation" => "slides",
        "application/pdf" => "pdf",
        "application/msword"
        | "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => "word",
        "application/vnd.ms-excel"
        | "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => "spreadsheet",
        m if m.starts_with("image/") => "image",
        _ => "file",
    }
}

/// A stored row from Google's resource.
pub fn file_of(v: &Value) -> Option<File> {
    let id = v.get("id")?.as_str()?.to_owned();
    if !id_ok(&id) {
        return None;
    }
    let mime = text(v, "mimeType");
    let changed = text(v, "modifiedTime");
    let owner = v
        .pointer("/owners/0")
        .map_or(String::new(), |o| text(o, "displayName"));
    Some(File {
        id,
        name: cap_bytes(&text(v, "name"), HEADER_MAX),
        kind: kind_of(&mime).to_owned(),
        owner: cap_bytes(&owner, HEADER_MAX),
        mine: flag(v, "ownedByMe"),
        changed_ms: DateTime::parse_from_rfc3339(&changed).map_or(0, |t| t.timestamp_millis()),
        changed,
        // a size comes as a string, and Google's own files have none
        size: v
            .get("size")
            .and_then(Value::as_str)
            .and_then(|s| s.parse().ok()),
        starred: flag(v, "starred"),
        shared: flag(v, "shared"),
        with_me: v.get("sharedWithMeTime").is_some(),
        trashed: flag(v, "trashed"),
        parents: v
            .get("parents")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|p| p.as_str().filter(|p| id_ok(p)).map(str::to_owned))
            .collect(),
        can: v.get("capabilities").map(|c| {
            CAPABILITIES
                .iter()
                .filter(|(google, _)| flag(c, google))
                .map(|(_, ours)| (*ours).to_owned())
                .collect()
        }),
        mime,
    })
}

fn files_of(v: &Value) -> Vec<File> {
    v.get("files")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(file_of)
        .collect()
}

fn row_json(f: &File) -> Value {
    json!({"id": f.id, "name": f.name, "kind": f.kind, "mime": f.mime, "owner": f.owner, "mine": f.mine, "changed": f.changed, "size": f.size, "starred": f.starred, "shared": f.shared, "trashed": f.trashed, "can": f.can, "parents": f.parents})
}

fn rows(files: &[File]) -> Vec<Value> {
    files.iter().map(row_json).collect()
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

fn no_access() -> Value {
    refusal("no_access", "This account can't read that in Google Drive.")
}

/// `{kind: "none"}`: what a preview says instead of showing something.
fn none(why: &str) -> Value {
    ok(json!({"kind": "none", "why": why}))
}

fn bump(core: &Core, account: &str) -> Result<(), Failure> {
    with_own_store(core, account, |store| {
        let next = store
            .state(account, SEQ)?
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0)
            + 1;
        store.set_state(account, SEQ, &next.to_string())
    })
}

/// Anything but a plain answer to a read the sync needs.
fn unexpected(what: &str) -> Failure {
    Failure::Plain(format!("Google Drive wouldn't give {what}."))
}

/// My Drive's id, which a change needs to know what's in it.
fn root_of(core: &Core, auth: &Bearer, account: &str) -> Result<String, Failure> {
    if let Some(root) = with_store(core, |store| store.state(account, ROOT))? {
        return Ok(root);
    }
    let Answer::Done(v) = core.drive.file(auth, "root", "id")? else {
        return Err(unexpected("its root folder"));
    };
    let root = text(&v, "id");
    with_own_store(core, account, |store| store.set_state(account, ROOT, &root))?;
    Ok(root)
}

/// Reads a list on until it holds `want` rows or has no more. `Some` is
/// Google refusing the next page.
fn fill(
    core: &Core,
    auth: &Bearer,
    account: &str,
    view: &str,
    want: i64,
) -> Result<Option<Answer>, Failure> {
    for _ in 0..FILL_MAX {
        let (next, total) = with_store(core, |store| {
            Ok((
                store.view_next(account, view)?,
                store.view_total(account, view)?,
            ))
        })?;
        if next.as_deref() == Some("") || total >= want {
            return Ok(None);
        }
        match core.drive.list(auth, view, next.as_deref())? {
            Answer::Done(v) => {
                let token = text(&v, "nextPageToken");
                with_own_store(core, account, |store| {
                    store.put_page(account, view, &files_of(&v), &token)
                })?;
            }
            refused => return Ok(Some(refused)),
        }
    }
    Ok(None)
}

/// One page of changes applied. The next token, and whether it's the last.
fn apply_page(
    core: &Core,
    account: &str,
    root: &str,
    page: &Value,
    moved: &mut bool,
) -> Result<(String, bool), Failure> {
    let (mut changed, mut gone) = (Vec::new(), Vec::new());
    for change in page
        .get("changes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let id = text(change, "fileId");
        if flag(change, "removed") {
            gone.push(id);
        } else if let Some(file) = change.get("file").and_then(file_of) {
            changed.push(file);
        }
    }
    let next = text(page, "nextPageToken");
    let last = next.is_empty();
    let token = if last {
        text(page, "newStartPageToken")
    } else {
        next
    };
    if token.is_empty() {
        return Err(unexpected("the next page of its changes"));
    }
    *moved |= with_own_store(core, account, |store| {
        store.apply_changes(account, root, &changed, &gone, &token)
    })?;
    Ok((token, last))
}

/// The first sync: the token first, so nothing that changes during the reads
/// below is missed, and kept last, so a read that fails is tried again. Recent
/// and Starred are kept from the start.
fn start(core: &Core, auth: &Bearer, account: &str) -> Result<(), Failure> {
    let Answer::Done(v) = core.drive.start_token(auth)? else {
        return Err(unexpected("a place to start"));
    };
    for view in [RECENT, "starred"] {
        if fill(core, auth, account, view, PAGE)?.is_some() {
            return Err(unexpected("its files"));
        }
    }
    with_own_store(core, account, |store| {
        store.set_state(account, CHANGES_TOKEN, &text(&v, "startPageToken"))
    })
}

fn sync_once(core: &Core, auth: &Bearer, account: &str, moved: &mut bool) -> Result<(), Failure> {
    let root = root_of(core, auth, account)?;
    let held = with_store(core, |store| store.state(account, CHANGES_TOKEN))?;
    let Some(mut token) = held else {
        start(core, auth, account)?;
        *moved = true;
        return Ok(());
    };
    // each page goes in with its token: what came in stays, and a page that
    // fails is asked for again from the same place
    loop {
        let Answer::Done(page) = core.drive.changes(auth, &token)? else {
            return Err(unexpected("its changes"));
        };
        let (next, last) = apply_page(core, account, &root, &page, moved)?;
        if last {
            return Ok(());
        }
        token = next;
    }
}

/// The Drive side of a sync, its outcome kept apart from Mail's and Calendar's:
/// a sign-in that doesn't cover Drive leaves them as they were.
pub fn sync(core: &Core, auth: &Bearer, account: &str) -> Result<(), Failure> {
    let _one_at_a_time = core.drive_sync.lock().unwrap_or_else(|e| e.into_inner());
    let mut moved = false;
    let result = sync_once(core, auth, account, &mut moved);
    let status = match &result {
        Ok(()) => "ok",
        Err(Failure::Offline(_)) => "offline",
        Err(Failure::NeedsAccess) => "needs_access",
        Err(Failure::AccountRequired) => return Err(Failure::AccountRequired),
        Err(Failure::Plain(_)) => "failed",
    };
    with_own_store(core, account, |store| {
        store.set_state(account, STATUS, status)
    })?;
    // what did come in has to show, even if the sync went no further
    if moved {
        bump(core, account)?;
    }
    result
}

/// The very first read does its own sync, so the token is taken before any
/// list is, and a change made after that list isn't missed.
fn first_sync(core: &Core, auth: &Bearer, account: &str) -> Result<(), Failure> {
    let status = with_store(core, |store| store.state(account, STATUS))?;
    if status.is_none()
        && let Err(Failure::AccountRequired) = sync(core, auth, account)
    {
        return Err(Failure::AccountRequired);
    }
    Ok(())
}

/// A file from the store, or else from Google, which is then kept. `None` when
/// it can't be had.
fn known_file(core: &Core, auth: &Bearer, account: &str, id: &str, online: bool) -> Option<File> {
    if let Ok(Some(file)) = with_store(core, |store| store.drive_file(account, id)) {
        return Some(file);
    }
    if !online {
        return None;
    }
    let Ok(Answer::Done(v)) = core.drive.file(auth, id, FILE_FIELDS) else {
        return None;
    };
    let file = file_of(&v)?;
    let _ = with_own_store(core, account, |store| {
        store.put_files(account, std::slice::from_ref(&file))
    });
    Some(file)
}

/// The way down to `folder`, from just under the view's root and ending with
/// it. It stops at a parent that can't be read: for a shared folder that's
/// usually the top of what was shared.
fn path_to(core: &Core, auth: &Bearer, account: &str, folder: &str, online: bool) -> Vec<Value> {
    let root = if online {
        root_of(core, auth, account).ok()
    } else {
        with_store(core, |store| store.state(account, ROOT))
            .ok()
            .flatten()
    };
    let (mut path, mut at) = (Vec::new(), Some(folder.to_owned()));
    for _ in 0..PATH_MAX {
        let Some(id) = at.take().filter(|id| Some(id) != root.as_ref()) else {
            break;
        };
        let Some(file) = known_file(core, auth, account, &id, online) else {
            break;
        };
        path.push(json!({"id": file.id, "name": file.name}));
        at = file.parents.first().cloned();
    }
    path.reverse();
    path
}

/// Where a list is, as a file's `parents` would say it, and whether a new file
/// can go there. A view with no one folder has neither; a folder that says
/// nothing about it is left for Google to answer.
fn place_of(core: &Core, account: &str, view: &str) -> Result<(Value, bool), Failure> {
    if view == "my" {
        let root = with_store(core, |store| store.state(account, ROOT))?;
        return Ok((json!(root), true));
    }
    let Some(folder) = view.strip_prefix("folder:") else {
        return Ok((Value::Null, false));
    };
    let held = with_store(core, |store| store.drive_file(account, folder))?;
    let adds = held
        .and_then(|f| f.can)
        .is_none_or(|can| can.iter().any(|c| c == "add"));
    Ok((json!(folder), adds))
}

/// The store's name for what's asked; `Err` says why it can't be.
fn view_of(args: &Value) -> Result<String, &'static str> {
    let view = js::string_or_empty(args.get("view"));
    let folder = js::string_or_empty(args.get("folder"));
    if !["my", "shared", RECENT, "starred", "trash"].contains(&view.as_str()) {
        return Err("That isn't a view of Drive.");
    }
    if folder.is_empty() {
        return Ok(view);
    }
    if !matches!(view.as_str(), "my" | "shared") {
        return Err("Only My Drive and Shared with me open a folder.");
    }
    if !id_ok(&folder) {
        return Err("That isn't a folder.");
    }
    Ok(format!("folder:{folder}"))
}

/// Why a list stopped, as the screen hears it. Past the first page the person
/// has rows on screen, and a refusal is an answer: an error would lose its code
/// on the way, and the rows stay. What was stored stays too, and the page token
/// hasn't moved, so a later try carries on.
fn stopped(failure: Result<Answer, Failure>, skip: i64) -> Result<Value, Failure> {
    let later = skip > 0;
    match failure {
        Ok(Answer::NotFound) => Ok(refusal(
            "not_found",
            "That folder is gone from Google Drive.",
        )),
        Ok(Answer::Forbidden(_)) | Err(Failure::NeedsAccess) if later => Ok(refusal(
            "no_access",
            "Google Drive stopped this list partway: this account can't read it any more.",
        )),
        Ok(Answer::Forbidden(_)) => Ok(no_access()),
        Err(Failure::AccountRequired) if later => Ok(refusal(
            "sign_in",
            "Google signed this account out partway through the list. Sign in again to see the rest.",
        )),
        Ok(Answer::Invalid(why)) => Ok(refusal("invalid_args", &why)),
        Ok(Answer::Done(_)) => Err(unexpected("that list")),
        Err(Failure::Plain(why)) if later => Ok(refusal("failed", &why)),
        Err(e) => Err(e),
    }
}

/// A view or a folder: the page at `skip`, reaching Google when the store runs
/// out. If Google stops answering, whatever's stored for this page still comes,
/// and the refusal waits for the page where nothing is left to show.
fn list(core: &Core, args: &Value) -> Result<Value, Failure> {
    let (auth, account) = signed_in(core)?;
    let skip = args.get("skip").and_then(Value::as_i64).unwrap_or(0).max(0);
    let ticket = js::string_or_empty(args.get("search"));
    if !ticket.is_empty() {
        return list_search(core, &auth, &account, &ticket, skip);
    }
    let view = match view_of(args) {
        Ok(view) => view,
        Err(why) => return Ok(bad_args(why)),
    };
    first_sync(core, &auth, &account)?;
    let filled = fill(core, &auth, &account, &view, skip + PAGE);
    // Google refuses an old page token: the list starts over when it's next
    // opened, as its rows can't be carried on from
    if matches!(filled, Ok(Some(Answer::Invalid(_))))
        && with_store(core, |store| store.view_next(&account, &view))?.is_some()
    {
        with_own_store(core, &account, |store| store.forget_view(&account, &view))?;
        return Ok(refusal(
            "stale",
            "This list is out of date. Open it again to see it fresh.",
        ));
    }
    let status = with_store(core, |store| store.state(&account, STATUS))?;
    let (saved, failure) = match filled {
        Ok(None) => (status.as_deref() == Some("offline"), None),
        Err(Failure::Offline(_)) => (true, None),
        Ok(Some(refused)) => (false, Some(Ok(refused))),
        Err(e) => (false, Some(Err(e))),
    };
    let (files, total, next) = with_store(core, |store| {
        Ok((
            store.view_files(&account, &view, skip, PAGE)?,
            store.view_total(&account, &view)?,
            store.view_next(&account, &view)?,
        ))
    })?;
    if let Some(failure) = failure
        && files.is_empty()
    {
        return stopped(failure, skip);
    }
    let held = skip + (files.len() as i64) < total;
    // offline only what's stored can come; online, no token on record yet is
    // not the end, only an empty one is
    let more = held || (!saved && next.as_deref() != Some(""));
    let path = match view.strip_prefix("folder:") {
        Some(folder) => path_to(core, &auth, &account, folder, !saved),
        None => Vec::new(),
    };
    let (at, can_add) = place_of(core, &account, &view)?;
    Ok(ok(
        json!({"files": rows(&files), "more": more, "saved": saved, "path": path, "at": at, "can_add": can_add}),
    ))
}

fn searched(core: &Core, ticket: &str) -> Option<String> {
    match core
        .tickets
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .subject(ticket)
    {
        Some(Subject::DriveSearch(words)) => Some(words.clone()),
        _ => None,
    }
}

/// A search on Google, or among stored names without a connection. True if it
/// came from the store.
fn find(
    core: &Core,
    auth: &Bearer,
    account: &str,
    words: &str,
    size: i64,
) -> Result<(Vec<File>, bool), Failure> {
    match core.drive.search(auth, words, size) {
        Ok(Answer::Done(v)) => {
            let files = files_of(&v);
            with_own_store(core, account, |store| store.put_files(account, &files))?;
            Ok((files, false))
        }
        Ok(_) => Err(Failure::Plain(
            "Google Drive wouldn't run that search.".to_owned(),
        )),
        Err(Failure::Offline(_)) => {
            let files = with_store(core, |store| store.search_files(account, words, size))?;
            Ok((files, true))
        }
        Err(e) => Err(e),
    }
}

/// The full screen, opened on a card's search: one page, which Google's
/// search isn't asked to go past.
fn list_search(
    core: &Core,
    auth: &Bearer,
    account: &str,
    ticket: &str,
    skip: i64,
) -> Result<Value, Failure> {
    let Some(words) = searched(core, ticket) else {
        return Ok(refusal("not_found", "This search is out of date."));
    };
    let (files, saved) = find(core, auth, account, &words, PAGE)?;
    let page = files.get(skip as usize..).unwrap_or_default();
    Ok(ok(
        json!({"files": rows(page), "more": false, "saved": saved, "path": []}),
    ))
}

/// A card's rows, by its ticket.
fn card(core: &Core, args: &Value) -> Result<Value, Failure> {
    let (auth, account) = signed_in(core)?;
    let Some(words) = searched(core, &js::string_or_empty(args.get("ticket"))) else {
        return Ok(refusal("not_found", "This card is out of date."));
    };
    let (files, saved) = find(core, &auth, &account, &words, CARD_ROWS as i64 + 1)?;
    let shown = &files[..files.len().min(CARD_ROWS)];
    Ok(ok(
        json!({"rows": rows(shown), "more": files.len() > CARD_ROWS, "saved": saved}),
    ))
}

fn search(core: &Core, args: &Value) -> Result<Value, Failure> {
    let words = js::trim(&js::string_or_empty(args.get("query"))).to_owned();
    if words.is_empty() {
        return Ok(plain_error("gws_search_drive needs a query"));
    }
    let (auth, account) = signed_in(core)?;
    let (files, _) = find(core, &auth, &account, &words, SEARCH_MAX)?;
    Ok(ok(
        json!({"items": rows(&files), "untrusted": UNTRUSTED_DRIVE}),
    ))
}

fn about(core: &Core) -> Result<Value, Failure> {
    let (auth, account) = signed_in(core)?;
    let number = |v: &Value, at: &str| {
        v.pointer(at)
            .and_then(Value::as_str)
            .and_then(|n| n.parse::<i64>().ok())
    };
    match core.drive.about(&auth) {
        Ok(Answer::Done(v)) => {
            let user = v.get("user").unwrap_or(&Value::Null);
            let name = Some(text(user, "displayName")).filter(|n| !n.is_empty());
            let answer = json!({"used": number(&v, "/storageQuota/usage").unwrap_or(0), "limit": number(&v, "/storageQuota/limit"), "user": name.unwrap_or_else(|| text(user, "emailAddress"))});
            with_own_store(core, &account, |store| {
                store.set_state(&account, ABOUT, &answer.to_string())
            })?;
            Ok(ok(answer))
        }
        Ok(_) => Err(unexpected("your storage")),
        Err(e @ Failure::Offline(_)) => {
            let held = with_store(core, |store| store.state(&account, ABOUT))?;
            match held.and_then(|h| serde_json::from_str::<Value>(&h).ok()) {
                Some(answer) => Ok(ok(answer)),
                None => Err(e),
            }
        }
        Err(e) => Err(e),
    }
}

fn changes(core: &Core) -> Result<Value, Failure> {
    let (_, account) = signed_in(core)?;
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
        json!({"seq": seq, "status": status.unwrap_or_else(|| "syncing".to_owned())}),
    ))
}

/// Who a permission is for, in words a person reads.
fn person_of(p: &Value) -> Value {
    let name = match text(p, "type").as_str() {
        "anyone" => "Anyone with the link".to_owned(),
        "domain" => text(p, "domain"),
        _ => text(p, "displayName"),
    };
    json!({"name": cap_bytes(&name, HEADER_MAX), "email": text(p, "emailAddress"), "role": text(p, "role")})
}

pub(crate) fn id_of(args: &Value) -> Result<String, Value> {
    let id = js::trim(&js::string_or_empty(args.get("id"))).to_owned();
    if id_ok(&id) {
        Ok(id)
    } else {
        Err(bad_args("That isn't a file."))
    }
}

fn file(core: &Core, args: &Value) -> Result<Value, Failure> {
    let (auth, account) = signed_in(core)?;
    let id = match id_of(args) {
        Ok(id) => id,
        Err(answer) => return Ok(answer),
    };
    let fields = format!("{FILE_FIELDS},{DETAIL_FIELDS}");
    let v = match core.drive.file(&auth, &id, &fields) {
        Ok(Answer::Done(v)) => v,
        Ok(Answer::NotFound) => return Ok(gone()),
        Ok(Answer::Forbidden(_)) => return Ok(no_access()),
        Ok(Answer::Invalid(why)) => return Ok(bad_args(&why)),
        // offline, what the row knows is all there is
        Err(e @ Failure::Offline(_)) => {
            let Some(row) = with_store(core, |store| store.drive_file(&account, &id))? else {
                return Err(e);
            };
            return Ok(ok(detail(core, &auth, &account, &row, &Value::Null, false)));
        }
        Err(e) => return Err(e),
    };
    let Some(row) = file_of(&v) else {
        return Err(unexpected("that file"));
    };
    Ok(ok(detail(core, &auth, &account, &row, &v, true)))
}

fn detail(core: &Core, auth: &Bearer, account: &str, row: &File, v: &Value, online: bool) -> Value {
    let mut answer = row_json(row);
    let path = match row.parents.first() {
        Some(parent) => path_to(core, auth, account, parent, online),
        None => Vec::new(),
    };
    let people: Vec<Value> = v
        .get("permissions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(PEOPLE_MAX)
        .map(person_of)
        .collect();
    answer["path"] = json!(path);
    answer["modified_by"] = json!(
        v.pointer("/lastModifyingUser/displayName")
            .and_then(Value::as_str)
            .unwrap_or_default()
    );
    answer["created"] = json!(text(v, "createdTime"));
    answer["description"] = json!(calendar::notes_of(v));
    answer["people"] = json!(people);
    answer["can_download"] =
        json!(v.pointer("/capabilities/canDownload") == Some(&Value::Bool(true)));
    answer
}

/// The first image bytes really being what the file says it is, since what a
/// file says is its uploader's word.
fn image_is(mime: &str, bytes: &[u8]) -> bool {
    match mime {
        "image/png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "image/jpeg" => bytes.starts_with(&[0xff, 0xd8, 0xff]),
        "image/gif" => bytes.starts_with(b"GIF8"),
        "image/webp" => bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP"),
        _ => false,
    }
}

/// The plain export of a Google file: never HTML, whatever else Google offers.
fn export_type(mime: &str) -> Option<&'static str> {
    match mime {
        DOC | SLIDES => Some(TEXT),
        SHEET => Some("text/csv"),
        _ => None,
    }
}

/// A file's words, up to the cap and marked when cut. A bit past the cap is
/// read, so a cut can be told from a file that just fits.
fn read_text(core: &Core, auth: &Bearer, row: &File) -> Result<Download, Failure> {
    let max = TEXT_MAX as u64 + 4;
    let got = match export_type(&row.mime) {
        Some(mime) => core.drive.export(auth, &row.id, mime, max)?,
        None => core.drive.media(auth, &row.id, max)?,
    };
    Ok(match got {
        Download::Bytes(bytes) => {
            let words = String::from_utf8_lossy(&bytes);
            let words = words.trim_start_matches('\u{feff}');
            Download::Bytes(cap_bytes(words, TEXT_MAX).into_bytes())
        }
        refused => refused,
    })
}

/// A row as Google has it now, or the answer to give instead.
fn live_row(core: &Core, auth: &Bearer, id: &str) -> Result<Result<File, Value>, Failure> {
    Ok(match core.drive.file(auth, id, FILE_FIELDS)? {
        Answer::Done(v) => file_of(&v).ok_or_else(gone),
        Answer::NotFound => Err(gone()),
        Answer::Forbidden(_) => Err(no_access()),
        Answer::Invalid(why) => Err(bad_args(&why)),
    })
}

/// Why a read that Google refused shows nothing.
fn why_not(answer: &Answer) -> &'static str {
    match answer {
        Answer::Forbidden(reason) if reason == "exportSizeLimitExceeded" => {
            "This file is too big to preview."
        }
        Answer::NotFound => "This file is gone from Google Drive.",
        Answer::Forbidden(_) => "Google Drive won't hand over this file for a preview.",
        _ => "This file can't be previewed.",
    }
}

fn preview_image(core: &Core, auth: &Bearer, row: &File) -> Result<Value, Failure> {
    const TOO_BIG: &str = "This image is too big to preview here.";
    let size = row.size.unwrap_or(0);
    if size > IMAGE_MAX || size as u64 > IMAGE_READ_MAX {
        return Ok(none(TOO_BIG));
    }
    // a size is only what Google was told, so the read has its own stop
    let bytes = match core.drive.media(auth, &row.id, IMAGE_READ_MAX + 1)? {
        Download::Bytes(bytes) => bytes,
        Download::Refused(answer) => return Ok(none(why_not(&answer))),
    };
    if bytes.len() as u64 > IMAGE_READ_MAX {
        return Ok(none(TOO_BIG));
    }
    if !image_is(&row.mime, &bytes) {
        return Ok(none("This file isn't the image it says it is."));
    }
    let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
    let data_url = format!("data:{};base64,{data}", row.mime);
    // not `ok`: it would send the image twice, and a result has a limit
    Ok(
        json!({"content": [{"type": "text", "text": "An image."}], "structuredContent": {"kind": "image", "data_url": data_url}}),
    )
}

fn preview(core: &Core, args: &Value) -> Result<Value, Failure> {
    let (auth, _) = signed_in(core)?;
    let id = match id_of(args) {
        Ok(id) => id,
        Err(answer) => return Ok(answer),
    };
    let row = match live_row(core, &auth, &id)? {
        Ok(row) => row,
        Err(answer) => return Ok(answer),
    };
    if IMAGES.contains(&row.mime.as_str()) {
        return preview_image(core, &auth, &row);
    }
    if export_type(&row.mime).is_none() && row.mime != TEXT {
        return Ok(none("This kind of file has no preview."));
    }
    if row.size.is_some_and(|size| size as u64 > TEXT_MAX as u64) {
        return Ok(none("This file is too long to preview."));
    }
    Ok(match read_text(core, &auth, &row)? {
        Download::Bytes(bytes) => {
            ok(json!({"kind": "text", "text": String::from_utf8_lossy(&bytes)}))
        }
        Download::Refused(answer) => none(why_not(&answer)),
    })
}

/// For the model: a Doc's or text file's words, marked as a stranger's.
fn read_doc(core: &Core, args: &Value) -> Result<Value, Failure> {
    let id = js::trim(&js::string_or_empty(args.get("id"))).to_owned();
    if id.is_empty() {
        return Ok(plain_error("gws_read_doc_text needs an id"));
    }
    if !id_ok(&id) {
        return Ok(bad_args("That isn't a file."));
    }
    let (auth, _) = signed_in(core)?;
    let row = match live_row(core, &auth, &id)? {
        Ok(row) => row,
        Err(answer) => return Ok(answer),
    };
    if row.mime != DOC && row.mime != TEXT {
        return Ok(refusal(
            "unsupported",
            "Only Google Docs and text files can be read this way.",
        ));
    }
    Ok(match read_text(core, &auth, &row)? {
        Download::Bytes(bytes) => ok(
            json!({"id": row.id, "name": row.name, "text": String::from_utf8_lossy(&bytes), "untrusted": UNTRUSTED_DRIVE}),
        ),
        Download::Refused(Answer::NotFound) => gone(),
        Download::Refused(_) => no_access(),
    })
}

/// Offers the `files` card with a chat's search. Nothing about the files goes
/// in it: props hold a ticket, and the card reads its rows itself.
pub fn offer_card(core: &Core, tool: &str, args: &Value, result: &mut Value) {
    let found = result
        .pointer("/structuredContent/items")
        .and_then(Value::as_array)
        .is_some_and(|i| !i.is_empty());
    if tool != SEARCH_DRIVE || !found {
        return;
    }
    let words = js::trim(&js::string_or_empty(args.get("query"))).to_owned();
    let ticket = core
        .tickets
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .issue(&Subject::DriveSearch(words));
    result["_meta"] = json!({"buwiz/open": {"app": "drive", "props": {"search": ticket}, "title": "Drive", "placement": "inline"}});
}

/// `/file [name]` and `@gws:file:`. While it's typed, the stored files whose
/// name matches, each as a ticket: resolving runs on the server's own loop, so
/// it asks no Google, and what replaces the typed text is never a name. Said,
/// it opens Drive on that search, and no model hears of it. The title stays
/// "Drive", since titles reach the model.
fn resolve_file(core: &Core, args: &Value) -> Value {
    let value = js::trim(&js::string_or_empty(args.get("value"))).to_owned();
    if args.get("complete") == Some(&Value::Bool(true)) {
        let found = core
            .accounts
            .session()
            .filter(|_| !value.is_empty())
            .and_then(|(_, account)| {
                with_store(core, |s| s.search_files(&account, &value, CHOICES_MAX)).ok()
            })
            .unwrap_or_default();
        let mut tickets = core.tickets.lock().unwrap_or_else(|e| e.into_inner());
        let choices: Vec<Value> = found
            .iter()
            .map(|f| json!({"label": f.name, "value": tickets.issue(&Subject::DriveSearch(f.name.clone()))}))
            .collect();
        return ok(json!({"result": "choices", "choices": choices}));
    }
    if value.is_empty() {
        return ok(
            json!({"result": "open", "app": "drive", "title": "Drive", "placement": "full", "props": {"view": "my"}}),
        );
    }
    let mut tickets = core.tickets.lock().unwrap_or_else(|e| e.into_inner());
    // a choice comes back as its ticket
    let ticket = match tickets.subject(&value) {
        Some(Subject::DriveSearch(_)) => value,
        _ => tickets.issue(&Subject::DriveSearch(value)),
    };
    ok(
        json!({"result": "open", "app": "drive", "title": "Drive", "placement": "full", "props": {"search": ticket}}),
    )
}

/// Runs a Drive tool; `None` for any other tool. Every call says a screen is
/// open, so the sync keeps up with it.
pub fn run(core: &Core, tool: &str, args: &Value) -> Option<Value> {
    if tool == RESOLVE && args.get("name").and_then(Value::as_str) == Some("file") {
        return Some(resolve_file(core, args));
    }
    let result = match tool {
        DRIVE_LIST => list(core, args),
        DRIVE_ABOUT => about(core),
        DRIVE_CHANGES => changes(core),
        DRIVE_FILE => file(core, args),
        DRIVE_PREVIEW => preview(core, args),
        DRIVE_CARD => card(core, args),
        SEARCH_DRIVE => search(core, args),
        READ_DOC_TEXT => read_doc(core, args),
        _ => return None,
    };
    core.sync.seen();
    Some(result.unwrap_or_else(failed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_and_backslashes_stay_inside_the_string() {
        assert_eq!(quoted("plain"), "'plain'");
        assert_eq!(quoted("it's"), r"'it\'s'");
        assert_eq!(quoted(r"a\b"), r"'a\\b'");
        // a backslash can't be used to cancel the escape that follows it
        assert_eq!(quoted(r"\'"), r"'\\\''");
    }

    #[test]
    fn an_id_that_isnt_drives_never_reaches_a_path_or_a_query() {
        for bad in [
            "",
            "..",
            "a/b",
            "a b",
            "a'b",
            "a?alt=media",
            &"x".repeat(257),
        ] {
            assert!(!id_ok(bad), "{bad}");
        }
        assert!(id_ok("1AbC-d_9"));
    }

    #[test]
    fn a_view_is_one_of_five_and_only_two_open_a_folder() {
        let view = |args: Value| view_of(&args);
        assert_eq!(view(json!({"view": "my"})).unwrap(), "my");
        assert_eq!(
            view(json!({"view": "shared", "folder": "f1"})).unwrap(),
            "folder:f1"
        );
        assert!(view(json!({"view": "trash", "folder": "f1"})).is_err());
        assert!(view(json!({"view": "my", "folder": "' or trashed = true"})).is_err());
        assert!(view(json!({"view": "folder:f1"})).is_err());
        assert!(view(json!({})).is_err());
    }

    #[test]
    fn every_list_but_trash_asks_for_no_trash() {
        for view in ["my", "shared", RECENT, "starred", "folder:f1"] {
            assert!(listing(view).0.contains("trashed = false"), "{view}");
        }
        assert_eq!(listing("trash").0, "trashed = true");
    }

    #[test]
    fn a_row_reads_googles_resource() {
        let f = file_of(&json!({"id": "a1", "name": "Notes", "mimeType": DOC, "owners": [{"displayName": "Ana Reyes"}], "ownedByMe": false, "modifiedTime": "2026-11-06T09:00:00.000Z", "shared": true, "sharedWithMeTime": "2026-11-05T09:00:00.000Z", "parents": ["p1"]})).unwrap();
        assert_eq!(
            (f.kind.as_str(), f.owner.as_str(), f.size),
            ("doc", "Ana Reyes", None)
        );
        assert!(f.with_me && f.shared && !f.mine && f.changed_ms > 0);
        let sized =
            file_of(&json!({"id": "a2", "name": "x", "mimeType": "image/png", "size": "1024"}));
        assert_eq!(sized.unwrap().size, Some(1024));
        assert_eq!(file_of(&json!({"id": "../x"})), None);
    }

    #[test]
    fn each_type_has_its_kind() {
        for (mime, kind) in [
            (FOLDER, "folder"),
            (SLIDES, "slides"),
            (SHEET, "sheet"),
            ("application/pdf", "pdf"),
            ("image/svg+xml", "image"),
            ("application/msword", "word"),
            (
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                "spreadsheet",
            ),
            ("application/zip", "file"),
        ] {
            assert_eq!(kind_of(mime), kind);
        }
    }

    #[test]
    fn google_slowing_us_down_is_not_a_lost_sign_in() {
        let body = |reason: &str, message: &str| {
            json!({"error": {"message": message, "errors": [{"reason": reason}]}}).to_string()
        };
        assert_eq!(
            refusal_of(403, body("insufficientPermissions", "x").as_bytes()),
            Err(Failure::NeedsAccess)
        );
        assert_eq!(
            refusal_of(
                403,
                body("", "Request had insufficient authentication scopes.").as_bytes()
            ),
            Err(Failure::NeedsAccess)
        );
        assert_eq!(
            refusal_of(403, body("userRateLimitExceeded", "x").as_bytes()),
            Err(busy())
        );
        assert_eq!(
            refusal_of(403, body("insufficientFilePermissions", "x").as_bytes()),
            Ok(Answer::Forbidden("insufficientFilePermissions".to_owned()))
        );
    }

    #[test]
    fn an_image_has_to_be_what_it_says() {
        assert!(image_is("image/png", b"\x89PNG\r\n\x1a\nrest"));
        assert!(image_is("image/webp", b"RIFF1234WEBPVP8 "));
        assert!(!image_is("image/png", b"<html><script>"));
        assert!(!image_is("image/svg+xml", b"<svg/>"));
    }
}
