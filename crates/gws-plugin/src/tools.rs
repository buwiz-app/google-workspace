//! The plugin's tools, each with who may call it. A tool names its audience
//! itself ([`Def::audience`]): protocol 16.4 reads a missing audience as open
//! to the model while the Hub's press check reads it as closed, so nothing
//! here leaves it to either. `serve` checks it too.

use serde_json::{json, Value};

use buwiz_plugin::Tool;

use crate::accounts::{Accounts, GOOGLE};
use crate::gmail::{self, Failure, Gmail, BODY_MAX};
use crate::js;

pub const SIGN_IN: &str = "Sign in to Google in Buwiz Bot";

/// A tool result: `structuredContent` and its JSON as the text.
pub fn ok(structured: Value) -> Value {
    json!({"content": [{"type": "text", "text": structured.to_string()}], "structuredContent": structured})
}

/// A failed tool call with a `{code, message}` for Rox.
pub fn fail(code: &str, message: &str) -> Value {
    json!({"content": [{"type": "text", "text": message}], "structuredContent": {"code": code, "message": message}, "isError": true})
}

/// A failed tool call with only its message.
pub fn plain_error(message: &str) -> Value {
    json!({"content": [{"type": "text", "text": message}], "isError": true})
}

/// Who may call a tool: `_meta["buwiz/audience"]`.
pub const AGENT_AND_APP: &[&str] = &["agent", "app"];
/// The person's screens only: not on the model's list, and a call from the model is refused.
pub const APP_ONLY: &[&str] = &["app"];

/// One tool: what `tools/list` shows and who may call it.
pub struct Def {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
    pub read_only: bool,
    pub audience: &'static [&'static str],
    /// Reaches past this Mac, to Google.
    pub open_world: bool,
}

pub const SEARCH_MAIL: &str = "gws_search_mail";
pub const READ_MAIL: &str = "gws_read_mail";
pub const STATUS: &str = "gws_status";
pub const RESOLVE: &str = "gws_resolve";

/// Every tool, in `tools/list` order.
pub fn defs() -> Vec<Def> {
    vec![
        Def {
            name: SEARCH_MAIL,
            description: "Search the user's Gmail (Gmail search syntax). Returns {items: [{id, from, subject, date, snippet}]}; read one with gws_read_mail.",
            input_schema: json!({
                "type": "object",
                "properties": {"query": {"type": "string", "description": "Gmail search, e.g. from:bir.gov.ph newer_than:2d"}, "limit": {"type": "integer", "minimum": 1, "maximum": 25, "description": "at most this many messages (default 10)"}},
                "required": ["query"],
            }),
            read_only: true,
            audience: AGENT_AND_APP,
            open_world: true,
        },
        Def {
            name: READ_MAIL,
            description: "Read one Gmail message by id (from gws_search_mail). Returns {id, from, to, subject, date, body}; the body is plain text, at most 64 KiB.",
            input_schema: json!({"type": "object", "properties": {"id": {"type": "string", "description": "the message id"}}, "required": ["id"]}),
            read_only: true,
            audience: AGENT_AND_APP,
            open_world: true,
        },
        Def {
            name: STATUS,
            description: "The signed-in Google account and which features its access covers.",
            input_schema: json!({"type": "object", "properties": {}}),
            read_only: true,
            audience: APP_ONLY,
            open_world: false,
        },
        Def {
            name: RESOLVE,
            description: "Resolve the /gws command.",
            input_schema: json!({"type": "object"}),
            read_only: true,
            audience: APP_ONLY,
            open_world: false,
        },
    ]
}

/// The SDK's view of [`defs`].
pub fn list() -> Vec<Tool> {
    defs().into_iter().map(|d| Tool { name: d.name, description: d.description, input_schema: d.input_schema, read_only: d.read_only }).collect()
}

pub fn has(name: &str) -> bool {
    defs().iter().any(|d| d.name == name)
}

/// A tool that needs the Google account the Bot has not pushed (protocol 1.0, accounts):
/// the host turns it into a "Sign in to Google" card.
pub fn account_required() -> Value {
    json!({"content": [{"type": "text", "text": SIGN_IN}], "structuredContent": {"code": "account_required", "account": GOOGLE, "message": SIGN_IN}, "isError": true})
}

/// Runs a mail tool; a failure is an isError result, never a crash.
pub fn run(gmail: &Gmail, accounts: &Accounts, name: &str, args: &Value) -> Value {
    let r = match name {
        STATUS => Ok(status(accounts)),
        RESOLVE => Ok(resolve(args)),
        SEARCH_MAIL => search_mail(gmail, accounts, args),
        READ_MAIL => read_mail(gmail, accounts, args),
        _ => Ok(fail("not_found", "No such tool.")),
    };
    match r {
        Ok(v) => v,
        Err(Failure::AccountRequired) => account_required(),
        Err(Failure::Plain(e)) => plain_error(&js::slice_utf16(&e, 500)),
    }
}

