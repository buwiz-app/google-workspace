//! The MCP server on stdio, answering as server.mjs did through the Node
//! SDK (1.32): one JSON-RPC message a line; responses written
//! `{result, jsonrpc, id}` or `{jsonrpc, id, error}`; a thrown McpError's
//! message `MCP error <code>: <text>`; a request the SDK's schemas refuse
//! answered -32603 with zod's issue list; a line that is not a JSON-RPC
//! message skipped. Requests run concurrently, and one cancelled by
//! `notifications/cancelled` gets no answer.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinSet;

use crate::reserved::{self, Key};
use crate::upstream::Upstream;
use crate::{config, tools};

const LATEST: &str = "2025-11-25";
const SUPPORTED: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05", "2024-10-07"];
const INSTRUCTIONS: &str = "Gmail is read-only here: gws_search_mail finds messages, gws_read_mail reads one. Neither sends, labels or deletes mail.";
const MANIFEST_URI: &str = "rox://manifest";

const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_REQUEST: i64 = -32600;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL: i64 = -32603;

/// A failed request: its JSON-RPC error.
struct Error {
    code: i64,
    message: String,
}

impl Error {
    /// The SDK's `McpError`.
    fn mcp(code: i64, text: &str) -> Self {
        Self { code, message: format!("MCP error {code}: {text}") }
    }

    /// A plain `Error` thrown in a handler.
    fn internal(message: String) -> Self {
        Self { code: INTERNAL, message }
    }
}

#[derive(Default)]
struct Plugin {
    key: Key,
    upstream: Upstream,
}

/// Serves until stdin ends (after the requests in flight) or a SIGTERM.
pub async fn serve() -> std::io::Result<()> {
    let plugin = Arc::new(Plugin::default());
    let out = Arc::new(AsyncMutex::new(tokio::io::stdout()));
    let running: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>> = Arc::default();
    let mut tasks = JoinSet::new();
    let mut lines = BufReader::new(tokio::io::stdin()).split(b'\n');
    let mut term = signal(SignalKind::terminate())?;
    loop {
        let raw = tokio::select! {
            line = lines.next_segment() => match line? {
                Some(raw) => raw,
                None => break,
            },
            _ = term.recv() => return Ok(()),
        };
        let text = String::from_utf8_lossy(&raw);
        let Ok(Value::Object(msg)) = serde_json::from_str::<Value>(text.strip_suffix('\r').unwrap_or(&text)) else { continue };
        if msg.get("jsonrpc") != Some(&json!("2.0")) || msg.get("params").is_some_and(|p| !p.is_object()) {
            continue;
        }
        let Some(method) = msg.get("method").and_then(Value::as_str).map(str::to_owned) else { continue };
        let params = msg.get("params").cloned();
        let Some(id) = msg.get("id") else {
            if method == "notifications/cancelled" {
                let request = params.as_ref().and_then(|p| p.get("requestId")).map(Value::to_string).unwrap_or_default();
                if let Some(flag) = running.lock().unwrap_or_else(|e| e.into_inner()).get(&request) {
                    flag.store(true, Ordering::SeqCst);
                }
            }
            continue;
        };
        let id = match id {
            Value::String(_) => id.clone(),
            Value::Number(n) if n.as_f64().is_some_and(|f| f.fract() == 0.0) => n.as_i64().map_or_else(|| id.clone(), Value::from),
            _ => continue,
        };
        let key = id.to_string();
        let cancelled = Arc::new(AtomicBool::new(false));
        running.lock().unwrap_or_else(|e| e.into_inner()).insert(key.clone(), cancelled.clone());
        let (plugin, out, running) = (plugin.clone(), out.clone(), running.clone());
        tasks.spawn(async move {
            let answer = handle(&plugin, &method, params.as_ref()).await;
            running.lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
            if cancelled.load(Ordering::SeqCst) {
                return;
            }
            let reply = match answer {
                Ok(result) => json!({"result": result, "jsonrpc": "2.0", "id": id}),
                Err(e) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": e.code, "message": e.message}}),
            };
            let mut line = reply.to_string();
            line.push('\n');
            let mut out = out.lock().await;
            let _ = out.write_all(line.as_bytes()).await;
            let _ = out.flush().await;
        });
        while tasks.try_join_next().is_some() {}
    }
    // stdin is done: answer what is in flight, then go (the upstream with us)
    tokio::select! {
        _ = async { while tasks.join_next().await.is_some() {} } => {}
        _ = term.recv() => {}
    }
    Ok(())
}

