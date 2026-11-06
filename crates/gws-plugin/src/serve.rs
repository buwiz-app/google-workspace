//! The MCP loop. `buwiz_plugin::Server` answers everything but a tool of
//! ours, whose caller it still checks; the tool then runs on its own thread,
//! so a slow Google call holds up nothing. The reply is written whenever it
//! is ready: JSON-RPC ids pair them up.
//!
//! `buwiz.account.set` and `clear` stop here and never reach the SDK, so the
//! pushed token has one home, [`crate::Core`], which the tools' threads and
//! the sync can reach. The SDK's own copy stays empty.

use std::collections::HashMap;
use std::io::{BufRead as _, Write as _};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use buwiz_plugin::manifest::{MANIFEST_ENV, Served};
use buwiz_plugin::{Caller, Origin, Plugin as _, Server};
use serde_json::{Value, json};

use crate::{Gws, tools};

/// Adds each tool's audience and full annotations to a `tools/list` reply:
/// the SDK's `Tool` carries `readOnlyHint` alone.
fn with_audience(mut reply: Value) -> Value {
    let defs = tools::defs();
    for tool in reply
        .pointer_mut("/result/tools")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
    {
        let def = tool
            .get("name")
            .and_then(Value::as_str)
            .and_then(|name| defs.iter().find(|d| d.name == name));
        if let Some(def) = def {
            tool["_meta"] = json!({"buwiz/audience": def.audience});
            tool["annotations"] = json!({"readOnlyHint": def.read_only, "destructiveHint": false, "idempotentHint": def.read_only, "openWorldHint": def.open_world});
        }
    }
    reply
}

/// A message for [`Server`], or one this loop answers.
enum Route {
    Server,
    Initialize,
    List,
    Account(String),
    Provision,
    Cancelled,
    Tool { name: String, args: Value },
}

fn route(message: &Value) -> Route {
    let method = message.get("method").and_then(Value::as_str);
    if method == Some("notifications/cancelled") {
        return Route::Cancelled;
    }
    if method == Some("initialize") {
        return Route::Initialize;
    }
    if method == Some("tools/list") {
        return Route::List;
    }
    // a call with no id is a notification: the SDK answers nothing to it
    if method != Some("tools/call") || message.get("id").is_none() {
        return Route::Server;
    }
    let params = message.get("params");
    match params.and_then(|p| p.get("name")).and_then(Value::as_str) {
        Some(name @ ("buwiz.account.set" | "buwiz.account.clear")) => {
            Route::Account(name.to_owned())
        }
        Some("buwiz.provision") => Route::Provision,
        Some(name) if tools::has(name) => match params.and_then(|p| p.get("arguments")) {
            None | Some(Value::Null) => Route::Tool {
                name: name.to_owned(),
                args: json!({}),
            },
            Some(args) if args.is_object() => Route::Tool {
                name: name.to_owned(),
                args: args.clone(),
            },
            // the SDK's refusal of arguments that aren't an object
            Some(_) => Route::Server,
        },
        _ => Route::Server,
    }
}

fn is_error(reply: Option<&Value>) -> bool {
    reply.is_none_or(|r| {
        r.get("error").is_some() || r.pointer("/result/isError") == Some(&Value::Bool(true))
    })
}

/// The plugin's own check of a tool's audience, beside the Hub's, and closed
/// by default: only a caller it can name reaches anything.
fn audience_allows(name: &str, origin: Origin) -> bool {
    let names = |who: &str| {
        tools::defs()
            .iter()
            .any(|d| d.name == name && d.audience.contains(&who))
    };
    match origin {
        Origin::App => names("app"),
        // a routine's turn is the model's
        Origin::Agent | Origin::Routine => names("agent"),
        Origin::Resolve => name == tools::RESOLVE,
        Origin::Unknown => false,
    }
}

/// The MCP revisions this server speaks, newest first; the broker takes any.
const MCP_VERSIONS: &[&str] = &[
    "2025-11-25",
    "2025-06-18",
    "2025-03-26",
    "2024-11-05",
    "2024-10-07",
];

/// `initialize`'s answer, on the revision the client asked for when it's one
/// of ours, else on the newest.
fn negotiated(mut reply: Value, message: &Value) -> Value {
    let asked = message
        .pointer("/params/protocolVersion")
        .and_then(Value::as_str);
    let version = asked
        .filter(|v| MCP_VERSIONS.contains(v))
        .unwrap_or(MCP_VERSIONS[0]);
    if let Some(result) = reply.get_mut("result") {
        result["protocolVersion"] = json!(version);
    }
    reply
}

