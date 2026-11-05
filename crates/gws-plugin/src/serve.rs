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

use buwiz_plugin::manifest::MANIFEST_ENV;
use buwiz_plugin::{Caller, Origin, Plugin as _, Server};
use serde_json::{Value, json};

use crate::{Gws, tools};

/// Adds each tool's audience and full annotations to a `tools/list` reply:
/// the SDK's `Tool` carries `readOnlyHint` alone.
fn with_audience(mut reply: Value) -> Value {
    let defs = tools::defs();
    for tool in reply.pointer_mut("/result/tools").and_then(Value::as_array_mut).into_iter().flatten() {
        let def = tool.get("name").and_then(Value::as_str).and_then(|name| defs.iter().find(|d| d.name == name));
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
    if method == Some("tools/list") {
        return Route::List;
    }
    // a call with no id is a notification: the SDK answers nothing to it
    if method != Some("tools/call") || message.get("id").is_none() {
        return Route::Server;
    }
    let params = message.get("params");
    match params.and_then(|p| p.get("name")).and_then(Value::as_str) {
        Some(name @ ("buwiz.account.set" | "buwiz.account.clear")) => Route::Account(name.to_owned()),
        Some("buwiz.provision") => Route::Provision,
        Some(name) if tools::has(name) => match params.and_then(|p| p.get("arguments")) {
            None | Some(Value::Null) => Route::Tool { name: name.to_owned(), args: json!({}) },
            Some(args) if args.is_object() => Route::Tool { name: name.to_owned(), args: args.clone() },
            // the SDK's refusal of arguments that aren't an object
            Some(_) => Route::Server,
        },
        _ => Route::Server,
    }
}

fn is_error(reply: Option<&Value>) -> bool {
    reply.is_none_or(|r| r.get("error").is_some() || r.pointer("/result/isError") == Some(&Value::Bool(true)))
}

/// The plugin's own check of a tool's audience, beside the Hub's: a screen, a
/// resolve or a routine may reach what they're granted, but the model reaches
/// only a tool that names `agent`. The SDK doesn't know the audience.
fn audience_allows(name: &str, origin: Origin) -> bool {
    origin != Origin::Agent || tools::defs().iter().any(|d| d.name == name && d.audience.contains(&"agent"))
}

type Out = Arc<Mutex<std::io::Stdout>>;

fn write(out: &Out, reply: &Value) {
    let mut line = reply.to_string();
    line.push('\n');
    let mut out = out.lock().unwrap_or_else(|e| e.into_inner());
    let _ = out.write_all(line.as_bytes());
    let _ = out.flush();
}

/// The manifest must be the signed one the Bot verified. Without this the
/// SDK would serve the unsigned one and check callers against it.
fn require_manifest() -> Result<(), String> {
    let path = std::env::var_os(MANIFEST_ENV).filter(|p| !p.is_empty()).ok_or(format!("{MANIFEST_ENV} is not set, so there is no signed manifest to serve"))?;
    std::fs::read(&path).map(drop).map_err(|e| format!("{MANIFEST_ENV}: {e}"))
}

/// Runs in-flight requests can be cancelled by id.
type Running = Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>;

/// Serves stdin to stdout until stdin closes, then waits for what's running.
pub fn serve(plugin: Gws) -> Result<(), String> {
    require_manifest()?;
    run(plugin).map_err(|e| e.to_string())
}

fn run(plugin: Gws) -> std::io::Result<()> {
    let core = plugin.core.clone();
    let mut server = Server::new(plugin);
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
        // a bad byte is one bad line, not the end of the plugin
        let line = String::from_utf8_lossy(&raw);
        let line = line.trim_end_matches(['\n', '\r']);
        if line.is_empty() {
            continue;
        }
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            // the SDK's answer to what isn't JSON
            if let Some(reply) = server.handle_line(line) {
                let mut out = out.lock().unwrap_or_else(|e| e.into_inner());
                out.write_all(reply.as_bytes())?;
                out.write_all(b"\n")?;
                out.flush()?;
            }
            continue;
        };
        running.retain(|t| !t.is_finished());
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        match route(&message) {
            Route::Tool { name, args } if core.provisioned() => {
                let params = message.get("params").cloned().unwrap_or(Value::Null);
                let tool = server.plugin().tools().into_iter().find(|t| t.name == name).expect("routed tools exist");
                let caller = Caller::of(&params);
                if let Err(why) = server.authorize(&tool, &caller) {
                    write(&out, &json!({"jsonrpc": "2.0", "id": id, "result": buwiz_plugin::fail("permission_denied", &why)}));
                    continue;
                }
                if !audience_allows(&name, caller.origin) {
                    write(&out, &json!({"jsonrpc": "2.0", "id": id, "result": buwiz_plugin::fail("permission_denied", "Only a person can do this.")}));
                    continue;
                }
                let cancelled = Arc::new(AtomicBool::new(false));
                let key = id.to_string();
                cancels.lock().unwrap_or_else(|e| e.into_inner()).insert(key.clone(), cancelled.clone());
                let (core, out, cancels) = (core.clone(), out.clone(), cancels.clone());
                running.push(std::thread::spawn(move || {
                    let result = core.run(&name, &args);
                    cancels.lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
                    if !cancelled.load(Ordering::SeqCst) {
                        write(&out, &json!({"jsonrpc": "2.0", "id": id, "result": result}));
                    }
                }));
            }
            Route::Cancelled => {
                let request = message.pointer("/params/requestId").map(Value::to_string).unwrap_or_default();
                if let Some(flag) = cancels.lock().unwrap_or_else(|e| e.into_inner()).get(&request) {
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
                let result = if name == "buwiz.account.set" { core.accounts.set(args) } else { core.accounts.clear(args) };
                write(&out, &json!({"jsonrpc": "2.0", "id": id, "result": result}));
            }
            Route::Provision => {
                let reply = server.handle(&message);
                if !is_error(reply.as_ref())
                    && let Some(seed) = message.pointer("/params/arguments/key/seed").and_then(Value::as_str)
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_model_reaches_only_a_tool_that_names_it() {
        assert!(audience_allows(tools::READ_MAIL, Origin::Agent));
        assert!(!audience_allows(tools::STATUS, Origin::Agent), "the address on it is the screen's");
        assert!(audience_allows(tools::STATUS, Origin::App));
        assert!(audience_allows(tools::RESOLVE, Origin::Resolve));
    }

    #[test]
    fn a_call_with_no_id_or_bad_arguments_goes_to_the_sdk() {
        let call = |extra: Value| {
            let mut m = json!({"jsonrpc": "2.0", "method": "tools/call", "params": {"name": tools::STATUS}});
            for (k, v) in extra.as_object().unwrap() {
                if k == "id" { m["id"] = v.clone() } else { m["params"][k] = v.clone() }
            }
            m
        };
        assert!(matches!(route(&call(json!({}))), Route::Server), "no id: a notification");
        assert!(matches!(route(&call(json!({"id": 1, "arguments": [1]}))), Route::Server), "not an object");
        assert!(matches!(route(&call(json!({"id": 1, "arguments": null}))), Route::Tool { .. }));
        assert!(matches!(route(&call(json!({"id": 1}))), Route::Tool { .. }));
    }
}
