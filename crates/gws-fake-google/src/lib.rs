//! A stub Google on 127.0.0.1, for tests and test Bots: sign-in, and Gmail,
//! with fake people and fake mail. It keeps a log of every request, so a
//! test can prove what reached "Google" and, as importantly, what didn't.
//! Two ways in:
//!
//! - [`FakeGoogle`]: a whole fake account, which behaves as Google does.
//! - [`serve`]: one route function, for a single odd answer (a 403, a 500).
//!
//! Nothing here is ever part of a release.

use std::collections::BTreeMap;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use serde_json::{Value, json};

/// The account every fake sign-in is.
pub const FAKE_EMAIL: &str = "owner@example.com";
const GMAIL: &str = "/gmail/v1/users/me";

/// A request the stub saw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen {
    pub method: String,
    /// The path and query, as sent. [`serve`] strips the Gmail prefix from it.
    pub target: String,
    pub authorization: String,
    pub body: String,
}

/// One HTTP answer: status, content type, body.
struct Reply(u16, &'static str, Vec<u8>, Vec<(String, String)>);

impl Reply {
    fn json(status: u16, v: Value) -> Self {
        Self(
            status,
            "application/json",
            v.to_string().into_bytes(),
            Vec::new(),
        )
    }
}

struct Request {
    method: String,
    target: String,
    authorization: String,
    /// The `Content-Type`.
    kind: String,
    body: Vec<u8>,
}

impl Request {
    fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or_default()
    }

    fn query(&self) -> BTreeMap<String, Vec<String>> {
        let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for pair in self
            .target
            .split_once('?')
            .map_or("", |(_, q)| q)
            .split('&')
            .filter(|p| !p.is_empty())
        {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            out.entry(decode(k)).or_default().push(decode(v));
        }
        out
    }

    fn param(&self, key: &str) -> Option<String> {
        self.query().remove(key).and_then(|mut v| v.pop())
    }
}

fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    let (mut out, mut i) = (Vec::with_capacity(bytes.len()), 0);
    while i < bytes.len() {
        match (
            bytes[i],
            bytes.get(i + 1).copied().and_then(hex),
            bytes.get(i + 2).copied().and_then(hex),
        ) {
            (b'+', ..) => out.push(b' '),
            (b'%', Some(high), Some(low)) => {
                out.push(high * 16 + low);
                i += 2;
            }
            (b, ..) => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Reads one request from `stream`.
fn read_request(stream: &TcpStream) -> Option<Request> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.split(' ');
    let (method, target) = (parts.next()?.to_owned(), parts.next()?.to_owned());
    let (mut authorization, mut kind, mut length) = (String::new(), String::new(), 0usize);
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            if k.eq_ignore_ascii_case("authorization") {
                authorization = v.trim().to_owned();
            } else if k.eq_ignore_ascii_case("content-type") {
                kind = v.trim().to_owned();
            } else if k.eq_ignore_ascii_case("content-length") {
                length = v.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).ok()?;
    Some(Request {
        method,
        target,
        authorization,
        kind,
        body,
    })
}

fn write_reply(mut stream: TcpStream, r: Reply) {
    let Reply(status, kind, body, headers) = r;
    let extra: String = headers
        .iter()
        .map(|(k, v)| format!("{k}: {v}\r\n"))
        .collect();
    let head = format!(
        "HTTP/1.1 {status} Fake\r\ncontent-type: {kind}\r\ncontent-length: {}\r\nconnection: close\r\n{extra}\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&body);
}

fn listen(port: u16, handle: impl Fn(Request) -> Reply + Send + Sync + 'static) -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", port)).expect("the stub's port is free");
    let port = listener.local_addr().expect("a bound address").port();
    let handle = Arc::new(handle);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let handle = handle.clone();
            std::thread::spawn(move || {
                if let Some(request) = read_request(&stream) {
                    write_reply(stream, handle(request));
                }
            });
        }
    });
    port
}

/// A fake answering one route function.
pub struct Fake {
    /// The API base to give `Gmail::new`.
    pub base: String,
    pub seen: Arc<Mutex<Vec<Seen>>>,
}

impl Fake {
    pub fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

/// A fake answering `route(target)` with `(status, JSON body)`.
pub fn serve(route: impl Fn(&str) -> (u16, String) + Send + Sync + 'static) -> Fake {
    let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
    let log = seen.clone();
    let port = listen(0, move |r| {
        let target = r.target.trim_start_matches(GMAIL).to_owned();
        log.lock().unwrap().push(Seen {
            method: r.method,
            target: target.clone(),
            authorization: r.authorization,
            body: String::from_utf8_lossy(&r.body).into_owned(),
        });
        let (status, body) = route(&target);
        Reply(status, "application/json", body.into_bytes(), Vec::new())
    });
    Fake {
        base: format!("http://127.0.0.1:{port}{GMAIL}"),
        seen,
    }
}

/// A fake mail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mail {
    pub id: String,
    pub thread: String,
    pub labels: Vec<String>,
    pub from: String,
    pub to: String,
    pub subject: String,
    /// Unix milliseconds.
    pub at: i64,
    pub text: String,
    pub html: Option<String>,
    /// `(file name, mime, bytes)`.
    pub attachments: Vec<(String, String, Vec<u8>)>,
    /// Its `Message-ID`, for a search by it.
    pub message_id: String,
}

