//! Pictures in a mail. The reader's page loads nothing (`img-src data:`), so
//! every picture it shows comes in here as a `data:` URL, and the cleaner
//! drops any other.
//!
//! A picture sent inside the mail, by its Content-ID, is the mail's own and
//! shows at once. A picture on the web shows only when the person presses
//! Show pictures, or opens mail from a sender they said always to show. The
//! plugin fetches those itself, under the limits below, and the page stays
//! sealed. Nothing the model calls reaches a fetch: the tools here are the
//! screen's alone.
//!
//! What's picked goes in each message under [`KEY`], where [`inline`] finds it
//! for [`clean::clean_with`], in page order while the view still fits a result.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs as _};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::{Value, json};
use ureq::Agent;
use ureq::http::Uri;
use ureq::unversioned::resolver::{ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{DefaultConnector, NextTimeout};
use url::Url;

use crate::accounts::Bearer;
use crate::apis::Apis;
use crate::gmail::{self, Failure};
use crate::mail::{self, signed_in, with_own_store, with_store};
use crate::tools::{
    MAIL_PICTURE_SENDER_ADD, MAIL_PICTURE_SENDER_REMOVE, MAIL_PICTURES, fail, failed, ok,
};
use crate::{Core, clean, js};

/// The largest picture taken, inside a mail or from the web, by its bytes.
pub const PICTURE_MAX: usize = 2 * 1024 * 1024;
/// The most web pictures fetched for one message.
pub const PER_MAIL: usize = 40;
/// The most web pictures fetched for one press, whatever the thread.
pub const PER_THREAD: usize = 80;
/// The most calls to Gmail for the pictures inside a thread's mail.
pub const CID_CALLS: usize = 20;
/// The most redirects one picture may take, each checked as the first was.
pub const REDIRECTS: usize = 3;
/// How long one picture may take, redirects and all.
pub const TIMEOUT: Duration = Duration::from_secs(10);
/// Pictures fetched at once.
const AT_ONCE: usize = 8;
/// The whole press: the Bot gives up on a call after 30 seconds.
const ALL_WITHIN: Duration = Duration::from_secs(25);
/// What a view with its pictures may come to: a result stops at 1 MiB.
const ROOM: usize = 880 * 1024;
/// Where a message carries the pictures picked for it. Gmail's messages have
/// no such field, and any found is dropped before anything is put there.
const KEY: &str = "gwsPictures";
/// Says nothing about the person or their account.
const USER_AGENT: &str = "Mozilla/5.0 (compatible; Buwiz Bot)";
const ACCEPT: &str = "image/png,image/jpeg,image/gif,image/webp";
/// The longest address a sender can be trusted by.
const ADDRESS_MAX: usize = 254;

/// The kind of picture `bytes` are, by their first bytes alone: what a mail or
/// a server says of them is a stranger's word. Never SVG, which can carry
/// markup of its own.
pub fn kind_of(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Some("image/webp")
    } else {
        None
    }
}

/// `bytes` as a `data:` URL, if they're a picture within [`PICTURE_MAX`].
pub fn data_url(bytes: &[u8]) -> Option<String> {
    if bytes.len() > PICTURE_MAX {
        return None;
    }
    let kind = kind_of(bytes)?;
    let data = base64::engine::general_purpose::STANDARD.encode(bytes);
    Some(format!("data:{kind};base64,{data}"))
}

/// The `data:` URL picked for `url` in `message`, for [`clean::clean_with`].
pub fn inline(message: &Value, url: &str) -> Option<String> {
    message.get(KEY)?.get(url)?.as_str().map(str::to_owned)
}

/// The pictures `message`'s HTML asks for, as the cleaner names them.
fn wanted(message: &Value) -> Vec<String> {
    gmail::html_body(message).map_or_else(Vec::new, |h| clean::clean(&h).images)
}

/// `cid:<id>`'s id, as a part's `Content-ID` gives it: no angle brackets, and
/// the URL's percent escapes undone.
fn cid_of(url: &str) -> Option<String> {
    let rest = url
        .get(..4)
        .filter(|s| s.eq_ignore_ascii_case("cid:"))
        .map(|_| &url[4..])?;
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
        match (bytes[at], bytes.get(at + 1), bytes.get(at + 2)) {
            (b'%', Some(&h), Some(&l)) if hex(h).is_some() && hex(l).is_some() => {
                out.push(hex(h)? * 16 + hex(l)?);
                at += 3;
            }
            (b, ..) => {
                out.push(b);
                at += 1;
            }
        }
    }
    let id = String::from_utf8(out).ok()?;
    let id = id
        .trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .trim();
    (!id.is_empty()).then(|| id.to_owned())
}

fn part_header<'a>(part: &'a Value, name: &str) -> Option<&'a str> {
    part.get("headers")?
        .as_array()?
        .iter()
        .find(|h| {
            h.get("name")
                .and_then(Value::as_str)
                .is_some_and(|n| n.eq_ignore_ascii_case(name))
        })?
        .get("value")?
        .as_str()
}

/// Every part of `message` with a Content-ID, by that id.
fn by_content_id(message: &Value) -> HashMap<String, &Value> {
    fn walk<'a>(part: &'a Value, out: &mut HashMap<String, &'a Value>) {
        if let Some(id) = part_header(part, "content-id") {
            let id = id
                .trim()
                .trim_start_matches('<')
                .trim_end_matches('>')
                .trim();
            if !id.is_empty() {
                out.entry(id.to_owned()).or_insert(part);
            }
        }
        for p in part
            .get("parts")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            walk(p, out);
        }
    }
    let mut out = HashMap::new();
    if let Some(p) = message.get("payload") {
        walk(p, &mut out);
    }
    out
}

/// Gmail's base64url, padded or not.
fn unbase64(data: &str) -> Option<Vec<u8>> {
    let clean: String = data
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '=')
        .map(|c| match c {
            '+' => '-',
            '/' => '_',
            c => c,
        })
        .collect();
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(clean)
        .ok()
}

