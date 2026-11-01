//! An MCP client over stdio, as the broker's Node SDK (1.32) runs one: the
//! child gets HOME, LOGNAME, PATH, SHELL, TERM and USER from its parent's
//! environment plus its own (PATH with the usual tool folders added, see
//! [`child_env`]), newline-delimited JSON-RPC on stdin and
//! stdout, stderr handed to the owner, `initialize` then
//! `notifications/initialized`. A request past its timeout is cancelled
//! (`notifications/cancelled`) and fails -32001; a line that is not JSON-RPC
//! closes the connection, as the SDK's error handler does in plugins.mjs.
//! Closing ends stdin, then SIGTERM after 2 s, then SIGKILL after 2 more.
//!
//! Results come back as the plugin wrote them. Node's side of the line
//! runs them through the SDK's schemas, so callers see what they saw before.
//!
//! Copied from the Bot's `rox-mcp` crate (buwiz-bot/buwiz-bot `broker/rox-mcp`),
//! which its broker hosts plugins with; here it reaches the upstream.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{oneshot, Mutex as AsyncMutex, Notify};

pub const PROTOCOL_VERSION: &str = "2025-11-25";
const SUPPORTED: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05", "2024-10-07"];
/// The SDK's default request timeout.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
const INHERITED: &[&str] = &["HOME", "LOGNAME", "PATH", "SHELL", "TERM", "USER"];

pub const REQUEST_TIMEOUT: i64 = -32001;
pub const CONNECTION_CLOSED: i64 = -32000;
pub const METHOD_NOT_FOUND: i64 = -32601;

/// What a plugin is started with.
#[derive(Debug, Clone, PartialEq)]
pub struct Spec {
    pub command: String,
    pub args: Vec<String>,
    /// In the order given.
    pub env: Vec<(String, String)>,
    /// The environment under `env`: the SDK's six inherited names (v1
    /// plugins), or all of this process's (v0 MCP servers, as mcp.mjs gives
    /// them Node's).
    pub inherit_all: bool,
    /// `clientInfo.name` in the handshake.
    pub client: &'static str,
}

/// A failed request: a JSON-RPC error, a timeout, a closed connection, or
/// a process that would not start (`code` 0). `message` reads as the
/// SDK's `McpError` (`MCP error <code>: <message>`) or Node's spawn error.
#[derive(Debug, Clone, PartialEq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    /// The request never reached the server (the connection was already
    /// closed): safe to send again on a new connection.
    pub unsent: bool,
}

impl RpcError {
    fn mcp(code: i64, message: &str) -> Self {
        Self { code, message: format!("MCP error {code}: {message}"), unsent: false }
    }

    fn unsent() -> Self {
        Self { unsent: true, ..Self::mcp(CONNECTION_CLOSED, "Connection closed") }
    }

    /// An error of this process's own (a spawn that failed), not the server's.
    pub fn local(message: impl Into<String>) -> Self {
        Self { code: 0, message: message.into(), unsent: true }
    }
}

/// What the connection tells its owner.
pub trait Hooks: Send + Sync {
    /// A chunk of the child's stderr.
    fn stderr(&self, chunk: &str);
    /// `notifications/resources/list_changed`.
    fn list_changed(&self);
    /// The child is gone (after `close` too).
    fn closed(&self);
}

type Pending = oneshot::Sender<Result<Value, RpcError>>;

pub struct Conn {
    pub pid: Option<u32>,
    stdin: AsyncMutex<Option<ChildStdin>>,
    child: AsyncMutex<Option<Child>>,
    pending: Mutex<HashMap<u64, Pending>>,
    next_id: AtomicU64,
    closed: AtomicBool,
    exited: Notify,
    /// The owner is stopping it: its close is not a crash.
    pub stopping: AtomicBool,
    instructions: Mutex<String>,
    capabilities: Mutex<Value>,
}

/// mcp.mjs's `toolPath()`: an app started from Finder gets launchd's bare
/// PATH, without the folders npx, uvx and bunx usually live in.
pub fn tool_path(path: &str, home: &str) -> String {
    let mut have: Vec<String> = path.split(':').filter(|p| !p.is_empty()).map(str::to_owned).collect();
    for extra in [format!("{home}/.local/bin"), format!("{home}/.cargo/bin"), format!("{home}/.bun/bin"), "/opt/homebrew/bin".into(), "/usr/local/bin".into()] {
        if !have.contains(&extra) {
            have.push(extra);
        }
    }
    have.join(":")
}

/// The environment the child starts with.
pub fn child_env(spec: &Spec) -> Vec<(String, String)> {
    let base: Vec<(String, String)> =
        if spec.inherit_all { std::env::vars().collect() } else { INHERITED.iter().filter_map(|k| std::env::var(k).ok().filter(|v| !v.starts_with("()")).map(|v| (k.to_string(), v))).collect() };
    child_env_from(spec, base)
}

