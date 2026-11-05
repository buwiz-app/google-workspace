//! Gmail's REST API (`gmail/v1/users/me`), read-only, with the token Buwiz
//! Bot pushed (`crate::accounts`), and what the tools make of its answers:
//! headers, a message's plain-text body (text/plain, else text/html with
//! the tags stripped), one-line snippets, the 64 KiB cap.
//!
//! The HTTP client is ureq (rustls with Mozilla's roots, so no OpenSSL in it),
//! blocking, on the tool's own thread. It follows no redirects, so the token
//! only ever goes to the API base.

use std::time::Duration;

use base64::Engine as _;
use serde_json::Value;

use crate::accounts::Bearer;
use crate::js;

/// The production API base.
pub const GMAIL_API: &str = "https://gmail.googleapis.com/gmail/v1/users/me";
pub const BODY_MAX: usize = 64 * 1024;
pub const SNIPPET_MAX: usize = 200;
const TIMEOUT: Duration = Duration::from_secs(30);
/// Requests in flight for one search.
const METADATA_AT_ONCE: usize = 8;
/// A list or metadata answer.
const SMALL_MAX: u64 = 1024 * 1024;
/// A `format=full` message (attachments are not in it, only their ids).
const FULL_MAX: u64 = 32 * 1024 * 1024;

/// Why a Gmail call gave nothing.
#[derive(Debug, PartialEq)]
pub enum Failure {
    /// No usable token: none pushed, expired, or Gmail answered 401.
    AccountRequired,
    /// Anything else, as a plain message.
    Plain(String),
}

/// The Gmail API at a base URL.
#[derive(Clone)]
pub struct Gmail {
    agent: ureq::Agent,
    base: String,
}

impl Default for Gmail {
    fn default() -> Self {
        Self::new(GMAIL_API)
    }
}

