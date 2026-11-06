//! Drive's writes: a new folder, an upload, a rename, a move, the Trash and
//! sharing. Only a person's press gets here. Every tool is the app's alone, so
//! the model never sees one, and `serve` refuses it and a routine, and an AI's
//! press too, which goes out as the AI whatever its label says.
//!
//! Nothing here deletes a file: [`DriveApi`] has no such call, and the Trash is
//! `files.update` with `trashed`.
//!
//! A name is the person's or a stranger's, and goes to Google in a JSON body
//! alone; an id is checked before it's a path. A write Google refuses changes
//! nothing here. One it takes goes straight into the store, from Google's own
//! answer, so the list is right before the changes sync comes round.

use std::io::Cursor;
use std::sync::MutexGuard;

use gws_store::drive::File;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use super::{
    Answer, DriveApi, FILE_FIELDS, FOLDER, Response, bump, file_of, id_ok, refusal_of, root_of,
    row_json, text,
};
use crate::accounts::Bearer;
use crate::compose;
use crate::files::Sent;
use crate::gmail::{Failure, HEADER_MAX};
use crate::mail::{signed_in, with_own_store};
use crate::tools::{
    DRIVE_ACCESS, DRIVE_FOLDER, DRIVE_MOVE, DRIVE_RENAME, DRIVE_SHARE, DRIVE_TRASH, DRIVE_UNDO,
    DRIVE_UPLOAD, failed, ok,
};
use crate::{Core, js};

/// The most files one press takes.
const ACT_MAX: usize = 100;
/// How many actions can still be undone.
const UNDOS_MAX: usize = 20;
/// The longest message that goes with an invitation.
const MESSAGE_MAX: usize = 4000;
/// The most people one press adds or takes off.
const PEOPLE_MAX: usize = 100;
/// Google's own largest page of people.
const PEOPLE_PAGE: &str = "100";
/// Pages of people read for one file. Past this the list can't be shown whole,
/// and a Share that can't show everyone isn't offered.
const PEOPLE_PAGES: usize = 50;
const PEOPLE_FIELDS: &str =
    "nextPageToken,permissions(id,type,role,emailAddress,displayName,domain)";
const ROLES: [&str; 3] = ["reader", "commenter", "writer"];
/// What Google stands for "My Drive" here.
const MY_DRIVE: &str = "root";
const NO_TYPE: &str = "application/octet-stream";

/// What one action changed, so Undo can put it back exactly.
enum Back {
    Name {
        id: String,
        name: String,
    },
    Parents {
        id: String,
        parents: Vec<String>,
    },
    /// What this press trashed, and nothing that was in the Trash already.
    Trash(Vec<String>),
}

/// What to answer when a write didn't happen, or a failure saying why.
type Stopped = Result<Value, Failure>;

struct Kept {
    account: String,
    back: Back,
}

#[derive(Default)]
pub struct Undos {
    next: u64,
    kept: std::collections::VecDeque<(String, Kept)>,
}

impl Undos {
    fn keep(&mut self, kept: Kept) -> String {
        self.next += 1;
        let id = format!("du{}", self.next);
        self.kept.push_back((id.clone(), kept));
        if self.kept.len() > UNDOS_MAX {
            self.kept.pop_front();
        }
        id
    }

    fn take(&mut self, id: &str) -> Option<Kept> {
        let at = self.kept.iter().position(|(k, _)| k == id)?;
        self.kept.remove(at).map(|(_, kept)| kept)
    }
}

fn undos(core: &Core) -> MutexGuard<'_, Undos> {
    core.drive_undos.lock().unwrap_or_else(|e| e.into_inner())
}

fn keep(core: &Core, account: &str, back: Back) -> String {
    undos(core).keep(Kept {
        account: account.to_owned(),
        back,
    })
}

fn refusal(code: &str, why: &str) -> Value {
    ok(json!({"refused": code, "message": why}))
}

fn bad_args(why: &str) -> Value {
    refusal("invalid_args", why)
}

/// A write Google turned down, in words for the person. Nothing changed.
fn refused(answer: Answer) -> Value {
    match answer {
        Answer::NotFound => refusal(
            "not_found",
            "That's gone from Google Drive, so nothing changed.",
        ),
        Answer::Invalid(why) => bad_args(&format!("Google Drive didn't accept that: {why}")),
        Answer::Forbidden(reason) if reason == "storageQuotaExceeded" => refusal(
            "no_space",
            "Your Google storage is full, so nothing changed.",
        ),
        Answer::Done(_) | Answer::Forbidden(_) => refusal(
            "no_access",
            "This account can't do that in Google Drive, so nothing changed.",
        ),
    }
}

