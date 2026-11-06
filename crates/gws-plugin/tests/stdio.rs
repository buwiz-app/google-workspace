//! The binary over stdio, as Buwiz Bot's broker drives it: provision, the
//! account pushes, and the mail tools without a usable token (no network:
//! every call here stops before Gmail). The token never appears on stdout
//! or stderr.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};

use base64::Engine as _;
use serde_json::{Value, json};

const TOKEN: &str = "ya29.stdio-secret-token";
#[cfg(feature = "stub")]
const FAKE_SUBJECT: &str = "owner@example.com";

fn b64(b: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

/// The manifest the binary prints, in a file: what a signed one is to the plugin here.
fn manifest_file(name: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("gws-stdio-{name}-{}.json", std::process::id()));
    let out = Command::new(env!("CARGO_BIN_EXE_buwiz-gws"))
        .arg("manifest")
        .output()
        .unwrap();
    std::fs::write(&path, out.stdout).unwrap();
    path
}

/// A plugin run with `lines` on stdin: its status, stdout and stderr.
fn run(
    manifest: Option<&std::path::Path>,
    lines: &[Value],
) -> (std::process::ExitStatus, String, String) {
    let mut c = Command::new(env!("CARGO_BIN_EXE_buwiz-gws"));
    c.arg("mcp")
        .env_remove("BUWIZ_PLUGIN_MANIFEST")
        .env(
            "GWS_DATA_DIR",
            std::env::temp_dir().join(format!("gws-stdio-data-{}", std::process::id())),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
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
    (
        out.status,
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn provision() -> Value {
    let seed = [7u8; 32];
    let public = b64(ed25519_dalek::SigningKey::from_bytes(&seed)
        .verifying_key()
        .as_bytes());
    json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "buwiz.provision", "arguments": {"protocol": "1.0.0", "plugin": "app.buwiz.google-workspace", "key": {"alg": "ed25519", "seed": b64(&seed)}, "pub": public}}})
}

#[test]
fn without_a_readable_signed_manifest_it_does_not_start() {
    let (status, out, err) = run(None, &[]);
    assert!(
        !status.success() && out.is_empty() && err.contains("BUWIZ_PLUGIN_MANIFEST"),
        "{err}"
    );
    let (status, _, err) = run(
        Some(std::path::Path::new("/nonexistent/manifest.json")),
        &[],
    );
    assert!(
        !status.success() && err.contains("BUWIZ_PLUGIN_MANIFEST"),
        "{err}"
    );
}

/// What the Bot verified is what's served, whatever becomes of the file.
#[test]
fn the_manifest_is_read_once() {
    let manifest = manifest_file("pinned");
    let text = std::fs::read_to_string(&manifest).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_buwiz-gws"))
        .arg("mcp")
        .env("BUWIZ_PLUGIN_MANIFEST", &manifest)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut next = || {
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        serde_json::from_str::<Value>(&line).unwrap()
    };
    writeln!(stdin, "{}", provision()).unwrap();
    next();
    std::fs::write(&manifest, r#"{"swapped": true}"#).unwrap();
    writeln!(stdin, "{}", json!({"jsonrpc": "2.0", "id": 2, "method": "resources/read", "params": {"uri": "buwiz://manifest"}})).unwrap();
    assert_eq!(next()["result"]["contents"][0]["text"], text);
    drop(stdin);
    child.wait().unwrap();
}

#[test]
fn a_caller_it_cant_name_gets_nothing() {
    let manifest = manifest_file("nameless");
    let call = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "gws_status", "arguments": {}}});
    let (_, out, _) = run(Some(&manifest), &[provision(), call]);
    let reply: Value = serde_json::from_str(out.lines().last().unwrap()).unwrap();
    assert_eq!(
        reply["result"]["structuredContent"]["code"], "permission_denied",
        "{out}"
    );
}

#[test]
fn a_call_with_no_id_is_not_answered() {
    let manifest = manifest_file("noid");
    let call = json!({"jsonrpc": "2.0", "method": "tools/call", "params": {"name": "gws_status", "arguments": {}}});
    let ping = json!({"jsonrpc": "2.0", "id": 2, "method": "ping"});
    let (_, out, _) = run(Some(&manifest), &[provision(), call, ping]);
    let replies: Vec<Value> = out
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(
        replies.iter().map(|r| r["id"].clone()).collect::<Vec<_>>(),
        [json!(1), json!(2)],
        "{out}"
    );
}

#[test]
fn a_bad_byte_is_one_bad_line() {
    let manifest = manifest_file("byte");
    let mut child = Command::new(env!("CARGO_BIN_EXE_buwiz-gws"))
        .arg("mcp")
        .env("BUWIZ_PLUGIN_MANIFEST", &manifest)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\",\"x\":\"\xff\xfe\"}\n\xc3\x28\n",
        )
        .unwrap();
    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc": "2.0", "id": 2, "method": "ping"})
    )
    .unwrap();
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("\"id\":2"), "it read on: {text}");
}