/// A Gmail message id: what the API hands out (hex), so it is safe in a
/// path as it is.
pub fn id_ok(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

impl Gmail {
    /// `base` is [`GMAIL_API`] in production; tests pass a local server.
    pub fn new(base: &str) -> Self {
        let config = ureq::Agent::config_builder().http_status_as_error(false).max_redirects(0).timeout_global(Some(TIMEOUT)).user_agent("buwiz-gws").build();
        Self { agent: config.new_agent(), base: base.trim_end_matches('/').to_owned() }
    }

    /// A GET of `path` under the base: its JSON, or `None` for a 404.
    fn get(&self, auth: &Bearer, path: String, query: Vec<(&'static str, String)>, cap: u64) -> Result<Option<Value>, Failure> {
        let url = format!("{}/{path}", self.base);
        let mut req = self.agent.get(&url).header("authorization", auth.header()).header("accept", "application/json");
        for (k, v) in &query {
            req = req.query(*k, v);
        }
        let mut res = req.call().map_err(|e| Failure::Plain(format!("could not reach Gmail: {}", js::slice_utf16(&e.to_string(), 200))))?;
        let status = res.status().as_u16();
        let body = res.body_mut().with_config().limit(cap).read_to_vec().map_err(|e| Failure::Plain(format!("Gmail's answer could not be read: {}", js::slice_utf16(&e.to_string(), 200))))?;
        match status {
            200 => serde_json::from_slice(&body).map(Some).map_err(|_| Failure::Plain("Gmail's answer is not JSON".to_owned())),
            401 => Err(Failure::AccountRequired),
            404 => Ok(None),
            _ => {
                let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let why = v.pointer("/error/message").and_then(Value::as_str).unwrap_or("no reason given");
                Err(Failure::Plain(format!("Gmail answered {status}: {}", js::slice_utf16(why, 300))))
            }
        }
    }

    /// The ids of the first `max` messages `q` finds.
    pub fn search(&self, auth: &Bearer, q: &str, max: usize) -> Result<Vec<String>, Failure> {
        let found = self.get(auth, "messages".to_owned(), vec![("q", q.to_owned()), ("maxResults", max.to_string())], SMALL_MAX)?;
        let list = found.as_ref().and_then(|v| v.get("messages")).and_then(Value::as_array);
        Ok(list.into_iter().flatten().filter_map(|m| m.get("id").and_then(Value::as_str)).map(str::to_owned).collect())
    }

    /// Each message's From, Subject, Date and snippet (`format=metadata`),
    /// fetched together; `None` for one Gmail no longer has.
    pub fn metadata(&self, auth: &Bearer, ids: &[String]) -> Result<Vec<Option<Value>>, Failure> {
        let headers = ["From", "Subject", "Date"].map(|h| ("metadataHeaders", h.to_owned()));
        let mut out = Vec::with_capacity(ids.len());
        for batch in ids.chunks(METADATA_AT_ONCE) {
            let done: Result<Vec<_>, Failure> = std::thread::scope(|scope| {
                let gets: Vec<_> = batch
                    .iter()
                    .map(|id| {
                        let mut query = vec![("format", "metadata".to_owned())];
                        query.extend(headers.iter().cloned());
                        id_ok(id).then(|| scope.spawn(move || self.get(auth, format!("messages/{id}"), query, SMALL_MAX)))
                    })
                    .collect();
                let mut got = Vec::with_capacity(gets.len());
                for g in gets {
                    got.push(match g {
                        Some(g) => g.join().unwrap_or_else(|_| Err(Failure::Plain("the Gmail request failed".to_owned())))?,
                        None => None,
                    });
                }
                Ok(got)
            });
            out.extend(done?);
        }
        Ok(out)
    }

    /// One message, `format=full`; `None` when Gmail has no such message.
    pub fn full(&self, auth: &Bearer, id: &str) -> Result<Option<Value>, Failure> {
        if !id_ok(id) {
            return Ok(None);
        }
        self.get(auth, format!("messages/{id}"), vec![("format", "full".to_owned())], FULL_MAX)
    }
}

/// A header of a message's `payload` (names are case-insensitive), trimmed;
/// empty when missing.
pub fn header(message: &Value, name: &str) -> String {
    let headers = message.pointer("/payload/headers").and_then(Value::as_array);
    let found = headers.into_iter().flatten().find(|h| h.get("name").and_then(Value::as_str).is_some_and(|n| n.eq_ignore_ascii_case(name)));
    js::trim(found.and_then(|h| h.get("value")).and_then(Value::as_str).unwrap_or_default()).to_owned()
}

/// Gmail's `snippet` (HTML-escaped) as plain text on one line.
pub fn snippet(message: &Value) -> String {
    snippet_of(&decode_entities(message.get("snippet").and_then(Value::as_str).unwrap_or_default()))
}

/// The message's body as plain text: its first text/plain part, else its
/// first text/html part with the tags stripped; attachments are skipped.
pub fn body(message: &Value) -> String {
    let mut leaves = Vec::new();
    if let Some(p) = message.get("payload") {
        collect_leaves(p, &mut leaves);
    }
    let mime = |p: &Value| p.get("mimeType").and_then(Value::as_str).unwrap_or_default().to_ascii_lowercase();
    let text = if let Some(p) = leaves.iter().find(|p| mime(p) == "text/plain") {
        decode_part(p)
    } else if let Some(p) = leaves.iter().find(|p| mime(p) == "text/html") {
        strip_html(&decode_part(p))
    } else {
        String::new()
    };
    text.replace("\r\n", "\n").trim_end_matches(['\n', '\r', ' ', '\t']).to_owned()
}

fn collect_leaves<'a>(part: &'a Value, out: &mut Vec<&'a Value>) {
    match part.get("parts").and_then(Value::as_array) {
        Some(parts) if !parts.is_empty() => parts.iter().for_each(|p| collect_leaves(p, out)),
        _ => {
            let named = part.get("filename").and_then(Value::as_str).is_some_and(|f| !f.is_empty());
            let attached = part_header(part, "content-disposition").is_some_and(|d| d.trim_start().to_ascii_lowercase().starts_with("attachment"));
            if !named && !attached {
                out.push(part);
            }
        }
    }
}

fn part_header<'a>(part: &'a Value, name: &str) -> Option<&'a str> {
    let headers = part.get("headers").and_then(Value::as_array)?;
    headers.iter().find(|h| h.get("name").and_then(Value::as_str).is_some_and(|n| n.eq_ignore_ascii_case(name))).and_then(|h| h.get("value")).and_then(Value::as_str)
}