/// [`child_env`] over `base`, the inherited part. A v1 plugin gets
/// [`tool_path`]'s PATH as a v0 server does (whose spec carries it): a
/// Dock-launched Rox's plugin finds `uvx` too. The spec's own env wins.
pub fn child_env_from(spec: &Spec, mut env: Vec<(String, String)>) -> Vec<(String, String)> {
    if !spec.inherit_all {
        let var = |k: &str| env.iter().find(|(e, _)| e == k).map(|(_, v)| v.clone()).unwrap_or_default();
        let path = tool_path(&var("PATH"), &var("HOME"));
        env.retain(|(e, _)| e != "PATH");
        env.push(("PATH".to_owned(), path));
    }
    for (k, v) in &spec.env {
        env.retain(|(e, _)| e != k);
        env.push((k.clone(), v.clone()));
    }
    env
}

/// Node's text for a spawn that failed (`spawn <cmd> ENOENT`).
fn spawn_error(command: &str, e: &std::io::Error) -> String {
    let code = match e.raw_os_error() {
        Some(libc::ENOENT) => "ENOENT",
        Some(libc::EACCES) => "EACCES",
        Some(libc::ENOTDIR) => "ENOTDIR",
        _ => return format!("spawn {command}: {e}"),
    };
    format!("spawn {command} {code}")
}

impl Conn {
    /// Starts the child and runs the handshake.
    pub async fn connect(spec: &Spec, hooks: Arc<dyn Hooks>) -> Result<Arc<Conn>, RpcError> {
        Self::connect_within(spec, hooks, DEFAULT_TIMEOUT).await
    }

    /// [`Conn::connect`] with `initialize` answered within `timeout`.
    pub async fn connect_within(spec: &Spec, hooks: Arc<dyn Hooks>, timeout: Duration) -> Result<Arc<Conn>, RpcError> {
        let mut cmd = tokio::process::Command::new(&spec.command);
        cmd.args(&spec.args).env_clear().envs(child_env(spec)).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| RpcError::local(spawn_error(&spec.command, &e)))?;
        let (stdout, mut stderr) = (child.stdout.take().expect("piped"), child.stderr.take().expect("piped"));
        let conn = Arc::new(Conn {
            pid: child.id(),
            stdin: AsyncMutex::new(child.stdin.take()),
            child: AsyncMutex::new(Some(child)),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(0),
            closed: AtomicBool::new(false),
            exited: Notify::new(),
            stopping: AtomicBool::new(false),
            instructions: Mutex::new(String::new()),
            capabilities: Mutex::new(Value::Null),
        });
        let h = hooks.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 8192];
            while let Ok(n) = stderr.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                h.stderr(&String::from_utf8_lossy(&buf[..n]));
            }
        });
        tokio::spawn(read_loop(conn.clone(), stdout, hooks));

        let init = conn.request("initialize", json!({"protocolVersion": PROTOCOL_VERSION, "capabilities": {}, "clientInfo": {"name": spec.client, "version": "1.0.0"}}), timeout).await;
        let init = match init {
            Ok(v) => v,
            Err(e) => {
                conn.close().await;
                return Err(e);
            }
        };
        let version = init.get("protocolVersion").and_then(Value::as_str).unwrap_or_default();
        if !SUPPORTED.contains(&version) {
            conn.close().await;
            return Err(RpcError { code: 0, message: format!("Server's protocol version is not supported: {version}"), unsent: false });
        }
        *conn.capabilities.lock().unwrap_or_else(|e| e.into_inner()) = init.get("capabilities").cloned().unwrap_or(Value::Null);
        if let Some(i) = init.get("instructions").and_then(Value::as_str) {
            *conn.instructions.lock().unwrap_or_else(|e| e.into_inner()) = i.to_owned();
        }
        conn.notify("notifications/initialized", None).await;
        Ok(conn)
    }

    /// The server's `capabilities` from the handshake.
    pub fn capabilities(&self) -> Value {
        self.capabilities.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn instructions(&self) -> String {
        self.instructions.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    async fn write(&self, msg: &Value) -> bool {
        let mut line = serde_json::to_string(msg).unwrap_or_default();
        line.push('\n');
        let mut stdin = self.stdin.lock().await;
        match stdin.as_mut() {
            Some(s) => s.write_all(line.as_bytes()).await.is_ok() && s.flush().await.is_ok(),
            None => false,
        }
    }

    async fn notify(&self, method: &str, params: Option<Value>) {
        let mut msg = json!({"jsonrpc": "2.0", "method": method});
        if let Some(p) = params {
            msg["params"] = p;
        }
        self.write(&msg).await;
    }

    /// One request; its result, or the error.
    pub async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, RpcError> {
        if self.is_closed() {
            return Err(RpcError::unsent());
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).insert(id, tx);
        if !self.write(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})).await {
            self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
            return Err(RpcError::unsent());
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(answer)) => answer,
            Ok(Err(_)) => Err(RpcError::mcp(CONNECTION_CLOSED, "Connection closed")),
            Err(_) => {
                self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
                let reason = "McpError: MCP error -32001: Request timed out";
                self.notify("notifications/cancelled", Some(json!({"requestId": id, "reason": reason}))).await;
                Err(RpcError::mcp(REQUEST_TIMEOUT, "Request timed out"))
            }
        }
    }

    /// `tools/call` with `_meta` when given.
    pub async fn call_tool(&self, name: &str, args: &Value, meta: Option<&Value>, timeout: Duration) -> Result<Value, RpcError> {
        let mut params = json!({"name": name, "arguments": args});
        if let Some(m) = meta {
            params["_meta"] = m.clone();
        }
        self.request("tools/call", params, timeout).await
    }

    /// Fails every request in flight now (the owner stops the plugin).
    pub fn abandon(&self) {
        let pending: Vec<Pending> = self.pending.lock().unwrap_or_else(|e| e.into_inner()).drain().map(|(_, tx)| tx).collect();
        for tx in pending {
            let _ = tx.send(Err(RpcError::mcp(CONNECTION_CLOSED, "Connection closed")));
        }
    }

    /// The SDK's `transport.close()`: end stdin, then SIGTERM after 2 s,
    /// then SIGKILL after 2 more; returns once the child is gone.
    pub async fn close(&self) {
        self.stdin.lock().await.take();
        let Some(mut child) = self.child.lock().await.take() else { return };
        for signal in [Some(libc::SIGTERM), Some(libc::SIGKILL), None] {
            if tokio::time::timeout(Duration::from_secs(2), child.wait()).await.is_ok() {
                break;
            }
            match (signal, child.id()) {
                (Some(sig), Some(pid)) => unsafe {
                    libc::kill(pid as i32, sig);
                },
                _ => {
                    let _ = child.wait().await;
                }
            }
        }
        // the reader's end (a grandchild may hold stdout open: don't wait long)
        let _ = tokio::time::timeout(Duration::from_secs(2), self.exited.notified()).await;
    }
}