/// What the screens need to know about the account. The address is shown to
/// the person, so this tool is never the model's.
fn status(accounts: &Accounts) -> Value {
    match accounts.signed_in() {
        Some((subject, scopes)) => ok(json!({"signed_in": true, "subject": subject, "scopes": scopes})),
        None => ok(json!({"signed_in": false})),
    }
}

/// `/mail`, `/calendar`, `/drive`, and `/gws <one of them>`: opens that screen.
fn resolve(args: &Value) -> Value {
    let name = js::string_or_empty(args.get("name"));
    let value = js::trim(&js::string_or_empty(args.get("value"))).to_lowercase();
    let wanted = if name == "gws" { value.as_str() } else { name.as_str() };
    let (app, title, ..) = crate::manifest::APPS.iter().find(|a| a.0 == wanted).unwrap_or(&crate::manifest::APPS[0]);
    ok(json!({"result": "open", "app": app, "title": title, "placement": "full"}))
}

fn search_mail(gmail: &Gmail, accounts: &Accounts, args: &Value) -> Result<Value, Failure> {
    let query = js::trim(&js::string_or_empty(args.get("query"))).to_owned();
    if query.is_empty() {
        return Ok(plain_error("gws_search_mail needs a query"));
    }
    let n = js::to_number(args.get("limit"));
    let n = if n == 0.0 || n.is_nan() { 10.0 } else { n };
    let limit = js::round(n).clamp(1.0, 25.0) as usize;
    let auth = accounts.google().ok_or(Failure::AccountRequired)?;
    let mut ids = gmail.search(&auth, &query, limit)?;
    ids.truncate(limit);
    let found = gmail.metadata(&auth, &ids)?;
    let items: Vec<Value> = ids
        .iter()
        .zip(found)
        .map(|(id, m)| match m {
            Some(m) => json!({"id": id, "from": gmail::header(&m, "From"), "subject": gmail::header(&m, "Subject"), "date": gmail::header(&m, "Date"), "snippet": gmail::snippet(&m)}),
            None => json!({"id": id, "from": "", "subject": "", "date": "", "snippet": ""}),
        })
        .collect();
    Ok(ok(json!({"items": items})))
}