/// An attachment id fit to go in a path: Gmail's, which can run long.
fn attachment_ok(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 4096
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// A part's picture: its bytes inline, else from the attachment API within
/// `by`, and nothing past [`PICTURE_MAX`] fetched at all. `calls` counts the
/// attachment calls, which stop at [`CID_CALLS`]. Nothing is fetched that
/// `fits` says can't be kept, and `gate` hears only of what's asked for.
/// `short` is set when Google said too little of a part to fetch it by, which
/// another read may not.
#[allow(clippy::too_many_arguments)]
fn part_picture(
    core: &Core,
    auth: &Bearer,
    message: &str,
    part: &Value,
    by: Instant,
    calls: &mut usize,
    fits: &dyn Fn(u64) -> bool,
    gate: &mut dyn FnMut(u64) -> bool,
    short: &mut bool,
) -> Option<String> {
    if let Some(data) = part.pointer("/body/data").and_then(Value::as_str) {
        // base64 of more than the cap can't decode to within it
        if data.len() > PICTURE_MAX / 3 * 4 + 8 {
            return None;
        }
        return data_url(&unbase64(data)?);
    }
    let size = part.pointer("/body/size").and_then(Value::as_u64);
    let id = part.pointer("/body/attachmentId").and_then(Value::as_str);
    let (size, id) = match (size, id) {
        (Some(size), Some(id)) => (size, id),
        // an empty part is no picture
        (Some(0), None) => return None,
        _ => {
            *short = true;
            return None;
        }
    };
    if size > PICTURE_MAX as u64 || !attachment_ok(id) || !gmail::id_ok(message) {
        return None;
    }
    if *calls >= CID_CALLS || !fits(size) {
        return None;
    }
    // a press out of time asks for nothing, and so charges nothing
    let left = left(by)?;
    if !gate(size) {
        return None;
    }
    *calls += 1;
    let bytes = core
        .gmail
        .attachment_within(auth, message, id, left)
        .ok()??;
    data_url(&bytes)
}

/// What's left of the time until `by`, if any is.
fn left(by: Instant) -> Option<Duration> {
    by.checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
}

fn messages_mut(thread: &mut Value) -> &mut [Value] {
    match thread.get_mut("messages").and_then(Value::as_array_mut) {
        Some(list) => list,
        None => &mut [],
    }
}

fn messages(thread: &Value) -> &[Value] {
    thread
        .get("messages")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// The smallest picture there is, as a `data:` URL: with less room than this
/// left, nothing more can go in.
const SMALLEST: usize = 64;

/// Where the pictures picked for a thread wait to go in: in the order they're
/// offered, while the view stays within [`ROOM`]. A picture shown twice costs
/// twice. What doesn't fit goes at once, so no more than fits is held.
struct Placer {
    room: usize,
    /// Each message's pictures, as the cleaner names them, read once.
    uses: Vec<Vec<String>>,
    picks: Vec<(usize, String, String)>,
}

impl Placer {
    /// For `thread` as it is, with nothing placed in it.
    fn new(thread: &Value) -> Self {
        let base = mail::view_of(thread).to_string().len();
        Self {
            room: ROOM.saturating_sub(base),
            uses: messages(thread).iter().map(wanted).collect(),
            picks: Vec::new(),
        }
    }

    /// How many times `url` shows in `messages`.
    fn times(&self, messages: &[usize], url: &str) -> usize {
        messages
            .iter()
            .map(|&m| self.uses[m].iter().filter(|u| *u == url).count().max(1))
            .sum()
    }

    /// Whether a picture of `size` bytes for `url` in `messages` could be
    /// taken: asked before it's fetched, so none is fetched only to stay out.
    fn fits(&self, messages: &[usize], url: &str, size: u64) -> bool {
        // the longest kind's `data:` prefix, and the base64 of `size`
        let data = "data:image/jpeg;base64,".len() as u64 + size.div_ceil(3) * 4;
        (data + 2).saturating_mul(self.times(messages, url) as u64) <= self.room as u64
    }

    /// Takes `data` for `url` in each of `messages`, if it all fits.
    fn offer(&mut self, messages: &[usize], url: &str, data: String) -> bool {
        let cost = (data.len() + 2) * self.times(messages, url);
        if cost > self.room {
            return false;
        }
        self.room -= cost;
        for &m in messages {
            self.picks.push((m, url.to_owned(), data.clone()));
        }
        true
    }

    fn full(&self) -> bool {
        self.room < SMALLEST
    }

    /// Puts what was taken into `thread`'s messages.
    fn finish(self, thread: &mut Value) {
        let list = messages_mut(thread);
        for (m, url, data) in self.picks {
            let message = &mut list[m];
            if !message.get(KEY).is_some_and(Value::is_object) {
                message[KEY] = json!({});
            }
            message[KEY][url] = Value::String(data);
        }
    }
}

/// The pictures inside `thread`'s mail, by Content-ID, offered to `placer` by
/// `by`, with no more than [`CID_CALLS`] calls to Gmail for them. True when
/// Google said too little of one to fetch it by.
fn inside(
    core: &Core,
    auth: &Bearer,
    thread: &Value,
    placer: &mut Placer,
    by: Instant,
    gate: &mut dyn FnMut(u64) -> bool,
) -> bool {
    let mut short = false;
    let mut calls = 0;
    for (n, m) in messages(thread).iter().enumerate() {
        let parts = by_content_id(m);
        if parts.is_empty() {
            continue;
        }
        let id = m.get("id").and_then(Value::as_str).unwrap_or_default();
        let mut seen = HashSet::new();
        for url in placer.uses[n].clone() {
            if placer.full() {
                return short;
            }
            let Some(part) = cid_of(&url).and_then(|cid| parts.get(&cid).copied()) else {
                continue;
            };
            if !seen.insert(url.clone()) {
                continue;
            }
            let room = |size| placer.fits(&[n], &url, size);
            let picture = part_picture(
                core, auth, id, part, by, &mut calls, &room, gate, &mut short,
            );
            if let Some(data) = picture {
                placer.offer(&[n], &url, data);
            }
        }
    }
    short
}

/// The parts of `message` its page shows by Content-ID: named by a picture in
/// its HTML, and within [`PICTURE_MAX`], whatever their disposition says. By
/// Gmail's part id, the root's as `root`.
pub(crate) fn shown_inside(message: &Value) -> HashSet<String> {
    let parts = by_content_id(message);
    if parts.is_empty() {
        return HashSet::new();
    }
    wanted(message)
        .iter()
        .filter_map(|url| parts.get(&cid_of(url)?).copied())
        .filter(|part| {
            part.pointer("/body/data").is_some()
                || part
                    .pointer("/body/size")
                    .and_then(Value::as_u64)
                    .is_some_and(|size| size <= PICTURE_MAX as u64)
        })
        .filter_map(|part| match part.get("partId")?.as_str()? {
            "" => Some("root".to_owned()),
            id => Some(id.to_owned()),
        })
        .collect()
}

/// Takes out anything that looks like what [`Placer`] puts in.
fn unplace(thread: &mut Value) {
    for m in messages_mut(thread) {
        if let Some(m) = m.as_object_mut() {
            m.remove(KEY);
        }
    }
}

/// An address fit to trust a sender by: lower case, and nothing in it but an
/// address could hold.
fn address_ok(address: &str) -> bool {
    address.len() <= ADDRESS_MAX
        && address
            .split_once('@')
            .is_some_and(|(who, at)| !who.is_empty() && !at.is_empty())
        && !address
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || "<>\"'(),;:\\[]".contains(c))
}

/// A message's From address, in lower case, if it's one.
fn from_of(message: &Value) -> Option<String> {
    let (_, address) = mail::sender_of(&gmail::short_header(message, "From"));
    let address = address.to_lowercase();
    address_ok(&address).then_some(address)
}

/// `text` without its `( … )` comments, which say nothing that counts.
fn no_comments(text: &str) -> String {
    let (mut out, mut depth) = (String::with_capacity(text.len()), 0usize);
    for c in text.chars() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            c if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

/// Whether `domain` is `signer` or below it: what DKIM's alignment allows.
fn aligned(domain: &str, signer: &str) -> bool {
    let signer = signer.trim_end_matches('.').to_ascii_lowercase();
    !signer.is_empty() && (domain == signer || domain.ends_with(&format!(".{signer}")))
}

/// Whether Gmail vouches for `message`'s From: its own Authentication-Results
/// say DMARC passed for the From domain, or DKIM passed for a domain the From
/// domain is or is below. Gmail's is the first of them and names
/// mx.google.com; any further down came with the mail, from anyone.
pub fn vouched(message: &Value) -> bool {
    let Some(domain) = from_of(message).and_then(|a| Some(a.rsplit_once('@')?.1.to_owned())) else {
        return false;
    };
    let results = message
        .pointer("/payload/headers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|h| {
            h.get("name")
                .and_then(Value::as_str)
                .is_some_and(|n| n.eq_ignore_ascii_case("authentication-results"))
        })
        .and_then(|h| h.get("value"))
        .and_then(Value::as_str)
        .map(no_comments);
    let Some(results) = results else {
        return false;
    };
    let mut parts = results.split(';');
    let by = parts.next().unwrap_or_default().split_whitespace().next();
    if !by.is_some_and(|b| b.eq_ignore_ascii_case("mx.google.com")) {
        return false;
    }
    parts.any(|part| {
        let words: Vec<String> = part
            .split_whitespace()
            .map(str::to_ascii_lowercase)
            .collect();
        let property = |name: &str| {
            words
                .iter()
                .find_map(|w| w.strip_prefix(name).map(str::to_owned))
        };
        match words.first().map(String::as_str) {
            Some("dmarc=pass") => property("header.from=").is_some_and(|d| d == domain),
            Some("dkim=pass") => property("header.d=")
                .or_else(|| {
                    property("header.i=").and_then(|i| Some(i.rsplit_once('@')?.1.to_owned()))
                })
                .is_some_and(|signer| aligned(&domain, &signer)),
            _ => false,
        }
    })
}

/// Who the pictures are from, and whether Gmail vouches for them: the newest
/// message neither sent nor written by the person. None if every one is theirs.
fn sender(thread: &Value, account: &str) -> Option<(String, bool)> {
    let account = account.to_lowercase();
    let theirs = |m: &&Value| {
        !m.get("labelIds")
            .and_then(Value::as_array)
            .is_some_and(|l| l.iter().any(|l| l == "SENT"))
            && from_of(m).is_some_and(|a| a != account)
    };
    let m = messages(thread).iter().rev().find(theirs)?;
    Some((from_of(m)?, vouched(m)))
}

/// What the view says of who its pictures are from.
fn named(thread: &Value, account: &str) -> Value {
    match sender(thread, account) {
        Some((from, vouched)) => json!({"from": from, "vouched": vouched}),
        None => json!({"from": null, "vouched": false}),
    }
}

/// The thread's view, with the pictures inside its mail shown, and who its
/// pictures are from: for the thread view, which the store keeps. `gate` is
/// asked before each picture is fetched from Gmail, with its size: a read
/// ahead keeps to its round's bytes and its sign-in by it. True beside it when
/// Google said too little of a picture to fetch it by.
pub fn view_within(
    core: &Core,
    auth: &Bearer,
    account: &str,
    thread: &mut Value,
    gate: &mut dyn FnMut(u64) -> bool,
) -> (Value, bool) {
    let by = Instant::now() + ALL_WITHIN;
    unplace(thread);
    let mut placer = Placer::new(thread);
    let short = inside(core, auth, thread, &mut placer, by, gate);
    placer.finish(thread);
    let mut view = mail::view_of(thread);
    view["pictures"] = named(thread, account);
    (view, short)
}

/// `view` with whether its sender is trusted now, as a tool's answer: only a
/// sender Gmail vouches for can be. Not `ok`: that sends the view twice, and
/// pictures make it big.
pub fn answer(core: &Core, account: &str, mut view: Value) -> Value {
    let from = view
        .pointer("/pictures/from")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let vouched = view.pointer("/pictures/vouched") == Some(&Value::Bool(true));
    let trusted = vouched
        && from.is_some_and(|from| {
            with_store(core, |s| s.sender_trusted(account, &from)).unwrap_or(false)
        });
    if view.get("pictures").is_some_and(Value::is_object) {
        view["pictures"]["trusted"] = Value::Bool(trusted);
    }
    json!({"content": [{"type": "text", "text": "A conversation."}], "structuredContent": view})
}

// --- The web -------------------------------------------------------------------

/// Whether `ip` is an address on the public internet: not this Mac, not the
/// local network, not the carrier's, and not one address for many.
pub fn public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => public_v4(v4),
        IpAddr::V6(v6) => public_v6(v6),
    }
}

