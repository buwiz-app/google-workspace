//! The plugin's two read-only mail tools. Their allowlist is the plugin's
//! read-only boundary: nothing else of the upstream's is reachable.

use serde_json::{json, Value};

use crate::gmail::{self, BODY_MAX};
use crate::js;
use crate::upstream::Upstream;

const SEARCH: &str = "search_gmail_messages";
const BATCH: &str = "get_gmail_messages_content_batch";

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

/// tools/list's entries for the mail tools.
pub fn list() -> Vec<Value> {
    let read_only = json!({"readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": true});
    vec![
        json!({
            "name": "gws_search_mail",
            "description": "Search the user's Gmail (Gmail search syntax). Returns {items: [{id, from, subject, date, snippet}]}; read one with gws_read_mail.",
            "inputSchema": {
                "type": "object",
                "properties": {"query": {"type": "string", "description": "Gmail search, e.g. from:bir.gov.ph newer_than:2d"}, "limit": {"type": "integer", "minimum": 1, "maximum": 25, "description": "at most this many messages (default 10)"}},
                "required": ["query"],
            },
            "annotations": read_only,
        }),
        json!({
            "name": "gws_read_mail",
            "description": "Read one Gmail message by id (from gws_search_mail). Returns {id, from, to, subject, date, body}; the body is plain text, at most 64 KiB.",
            "inputSchema": {"type": "object", "properties": {"id": {"type": "string", "description": "the message id"}}, "required": ["id"]},
            "annotations": read_only,
        }),
    ]
}

pub fn has(name: &str) -> bool {
    name == "gws_search_mail" || name == "gws_read_mail"
}

/// Runs a mail tool; a failure is a plain isError result, never a crash.
pub async fn run(up: &Upstream, name: &str, args: &Value) -> Value {
    let r = match name {
        "gws_search_mail" => search_mail(up, args).await,
        _ => read_mail(up, args).await,
    };
    r.unwrap_or_else(|e| plain_error(&js::slice_utf16(&e, 500)))
}

async fn search_mail(up: &Upstream, args: &Value) -> Result<Value, String> {
    let query = js::trim(&js::string_or_empty(args.get("query"))).to_owned();
    if query.is_empty() {
        return Ok(plain_error("gws_search_mail needs a query"));
    }
    let n = js::to_number(args.get("limit"));
    let n = if n == 0.0 || n.is_nan() { 10.0 } else { n };
    let limit = js::round(n).clamp(1.0, 25.0) as usize;
    let mut ids = gmail::search_ids(&up.call(SEARCH, json!({"query": query, "page_size": limit})).await?);
    ids.truncate(limit);
    if ids.is_empty() {
        return Ok(ok(json!({"items": []})));
    }
    // a later copy of an id wins, as a Map built from the list
    let found = gmail::batch_messages(&up.call(BATCH, json!({"message_ids": ids, "format": "full"})).await?);
    let items: Vec<Value> = ids
        .iter()
        .map(|id| match found.iter().rev().find(|m| &m.id == id) {
            Some(m) => json!({"id": id, "from": m.from, "subject": m.subject, "date": m.date, "snippet": gmail::snippet_of(&m.body)}),
            None => json!({"id": id, "from": "", "subject": "", "date": "", "snippet": ""}),
        })
        .collect();
    Ok(ok(json!({"items": items})))
}

async fn read_mail(up: &Upstream, args: &Value) -> Result<Value, String> {
    let id = js::trim(&js::string_or_empty(args.get("id"))).to_owned();
    if id.is_empty() {
        return Ok(plain_error("gws_read_mail needs an id"));
    }
    let found = gmail::batch_messages(&up.call(BATCH, json!({"message_ids": [id], "format": "full"})).await?);
    let Some(m) = found.into_iter().find(|m| m.id == id) else { return Ok(plain_error(&format!("no message {}", js::slice_utf16(&id, 80)))) };
    Ok(ok(json!({"id": id, "from": m.from, "to": m.to, "subject": m.subject, "date": m.date, "body": gmail::cap_bytes(&m.body, BODY_MAX)})))
}
