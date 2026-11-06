//! Google Drive, faked: `files.list` with `q`, `orderBy`, paging and `fields`,
//! `files.get`, `alt=media`, `files.export`, `about.get`, the changes feed, and
//! `files.update` and `files.create` through a resumable upload. Any other write
//! gets a 405, and an upload it doesn't know a 400.
//!
//! `q` is really parsed, quotes and backslashes as Google says they work, so a
//! query that breaks out of its string does here what it would there. `fields`
//! is really applied, so a field the plugin forgot to ask for isn't there.
//!
//! Only what the plugin needs is here. It's written from what's known of
//! Google's API: passing against it proves the plugin against this stub, not
//! against Google.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use crate::{FAKE_EMAIL, NOW_MS, Reply, Request, civil};

/// My Drive's own id. `root` stands for it, in a `q` and in a path.
pub const DRIVE_ROOT: &str = "0AFakeDriveRootPk9";
pub const FOLDER: &str = "application/vnd.google-apps.folder";
pub const GOOGLE_DOC: &str = "application/vnd.google-apps.document";
pub const GOOGLE_SHEET: &str = "application/vnd.google-apps.spreadsheet";
pub const GOOGLE_SLIDES: &str = "application/vnd.google-apps.presentation";
const DOCX: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
const PPTX: &str = "application/vnd.openxmlformats-officedocument.presentationml.presentation";
const OWNER_NAME: &str = "Owner Fake";
/// Past this an export is refused, as Google's is.
const EXPORT_MAX: usize = 10 * 1024 * 1024;
/// A 1 x 1 PNG.
pub const TINY_PNG: [u8; 67] = [
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0x0d, 0x49, 0x48, 0x44, 0x52, 0, 0, 0,
    1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 0x1f, 0x15, 0xc4, 0x89, 0, 0, 0, 0x0a, 0x49, 0x44, 0x41, 0x54,
    0x78, 0x9c, 0x63, 0, 1, 0, 0, 5, 0, 1, 0x0d, 0x0a, 0x2d, 0xb4, 0, 0, 0, 0, 0x49, 0x45, 0x4e,
    0x44, 0xae, 0x42, 0x60, 0x82,
];

/// A file on a fake Drive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeFile {
    pub id: String,
    pub name: String,
    pub mime: String,
    /// None for a file shared with the person from outside their Drive.
    pub parents: Vec<String>,
    /// `(name, address)`.
    pub owner: (String, String),
    pub modified_ms: i64,
    pub created_ms: i64,
    /// What it holds, or what exports of it hold for a Google file.
    pub content: Vec<u8>,
    /// What `size` says, if not the truth.
    pub claimed_size: Option<i64>,
    pub description: String,
    pub starred: bool,
    pub shared: bool,
    pub shared_with_me: bool,
    pub trashed: bool,
    pub can_download: bool,
    /// `(name, address, role)`, beside the owner.
    pub people: Vec<(String, String, String)>,
    /// Moves with every change to the file, its name included, as Drive's does.
    pub version: i64,
}

impl FakeFile {
    /// A file the person owns, in My Drive.
    pub fn new(id: &str, name: &str, mime: &str) -> Self {
        Self {
            id: id.to_owned(),
            name: name.to_owned(),
            mime: mime.to_owned(),
            parents: vec![DRIVE_ROOT.to_owned()],
            owner: (OWNER_NAME.to_owned(), FAKE_EMAIL.to_owned()),
            modified_ms: NOW_MS,
            created_ms: NOW_MS - 86_400_000,
            content: Vec::new(),
            claimed_size: None,
            description: String::new(),
            starred: false,
            shared: false,
            shared_with_me: false,
            trashed: false,
            can_download: true,
            people: Vec::new(),
            version: 1,
        }
    }

    pub fn folder(id: &str, name: &str) -> Self {
        Self::new(id, name, FOLDER)
    }

    pub fn inside(mut self, folder: &str) -> Self {
        self.parents = vec![folder.to_owned()];
        self
    }

    pub fn holding(mut self, content: &[u8]) -> Self {
        self.content = content.to_vec();
        self
    }

    pub fn modified(mut self, ms: i64) -> Self {
        self.modified_ms = ms;
        self
    }

    /// Someone else's, shared with the person, and in no folder of theirs.
    pub fn shared_by(mut self, name: &str, address: &str) -> Self {
        self.owner = (name.to_owned(), address.to_owned());
        self.parents.clear();
        self.shared = true;
        self.shared_with_me = true;
        self
    }

    fn google(&self) -> bool {
        self.mime.starts_with("application/vnd.google-apps.")
    }

    fn mine(&self) -> bool {
        self.owner.1 == FAKE_EMAIL
    }