/// A failure, in the same words. Being offline is said here, not as the
/// network's own error: nothing about it is the person's to read.
fn given_up(e: Failure) -> Result<Value, Failure> {
    match e {
        Failure::Offline(_) => Ok(refusal(
            "offline",
            "Google Drive can't be reached right now, so nothing changed.",
        )),
        other => Err(other),
    }
}

impl DriveApi {
    fn answered(mut res: Response) -> Result<Answer, Failure> {
        let status = res.status().as_u16();
        let body = Self::body(&mut res)?;
        if !(200..300).contains(&status) {
            return refusal_of(status, &body);
        }
        // a delete answers with nothing at all
        if body.is_empty() {
            return Ok(Answer::Done(Value::Null));
        }
        serde_json::from_slice(&body)
            .map(Answer::Done)
            .map_err(|_| Failure::Plain("Google Drive's answer is not JSON".to_owned()))
    }

    fn unreachable(e: &ureq::Error) -> Failure {
        Failure::Offline(format!(
            "could not reach Google Drive: {}",
            js::slice_utf16(&e.to_string(), 200)
        ))
    }

    /// `files.create` with no bytes: a folder, or an empty file.
    fn make(&self, auth: &Bearer, metadata: &Value) -> Result<Answer, Failure> {
        let res = self
            .agent
            .post(format!("{}/files", self.base))
            .query("fields", FILE_FIELDS)
            .header("authorization", auth.header())
            .header("content-type", "application/json; charset=UTF-8")
            .send(metadata.to_string().as_str())
            .map_err(|e| Self::unreachable(&e))?;
        Self::answered(res)
    }

    /// `files.update` of metadata. The Trash is `{"trashed": true}` in here:
    /// nothing in this file can delete a file.
    fn change(
        &self,
        auth: &Bearer,
        id: &str,
        metadata: &Value,
        parents: Option<(&[String], &[String])>,
    ) -> Result<Answer, Failure> {
        let mut req = self
            .agent
            .patch(format!("{}/files/{id}", self.base))
            .query("fields", FILE_FIELDS)
            .header("authorization", auth.header())
            .header("content-type", "application/json; charset=UTF-8");
        if let Some((add, remove)) = parents {
            if !add.is_empty() {
                req = req.query("addParents", add.join(","));
            }
            if !remove.is_empty() {
                req = req.query("removeParents", remove.join(","));
            }
        }
        let res = req
            .send(metadata.to_string().as_str())
            .map_err(|e| Self::unreachable(&e))?;
        Self::answered(res)
    }

    /// One page of who has a file.
    fn people_page(&self, auth: &Bearer, id: &str, token: Option<&str>) -> Result<Answer, Failure> {
        let mut query = vec![
            ("pageSize", PEOPLE_PAGE.to_owned()),
            ("fields", PEOPLE_FIELDS.to_owned()),
        ];
        query.extend(token.map(|t| ("pageToken", t.to_owned())));
        self.get(auth, &format!("files/{id}/permissions"), &query)
    }

    /// `permissions.create`. The mail is why this is the person's alone: Google
    /// tells whoever it's for, and takes the message with it.
    fn invite(
        &self,
        auth: &Bearer,
        id: &str,
        email: &str,
        role: &str,
        message: Option<&str>,
    ) -> Result<Answer, Failure> {
        let mut req = self
            .agent
            .post(format!("{}/files/{id}/permissions", self.base))
            .query("sendNotificationEmail", "true")
            .query("fields", "id")
            .header("authorization", auth.header())
            .header("content-type", "application/json; charset=UTF-8");
        if let Some(message) = message {
            req = req.query("emailMessage", message);
        }
        let body = json!({"type": "user", "role": role, "emailAddress": email});
        let res = req
            .send(body.to_string().as_str())
            .map_err(|e| Self::unreachable(&e))?;
        Self::answered(res)
    }

    /// `permissions.delete`: someone's access, and only ever that.
    fn unshare(&self, auth: &Bearer, id: &str, person: &str) -> Result<Answer, Failure> {
        let res = self
            .agent
            .delete(format!("{}/files/{id}/permissions/{person}", self.base))
            .header("authorization", auth.header())
            .call()
            .map_err(|e| Self::unreachable(&e))?;
        Self::answered(res)
    }
}