/// Whether a parsed line is a JSON-RPC message at all.
fn is_message(message: &Value) -> bool {
    message.get("jsonrpc") == Some(&json!("2.0"))
        && message.get("method").is_some_and(Value::is_string)
        && message.get("params").is_none_or(Value::is_object)
}

/// The error for a line that isn't a request but names an id, so its sender
/// isn't left waiting. With no id there's nobody to answer.
fn invalid(line: &Value) -> Option<Value> {
    let id = line
        .get("id")
        .filter(|id| id.is_string() || id.is_number())?;
    Some(
        json!({"jsonrpc": "2.0", "id": id, "error": {"code": buwiz_plugin::INVALID_REQUEST, "message": "invalid request"}}),
    )
}

/// Tools running at once, past which a call is refused: a Bot's screens hold
/// 8 in flight each, and each call here is a thread.
const MAX_RUNNING: usize = 32;
/// How long exit waits for the sync thread.
const STOP_WAIT: Duration = Duration::from_secs(2);

type Out = Arc<Mutex<std::io::Stdout>>;

fn write(out: &Out, reply: &Value) {
    let mut line = reply.to_string();
    line.push('\n');
    let mut out = out.lock().unwrap_or_else(|e| e.into_inner());
    let _ = out.write_all(line.as_bytes());
    let _ = out.flush();
}

/// The signed manifest, read once. Served from memory, so the bytes the Bot
/// verified are the ones the caller checks use for as long as this process
/// runs, whatever happens to the file. Without one the plugin doesn't start:
/// the SDK would serve the unsigned manifest and check callers against that.
fn pinned_manifest() -> Result<Served, String> {
    let path = std::env::var_os(MANIFEST_ENV)
        .filter(|p| !p.is_empty())
        .ok_or(format!(
            "{MANIFEST_ENV} is not set, so there is no signed manifest to serve"
        ))?;
    let unsigned = std::fs::read_to_string(&path).map_err(|e| format!("{MANIFEST_ENV}: {e}"))?;
    Ok(Served {
        signed: None,
        unsigned,
    })
}

/// A tool's answer. A panic is an error for this call, not a silent caller.
fn answer(run: impl FnOnce() -> Value) -> Value {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(run))
        .unwrap_or_else(|_| buwiz_plugin::fail("internal", "That didn't work. Try again."))
}

/// Runs in-flight requests can be cancelled by id.
type Running = Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>;

/// Serves stdin to stdout until stdin closes, then waits for what's running.
pub fn serve(plugin: Gws) -> Result<(), String> {
    run(plugin, pinned_manifest()?).map_err(|e| e.to_string())
}