    /// `md5Checksum`, as far as anyone here can tell: 32 hex digits that move
    /// with the bytes. Not a real MD5, and the plugin only ever compares it.
    fn checksum(&self) -> String {
        let hash = |seed: u8| {
            self.content.iter().fold(tag(&seed.to_string()), |h, b| {
                (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
            })
        };
        format!("{:016x}{:016x}", hash(1), hash(2))
    }
}

/// What a live run's Drive holds: every kind of file the screen has to show.
pub(crate) fn demo() -> Vec<FakeFile> {
    let hour = 3_600_000;
    let ago = |n: i64| NOW_MS - n * hour;
    let mut starred = FakeFile::new("drv-doc-tax", "Tax checklist", GOOGLE_DOC)
        .holding(b"Fake tax checklist.\nReceipts first.")
        .modified(ago(30));
    starred.starred = true;
    let mut trashed = FakeFile::new("drv-old-draft", "Old draft.docx", DOCX)
        .holding(b"PK fake old draft")
        .modified(ago(200));
    trashed.trashed = true;
    let mut notes = FakeFile::new("drv-doc-notes", "Meeting notes", GOOGLE_DOC)
        .holding(b"Fake meeting notes.\nRoof first, then the fence.")
        .modified(ago(1));
    notes.description = "Notes from <b>Monday</b>".to_owned();
    notes.shared = true;
    notes.people = vec![(
        "Ana Reyes".to_owned(),
        "ana@example.com".to_owned(),
        "writer".to_owned(),
    )];
    vec![
        FakeFile::folder("drv-clients", "Clients").modified(ago(10)),
        FakeFile::folder("drv-reyes", "Reyes Bakery")
            .inside("drv-clients")
            .modified(ago(11)),
        FakeFile::folder("drv-photos", "Photos").modified(ago(12)),
        FakeFile::new("drv-quote", "Roof quote.docx", DOCX)
            .inside("drv-reyes")
            .holding(include_bytes!("roof-quote.docx"))
            .modified(ago(2)),
        FakeFile::new("drv-budget", "Budget 2026.xlsx", XLSX)
            .inside("drv-reyes")
            .holding(include_bytes!("budget.xlsx"))
            .modified(ago(3)),
        FakeFile::new("drv-contract", "Signed contract.pdf", "application/pdf")
            .inside("drv-reyes")
            .holding(b"%PDF-1.7 fake")
            .modified(ago(4)),
        FakeFile::new("drv-storefront", "Storefront.png", "image/png")
            .inside("drv-photos")
            .holding(&TINY_PNG)
            .modified(ago(5)),
        FakeFile::new("drv-site-photo", "Site photo.png", "image/png")
            .inside("drv-photos")
            .holding(include_bytes!("site-photo.png"))
            .modified(ago(6)),
        notes,
        FakeFile::new("drv-sheet-invoices", "Invoices", GOOGLE_SHEET)
            .holding(b"number,total\n1042,1200\n")
            .modified(ago(6)),
        FakeFile::new("drv-slides-pitch", "Pitch", GOOGLE_SLIDES)
            .holding(b"Fake pitch.\nSlide two.")
            .modified(ago(7)),
        FakeFile::new("drv-text-todo", "To do.txt", "text/plain")
            .holding(b"Call the roofer\nPay the invoice\n")
            .modified(ago(8)),
        FakeFile::new("drv-shared-brief", "Shared brief", GOOGLE_DOC)
            .holding(b"Fake brief from Ana.")
            .shared_by("Ana Reyes", "ana@example.com")
            .modified(ago(9)),
        starred,
        trashed,
    ]
}

/// A folder of `n` files, with one in fifty a folder of its own, named so that
/// only an order that puts folders first brings them to the top.
pub(crate) fn big_folder(id: &str, name: &str, n: usize) -> Vec<FakeFile> {
    let mut files = vec![FakeFile::folder(id, name)];
    files.extend((0..n).map(|i| {
        let file_id = format!("drv-{id}-{i:05}");
        let at = NOW_MS - i as i64 * 60_000;
        if i % 50 == 0 {
            FakeFile::folder(&file_id, &format!("Item {i:05} folder"))
                .inside(id)
                .modified(at)
        } else {
            FakeFile::new(&file_id, &format!("Item {i:05}.txt"), "text/plain")
                .inside(id)
                .holding(b"fake")
                .modified(at)
        }
    }));
    files
}

/// `ms` as Drive writes a time.
fn time(ms: i64) -> String {
    let (y, m, d) = civil(ms.div_euclid(86_400_000));
    let secs = ms.div_euclid(1000).rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        secs / 3600,
        secs / 60 % 60,
        secs % 60,
        ms.rem_euclid(1000)
    )
}

fn resource(f: &FakeFile) -> Value {
    let size = f.claimed_size.unwrap_or(f.content.len() as i64);
    let person = |name: &str, address: &str| json!({"displayName": name, "emailAddress": address});
    let mut permissions = vec![
        json!({"type": "user", "role": "owner", "displayName": f.owner.0, "emailAddress": f.owner.1}),
    ];
    permissions.extend(f.people.iter().map(|(name, address, role)| json!({"type": "user", "role": role, "displayName": name, "emailAddress": address})));
    let mut v = json!({
        "kind": "drive#file", "id": f.id, "name": f.name, "mimeType": f.mime,
        "owners": [{"displayName": f.owner.0, "emailAddress": f.owner.1, "me": f.mine()}],
        "ownedByMe": f.mine(), "modifiedTime": time(f.modified_ms), "createdTime": time(f.created_ms),
        "starred": f.starred, "shared": f.shared, "trashed": f.trashed,
        "lastModifyingUser": person(&f.owner.0, &f.owner.1),
        "permissions": permissions,
        "capabilities": {"canDownload": f.can_download},
        "version": f.version.to_string(),
    });
    if !f.parents.is_empty() {
        v["parents"] = json!(f.parents);
    }
    // Google's own files have no size and no checksum
    if !f.google() {
        v["size"] = json!(size.to_string());
        v["md5Checksum"] = json!(f.checksum());
    }
    if !f.description.is_empty() {
        v["description"] = json!(f.description);
    }
    if f.shared_with_me {
        v["sharedWithMeTime"] = json!(time(f.created_ms));
    }
    v
}

const FILE_FIELDS: [&str; 20] = [
    "kind",
    "id",
    "name",
    "mimeType",
    "owners",
    "ownedByMe",
    "modifiedTime",
    "createdTime",
    "starred",
    "shared",
    "trashed",
    "lastModifyingUser",
    "permissions",
    "capabilities",
    "parents",
    "size",
    "description",
    "sharedWithMeTime",
    "version",
    "md5Checksum",
];

/// A parsed `fields`: names, each with what's picked inside it.
#[derive(Debug, Clone, PartialEq)]
struct Fields(Vec<(String, Fields)>);

impl Fields {
    fn parse(text: &str) -> Option<Self> {
        let chars: Vec<char> = text.chars().collect();
        let mut at = 0;
        let fields = Self::list(&chars, &mut at)?;
        (at == chars.len()).then_some(fields)
    }