/// What a fake account starts with.
#[derive(Debug, Clone, Default)]
pub struct Seed {
    pub mails: Vec<Mail>,
    /// The scope a sign-in gets whatever it asks for, as for someone who
    /// signed in before an update asked for more.
    pub only_grants: Option<String>,
}

const PEOPLE: [(&str, &str); 8] = [
    ("Ana Reyes", "ana@example.com"),
    ("Ben Cruz", "ben@example.com"),
    ("Cara Lim", "cara@example.com"),
    ("Dan Sy", "dan@example.com"),
    ("Eva Tan", "eva@example.com"),
    ("Finn Yu", "finn@example.org"),
    ("Billing", "billing@shop.example"),
    ("Weekly Digest", "news@digest.example"),
];
const SUBJECTS: [&str; 8] = [
    "Lunch on Friday",
    "Roof quote, updated",
    "Your October receipt",
    "Five things this week",
    "Contract draft for review",
    "Photos from Saturday",
    "Office closed on the 14th",
    "Invoice 1042",
];
/// 2026-11-06 09:00 UTC.
const NOW_MS: i64 = 1_793_955_600_000;

impl Seed {
    /// `n` fake mails, newest first, some in threads of two, unread and starred here and there.
    pub fn demo(n: usize) -> Self {
        let mails = (0..n)
            .map(|i| {
                let (name, address) = PEOPLE[i % PEOPLE.len()];
                let thread = i / 2 * 2;
                let subject = SUBJECTS[(i / 2) % SUBJECTS.len()];
                let mut labels = vec!["INBOX".to_owned(), "CATEGORY_PERSONAL".to_owned()];
                if i % 3 == 0 {
                    labels.push("UNREAD".to_owned());
                }
                if i % 7 == 0 {
                    labels.push("STARRED".to_owned());
                }
                Mail {
                    id: format!("{:016x}", 0x18f0_0000_0000_u64 + (n - i) as u64),
                    thread: format!("{:016x}", 0x18f0_0000_0000_u64 + (n - thread) as u64),
                    labels,
                    from: format!("{name} <{address}>"),
                    to: FAKE_EMAIL.to_owned(),
                    subject: if i % 2 == 1 { format!("Re: {subject}") } else { subject.to_owned() },
                    at: NOW_MS - i as i64 * 3_600_000,
                    text: format!("Hello,\n\nThis is fake mail number {i} from {name}.\n\nThanks"),
                    html: (i % 2 == 0).then(|| format!("<html><body><p>Hello,</p><p>This is <b>fake</b> mail number {i} from {name}.</p></body></html>")),
                    attachments: Vec::new(),
                    message_id: String::new(),
                }
            })
            .collect();
        Self {
            mails,
            only_grants: None,
        }
    }
}

/// A mail built to reach out, run and mislead, aimed at this stub so its log
/// shows anything that got through. `{stub}` is the stub's own address.
pub const HOSTILE_HTML: &str = r##"<meta http-equiv="refresh" content="0;url={stub}/__refresh">
<script>new Image().src='{stub}/__script'</script>
<style>body{background:url({stub}/__css)}</style>
<link rel=stylesheet href="{stub}/__link.css">
<img src="{stub}/__pixel.png" onerror="location='{stub}/__onerror'" width=40 height=40 alt="tracking pixel">
<iframe src="{stub}/__iframe"></iframe>
<form action="{stub}/__form"><input name=q value=x><input type=submit value=Go></form>
<div style="background:url({stub}/__style)">HOSTILE-BODY-TEXT from Mallory Fake</div>
<p><a href="gws-link:0" style="font-size:24px">FORGED LINK</a></p>
<p><a href="{stub}/__clicked" style="font-size:24px">REAL LINK</a></p>"##;

impl Seed {
    /// Adds the hostile mail, newest of all.
    pub fn with_hostile(mut self, stub: &str) -> Self {
        self.mails.insert(
            0,
            Mail {
                id: "0000000000hostile".into(),
                thread: "0000000000hostile".into(),
                labels: vec!["INBOX".into(), "UNREAD".into()],
                from: "Mallory Fake <mallory@evil.example>".into(),
                to: FAKE_EMAIL.into(),
                subject: "Hostile test mail".into(),
                at: NOW_MS + 3_600_000,
                text: "plain".into(),
                html: Some(HOSTILE_HTML.replace("{stub}", stub)),
                attachments: Vec::new(),
                message_id: String::new(),
            },
        );
        self
    }
}

fn rfc2822(ms: i64) -> String {
    // every fake date is close enough to this form for a mail client
    let days = ms.div_euclid(86_400_000);
    let secs = ms.div_euclid(1000).rem_euclid(86_400);
    let (y, m, d) = civil(days);
    let months = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let weekdays = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    format!(
        "{}, {d:02} {} {y} {:02}:{:02}:{:02} +0000",
        weekdays[days.rem_euclid(7) as usize],
        months[(m - 1) as usize],
        secs / 3600,
        secs / 60 % 60,
        secs % 60
    )
}