/// A file a write left, in the store and every list it belongs in now, and the
/// sync told. A store that won't take it is only a stale row: the sync mends it,
/// and the write has happened whatever this says.
fn landed(core: &Core, auth: &Bearer, account: &str, files: &[File]) {
    if let Ok(root) = root_of(core, auth, account) {
        // a sync holding changes read before this write mustn't land after it
        let _one_at_a_time = core.drive_sync.lock().unwrap_or_else(|e| e.into_inner());
        let _ = with_own_store(core, account, |store| {
            store.apply_write(account, &root, files)
        });
    }
    let _ = bump(core, account);
    core.sync.wake();
}

fn rows_json(files: &[File]) -> Vec<Value> {
    files.iter().map(row_json).collect()
}

/// A name to give a file: what's left after the spaces, and something a list
/// can show. It's never cut, as a name the person didn't type isn't theirs.
fn name_of(args: &Value) -> Result<String, Value> {
    let name = js::trim(&js::string_or_empty(args.get("name"))).to_owned();
    if name.is_empty() {
        return Err(bad_args("A name can't be empty."));
    }
    if name.len() > HEADER_MAX {
        return Err(bad_args("That name is too long."));
    }
    if name.chars().any(char::is_control) {
        return Err(bad_args("A name can't hold a line break or a tab."));
    }
    Ok(name)
}

/// A folder as a parent: an id, or My Drive by its real id, so what goes out
/// doesn't rest on `root` being understood there.
fn folder_of(
    core: &Core,
    auth: &Bearer,
    account: &str,
    id: &str,
) -> Result<Result<String, Value>, Failure> {
    if id == MY_DRIVE {
        return root_of(core, auth, account).map(Ok);
    }
    Ok(if id_ok(id) {
        Ok(id.to_owned())
    } else {
        Err(bad_args("That isn't a folder."))
    })
}

/// `gws_drive_folder`.
fn folder(core: &Core, args: &Value) -> Result<Value, Failure> {
    let (auth, account) = signed_in(core)?;
    let name = match name_of(args) {
        Ok(name) => name,
        Err(answer) => return Ok(answer),
    };
    let parent = js::string_or_empty(args.get("parent"));
    let parent = match folder_of(core, &auth, &account, &parent) {
        Ok(Ok(parent)) => parent,
        Ok(Err(answer)) => return Ok(answer),
        Err(e) => return given_up(e),
    };
    let metadata = json!({"name": name, "mimeType": FOLDER, "parents": [parent]});
    finish_make(core, &auth, &account, core.drive.make(&auth, &metadata))
}

/// A new file's answer, whichever way it came.
fn finish_make(
    core: &Core,
    auth: &Bearer,
    account: &str,
    answer: Result<Answer, Failure>,
) -> Result<Value, Failure> {
    match changed(core, auth, account, answer) {
        Ok(file) => Ok(ok(json!({"files": [row_json(&file)]}))),
        Err(stopped) => stopped,
    }
}

/// What a picked file says it is, with a type that can't turn it into a Google
/// file: that would be a conversion, and it's the person's file that goes up.
fn upload_type(mime: &str) -> &str {
    if mime.starts_with("application/vnd.google-apps.") {
        NO_TYPE
    } else {
        mime
    }
}

/// `gws_drive_upload`: a whole file from `gws_upload_chunk`, through the same
/// resumable route as an edited file. The press spends it either way, and a try
/// again sends it over from the file the person picked. An empty file has no
/// bytes for a session to take.
fn upload(core: &Core, args: &Value) -> Result<Value, Failure> {
    let (auth, account) = signed_in(core)?;
    let id = js::string_or_empty(args.get("upload"));
    let held = core
        .uploads
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take(&id);
    let Some(part) = held else {
        return Ok(refusal(
            "not_found",
            "That upload is gone. Pick the file again.",
        ));
    };
    let parent = js::string_or_empty(args.get("parent"));
    let parent = match folder_of(core, &auth, &account, &parent) {
        Ok(Ok(parent)) => parent,
        Ok(Err(answer)) => return Ok(answer),
        Err(e) => return given_up(e),
    };
    let mime = upload_type(&part.mime);
    let metadata = json!({"name": part.name, "mimeType": mime, "parents": [parent]});
    let length = part.bytes.len() as u64;
    if length == 0 {
        return finish_make(core, &auth, &account, core.drive.make(&auth, &metadata));
    }
    let session = core
        .transfers
        .session(&auth, None, &metadata, mime, length, FILE_FIELDS);
    let place = match session {
        Ok(Ok(place)) => place,
        Ok(Err(answer)) => return Ok(refused(answer)),
        Err(e) => return given_up(e),
    };
    let sent = core
        .transfers
        .send(&place, &mut Cursor::new(part.bytes), length);
    let answer = sent.map(|sent| match sent {
        Sent::Done(v) => Answer::Done(v),
        Sent::Refused(answer) => answer,
    });
    finish_make(core, &auth, &account, answer)
}

