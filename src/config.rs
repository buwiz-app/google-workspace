//! What the install gives the plugin: `GWS_MANIFEST`, the signed manifest
//! it serves (plugin.json sets it to `${PLUGIN_ROOT}/manifest.signed.json`).

use std::path::PathBuf;

/// The signed manifest: `GWS_MANIFEST`, else `buwiz-gws.manifest.json`
/// beside this binary (where the app bundle puts the signed copy).
pub fn manifest_path() -> PathBuf {
    match std::env::var("GWS_MANIFEST") {
        Ok(p) if !p.is_empty() => PathBuf::from(p),
        _ => std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join("buwiz-gws.manifest.json"))).unwrap_or_else(|| PathBuf::from("buwiz-gws.manifest.json")),
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