/// Days since 1970-01-01 as (year, month, day).
fn civil(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn headers(m: &Mail) -> Value {
    json!([{"name": "From", "value": m.from}, {"name": "To", "value": m.to}, {"name": "Subject", "value": m.subject}, {"name": "Date", "value": rfc2822(m.at)}])
}

fn snippet(m: &Mail) -> String {
    m.text.replace('\n', " ").chars().take(120).collect()
}

fn payload(m: &Mail, full: bool) -> Value {
    if !full {
        return json!({"mimeType": "multipart/alternative", "headers": headers(m)});
    }
    let text = json!({"mimeType": "text/plain", "headers": [{"name": "Content-Type", "value": "text/plain; charset=utf-8"}], "body": {"data": b64(m.text.as_bytes()), "size": m.text.len()}});
    let mut parts = vec![text];
    if let Some(html) = &m.html {
        parts.push(json!({"mimeType": "text/html", "headers": [{"name": "Content-Type", "value": "text/html; charset=utf-8"}], "body": {"data": b64(html.as_bytes()), "size": html.len()}}));
    }
    let body = json!({"mimeType": "multipart/alternative", "parts": parts});
    if m.attachments.is_empty() {
        return json!({"mimeType": "multipart/alternative", "headers": headers(m), "parts": body["parts"]});
    }
    let mut mixed = vec![body];
    for (i, (name, mime, bytes)) in m.attachments.iter().enumerate() {
        mixed.push(json!({"mimeType": mime, "filename": name, "headers": [{"name": "Content-Disposition", "value": format!("attachment; filename=\"{name}\"")}], "body": {"attachmentId": format!("att-{}-{i}", m.id), "size": bytes.len()}}));
    }
    json!({"mimeType": "multipart/mixed", "headers": headers(m), "parts": mixed})
}

#[derive(Default)]
struct Account {
    mails: Vec<Mail>,
    /// How many more writes go through; `None` is no limit.
    writes_left: Option<usize>,
    wrote: bool,
    reads_fail_after_write: bool,
    /// `(draft id, message id, raw message, thread)`.
    drafts: Vec<(String, String, String, Option<String>)>,
    next_draft: u64,
    sent: Vec<Sent>,
    history: i64,
    /// `(history id, change)`, oldest first.
    changes: Vec<(i64, Value)>,
    /// History ids below this are gone, as an old one is at Google.
    oldest_history: i64,
    token_count: u64,
    granted: String,
    only_grants: Option<String>,
    log: Vec<Seen>,
}

/// A message sent through the stub, as it arrived.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Sent {
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub subject: String,
    pub body: String,
    pub message_id: String,
    pub in_reply_to: String,
    /// `(file name, type, bytes)`.
    pub attachments: Vec<(String, String, Vec<u8>)>,
}

/// A header's value in a block of headers.
fn header_in(head: &str, name: &str) -> String {
    let wanted = format!("{}:", name.to_lowercase());
    head.lines()
        .find(|l| l.to_lowercase().starts_with(&wanted))
        .map(|l| l[wanted.len()..].trim().to_owned())
        .unwrap_or_default()
}

/// What a base64 body holds, however it's wrapped.
fn unwrapped(body: &str) -> Vec<u8> {
    use base64::Engine as _;
    let plain: String = body.split_whitespace().collect();
    base64::engine::general_purpose::STANDARD
        .decode(plain)
        .unwrap_or_default()
}

/// A boundary named in a `Content-Type`.
fn boundary_of(kind: &str) -> Option<String> {
    let (_, rest) = kind.split_once("boundary=")?;
    Some(rest.split(';').next()?.trim().trim_matches('"').to_owned())
}

/// A multipart body's parts, each with its headers still on, as bytes: a part
/// can be a whole message.
fn parts_of(body: &[u8], boundary: &str) -> Vec<Vec<u8>> {
    let mark = format!("--{boundary}").into_bytes();
    let mut parts = Vec::new();
    let mut from = None;
    let mut i = 0;
    while i + mark.len() <= body.len() {
        if body[i..].starts_with(&mark) {
            if let Some(start) = from {
                let end = i.saturating_sub(2).max(start);
                parts.push(body[start..end].to_vec());
            }
            i += mark.len();
            if body[i..].starts_with(b"--") {
                return parts;
            }
            i += 2;
            from = Some(i);
        } else {
            i += 1;
        }
    }
    parts
}

