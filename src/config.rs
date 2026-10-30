//! What the install record's env gives the plugin, read as server.mjs read
//! it: `GWS_MANIFEST` (the signed manifest it serves), `GWS_UPSTREAM` (the
//! user's Google Workspace MCP server as compact JSON `{command, args,
//! env}`) and `GWS_USER_EMAIL` (the Google address to read). For example:
//!
//! ```text
//! GWS_UPSTREAM={"command":"uvx","args":["workspace-mcp==2.2.1","--single-user","--tools","gmail"],"env":{"GOOGLE_OAUTH_CLIENT_ID":"…","GOOGLE_OAUTH_CLIENT_SECRET":"…"}}
//! ```
//!
//! The upstream is pinned (`workspace-mcp==2.2.1`): the mail tools parse its
//! text answers, so its format must not drift under them.

use std::path::PathBuf;

use serde_json::{Map, Value};

use crate::js;

/// The signed manifest: `GWS_MANIFEST`, else `rox-gws.manifest.json`
/// beside this binary (where the app bundle puts the signed copy).
pub fn manifest_path() -> PathBuf {
    match std::env::var("GWS_MANIFEST") {
        Ok(p) if !p.is_empty() => PathBuf::from(p),
        _ => std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join("rox-gws.manifest.json"))).unwrap_or_else(|| PathBuf::from("rox-gws.manifest.json")),
    }
}

/// The manifest's text, read on every use; a failure reads as Node's
/// `readFileSync` error.
pub fn manifest_text() -> Result<String, String> {
    let path = manifest_path();
    let shown = path.display();
    match std::fs::read(&path) {
        Ok(bytes) => Ok(String::from_utf8_lossy(&bytes).into_owned()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(format!("ENOENT: no such file or directory, open '{shown}'")),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Err(format!("EACCES: permission denied, open '{shown}'")),
        Err(e) if e.kind() == std::io::ErrorKind::IsADirectory => Err("EISDIR: illegal operation on a directory, read".to_owned()),
        Err(e) => Err(format!("{e}, open '{shown}'")),
    }
}

/// `GWS_USER_EMAIL`, trimmed.
pub fn user_email() -> String {
    js::trim(&std::env::var("GWS_USER_EMAIL").unwrap_or_default()).to_owned()
}

/// The upstream server to start.
#[derive(Debug, Clone, PartialEq)]
pub struct Upstream {
    pub command: String,
    pub args: Vec<String>,
    /// In the order given.
    pub env: Vec<(String, String)>,
}

/// `GWS_UPSTREAM` parsed, or the plain message for a missing one.
pub fn upstream() -> Result<Upstream, String> {
    upstream_of(&std::env::var("GWS_UPSTREAM").unwrap_or_default())
}

pub fn upstream_of(text: &str) -> Result<Upstream, String> {
    let spec = serde_json::from_str::<Value>(text).ok();
    let Some(Value::Object(spec)) = spec else { return Err(NOT_SET_UP.to_owned()) };
    let Some(command) = spec.get("command").and_then(Value::as_str).filter(|c| !c.is_empty()) else { return Err(NOT_SET_UP.to_owned()) };
    let args = match spec.get("args") {
        Some(Value::Array(a)) => a.iter().map(|v| js::string(Some(v))).collect(),
        _ => Vec::new(),
    };
    let env = match spec.get("env") {
        Some(Value::Object(e)) => own_callback_port(e.clone()),
        _ => Map::new(),
    };
    // spawn() writes each value with String(); undefined ones are skipped
    let env = env.iter().map(|(k, v)| (k.clone(), js::string(Some(v)))).collect();
    Ok(Upstream { command: command.to_owned(), args, env })
}

const NOT_SET_UP: &str = "Google Workspace is not set up: the plugin's GWS_UPSTREAM does not name the Google Workspace MCP server to use";

// GWS_UPSTREAM is a copy of v0's workspace-mcp settings, and v0 runs its own
// copy. workspace-mcp signs in on WORKSPACE_MCP_PORT (else PORT, else 8000)
// or one of the next WORKSPACE_MCP_PORT_FALLBACK_COUNT (default 4) ports, so
// this copy takes the ports after that range and two sign-ins never clash. A
// desktop OAuth client accepts a loopback redirect on any port; a pinned
// GOOGLE_OAUTH_REDIRECT_URI is left alone.
fn own_callback_port(mut env: Map<String, Value>) -> Map<String, Value> {
    if js::truthy(env.get("GOOGLE_OAUTH_REDIRECT_URI")) {
        return env;
    }
    let set = |v: Option<&Value>| v.filter(|v| !v.is_null()).cloned();
    let port = js::to_number(Some(&set(env.get("WORKSPACE_MCP_PORT")).or_else(|| set(env.get("PORT"))).unwrap_or(Value::from(8000))));
    let fallback = js::to_number(Some(&set(env.get("WORKSPACE_MCP_PORT_FALLBACK_COUNT")).unwrap_or(Value::from(4))));
    let integer = |n: f64| n.is_finite() && n.fract() == 0.0;
    if !integer(port) || !integer(fallback) {
        return env;
    }
    env.shift_remove("PORT");
    env.insert("WORKSPACE_MCP_PORT".to_owned(), Value::String(js::number(port + 1.0 + fallback.max(0.0))));
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    fn port_of(env: &str) -> Option<String> {
        let u = upstream_of(&format!(r#"{{"command":"x","env":{env}}}"#)).unwrap();
        u.env.into_iter().find(|(k, _)| k == "WORKSPACE_MCP_PORT").map(|(_, v)| v)
    }

    #[test]
    fn the_upstream_signs_in_past_v0s_ports() {
        assert_eq!(port_of(r#"{"WORKSPACE_MCP_PORT":"8791"}"#).as_deref(), Some("8796"));
        assert_eq!(port_of(r#"{"WORKSPACE_MCP_PORT":"8791","WORKSPACE_MCP_PORT_FALLBACK_COUNT":"2"}"#).as_deref(), Some("8794"));
        assert_eq!(port_of("{}").as_deref(), Some("8005"));
        assert_eq!(port_of(r#"{"PORT":9000}"#).as_deref(), Some("9005"));
        assert_eq!(port_of(r#"{"WORKSPACE_MCP_PORT":"8791","GOOGLE_OAUTH_REDIRECT_URI":"http://localhost:8791/oauth2callback"}"#).as_deref(), Some("8791"));
        assert_eq!(port_of(r#"{"WORKSPACE_MCP_PORT":"x"}"#).as_deref(), Some("x"));
        let u = upstream_of(r#"{"command":"uvx","args":["workspace-mcp==2.2.1",3],"env":{"PORT":"8000","A":1}}"#).unwrap();
        assert_eq!(u.args, ["workspace-mcp==2.2.1", "3"]);
        assert_eq!(u.env, [("A".to_owned(), "1".to_owned()), ("WORKSPACE_MCP_PORT".to_owned(), "8005".to_owned())]);
    }

    #[test]
    fn a_missing_upstream_is_a_plain_message() {
        for text in ["", "{nope", "[]", r#"{"args":[]}"#, r#"{"command":""}"#, r#"{"command":5}"#] {
            assert_eq!(upstream_of(text).unwrap_err(), NOT_SET_UP, "{text}");
        }
    }
}