#[test]
fn a_bad_request_with_an_id_is_answered_and_one_without_is_not() {
    let manifest = manifest_file("junk");
    let lines = [
        json!({"jsonrpc": "1.0", "id": 1, "method": "ping"}),
        json!({"id": 2, "method": "ping"}),
        json!({"jsonrpc": "2.0", "id": 3}),
        json!({"jsonrpc": "2.0", "id": 4, "method": "ping", "params": [1]}),
        json!([5]),
        json!({"jsonrpc": "1.0", "method": "ping"}),
        json!({"jsonrpc": "2.0", "id": 6, "method": "ping"}),
    ];
    let (_, out, _) = run(Some(&manifest), &lines);
    let replies: Vec<Value> = out
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let ids: Vec<Value> = replies.iter().map(|r| r["id"].clone()).collect();
    assert_eq!(
        ids,
        [json!(1), json!(2), json!(3), json!(4), json!(6)],
        "{out}"
    );
    assert!(
        replies[..4].iter().all(|r| r["error"]["code"] == -32600),
        "{out}"
    );
}

/// A bad byte inside a search must not turn into a different search.
#[test]
fn a_line_with_a_bad_byte_is_refused_not_repaired() {
    let manifest = manifest_file("repair");
    let mut child = Command::new(env!("CARGO_BIN_EXE_buwiz-gws"))
        .arg("mcp")
        .env("BUWIZ_PLUGIN_MANIFEST", &manifest)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    writeln!(stdin, "{}", provision()).unwrap();
    stdin.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"gws_search_mail\",\"arguments\":{\"query\":\"from:\xff\"},\"_meta\":{\"buwiz/caller\":{\"origin\":\"agent\"}}}}\n").unwrap();
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let reply: Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
    assert_eq!(
        (reply["id"].clone(), reply["error"]["code"].clone()),
        (json!(2), json!(-32600)),
        "{text}"
    );
}

/// With no HOME there's no store, and mail search works all the same.
#[test]
fn no_home_means_no_store_not_no_plugin() {
    let manifest = manifest_file("nohome");
    let mut child = Command::new(env!("CARGO_BIN_EXE_buwiz-gws"))
        .arg("mcp")
        .env_remove("HOME")
        .env_remove("GWS_DATA_DIR")
        .env("BUWIZ_PLUGIN_MANIFEST", &manifest)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    writeln!(stdin, "{}", provision()).unwrap();
    writeln!(stdin, "{}", json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "gws_search_mail", "arguments": {"query": "x"}, "_meta": {"buwiz/caller": {"origin": "agent"}}}})).unwrap();
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && text.contains("account_required"),
        "{text}"
    );
}

/// Past the cap a call is refused at once, and the ones running still answer.
#[cfg(feature = "stub")]
#[test]
fn past_the_cap_a_call_is_refused_not_queued_forever() {
    let (gate, wait) = std::sync::mpsc::channel::<()>();
    let wait = std::sync::Mutex::new(wait);
    let fake = gws_fake_google::serve(move |_| {
        let _ = wait
            .lock()
            .unwrap()
            .recv_timeout(std::time::Duration::from_secs(10));
        (200, json!({"messages": []}).to_string())
    });
    let base = fake.base.trim_end_matches("/gmail/v1/users/me").to_owned();
    let manifest = manifest_file("cap");
    let mut child = Command::new(env!("CARGO_BIN_EXE_buwiz-gws"))
        .arg("mcp")
        .env("BUWIZ_PLUGIN_MANIFEST", &manifest)
        .env("GWS_STUB_BASE", base)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut next = || {
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        serde_json::from_str::<Value>(&line).unwrap()
    };
    writeln!(stdin, "{}", provision()).unwrap();
    next();
    writeln!(stdin, "{}", json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "buwiz.account.set", "arguments": {"account": "google", "label": "default", "access_token": "fake-access-1", "expires_at": 4_000_000_000u64}}})).unwrap();
    next();
    for id in 10..43 {
        writeln!(stdin, "{}", json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": "gws_search_mail", "arguments": {"query": "x"}, "_meta": {"buwiz/caller": {"origin": "agent"}}}})).unwrap();
    }
    let refused = next();
    assert_eq!(
        (
            refused["id"].clone(),
            refused["result"]["structuredContent"]["code"].clone()
        ),
        (json!(42), json!("rate_limited")),
        "{refused}"
    );
    // a full cap can't stop a screen asking where it stands
    writeln!(stdin, "{}", json!({"jsonrpc": "2.0", "id": 50, "method": "tools/call", "params": {"name": "gws_resolve", "arguments": {"kind": "command", "name": "mail"}, "_meta": {"buwiz/caller": {"origin": "resolve"}}}})).unwrap();
    assert_eq!(next()["id"], 50, "answered at once, past the cap");
    drop(gate);
    drop(stdin);
    child.wait().unwrap();
}

#[test]
fn arguments_that_arent_an_object_are_refused() {
    let manifest = manifest_file("args");
    let call = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "gws_status", "arguments": [1]}});
    let (_, out, _) = run(Some(&manifest), &[provision(), call]);
    let reply: Value = serde_json::from_str(out.lines().last().unwrap()).unwrap();
    assert_eq!(
        reply["result"]["structuredContent"]["code"], "invalid_args",
        "{out}"
    );
}