impl Sent {
    fn of(raw: &str) -> Sent {
        use base64::Engine as _;
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(raw)
            .unwrap_or_default();
        let message = String::from_utf8_lossy(&bytes).into_owned();
        let (head, body) = message.split_once("\r\n\r\n").unwrap_or((&message, ""));
        let field = |name: &str| {
            head.lines()
                .find_map(|l| l.strip_prefix(&format!("{name}: ")))
                .unwrap_or_default()
                .to_owned()
        };
        let plain = |value: String| match value
            .strip_prefix("=?UTF-8?B?")
            .and_then(|v| v.strip_suffix("?="))
        {
            Some(encoded) => base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .ok()
                .and_then(|b| String::from_utf8(b).ok())
                .unwrap_or(value),
            None => value,
        };
        let mut attachments = Vec::new();
        let mut text = String::from_utf8(unwrapped(body)).unwrap_or_default();
        if let Some(boundary) = boundary_of(&header_in(head, "Content-Type")) {
            for (at, part) in parts_of(body.as_bytes(), &boundary).iter().enumerate() {
                let part = String::from_utf8_lossy(part);
                let (head, bytes) = part.split_once("\r\n\r\n").unwrap_or((&part, ""));
                if at == 0 {
                    text = String::from_utf8(unwrapped(bytes)).unwrap_or_default();
                    continue;
                }
                let disposition = header_in(head, "Content-Disposition");
                let name = match disposition.split_once("filename*=UTF-8''") {
                    Some((_, name)) => decode(name),
                    None => disposition
                        .split_once("filename=")
                        .map(|(_, n)| n.trim_matches('"').to_owned())
                        .unwrap_or_default(),
                };
                let kind = header_in(head, "Content-Type");
                attachments.push((name, kind, unwrapped(bytes)));
            }
        }
        Sent {
            to: field("To"),
            cc: field("Cc"),
            bcc: field("Bcc"),
            subject: plain(field("Subject")),
            body: text,
            message_id: field("Message-ID"),
            in_reply_to: field("In-Reply-To"),
            attachments,
        }
    }

    fn resource(&self, id: &str, thread: Option<&str>) -> Value {
        let header = |name: &str, value: &str| json!({"name": name, "value": value});
        let headers = json!([
            header("To", &self.to),
            header("Cc", &self.cc),
            header("Bcc", &self.bcc),
            header("Subject", &self.subject),
            header("Message-ID", &self.message_id),
        ]);
        let text = json!({"mimeType": "text/plain", "body": {"data": b64(self.body.as_bytes())}});
        if self.attachments.is_empty() {
            let mut payload = text;
            payload["headers"] = headers;
            return json!({"id": id, "threadId": thread, "payload": payload});
        }
        let mut parts = vec![text];
        for (at, (name, kind, bytes)) in self.attachments.iter().enumerate() {
            parts.push(json!({"mimeType": kind, "filename": name, "body": {"attachmentId": format!("att-{at}"), "size": bytes.len()}}));
        }
        json!({"id": id, "threadId": thread, "payload": {"mimeType": "multipart/mixed", "headers": headers, "parts": parts}})
    }
}

impl Account {
    /// The People API's two searches: contacts, and who the person's mail knows.
    fn people(&mut self, r: &Request) -> Reply {
        if self
            .only_grants
            .as_deref()
            .is_some_and(|g| !g.contains("contacts"))
        {
            return Self::insufficient();
        }
        let query = r.param("query").unwrap_or_default().to_lowercase();
        let found: Vec<Value> = [("Zed Contact", "zed@contacts.example")]
            .iter()
            .filter(|(name, address)| {
                !query.is_empty()
                    && (name.to_lowercase().contains(&query) || address.contains(&query))
            })
            .map(|(name, address)| json!({"person": {"names": [{"displayName": name}], "emailAddresses": [{"value": address}]}}))
            .collect();
        Reply::json(200, json!({"results": found}))
    }

    /// Gmail's upload door: a metadata part and the message itself, handed on
    /// as the same draft write its plain door takes.
    fn upload(&mut self, mut r: Request, rest: &str) -> Reply {
        use base64::Engine as _;
        let parts = boundary_of(&r.kind)
            .map(|b| parts_of(&r.body, &b))
            .unwrap_or_default();
        let [metadata, message] = parts.as_slice() else {
            return Reply::json(
                400,
                json!({"error": {"code": 400, "message": "Bad upload."}}),
            );
        };
        let after_head = |part: &[u8]| {
            let at = part.windows(4).position(|w| w == b"\r\n\r\n").unwrap_or(0);
            part[at + 4..].to_vec()
        };
        let mut body: Value = serde_json::from_slice(&after_head(metadata)).unwrap_or(json!({}));
        body["message"]["raw"] = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(after_head(message))
            .into();
        r.body = body.to_string().into_bytes();
        self.gmail(&r, rest)
    }

    fn lacks_modify(&self) -> bool {
        self.only_grants
            .as_deref()
            .is_some_and(|g| !g.contains("gmail.modify"))
    }

    fn insufficient() -> Reply {
        Reply::json(
            403,
            json!({"error": {"code": 403, "message": "Request had insufficient authentication scopes."}}),
        )
    }