/// `id` as it is now, or the answer to give instead.
fn live(
    core: &Core,
    auth: &Bearer,
    id: &str,
    fields: &str,
) -> Result<Result<Value, Value>, Failure> {
    match core.drive.file(auth, id, fields) {
        Ok(Answer::Done(v)) => Ok(Ok(v)),
        Ok(answer) => Ok(Err(refused(answer))),
        Err(e) => given_up(e).map(Err),
    }
}

/// The file a write answered with, now in the store, or what to answer instead.
fn changed(
    core: &Core,
    auth: &Bearer,
    account: &str,
    answer: Result<Answer, Failure>,
) -> Result<File, Stopped> {
    match answer {
        Ok(Answer::Done(v)) => {
            let Some(file) = file_of(&v) else {
                return Err(Err(Failure::Plain(
                    "Google Drive's answer didn't name the file.".to_owned(),
                )));
            };
            landed(core, auth, account, std::slice::from_ref(&file));
            Ok(file)
        }
        Ok(answer) => Err(Ok(refused(answer))),
        Err(e) => Err(given_up(e)),
    }
}

/// `gws_drive_rename`. The old name is read from Google, not from what a screen
/// showed, so Undo puts back what was really there.
fn rename(core: &Core, args: &Value) -> Result<Value, Failure> {
    let (auth, account) = signed_in(core)?;
    let id = match super::id_of(args) {
        Ok(id) => id,
        Err(answer) => return Ok(answer),
    };
    let name = match name_of(args) {
        Ok(name) => name,
        Err(answer) => return Ok(answer),
    };
    let before = match live(core, &auth, &id, "name")? {
        Ok(v) => text(&v, "name"),
        Err(answer) => return Ok(answer),
    };
    let answer = core.drive.change(&auth, &id, &json!({"name": name}), None);
    let file = match changed(core, &auth, &account, answer) {
        Ok(file) => file,
        Err(stopped) => return stopped,
    };
    let back = Back::Name { id, name: before };
    let undo = keep(core, &account, back);
    Ok(ok(json!({"undo": undo, "files": [row_json(&file)]})))
}

fn parents_in(v: &Value) -> Vec<String> {
    v.get("parents")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|p| p.as_str().filter(|p| id_ok(p)).map(str::to_owned))
        .collect()
}

/// `gws_drive_move`: out of where it is, into `to`, in one write.
fn move_file(core: &Core, args: &Value) -> Result<Value, Failure> {
    let (auth, account) = signed_in(core)?;
    let id = match super::id_of(args) {
        Ok(id) => id,
        Err(answer) => return Ok(answer),
    };
    let to = js::string_or_empty(args.get("to"));
    let to = match folder_of(core, &auth, &account, &to) {
        Ok(Ok(to)) => to,
        Ok(Err(answer)) => return Ok(answer),
        Err(e) => return given_up(e),
    };
    if to == id {
        return Ok(bad_args("A folder can't go inside itself."));
    }
    let before = match live(core, &auth, &id, "parents")? {
        Ok(v) => parents_in(&v),
        Err(answer) => return Ok(answer),
    };
    if before.contains(&to) {
        return Ok(refusal("already_there", "It's already in that folder."));
    }
    let add = [to];
    let answer = core
        .drive
        .change(&auth, &id, &json!({}), Some((&add, &before)));
    let file = match changed(core, &auth, &account, answer) {
        Ok(file) => file,
        Err(stopped) => return stopped,
    };
    let back = Back::Parents {
        id,
        parents: before,
    };
    let undo = keep(core, &account, back);
    Ok(ok(json!({"undo": undo, "files": [row_json(&file)]})))
}