/// Needs the stub: without the feature the plugin would call the real Gmail.
#[cfg(feature = "stub")]
#[test]
fn a_cancelled_request_gets_no_answer() {
    // a server that holds the answer back until the cancel has gone in
    let (gate, wait) = std::sync::mpsc::channel::<()>();
    let wait = std::sync::Mutex::new(wait);
    let fake = gws_fake_google::serve(move |_| {
        let _ = wait
            .lock()
            .unwrap()
            .recv_timeout(std::time::Duration::from_secs(5));
        (200, json!({"messages": []}).to_string())
    });
    let base = fake.base.trim_end_matches("/gmail/v1/users/me").to_owned();
    let manifest = manifest_file("cancel");
    let mut child = Command::new(env!("CARGO_BIN_EXE_buwiz-gws"))
        .arg("mcp")
        .env("BUWIZ_PLUGIN_MANIFEST", &manifest)
        .env("GWS_STUB_BASE", base)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
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
    writeln!(stdin, "{}", json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "gws_search_mail", "arguments": {"query": "x"}, "_meta": {"buwiz/caller": {"origin": "agent"}}}})).unwrap();
    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 3}})
    )
    .unwrap();
    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc": "2.0", "id": 4, "method": "ping"})
    )
    .unwrap();
    assert!(
        next().contains("\"id\":4"),
        "the ping answers while the search waits"
    );
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
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
        )
        .unwrap();
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        transcript.push_str(&line);
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["id"], id, "{line}");
        v
    };
    let call = |ask: &mut dyn FnMut(&str, Value) -> Value, name: &str, args: Value| {
        ask("tools/call", json!({"name": name, "arguments": args, "_meta": {"buwiz/caller": {"origin": "agent"}}}))["result"].clone()
    };

    ask(
        "initialize",
        json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}),
    );
    let tools = ask("tools/list", json!({}));
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "buwiz.provision",
            "buwiz.session.open",
            "buwiz.session.close",
            "buwiz.ticket.resolve",
            "buwiz.uninstall",
            "buwiz.account.set",
            "buwiz.account.clear",
            "gws_search_mail",
            "gws_read_mail",
            "gws_status",
            "gws_mail_labels",
            "gws_mail_list",
            "gws_mail_thread",
            "gws_mail_changes",
            "gws_mail_card",
            "gws_mail_mark",
            "gws_mail_label",
            "gws_mail_archive",
            "gws_mail_trash",
            "gws_mail_undo",
            "gws_mail_draft_save",
            "gws_mail_draft_read",
            "gws_mail_reply_start",
            "gws_mail_draft_delete",
            "gws_mail_send_arm",
            "gws_mail_send",
            "gws_mail_draft_of",
            "gws_people_suggest",
            "gws_mail_job_start",
            "gws_mail_job",
            "gws_upload_chunk",
            "gws_upload_drop",
            "gws_search_events",
            "gws_event_draft",
            "gws_event_draft_read",
            "gws_cal_list",
            "gws_cal_show",
            "gws_cal_range",
            "gws_cal_changes",
            "gws_cal_next",
            "gws_cal_people",
            "gws_event_read",
            "gws_event_save",
            "gws_event_delete",
            "gws_event_restore",
            "gws_event_rsvp",
            "gws_search_drive",
            "gws_read_doc_text",
            "gws_drive_list",
            "gws_drive_about",
            "gws_drive_changes",
            "gws_drive_file",
            "gws_drive_preview",
            "gws_drive_card",
            "gws_drive_access",
            "gws_drive_upload",
            "gws_drive_folder",
            "gws_drive_rename",
            "gws_drive_move",
            "gws_drive_trash",
            "gws_drive_share",
            "gws_drive_undo",
            "gws_file_open",
            "gws_file_edits",
            "gws_file_upload",
            "gws_file_discard",
            "gws_mail_attachment_open",
            "gws_resolve"
        ]
    );
    let audience = |name: &str| {
        tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == name)
            .unwrap()["_meta"]["buwiz/audience"]
            .clone()
    };
    assert_eq!(audience("gws_read_mail"), json!(["agent", "app"]));
    assert_eq!(
        audience("gws_status"),
        json!(["app"]),
        "the address on it is the screen's, never the model's"
    );
    let read = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "gws_read_mail")
        .unwrap();
    assert_eq!(
        read["annotations"],
        json!({"readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": true})
    );

    let served = ask("resources/read", json!({"uri": "buwiz://manifest"}));
    let m: Value =
        serde_json::from_str(served["result"]["contents"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(
        (
            m["protocol"].as_str(),
            m["version"].as_str(),
            m["id"].as_str(),
            m["short"].as_str()
        ),
        (
            Some("1.0.0"),
            Some(env!("CARGO_PKG_VERSION")),
            Some("app.buwiz.google-workspace"),
            Some("gws")
        )
    );

    let seed = [7u8; 32];
    let public = b64(ed25519_dalek::SigningKey::from_bytes(&seed)
        .verifying_key()
        .as_bytes());
    let p = call(
        &mut ask,
        "buwiz.provision",
        json!({"protocol": "1.0.0", "plugin": "app.buwiz.google-workspace", "key": {"alg": "ed25519", "seed": b64(&seed)}, "pub": public}),
    );
    assert_eq!(p["structuredContent"]["ok"], true, "{p}");

    let account = |token: &str, expires_at: Value| json!({"account": "google", "label": "default", "provider": "google", "subject": "owner@example.com", "scopes": ["https://www.googleapis.com/auth/gmail.readonly"], "access_token": token, "token_type": "Bearer", "expires_at": expires_at});
    let required = json!({"code": "account_required", "account": "google", "message": "Sign in to Google in Buwiz Bot"});

    // nothing pushed yet
    assert_eq!(
        call(
            &mut ask,
            "gws_search_mail",
            json!({"query": "from:bir.gov.ph"})
        )["structuredContent"],
        required
    );
    // pushed, but expired
    assert_eq!(
        call(
            &mut ask,
            "buwiz.account.set",
            account(TOKEN, json!(1_000_000))
        )["structuredContent"],
        json!({"ok": true})
    );
    assert_eq!(
        call(&mut ask, "gws_read_mail", json!({"id": "18f1a"}))["structuredContent"],
        required
    );
    // pushed fresh, then cleared (a sign-out or a failed refresh)
    assert_eq!(
        call(
            &mut ask,
            "buwiz.account.set",
            account(TOKEN, json!(4_000_000_000u64))
        )["structuredContent"],
        json!({"ok": true})
    );
    assert_eq!(
        call(
            &mut ask,
            "buwiz.account.clear",
            json!({"account": "google", "label": "default"})
        )["structuredContent"],
        json!({"ok": true})
    );
    // through the real server: a tool that never leaves this Mac goes a short
    // way, and a card has to survive it
    let draft = call(
        &mut ask,
        "gws_event_draft",
        json!({"title": "Lunch", "start": "2026-11-10T12:00:00+08:00", "end": "2026-11-10T13:00:00+08:00"}),
    );
    assert_eq!(draft["_meta"]["buwiz/open"]["app"], "calendar", "{draft}");
    assert_eq!(
        draft["_meta"]["buwiz/open"]["props"]["draft"],
        draft["structuredContent"]["ticket"]
    );

    let r = call(&mut ask, "gws_search_mail", json!({"query": "x"}));
    assert_eq!(
        (r["structuredContent"].clone(), r["isError"].clone()),
        (required, json!(true))
    );
    // a malformed push is refused
    assert_eq!(
        call(
            &mut ask,
            "buwiz.account.set",
            json!({"account": "google", "access_token": TOKEN})
        )["structuredContent"]["code"],
        "invalid_args"
    );

    drop(ask);
    drop(stdin);
    let status = child.wait().unwrap();
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(status.success(), "{status}: {stderr}");
    assert!(!transcript.contains(TOKEN), "stdout never shows the token");
    assert!(
        !stderr.contains(TOKEN),
        "stderr never shows the token: {stderr}"
    );
}

/// What invites, cancels or answers is the person's. No token is pushed here,
/// so a call that got through would say to sign in, and reach no Google.
#[test]
fn the_model_is_refused_every_calendar_write_and_never_sees_one_on_its_list() {
    let manifest = manifest_file("calendar-writes");
    let writes = [
        "gws_event_save",
        "gws_event_delete",
        "gws_event_restore",
        "gws_event_rsvp",
        "gws_cal_show",
    ];
    let mut lines = vec![
        json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}}),
        provision(),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
    ];
    for (n, origin) in [(100, "agent"), (200, "routine")] {
        for (at, tool) in writes.iter().enumerate() {
            lines.push(json!({"jsonrpc": "2.0", "id": n + at, "method": "tools/call", "params": {"name": tool, "arguments": {"calendar": "primary", "id": "e", "start": "2026-11-02T09:00:00Z", "end": "2026-11-02T10:00:00Z", "guests": ["ana@example.com"]}, "_meta": {"buwiz/caller": {"origin": origin}}}}));
        }
    }
    let (status, out, err) = run(Some(&manifest), &lines);
    assert!(status.success(), "{err}");
    let replies: Vec<Value> = out
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let reply = |id: usize| replies.iter().find(|r| r["id"] == id).unwrap();
    let listed = reply(2)["result"]["tools"].as_array().unwrap();
    let for_model: Vec<&str> = listed
        .iter()
        .filter(|t| {
            t["_meta"]["buwiz/audience"]
                .as_array()
                .is_some_and(|a| a.contains(&json!("agent")))
        })
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        for_model,
        [
            "gws_search_mail",
            "gws_read_mail",
            "gws_mail_draft_save",
            "gws_search_events",
            "gws_event_draft",
            "gws_search_drive",
            "gws_read_doc_text"
        ]
    );
    for n in [100, 200] {
        for (at, tool) in writes.iter().enumerate() {
            let answer = &reply(n + at)["result"];
            assert_eq!(
                answer["structuredContent"]["code"], "permission_denied",
                "{tool}: {answer}"
            );
        }
    }
}