async fn read_loop(conn: Arc<Conn>, stdout: tokio::process::ChildStdout, hooks: Arc<dyn Hooks>) {
    let mut lines = BufReader::new(stdout).split(b'\n');
    while let Ok(Some(raw)) = lines.next_segment().await {
        let text = String::from_utf8_lossy(&raw);
        let text = text.strip_suffix('\r').unwrap_or(&text);
        let msg: Value = match serde_json::from_str(text) {
            Ok(Value::Object(o)) if o.get("jsonrpc") == Some(&json!("2.0")) => Value::Object(o),
            // not JSON-RPC: the connection is broken (plugins.mjs closes it)
            _ => break,
        };
        let id = msg.get("id").cloned();
        match (msg.get("method").and_then(Value::as_str), id) {
            (Some(method), Some(id)) => {
                let reply = if method == "ping" {
                    json!({"jsonrpc": "2.0", "id": id, "result": {}})
                } else {
                    json!({"jsonrpc": "2.0", "id": id, "error": {"code": METHOD_NOT_FOUND, "message": "Method not found"}})
                };
                conn.write(&reply).await;
            }
            (Some("notifications/resources/list_changed"), None) => hooks.list_changed(),
            (Some(_), None) => {}
            (None, Some(id)) => {
                let Some(n) = id.as_u64() else { continue };
                let Some(tx) = conn.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&n) else { continue };
                let answer = match (msg.get("result"), msg.get("error")) {
                    (Some(r), None) => Ok(r.clone()),
                    (_, Some(e)) => {
                        let code = e.get("code").and_then(Value::as_i64).unwrap_or(-32603);
                        Err(RpcError::mcp(code, e.get("message").and_then(Value::as_str).unwrap_or("")))
                    }
                    _ => Err(RpcError::mcp(-32603, "Invalid response")),
                };
                let _ = tx.send(answer);
            }
            (None, None) => break,
        }
    }
    // stdout is done or broken: the child goes, as the SDK's close does
    // (unless close() already holds it)
    let child = conn.child.lock().await.take();
    conn.stdin.lock().await.take();
    if let Some(mut child) = child {
        if tokio::time::timeout(Duration::from_secs(2), child.wait()).await.is_err() {
            let _ = child.kill().await;
        }
    }
    conn.closed.store(true, Ordering::SeqCst);
    conn.abandon();
    conn.exited.notify_waiters();
    conn.exited.notify_one();
    hooks.closed();
}
