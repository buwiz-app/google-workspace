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

#[test]
fn accounts_are_pushed_held_and_cleared_without_the_token_ever_showing() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_buwiz-gws"))
        .arg("mcp")
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