    fn resource(&self, m: &Mail, format: &str, wanted: &[String]) -> Value {
        let mut v = json!({"id": m.id, "threadId": m.thread, "labelIds": m.labels, "snippet": snippet(m), "historyId": self.history.to_string(), "internalDate": m.at.to_string(), "sizeEstimate": m.text.len()});
        match format {
            "minimal" => {}
            "metadata" => {
                let mut p = payload(m, false);
                if !wanted.is_empty() {
                    let kept: Vec<Value> = p["headers"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|h| {
                            wanted.iter().any(|w| {
                                h["name"]
                                    .as_str()
                                    .is_some_and(|n| n.eq_ignore_ascii_case(w))
                            })
                        })
                        .cloned()
                        .collect();
                    p["headers"] = json!(kept);
                }
                v["payload"] = p;
            }
            _ => v["payload"] = payload(m, true),
        }
        v
    }

    fn matches(m: &Mail, q: &str, labels: &[String]) -> bool {
        if !labels.iter().all(|l| m.labels.contains(l)) {
            return false;
        }
        // one `{a b}` group: any of its terms will do
        let (q, any) = match q.split_once('{').zip(q.split_once('}')) {
            Some(((before, rest), (_, after))) => {
                let group = rest.split('}').next().unwrap_or_default();
                (format!("{before} {after}"), Some(group.to_owned()))
            }
            None => (q.to_owned(), None),
        };
        if let Some(any) = any
            && !any.split_whitespace().any(|t| Self::matches(m, t, &[]))
        {
            return false;
        }
        q.split_whitespace().all(|term| {
            let term = term.to_lowercase();
            if term == "in:sent" {
                return m.labels.iter().any(|l| l == "SENT");
            }
            if let Some(to) = term.strip_prefix("to:") {
                return m.to.to_lowercase().contains(to.trim_matches('"'));
            }
            if let Some(from) = term.strip_prefix("from:") {
                return m.from.to_lowercase().contains(from);
            }
            if let Some(label) = term.strip_prefix("label:") {
                return m.labels.iter().any(|l| l.eq_ignore_ascii_case(label));
            }
            if let Some(what) = term.strip_prefix("is:") {
                return match what {
                    "unread" => m.labels.iter().any(|l| l == "UNREAD"),
                    "starred" => m.labels.iter().any(|l| l == "STARRED"),
                    _ => true,
                };
            }
            if let Some(id) = term.strip_prefix("rfc822msgid:") {
                return m.message_id.eq_ignore_ascii_case(id);
            }
            if term.contains(':') {
                return true;
            }
            m.subject.to_lowercase().contains(&term)
                || m.from.to_lowercase().contains(&term)
                || m.text.to_lowercase().contains(&term)
        })
    }

    fn page(found: Vec<Value>, key: &str, start: usize, max: usize) -> Value {
        let total = found.len();
        let page: Vec<Value> = found.into_iter().skip(start).take(max).collect();
        let mut v = json!({"resultSizeEstimate": total});
        if !page.is_empty() {
            v[key] = json!(page);
        }
        if start + max < total {
            v["nextPageToken"] = json!((start + max).to_string());
        }
        v
    }