async fn handle(plugin: &Plugin, method: &str, params: Option<&Value>) -> Result<Value, Error> {
    match method {
        "initialize" => {
            check(params, &[("protocolVersion", "string"), ("capabilities", "object"), ("clientInfo", "object")], &[("clientInfo", &[("name", "string"), ("version", "string")])])?;
            let asked = params.and_then(|p| p.get("protocolVersion")).and_then(Value::as_str).unwrap_or_default();
            let version = if SUPPORTED.contains(&asked) { asked } else { LATEST };
            Ok(json!({
                "protocolVersion": version,
                "capabilities": {"tools": {}, "resources": {}},
                "serverInfo": {"name": "rox-google-workspace", "version": "1.0.0"},
                "instructions": INSTRUCTIONS,
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => {
            optional_cursor(params)?;
            let mut list: Vec<Value> = reserved::NAMES.iter().map(|n| json!({"name": n, "description": "Reserved for Buwiz Bot.", "inputSchema": {"type": "object"}})).collect();
            list.extend(tools::list());
            Ok(json!({"tools": list}))
        }
        "tools/call" => {
            check(params, &[("name", "string"), ("arguments", "record?")], &[])?;
            let p = params.expect("checked");
            let name = p.get("name").and_then(Value::as_str).unwrap_or_default();
            let empty = json!({});
            let args = p.get("arguments").unwrap_or(&empty);
            call_tool(plugin, name, args).await
        }
        "resources/list" => {
            optional_cursor(params)?;
            Ok(json!({"resources": [{"uri": MANIFEST_URI, "name": "manifest", "mimeType": "application/json"}]}))
        }
        "resources/read" => {
            check(params, &[("uri", "string")], &[])?;
            let uri = params.and_then(|p| p.get("uri")).and_then(Value::as_str).unwrap_or_default();
            if uri != MANIFEST_URI {
                return Err(Error::mcp(INVALID_PARAMS, &format!("no resource {uri}")));
            }
            let text = config::manifest_text().map_err(Error::internal)?;
            Ok(json!({"contents": [{"uri": uri, "mimeType": "application/json", "text": text}]}))
        }
        _ => Err(Error { code: METHOD_NOT_FOUND, message: "Method not found".to_owned() }),
    }
}

async fn call_tool(plugin: &Plugin, name: &str, args: &Value) -> Result<Value, Error> {
    if reserved::NAMES.contains(&name) {
        return reserved::call(&plugin.key, name, args).map_err(Error::internal);
    }
    if is_reserved_name(name) {
        return Err(Error::mcp(METHOD_NOT_FOUND, &format!("{name} is not a tool of this plugin")));
    }
    // P7: nothing before a successful provision
    if !plugin.key.is_set() {
        return Err(Error::mcp(INVALID_REQUEST, "rox.provision must come first"));
    }
    if !tools::has(name) {
        return Err(Error::mcp(INVALID_PARAMS, &format!("no tool {name}")));
    }
    Ok(tools::run(&plugin.upstream, name, args).await)
}

/// `^rox\.[a-z.]+$` (grammar.mjs `reservedTool`).
fn is_reserved_name(name: &str) -> bool {
    name.strip_prefix("rox.").is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_lowercase() || b == b'.'))
}

/// zod's name for a value's type.
fn kind(v: Option<&Value>) -> &'static str {
    match v {
        None => "undefined",
        Some(Value::Null) => "null",
        Some(Value::Bool(_)) => "boolean",
        Some(Value::Number(_)) => "number",
        Some(Value::String(_)) => "string",
        Some(Value::Array(_)) => "array",
        Some(Value::Object(_)) => "object",
    }
}

/// The SDK's request schema check for `params`: each `(field, type)` in
/// order (`record?` an optional object), then each nested object's fields;
/// a miss throws zod's issues, which the SDK answers as -32603.
fn check(params: Option<&Value>, fields: &[(&str, &str)], nested: &[(&str, &[(&str, &str)])]) -> Result<(), Error> {
    let Some(p) = params else { return Err(zod(vec![issue("object", &["params"], None)])) };
    let mut issues = Vec::new();
    for (field, want) in fields {
        let v = p.get(*field);
        let fits = match *want {
            "record?" => v.is_none_or(Value::is_object),
            "record" | "object" => v.is_some_and(Value::is_object),
            _ => v.is_some_and(Value::is_string),
        };
        if !fits {
            issues.push(issue(want.trim_end_matches('?'), &["params", field], v));
        } else if let Some((_, inner)) = nested.iter().find(|(n, _)| n == field) {
            for (f, w) in *inner {
                let iv = v.and_then(|v| v.get(*f));
                if !iv.is_some_and(Value::is_string) {
                    issues.push(issue(w, &["params", field, f], iv));
                }
            }
        }
    }
    if issues.is_empty() {
        Ok(())
    } else {
        Err(zod(issues))
    }
}

/// tools/list and resources/list: an optional string `cursor`.
fn optional_cursor(params: Option<&Value>) -> Result<(), Error> {
    match params.and_then(|p| p.get("cursor")) {
        Some(c) if !c.is_string() => Err(zod(vec![issue("string", &["params", "cursor"], Some(c))])),
        _ => Ok(()),
    }
}

fn issue(expected: &str, path: &[&str], got: Option<&Value>) -> Value {
    json!({"expected": expected, "code": "invalid_type", "path": path, "message": format!("Invalid input: expected {expected}, received {}", kind(got))})
}

/// A ZodError's message: its issues as `JSON.stringify(issues, null, 2)`.
fn zod(issues: Vec<Value>) -> Error {
    Error::internal(serde_json::to_string_pretty(&issues).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_names_follow_the_grammar() {
        assert!(is_reserved_name("rox.frobnicate") && is_reserved_name("rox.a.b"));
        assert!(!is_reserved_name("rox.") && !is_reserved_name("rox.X") && !is_reserved_name("gws_read_mail") && !is_reserved_name("rox.a1"));
    }

    #[test]
    fn schema_misses_read_as_zod_issues() {
        let e = check(Some(&json!({"name": 5, "arguments": []})), &[("name", "string"), ("arguments", "record?")], &[]).err().unwrap();
        assert_eq!(e.code, INTERNAL);
        assert_eq!(e.message, "[\n  {\n    \"expected\": \"string\",\n    \"code\": \"invalid_type\",\n    \"path\": [\n      \"params\",\n      \"name\"\n    ],\n    \"message\": \"Invalid input: expected string, received number\"\n  },\n  {\n    \"expected\": \"record\",\n    \"code\": \"invalid_type\",\n    \"path\": [\n      \"params\",\n      \"arguments\"\n    ],\n    \"message\": \"Invalid input: expected record, received array\"\n  }\n]");
        assert!(check(Some(&json!({"name": "x"})), &[("name", "string"), ("arguments", "record?")], &[]).is_ok());
    }
}