    fn list(chars: &[char], at: &mut usize) -> Option<Self> {
        let mut out = Vec::new();
        loop {
            let start = *at;
            while chars
                .get(*at)
                .is_some_and(|c| c.is_alphanumeric() || *c == '*')
            {
                *at += 1;
            }
            if start == *at {
                return None;
            }
            let name: String = chars[start..*at].iter().collect();
            let inner = if chars.get(*at) == Some(&'(') {
                *at += 1;
                let inner = Self::list(chars, at)?;
                if chars.get(*at) != Some(&')') {
                    return None;
                }
                *at += 1;
                inner
            } else {
                Self(Vec::new())
            };
            out.push((name, inner));
            if chars.get(*at) != Some(&',') {
                return Some(Self(out));
            }
            *at += 1;
        }
    }

    /// What's asked of a file resource exists on one.
    fn known_on_file(&self) -> bool {
        self.0
            .iter()
            .all(|(name, _)| name == "*" || FILE_FIELDS.contains(&name.as_str()))
    }

    fn inner(&self, name: &str) -> Option<&Self> {
        self.0.iter().find(|(n, _)| n == name).map(|(_, f)| f)
    }

    fn pick(&self, v: &Value) -> Value {
        match v {
            Value::Array(items) => Value::Array(items.iter().map(|i| self.pick(i)).collect()),
            Value::Object(map) if !self.0.is_empty() => {
                let mut out = Map::new();
                for (name, inner) in &self.0 {
                    if name == "*" {
                        return v.clone();
                    }
                    if let Some(field) = map.get(name) {
                        out.insert(name.clone(), inner.pick(field));
                    }
                }
                Value::Object(out)
            }
            _ => v.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Text(String),
    Word(String),
    Is,
    IsNot,
    Open,
    Close,
}

/// `q`'s tokens. Inside a string `\'` is a quote and `\\` a backslash.
fn tokens(q: &str) -> Option<Vec<Token>> {
    let chars: Vec<char> = q.chars().collect();
    let (mut out, mut i) = (Vec::new(), 0);
    while i < chars.len() {
        match chars[i] {
            c if c.is_whitespace() => i += 1,
            '(' => {
                out.push(Token::Open);
                i += 1;
            }
            ')' => {
                out.push(Token::Close);
                i += 1;
            }
            '=' => {
                out.push(Token::Is);
                i += 1;
            }
            '!' if chars.get(i + 1) == Some(&'=') => {
                out.push(Token::IsNot);
                i += 2;
            }
            '\'' => {
                let mut text = String::new();
                i += 1;
                loop {
                    match *chars.get(i)? {
                        '\\' => {
                            text.push(*chars.get(i + 1)?);
                            i += 2;
                        }
                        '\'' => break,
                        c => {
                            text.push(c);
                            i += 1;
                        }
                    }
                }
                i += 1;
                out.push(Token::Text(text));
            }
            c if c.is_alphanumeric() => {
                let start = i;
                while chars.get(i).is_some_and(|c| c.is_alphanumeric()) {
                    i += 1;
                }
                out.push(Token::Word(chars[start..i].iter().collect()));
            }
            _ => return None,
        }
    }
    Some(out)
}

#[derive(Debug, Clone, PartialEq)]
enum Query {
    All(Vec<Query>),
    Any(Vec<Query>),
    Not(Box<Query>),
    NameHas(String),
    TextHas(String),
    NameIs(String),
    MimeIs(String),
    InFolder(String),
    Flag(String, bool),
}

struct Parser {
    tokens: Vec<Token>,
    at: usize,
}

impl Parser {
    fn next(&mut self) -> Option<Token> {
        let t = self.tokens.get(self.at).cloned();
        self.at += 1;
        t
    }

    fn word_is(&self, word: &str) -> bool {
        matches!(self.tokens.get(self.at), Some(Token::Word(w)) if w == word)
    }

    fn any(&mut self) -> Option<Query> {
        let mut terms = vec![self.all()?];
        while self.word_is("or") {
            self.at += 1;
            terms.push(self.all()?);
        }
        Some(Query::Any(terms))
    }

    fn all(&mut self) -> Option<Query> {
        let mut terms = vec![self.term()?];
        while self.word_is("and") {
            self.at += 1;
            terms.push(self.term()?);
        }
        Some(Query::All(terms))
    }

    fn text(&mut self) -> Option<String> {
        match self.next()? {
            Token::Text(t) => Some(t),
            _ => None,
        }
    }

    fn term(&mut self) -> Option<Query> {
        match self.next()? {
            Token::Open => {
                let inner = self.any()?;
                (self.next()? == Token::Close).then_some(inner)
            }
            Token::Word(w) if w == "not" => Some(Query::Not(Box::new(self.term()?))),
            Token::Text(id) => {
                (self.next()? == Token::Word("in".to_owned())).then_some(())?;
                (self.next()? == Token::Word("parents".to_owned())).then_some(())?;
                Some(Query::InFolder(id))
            }
            Token::Word(field) => {
                let op = self.next()?;
                let contains = op == Token::Word("contains".to_owned());
                match field.as_str() {
                    "name" if contains => Some(Query::NameHas(self.text()?)),
                    "name" if op == Token::Is => Some(Query::NameIs(self.text()?)),
                    "fullText" if contains => Some(Query::TextHas(self.text()?)),
                    "mimeType" if op == Token::Is => Some(Query::MimeIs(self.text()?)),
                    "mimeType" if op == Token::IsNot => {
                        Some(Query::Not(Box::new(Query::MimeIs(self.text()?))))
                    }
                    "trashed" | "starred" | "sharedWithMe" if op == Token::Is => {
                        let value = match self.next()? {
                            Token::Word(w) if w == "true" => true,
                            Token::Word(w) if w == "false" => false,
                            _ => return None,
                        };
                        Some(Query::Flag(field, value))
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }
}

fn parse(q: &str) -> Option<Query> {
    let mut parser = Parser {
        tokens: tokens(q)?,
        at: 0,
    };
    let query = parser.any()?;
    (parser.at == parser.tokens.len()).then_some(query)
}

impl Query {
    /// A folder this asks about, for a check that it's there.
    fn folders(&self) -> Vec<&str> {
        match self {
            Self::All(q) | Self::Any(q) => q.iter().flat_map(Self::folders).collect(),
            Self::Not(q) => q.folders(),
            Self::InFolder(id) => vec![id],
            _ => Vec::new(),
        }
    }

    fn full_text(&self) -> bool {
        match self {
            Self::TextHas(_) => true,
            Self::All(q) | Self::Any(q) => q.iter().any(Self::full_text),
            Self::Not(q) => q.full_text(),
            _ => false,
        }
    }

    fn holds(&self, f: &FakeFile) -> bool {
        let has = |text: &str, word: &str| text.to_lowercase().contains(&word.to_lowercase());
        match self {
            Self::All(q) => q.iter().all(|q| q.holds(f)),
            Self::Any(q) => q.iter().any(|q| q.holds(f)),
            Self::Not(q) => !q.holds(f),
            Self::NameHas(w) => has(&f.name, w),
            Self::TextHas(w) => {
                has(&f.name, w)
                    || has(&f.description, w)
                    || std::str::from_utf8(&f.content).is_ok_and(|text| has(text, w))
            }
            Self::NameIs(w) => f.name == *w,
            Self::MimeIs(w) => f.mime == *w,
            Self::InFolder(id) => {
                let id = if id == "root" { DRIVE_ROOT } else { id };
                f.parents.iter().any(|p| p == id)
            }
            Self::Flag(field, value) => {
                *value
                    == match field.as_str() {
                        "trashed" => f.trashed,
                        "starred" => f.starred,
                        _ => f.shared_with_me,
                    }
            }
        }
    }
}

fn error(code: u16, reason: &str, message: &str) -> Reply {
    Reply::json(
        code,
        json!({"error": {"code": code, "message": message, "errors": [{"domain": "global", "reason": reason, "message": message}]}}),
    )
}

/// A forced failure's answer, as Drive words each.
fn forced(status: u16) -> Reply {
    match status {
        401 => error(401, "authError", "Invalid Credentials"),
        403 => error(
            403,
            "insufficientFilePermissions",
            "The user does not have sufficient permissions for this file.",
        ),
        429 | 503 => error(status, "rateLimitExceeded", "Rate Limit Exceeded"),
        _ => error(status, "backendError", "Backend Error"),
    }
}

fn invalid(field: &str) -> Reply {
    error(400, "invalid", &format!("Invalid Value: {field}"))
}

/// A short hash, so a page token only fits the list it came from.
fn tag(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// A forced failure: how many more calls go through, then what they get.
#[derive(Debug, Clone, Copy)]
struct Failing {
    left: usize,
    status: u16,
}

impl Failing {
    fn due(slot: &mut Option<Self>) -> Option<Reply> {
        let fail = slot.as_mut()?;
        if fail.left > 0 {
            fail.left -= 1;
            return None;
        }
        Some(forced(fail.status))
    }
}

/// What an upload session will do with its bytes.
enum Write {
    /// `files.update`: a new revision of this file.
    Update(String),
    /// `files.create`: a new file.
    Create {
        name: String,
        mime: String,
        parents: Vec<String>,
    },
}

/// A resumable upload under way.
struct Session {
    write: Write,
    total: usize,
    got: Vec<u8>,
    fields: Option<Fields>,
}

/// A fake account's Drive.
#[derive(Default)]
pub(crate) struct Drive {
    files: BTreeMap<String, FakeFile>,
    /// Which file changed, oldest first: its place plus one is a token.
    changes: Vec<String>,
    /// Rows a page holds, whatever was asked for.
    pub(crate) page: Option<usize>,
    /// Page tokens from before this generation are refused.
    generation: u64,
    fail_list: Option<Failing>,
    fail_changes: Option<Failing>,
    /// Rows a page of changes holds.
    pub(crate) changes_page: Option<usize>,
    used: i64,
    /// Where this stub is, for the address of an upload.
    pub(crate) base: String,
    sessions: BTreeMap<u64, Session>,
    next_session: u64,
    next_copy: u64,
    fail_uploads: Option<Failing>,
    /// The next piece keeps only this much of what it brought.
    pub(crate) keep_short: Option<usize>,
    /// The answer to the last piece is no JSON.
    pub(crate) garble_last: bool,
}

impl Drive {
    pub(crate) fn new(seed: Vec<FakeFile>) -> Self {
        let used = seed.iter().map(|f| f.content.len() as i64).sum();
        Self {
            files: seed.into_iter().map(|f| (f.id.clone(), f)).collect(),
            used,
            ..Self::default()
        }
    }

    pub(crate) fn add(&mut self, file: FakeFile) {
        self.changes.push(file.id.clone());
        self.files.insert(file.id.clone(), file);
    }

    pub(crate) fn add_quietly(&mut self, file: FakeFile) {
        self.files.insert(file.id.clone(), file);
    }

    pub(crate) fn edit(&mut self, id: &str, change: impl FnOnce(&mut FakeFile)) -> bool {
        let Some(file) = self.files.get_mut(id) else {
            return false;
        };
        change(file);
        file.version += 1;
        self.changes.push(id.to_owned());
        true
    }

    pub(crate) fn held(&self, id: &str) -> Option<FakeFile> {
        self.files.get(id).cloned()
    }

    pub(crate) fn all(&self) -> Vec<FakeFile> {
        self.files.values().cloned().collect()
    }

    pub(crate) fn remove(&mut self, id: &str) -> bool {
        self.changes.push(id.to_owned());
        self.files.remove(id).is_some()
    }

    pub(crate) fn expire_pages(&mut self) {
        self.generation += 1;
    }

    /// After `left` more pages of a list, each page gets `status`.
    pub(crate) fn fail_list(&mut self, left: usize, status: u16) {
        self.fail_list = Some(Failing { left, status });
    }

    /// After `left` more calls of the changes feed, each gets `status`.
    pub(crate) fn fail_changes(&mut self, left: usize, status: u16) {
        self.fail_changes = Some(Failing { left, status });
    }

    /// After `left` more upload sessions start, each gets `status`.
    pub(crate) fn fail_uploads(&mut self, left: usize, status: u16) {
        self.fail_uploads = Some(Failing { left, status });
    }

    pub(crate) fn works(&mut self) {
        self.fail_list = None;
        self.fail_changes = None;
        self.fail_uploads = None;
    }

    /// `/upload/drive/v3/…`. Opening a session is `POST files` for a new file or
    /// `PATCH files/<id>` for a revision; each piece then goes by `PUT` to the
    /// address that answered, and only the last one writes anything.
    pub(crate) fn upload(&mut self, r: &Request, rest: &str) -> Reply {
        if r.param("uploadType").as_deref() != Some("resumable") {
            return invalid("uploadType");
        }
        if let Some(session) = r.param("upload_id") {
            return self.piece(r, &session);
        }
        if let Some(reply) = Failing::due(&mut self.fail_uploads) {
            return reply;
        }
        let parts: Vec<&str> = rest.split('/').collect();
        match (r.method.as_str(), parts.as_slice()) {
            ("POST", ["files"]) => self.start_create(r),
            ("PATCH", ["files", id]) => self.start_update(r, id),
            _ => error(405, "methodNotAllowed", "The stub has no such upload."),
        }
    }

    fn opened_session(&mut self, r: &Request, write: Write, rest: &str) -> Reply {
        let Some(total) = r
            .upload
            .get("x-upload-content-length")
            .and_then(|n| n.parse().ok())
        else {
            return invalid("X-Upload-Content-Length");
        };
        let fields = match Self::fields_of(r) {
            Ok(fields) => fields,
            Err(reply) => return reply,
        };
        self.next_session += 1;
        let id = self.next_session;
        self.sessions.insert(
            id,
            Session {
                write,
                total,
                got: Vec::new(),
                fields,
            },
        );
        let address = format!(
            "{}/upload/drive/v3/{rest}?uploadType=resumable&upload_id={id}",
            self.base
        );
        Reply(
            200,
            "application/json",
            Vec::new(),
            vec![("location".to_owned(), address)],
        )
    }

    fn start_update(&mut self, r: &Request, id: &str) -> Reply {
        if !r.body.is_empty() && Self::metadata(r).is_none_or(|m| !m.is_empty()) {
            return invalid("metadata");
        }
        let file = match self.file(id) {
            Ok(file) => file,
            Err(reply) => return reply,
        };
        if file.mime == FOLDER {
            return error(403, "fileNotWritable", "A folder has no content.");
        }
        let write = Write::Update(file.id.clone());
        self.opened_session(r, write, &format!("files/{id}"))
    }

    fn start_create(&mut self, r: &Request) -> Reply {
        let Some(metadata) = Self::metadata(r) else {
            return invalid("metadata");
        };
        if metadata
            .keys()
            .any(|k| !["name", "mimeType", "parents"].contains(&k.as_str()))
        {
            return invalid("metadata");
        }
        let text = |key: &str| {
            metadata
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
        };
        let (name, mime) = (text("name").to_owned(), text("mimeType").to_owned());
        // a Google type here would turn the upload into a Google file
        if name.is_empty() || mime.is_empty() || mime.starts_with("application/vnd.google-apps.") {
            return invalid("metadata");
        }
        let parents: Vec<String> = metadata
            .get("parents")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|p| p.as_str().map(str::to_owned))
            .collect();
        for parent in &parents {
            if parent != DRIVE_ROOT && !self.files.contains_key(parent) {
                return error(404, "notFound", &format!("File not found: {parent}."));
            }
        }
        let write = Write::Create {
            name,
            mime,
            parents,
        };
        self.opened_session(r, write, "files")
    }

    fn metadata(r: &Request) -> Option<Map<String, Value>> {
        match serde_json::from_slice(&r.body) {
            Ok(Value::Object(map)) if r.kind.starts_with("application/json") => Some(map),
            _ => None,
        }
    }

    /// One piece of a session. It has to start where the last one stopped, and
    /// say so; what's missing gets a 308 saying how far it got.
    fn piece(&mut self, r: &Request, id: &str) -> Reply {
        let Some(session) = id
            .parse()
            .ok()
            .and_then(|id: u64| self.sessions.get_mut(&id))
        else {
            return error(404, "notFound", "No such upload.");
        };
        let range = r
            .upload
            .get("content-range")
            .and_then(|range| range.strip_prefix("bytes "))
            .and_then(|range| range.split_once('/'))
            .and_then(|(span, total)| Some((span.split_once('-')?, total)))
            .and_then(|((from, to), total)| {
                Some((
                    from.parse::<usize>().ok()?,
                    to.parse::<usize>().ok()?,
                    total.parse::<usize>().ok()?,
                ))
            });
        let Some((from, to, total)) = range else {
            return invalid("Content-Range");
        };
        if from != session.got.len() || to + 1 != from + r.body.len() || total != session.total {
            return invalid("Content-Range");
        }
        let keep = self.keep_short.take().unwrap_or(r.body.len());
        session
            .got
            .extend_from_slice(&r.body[..keep.min(r.body.len())]);
        if session.got.len() < session.total {
            // with nothing kept there's nothing to say, and a 308 has no Range
            let held = (!session.got.is_empty()).then(|| {
                (
                    "range".to_owned(),
                    format!("bytes=0-{}", session.got.len() - 1),
                )
            });
            return Reply(308, "text/plain", Vec::new(), held.into_iter().collect());
        }
        let session = id
            .parse()
            .ok()
            .and_then(|id: u64| self.sessions.remove(&id))
            .expect("the session was just read");
        let reply = self.write(session);
        if std::mem::take(&mut self.garble_last) {
            return Reply(200, "application/json", b"<html>".to_vec(), Vec::new());
        }
        reply
    }

    fn write(&mut self, session: Session) -> Reply {
        self.used += session.got.len() as i64;
        let id = match session.write {
            Write::Update(id) => {
                let found = self.edit(&id, |file| {
                    file.content = session.got;
                    file.modified_ms += 1000;
                });
                if !found {
                    return error(404, "notFound", &format!("File not found: {id}."));
                }
                id
            }
            Write::Create {
                name,
                mime,
                parents,
            } => {
                self.next_copy += 1;
                let id = format!("drv-copy-{}", self.next_copy);
                let mut file = FakeFile::new(&id, &name, &mime);
                file.content = session.got;
                file.parents = if parents.is_empty() {
                    vec![DRIVE_ROOT.to_owned()]
                } else {
                    parents
                };
                self.add(file);
                id
            }
        };
        let v = resource(&self.files[&id]);
        match session.fields {
            Some(fields) if !fields.known_on_file() => invalid("fields"),
            Some(fields) => Reply::json(200, fields.pick(&v)),
            None => {
                let plain = Fields::parse("kind,id,name,mimeType").expect("a good field list");
                Reply::json(200, plain.pick(&v))
            }
        }
    }

    pub(crate) fn handle(&mut self, r: &Request, rest: &str) -> Reply {
        if r.method != "GET" {
            return error(405, "methodNotAllowed", "The stub has no such write.");
        }
        let parts: Vec<&str> = rest.split('/').collect();
        match parts.as_slice() {
            ["files"] => self.list(r),
            ["files", id] => self.get(r, id),
            ["files", id, "export"] => self.export(r, id),
            ["about"] => self.about(r),
            ["changes", "startPageToken"] => match Failing::due(&mut self.fail_changes) {
                Some(reply) => reply,
                None => Reply::json(
                    200,
                    json!({"kind": "drive#startPageToken", "startPageToken": self.token(self.changes.len())}),
                ),
            },
            ["changes"] => self.feed(r),
            _ => error(404, "notFound", "No such API in the stub."),
        }
    }

    fn token(&self, at: usize) -> String {
        format!("chg-{at}")
    }

    fn fields_of(r: &Request) -> Result<Option<Fields>, Reply> {
        r.param("fields")
            .map(|f| Fields::parse(&f).ok_or_else(|| invalid("fields")))
            .transpose()
    }

    fn order(files: &mut [&FakeFile], order: &str) -> Result<(), Reply> {
        let keys: Vec<&str> = order.split(',').map(str::trim).collect();
        for key in &keys {
            if !["folder", "name", "modifiedTime desc", ""].contains(key) {
                return Err(invalid("orderBy"));
            }
        }
        files.sort_by(|a, b| {
            let mut by = std::cmp::Ordering::Equal;
            for key in &keys {
                by = by.then_with(|| match *key {
                    "folder" => (b.mime == FOLDER).cmp(&(a.mime == FOLDER)),
                    "name" => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                    "modifiedTime desc" => b.modified_ms.cmp(&a.modified_ms),
                    _ => std::cmp::Ordering::Equal,
                });
            }
            by.then_with(|| a.id.cmp(&b.id))
        });
        Ok(())
    }

    fn list(&mut self, r: &Request) -> Reply {
        if let Some(reply) = Failing::due(&mut self.fail_list) {
            return reply;
        }
        let q = r.param("q").unwrap_or_default();
        let order = r.param("orderBy").unwrap_or_default();
        let query = if q.is_empty() {
            Query::All(Vec::new())
        } else {
            match parse(&q) {
                Some(query) => query,
                None => return invalid("q"),
            }
        };
        // Google answers 404 to a list of a folder it can't find
        for folder in query.folders() {
            if folder != "root" && folder != DRIVE_ROOT && !self.files.contains_key(folder) {
                return error(404, "notFound", "File not found: .");
            }
        }
        if query.full_text() && !order.is_empty() {
            return error(
                403,
                "sortingNotSupported",
                "Sorting is not supported for queries with fullText terms.",
            );
        }
        let size = match r.param("pageSize").map(|p| p.parse::<usize>()) {
            None => 100,
            Some(Ok(n @ 1..=1000)) => n,
            _ => return invalid("pageSize"),
        };
        let size = self.page.unwrap_or(size);
        let list = tag(&format!("{q}|{order}"));
        let from = match r.param("pageToken") {
            None => 0,
            Some(token) => match self.offset_of(&token, list) {
                Some(from) => from,
                None => return invalid("pageToken"),
            },
        };
        let fields = match Self::fields_of(r) {
            Ok(fields) => fields,
            Err(reply) => return reply,
        };
        let mut found: Vec<&FakeFile> = self.files.values().filter(|f| query.holds(f)).collect();
        if let Err(reply) = Self::order(&mut found, &order) {
            return reply;
        }
        let page: Vec<Value> = found
            .iter()
            .skip(from)
            .take(size)
            .map(|f| resource(f))
            .collect();
        let mut v = json!({"kind": "drive#fileList", "incompleteSearch": false});
        if from + size < found.len() {
            v["nextPageToken"] = json!(format!("pg-{}-{list:x}-{}", self.generation, from + size));
        }
        v["files"] = json!(page);
        match fields {
            // with no `fields`, Google gives a file's four plainest
            None => {
                let plain = Fields::parse("kind,id,name,mimeType").expect("a good field list");
                v["files"] = plain.pick(&v["files"]);
                Reply::json(200, v)
            }
            Some(fields) if fields.inner("files").is_some_and(|f| !f.known_on_file()) => {
                invalid("fields")
            }
            Some(fields) => Reply::json(200, fields.pick(&v)),
        }
    }

    fn offset_of(&self, token: &str, list: u64) -> Option<usize> {
        let mut parts = token.strip_prefix("pg-")?.split('-');
        let generation: u64 = parts.next()?.parse().ok()?;
        let tagged = u64::from_str_radix(parts.next()?, 16).ok()?;
        let from = parts.next()?.parse().ok()?;
        (generation == self.generation && tagged == list).then_some(from)
    }

    fn file(&self, id: &str) -> Result<&FakeFile, Reply> {
        let id = if id == "root" { DRIVE_ROOT } else { id };
        self.files
            .get(id)
            .ok_or_else(|| error(404, "notFound", &format!("File not found: {id}.")))
    }

    fn get(&self, r: &Request, id: &str) -> Reply {
        if id == "root" || id == DRIVE_ROOT {
            return Reply::json(200, json!({"id": DRIVE_ROOT, "name": "My Drive"}));
        }
        let file = match self.file(id) {
            Ok(file) => file,
            Err(reply) => return reply,
        };
        if r.param("alt").as_deref() == Some("media") {
            if file.google() {
                return error(
                    403,
                    "fileNotDownloadable",
                    "Only files with binary content can be downloaded. Use Export with Docs Editors files.",
                );
            }
            if !file.can_download {
                return error(403, "cannotDownloadFile", "This file can't be downloaded.");
            }
            return Reply(
                200,
                "application/octet-stream",
                file.content.clone(),
                Vec::new(),
            );
        }
        match Self::fields_of(r) {
            Err(reply) => reply,
            Ok(None) => {
                let plain = Fields::parse("kind,id,name,mimeType").expect("a good field list");
                Reply::json(200, plain.pick(&resource(file)))
            }
            Ok(Some(fields)) if !fields.known_on_file() => invalid("fields"),
            Ok(Some(fields)) => Reply::json(200, fields.pick(&resource(file))),
        }
    }

    fn export(&self, r: &Request, id: &str) -> Reply {
        let file = match self.file(id) {
            Ok(file) => file,
            Err(reply) => return reply,
        };
        if !file.google() {
            return error(
                403,
                "fileNotExportable",
                "Export only supports Docs Editors files.",
            );
        }
        let wanted = r.param("mimeType").unwrap_or_default();
        if file.content.len() > EXPORT_MAX {
            return error(
                403,
                "exportSizeLimitExceeded",
                "This file is too large to be exported.",
            );
        }
        // the HTML a person would be wrong to show: it reaches out to a server
        if wanted == "text/html" && file.mime == GOOGLE_DOC {
            let page = format!(
                "<html><body><img src=\"https://images.example.com/x.png\"><p>{}</p></body></html>",
                String::from_utf8_lossy(&file.content)
            );
            return Reply(200, "text/html", page.into_bytes(), Vec::new());
        }
        let office = match file.mime.as_str() {
            GOOGLE_DOC => DOCX,
            GOOGLE_SHEET => XLSX,
            GOOGLE_SLIDES => PPTX,
            _ => "",
        };
        if wanted == office {
            return Reply(
                200,
                "application/octet-stream",
                file.content.clone(),
                Vec::new(),
            );
        }
        let fits = match file.mime.as_str() {
            GOOGLE_DOC | GOOGLE_SLIDES => wanted == "text/plain",
            GOOGLE_SHEET => wanted == "text/csv",
            _ => false,
        };
        if !fits {
            return invalid("mimeType");
        }
        // Google's text exports start with a byte order mark
        let mut body = "\u{feff}".as_bytes().to_vec();
        body.extend_from_slice(&file.content);
        Reply(200, "text/plain", body, Vec::new())
    }

    fn about(&self, r: &Request) -> Reply {
        let fields = match Self::fields_of(r) {
            Ok(Some(fields)) => fields,
            // Google insists on `fields` here
            Ok(None) => return invalid("fields"),
            Err(reply) => return reply,
        };
        let v = json!({"kind": "drive#about", "user": {"displayName": OWNER_NAME, "emailAddress": FAKE_EMAIL},
            "storageQuota": {"limit": "16106127360", "usage": self.used.to_string(), "usageInDrive": self.used.to_string()}});
        Reply::json(200, fields.pick(&v))
    }

    fn feed(&mut self, r: &Request) -> Reply {
        if let Some(reply) = Failing::due(&mut self.fail_changes) {
            return reply;
        }
        let Some(from) = r
            .param("pageToken")
            .and_then(|t| t.strip_prefix("chg-")?.parse::<usize>().ok())
            .filter(|from| *from <= self.changes.len())
        else {
            return invalid("pageToken");
        };
        let fields = match Self::fields_of(r) {
            Ok(fields) => fields,
            Err(reply) => return reply,
        };
        let size = self.changes_page.unwrap_or(100);
        let to = (from + size).min(self.changes.len());
        let removed = r.param("includeRemoved").as_deref() != Some("false");
        let mut changes = Vec::new();
        for (at, id) in self.changes.iter().enumerate().take(to).skip(from) {
            // only a file's latest change is reported, as Google does
            if self.changes[at + 1..to].contains(id) {
                continue;
            }
            changes.push(match self.files.get(id) {
                Some(f) => {
                    json!({"kind": "drive#change", "changeType": "file", "fileId": id, "removed": false, "file": resource(f)})
                }
                None if removed => {
                    json!({"kind": "drive#change", "changeType": "file", "fileId": id, "removed": true})
                }
                None => continue,
            });
        }
        let mut v = json!({"kind": "drive#changeList", "changes": changes});
        if to < self.changes.len() {
            v["nextPageToken"] = json!(self.token(to));
        } else {
            v["newStartPageToken"] = json!(self.token(to));
        }
        match fields {
            Some(fields) => {
                let asked = fields.inner("changes").and_then(|c| c.inner("file"));
                if asked.is_some_and(|f| !f.known_on_file()) {
                    return invalid("fields");
                }
                Reply::json(200, fields.pick(&v))
            }
            None => Reply::json(200, v),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(q: &str, f: &FakeFile) -> bool {
        parse(q).unwrap().holds(f)
    }

    #[test]
    fn a_string_ends_only_at_a_quote_nothing_escapes() {
        let f = FakeFile::new("a", r"it's a \ thing", "text/plain");
        assert!(matches(r"name = 'it\'s a \\ thing'", &f));
        // the same words, unescaped, are a broken query
        assert_eq!(parse(r"name = 'it's a'"), None);
        assert_eq!(parse(r"name contains 'x\'"), None);
    }

    #[test]
    fn a_query_that_breaks_out_really_does_widen() {
        let mut trashed = FakeFile::new("a", "secret", "text/plain");
        trashed.trashed = true;
        assert!(!matches(
            "name contains 'nothing' and trashed = false",
            &trashed
        ));
        assert!(matches(
            "name contains 'nothing' or trashed = true and trashed = true",
            &trashed
        ));
    }

    #[test]
    fn fields_keep_only_what_was_asked() {
        let f = Fields::parse("a,b(c)").unwrap();
        assert_eq!(
            f.pick(&json!({"a": 1, "b": [{"c": 2, "d": 3}], "z": 4})),
            json!({"a": 1, "b": [{"c": 2}]})
        );
        assert_eq!(Fields::parse("a,(b"), None);
    }

    fn request(method: &str, target: &str, body: &[u8], upload: &[(&str, String)]) -> Request {
        Request {
            method: method.to_owned(),
            target: target.to_owned(),
            authorization: String::new(),
            kind: "application/json; charset=UTF-8".to_owned(),
            if_match: String::new(),
            upload: upload
                .iter()
                .map(|(k, v)| ((*k).to_owned(), v.clone()))
                .collect(),
            body: body.to_vec(),
        }
    }

    fn start(drive: &mut Drive, method: &str, rest: &str, body: &[u8], total: usize) -> String {
        let r = request(
            method,
            &format!("/upload/drive/v3/{rest}?uploadType=resumable"),
            body,
            &[("x-upload-content-length", total.to_string())],
        );
        let Reply(status, _, _, headers) = drive.upload(&r, rest);
        assert_eq!(status, 200);
        let at = headers[0].1.split("/upload/drive/v3/").nth(1).unwrap();
        format!("/upload/drive/v3/{at}")
    }

    fn put(drive: &mut Drive, at: &str, from: usize, bytes: &[u8], total: usize) -> Reply {
        let range = format!("bytes {from}-{}/{total}", from + bytes.len() - 1);
        let r = request("PUT", at, bytes, &[("content-range", range)]);
        drive.upload(&r, "files")
    }

    #[test]
    fn a_write_moves_version_and_checksum_only_at_its_last_piece() {
        let mut drive = Drive::new(vec![
            FakeFile::new("a", "a.txt", "text/plain").holding(b"x"),
        ]);
        let before = drive.held("a").unwrap();
        let at = start(&mut drive, "PATCH", "files/a", b"{}", 4);
        assert_eq!(put(&mut drive, &at, 0, b"ab", 4).0, 308);
        assert_eq!(drive.held("a").unwrap(), before, "nothing written yet");
        assert_eq!(
            put(&mut drive, &at, 0, b"ab", 4).0,
            400,
            "not where it stopped"
        );
        assert_eq!(put(&mut drive, &at, 2, b"cd", 4).0, 200);
        let after = drive.held("a").unwrap();
        assert_eq!((after.content.as_slice(), after.version), (&b"abcd"[..], 2));
        assert_ne!(after.checksum(), before.checksum());
    }

    #[test]
    fn an_upload_it_doesnt_know_is_refused() {
        let mut drive = Drive::new(vec![FakeFile::new("a", "a.txt", "text/plain")]);
        let plain = request(
            "PATCH",
            "/upload/drive/v3/files/a?uploadType=media",
            b"x",
            &[],
        );
        assert_eq!(drive.upload(&plain, "files/a").0, 400);
        let length = [("x-upload-content-length", "1".to_owned())];
        for body in [
            r#"{"name": "x"}"#,
            r#"{"name": "x", "mimeType": "application/vnd.google-apps.document"}"#,
            r#"{"name": "x", "mimeType": "text/plain", "writersCanShare": true}"#,
            r#"{"name": "x", "mimeType": "text/plain", "parents": ["nowhere"]}"#,
        ] {
            let r = request(
                "POST",
                "/upload/drive/v3/files?uploadType=resumable",
                body.as_bytes(),
                &length,
            );
            assert!(drive.upload(&r, "files").0 >= 400, "{body}");
        }
        assert_eq!(drive.all().len(), 1);
    }

    #[test]
    fn the_live_seed_has_every_kind_and_is_the_same_each_time() {
        let all = demo();
        assert_eq!(all, demo());
        for mime in [
            FOLDER,
            GOOGLE_DOC,
            GOOGLE_SHEET,
            GOOGLE_SLIDES,
            DOCX,
            XLSX,
            "application/pdf",
            "image/png",
        ] {
            assert!(all.iter().any(|f| f.mime == mime), "{mime}");
        }
        assert!(all.iter().any(|f| f.trashed));
        assert!(all.iter().any(|f| f.starred));
        assert!(all.iter().any(|f| f.shared_with_me));
        assert!(all.iter().any(|f| f.parents == ["drv-reyes"]), "nested");
        let people: Vec<&String> = all.iter().map(|f| &f.owner.1).collect();
        assert!(people.iter().all(|p| p.ends_with("@example.com")));
    }

    #[test]
    fn a_big_folder_is_that_many_with_folders_in_among_them() {
        let all = big_folder("big", "Big", 5000);
        assert_eq!(all.len(), 5001);
        assert_eq!(all.iter().filter(|f| f.mime == FOLDER).count(), 101);
        let ids: std::collections::BTreeSet<&String> = all.iter().map(|f| &f.id).collect();
        assert_eq!(ids.len(), 5001);
    }
}