fn public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(a == 0
        || a == 10
        || a == 127
        || (a == 100 && (64..128).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..32).contains(&b))
        || (a == 192 && b == 168)
        || (a == 192 && b == 0 && (c == 0 || c == 2))
        || (a == 192 && b == 88 && c == 99)
        || (a == 198 && (18..20).contains(&b))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
        || a >= 224)
}

fn public_v6(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    // an IPv4 address carried in an IPv6 one is that IPv4 address
    let carried = |hi: u16, lo: u16| {
        let [a, b] = hi.to_be_bytes();
        let [c, d] = lo.to_be_bytes();
        Ipv4Addr::new(a, b, c, d)
    };
    if let Some(v4) = ip.to_ipv4_mapped() {
        return public_v4(v4);
    }
    match s {
        // unspecified, loopback, and the old IPv4-compatible form
        [0, 0, 0, 0, 0, 0, hi, lo] => hi != 0 && public_v4(carried(hi, lo)),
        // NAT64
        [0x64, 0xff9b, 0, 0, 0, 0, hi, lo] => public_v4(carried(hi, lo)),
        // a local NAT64's own prefix (RFC 8215), and SIIT's IPv4-translated
        // form: what's behind either is someone's network, not the internet
        [0x64, 0xff9b, 1, ..] | [0, 0, 0, 0, 0xffff, 0, ..] => false,
        // 6to4
        [0x2002, hi, lo, ..] => public_v4(carried(hi, lo)),
        _ => {
            let first = s[0];
            !((first & 0xfe00) == 0xfc00 // unique local
                || (first & 0xffc0) == 0xfe80 // link local
                || (first & 0xffc0) == 0xfec0 // site local, as it was
                || (first & 0xff00) == 0xff00 // multicast
                || (first == 0x2001 && s[1] == 0) // Teredo
                || (first == 0x2001 && s[1] == 0x0db8) // documentation
                || (first == 0x0100 && s[1..4] == [0, 0, 0])) // discard
        }
    }
}

/// Names a picture's host to the addresses it may be fetched from, and to
/// nothing if any is refused. The connection goes to exactly what this
/// returns, so a name can't move to another address between check and use.
#[derive(Debug)]
struct Checked {
    stub: Option<SocketAddr>,
}

