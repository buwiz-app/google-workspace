//! The user's Google Workspace MCP server (workspace-mcp), started on the
//! first mail call from `GWS_UPSTREAM` and kept while it lives, over the
//! stdio client rox-broker uses (`rox-mcp`). Its stderr is ours.

use std::sync::Arc;
use std::time::Duration;

use rox_mcp::{Conn, Hooks, Spec, REQUEST_TIMEOUT};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::{config, js};

const START_TIMEOUT: Duration = Duration::from_secs(30);
const CALL_TIMEOUT: Duration = Duration::from_secs(60);

struct Stderr;

impl Hooks for Stderr {
    fn stderr(&self, chunk: &str) {
        eprint!("{chunk}");
    }
    fn list_changed(&self) {}
    fn closed(&self) {}
}

struct Live {
    conn: Arc<Conn>,
    tools: Vec<Value>,
}

#[derive(Default)]
pub struct Upstream {
    live: Mutex<Option<Arc<Live>>>,
}

const DID_NOT_ANSWER: &str = "the Google Workspace server did not answer within 30 s";

impl Upstream {
    async fn connect(&self) -> Result<Arc<Live>, String> {
        let mut live = self.live.lock().await;
        if let Some(l) = live.as_ref().filter(|l| !l.conn.is_closed()) {
            return Ok(l.clone());
        }
        let up = config::upstream()?;
        let spec = Spec { command: up.command, args: up.args, env: up.env, inherit_all: false, client: "rox-gws" };
        let could_not = |e: String| format!("the Google Workspace server could not start: {}", js::slice_utf16(&e, 200));
        let timed = |e: rox_mcp::RpcError| if e.code == REQUEST_TIMEOUT { DID_NOT_ANSWER.to_owned() } else { e.message };
        let conn = Conn::connect_within(&spec, Arc::new(Stderr), START_TIMEOUT).await.map_err(|e| could_not(timed(e)))?;
        let tools = match conn.request("tools/list", json!({}), START_TIMEOUT).await {
            Ok(r) => r.get("tools").and_then(Value::as_array).cloned().unwrap_or_default(),
            Err(e) => {
                conn.close().await;
                return Err(could_not(timed(e)));
            }
        };
        let l = Arc::new(Live { conn, tools });
        *live = Some(l.clone());
        Ok(l)
    }

    /// The upstream tool's text; the account argument when its schema
    /// takes one.
    pub async fn call(&self, name: &str, args: Value) -> Result<String, String> {
        let up = self.connect().await?;
        let tool = up.tools.iter().find(|t| t.get("name").and_then(Value::as_str) == Some(name)).ok_or_else(|| format!("the Google Workspace server has no {name} tool"))?;
        let schema = tool.get("inputSchema");
        let takes_email = js::truthy(schema.and_then(|s| s.get("properties")).and_then(|p| p.get("user_google_email")));
        let email = config::user_email();
        let required = match schema.and_then(|s| s.get("required")) {
            Some(Value::Array(r)) => r.iter().any(|x| x == "user_google_email"),
            Some(Value::String(r)) => r.contains("user_google_email"),
            _ => false,
        };
        if takes_email && email.is_empty() && required {
            return Err("Google Workspace is not set up: the plugin's GWS_USER_EMAIL (the Google address to read) is empty".to_owned());
        }
        let mut full = args;
        if takes_email && !email.is_empty() {
            full["user_google_email"] = Value::String(email);
        }
        let r = up.conn.call_tool(name, &full, None, CALL_TIMEOUT).await.map_err(|e| format!("the Google Workspace server failed: {}", js::slice_utf16(&e.message, 200)))?;
        let text = r
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|c| c.get("type").and_then(Value::as_str) == Some("text"))
            .map(|c| js::string(c.get("text")))
            .collect::<Vec<_>>()
            .join("\n");
        if js::truthy(r.get("isError")) {
            let reason = js::slice_utf16(&text, 300);
            return Err(format!("the Google Workspace server refused {name}: {}", if reason.is_empty() { "no reason given" } else { &reason }));
        }
        Ok(text)
    }
}