    fn gmail(&mut self, r: &Request, path: &str) -> Reply {
        let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let not_found = || {
            Reply::json(
                404,
                json!({"error": {"code": 404, "message": "Requested entity was not found."}}),
            )
        };
        let wanted = r.query().remove("metadataHeaders").unwrap_or_default();
        let format = r.param("format").unwrap_or_else(|| "full".to_owned());
        let labels = r.query().remove("labelIds").unwrap_or_default();
        let (q, start) = (
            r.param("q").unwrap_or_default(),
            r.param("pageToken")
                .and_then(|t| t.parse().ok())
                .unwrap_or(0),
        );
        let max = r
            .param("maxResults")
            .and_then(|t| t.parse().ok())
            .unwrap_or(100)
            .min(500);
        if self.reads_fail_after_write && self.wrote && r.method == "GET" && format == "metadata" {
            return Reply::json(
                500,
                json!({"error": {"code": 500, "message": "Backend error"}}),
            );
        }
        match (r.method.as_str(), segs.as_slice()) {
            ("GET", ["profile"]) => Reply::json(
                200,
                json!({"emailAddress": FAKE_EMAIL, "messagesTotal": self.mails.len(), "threadsTotal": self.mails.len(), "historyId": self.history.to_string()}),
            ),
            ("GET", ["labels"]) => {
                let mut list: Vec<Value> = [
                    "INBOX",
                    "SENT",
                    "DRAFT",
                    "TRASH",
                    "SPAM",
                    "STARRED",
                    "UNREAD",
                    "IMPORTANT",
                    "CATEGORY_PERSONAL",
                ]
                .iter()
                .map(|l| json!({"id": l, "name": l, "type": "system"}))
                .collect();
                list.push(json!({"id": "Label_1", "name": "Work", "type": "user"}));
                list.push(json!({"id": "Label_2", "name": "Receipts", "type": "user"}));
                Reply::json(200, json!({"labels": list}))
            }
            ("GET", ["labels", id]) => {
                let total = self
                    .mails
                    .iter()
                    .filter(|m| m.labels.iter().any(|l| l == id))
                    .count();
                let unread = self
                    .mails
                    .iter()
                    .filter(|m| {
                        m.labels.iter().any(|l| l == id) && m.labels.iter().any(|l| l == "UNREAD")
                    })
                    .count();
                let (name, kind) = match *id {
                    "Label_1" => ("Work", "user"),
                    "Label_2" => ("Receipts", "user"),
                    other => (other, "system"),
                };
                Reply::json(
                    200,
                    json!({"id": id, "name": name, "type": kind, "messagesTotal": total, "messagesUnread": unread, "threadsTotal": total, "threadsUnread": unread}),
                )
            }
            ("GET", ["messages"]) => {
                let found = self
                    .mails
                    .iter()
                    .filter(|m| Self::matches(m, &q, &labels))
                    .map(|m| json!({"id": m.id, "threadId": m.thread}))
                    .collect();
                Reply::json(200, Self::page(found, "messages", start, max))
            }
            ("GET", ["messages", id]) => match self.mails.iter().find(|m| m.id == *id) {
                Some(m) => Reply::json(200, self.resource(m, &format, &wanted)),
                None => not_found(),
            },
            ("GET", ["messages", id, "attachments", att]) => {
                let at = att.rsplit('-').next().and_then(|i| i.parse::<usize>().ok());
                let held = self
                    .drafts
                    .iter()
                    .find(|d| d.1 == *id)
                    .map(|d| Sent::of(&d.2));
                let found = match &held {
                    Some(draft) => at.and_then(|i| draft.attachments.get(i)),
                    None => self
                        .mails
                        .iter()
                        .find(|m| m.id == *id)
                        .and_then(|m| at.and_then(|i| m.attachments.get(i))),
                };
                match found {
                    Some((_, _, bytes)) => {
                        Reply::json(200, json!({"size": bytes.len(), "data": b64(bytes)}))
                    }
                    None => not_found(),
                }
            }
            ("GET", ["threads"]) => {
                let mut seen = Vec::new();
                let mut found = Vec::new();
                for m in self.mails.iter().filter(|m| Self::matches(m, &q, &labels)) {
                    if !seen.contains(&m.thread) {
                        seen.push(m.thread.clone());
                        found.push(json!({"id": m.thread, "snippet": snippet(m), "historyId": self.history.to_string()}));
                    }
                }
                Reply::json(200, Self::page(found, "threads", start, max))
            }
            ("GET", ["threads", id]) => {
                let mut mails: Vec<&Mail> = self.mails.iter().filter(|m| m.thread == *id).collect();
                if mails.is_empty() {
                    return not_found();
                }
                mails.sort_by_key(|m| m.at);
                let messages: Vec<Value> = mails
                    .iter()
                    .map(|m| self.resource(m, &format, &wanted))
                    .collect();
                Reply::json(
                    200,
                    json!({"id": id, "historyId": self.history.to_string(), "messages": messages}),
                )
            }
            ("POST", ["drafts"]) | ("PUT", ["drafts", _]) => {
                if self.lacks_modify() {
                    return Self::insufficient();
                }
                let body: Value = serde_json::from_slice(&r.body).unwrap_or(Value::Null);
                let raw = body
                    .pointer("/message/raw")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let thread = body
                    .pointer("/message/threadId")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                self.next_draft += 1;
                let message = format!("m{}", self.next_draft);
                let id = match segs.as_slice() {
                    [_, id] => {
                        let Some(held) = self.drafts.iter_mut().find(|d| d.0 == *id) else {
                            return not_found();
                        };
                        held.1 = message.clone();
                        held.2 = raw;
                        held.3 = thread;
                        (*id).to_owned()
                    }
                    _ => {
                        let id = format!("d{}", self.next_draft);
                        self.drafts.push((id.clone(), message.clone(), raw, thread));
                        id
                    }
                };
                Reply::json(200, json!({"id": id, "message": {"id": message}}))
            }
            ("GET", ["drafts"]) => {
                let listed: Vec<Value> = self
                    .drafts
                    .iter()
                    .map(|(id, message, _, thread)| json!({"id": id, "message": {"id": message, "threadId": thread}}))
                    .collect();
                Reply::json(200, json!({"drafts": listed}))
            }
            ("GET", ["drafts", id]) => match self.drafts.iter().find(|d| d.0 == *id) {
                Some((id, message, raw, thread)) => {
                    let sent = Sent::of(raw);
                    Reply::json(
                        200,
                        json!({"id": id, "message": sent.resource(message, thread.as_deref())}),
                    )
                }
                None => not_found(),
            },
            ("DELETE", ["drafts", id]) => {
                let before = self.drafts.len();
                self.drafts.retain(|d| d.0 != *id);
                if self.drafts.len() == before {
                    return not_found();
                }
                Reply(204, "text/plain", Vec::new(), Vec::new())
            }
            ("POST", ["drafts", "send"]) => {
                if self.lacks_modify() {
                    return Self::insufficient();
                }
                let body: Value = serde_json::from_slice(&r.body).unwrap_or(Value::Null);
                let id = body["id"].as_str().unwrap_or_default();
                let Some(at) = self.drafts.iter().position(|d| d.0 == id) else {
                    return not_found();
                };
                let (_, message, raw, thread) = self.drafts.remove(at);
                let sent = Sent::of(&raw);
                self.history += 1;
                let thread = thread.unwrap_or_else(|| message.clone());
                self.mails.insert(
                    0,
                    Mail {
                        id: message.clone(),
                        thread: thread.clone(),
                        labels: vec!["SENT".to_owned()],
                        from: FAKE_EMAIL.to_owned(),
                        to: sent.to.clone(),
                        subject: sent.subject.clone(),
                        at: 1_793_955_600_000,
                        text: sent.body.clone(),
                        html: None,
                        attachments: sent.attachments.clone(),
                        message_id: sent.message_id.clone(),
                    },
                );
                self.sent.push(sent);
                Reply::json(200, json!({"id": message, "threadId": thread}))
            }
            ("POST", ["threads", id, action]) | ("POST", ["messages", id, action]) => {
                if self.writes_left == Some(0) {
                    return Reply::json(
                        500,
                        json!({"error": {"code": 500, "message": "Backend error"}}),
                    );
                }
                if let Some(left) = &mut self.writes_left {
                    *left -= 1;
                }
                self.wrote = true;
                if self
                    .only_grants
                    .as_deref()
                    .is_some_and(|g| !g.contains("gmail.modify"))
                {
                    return Reply::json(
                        403,
                        json!({"error": {"code": 403, "message": "Request had insufficient authentication scopes."}}),
                    );
                }
                let by_thread = segs[0] == "threads";
                let body: Value = serde_json::from_slice(&r.body).unwrap_or(Value::Null);
                let list = |k: &str| -> Vec<String> {
                    body[k]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|l| l.as_str().map(str::to_owned))
                        .collect()
                };
                let (mut add, mut remove) = (list("addLabelIds"), list("removeLabelIds"));
                match *action {
                    "modify" => {}
                    "trash" => {
                        add.push("TRASH".to_owned());
                        remove.push("INBOX".to_owned());
                    }
                    "untrash" => remove.push("TRASH".to_owned()),
                    _ => return not_found(),
                }
                let mut touched = Vec::new();
                for m in &mut self.mails {
                    if (by_thread && m.thread == *id) || (!by_thread && m.id == *id) {
                        m.labels.retain(|l| !remove.contains(l));
                        for l in &add {
                            if !m.labels.contains(l) {
                                m.labels.push(l.clone());
                            }
                        }
                        touched.push(json!({"message": {"id": m.id, "threadId": m.thread}}));
                    }
                }
                if touched.is_empty() {
                    return not_found();
                }
                self.history += 1;
                let history = self.history;
                self.changes.push((
                    history,
                    json!({"id": history.to_string(), "labelsAdded": touched}),
                ));
                Reply::json(200, json!({"id": id}))
            }
            ("GET", ["history"]) => {
                let since: i64 = r
                    .param("startHistoryId")
                    .and_then(|h| h.parse().ok())
                    .unwrap_or(0);
                if since < self.oldest_history {
                    return not_found();
                }
                let list: Vec<Value> = self
                    .changes
                    .iter()
                    .filter(|(id, _)| *id > since)
                    .map(|(_, c)| c.clone())
                    .collect();
                let mut v = json!({"historyId": self.history.to_string()});
                if !list.is_empty() {
                    v["history"] = json!(list);
                }
                Reply::json(200, v)
            }
            _ => not_found(),
        }
    }

    fn token(&mut self, r: &Request) -> Reply {
        let form = String::from_utf8_lossy(&r.body).into_owned();
        let param = |k: &str| {
            form.split('&')
                .find_map(|p| p.strip_prefix(&format!("{k}=")).map(decode))
        };
        self.token_count += 1;
        let claims = b64(json!({"email": FAKE_EMAIL, "sub": "1000"})
            .to_string()
            .as_bytes());
        let scope = self.only_grants.clone().unwrap_or_else(|| {
            if self.granted.is_empty() {
                param("scope").unwrap_or_default()
            } else {
                self.granted.clone()
            }
        });
        let mut v = json!({"access_token": format!("fake-access-{}", self.token_count), "token_type": "Bearer", "expires_in": 3599, "scope": scope, "id_token": format!("e30.{claims}.c2ln")});
        if param("grant_type").as_deref() == Some("authorization_code") {
            v["refresh_token"] = json!("fake-refresh");
        }
        Reply::json(200, v)
    }

    fn authorize(&mut self, r: &Request) -> Reply {
        // the person's consent: straight back to the app with a code
        let (redirect, state) = (
            r.param("redirect_uri").unwrap_or_default(),
            r.param("state").unwrap_or_default(),
        );
        self.granted = r.param("scope").unwrap_or_default();
        let sep = if redirect.contains('?') { '&' } else { '?' };
        Reply(
            302,
            "text/plain",
            Vec::new(),
            vec![(
                "location".to_owned(),
                format!("{redirect}{sep}code=fake-code&state={state}"),
            )],
        )
    }

    fn handle(&mut self, r: Request) -> Reply {
        let path = r.path().to_owned();
        if path != "/__log" && path != "/__writes" {
            self.log.push(Seen {
                method: r.method.clone(),
                target: r.target.clone(),
                authorization: r.authorization.clone(),
                body: String::from_utf8_lossy(&r.body).into_owned(),
            });
        }
        if path == "/__log" {
            let log: Vec<Value> = self
                .log
                .iter()
                .map(|s| json!({"method": s.method, "target": s.target, "body": s.body}))
                .collect();
            return Reply::json(200, json!({"log": log}));
        }
        // a live run's way to make writes fail: `?left=<n>`, or no value for none
        if path == "/__writes" {
            self.writes_left = r.param("left").and_then(|n| n.parse().ok());
            return Reply::json(200, json!({"left": self.writes_left}));
        }
        if path == "/o/oauth2/v2/auth" {
            return self.authorize(&r);
        }
        if path == "/token" {
            return self.token(&r);
        }
        if !r.authorization.starts_with("Bearer fake-") {
            return Reply::json(
                401,
                json!({"error": {"code": 401, "message": "Request had invalid authentication credentials."}}),
            );
        }
        if let Some(rest) = path.strip_prefix(GMAIL) {
            return self.gmail(&r, rest);
        }
        if path.starts_with("/people/v1/") {
            return self.people(&r);
        }
        if let Some(rest) = path
            .strip_prefix("/upload")
            .and_then(|p| p.strip_prefix(GMAIL))
        {
            let rest = rest.to_owned();
            return self.upload(r, &rest);
        }
        Reply::json(
            404,
            json!({"error": {"code": 404, "message": "No such API in the stub."}}),
        )
    }
}

