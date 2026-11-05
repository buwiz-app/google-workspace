//! The MCP loop. `buwiz_plugin::Server` answers everything but a tool of
//! ours, whose caller it still checks; the tool then runs on its own thread,
//! so a slow Google call holds up nothing. The reply is written whenever it
//! is ready: JSON-RPC ids pair them up. Token pushes are copied to
//! [`crate::Core`], because the tools' threads and the sync can't reach the
//! server's own copy.

use std::io::{BufRead as _, Write as _};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use buwiz_plugin::{Caller, Plugin as _, Server};
use serde_json::{Value, json};

use crate::{Core, Gws, tools};

/// Adds each tool's `_meta` to a `tools/list` reply.
fn with_audience(mut reply: Value) -> Value {
    let defs = tools::defs();
    for tool in reply.pointer_mut("/result/tools").and_then(Value::as_array_mut).into_iter().flatten() {
        let def = tool.get("name").and_then(Value::as_str).and_then(|name| defs.iter().find(|d| d.name == name));
        if let Some(def) = def {
            tool["_meta"] = json!({"buwiz/audience": def.audience});
        }
    }
    reply
}

/// A message for [`Server`], or a tool of ours to run.
enum Route {
    Server,
    List,
    Account,
    Provision,
    Tool { name: String, args: Value },
}

fn route(message: &Value) -> Route {
    let method = message.get("method").and_then(Value::as_str);
    if method == Some("tools/list") {
        return Route::List;
    }
    if method != Some("tools/call") {
        return Route::Server;
    }
    let params = message.get("params");
    match params.and_then(|p| p.get("name")).and_then(Value::as_str) {
        Some("buwiz.account.set" | "buwiz.account.clear") => Route::Account,
        Some("buwiz.provision") => Route::Provision,
        Some(name) if tools::has(name) => {
            let args = params.and_then(|p| p.get("arguments")).filter(|a| !a.is_null()).cloned().unwrap_or_else(|| json!({}));
            Route::Tool { name: name.to_owned(), args }
        }
        _ => Route::Server,
    }
}

fn is_error(reply: Option<&Value>) -> bool {
    reply.is_none_or(|r| r.get("error").is_some() || r.pointer("/result/isError") == Some(&Value::Bool(true)))
}

type Out = Arc<Mutex<std::io::Stdout>>;

fn write(out: &Out, reply: &Value) {
    let mut line = reply.to_string();
    line.push('\n');
    let mut out = out.lock().unwrap_or_else(|e| e.into_inner());
    let _ = out.write_all(line.as_bytes());
    let _ = out.flush();
}

/// Mirrors an accepted push into `core`.
fn mirror(core: &Core, message: &Value) {
    let Some(params) = message.get("params") else { return };
    let empty = json!({});
    let args = params.get("arguments").unwrap_or(&empty);
    match params.get("name").and_then(Value::as_str) {
        Some("buwiz.account.set") => drop(core.accounts.set(args)),
        Some("buwiz.account.clear") => drop(core.accounts.clear(args)),
        _ => {}
    }
}

/// Serves stdin to stdout until stdin closes, then waits for what's running.
pub fn serve(plugin: Gws) -> std::io::Result<()> {
    let core = plugin.core.clone();
    let mut server = Server::new(plugin);
    let out: Out = Arc::new(Mutex::new(std::io::stdout()));
    let mut provisioned = false;
    let mut running: Vec<JoinHandle<()>> = Vec::new();
    for line in std::io::stdin().lock().lines() {
        let line = line?;
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            // the SDK's answer to what isn't JSON
            if let Some(reply) = server.handle_line(&line) {
                let mut out = out.lock().unwrap_or_else(|e| e.into_inner());
                out.write_all(reply.as_bytes())?;
                out.write_all(b"\n")?;
                out.flush()?;
            }
            continue;
        };
        running.retain(|t| !t.is_finished());
        match route(&message) {
            Route::Tool { name, args } if provisioned => {
                let params = message.get("params").cloned().unwrap_or(Value::Null);
                let id = message.get("id").cloned().unwrap_or(Value::Null);
                let tool = server.plugin().tools().into_iter().find(|t| t.name == name).expect("routed tools exist");
                if let Err(why) = server.authorize(&tool, &Caller::of(&params)) {
                    write(&out, &json!({"jsonrpc": "2.0", "id": id, "result": buwiz_plugin::fail("permission_denied", &why)}));
                    continue;
                }
                let (core, out) = (core.clone(), out.clone());
                running.push(std::thread::spawn(move || {
                    let result = core.run(&name, &args);
                    write(&out, &json!({"jsonrpc": "2.0", "id": id, "result": result}));
                }));
            }
            Route::List => {
                if let Some(reply) = server.handle(&message) {
                    write(&out, &with_audience(reply));
                }
            }
            Route::Account => {
                let reply = server.handle(&message);
                if !is_error(reply.as_ref()) {
                    mirror(&core, &message);
                }
                if let Some(reply) = reply {
                    write(&out, &reply);
                }
            }
            Route::Provision => {
                let reply = server.handle(&message);
                provisioned = provisioned || !is_error(reply.as_ref());
                if let Some(reply) = reply {
                    write(&out, &reply);
                }
            }
            Route::Server | Route::Tool { .. } => {
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
