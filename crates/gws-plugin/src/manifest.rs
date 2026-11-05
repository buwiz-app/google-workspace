//! The manifest, built in code: Mail, Calendar and Drive, each an app of its
//! own with its own native screen, title and command.

use serde_json::{Value, json};

use crate::tools;

pub const ID: &str = "app.buwiz.google-workspace";
pub const SHORT: &str = "gws";
pub const TITLE: &str = "Google Workspace";
pub const RELEASE_KEY_ID: &str = "gc-2026-01";
/// `(app id, title, native screen, description)`: one full screen each.
pub const APPS: [(&str, &str, &str, &str); 3] = [
    ("mail", "Mail", "gws.mail", "Your Google mail."),
    ("calendar", "Calendar", "gws.calendar", "Your Google calendar."),
    ("drive", "Drive", "gws.drive", "Your files in Google Drive."),
];

/// The tools each screen calls. Each gets its own and no other, so approving
/// Calendar never grants a read of Gmail.
fn screen_tools(id: &str) -> Vec<&'static str> {
    let mut own = vec![tools::STATUS];
    if id == "mail" {
        own.extend([tools::SEARCH_MAIL, tools::READ_MAIL]);
    }
    own
}

fn app(&(id, title, screen, description): &(&str, &str, &str, &str)) -> Value {
    json!({
        "id": id,
        "title": title,
        "description": description,
        "native": { "screen": screen, "abi": buwiz_plugin::manifest::NATIVE_ABI },
        "placement": ["full"],
        "permissions": { "tools": screen_tools(id) },
    })
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
            { "name": "gws", "description": "Open Mail, Calendar or Drive", "args": "[mail, calendar or drive]", "resolve": tools::RESOLVE },
            { "name": "mail", "description": "Open Mail", "resolve": tools::RESOLVE },
            { "name": "calendar", "description": "Open Calendar", "resolve": tools::RESOLVE },
            { "name": "drive", "description": "Open Drive", "resolve": tools::RESOLVE }
        ],
        "apps": APPS.iter().map(app).collect::<Vec<_>>(),
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
    fn mail_calendar_and_drive_are_separate_screens() {
        let m = unsigned(RELEASE_KEY_ID);
        let apps = m["apps"].as_array().unwrap();
        let ids: Vec<&str> = apps.iter().map(|a| a["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["mail", "calendar", "drive"]);
        let screens: Vec<&str> = apps.iter().map(|a| a["native"]["screen"].as_str().unwrap()).collect();
        assert_eq!(screens, ["gws.mail", "gws.calendar", "gws.drive"]);
    }

    #[test]
    fn only_mail_is_granted_mail_tools() {
        let m = unsigned(RELEASE_KEY_ID);
        let tools_of = |id: &str| m["apps"].as_array().unwrap().iter().find(|a| a["id"] == id).unwrap()["permissions"]["tools"].clone();
        assert_eq!(tools_of("mail"), json!(["gws_status", "gws_search_mail", "gws_read_mail"]));
        assert_eq!(tools_of("calendar"), json!(["gws_status"]));
        assert_eq!(tools_of("drive"), json!(["gws_status"]));
    }

    #[test]
    fn the_version_is_the_packages() {
        assert_eq!(unsigned(RELEASE_KEY_ID)["version"], env!("CARGO_PKG_VERSION"));
    }
}