fn run(plugin: Gws, manifest: Served) -> std::io::Result<()> {
    let core = plugin.core.clone();
    let syncing = crate::mail::start(core.clone());
    let mut server = Server::with_manifest(plugin, manifest);
    let out: Out = Arc::new(Mutex::new(std::io::stdout()));
    let mut running: Vec<JoinHandle<()>> = Vec::new();
    let cancels: Running = Arc::default();
    let mut input = std::io::stdin().lock();
    let mut raw = Vec::new();
    loop {
        raw.clear();
        if input.read_until(b'\n', &mut raw)? == 0 {
            break;
        }
        // a bad byte is one bad line, not the end of the plugin. It's refused,
        // not repaired: a repaired search is a different search.
        let lossy = String::from_utf8_lossy(&raw);
        let line = lossy.trim_end_matches(['\n', '\r']);
        if line.is_empty() {
            continue;
        }
        let parsed = serde_json::from_str::<Value>(line).ok();
        let Some(message) = parsed
            .as_ref()
            .filter(|m| is_message(m) && matches!(lossy, std::borrow::Cow::Borrowed(_)))
            .cloned()
        else {
            if let Some(reply) = parsed.as_ref().and_then(invalid) {
                write(&out, &reply);
            }
            continue;
        };
        running.retain(|t| !t.is_finished());
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        match route(&message) {
            Route::Tool { name, args } if core.provisioned() => {
                let params = message.get("params").cloned().unwrap_or(Value::Null);
                let tool = server
                    .plugin()
                    .tools()
                    .into_iter()
                    .find(|t| t.name == name)
                    .expect("routed tools exist");
                let caller = Caller::of(&params);
                if let Err(why) = server.authorize(&tool, &caller) {
                    write(
                        &out,
                        &json!({"jsonrpc": "2.0", "id": id, "result": buwiz_plugin::fail("permission_denied", &why)}),
                    );
                    continue;
                }
                if !audience_allows(&name, caller.origin) {
                    write(
                        &out,
                        &json!({"jsonrpc": "2.0", "id": id, "result": buwiz_plugin::fail("permission_denied", "Only a person can do this.")}),
                    );
                    continue;
                }
                // what never leaves this Mac answers here, whatever is running:
                // a full cap can't stop a screen knowing where it stands
                if tools::is_local(&name) {
                    write(
                        &out,
                        &json!({"jsonrpc": "2.0", "id": id, "result": answer(|| core.run(&name, &args))}),
                    );
                    continue;
                }
                if running.len() >= MAX_RUNNING {
                    write(
                        &out,
                        &json!({"jsonrpc": "2.0", "id": id, "result": buwiz_plugin::fail("rate_limited", "Busy with other requests. Try again in a moment.")}),
                    );
                    continue;
                }
                let cancelled = Arc::new(AtomicBool::new(false));
                let key = id.to_string();
                cancels
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(key.clone(), cancelled.clone());
                let (core, out, cancels) = (core.clone(), out.clone(), cancels.clone());
                // a routine's turn has no one to show a card to
                let chat = caller.origin == Origin::Agent && caller.routine.is_none();
                let model = matches!(caller.origin, Origin::Agent | Origin::Routine);
                running.push(std::thread::spawn(move || {
                    let result = answer(|| {
                        if chat {
                            core.run_for_chat(&name, &args)
                        } else if model {
                            core.run_for_model(&name, &args)
                        } else {
                            core.run(&name, &args)
                        }
                    });
                    cancels
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&key);
                    if !cancelled.load(Ordering::SeqCst) {
                        write(&out, &json!({"jsonrpc": "2.0", "id": id, "result": result}));
                    }
                }));
            }
            Route::Initialize => {
                if let Some(reply) = server.handle(&message) {
                    write(&out, &negotiated(reply, &message));
                }
            }
            Route::Cancelled => {
                let request = message
                    .pointer("/params/requestId")
                    .map(Value::to_string)
                    .unwrap_or_default();
                if let Some(flag) = cancels
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&request)
                {
                    flag.store(true, Ordering::SeqCst);
                }
            }
            Route::List => {
                if let Some(reply) = server.handle(&message) {
                    write(&out, &with_audience(reply));
                }
            }
            Route::Account(name) if core.provisioned() => {
                let empty = json!({});
                let args = message.pointer("/params/arguments").unwrap_or(&empty);
                let result = core.account(&name, args);
                write(&out, &json!({"jsonrpc": "2.0", "id": id, "result": result}));
            }
            Route::Provision => {
                let reply = server.handle(&message);
                if !is_error(reply.as_ref())
                    && let Some(seed) = message
                        .pointer("/params/arguments/key/seed")
                        .and_then(Value::as_str)
                {
                    core.unlock(seed);
                }
                if let Some(reply) = reply {
                    write(&out, &reply);
                }
            }
            Route::Server | Route::Tool { .. } | Route::Account(_) => {
                if let Some(reply) = server.handle(&message) {
                    write(&out, &reply);
                }
            }
        }
    }
    for t in running {
        let _ = t.join();
    }
    // the process's own teardown frees what the store's crypto uses, so the
    // sync thread ends first; a call stuck on the network doesn't hold it up
    core.sync.stop();
    let deadline = Instant::now() + STOP_WAIT;
    while !syncing.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_write_is_the_models_or_a_routines() {
        // a draft it fills sends nothing: Send is the person's
        let writes: Vec<&str> = tools::defs()
            .iter()
            .filter(|d| !d.read_only && d.name != tools::MAIL_DRAFT_SAVE)
            .map(|d| d.name)
            .collect();
        for name in [
            "gws_mail_mark",
            "gws_mail_label",
            "gws_mail_archive",
            "gws_mail_trash",
            "gws_mail_undo",
            "gws_mail_draft_delete",
            "gws_mail_send_arm",
            "gws_mail_send",
        ] {
            assert!(writes.contains(&name), "{name}");
        }
        for name in writes {
            assert!(audience_allows(name, Origin::App), "{name}");
            assert!(!audience_allows(name, Origin::Agent), "{name}");
            assert!(!audience_allows(name, Origin::Routine), "{name}");
            assert!(!audience_allows(name, Origin::Unknown), "{name}");
        }
    }

    #[test]
    fn the_model_reaches_only_a_tool_that_names_it() {
        assert!(audience_allows(tools::READ_MAIL, Origin::Agent));
        assert!(audience_allows(tools::READ_MAIL, Origin::Routine));
        assert!(
            !audience_allows(tools::STATUS, Origin::Agent),
            "the address on it is the screen's"
        );
        assert!(!audience_allows(tools::STATUS, Origin::Routine));
        assert!(audience_allows(tools::STATUS, Origin::App));
        assert!(audience_allows(tools::RESOLVE, Origin::Resolve));
        assert!(
            !audience_allows(tools::STATUS, Origin::Resolve),
            "a resolve reaches the resolve tool"
        );
    }

    #[test]
    fn a_caller_it_cant_name_reaches_nothing() {
        for def in tools::defs() {
            assert!(!audience_allows(def.name, Origin::Unknown), "{}", def.name);
        }
    }

    #[test]
    fn initialize_answers_the_revision_asked_for_if_it_is_ours() {
        let reply = || json!({"result": {"protocolVersion": "2025-11-25", "capabilities": {}}});
        let asked = |v: &str| json!({"params": {"protocolVersion": v}});
        assert_eq!(
            negotiated(reply(), &asked("2024-11-05"))["result"]["protocolVersion"],
            "2024-11-05"
        );
        assert_eq!(
            negotiated(reply(), &asked("1999-01-01"))["result"]["protocolVersion"],
            "2025-11-25",
            "else the newest"
        );
        assert_eq!(
            negotiated(reply(), &json!({}))["result"]["protocolVersion"],
            "2025-11-25"
        );
    }

    #[test]
    fn only_a_line_with_an_id_gets_an_error() {
        assert_eq!(
            invalid(&json!({"jsonrpc": "1.0", "id": 7, "method": "ping"})).unwrap()["id"],
            7
        );
        assert_eq!(
            invalid(&json!({"id": "a"})).unwrap()["error"]["code"],
            buwiz_plugin::INVALID_REQUEST
        );
        assert_eq!(invalid(&json!({"method": "ping"})), None);
        assert_eq!(invalid(&json!([1])), None);
        assert_eq!(invalid(&json!({"id": null})), None);
    }

    #[test]
    fn only_a_json_rpc_message_counts() {
        assert!(is_message(
            &json!({"jsonrpc": "2.0", "id": 1, "method": "ping"})
        ));
        assert!(is_message(
            &json!({"jsonrpc": "2.0", "method": "x", "params": {}})
        ));
        for bad in [
            json!([1]),
            json!({"id": 1, "method": "ping"}),
            json!({"jsonrpc": "1.0", "id": 1, "method": "ping"}),
            json!({"jsonrpc": "2.0", "id": 1}),
            json!({"jsonrpc": "2.0", "id": 1, "method": "ping", "params": [1]}),
        ] {
            assert!(!is_message(&bad), "{bad}");
        }
    }

    #[test]
    fn a_panic_is_an_answer() {
        let r = answer(|| panic!("a bug"));
        assert_eq!(
            (r["isError"].clone(), r["structuredContent"]["code"].clone()),
            (json!(true), json!("internal"))
        );
        assert!(
            !r.to_string().contains("a bug"),
            "what panicked stays out of the answer"
        );
        assert_eq!(answer(|| json!({"ok": true})), json!({"ok": true}));
    }

    #[test]
    fn a_call_with_no_id_or_bad_arguments_goes_to_the_sdk() {
        let call = |extra: Value| {
            let mut m = json!({"jsonrpc": "2.0", "method": "tools/call", "params": {"name": tools::STATUS}});
            for (k, v) in extra.as_object().unwrap() {
                if k == "id" {
                    m["id"] = v.clone()
                } else {
                    m["params"][k] = v.clone()
                }
            }
            m
        };
        assert!(
            matches!(route(&call(json!({}))), Route::Server),
            "no id: a notification"
        );
        assert!(
            matches!(
                route(&call(json!({"id": 1, "arguments": [1]}))),
                Route::Server
            ),
            "not an object"
        );
        assert!(matches!(
            route(&call(json!({"id": 1, "arguments": null}))),
            Route::Tool { .. }
        ));
        assert!(matches!(route(&call(json!({"id": 1}))), Route::Tool { .. }));
    }
}