impl Resolver for Checked {
    fn resolve(
        &self,
        uri: &Uri,
        _config: &ureq::config::Config,
        timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, ureq::Error> {
        let (Some(host), Some(scheme)) = (uri.host(), uri.scheme_str()) else {
            return Err(ureq::Error::HostNotFound);
        };
        let port = uri
            .port_u16()
            .unwrap_or(if scheme == "http" { 80 } else { 443 });
        let wait = if timeout.after.is_not_happening() {
            TIMEOUT
        } else {
            (*timeout.after).min(TIMEOUT)
        };
        let name = format!("{host}:{port}");
        let (tx, rx) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let _ = tx.send(name.to_socket_addrs().map(Vec::from_iter));
        });
        let found = match rx.recv_timeout(wait) {
            Ok(Ok(found)) => found,
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => return Err(ureq::Error::Timeout(timeout.reason)),
        };
        let allowed = |a: &SocketAddr| public(a.ip()) || Some(*a) == self.stub;
        if found.is_empty() || !found.iter().all(allowed) {
            return Err(ureq::Error::HostNotFound);
        }
        let mut out = self.empty();
        for a in found.into_iter().take(16) {
            out.push(a);
        }
        Ok(out)
    }
}

/// What fetches web pictures, and from where.
pub struct Fetcher {
    /// The one place plain http and a loopback address are allowed: the stub
    /// Google, in a `stub` build alone.
    stub: Option<SocketAddr>,
    timeout: Duration,
}

impl Fetcher {
    /// In a release, https to the public internet only. A `stub` build also
    /// reaches the stub Google the APIs point at, which serves the rich
    /// newsletter's pictures.
    pub fn new(apis: &Apis) -> Self {
        #[cfg(feature = "stub")]
        let stub = Url::parse(&apis.gmail)
            .ok()
            .filter(|u| u.scheme() == "http" && u.host_str() == Some("127.0.0.1"))
            .and_then(|u| Some(SocketAddr::from(([127, 0, 0, 1], u.port()?))));
        #[cfg(not(feature = "stub"))]
        let stub = {
            let _ = apis;
            None
        };
        Self {
            stub,
            timeout: TIMEOUT,
        }
    }

    #[cfg(test)]
    fn allowing(stub: SocketAddr, timeout: Duration) -> Self {
        Self {
            stub: Some(stub),
            timeout,
        }
    }

    /// No cookies (ureq keeps none without its `cookies` feature), no proxy,
    /// no redirect it follows itself, and no Referer: ureq sends none.
    fn agent(&self) -> Agent {
        let config = Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .max_redirects_will_error(false)
            .save_redirect_history(false)
            .proxy(None)
            .user_agent(USER_AGENT)
            .accept(ACCEPT)
            .max_idle_connections_per_host(AT_ONCE)
            .build();
        Agent::with_parts(
            config,
            DefaultConnector::default(),
            Checked { stub: self.stub },
        )
    }

    /// Whether `url` may be asked for at all: https, a host and no name and
    /// password in front of it; plain http only to the stub.
    fn url_ok(&self, url: &Url) -> bool {
        let plain = url.username().is_empty() && url.password().is_none();
        let stub = self.stub.is_some_and(|stub| {
            url.host_str() == Some(&stub.ip().to_string())
                && url.port_or_known_default() == Some(stub.port())
        });
        plain
            && url.host_str().is_some()
            && match url.scheme() {
                "https" => true,
                "http" => stub,
                _ => false,
            }
    }

    /// One picture as a `data:` URL, by `deadline`.
    ///
    /// Privacy: this fetch comes from this computer, so the sender can see that
    /// the mail was opened, when, and from which internet address. Gmail hides
    /// that behind its own proxy; the Bot has none. That's why it waits for the
    /// person's press, or their word that this sender may always know.
    fn fetch(&self, agent: &Agent, start: &str, deadline: Instant) -> Option<String> {
        let mut url = Url::parse(start.trim()).ok()?;
        for _ in 0..=REDIRECTS {
            url.set_fragment(None);
            if !self.url_ok(&url) {
                return None;
            }
            let left = deadline
                .checked_duration_since(Instant::now())
                .filter(|d| !d.is_zero())?;
            let mut res = agent
                .get(url.as_str())
                .config()
                .timeout_global(Some(left))
                .build()
                .call()
                .ok()?;
            match res.status().as_u16() {
                200 => {
                    let bytes = res
                        .body_mut()
                        .with_config()
                        // one byte past the cap, so a picture just over it is seen to be
                        .limit(PICTURE_MAX as u64 + 1)
                        .read_to_vec()
                        .ok()?;
                    return data_url(&bytes);
                }
                301 | 302 | 303 | 307 | 308 => {
                    let to = res.headers().get("location")?.to_str().ok()?;
                    url = url.join(to).ok()?;
                }
                _ => return None,
            }
        }
        None
    }

    /// Each of `urls` fetched, a few at once, all by `by`: each that comes
    /// goes straight to `take`, which says whether to go on. Once it says no,
    /// no more are asked for.
    fn fetch_all(&self, urls: &[&str], by: Instant, take: impl Fn(usize, String) -> bool + Sync) {
        let agent = self.agent();
        let next = Mutex::new(0usize);
        let stop = AtomicBool::new(false);
        std::thread::scope(|s| {
            for _ in 0..AT_ONCE.min(urls.len()) {
                s.spawn(|| {
                    while !stop.load(Ordering::Relaxed) {
                        let at = {
                            let mut next = next.lock().unwrap_or_else(|e| e.into_inner());
                            *next += 1;
                            *next - 1
                        };
                        let Some(url) = urls.get(at) else { break };
                        let one_by = (Instant::now() + self.timeout).min(by);
                        if let Some(picture) = self.fetch(&agent, url, one_by)
                            && !take(at, picture)
                        {
                            stop.store(true, Ordering::Relaxed);
                        }
                    }
                });
            }
        });
    }
}

/// The web pictures the messages `only` lets through ask for, each URL once
/// with every message that shows it: http and https only, no more than
/// [`PER_MAIL`] of a message's and [`PER_THREAD`] in all.
fn on_the_web(uses: &[Vec<String>], only: impl Fn(usize) -> bool) -> Vec<(String, Vec<usize>)> {
    let mut out: Vec<(String, Vec<usize>)> = Vec::new();
    let mut at: HashMap<String, usize> = HashMap::new();
    for (n, urls) in uses.iter().enumerate() {
        if !only(n) {
            continue;
        }
        let mut mine = HashSet::new();
        for url in urls {
            let low = url.to_ascii_lowercase();
            if !(low.starts_with("https:") || low.starts_with("http:")) || mine.contains(url) {
                continue;
            }
            if mine.len() == PER_MAIL {
                break;
            }
            match at.get(url) {
                Some(&i) => out[i].1.push(n),
                None if out.len() < PER_THREAD => {
                    at.insert(url.clone(), out.len());
                    out.push((url.clone(), vec![n]));
                }
                None => continue,
            }
            mine.insert(url.clone());
        }
    }
    out
}