fn ids_of(args: &Value) -> Option<Vec<String>> {
    let list = args.get("ids")?.as_array()?;
    let mut ids: Vec<String> = Vec::new();
    for id in list {
        let id = id.as_str().filter(|id| id_ok(id))?.to_owned();
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    (!ids.is_empty() && ids.len() <= ACT_MAX).then_some(ids)
}

/// One file to the Trash, unless it's there already: what the person trashed
/// before stays out of this press's Undo. `None` is that case.
fn trash_one(
    core: &Core,
    auth: &Bearer,
    id: &str,
) -> Result<Result<Option<Value>, Value>, Failure> {
    let was = match live(core, auth, id, "trashed")? {
        Ok(v) => v,
        Err(answer) => return Ok(Err(answer)),
    };
    if was.get("trashed") == Some(&Value::Bool(true)) {
        return Ok(Ok(None));
    }
    match core.drive.change(auth, id, &json!({"trashed": true}), None) {
        Ok(Answer::Done(v)) => Ok(Ok(Some(v))),
        Ok(answer) => Ok(Err(refused(answer))),
        Err(e) => given_up(e).map(Err),
    }
}

/// `gws_drive_trash`. It stops at the first that fails, and what went before it
/// can still be undone.
fn trash(core: &Core, args: &Value) -> Result<Value, Failure> {
    let (auth, account) = signed_in(core)?;
    let Some(ids) = ids_of(args) else {
        return Ok(bad_args("That isn't a set of files."));
    };
    let (mut files, mut failure) = (Vec::new(), None);
    for id in &ids {
        match trash_one(core, &auth, id) {
            Ok(Ok(Some(v))) => files.extend(file_of(&v)),
            Ok(Ok(None)) => {}
            Ok(Err(answer)) => {
                failure = Some(Ok(answer));
                break;
            }
            Err(e) => {
                failure = Some(Err(e));
                break;
            }
        }
    }
    let done: Vec<String> = files.iter().map(|f: &File| f.id.clone()).collect();
    if done.is_empty() {
        return match failure {
            Some(failure) => failure,
            None => Ok(ok(json!({"files": []}))),
        };
    }
    // the writes have happened, so the way back is kept before anything else
    let undo = keep(core, &account, Back::Trash(done));
    landed(core, &auth, &account, &files);
    let mut answer = json!({"undo": undo, "files": rows_json(&files)});
    if failure.is_some() {
        answer["partial"] = json!(true);
    }
    Ok(ok(answer))
}

/// Takes back one action. What's left of it after a failure can be tried again,
/// under the same id.
fn undo(core: &Core, args: &Value) -> Result<Value, Failure> {
    let (auth, account) = signed_in(core)?;
    let id = js::string_or_empty(args.get("undo"));
    let held = undos(core).take(&id);
    let Some(kept) = held.filter(|k| k.account == account) else {
        return Ok(refusal("not_found", "That can't be undone any more."));
    };
    let (mut files, mut stopped) = (Vec::new(), None);
    let mut take_back = |answer| match changed(core, &auth, &account, answer) {
        Ok(file) => {
            files.push(file);
            true
        }
        Err(stop) => {
            stopped = Some(stop);
            false
        }
    };
    let left = match kept.back {
        Back::Name { id, name } => {
            let answer = core.drive.change(&auth, &id, &json!({"name": name}), None);
            (!take_back(answer)).then_some(Back::Name { id, name })
        }
        Back::Parents { id, parents } => {
            let answer = parents_back(core, &auth, &id, &parents);
            (!take_back(answer)).then_some(Back::Parents { id, parents })
        }
        Back::Trash(mut ids) => {
            while let Some(id) = ids.first() {
                let answer = core
                    .drive
                    .change(&auth, id, &json!({"trashed": false}), None);
                if !take_back(answer) {
                    break;
                }
                ids.remove(0);
            }
            (!ids.is_empty()).then_some(Back::Trash(ids))
        }
    };
    if let (Some(back), Some(stopped)) = (left, stopped) {
        let account = account.clone();
        undos(core).kept.push_back((id, Kept { account, back }));
        return stopped;
    }
    Ok(ok(json!({"files": rows_json(&files)})))
}

/// The write that takes a file back to `parents`, from wherever it is now.
fn parents_back(
    core: &Core,
    auth: &Bearer,
    id: &str,
    parents: &[String],
) -> Result<Answer, Failure> {
    let now = match core.drive.file(auth, id, "parents")? {
        Answer::Done(v) => parents_in(&v),
        answer => return Ok(answer),
    };
    let add: Vec<String> = parents
        .iter()
        .filter(|p| !now.contains(p))
        .cloned()
        .collect();
    let remove: Vec<String> = now
        .iter()
        .filter(|p| !parents.contains(p))
        .cloned()
        .collect();
    core.drive
        .change(auth, id, &json!({}), Some((&add, &remove)))
}

/// Someone with a file, as a screen shows them. `removable` is whether this
/// screen may take them off: an owner stays, and so does the person themselves,
/// and link and domain access are left to Google's own screen.
fn person_of(p: &Value, me: &str) -> Value {
    let (kind, role) = (text(p, "type"), text(p, "role"));
    let email = text(p, "emailAddress");
    let name = match kind.as_str() {
        "anyone" => "Anyone with the link".to_owned(),
        "domain" => format!("Anyone at {}", text(p, "domain")),
        _ => text(p, "displayName"),
    };
    let you = email.eq_ignore_ascii_case(me);
    let removable = matches!(kind.as_str(), "user" | "group") && role != "owner" && !you;
    json!({"id": text(p, "id"), "kind": kind, "name": name, "email": email, "role": role, "you": you, "removable": removable})
}

/// A list of people as a mark: anything about who's on it moves it.
fn mark_of(people: &[Value]) -> String {
    let mut lines: Vec<String> = people
        .iter()
        .map(|p| {
            format!(
                "{}\0{}\0{}",
                text(p, "id"),
                text(p, "role"),
                text(p, "email")
            )
        })
        .collect();
    lines.sort();
    let hash = Sha256::digest(lines.join("\n").as_bytes());
    format!("{hash:x}")[..32].to_owned()
}

/// Every permission on a file, however many pages it takes, or the answer to
/// give instead. `None` is a list too long to read whole.
fn everyone(
    core: &Core,
    auth: &Bearer,
    account: &str,
    id: &str,
) -> Result<Result<Option<Vec<Value>>, Value>, Failure> {
    let (mut people, mut token) = (Vec::new(), None::<String>);
    for _ in 0..PEOPLE_PAGES {
        let page = match core.drive.people_page(auth, id, token.as_deref())? {
            Answer::Done(page) => page,
            answer => return Ok(Err(refused(answer))),
        };
        people.extend(
            page.get("permissions")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|p| person_of(p, account)),
        );
        token = Some(text(&page, "nextPageToken")).filter(|t| !t.is_empty());
        if token.is_none() {
            // the owner first, then as Google has them
            people.sort_by_key(|p| p.get("role") != Some(&json!("owner")));
            return Ok(Ok(Some(people)));
        }
    }
    Ok(Ok(None))
}

