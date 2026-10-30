//! The reserved tools (spec 7.8), as the fixture has them: the key in
//! memory only, provision first. The plugin has no apps, so session opens
//! are refused, and no tickets, so every ticket is invalid.

use std::sync::Mutex;

use serde_json::{json, Value};

use crate::tools::{fail, ok};
use crate::{config, js};

pub const NAMES: [&str; 5] = ["rox.provision", "rox.session.open", "rox.session.close", "rox.ticket.resolve", "rox.uninstall"];

/// The provisioned key: memory only (4.3), never logged, written or echoed.
#[derive(Default)]
pub struct Key(Mutex<Option<String>>);

impl Key {
    pub fn is_set(&self) -> bool {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).is_some()
    }
}

/// A reserved tool's result, or an error thrown to the caller as JSON-RPC
/// -32603 (a bad seed, an unreadable manifest).
pub fn call(key: &Key, name: &str, args: &Value) -> Result<Value, String> {
    Ok(match name {
        "rox.provision" => return provision(key, args),
        "rox.session.open" => fail("invalid_args", &format!("no app {}: this plugin has no apps", js::slice_utf16(&js::string(args.get("app")), 60))),
        "rox.ticket.resolve" => ok(json!({"valid": false})),
        _ => ok(json!({"ok": true})),
    })
}

fn provision(key: &Key, args: &Value) -> Result<Value, String> {
    let k = args.get("key");
    if k.and_then(|k| k.get("alg")) != Some(&json!("ed25519")) {
        return Ok(fail("invalid_args", "the key and pub do not match"));
    }
    let seed = k.and_then(|k| k.get("seed")).and_then(Value::as_str).unwrap_or_default();
    let public = rox_v1::keys::SessionKey::from_seed(seed).map_err(|_| "a seed is 32 bytes of base64url".to_owned())?.public();
    if args.get("pub").and_then(Value::as_str) != Some(public.as_str()) {
        return Ok(fail("invalid_args", "the key and pub do not match"));
    }
    {
        let mut held = key.0.lock().unwrap_or_else(|e| e.into_inner());
        if held.as_deref().is_some_and(|s| s != seed) {
            return Ok(fail("invalid_args", "already provisioned with another key"));
        }
        *held = Some(seed.to_owned());
    }
    let m = rox_v1::jcs::parse_ijson(&config::manifest_text()?)?;
    let sha = rox_v1::keys::sha256_hex(&rox_v1::jcs::signed_bytes(&m)?);
    Ok(ok(json!({"ok": true, "manifest_sha256": sha})))
}
