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

/// A part's picture: its bytes inline, else from the attachment API, and
/// nothing past [`PICTURE_MAX`] fetched at all.
fn part_picture(core: &Core, auth: &Bearer, message: &str, part: &Value) -> Option<String> {
    if let Some(data) = part.pointer("/body/data").and_then(Value::as_str) {
        // base64 of more than the cap can't decode to within it
        if data.len() > PICTURE_MAX / 3 * 4 + 8 {
            return None;
        }
        return data_url(&unbase64(data)?);
    }
    let size = part.pointer("/body/size").and_then(Value::as_u64)?;
    let id = part.pointer("/body/attachmentId").and_then(Value::as_str)?;
    if size > PICTURE_MAX as u64 || !attachment_ok(id) || !gmail::id_ok(message) {
        return None;
    }
    let bytes = core.gmail.attachment(auth, message, id).ok()??;
    data_url(&bytes)
}

/// One picture found for a message: its URL as the page names it, and what
/// takes its place.
struct Found {
    message: usize,
    url: String,
    data: String,
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

/// The pictures inside `thread`'s mail, by Content-ID.
fn inside(core: &Core, auth: &Bearer, thread: &Value) -> Vec<Found> {
    let mut found = Vec::new();
    for (n, m) in messages(thread).iter().enumerate() {
        let parts = by_content_id(m);
        if parts.is_empty() {
            continue;
        }
        let id = m.get("id").and_then(Value::as_str).unwrap_or_default();
        let mut seen = HashSet::new();
        for url in wanted(m) {
            let Some(part) = cid_of(&url).and_then(|cid| parts.get(&cid).copied()) else {
                continue;
            };
            if !seen.insert(url.clone()) {
                continue;
            }
            if let Some(data) = part_picture(core, auth, id, part) {
                found.push(Found {
                    message: n,
                    url,
                    data,
                });
            }
        }
    }
    found
}

/// Puts `found` in `thread`'s messages, in order, while the view stays within
/// [`ROOM`]. A picture shown twice costs twice.
fn place(thread: &mut Value, found: Vec<Found>) {
    let base = mail::view_of(thread).to_string().len();
    let mut room = ROOM.saturating_sub(base);
    let uses: Vec<Vec<String>> = messages(thread).iter().map(wanted).collect();
    let list = messages_mut(thread);
    for f in found {
        let times = uses[f.message].iter().filter(|u| **u == f.url).count();
        let cost = (f.data.len() + 2) * times.max(1);
        if cost > room {
            continue;
        }
        room -= cost;
        let message = &mut list[f.message];
        if !message.get(KEY).is_some_and(Value::is_object) {
            message[KEY] = json!({});
        }
        message[KEY][f.url] = Value::String(f.data);
    }
}

/// Takes out anything that looks like what [`place`] puts in.
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

/// Who the pictures are from: the newest message the person didn't send.
fn sender(thread: &Value) -> Option<String> {
    let list = messages(thread);
    let theirs = |m: &&Value| {
        !m.get("labelIds")
            .and_then(Value::as_array)
            .is_some_and(|l| l.iter().any(|l| l == "SENT"))
    };
    let m = list.iter().rev().find(theirs).or(list.last())?;
    let (_, address) = mail::sender_of(&gmail::short_header(m, "From"));
    let address = address.to_lowercase();
    address_ok(&address).then_some(address)
}

/// The thread's view, with the pictures inside its mail shown, and who its
/// pictures are from. For the thread view, which the store keeps.
pub fn view(core: &Core, auth: &Bearer, thread: &mut Value) -> Value {
    unplace(thread);
    let found = inside(core, auth, thread);
    place(thread, found);
    let mut view = mail::view_of(thread);
    view["pictures"] = json!({"from": sender(thread)});
    view
}

/// `view` with whether its sender is trusted now, as a tool's answer. Not
/// `ok`: that sends the view twice, and pictures make it big.
pub fn answer(core: &Core, account: &str, mut view: Value) -> Value {
    let from = view
        .pointer("/pictures/from")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let trusted = from.is_some_and(|from| {
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

    /// Each of `urls` fetched, a few at once, all of them within
    /// [`ALL_WITHIN`]: what didn't come is `None`.
    fn fetch_all(&self, urls: &[String]) -> Vec<Option<String>> {
        let agent = self.agent();
        let all_by = Instant::now() + ALL_WITHIN;
        let next = Mutex::new(0usize);
        let got: Mutex<Vec<Option<String>>> = Mutex::new(vec![None; urls.len()]);
        std::thread::scope(|s| {
            for _ in 0..AT_ONCE.min(urls.len()) {
                s.spawn(|| {
                    loop {
                        let at = {
                            let mut next = next.lock().unwrap_or_else(|e| e.into_inner());
                            *next += 1;
                            *next - 1
                        };
                        let Some(url) = urls.get(at) else { break };
                        let by = (Instant::now() + self.timeout).min(all_by);
                        let picture = self.fetch(&agent, url, by);
                        got.lock().unwrap_or_else(|e| e.into_inner())[at] = picture;
                    }
                });
            }
        });
        got.into_inner().unwrap_or_else(|e| e.into_inner())
    }
}

/// The web pictures each message asks for: http and https only, each once,
/// and no more than [`PER_MAIL`] of a message's.
fn on_the_web(thread: &Value, only: impl Fn(&Value) -> bool) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for (n, m) in messages(thread).iter().enumerate() {
        if !only(m) {
            continue;
        }
        let mut seen = HashSet::new();
        let urls = wanted(m).into_iter().filter(|u| {
            let low = u.to_ascii_lowercase();
            (low.starts_with("https:") || low.starts_with("http:")) && seen.insert(u.clone())
        });
        out.extend(urls.take(PER_MAIL).map(|u| (n, u)));
    }
    out
}

/// `gws_mail_pictures {thread, trusted?}`: the thread view with its web
/// pictures in it. Only the screen calls this, at the person's press or as
/// they open mail from a sender they trust; `trusted` keeps to that sender's
/// own messages. What's fetched isn't stored.
fn show(core: &Core, args: &Value) -> Result<Value, Failure> {
    let (auth, account) = signed_in(core)?;
    let id = js::trim(&js::string_or_empty(args.get("thread"))).to_owned();
    if !gmail::id_ok(&id) {
        return Ok(fail("invalid_args", "That isn't a thread."));
    }
    let Some(mut thread) = core.gmail.thread_full(&auth, &id)? else {
        return Ok(fail("not_found", "That conversation is gone."));
    };
    unplace(&mut thread);
    let only_trusted = args.get("trusted") == Some(&Value::Bool(true));
    let trusted = |m: &Value| {
        let (_, address) = mail::sender_of(&gmail::short_header(m, "From"));
        let address = address.to_lowercase();
        address_ok(&address)
            && with_store(core, |s| s.sender_trusted(&account, &address)).unwrap_or(false)
    };
    let asks = on_the_web(&thread, |m| !only_trusted || trusted(m));
    let urls: Vec<String> = asks.iter().map(|(_, u)| u.clone()).collect();
    let mut found = inside(core, &auth, &thread);
    let fetched = core.pictures.fetch_all(&urls);
    found.extend(
        asks.into_iter()
            .zip(fetched)
            .filter_map(|((message, url), data)| {
                Some(Found {
                    message,
                    url,
                    data: data?,
                })
            }),
    );
    place(&mut thread, found);
    let mut view = mail::view_of(&thread);
    view["pictures"] = json!({"from": sender(&thread), "asked": true});
    Ok(answer(core, &account, view))
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

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";

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
        assert_eq!(fetcher.fetch_all(&[server.url("/slow")]), [None]);
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

    #[test]
    fn no_more_than_40_pictures_of_a_mail_are_fetched() {
        let server = Server::start(|_, _| Answer::png());
        let page: String = (0..45)
            .map(|n| format!(r#"<img src="{}">"#, server.url(&format!("/{n}.png"))))
            .collect();
        let mail = json!({"payload": {"mimeType": "text/html", "body": {"data": base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&page)}}});
        let thread = json!({"messages": [mail.clone(), mail]});
        let asks = on_the_web(&thread, |_| true);
        assert_eq!(asks.len(), 2 * PER_MAIL);
        let urls: Vec<String> = asks
            .into_iter()
            .take(PER_MAIL + 5)
            .map(|(_, u)| u)
            .collect();
        let got = server.fetcher().fetch_all(&urls);
        assert!(got.iter().all(Option::is_some));
        assert_eq!(PER_MAIL, 40);
    }

    #[test]
    fn a_picture_that_doesnt_fit_the_answer_stays_out() {
        let page = r#"<img src="cid:a"><img src="cid:b">"#;
        let mail = json!({"payload": {"mimeType": "text/html", "body": {"data": base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(page)}}});
        let mut thread = json!({"id": "t", "messages": [mail]});
        let big = format!("data:image/png;base64,{}", "A".repeat(ROOM / 2 + 10));
        let found = |url: &str| Found {
            message: 0,
            url: url.to_owned(),
            data: big.clone(),
        };
        place(&mut thread, vec![found("cid:a"), found("cid:b")]);
        let view = mail::view_of(&thread);
        assert_eq!(view["images"], 1, "the second didn't fit");
        assert!(view.to_string().len() <= ROOM);
    }
}