/// Through the real server, as the Bot drives it, since a card can be lost
/// between the tool and stdout where no in-process test sees it. A search from
/// a chat offers the `files` card; from a routine, or a screen, it doesn't.
#[cfg(feature = "stub")]
#[test]
fn a_chats_drive_search_offers_the_files_card_through_the_real_server() {
    use gws_fake_google::{FakeFile, FakeGoogle, GOOGLE_DOC, Seed};

    let google = FakeGoogle::start(Seed::default());
    google.seed_file(
        FakeFile::new("drv-card", "Fake roof quote secret-name", GOOGLE_DOC).holding(b"Fake words"),
    );
    let manifest = manifest_file("drive-card");
    let data = std::env::temp_dir().join(format!("gws-stdio-drive-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    let mut child = Command::new(env!("CARGO_BIN_EXE_buwiz-gws"))
        .arg("mcp")
        .env("BUWIZ_PLUGIN_MANIFEST", &manifest)
        .env("GWS_STUB_BASE", google.base())
        .env("GWS_DATA_DIR", &data)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut id = 1;
    let mut ask = |name: &str, args: Value, caller: Value| -> Value {
        id += 1;
        writeln!(stdin, "{}", json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": name, "arguments": args, "_meta": {"buwiz/caller": caller}}})).unwrap();
        // the sync thread writes nothing here, so the next line is this reply
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["id"], id, "{line}");
        v["result"].clone()
    };
    let provision = provision();
    ask(
        "buwiz.provision",
        provision["params"]["arguments"].clone(),
        json!({}),
    );
    ask(
        "buwiz.account.set",
        json!({"account": "google", "label": "default", "subject": FAKE_SUBJECT, "access_token": "fake-access-1", "expires_at": 4_000_000_000u64}),
        json!({}),
    );

    let agent = json!({"origin": "agent"});
    let found = ask("gws_search_drive", json!({"query": "roof"}), agent.clone());
    let open = &found["_meta"]["buwiz/open"];
    assert_eq!(open["app"], "drive", "{found}");
    assert_eq!(open["title"], "Drive");
    assert_eq!(open["placement"], "inline");
    let ticket = open["props"]["search"].as_str().unwrap();
    assert!(ticket.len() <= 30, "the manifest's props hold it");
    assert!(
        !found["_meta"].to_string().contains("secret-name"),
        "no file name in _meta: {found}"
    );
    assert_eq!(found["structuredContent"]["items"][0]["id"], "drv-card");
    assert!(found["structuredContent"]["untrusted"].is_string());

    // nothing found is no card
    let none = ask("gws_search_drive", json!({"query": "zzz"}), agent);
    assert_eq!(none["structuredContent"]["items"], json!([]));
    assert_eq!(none.get("_meta"), None, "{none}");

    let routine = ask(
        "gws_search_drive",
        json!({"query": "roof"}),
        json!({"origin": "routine", "routine": "r1"}),
    );
    assert_eq!(routine["structuredContent"]["items"][0]["id"], "drv-card");
    assert_eq!(
        routine.get("_meta"),
        None,
        "a routine has no one to show it to"
    );
    drop(ask);
    drop(stdin);
    child.wait().unwrap();
    let _ = std::fs::remove_dir_all(&data);
}

/// A person's press, through the real server: an app's call from an open the
/// plugin certified. `_meta` has to reach stdout from a call that was never a
/// chat's, and the model and a routine must not get so far.
#[cfg(feature = "stub")]
#[test]
fn a_press_gets_its_document_through_the_real_server_and_the_model_gets_nothing() {
    use gws_fake_google::{FakeFile, FakeGoogle, Seed};

    const DOCX: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
    let google = FakeGoogle::start(Seed::default());
    google.seed_file(FakeFile::new("drv-open", "../../Roof quote.docx", DOCX).holding(b"PK fake"));
    google.seed_file(
        FakeFile::new("drv-doc", "Notes", gws_fake_google::GOOGLE_DOC).holding(b"PK doc"),
    );
    let manifest = manifest_file("drive-open");
    let data = std::env::temp_dir().join(format!("gws-stdio-open-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    let mut child = Command::new(env!("CARGO_BIN_EXE_buwiz-gws"))
        .arg("mcp")
        .env("BUWIZ_PLUGIN_MANIFEST", &manifest)
        .env("GWS_STUB_BASE", google.base())
        .env("GWS_DATA_DIR", &data)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut id = 1;
    let mut ask = |name: &str, args: Value, caller: Value| -> Value {
        id += 1;
        writeln!(stdin, "{}", json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": name, "arguments": args, "_meta": {"buwiz/caller": caller}}})).unwrap();
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["id"], id, "{line}");
        v["result"].clone()
    };
    let provision = provision();
    ask(
        "buwiz.provision",
        provision["params"]["arguments"].clone(),
        json!({}),
    );
    ask(
        "buwiz.account.set",
        json!({"account": "google", "label": "default", "subject": FAKE_SUBJECT, "access_token": "fake-access-1", "expires_at": 4_000_000_000u64}),
        json!({}),
    );
    let certified = |ask: &mut dyn FnMut(&str, Value, Value) -> Value, app: &str, open_id: &str| {
        let plan = gws::manifest::unsigned(gws::manifest::RELEASE_KEY_ID);
        let entry = buwiz_plugin::manifest::app(&plan, app).unwrap();
        let not_after = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            + 3_600_000;
        let opened = ask(
            "buwiz.session.open",
            json!({"app": app, "open_id": open_id, "ks_pub": buwiz_plugin::b64::encode(&[2; 32]), "perms_sha256": buwiz_plugin::manifest::permissions_digest(entry).unwrap(), "not_after": not_after}),
            Value::Null,
        );
        assert_ne!(opened["isError"], true, "{opened}");
        json!({"origin": "app", "plugin": "app.buwiz.google-workspace", "app": app, "open_id": open_id, "ts": 1})
    };
    let open_id = buwiz_plugin::b64::encode(&[9; 16]);
    let press = certified(&mut ask, "drive", &open_id);

    let opened = ask("gws_file_open", json!({"id": "drv-open"}), press.clone());
    let document = &opened["_meta"]["buwiz/document"];
    let path = std::path::PathBuf::from(
        document["path"]
            .as_str()
            .unwrap_or_else(|| panic!("{opened}")),
    );
    assert_eq!(
        path.parent().unwrap(),
        std::fs::canonicalize(data.join("open/drv-open")).unwrap()
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"PK fake");
    assert_eq!(document["name"], "Roof quote.docx");
    assert_eq!(document["mime"], DOCX);
    assert_eq!(opened["structuredContent"]["opened"], true);
    assert!(
        !opened["structuredContent"]
            .to_string()
            .contains("gws-stdio"),
        "{opened}"
    );

    // a Google file, which takes the other way down the same call
    let exported = ask("gws_file_open", json!({"id": "drv-doc"}), press.clone());
    assert_eq!(
        exported["_meta"]["buwiz/document"]["name"], "Notes.docx",
        "{exported}"
    );

    // the rest of the round trip, as the screen drives it
    std::fs::write(&path, b"PK edited").unwrap();
    let edits = ask("gws_file_edits", json!({}), press.clone());
    assert_eq!(
        edits["structuredContent"]["files"][0]["id"], "drv-open",
        "{edits}"
    );
    let sent = ask(
        "gws_file_upload",
        json!({"id": "drv-open", "mode": "update"}),
        press.clone(),
    );
    assert_eq!(sent["structuredContent"]["uploaded"], true, "{sent}");
    assert_eq!(google.drive_file("drv-open").unwrap().content, b"PK edited");

    // discard is answered on the loop itself, not on a thread of its own
    let dropped = ask("gws_file_discard", json!({"id": "drv-doc"}), press.clone());
    assert_eq!(dropped["structuredContent"]["discarded"], true, "{dropped}");
    assert!(!data.join("open/drv-doc").exists());

    // Mail's screen isn't Drive's: a certified open reaches only what its app holds
    let mail_id = buwiz_plugin::b64::encode(&[8; 16]);
    let mail = certified(&mut ask, "mail", &mail_id);
    let wrong = ask("gws_file_open", json!({"id": "drv-open"}), mail);
    assert_eq!(
        wrong["structuredContent"]["code"], "permission_denied",
        "{wrong}"
    );

    // an open nobody certified is nobody's
    let forged = json!({"origin": "app", "plugin": "app.buwiz.google-workspace", "app": "drive", "open_id": "forged", "ts": 1});
    let denied = ask("gws_file_open", json!({"id": "drv-open"}), forged);
    assert_eq!(
        denied["structuredContent"]["code"], "permission_denied",
        "{denied}"
    );

    let writes = google.writes().len();
    let cases = [
        ("gws_file_open", json!({"id": "drv-open"})),
        ("gws_file_edits", json!({})),
        (
            "gws_file_upload",
            json!({"id": "drv-open", "mode": "replace"}),
        ),
        ("gws_file_discard", json!({"id": "drv-open"})),
        (
            "gws_mail_attachment_open",
            json!({"message": "m1", "attachment": "a1"}),
        ),
    ];
    for caller in [
        json!({"origin": "agent"}),
        json!({"origin": "routine", "routine": "r1"}),
    ] {
        for (tool, args) in &cases {
            let answer = ask(tool, args.clone(), caller.clone());
            assert_eq!(
                answer["structuredContent"]["code"], "permission_denied",
                "{tool}: {answer}"
            );
            assert_eq!(answer.get("_meta"), None, "{tool}");
        }
    }
    assert_eq!(google.writes().len(), writes);
    drop(ask);
    drop(stdin);
    child.wait().unwrap();
    let _ = std::fs::remove_dir_all(&data);
}

/// Trash and Share are the person's and never the AI's, whatever it's pressing.
/// Through the real server: the same calls go through for a press of the
/// person's, so it's who's asking that's refused and nothing else. An AI's press
/// reaches here as the AI, and a label it can read, "Move to trash" or "Share",
/// is no way in: nothing here reads one, and the tool is refused all the same.
#[cfg(feature = "stub")]
#[test]
fn the_model_and_a_routine_are_refused_every_drive_write_and_a_person_is_not() {
    use gws_fake_google::{FakeFile, FakeGoogle, Seed};

    let google = FakeGoogle::start(Seed::default());
    google.seed_file(
        FakeFile::new("drv-a", "Roof quote secret-name.txt", "text/plain").holding(b"x"),
    );
    google.seed_file(FakeFile::folder("drv-into", "Clients"));
    let manifest = manifest_file("drive-writes");
    let data = std::env::temp_dir().join(format!("gws-stdio-writes-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    let mut child = Command::new(env!("CARGO_BIN_EXE_buwiz-gws"))
        .arg("mcp")
        .env("BUWIZ_PLUGIN_MANIFEST", &manifest)
        .env("GWS_STUB_BASE", google.base())
        .env("GWS_DATA_DIR", &data)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut id = 1;
    let mut ask = |name: &str, args: Value, caller: Value| -> Value {
        id += 1;
        writeln!(stdin, "{}", json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": name, "arguments": args, "_meta": {"buwiz/caller": caller}}})).unwrap();
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["id"], id, "{line}");
        v["result"].clone()
    };
    let provision = provision();
    ask(
        "buwiz.provision",
        provision["params"]["arguments"].clone(),
        json!({}),
    );
    ask(
        "buwiz.account.set",
        json!({"account": "google", "label": "default", "subject": FAKE_SUBJECT, "access_token": "fake-access-1", "expires_at": 4_000_000_000u64}),
        json!({}),
    );

    let writes = [
        (
            "gws_drive_trash",
            json!({"ids": ["drv-a"], "label": "Move to trash"}),
        ),
        (
            "gws_drive_share",
            json!({"id": "drv-a", "add": [{"email": "ana@example.com", "role": "writer"}], "seen": "x", "label": "Share with these people"}),
        ),
        ("gws_drive_rename", json!({"id": "drv-a", "name": "Stolen"})),
        ("gws_drive_move", json!({"id": "drv-a", "to": "drv-into"})),
        (
            "gws_drive_folder",
            json!({"parent": "root", "name": "Stolen"}),
        ),
        (
            "gws_drive_upload",
            json!({"upload": "up1", "parent": "root"}),
        ),
        ("gws_drive_undo", json!({"undo": "du1"})),
        ("gws_drive_access", json!({"id": "drv-a"})),
    ];
    for caller in [
        json!({"origin": "agent"}),
        json!({"origin": "routine", "routine": "r1"}),
    ] {
        for (tool, args) in &writes {
            let answer = ask(tool, args.clone(), caller.clone());
            assert_eq!(
                answer["structuredContent"]["code"], "permission_denied",
                "{tool}: {answer}"
            );
            assert_eq!(answer["isError"], true, "{tool}");
            assert!(!answer.to_string().contains("secret-name"), "{answer}");
        }
    }
    assert_eq!(google.writes(), [], "not one write reached Google");
    assert_eq!(google.drive_writes(), []);
    let file = google.drive_file("drv-a").unwrap();
    assert!(!file.trashed && file.people.is_empty());

    // the same calls, from a screen a person has open
    let plan = gws::manifest::unsigned(gws::manifest::RELEASE_KEY_ID);
    let entry = buwiz_plugin::manifest::app(&plan, "drive").unwrap();
    let not_after = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 3_600_000;
    let open_id = buwiz_plugin::b64::encode(&[7; 16]);
    let opened = ask(
        "buwiz.session.open",
        json!({"app": "drive", "open_id": open_id, "ks_pub": buwiz_plugin::b64::encode(&[2; 32]), "perms_sha256": buwiz_plugin::manifest::permissions_digest(entry).unwrap(), "not_after": not_after}),
        Value::Null,
    );
    assert_ne!(opened["isError"], true, "{opened}");
    let press = json!({"origin": "app", "plugin": "app.buwiz.google-workspace", "app": "drive", "open_id": open_id, "ts": 1});

    let people = ask("gws_drive_access", json!({"id": "drv-a"}), press.clone());
    let seen = people["structuredContent"]["seen"].as_str().unwrap();
    assert_eq!(
        people["structuredContent"]["people"][0]["role"], "owner",
        "{people}"
    );
    let shared = ask(
        "gws_drive_share",
        json!({"id": "drv-a", "add": [{"email": "ana@example.com", "role": "writer"}], "message": "Please look", "seen": seen}),
        press.clone(),
    );
    assert_eq!(shared["structuredContent"]["added"], 1, "{shared}");
    assert_eq!(shared["structuredContent"]["files"][0]["id"], "drv-a");
    assert_eq!(shared["structuredContent"]["files"][0]["shared"], true);
    let invited = google.drive_writes();
    assert_eq!(
        invited[0]
            .query
            .get("sendNotificationEmail")
            .map(String::as_str),
        Some("true")
    );

    let trashed = ask("gws_drive_trash", json!({"ids": ["drv-a"]}), press.clone());
    let undo = trashed["structuredContent"]["undo"].as_str().unwrap();
    assert_eq!(
        trashed["structuredContent"]["files"][0]["trashed"], true,
        "{trashed}"
    );
    assert!(google.drive_file("drv-a").unwrap().trashed);
    let back = ask("gws_drive_undo", json!({"undo": undo}), press.clone());
    assert_eq!(
        back["structuredContent"]["files"][0]["trashed"], false,
        "{back}"
    );
    assert!(!google.drive_file("drv-a").unwrap().trashed);

    // an Undo the model can't spend, even with the id the screen just had
    let again = ask("gws_drive_trash", json!({"ids": ["drv-a"]}), press);
    let undo = again["structuredContent"]["undo"].as_str().unwrap();
    let refused = ask(
        "gws_drive_undo",
        json!({"undo": undo}),
        json!({"origin": "agent"}),
    );
    assert_eq!(refused["structuredContent"]["code"], "permission_denied");
    assert!(google.drive_file("drv-a").unwrap().trashed);

    drop(ask);
    drop(stdin);
    child.wait().unwrap();
    let _ = std::fs::remove_dir_all(&data);
}

/// A live run against a real account writes to one address alone, so Share keeps
/// to that as Send does, before any request leaves.
#[cfg(feature = "stub")]
#[test]
fn a_test_run_shares_with_nobody_but_its_own_address() {
    use gws_fake_google::{FakeFile, FakeGoogle, Seed};

    let google = FakeGoogle::start(Seed::default());
    google.seed_file(FakeFile::new("drv-a", "A.txt", "text/plain").holding(b"x"));
    let manifest = manifest_file("only-to");
    let data = std::env::temp_dir().join(format!("gws-stdio-only-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    let mut child = Command::new(env!("CARGO_BIN_EXE_buwiz-gws"))
        .arg("mcp")
        .env("BUWIZ_PLUGIN_MANIFEST", &manifest)
        .env("GWS_STUB_BASE", google.base())
        .env("GWS_DATA_DIR", &data)
        .env("GWS_ONLY_TO", "support@example.com")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut id = 1;
    let mut ask = |name: &str, args: Value, caller: Value| -> Value {
        id += 1;
        writeln!(stdin, "{}", json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": name, "arguments": args, "_meta": {"buwiz/caller": caller}}})).unwrap();
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        serde_json::from_str::<Value>(&line).unwrap()["result"].clone()
    };
    let provision = provision();
    ask(
        "buwiz.provision",
        provision["params"]["arguments"].clone(),
        json!({}),
    );
    ask(
        "buwiz.account.set",
        json!({"account": "google", "label": "default", "subject": FAKE_SUBJECT, "access_token": "fake-access-1", "expires_at": 4_000_000_000u64}),
        json!({}),
    );
    let plan = gws::manifest::unsigned(gws::manifest::RELEASE_KEY_ID);
    let entry = buwiz_plugin::manifest::app(&plan, "drive").unwrap();
    let open_id = buwiz_plugin::b64::encode(&[6; 16]);
    let not_after = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 3_600_000;
    ask(
        "buwiz.session.open",
        json!({"app": "drive", "open_id": open_id, "ks_pub": buwiz_plugin::b64::encode(&[2; 32]), "perms_sha256": buwiz_plugin::manifest::permissions_digest(entry).unwrap(), "not_after": not_after}),
        Value::Null,
    );
    let press = json!({"origin": "app", "plugin": "app.buwiz.google-workspace", "app": "drive", "open_id": open_id, "ts": 1});
    let people = ask("gws_drive_access", json!({"id": "drv-a"}), press.clone());
    let seen = people["structuredContent"]["seen"].clone();
    let refused = ask(
        "gws_drive_share",
        json!({"id": "drv-a", "add": [{"email": "ana@example.com", "role": "reader"}], "seen": seen}),
        press.clone(),
    );
    assert_eq!(
        refused["structuredContent"]["refused"], "not_allowed",
        "{refused}"
    );
    assert_eq!(google.drive_writes(), []);
    let fine = ask(
        "gws_drive_share",
        json!({"id": "drv-a", "add": [{"email": "support@example.com", "role": "reader"}], "seen": seen}),
        press,
    );
    assert_eq!(fine["structuredContent"]["added"], 1, "{fine}");
    drop(ask);
    drop(stdin);
    child.wait().unwrap();
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn the_manifest_command_prints_this_version_for_protocol_1_0() {
    let out = Command::new(env!("CARGO_BIN_EXE_buwiz-gws"))
        .arg("manifest")
        .output()
        .unwrap();
    let m: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(m["protocol"].as_str(), Some("1.0.0"));
    assert_eq!(
        m["version"].as_str(),
        Some(env!("CARGO_PKG_VERSION")),
        "Cargo.toml and the manifest agree"
    );
}
