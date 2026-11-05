//! The binary over stdio, as Buwiz Bot's broker drives it: provision, the
//! account pushes, and the mail tools without a usable token (no network:
//! every call here stops before Gmail). The token never appears on stdout
//! or stderr.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};

use base64::Engine as _;
use serde_json::{json, Value};

const TOKEN: &str = "ya29.stdio-secret-token";

fn b64(b: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

/// The manifest the binary prints, in a file: what a signed one is to the plugin here.
fn manifest_file(name: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("gws-stdio-{name}-{}.json", std::process::id()));
    let out = Command::new(env!("CARGO_BIN_EXE_buwiz-gws")).arg("manifest").output().unwrap();
    std::fs::write(&path, out.stdout).unwrap();
    path
}

/// A plugin run with `lines` on stdin: its status, stdout and stderr.
fn run(manifest: Option<&std::path::Path>, lines: &[Value]) -> (std::process::ExitStatus, String, String) {
    let mut c = Command::new(env!("CARGO_BIN_EXE_buwiz-gws"));
    c.arg("mcp").env_remove("BUWIZ_PLUGIN_MANIFEST").env("GWS_DATA_DIR", std::env::temp_dir().join(format!("gws-stdio-data-{}", std::process::id()))).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(m) = manifest {
        c.env("BUWIZ_PLUGIN_MANIFEST", m);
    }
    let mut child = c.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    for line in lines {
        let _ = writeln!(stdin, "{line}");
    }
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    (out.status, String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned())
}

fn provision() -> Value {
    let seed = [7u8; 32];
    let public = b64(ed25519_dalek::SigningKey::from_bytes(&seed).verifying_key().as_bytes());
    json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "buwiz.provision", "arguments": {"protocol": "1.0.0", "plugin": "app.buwiz.google-workspace", "key": {"alg": "ed25519", "seed": b64(&seed)}, "pub": public}}})
}

#[test]
fn without_a_readable_signed_manifest_it_does_not_start() {
    let (status, out, err) = run(None, &[]);
    assert!(!status.success() && out.is_empty() && err.contains("BUWIZ_PLUGIN_MANIFEST"), "{err}");
    let (status, _, err) = run(Some(std::path::Path::new("/nonexistent/manifest.json")), &[]);
    assert!(!status.success() && err.contains("BUWIZ_PLUGIN_MANIFEST"), "{err}");
}

#[test]
fn a_call_with_no_id_is_not_answered_and_a_bad_byte_is_one_bad_line() {
    let manifest = manifest_file("noid");
    let call = json!({"jsonrpc": "2.0", "method": "tools/call", "params": {"name": "gws_status", "arguments": {}}});
    let ping = json!({"jsonrpc": "2.0", "id": 2, "method": "ping"});
    let (_, out, _) = run(Some(&manifest), &[provision(), call, ping]);
    let replies: Vec<Value> = out.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(replies.iter().map(|r| r["id"].clone()).collect::<Vec<_>>(), [json!(1), json!(2)], "{out}");
}

#[test]
fn arguments_that_arent_an_object_are_refused() {
    let manifest = manifest_file("args");
    let call = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "gws_status", "arguments": [1]}});
    let (_, out, _) = run(Some(&manifest), &[provision(), call]);
    let reply: Value = serde_json::from_str(out.lines().last().unwrap()).unwrap();
    assert_eq!(reply["result"]["structuredContent"]["code"], "invalid_args", "{out}");
}

/// Needs the stub: without the feature the plugin would call the real Gmail.
#[cfg(feature = "stub")]
#[test]
fn a_cancelled_request_gets_no_answer() {
    // a server that holds the answer back until the cancel has gone in
    let (gate, wait) = std::sync::mpsc::channel::<()>();
    let wait = std::sync::Mutex::new(wait);
    let fake = gws_fake_google::serve(move |_| {
        let _ = wait.lock().unwrap().recv_timeout(std::time::Duration::from_secs(5));
        (200, json!({"messages": []}).to_string())
    });
    let base = fake.base.trim_end_matches("/gmail/v1/users/me").to_owned();
    let manifest = manifest_file("cancel");
    let mut child = Command::new(env!("CARGO_BIN_EXE_buwiz-gws")).arg("mcp").env("BUWIZ_PLUGIN_MANIFEST", &manifest).env("GWS_STUB_BASE", base).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut next = || {
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        line
    };
    writeln!(stdin, "{}", provision()).unwrap();
    assert!(next().contains("manifest_sha256"));
    writeln!(stdin, "{}", json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "buwiz.account.set", "arguments": {"account": "google", "label": "default", "access_token": "fake-access-1", "expires_at": 4_000_000_000u64}}})).unwrap();
    next();
    writeln!(stdin, "{}", json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "gws_search_mail", "arguments": {"query": "x"}}})).unwrap();
    writeln!(stdin, "{}", json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 3}})).unwrap();
    writeln!(stdin, "{}", json!({"jsonrpc": "2.0", "id": 4, "method": "ping"})).unwrap();
    assert!(next().contains("\"id\":4"), "the ping answers while the search waits");
    drop(gate);
    drop(stdin);
    let mut rest = String::new();
    stdout.read_to_string(&mut rest).unwrap();
    child.wait().unwrap();
    assert!(!rest.contains("\"id\":3"), "{rest}");
}