/// A whole fake Google account on a port of its own.
pub struct FakeGoogle {
    port: u16,
    account: Arc<Mutex<Account>>,
}

impl FakeGoogle {
    pub fn start(seed: Seed) -> Self {
        Self::start_on(0, seed)
    }

    pub fn start_on(port: u16, seed: Seed) -> Self {
        let account = Arc::new(Mutex::new(Account {
            mails: seed.mails,
            history: 1000,
            oldest_history: 0,
            only_grants: seed.only_grants,
            ..Account::default()
        }));
        let shared = account.clone();
        let port = listen(port, move |r| shared.lock().unwrap().handle(r));
        Self { port, account }
    }

    /// `http://127.0.0.1:<port>`: what `GWS_STUB_BASE` names.
    pub fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Every request so far, oldest first.
    pub fn log(&self) -> Vec<Seen> {
        self.account.lock().unwrap().log.clone()
    }

    /// The requests that would change something at Google.
    pub fn writes(&self) -> Vec<Seen> {
        self.log()
            .into_iter()
            .filter(|s| s.method != "GET" && !s.target.starts_with("/token"))
            .collect()
    }

    /// What was sent, oldest first.
    pub fn sent(&self) -> Vec<Sent> {
        self.account.lock().unwrap().sent.clone()
    }