fn read_mail(gmail: &Gmail, accounts: &Accounts, args: &Value) -> Result<Value, Failure> {
    let id = js::trim(&js::string_or_empty(args.get("id"))).to_owned();
    if id.is_empty() {
        return Ok(plain_error("gws_read_mail needs an id"));
    }
    let auth = accounts.google().ok_or(Failure::AccountRequired)?;
    let Some(m) = gmail.full(&auth, &id)? else { return Ok(plain_error(&format!("no message {}", js::slice_utf16(&id, 80)))) };
    let h = |name| gmail::header(&m, name);
    Ok(ok(json!({"id": id, "from": h("From"), "to": h("To"), "subject": h("Subject"), "date": h("Date"), "body": gmail::cap_bytes(&gmail::body(&m), BODY_MAX)})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gws_fake_google::{self as fake_gmail, Fake};
    use base64::Engine as _;

    const TOKEN: &str = "ya29.never-shown";

    fn b64(s: &str) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(s)
    }

    fn signed_in(expires_at: Value) -> Accounts {
        let a = Accounts::default();
        a.set(&json!({"account": "google", "label": "default", "provider": "google", "subject": "owner@example.com", "scopes": ["https://www.googleapis.com/auth/gmail.readonly"], "access_token": TOKEN, "token_type": "Bearer", "expires_at": expires_at}));
        a
    }

    fn in_an_hour() -> Value {
        json!(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() + 3600)
    }

    fn meta(id: &str, from: &str, subject: &str, snippet: &str) -> String {
        json!({"id": id, "snippet": snippet, "payload": {"headers": [{"name": "From", "value": from}, {"name": "Subject", "value": subject}, {"name": "Date", "value": "Mon, 26 Oct 2026 09:12:00 +0800"}]}}).to_string()
    }

    /// A mailbox of two messages (`18f1a`, `18f1b`) and one the list names
    /// but Gmail no longer has (`gone`).
    fn mailbox() -> Fake {
        fake_gmail::serve(|target| {
            let (path, _) = target.split_once('?').unwrap_or((target, ""));
            match path {
                "/messages" if target.contains("q=nothing") => (200, json!({"resultSizeEstimate": 0}).to_string()),
                "/messages" => (200, json!({"messages": [{"id": "18f1a", "threadId": "t"}, {"id": "gone", "threadId": "t"}, {"id": "18f1b", "threadId": "t"}], "resultSizeEstimate": 3}).to_string()),
                "/messages/18f1a" if target.contains("format=full") => (
                    200,
                    json!({"id": "18f1a", "payload": {"mimeType": "multipart/alternative", "headers": [{"name": "From", "value": "BIR eFPS <efps@bir.gov.ph>"}, {"name": "To", "value": "owner@example.com"}, {"name": "Subject", "value": "Tax Return Receipt Confirmation"}, {"name": "Date", "value": "Mon, 26 Oct 2026 09:12:00 +0800"}], "parts": [
                        {"mimeType": "text/plain", "body": {"data": b64("Your 1701Q for TIN 000-000-000 was received.\r\n\r\nReference: 2026-ABC-123\r\n")}},
                        {"mimeType": "text/html", "body": {"data": b64("<p>html</p>")}},
                    ]}})
                    .to_string(),
                ),
                "/messages/big" => (200, json!({"id": "big", "payload": {"mimeType": "text/html", "headers": [], "body": {"data": b64(&format!("<div>{}</div>", "é".repeat(70_000)))}}}).to_string()),
                "/messages/18f1a" => (200, meta("18f1a", "BIR eFPS <efps@bir.gov.ph>", "Tax Return Receipt Confirmation", "Your 1701Q for TIN 000-000-000 was received. Reference: 2026-ABC-123")),
                "/messages/18f1b" => (200, meta("18f1b", "Ana <ana@example.com>", "Lunch", "  See you   at noon &amp; don&#39;t be late")),
                "/messages/denied" => (403, json!({"error": {"code": 403, "message": "Request had insufficient authentication scopes.", "status": "PERMISSION_DENIED"}}).to_string()),
                _ => (404, json!({"error": {"code": 404, "message": "Requested entity was not found."}}).to_string()),
            }
        })
    }

    fn data(r: &Value) -> &Value {
        &r["structuredContent"]
    }

    #[test]
    fn search_lists_then_reads_each_messages_metadata() {
        let fake = mailbox();
        let (gmail, accounts) = (Gmail::new(&fake.base), signed_in(in_an_hour()));
        let r = run(&gmail, &accounts, "gws_search_mail", &json!({"query": " from:bir.gov.ph newer_than:2d ", "limit": 3}));
        assert_eq!(r.get("isError"), None, "{r}");
        assert_eq!(
            data(&r),
            &json!({"items": [
                {"id": "18f1a", "from": "BIR eFPS <efps@bir.gov.ph>", "subject": "Tax Return Receipt Confirmation", "date": "Mon, 26 Oct 2026 09:12:00 +0800", "snippet": "Your 1701Q for TIN 000-000-000 was received. Reference: 2026-ABC-123"},
                {"id": "gone", "from": "", "subject": "", "date": "", "snippet": ""},
                {"id": "18f1b", "from": "Ana <ana@example.com>", "subject": "Lunch", "date": "Mon, 26 Oct 2026 09:12:00 +0800", "snippet": "See you at noon & don't be late"},
            ]})
        );
        assert_eq!(r["content"][0]["text"], data(&r).to_string(), "the text is the structured JSON");
        let seen = fake.seen();
        assert_eq!(seen.len(), 4);
        assert!(seen[0].target.starts_with("/messages?q=from%3Abir.gov.ph") && seen[0].target.ends_with("&maxResults=3"), "{:?}", seen[0]);
        let mut metas: Vec<&str> = seen[1..].iter().map(|s| s.target.as_str()).collect();
        metas.sort();
        assert_eq!(metas, ["/messages/18f1a?format=metadata&metadataHeaders=From&metadataHeaders=Subject&metadataHeaders=Date", "/messages/18f1b?format=metadata&metadataHeaders=From&metadataHeaders=Subject&metadataHeaders=Date", "/messages/gone?format=metadata&metadataHeaders=From&metadataHeaders=Subject&metadataHeaders=Date"]);
        assert!(seen.iter().all(|s| s.authorization == format!("Bearer {TOKEN}")), "the pushed token goes to Gmail");
        assert!(!r.to_string().contains(TOKEN), "and never into a result");

        let none = run(&gmail, &accounts, "gws_search_mail", &json!({"query": "nothing"}));
        assert_eq!(data(&none), &json!({"items": []}));
        assert!(fake.seen().last().unwrap().target.ends_with("maxResults=10"), "10 by default");
        let capped = run(&gmail, &accounts, "gws_search_mail", &json!({"query": "nothing", "limit": 99}));
        assert_eq!(data(&capped), &json!({"items": []}));
        assert!(fake.seen().last().unwrap().target.ends_with("maxResults=25"));
    }

    #[test]
    fn read_gives_the_headers_and_the_plain_body() {
        let fake = mailbox();
        let (gmail, accounts) = (Gmail::new(&fake.base), signed_in(in_an_hour()));
        let r = run(&gmail, &accounts, "gws_read_mail", &json!({"id": "18f1a"}));
        assert_eq!(
            data(&r),
            &json!({"id": "18f1a", "from": "BIR eFPS <efps@bir.gov.ph>", "to": "owner@example.com", "subject": "Tax Return Receipt Confirmation", "date": "Mon, 26 Oct 2026 09:12:00 +0800", "body": "Your 1701Q for TIN 000-000-000 was received.\n\nReference: 2026-ABC-123"})
        );
        assert_eq!(fake.seen()[0].target, "/messages/18f1a?format=full");
        let big = run(&gmail, &accounts, "gws_read_mail", &json!({"id": "big"}));
        let body = data(&big)["body"].as_str().unwrap();
        assert!(body.len() <= BODY_MAX && body.ends_with("…[truncated]") && body.starts_with("éé"), "the body is capped at 64 KiB");
        assert_eq!((data(&big)["from"].as_str(), data(&big)["to"].as_str()), (Some(""), Some("")));
        let gone = run(&gmail, &accounts, "gws_read_mail", &json!({"id": "nope"}));
        assert_eq!(gone, plain_error("no message nope"));
        let n = fake.seen().len();
        assert_eq!(run(&gmail, &accounts, "gws_read_mail", &json!({"id": "../../labels"})), plain_error("no message ../../labels"));
        assert_eq!(fake.seen().len(), n, "an id that is not Gmail's is never sent");
        let denied = run(&gmail, &accounts, "gws_read_mail", &json!({"id": "denied"}));
        assert_eq!(denied, plain_error("Gmail answered 403: Request had insufficient authentication scopes."));
        assert!(!format!("{r}{big}{gone}{denied}").contains(TOKEN));
    }

    #[test]
    fn a_missing_or_expired_token_asks_for_sign_in_without_calling_gmail() {
        let fake = mailbox();
        let gmail = Gmail::new(&fake.base);
        let want = json!({"content": [{"type": "text", "text": "Sign in to Google in Buwiz Bot"}], "structuredContent": {"code": "account_required", "account": "google", "message": "Sign in to Google in Buwiz Bot"}, "isError": true});
        let expired = signed_in(json!(1_000_000));
        let cleared = signed_in(in_an_hour());
        cleared.clear(&json!({"account": "google", "label": "default"}));
        for accounts in [Accounts::default(), expired, cleared] {
            assert_eq!(run(&gmail, &accounts, "gws_search_mail", &json!({"query": "x"})), want);
            assert_eq!(run(&gmail, &accounts, "gws_read_mail", &json!({"id": "18f1a"})), want);
        }
        assert!(fake.seen().is_empty());
        // the argument checks still come first
        assert_eq!(run(&gmail, &Accounts::default(), "gws_search_mail", &json!({"query": " "})), plain_error("gws_search_mail needs a query"));
        assert_eq!(run(&gmail, &Accounts::default(), "gws_read_mail", &json!({})), plain_error("gws_read_mail needs an id"));
    }

    #[test]
    fn gmail_answering_401_asks_for_sign_in() {
        let fake = fake_gmail::serve(|_| (401, json!({"error": {"code": 401, "message": "Request had invalid authentication credentials."}}).to_string()));
        let (gmail, accounts) = (Gmail::new(&fake.base), signed_in(in_an_hour()));
        for (tool, args) in [("gws_search_mail", json!({"query": "x"})), ("gws_read_mail", json!({"id": "18f1a"}))] {
            let r = run(&gmail, &accounts, tool, &args);
            assert_eq!(data(&r)["code"], "account_required", "{tool}");
            assert_eq!(data(&r)["account"], "google");
        }
        // a 401 on a metadata get, after the list
        let fake = fake_gmail::serve(|t| if t.starts_with("/messages?") { (200, json!({"messages": [{"id": "a1"}]}).to_string()) } else { (401, "{}".to_owned()) });
        let r = run(&Gmail::new(&fake.base), &accounts, "gws_search_mail", &json!({"query": "x"}));
        assert_eq!(data(&r)["code"], "account_required");
    }

    #[test]
    fn an_unreachable_gmail_is_a_plain_error() {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let r = run(&Gmail::new(&format!("http://127.0.0.1:{port}")), &signed_in(in_an_hour()), "gws_search_mail", &json!({"query": "x"}));
        assert_eq!(r["isError"], true);
        let text = r["content"][0]["text"].as_str().unwrap();
        assert!(text.starts_with("could not reach Gmail: ") && !text.contains(TOKEN), "{text}");
    }
}
