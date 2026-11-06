//! package/plugin.json, the 1.0 package's source: it agrees with the
//! served manifest, and (with `BUWIZ_BROKER_BIN` set to a buwiz-broker build)
//! what the release script makes of it is signed with a throwaway key and
//! passes `buwiz-broker manifest package` and `manifest verify` as Buwiz Bot
//! checks them at install.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use base64::Engine as _;
use serde_json::{Value, json};

fn repo(p: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").join(p)
}

fn read_json(p: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

#[test]
fn the_template_matches_the_served_manifest() {
    let p = read_json(&repo("package/plugin.json"));
    let m = gws::manifest::unsigned(gws::manifest::RELEASE_KEY_ID);
    let b = &p["buwiz"];
    for k in ["id", "short", "version", "protocol"] {
        assert_eq!(b[k], m[k], "buwiz.{k}");
    }
    assert_eq!(
        (p["version"].as_str(), b["version"].as_str()),
        (
            Some(env!("CARGO_PKG_VERSION")),
            Some(env!("CARGO_PKG_VERSION"))
        )
    );
    assert_eq!(
        b["publisher"],
        json!({"id": m["publisher"]["id"], "key_id": m["publisher"]["key_id"]})
    );
    assert_eq!(b["server"], "gws");
    assert_eq!(
        p["mcpServers"]["gws"],
        json!({"command": "${PLUGIN_ROOT}/bin/buwiz-gws", "args": ["mcp"], "env": {"BUWIZ_PLUGIN_MANIFEST": "${PLUGIN_ROOT}/manifest.signed.json"}})
    );
    assert_eq!(
        b["accounts"],
        json!([{"id": "google", "provider": "google", "label": "Google account", "scopes": ["https://www.googleapis.com/auth/gmail.modify", "https://www.googleapis.com/auth/contacts.readonly", "https://www.googleapis.com/auth/contacts.other.readonly", "https://www.googleapis.com/auth/calendar.events", "https://www.googleapis.com/auth/calendar.calendarlist.readonly", "https://www.googleapis.com/auth/drive"], "required": true}])
    );
    assert!(b.get("signature").is_none(), "the release script signs it");
}

fn broker(bin: &str, args: &[&str], stdin: Option<&Value>) -> Value {
    let mut c = Command::new(bin)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(v) = stdin {
        use std::io::Write as _;
        c.stdin
            .take()
            .unwrap()
            .write_all(v.to_string().as_bytes())
            .unwrap();
    }
    drop(c.stdin.take());
    let out = c.wait_with_output().unwrap();
    assert!(
        out.status.success() || stdin.is_some(),
        "buwiz-broker {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap_or(Value::Null)
}

fn plugin_json(template: &Path, dir: &Path, platform: &str, asset: &str, published: Option<&Path>) {
    let mut c = Command::new("python3");
    c.arg(repo("tools/release/plugin-json.py"))
        .arg(template)
        .arg(dir)
        .arg(platform)
        .arg(asset);
    if let Some(p) = published {
        c.arg(p);
    }
    let out = c.output().unwrap();
    assert!(
        out.status.success(),
        "plugin-json.py: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn the_signed_package_passes_the_bots_checks() {
    let Ok(bin) = std::env::var("BUWIZ_BROKER_BIN") else {
        eprintln!(
            "skipped: set BUWIZ_BROKER_BIN to a buwiz-broker build to check the signed package"
        );
        return;
    };
    let dir = std::env::temp_dir().join(format!("buwiz-gws-package-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let s = |p: &Path| p.display().to_string();
    // a throwaway publisher key under a test key id: never the release one
    const TEST_KEY_ID: &str = "gws-test-01";
    let seed = [42u8; 32];
    let b64 = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
    let key = dir.join("test.key");
    std::fs::write(&key, b64(&seed)).unwrap();
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let public = b64(ed25519_dalek::SigningKey::from_bytes(&seed)
        .verifying_key()
        .as_bytes());
    let trust = json!({
        "publishers": {TEST_KEY_ID: {"alg": "ed25519", "publisher": "app.buwiz", "public_key": public, "first_party": true}},
        "approved_plugins": [{"id": "app.buwiz.google-workspace", "publisher_key_id": TEST_KEY_ID}],
    });

    // the release script's steps: the served manifest signed, the binary
    // beside it, plugin.json filled in and its block signed
    let unsigned = dir.join("manifest.json");
    std::fs::write(&unsigned, gws::manifest::unsigned(TEST_KEY_ID).to_string()).unwrap();
    let mut template = read_json(&repo("package/plugin.json"));
    template["buwiz"]["publisher"]["key_id"] = json!(TEST_KEY_ID);
    let template_file = dir.join("plugin.template.json");
    std::fs::write(&template_file, template.to_string()).unwrap();
    broker(
        &bin,
        &[
            "sign-manifest",
            &s(&unsigned),
            "--key-file",
            &s(&key),
            "--out",
            &s(&dir.join("manifest.signed.json")),
        ],
        None,
    );
    std::fs::write(dir.join("buwiz-gws-macos-aarch64"), b"a binary").unwrap();
    plugin_json(
        &template_file,
        &dir,
        "macos-aarch64",
        "buwiz-gws-macos-aarch64",
        None,
    );
    let signed = dir.join("plugin.json");
    broker(
        &bin,
        &[
            "sign-manifest",
            &s(&dir.join("plugin.unsigned.json")),
            "--block",
            "buwiz",
            "--key-file",
            &s(&key),
            "--out",
            &s(&signed),
        ],
        None,
    );

    let served = broker(
        &bin,
        &["manifest", "verify"],
        Some(
            &json!({"text": std::fs::read_to_string(dir.join("manifest.signed.json")).unwrap(), "trust": trust}),
        ),
    );
    assert_eq!(served["ok"], true, "{served}");
    assert_eq!(
        (
            served["value"]["manifest"]["protocol"].as_str(),
            served["value"]["manifest"]["version"].as_str()
        ),
        (Some("1.0.0"), Some(env!("CARGO_PKG_VERSION")))
    );

    let text = std::fs::read_to_string(&signed).unwrap();
    let p = broker(
        &bin,
        &["manifest", "package"],
        Some(&json!({"text": text, "trust": trust, "platform": "macos-aarch64"})),
    );
    assert_eq!(p["ok"], true, "{p}");
    let sha = |f: &str| {
        use sha2::Digest as _;
        sha2::Sha256::digest(std::fs::read(dir.join(f)).unwrap())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    };
    assert_eq!(
        p["value"],
        json!({
            "id": "app.buwiz.google-workspace", "short": "gws", "version": env!("CARGO_PKG_VERSION"), "server": "gws",
            "accounts": [{"id": "google", "provider": "google", "label": "Google account", "scopes": ["https://www.googleapis.com/auth/gmail.modify", "https://www.googleapis.com/auth/contacts.readonly", "https://www.googleapis.com/auth/contacts.other.readonly", "https://www.googleapis.com/auth/calendar.events", "https://www.googleapis.com/auth/calendar.calendarlist.readonly", "https://www.googleapis.com/auth/drive"], "required": true}],
            "files": [{"path": "manifest.signed.json", "asset": "manifest.signed.json", "sha256": sha("manifest.signed.json")}],
            "binary": {"path": "bin/buwiz-gws", "asset": "buwiz-gws-macos-aarch64", "sha256": sha("buwiz-gws-macos-aarch64")},
        })
    );
    // the block's signature is the throwaway key's: another trust refuses it
    let other = json!({"publishers": {TEST_KEY_ID: {"alg": "ed25519", "publisher": "app.buwiz", "public_key": b64(ed25519_dalek::SigningKey::from_bytes(&[1; 32]).verifying_key().as_bytes()), "first_party": true}}, "approved_plugins": trust["approved_plugins"]});
    let refused = broker(
        &bin,
        &["manifest", "package"],
        Some(&json!({"text": text, "trust": other, "platform": "macos-aarch64"})),
    );
    assert_ne!(refused["ok"], true, "{refused}");

    // a second platform's run keeps the first platform's binary
    std::fs::write(dir.join("buwiz-gws-linux-x86_64"), b"another binary").unwrap();
    plugin_json(
        &template_file,
        &dir,
        "linux-x86_64",
        "buwiz-gws-linux-x86_64",
        Some(&signed),
    );
    let both = read_json(&dir.join("plugin.unsigned.json"));
    let platforms: Vec<&String> = both["buwiz"]["binaries"]
        .as_object()
        .unwrap()
        .keys()
        .collect();
    assert_eq!(platforms, ["linux-x86_64", "macos-aarch64"]);
    assert!(
        both["buwiz"].get("signature").is_none(),
        "re-signed after the merge"
    );
    broker(
        &bin,
        &[
            "sign-manifest",
            &s(&dir.join("plugin.unsigned.json")),
            "--block",
            "buwiz",
            "--key-file",
            &s(&key),
            "--out",
            &s(&signed),
        ],
        None,
    );
    for (platform, asset) in [
        ("linux-x86_64", "buwiz-gws-linux-x86_64"),
        ("macos-aarch64", "buwiz-gws-macos-aarch64"),
    ] {
        let p = broker(
            &bin,
            &["manifest", "package"],
            Some(
                &json!({"text": std::fs::read_to_string(&signed).unwrap(), "trust": trust, "platform": platform}),
            ),
        );
        assert_eq!(p["value"]["binary"]["asset"], asset, "{p}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