    /// Each mail's labels, sorted: what a test compares across an undo.
    pub fn mailbox_labels(&self) -> Value {
        let a = self.account.lock().unwrap();
        let mails: Vec<Value> = a
            .mails
            .iter()
            .map(|m| {
                let mut labels = m.labels.clone();
                labels.sort();
                json!({"id": m.id, "labels": labels})
            })
            .collect();
        json!(mails)
    }

    /// After `n` more writes, every write fails.
    pub fn writes_fail_after(&self, n: usize) {
        self.account.lock().unwrap().writes_left = Some(n);
    }

    /// Writes work again.
    pub fn writes_work(&self) {
        self.account.lock().unwrap().writes_left = None;
    }

    /// Once something is written, reading a thread's metadata fails.
    pub fn reads_fail_after_a_write(&self) {
        self.account.lock().unwrap().reads_fail_after_write = true;
    }

    /// From now on a token covers `scope` alone, and writes need more.
    pub fn only_grants(&self, scope: &str) {
        self.account.lock().unwrap().only_grants = Some(scope.to_owned());
    }

    /// A new mail arrives, as Gmail's history shows it.
    pub fn deliver(&self, mail: Mail) {
        let mut a = self.account.lock().unwrap();
        a.history += 1;
        let (id, thread, labels) = (mail.id.clone(), mail.thread.clone(), mail.labels.clone());
        let history = a.history;
        a.changes.push((history, json!({"id": history.to_string(), "messagesAdded": [{"message": {"id": id, "threadId": thread, "labelIds": labels}}]})));
        a.mails.insert(0, mail);
    }

    /// Forgets history before now, as Google does after a while.
    pub fn expire_history(&self) {
        let mut a = self.account.lock().unwrap();
        a.oldest_history = a.history + 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_read_like_mail_dates() {
        assert_eq!(rfc2822(NOW_MS), "Fri, 06 Nov 2026 09:00:00 +0000");
    }

    #[test]
    fn percent_decoding_keeps_text() {
        assert_eq!(decode("from%3Aana+x"), "from:ana x");
        assert_eq!(decode("%aé%"), "%aé%", "a stray percent is kept");
    }
}