/// `gws_mail_pictures {thread, trusted?, from?}`: the thread view with its web
/// pictures in it. Only the screen calls this: at the person's press of Show
/// pictures (every message), of Always from a sender (`from`: that sender's
/// messages), or as they open mail from a sender they trust (`trusted`: the
/// messages from a trusted sender Gmail vouches for). What's fetched isn't
/// stored. The whole of it, Gmail's calls too, keeps to [`ALL_WITHIN`].
fn show(core: &Core, args: &Value) -> Result<Value, Failure> {
    let by = Instant::now() + ALL_WITHIN;
    let (auth, account) = signed_in(core)?;
    let id = js::trim(&js::string_or_empty(args.get("thread"))).to_owned();
    if !gmail::id_ok(&id) {
        return Ok(fail("invalid_args", "That isn't a thread."));
    }
    let Some(mut thread) = core.gmail.thread_full_within(&auth, &id, ALL_WITHIN)? else {
        return Ok(fail("not_found", "That conversation is gone."));
    };
    unplace(&mut thread);
    let only_trusted = args.get("trusted") == Some(&Value::Bool(true));
    let only_from = args
        .get("from")
        .and_then(Value::as_str)
        .map(|a| js::trim(a).to_lowercase());
    let list = messages(&thread);
    let lets = |n: usize| {
        let m = &list[n];
        let from = from_of(m);
        if let Some(only) = &only_from {
            return from.as_ref() == Some(only);
        }
        !only_trusted
            || (vouched(m)
                && from.is_some_and(|a| {
                    with_store(core, |s| s.sender_trusted(&account, &a)).unwrap_or(false)
                }))
    };
    let mut placer = Placer::new(&thread);
    let asks = on_the_web(&placer.uses, lets);
    inside(core, &auth, &thread, &mut placer, by, &mut |_| true);
    let placer = Mutex::new(placer);
    let urls: Vec<&str> = asks.iter().map(|(u, _)| u.as_str()).collect();
    if !placer.lock().unwrap_or_else(|e| e.into_inner()).full() {
        core.pictures.fetch_all(&urls, by, |i, data| {
            let mut placer = placer.lock().unwrap_or_else(|e| e.into_inner());
            placer.offer(&asks[i].1, &asks[i].0, data);
            !placer.full()
        });
    }
    let placer = placer.into_inner().unwrap_or_else(|e| e.into_inner());
    placer.finish(&mut thread);
    let mut view = mail::view_of(&thread);
    view["pictures"] = named(&thread, &account);
    view["pictures"]["asked"] = Value::Bool(true);
    Ok(answer(core, &account, view))
}

/// Whether `tool` is one of these, which only a person's screen may call.
pub fn is_tool(tool: &str) -> bool {
    [
        MAIL_PICTURES,
        MAIL_PICTURE_SENDER_ADD,
        MAIL_PICTURE_SENDER_REMOVE,
    ]
    .contains(&tool)
}

/// The address a sender tool names, made ready to store.
fn address_arg(args: &Value) -> Option<String> {
    let address = js::trim(&js::string_or_empty(args.get("address"))).to_lowercase();
    address_ok(&address).then_some(address)
}

fn add(core: &Core, args: &Value) -> Result<Value, Failure> {
    let (_, account) = signed_in(core)?;
    let Some(address) = address_arg(args) else {
        return Ok(fail("invalid_args", "That isn't an address."));
    };
    let now = core.clock.now_ms();
    with_own_store(core, &account, |s| s.trust_sender(&account, &address, now))?;
    Ok(ok(json!({"trusted": true})))
}

fn remove(core: &Core, args: &Value) -> Result<Value, Failure> {
    let (_, account) = signed_in(core)?;
    let Some(address) = address_arg(args) else {
        return Ok(fail("invalid_args", "That isn't an address."));
    };
    with_store(core, |s| s.distrust_sender(&account, &address))?;
    Ok(ok(json!({"trusted": false})))
}