/// `gws_drive_access`: the list a Share shows. Only a whole list comes: nobody
/// can press Share on a part of one.
fn access(core: &Core, args: &Value) -> Result<Value, Failure> {
    let (auth, account) = signed_in(core)?;
    let id = match super::id_of(args) {
        Ok(id) => id,
        Err(answer) => return Ok(answer),
    };
    match everyone(core, &auth, &account, &id) {
        Ok(Ok(Some(people))) => Ok(ok(json!({"seen": mark_of(&people), "people": people}))),
        Ok(Ok(None)) => Ok(refusal(
            "too_many",
            "Too many people have this file to show them all here, so it can't be shared from here.",
        )),
        Ok(Err(answer)) => Ok(answer),
        Err(e) => given_up(e),
    }
}

/// An address Google could use. It's the person's to get right; this is here so
/// what isn't one never leaves.
fn address_ok(email: &str) -> bool {
    let parts = email.split_once('@');
    email.len() <= 254
        && !email.chars().any(|c| c.is_control() || c.is_whitespace())
        && parts.is_some_and(|(who, host)| {
            !who.is_empty() && !host.contains('@') && host.contains('.') && !host.ends_with('.')
        })
}

/// Who a press adds, each once, or why not.
fn adds_of(args: &Value) -> Result<Vec<(String, String)>, &'static str> {
    let mut adds: Vec<(String, String)> = Vec::new();
    for person in args
        .get("add")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let email = js::trim(&js::string_or_empty(person.get("email"))).to_owned();
        let role = js::string_or_empty(person.get("role"));
        if !address_ok(&email) {
            return Err("One of those isn't an email address.");
        }
        // an owner or an organiser is a transfer, which isn't sharing
        if !ROLES.contains(&role.as_str()) {
            return Err("That isn't a role a file can be shared with.");
        }
        if adds.iter().any(|(a, _)| a.eq_ignore_ascii_case(&email)) {
            return Err("That address is there twice.");
        }
        adds.push((email, role));
    }
    Ok(adds)
}

