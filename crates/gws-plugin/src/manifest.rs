//! The manifest, built in code: one native app, `workspace`, and the `/gws` command.

use serde_json::{Value, json};

use crate::tools;

pub const ID: &str = "app.buwiz.google-workspace";
pub const SHORT: &str = "gws";
pub const TITLE: &str = "Google Workspace";
pub const RELEASE_KEY_ID: &str = "gc-2026-01";
/// The app, and the native screen it runs (`gws-screens`).
pub const APP: &str = "workspace";
pub const SCREEN: &str = "gws.workspace";

/// The tools the screen calls.
fn screen_tools() -> Vec<&'static str> {
    tools::defs().into_iter().filter(|d| d.name != tools::RESOLVE).map(|d| d.name).collect()
}

/// The unsigned manifest under publisher key id `key_id`.
pub fn unsigned(key_id: &str) -> Value {
    json!({
        "protocol": buwiz_plugin::PROTOCOL,
        "id": ID,
        "short": SHORT,
        "version": env!("CARGO_PKG_VERSION"),
        "title": TITLE,
        "description": "Mail, Calendar and Drive in Buwiz Bot, with the Google account you sign in to.",
        "publisher": { "id": "app.buwiz", "key_id": key_id, "name": "Goldcoders" },
        "commands": [
            { "name": "gws", "description": "Open Mail, Calendar or Drive", "args": "[mail, calendar or drive]", "resolve": tools::RESOLVE }
        ],
        "apps": [{
            "id": APP,
            "title": TITLE,
            "description": "Your Google mail, calendar and files.",
            "native": { "screen": SCREEN, "abi": buwiz_plugin::manifest::NATIVE_ABI },
            "props": {
                "type": "object",
                "properties": { "page": { "type": "string", "enum": ["mail", "calendar", "drive"] } }
            },
            "placement": ["full"],
            "permissions": { "tools": screen_tools() },
        }],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_manifest_names_only_real_tools_and_every_tool_has_an_audience() {
        let m = unsigned(RELEASE_KEY_ID);
        assert_eq!(buwiz_plugin::manifest::problems(&m, &tools::list()), Vec::<String>::new());
        for d in tools::defs() {
            assert!(!d.audience.is_empty(), "{} must say who may call it: the model's list and the Hub read a missing audience in opposite ways", d.name);
            assert!(d.audience.iter().all(|a| ["agent", "app"].contains(a)), "{}", d.name);
        }
    }

    #[test]
    fn the_version_is_the_packages() {
        assert_eq!(unsigned(RELEASE_KEY_ID)["version"], env!("CARGO_PKG_VERSION"));
    }
}