/// Runs a pictures tool; `None` for any other.
pub fn run(core: &Core, tool: &str, args: &Value) -> Option<Value> {
    let result = match tool {
        MAIL_PICTURES => show(core, args),
        MAIL_PICTURE_SENDER_ADD => add(core, args),
        MAIL_PICTURE_SENDER_REMOVE => remove(core, args),
        _ => return None,
    };
    Some(result.unwrap_or_else(failed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead as _, BufReader, Read as _, Write as _};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";

    #[test]
    fn a_picture_out_of_time_is_neither_asked_for_nor_charged() {
        let dir = std::env::temp_dir().join(format!("gws-late-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let core = Core::new(
            &Apis::at("http://127.0.0.1:9"),
            Ok(gws_store::Paths::in_dir(&dir)),
        );
        core.unlock("seed");
        let set = core.account("buwiz.account.set", &json!({"account": "google", "label": "default", "provider": "google", "subject": "owner@example.com", "scopes": [], "access_token": "fake-access-1", "token_type": "Bearer", "expires_at": 4_000_000_000u64}));
        assert_ne!(set["isError"], true, "{set}");
        let (auth, _) = core.accounts.session().expect("signed in");
        let part = json!({"partId": "1", "mimeType": "image/png", "body": {"attachmentId": "att-1", "size": 1024}});
        let (mut calls, mut charged, mut short) = (0, 0u64, false);
        let picture = part_picture(
            &core,
            &auth,
            "1a0000000000001",
            &part,
            Instant::now(),
            &mut calls,
            &|_| true,
            &mut |size| {
                charged += size;
                true
            },
            &mut short,
        );
        assert_eq!(picture, None);
        assert_eq!(charged, 0, "nothing asked for is nothing spent");
        assert_eq!(calls, 0);
        assert!(!short);
    }

    #[test]
    fn a_picture_is_judged_by_its_bytes() {
        assert_eq!(kind_of(PNG), Some("image/png"));
        assert_eq!(kind_of(&[0xff, 0xd8, 0xff, 0xe0]), Some("image/jpeg"));
        assert_eq!(kind_of(b"GIF89a.."), Some("image/gif"));
        assert_eq!(kind_of(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        for not in [
            &b"<svg xmlns='http://www.w3.org/2000/svg'/>"[..],
            b"<html><script>alert(1)</script>",
            b"RIFF\0\0\0\0WAVE",
            b"",
        ] {
            assert_eq!(kind_of(not), None);
        }
        let mut big = PNG.to_vec();
        big.resize(PICTURE_MAX, 0);
        assert!(data_url(&big).is_some(), "2 MB is still in");
        big.push(0);
        assert_eq!(data_url(&big), None, "a byte over is out");
    }

    #[test]
    fn only_a_public_address_is_fetched_from() {
        for private in [
            "127.0.0.1",
            "127.9.9.9",
            "0.0.0.0",
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "100.127.255.255",
            "224.0.0.1",
            "239.255.255.250",
            "255.255.255.255",
            "::",
            "::1",
            "fe80::1",
            "fc00::1",
            "fd12:3456::1",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "::ffff:169.254.169.254",
            "::127.0.0.1",
            "64:ff9b::a00:1",
            "2002:c0a8:101::1",
            "2001:db8::1",
            // RFC 8215's local NAT64 prefix, and SIIT's IPv4-translated form,
            // whatever IPv4 address they carry
            "64:ff9b:1::808:808",
            "64:ff9b:1:ffff::1",
            "::ffff:0:808:808",
            "::ffff:0:7f00:1",
        ] {
            assert!(!public(private.parse().unwrap()), "{private}");
        }
        for open in [
            "8.8.8.8",
            "1.1.1.1",
            "172.32.0.1",
            "100.128.0.1",
            "93.184.216.34",
            "2606:4700:4700::1111",
            "::ffff:8.8.8.8",
            "2002:808:808::1",
        ] {
            assert!(public(open.parse().unwrap()), "{open}");
        }
    }

    #[test]
    fn a_cid_url_names_its_part() {
        assert_eq!(cid_of("cid:logo@digest").as_deref(), Some("logo@digest"));
        assert_eq!(
            cid_of("CID:%3Clogo%40digest%3E").as_deref(),
            Some("logo@digest")
        );
        assert_eq!(cid_of("cid:"), None);
        assert_eq!(cid_of("https://a.example/cid:x"), None);
    }

    #[test]
    fn an_address_to_trust_is_one_address() {
        assert!(address_ok("news@digest.example"));
        for bad in ["", "news", "@x", "a@", "a b@x", "a@x\n", "<a@x>", "a@x,b@y"] {
            assert!(!address_ok(bad), "{bad:?}");
        }
        assert!(!address_ok(&format!("{}@x.example", "a".repeat(260))));
    }

    #[cfg(not(feature = "stub"))]
    #[test]
    fn a_release_refuses_http_and_this_mac_whatever_the_apis_say() {
        let server = Server::start(|_, _| Answer::png());
        let fetcher = Fetcher::new(&Apis::at(&server.base()));
        assert!(fetcher.stub.is_none());
        let agent = fetcher.agent();
        let by = Instant::now() + TIMEOUT;
        assert_eq!(fetcher.fetch(&agent, &server.url("/a.png"), by), None);
        let https = server.url("/a.png").replace("http:", "https:");
        assert_eq!(fetcher.fetch(&agent, &https, by), None);
        let named = https.replace("127.0.0.1", "localhost");
        assert_eq!(fetcher.fetch(&agent, &named, by), None);
        assert_eq!(server.hits(), 0, "nothing reached this Mac");
    }

    #[cfg(feature = "stub")]
    #[test]
    fn a_stub_build_reaches_the_stub_and_nothing_else_here() {
        let server = Server::start(|_, _| Answer::png());
        let other = Server::start(|_, _| Answer::png());
        let fetcher = Fetcher::new(&Apis::at(&server.base()));
        let (agent, by) = (fetcher.agent(), Instant::now() + TIMEOUT);
        assert!(fetcher.fetch(&agent, &server.url("/a.png"), by).is_some());
        assert_eq!(fetcher.fetch(&agent, &other.url("/a.png"), by), None);
        assert_eq!(other.hits(), 0);
    }

    // --- A web server of the test's own, on this Mac ----------------------------

    struct Answer {
        head: String,
        body: Vec<u8>,
        /// Holds the answer back this long, after the head.
        stall: Duration,
    }

    impl Answer {
        fn new(status: &str, headers: &[(&str, &str)], body: Vec<u8>) -> Self {
            let mut head = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\n", body.len());
            for (k, v) in headers {
                head.push_str(&format!("{k}: {v}\r\n"));
            }
            head.push_str("Connection: close\r\n\r\n");
            Self {
                head,
                body,
                stall: Duration::ZERO,
            }
        }

        fn png() -> Self {
            Self::new("200 OK", &[("Content-Type", "text/html")], PNG.to_vec())
        }

        fn to(location: &str) -> Self {
            Self::new("302 Found", &[("Location", location)], Vec::new())
        }
    }

    type Handler = dyn Fn(&str, &str) -> Answer + Send + Sync;

    /// Answers each request with `handler(path, head)`, and keeps every head.
    struct Server {
        addr: SocketAddr,
        heads: Arc<Mutex<Vec<String>>>,
    }

    impl Server {
        fn start(handler: impl Fn(&str, &str) -> Answer + Send + Sync + 'static) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let heads: Arc<Mutex<Vec<String>>> = Arc::default();
            let handler: Arc<Handler> = Arc::new(handler);
            let kept = heads.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    let (handler, kept) = (handler.clone(), kept.clone());
                    std::thread::spawn(move || {
                        let mut reader = BufReader::new(stream.try_clone().unwrap());
                        let mut head = String::new();
                        loop {
                            let mut line = String::new();
                            if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                                break;
                            }
                            head.push_str(&line);
                        }
                        let path = head.split(' ').nth(1).unwrap_or("/").to_owned();
                        kept.lock().unwrap().push(head.clone());
                        let answer = handler(&path, &head);
                        let _ = stream.write_all(answer.head.as_bytes());
                        std::thread::sleep(answer.stall);
                        let _ = stream.write_all(&answer.body);
                        let _ = stream.flush();
                        let _ = reader.read(&mut [0; 1]);
                    });
                }
            });
            Self { addr, heads }
        }

        fn base(&self) -> String {
            format!("http://{}", self.addr)
        }

        fn url(&self, path: &str) -> String {
            format!("{}{path}", self.base())
        }

        fn hits(&self) -> usize {
            self.heads.lock().unwrap().len()
        }

        fn fetcher(&self) -> Fetcher {
            Fetcher::allowing(self.addr, TIMEOUT)
        }

        fn get(&self, path: &str) -> Option<String> {
            let fetcher = self.fetcher();
            fetcher.fetch(&fetcher.agent(), &self.url(path), Instant::now() + TIMEOUT)
        }
    }

    #[test]
    fn a_picture_comes_by_its_bytes_whatever_its_header_says() {
        let server = Server::start(|path, _| match path {
            "/png" => Answer::png(),
            "/liar" => Answer::new(
                "200 OK",
                &[("Content-Type", "image/png")],
                b"<html><script>alert(1)</script></html>".to_vec(),
            ),
            "/svg" => Answer::new(
                "200 OK",
                &[("Content-Type", "image/svg+xml")],
                b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>".to_vec(),
            ),
            _ => Answer::new("404 Not Found", &[], Vec::new()),
        });
        let png = server.get("/png").expect("a png named text/html");
        assert!(png.starts_with("data:image/png;base64,"), "{png}");
        assert_eq!(server.get("/liar"), None);
        assert_eq!(server.get("/svg"), None);
        assert_eq!(server.get("/missing"), None);
    }

    #[test]
    fn a_picture_over_2_mb_is_left_out() {
        let sized = |n: usize| {
            let mut bytes = PNG.to_vec();
            bytes.resize(n, 0);
            bytes
        };
        let server = Server::start(move |path, _| {
            let n = if path == "/edge" {
                PICTURE_MAX
            } else {
                PICTURE_MAX + 1
            };
            Answer::new("200 OK", &[], sized(n))
        });
        assert!(server.get("/edge").is_some());
        assert_eq!(server.get("/over"), None);
    }

    /// A length that says less doesn't let more through: what's read stops.
    #[test]
    fn a_picture_that_says_it_is_small_is_still_stopped_at_2_mb() {
        let server = Server::start(|_, _| {
            let mut body = PNG.to_vec();
            body.resize(PICTURE_MAX + 4096, 0);
            Answer {
                head: "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_owned(),
                body,
                stall: Duration::ZERO,
            }
        });
        assert_eq!(server.get("/unsized"), None);
    }

    #[test]
    fn a_slow_picture_gives_up_on_time() {
        assert_eq!(TIMEOUT, Duration::from_secs(10));
        let server = Server::start(|_, _| {
            let mut slow = Answer::png();
            slow.stall = Duration::from_secs(5);
            slow
        });
        let fetcher = Fetcher::allowing(server.addr, Duration::from_millis(300));
        let at = Instant::now();
        let got = AtomicUsize::new(0);
        let url = server.url("/slow");
        fetcher.fetch_all(&[&url], at + ALL_WITHIN, |_, _| {
            got.fetch_add(1, Ordering::Relaxed);
            true
        });
        assert_eq!(got.into_inner(), 0);
        assert!(at.elapsed() < Duration::from_secs(3), "{:?}", at.elapsed());
    }

    #[test]
    fn three_redirects_are_followed_and_a_fourth_is_not() {
        let server = Server::start(|path, _| {
            let n: usize = path.trim_start_matches("/r").parse().unwrap_or(0);
            if n == 0 {
                Answer::png()
            } else {
                Answer::to(&format!("/r{}", n - 1))
            }
        });
        assert!(server.get("/r3").is_some());
        assert_eq!(server.get("/r4"), None);
    }

    #[test]
    fn a_redirect_is_checked_again() {
        let elsewhere = Server::start(|_, _| Answer::png());
        let to = elsewhere.url("/a.png");
        let https = to.replace("http:", "https:");
        let server = Server::start(move |path, _| match path {
            "/to-http" => Answer::to(&to),
            "/to-https-here" => Answer::to(&https),
            "/to-metadata" => Answer::to("http://169.254.169.254/latest/meta-data"),
            "/to-file" => Answer::to("file:///etc/passwd"),
            _ => Answer::png(),
        });
        for path in ["/to-http", "/to-https-here", "/to-metadata", "/to-file"] {
            assert_eq!(server.get(path), None, "{path}");
        }
        assert_eq!(elsewhere.hits(), 0, "no redirect reached another address");
    }

    #[test]
    fn a_fetch_carries_no_cookie_referer_or_account() {
        let server = Server::start(|path, _| match path {
            "/first" => Answer::new(
                "302 Found",
                &[
                    ("Location", "/second"),
                    ("Set-Cookie", "seen=owner@example.com"),
                ],
                Vec::new(),
            ),
            _ => Answer::png(),
        });
        assert!(server.get("/first").is_some());
        let fetcher = server.fetcher();
        let agent = fetcher.agent();
        let by = Instant::now() + TIMEOUT;
        assert!(fetcher.fetch(&agent, &server.url("/first"), by).is_some());
        let heads = server.heads.lock().unwrap().clone();
        assert_eq!(heads.len(), 4, "{heads:?}");
        for head in heads {
            let low = head.to_ascii_lowercase();
            assert!(!low.contains("\ncookie:"), "{head}");
            assert!(!low.contains("\nreferer:"), "{head}");
            assert!(!low.contains("authorization:"), "{head}");
            assert!(!low.contains('@'), "{head}");
            assert!(
                low.contains(&format!("user-agent: {}", USER_AGENT.to_ascii_lowercase())),
                "{head}"
            );
        }
    }

    /// The press's own clock stops what's left, however long each may take.
    #[test]
    fn a_press_stops_on_its_own_clock() {
        let server = Server::start(|_, _| {
            let mut slow = Answer::png();
            slow.stall = Duration::from_secs(5);
            slow
        });
        let fetcher = server.fetcher();
        let urls: Vec<String> = (0..20).map(|n| server.url(&format!("/{n}"))).collect();
        let urls: Vec<&str> = urls.iter().map(String::as_str).collect();
        let at = Instant::now();
        fetcher.fetch_all(&urls, at + Duration::from_millis(300), |_, _| true);
        assert!(at.elapsed() < Duration::from_secs(3), "{:?}", at.elapsed());
    }

    fn html_mail(page: &str) -> Value {
        json!({"payload": {"mimeType": "text/html", "body": {"data": base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(page)}}})
    }

    #[test]
    fn no_more_than_40_pictures_of_a_mail_or_80_of_a_press_are_asked_for() {
        let page = |from: usize| -> String {
            (from..from + 45)
                .map(|n| format!(r#"<img src="https://a.example/{n}.png">"#))
                .collect()
        };
        let thread = json!({"messages": [html_mail(&page(0)), html_mail(&page(100)), html_mail(&page(200))]});
        let uses: Vec<Vec<String>> = messages(&thread).iter().map(wanted).collect();
        let asks = on_the_web(&uses, |_| true);
        assert_eq!((PER_MAIL, PER_THREAD), (40, 80));
        assert_eq!(asks.len(), PER_THREAD);
        assert_eq!(asks.iter().filter(|(_, m)| m == &[0]).count(), PER_MAIL);
        assert!(
            asks.iter().all(|(_, m)| m != &[2]),
            "the third mail is past the cap"
        );
        assert_eq!(on_the_web(&uses, |n| n == 2).len(), PER_MAIL);
    }

    #[test]
    fn a_picture_in_two_mails_is_fetched_once_and_shows_in_both() {
        let page = r#"<img src="https://a.example/logo.png"><img src="https://a.example/x.png">"#;
        let thread = json!({"messages": [html_mail(page), html_mail(page)]});
        let uses: Vec<Vec<String>> = messages(&thread).iter().map(wanted).collect();
        let asks = on_the_web(&uses, |_| true);
        assert_eq!(
            asks,
            [
                ("https://a.example/logo.png".to_owned(), vec![0, 1]),
                ("https://a.example/x.png".to_owned(), vec![0, 1])
            ]
        );
    }

    /// Once nothing more fits, nothing more is fetched, and what didn't fit
    /// isn't kept.
    #[test]
    fn fetching_stops_once_the_answer_is_full() {
        let page = |base: &str| -> String {
            (0..30)
                .map(|n| format!(r#"<img src="{base}/{n}.png">"#))
                .collect()
        };
        let size = Arc::new(AtomicUsize::new(0));
        let sized = size.clone();
        let server = Server::start(move |_, _| {
            let mut bytes = PNG.to_vec();
            bytes.resize(sized.load(Ordering::Relaxed), 0);
            Answer::new("200 OK", &[], bytes)
        });
        let thread = json!({"id": "t", "messages": [html_mail(&page(&server.base()))]});
        // one picture that leaves less room than any picture takes
        let room = Placer::new(&thread).room;
        size.store((room - 54) / 4 * 3, Ordering::Relaxed);
        let placer = Mutex::new(Placer::new(&thread));
        let asks = on_the_web(&placer.lock().unwrap().uses, |_| true);
        let urls: Vec<&str> = asks.iter().map(|(u, _)| u.as_str()).collect();
        server
            .fetcher()
            .fetch_all(&urls, Instant::now() + ALL_WITHIN, |i, data| {
                let mut p = placer.lock().unwrap();
                p.offer(&asks[i].1, &asks[i].0, data);
                !p.full()
            });
        let placer = placer.into_inner().unwrap();
        assert_eq!(placer.picks.len(), 1, "one fit, and the rest weren't kept");
        assert!(placer.full());
        assert!(
            server.hits() <= AT_ONCE * 2,
            "{} of 30 fetched for nothing",
            server.hits()
        );
    }

    #[test]
    fn a_picture_that_doesnt_fit_the_answer_stays_out() {
        let mut thread =
            json!({"id": "t", "messages": [html_mail(r#"<img src="cid:a"><img src="cid:b">"#)]});
        let big = format!("data:image/png;base64,{}", "A".repeat(ROOM / 2 + 10));
        let mut placer = Placer::new(&thread);
        assert!(placer.offer(&[0], "cid:a", big.clone()));
        assert!(!placer.offer(&[0], "cid:b", big));
        assert_eq!(placer.picks.len(), 1, "what didn't fit wasn't kept");
        placer.finish(&mut thread);
        let view = mail::view_of(&thread);
        assert_eq!(view["images"], 1, "the second didn't fit");
        assert!(view.to_string().len() <= ROOM);
    }

    fn signed(from: &str, results: &[&str]) -> Value {
        let mut headers = vec![json!({"name": "From", "value": from})];
        for r in results {
            headers.push(json!({"name": "Authentication-Results", "value": r}));
        }
        json!({"payload": {"headers": headers}})
    }

    #[test]
    fn only_gmails_word_that_the_sender_is_real_counts() {
        let from = "Digest <news@digest.example>";
        for good in [
            "mx.google.com; dkim=pass header.i=@digest.example header.s=s1; spf=pass smtp.mailfrom=news@digest.example; dmarc=pass (p=NONE) header.from=digest.example",
            "mx.google.com; dmarc=pass (p=REJECT sp=REJECT dis=NONE) header.from=digest.example",
            "mx.google.com; dkim=pass header.d=digest.example",
            "mx.google.com; dkim=pass header.i=@mail.example header.d=example; dmarc=fail header.from=x",
        ] {
            let sender = if good.contains("header.d=example") {
                "news@news.example"
            } else {
                from
            };
            assert!(vouched(&signed(sender, &[good])), "{good}");
        }
        for bad in [
            // the forged mail's own line, under nobody's
            "mx.google.com; spf=softfail smtp.mailfrom=news@digest.example; dmarc=fail (p=NONE) header.from=digest.example",
            "mx.google.com; dkim=pass header.i=@evil.example; dmarc=pass header.from=evil.example",
            "mx.google.com; dkim=pass header.d=notdigest.example",
            "mx.google.com; dkim=fail header.d=digest.example",
            "mx.google.com; spf=pass smtp.mailfrom=news@digest.example",
            "mx.google.com; dkim=neutral (dkim=pass header.d=digest.example)",
            "evil.example; dmarc=pass header.from=digest.example",
            "",
        ] {
            assert!(!vouched(&signed(from, &[bad])), "{bad}");
        }
        assert!(!vouched(&signed(from, &[])), "no word is no pass");
        // a line the sender wrote sits below Gmail's, and isn't read
        let under = signed(
            from,
            &[
                "mx.google.com; dmarc=fail header.from=digest.example",
                "mx.google.com; dmarc=pass header.from=digest.example",
            ],
        );
        assert!(!vouched(&under));
    }

    /// Gmail adds its own verdict last, so it's on top: a sender's copy of one
    /// that says pass, however much it looks like Gmail's, sits below and
    /// counts for nothing.
    #[test]
    fn a_forged_pass_below_gmails_own_fail_is_ignored() {
        let message = json!({"labelIds": ["INBOX"], "payload": {"headers": [
            {"name": "Authentication-Results", "value": "mx.google.com; spf=softfail (google.com: domain of transitioning news@digest.example does not designate 192.0.2.9 as permitted sender) smtp.mailfrom=news@digest.example; dmarc=fail (p=NONE sp=NONE dis=NONE) header.from=digest.example"},
            {"name": "Received", "value": "from evil.example by mx.google.com"},
            {"name": "Authentication-Results", "value": "mx.google.com; dkim=pass header.i=@digest.example header.d=digest.example; dmarc=pass (p=REJECT) header.from=digest.example"},
            {"name": "ARC-Authentication-Results", "value": "i=1; mx.google.com; dkim=pass header.d=digest.example; dmarc=pass header.from=digest.example"},
            {"name": "From", "value": "Weekly Digest <news@digest.example>"},
        ]}});
        assert!(!vouched(&message));
        assert_eq!(
            sender(&json!({"messages": [message]}), "owner@example.com"),
            Some(("news@digest.example".to_owned(), false)),
            "offered, as a stranger"
        );
    }

    #[test]
    fn the_persons_own_mail_offers_no_always() {
        let me = "owner@example.com";
        let mine =
            json!({"labelIds": ["SENT"], "payload": {"headers": [{"name": "From", "value": me}]}});
        let also_mine = json!({"labelIds": ["INBOX"], "payload": {"headers": [{"name": "From", "value": "Owner <OWNER@example.com>"}]}});
        assert_eq!(
            sender(&json!({"messages": [mine.clone(), also_mine.clone()]}), me),
            None
        );
        let theirs = signed("Ana <ana@example.com>", &[]);
        assert_eq!(
            sender(&json!({"messages": [theirs, mine, also_mine]}), me),
            Some(("ana@example.com".to_owned(), false))
        );
    }

    /// Hosts written in other forms still name this Mac, and are refused.
    #[test]
    fn this_mac_in_any_spelling_is_refused() {
        let fetcher = Fetcher {
            stub: None,
            timeout: TIMEOUT,
        };
        let agent = fetcher.agent();
        let by = Instant::now() + TIMEOUT;
        let config = ureq::config::Config::default();
        for host in [
            "https://2130706433/a.png",
            "https://0x7f000001/a.png",
            "https://0177.0.0.1/a.png",
            "https://0x7f.1/a.png",
            "https://[::1]/a.png",
            "https://[::ffff:127.0.0.1]/a.png",
            "https://[0:0:0:0:0:0:0:1]/a.png",
            "https://localhost/a.png",
        ] {
            // what's asked for is always the parsed URL, whose host is the
            // plain address: the system's resolver reads 0177 as 177, not 127
            let parsed = Url::parse(host).unwrap();
            let uri: Uri = parsed.as_str().parse().unwrap();
            let resolved = Checked { stub: None }.resolve(&uri, &config, NextTimeout::default());
            assert!(resolved.is_err(), "{host} as {parsed}: {resolved:?}");
            assert_eq!(fetcher.fetch(&agent, host, by), None, "{host}");
        }
    }
}