/// Why this press can't go on the people the file has now.
fn not_fit(
    people: &[Value],
    adds: &[(String, String)],
    removes: &[String],
) -> Option<&'static str> {
    let holds = |email: &str| {
        people
            .iter()
            .any(|p| text(p, "email").eq_ignore_ascii_case(email))
    };
    if adds.iter().any(|(email, _)| holds(email)) {
        return Some("One of those people has this file already.");
    }
    let may_go = |id: &String| {
        people
            .iter()
            .any(|p| text(p, "id") == *id && p.get("removable") == Some(&json!(true)))
    };
    if !removes.iter().all(may_go) {
        return Some("One of those can't be taken off from here.");
    }
    None
}

/// `gws_drive_share`. The list the person read has to still be the list: anyone
/// else on it now was never shown to them, so nothing goes. Google's own mail
/// goes out only with an add: a removal tells nobody.
fn share(core: &Core, args: &Value) -> Result<Value, Failure> {
    let (auth, account) = signed_in(core)?;
    let id = match super::id_of(args) {
        Ok(id) => id,
        Err(answer) => return Ok(answer),
    };
    let adds = match adds_of(args) {
        Ok(adds) => adds,
        Err(why) => return Ok(bad_args(why)),
    };
    let removes: Vec<String> = args
        .get("remove")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|p| p.as_str().map(str::to_owned))
        .collect();
    if adds.is_empty() && removes.is_empty() {
        return Ok(bad_args("Nobody's been added or taken off."));
    }
    if adds.len() + removes.len() > PEOPLE_MAX {
        return Ok(bad_args("That's too many people for one press."));
    }
    if !removes.iter().all(|p| id_ok(p)) {
        return Ok(bad_args("That isn't someone who has this file."));
    }
    if let Some(allowed) = compose::only_to()
        && let Some(other) = compose::outside(&allowed, adds.iter().map(|(a, _)| a.as_str()))
    {
        return Ok(refusal(
            "not_allowed",
            &format!("This test run shares only with {allowed}, not {other}."),
        ));
    }
    let message = Some(js::trim(&js::string_or_empty(args.get("message"))).to_owned())
        .filter(|m| !m.is_empty());
    if message.as_ref().is_some_and(|m| m.len() > MESSAGE_MAX) {
        return Ok(bad_args("That message is too long."));
    }
    let people = match everyone(core, &auth, &account, &id) {
        Ok(Ok(Some(people))) => people,
        Ok(Ok(None)) => {
            return Ok(refusal(
                "too_many",
                "Too many people have this file to check them all, so nothing changed.",
            ));
        }
        Ok(Err(answer)) => return Ok(answer),
        Err(e) => return given_up(e),
    };
    if mark_of(&people) != js::string_or_empty(args.get("seen")) {
        return Ok(refusal(
            "changed",
            "Who has this file changed since you looked, so nothing changed. Check the list again.",
        ));
    }
    if let Some(why) = not_fit(&people, &adds, &removes) {
        return Ok(bad_args(why));
    }
    let (mut added, mut removed, mut failure) = (0, 0, None);
    for (email, role) in &adds {
        match core
            .drive
            .invite(&auth, &id, email, role, message.as_deref())
        {
            Ok(Answer::Done(_)) => added += 1,
            Ok(answer) => {
                failure = Some(Ok(refused(answer)));
                break;
            }
            Err(e) => {
                failure = Some(given_up(e));
                break;
            }
        }
    }
    if failure.is_none() {
        for person in &removes {
            match core.drive.unshare(&auth, &id, person) {
                Ok(Answer::Done(_)) => removed += 1,
                Ok(answer) => {
                    failure = Some(Ok(refused(answer)));
                    break;
                }
                Err(e) => {
                    failure = Some(given_up(e));
                    break;
                }
            }
        }
    }
    if added + removed == 0
        && let Some(failure) = failure
    {
        return failure;
    }
    // sharing moves the row's own `shared`, so it comes back from Google
    let row = match core.drive.file(&auth, &id, FILE_FIELDS) {
        Ok(Answer::Done(v)) => file_of(&v),
        _ => None,
    };
    let files: Vec<File> = row.into_iter().collect();
    landed(core, &auth, &account, &files);
    let mut answer = json!({"added": added, "removed": removed, "files": rows_json(&files)});
    if failure.is_some() {
        answer["partial"] = json!(true);
    }
    Ok(ok(answer))
}