#[test]
fn accounts_are_pushed_held_and_cleared_without_the_token_ever_showing() {
    let manifest = manifest_file("accounts");
    let mut child = Command::new(env!("CARGO_BIN_EXE_buwiz-gws"))
        .arg("mcp")
        .env("BUWIZ_PLUGIN_MANIFEST", &manifest)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut transcript = String::new();
    let mut id = 0;
    let mut ask = |method: &str, params: Value| -> Value {
        id += 1;
        writeln!(stdin, "{}", json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})).unwrap();
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        transcript.push_str(&line);
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["id"], id, "{line}");
        v
    };
    let call = |ask: &mut dyn FnMut(&str, Value) -> Value, name: &str, args: Value| ask("tools/call", json!({"name": name, "arguments": args}))["result"].clone();

    ask("initialize", json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}));
    let tools = ask("tools/list", json!({}));
    let names: Vec<&str> = tools["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["buwiz.provision", "buwiz.session.open", "buwiz.session.close", "buwiz.ticket.resolve", "buwiz.uninstall", "buwiz.account.set", "buwiz.account.clear", "gws_search_mail", "gws_read_mail", "gws_status", "gws_resolve"]);
    let audience = |name: &str| tools["result"]["tools"].as_array().unwrap().iter().find(|t| t["name"] == name).unwrap()["_meta"]["buwiz/audience"].clone();
    assert_eq!(audience("gws_read_mail"), json!(["agent", "app"]));
    assert_eq!(audience("gws_status"), json!(["app"]), "the address on it is the screen's, never the model's");
    let read = tools["result"]["tools"].as_array().unwrap().iter().find(|t| t["name"] == "gws_read_mail").unwrap();
    assert_eq!(read["annotations"], json!({"readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": true}));

    let served = ask("resources/read", json!({"uri": "buwiz://manifest"}));
    let m: Value = serde_json::from_str(served["result"]["contents"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!((m["protocol"].as_str(), m["version"].as_str(), m["id"].as_str(), m["short"].as_str()), (Some("1.0.0"), Some(env!("CARGO_PKG_VERSION")), Some("app.buwiz.google-workspace"), Some("gws")));

    let seed = [7u8; 32];
    let public = b64(ed25519_dalek::SigningKey::from_bytes(&seed).verifying_key().as_bytes());
    let p = call(&mut ask, "buwiz.provision", json!({"protocol": "1.0.0", "plugin": "app.buwiz.google-workspace", "key": {"alg": "ed25519", "seed": b64(&seed)}, "pub": public}));
    assert_eq!(p["structuredContent"]["ok"], true, "{p}");

    let account = |token: &str, expires_at: Value| json!({"account": "google", "label": "default", "provider": "google", "subject": "owner@example.com", "scopes": ["https://www.googleapis.com/auth/gmail.readonly"], "access_token": token, "token_type": "Bearer", "expires_at": expires_at});
    let required = json!({"code": "account_required", "account": "google", "message": "Sign in to Google in Buwiz Bot"});

    // nothing pushed yet
    assert_eq!(call(&mut ask, "gws_search_mail", json!({"query": "from:bir.gov.ph"}))["structuredContent"], required);
    // pushed, but expired
    assert_eq!(call(&mut ask, "buwiz.account.set", account(TOKEN, json!(1_000_000)))["structuredContent"], json!({"ok": true}));
    assert_eq!(call(&mut ask, "gws_read_mail", json!({"id": "18f1a"}))["structuredContent"], required);
    // pushed fresh, then cleared (a sign-out or a failed refresh)
    assert_eq!(call(&mut ask, "buwiz.account.set", account(TOKEN, json!(4_000_000_000u64)))["structuredContent"], json!({"ok": true}));
    assert_eq!(call(&mut ask, "buwiz.account.clear", json!({"account": "google", "label": "default"}))["structuredContent"], json!({"ok": true}));
    let r = call(&mut ask, "gws_search_mail", json!({"query": "x"}));
    assert_eq!((r["structuredContent"].clone(), r["isError"].clone()), (required, json!(true)));
    // a malformed push is refused
    assert_eq!(call(&mut ask, "buwiz.account.set", json!({"account": "google", "access_token": TOKEN}))["structuredContent"]["code"], "invalid_args");

    drop(ask);
    drop(stdin);
    let status = child.wait().unwrap();
    assert!(status.success());
    let mut stderr = String::new();
    child.stderr.take().unwrap().read_to_string(&mut stderr).unwrap();
    assert!(!transcript.contains(TOKEN), "stdout never shows the token");
    assert!(!stderr.contains(TOKEN), "stderr never shows the token: {stderr}");
}

#[test]
fn the_manifest_command_prints_this_version_for_protocol_1_0() {
    let out = Command::new(env!("CARGO_BIN_EXE_buwiz-gws")).arg("manifest").output().unwrap();
    let m: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(m["protocol"].as_str(), Some("1.0.0"));
    assert_eq!(m["version"].as_str(), Some(env!("CARGO_PKG_VERSION")), "Cargo.toml and the manifest agree");
}