/// A part's `body.data` (base64url, padded or not) as text in its
/// charset: UTF-8 (invalid bytes replaced), or Latin-1 when it says so.
fn decode_part(part: &Value) -> String {
    let data = part.pointer("/body/data").and_then(Value::as_str).unwrap_or_default();
    let clean: String = data.chars().filter(|c| !c.is_whitespace() && *c != '=').map(|c| match c {
        '+' => '-',
        '/' => '_',
        c => c,
    }).collect();
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(clean).unwrap_or_default();
    let ctype = part_header(part, "content-type").unwrap_or_default().to_ascii_lowercase();
    let charset = ctype.split(';').find_map(|p| p.trim().strip_prefix("charset=")).map(|c| c.trim_matches('"').to_owned()).unwrap_or_default();
    if matches!(charset.as_str(), "iso-8859-1" | "latin1" | "latin-1" | "windows-1252" | "cp1252") {
        bytes.iter().map(|&b| char::from(b)).collect()
    } else {
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

/// `&amp;`, `&lt;`, `&gt;`, `&quot;`, `&apos;`, `&nbsp;` and numeric
/// references decoded; any other `&` kept.
pub fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let end = rest[1..].find(|c: char| !(c.is_ascii_alphanumeric() || c == '#')).map(|i| i + 1).filter(|&i| rest[i..].starts_with(';'));
        let decoded = end.and_then(|e| {
            let name = &rest[1..e];
            let c = match name {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some('\u{a0}'),
                _ => {
                    let n = name.strip_prefix("#x").or_else(|| name.strip_prefix("#X")).map(|h| u32::from_str_radix(h, 16)).or_else(|| name.strip_prefix('#').map(str::parse::<u32>));
                    n.and_then(Result::ok).and_then(char::from_u32)
                }
            };
            c.map(|c| (c, e + 1))
        });
        match decoded {
            Some((c, len)) => {
                out.push(c);
                rest = &rest[len..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

const BLOCKS: [&str; 18] = ["br", "p", "div", "tr", "li", "ul", "ol", "table", "blockquote", "hr", "h1", "h2", "h3", "h4", "h5", "h6", "section", "article"];
const SKIPPED: [&str; 4] = ["script", "style", "head", "title"];

/// HTML as plain text: tags gone (a block tag ends its line), script,
/// style and head dropped, entities decoded, runs of space collapsed, at
/// most one blank line in a row.
pub fn strip_html(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut text = String::new();
    let mut i = 0;
    while i < html.len() {
        let Some(lt) = html[i..].find('<').map(|n| i + n) else {
            text.push_str(&html[i..].replace(['\n', '\r'], " "));
            break;
        };
        // a line break in the source is a space; block tags end lines
        text.push_str(&html[i..lt].replace(['\n', '\r'], " "));
        if lower[lt..].starts_with("<!--") {
            i = lower[lt..].find("-->").map_or(html.len(), |n| lt + n + 3);
            continue;
        }
        let gt = html[lt..].find('>').map_or(html.len(), |n| lt + n + 1);
        let tag = lower[lt + 1..gt].trim_start_matches('/');
        let name: String = tag.chars().take_while(char::is_ascii_alphanumeric).collect();
        i = gt;
        if SKIPPED.contains(&name.as_str()) && !lower[lt + 1..].starts_with('/') {
            let close = format!("</{name}");
            i = lower[gt..].find(&close).map_or(html.len(), |n| lower[gt + n..].find('>').map_or(html.len(), |m| gt + n + m + 1));
        } else if BLOCKS.contains(&name.as_str()) {
            text.push('\n');
        } else if name == "td" || name == "th" {
            text.push(' ');
        }
    }
    let mut out = String::new();
    let mut blank = 0;
    for line in text.split('\n') {
        let decoded = decode_entities(&line.split_whitespace().collect::<Vec<_>>().join(" "));
        let line = decoded.replace('\u{a0}', " ");
        let line = line.trim();
        if line.is_empty() {
            blank += 1;
            continue;
        }
        if !out.is_empty() {
            out.push_str(if blank > 0 { "\n\n" } else { "\n" });
        }
        blank = 0;
        out.push_str(line);
    }
    out
}

/// The text on one line, at most 200 UTF-16 units with the ellipsis.
pub fn snippet_of(body: &str) -> String {
    let mut flat = String::new();
    let mut gap = false;
    for c in body.chars() {
        if js::is_space(c) {
            gap = true;
        } else {
            if gap && !flat.is_empty() {
                flat.push(' ');
            }
            gap = false;
            flat.push(c);
        }
    }
    if js::len_utf16(&flat) > SNIPPET_MAX {
        format!("{}…", js::slice_utf16(&flat, SNIPPET_MAX - 1))
    } else {
        flat
    }
}

/// `text` within `max` UTF-8 bytes, marked when cut; never half a character.
pub fn cap_bytes(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    const MARKER: &str = "…[truncated]";
    let n = max - MARKER.len();
    let mut end = n;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut cut = &text[..end];
    // as Node cut it: one trailing U+FFFD goes (the half character, or, cut
    // on a boundary, a real one that ends the cut)
    if end == n {
        cut = cut.strip_suffix('\u{fffd}').unwrap_or(cut);
    }
    format!("{cut}{MARKER}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn b64(s: &str) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(s)
    }

    #[test]
    fn headers_are_found_case_insensitively() {
        let m = json!({"payload": {"headers": [{"name": "from", "value": "  Ann <ann@example.com> "}, {"name": "Subject", "value": "Hi"}]}});
        assert_eq!(header(&m, "From"), "Ann <ann@example.com>");
        assert_eq!(header(&m, "subject"), "Hi");
        assert_eq!(header(&m, "Date"), "");
        assert_eq!(header(&json!({}), "From"), "");
    }

    #[test]
    fn the_body_is_the_text_plain_part_else_the_html_one_stripped() {
        let plain = json!({"payload": {"mimeType": "multipart/mixed", "parts": [
            {"mimeType": "multipart/alternative", "parts": [
                {"mimeType": "text/plain", "headers": [{"name": "Content-Type", "value": "text/plain; charset=\"UTF-8\""}], "body": {"data": b64("Your 1701Q was received.\r\n\r\nRef: 2026-ABC\r\n\r\n")}},
                {"mimeType": "text/html", "body": {"data": b64("<p>html</p>")}},
            ]},
            {"mimeType": "text/plain", "filename": "notes.txt", "body": {"attachmentId": "a1"}},
        ]}});
        assert_eq!(body(&plain), "Your 1701Q was received.\n\nRef: 2026-ABC");
        let html = json!({"payload": {"mimeType": "multipart/alternative", "parts": [
            {"mimeType": "text/html", "body": {"data": b64("<html><head><title>x</title><style>p{}</style></head><body><p>Hello&nbsp;<b>Ann</b>,</p><p>See   you<br>at noon &amp; bring &#8364;5 &#x1F600;</p><script>evil()</script><!-- c --></body></html>")}},
        ]}});
        assert_eq!(body(&html), "Hello Ann,\n\nSee you\nat noon & bring €5 😀");
        // a single-part message, padded standard base64 and Latin-1
        let latin = json!({"payload": {"mimeType": "text/plain", "headers": [{"name": "content-type", "value": "text/plain; charset=ISO-8859-1"}], "body": {"data": "Y2Fm6Q=="}}});
        assert_eq!(body(&latin), "café");
        let bad_utf8 = json!({"payload": {"mimeType": "text/plain", "body": {"data": "Y2Fm6Q"}}});
        assert_eq!(body(&bad_utf8), "caf\u{fffd}");
        assert_eq!(body(&json!({"payload": {"mimeType": "image/png", "body": {"data": "AAAA"}}})), "");
        let attached = json!({"payload": {"mimeType": "multipart/mixed", "parts": [{"mimeType": "text/plain", "headers": [{"name": "Content-Disposition", "value": "attachment"}], "body": {"data": b64("no")}}]}});
        assert_eq!(body(&attached), "");
    }

    #[test]
    fn entities_decode_and_unknown_ones_stay() {
        assert_eq!(decode_entities("It&#39;s &quot;ok&quot; &lt;b&gt; &amp;amp; &copy; AT&T &#xZZ; &"), "It's \"ok\" <b> &amp; &copy; AT&T &#xZZ; &");
    }

    #[test]
    fn html_keeps_one_blank_line_and_drops_skipped_blocks() {
        assert_eq!(strip_html("<div>a</div><p></p><p></p><div>b</div>"), "a\n\nb");
        assert_eq!(strip_html("<table><tr><td>x</td><td>y</td></tr></table>"), "x y");
        assert_eq!(strip_html("plain < text"), "plain");
        assert_eq!(strip_html("one\ntwo<br>\nthree"), "one two\nthree");
        assert_eq!(strip_html("<SCRIPT>a</SCRIPT>ok<style"), "ok");
    }

    #[test]
    fn snippets_are_one_line_and_capped() {
        assert_eq!(snippet_of("  See you \u{a0} at\tnoon.\r\n\n"), "See you at noon.");
        let long = format!("{}abcd😀tail", "word ".repeat(39));
        assert_eq!(snippet_of(&long), format!("{}abcd…", "word ".repeat(39)));
        assert_eq!(js::len_utf16(&snippet_of(&"x".repeat(500))), 200);
        assert_eq!(snippet(&json!({"snippet": "Don&#39;t  forget"})), "Don't forget");
        assert_eq!(snippet(&json!({})), "");
    }

    #[test]
    fn bodies_are_capped_at_64_kib_on_a_character_boundary() {
        let big = cap_bytes(&"é".repeat(70_000), BODY_MAX);
        assert!(big.len() <= BODY_MAX && big.ends_with("…[truncated]"));
        assert_eq!(big.len(), 65_522 + "…[truncated]".len());
        let three = cap_bytes(&format!("a{}", "€".repeat(30_000)), BODY_MAX);
        assert_eq!(three.len(), 1 + 3 * 21_840 + "…[truncated]".len());
        let fffd = cap_bytes(&format!("{}\u{fffd}{}", "a".repeat(65_519), "b".repeat(100)), BODY_MAX);
        assert_eq!(fffd, format!("{}…[truncated]", "a".repeat(65_519)));
        assert_eq!(cap_bytes("short", BODY_MAX), "short");
    }

    #[test]
    fn message_ids_are_path_safe() {
        assert!(id_ok("18f1a2b3c4d5e6f7") && id_ok("a-b_c"));
        for bad in ["", "../x", "a/b", "a?b", "a b", "a%2F", &"x".repeat(65)] {
            assert!(!id_ok(bad), "{bad}");
        }
    }
}