/// Runs a Drive write; `None` for any other tool.
pub fn run(core: &Core, tool: &str, args: &Value) -> Option<Value> {
    let result = match tool {
        DRIVE_ACCESS => access(core, args),
        DRIVE_UPLOAD => upload(core, args),
        DRIVE_FOLDER => folder(core, args),
        DRIVE_RENAME => rename(core, args),
        DRIVE_MOVE => move_file(core, args),
        DRIVE_TRASH => trash(core, args),
        DRIVE_SHARE => share(core, args),
        DRIVE_UNDO => undo(core, args),
        _ => return None,
    };
    core.sync.seen();
    Some(result.unwrap_or_else(failed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_has_to_be_one() {
        for good in ["ana@example.com", "a.b+c@mail.example.com"] {
            assert!(address_ok(good), "{good}");
        }
        for bad in [
            "",
            "ana",
            "@example.com",
            "ana@",
            "ana@example",
            "ana@example.",
            "a b@example.com",
            "ana@example.com\nbcc",
            "a@b@example.com",
        ] {
            assert!(!address_ok(bad), "{bad}");
        }
    }

    #[test]
    fn only_people_sharing_gave_can_be_taken_off_here() {
        let me = "owner@example.com";
        let removable = |p: Value| person_of(&p, me)["removable"].clone();
        let them =
            json!({"id": "1", "type": "user", "role": "writer", "emailAddress": "ana@example.com"});
        assert_eq!(removable(them), json!(true));
        let owner =
            json!({"id": "2", "type": "user", "role": "owner", "emailAddress": "ana@example.com"});
        assert_eq!(removable(owner), json!(false));
        let you = json!({"id": "3", "type": "user", "role": "writer", "emailAddress": "OWNER@example.com"});
        assert_eq!(removable(you), json!(false));
        let link = json!({"id": "anyoneWithLink", "type": "anyone", "role": "reader"});
        assert_eq!(person_of(&link, me)["name"], "Anyone with the link");
        assert_eq!(removable(link), json!(false));
        let domain =
            json!({"id": "4", "type": "domain", "role": "reader", "domain": "example.com"});
        assert_eq!(person_of(&domain, me)["name"], "Anyone at example.com");
        assert_eq!(removable(domain), json!(false));
    }

    #[test]
    fn the_mark_moves_with_anyone_on_the_list_and_nothing_else() {
        let person =
            |id: &str, role: &str| json!({"id": id, "role": role, "email": "a@example.com"});
        let list = [person("1", "reader"), person("2", "writer")];
        let turned = [person("2", "writer"), person("1", "reader")];
        assert_eq!(mark_of(&list), mark_of(&turned), "order says nothing");
        assert_ne!(mark_of(&list), mark_of(&list[..1]));
        assert_ne!(
            mark_of(&list),
            mark_of(&[person("1", "writer"), person("2", "writer")])
        );
    }

    #[test]
    fn a_picked_file_can_never_become_a_google_file() {
        assert_eq!(upload_type("application/pdf"), "application/pdf");
        assert_eq!(upload_type("application/vnd.google-apps.document"), NO_TYPE);
        assert_eq!(upload_type(FOLDER), NO_TYPE);
    }

    #[test]
    fn a_share_can_only_add_and_only_with_the_roles_of_sharing() {
        let adds = |people: Value| adds_of(&json!({"add": people}));
        assert_eq!(
            adds(json!([{"email": " ana@example.com ", "role": "reader"}])).unwrap(),
            [("ana@example.com".to_owned(), "reader".to_owned())]
        );
        for role in ["owner", "organizer", "fileOrganizer", ""] {
            assert!(
                adds(json!([{"email": "a@example.com", "role": role}])).is_err(),
                "{role}"
            );
        }
        assert!(
            adds(json!([
                {"email": "A@example.com", "role": "reader"},
                {"email": "a@example.com", "role": "writer"},
            ]))
            .is_err()
        );
    }
}
