//! The manifest, built in code: Mail, Calendar and Drive, each an app of its
//! own with its own native screen, title and command.

use serde_json::{Value, json};

use crate::tools;

pub const ID: &str = "app.buwiz.google-workspace";
pub const SHORT: &str = "gws";
pub const TITLE: &str = "Google Workspace";
pub const RELEASE_KEY_ID: &str = "gc-2026-01";
/// One full screen: its app and everything it's granted. Each screen names its
/// own tools and no other's, so approving Calendar never grants a read of Gmail.
pub struct App {
    pub id: &'static str,
    pub title: &'static str,
    pub screen: &'static str,
    pub description: &'static str,
    pub tools: &'static [&'static str],
    /// Also shows as a card in a chat.
    pub card: bool,
    /// The person can hand it files.
    pub files: bool,
    /// Sites it may send the person to, after they agree.
    pub sites: &'static [&'static str],
}

pub const APPS: [App; 3] = [
    App {
        id: "mail",
        title: "Mail",
        screen: "gws.mail",
        description: "Your Google mail.",
        tools: &[
            tools::STATUS,
            tools::SEARCH_MAIL,
            tools::READ_MAIL,
            tools::MAIL_LABELS,
            tools::MAIL_LIST,
            tools::MAIL_THREAD,
            tools::MAIL_CHANGES,
            tools::MAIL_CARD,
            tools::MAIL_MARK,
            tools::MAIL_LABEL,
            tools::MAIL_ARCHIVE,
            tools::MAIL_TRASH,
            tools::MAIL_UNDO,
            tools::MAIL_DRAFT_SAVE,
            tools::MAIL_DRAFT_READ,
            tools::MAIL_DRAFT_DELETE,
            tools::MAIL_SEND,
            tools::MAIL_REPLY_START,
            tools::MAIL_JOB,
            tools::MAIL_JOB_START,
            tools::PEOPLE_SUGGEST,
            tools::MAIL_SEND_ARM,
            tools::MAIL_SEND_DISARM,
            tools::MAIL_DRAFT_OF,
            tools::UPLOAD_CHUNK,
            tools::UPLOAD_DROP,
        ],
        card: true,
        files: true,
        sites: &["https://console.cloud.google.com"],
    },
    App {
        id: "calendar",
        title: "Calendar",
        screen: "gws.calendar",
        description: "Your Google calendar.",
        tools: &[
            tools::STATUS,
            tools::CAL_LIST,
            tools::CAL_SHOW,
            tools::CAL_RANGE,
            tools::CAL_CHANGES,
            tools::CAL_NEXT,
            tools::CAL_PEOPLE,
            tools::EVENT_READ,
            tools::SEARCH_EVENTS,
            tools::EVENT_SAVE,
            tools::EVENT_DELETE,
            tools::EVENT_RESTORE,
            tools::EVENT_RSVP,
            tools::EVENT_DRAFT,
            tools::EVENT_DRAFT_READ,
        ],
        card: true,
        files: false,
        sites: &["https://meet.google.com"],
    },
    App {
        id: "drive",
        title: "Drive",
        screen: "gws.drive",
        description: "Your files in Google Drive.",
        tools: &[
            tools::STATUS,
            tools::DRIVE_LIST,
            tools::DRIVE_ABOUT,
            tools::DRIVE_CHANGES,
            tools::DRIVE_FILE,
            tools::DRIVE_PREVIEW,
            tools::DRIVE_CARD,
            tools::SEARCH_DRIVE,
            tools::READ_DOC_TEXT,
        ],
        card: true,
        files: false,
        sites: &[],
    },
];

fn app(a: &App) -> Value {
    let mut permissions = json!({ "tools": a.tools });
    let mut entry = json!({
        "id": a.id,
        "title": a.title,
        "description": a.description,
        "native": { "screen": a.screen, "abi": buwiz_plugin::manifest::NATIVE_ABI },
        "placement": ["full"],
    });
    if !a.sites.is_empty() || a.card {
        permissions["open"] =
            json!({ "apps": if a.card { vec![a.id] } else { Vec::new() }, "external": a.sites });
    }
    if a.card {
        entry["placement"] = json!(["full", "inline"]);
        entry["inline"] = json!({ "width": 420, "height": 96, "min_height": 96, "max_height": 480, "resizable": false, "wait_ready": true });
        entry["props"] = json!({ "type": "object", "properties": { "search": { "type": "string", "maxLength": 30 }, "thread": { "type": "string", "maxLength": 30 }, "draft": { "type": "string", "maxLength": 30 }, "view": { "type": "string", "maxLength": 30 }, "folder": { "type": "string", "maxLength": 256 }, "event": { "type": "object", "properties": { "calendar": { "type": "string", "maxLength": 512 }, "id": { "type": "string", "maxLength": 512 } } } } });
    }
    if a.files {
        permissions["pick_file"] =
            json!({ "max_bytes": crate::compose::ATTACHMENTS_MAX, "multiple": true });
    }
    entry["permissions"] = permissions;
    entry
}

/// The unsigned manifest under publisher key id `key_id`.
pub fn unsigned(key_id: &str) -> Value {
    json!({
        "protocol": buwiz_plugin::PROTOCOL,
        "id": ID,
        "short": SHORT,
        "version": env!("CARGO_PKG_VERSION"),
        "title": TITLE,
        "description": "Your Gmail in Buwiz Bot, with the Google account you sign in to.",
        "publisher": { "id": "app.buwiz", "key_id": key_id, "name": "Goldcoders" },
        "commands": [
            { "name": "gws", "description": "Open Mail, Calendar or Drive", "args": "[mail, calendar or drive]", "resolve": tools::RESOLVE },
            { "name": "mail", "description": "Open Mail", "args": "[inbox, unread, drafts, sent, important or starred]", "resolve": tools::RESOLVE },
            { "name": "gws-draft", "description": "Start an email for the assistant to draft", "resolve": tools::RESOLVE },
            { "name": "calendar", "description": "Open Calendar", "args": "[today, week, next or new]", "resolve": tools::RESOLVE },
            { "name": "gws-next", "description": "Show your next event", "resolve": tools::RESOLVE },
            { "name": "drive", "description": "Open Drive", "resolve": tools::RESOLVE },
            { "name": "file", "description": "Find a file in Drive", "args": "[name]", "resolve": tools::RESOLVE }
        ],
        "scopes": [
            { "name": "mail", "description": "Mailbox view", "resolve": tools::RESOLVE },
            { "name": "calendar", "description": "Calendar view", "resolve": tools::RESOLVE },
            { "name": "file", "description": "A file in Drive, by name", "resolve": tools::RESOLVE }
        ],
        "apps": APPS.iter().map(app).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tool an app's screen calls has to be in its manifest entry, or the
    /// Bot refuses the call. No write is left out of Mail's.
    #[test]
    fn mail_lists_every_tool_its_screen_calls() {
        let mail = APPS
            .iter()
            .find(|a| a.tools.contains(&tools::MAIL_LIST))
            .unwrap();
        let screen = include_str!("../../gws-screens/src/mail.rs");
        for def in tools::defs() {
            if screen.contains(&format!("\"{}\"", def.name)) {
                assert!(mail.tools.contains(&def.name), "{}", def.name);
            }
        }
    }

    #[test]
    fn calendar_lists_every_tool_its_screen_calls() {
        let calendar = APPS
            .iter()
            .find(|a| a.tools.contains(&tools::CAL_LIST))
            .unwrap();
        let screens = [
            include_str!("../../gws-screens/src/calendar.rs"),
            include_str!("../../gws-screens/src/calendar/cards.rs"),
            include_str!("../../gws-screens/src/calendar/detail.rs"),
            include_str!("../../gws-screens/src/calendar/editor.rs"),
            include_str!("../../gws-screens/src/calendar/event.rs"),
            include_str!("../../gws-screens/src/calendar/grid.rs"),
        ];
        for def in tools::defs() {
            let named = format!("\"{}\"", def.name);
            if screens.iter().any(|s| s.contains(&named)) {
                assert!(calendar.tools.contains(&def.name), "{}", def.name);
            }
        }
    }

    #[test]
    fn drive_lists_every_tool_its_screen_calls() {
        let drive = APPS
            .iter()
            .find(|a| a.tools.contains(&tools::DRIVE_LIST))
            .unwrap();
        let screens = [
            include_str!("../../gws-screens/src/drive.rs"),
            include_str!("../../gws-screens/src/drive/card.rs"),
            include_str!("../../gws-screens/src/drive/detail.rs"),
            include_str!("../../gws-screens/src/drive/file.rs"),
            include_str!("../../gws-screens/src/drive/list.rs"),
        ];
        for def in tools::defs() {
            let named = format!("\"{}\"", def.name);
            if screens.iter().any(|s| s.contains(&named)) {
                assert!(drive.tools.contains(&def.name), "{}", def.name);
            }
        }
    }

    #[test]
    fn the_manifest_names_only_real_tools_and_every_tool_has_an_audience() {
        let m = unsigned(RELEASE_KEY_ID);
        assert_eq!(
            buwiz_plugin::manifest::problems(&m, &tools::list()),
            Vec::<String>::new()
        );
        for d in tools::defs() {
            assert!(
                !d.audience.is_empty(),
                "{} must say who may call it: the model's list and the Hub read a missing audience in opposite ways",
                d.name
            );
            assert!(
                d.audience.iter().all(|a| ["agent", "app"].contains(a)),
                "{}",
                d.name
            );
        }
        // what writes to a calendar is the person's, and says it writes
        for name in [
            tools::EVENT_SAVE,
            tools::EVENT_DELETE,
            tools::EVENT_RESTORE,
            tools::EVENT_RSVP,
        ] {
            let def = tools::defs().into_iter().find(|d| d.name == name).unwrap();
            assert_eq!(def.audience, tools::APP_ONLY, "{name}");
            assert!(!def.read_only, "{name}");
        }
    }

    #[test]
    fn mail_calendar_and_drive_are_separate_screens() {
        let m = unsigned(RELEASE_KEY_ID);
        let apps = m["apps"].as_array().unwrap();
        let ids: Vec<&str> = apps.iter().map(|a| a["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["mail", "calendar", "drive"]);
        let screens: Vec<&str> = apps
            .iter()
            .map(|a| a["native"]["screen"].as_str().unwrap())
            .collect();
        assert_eq!(screens, ["gws.mail", "gws.calendar", "gws.drive"]);
    }

    #[test]
    fn only_mail_is_granted_mail_tools() {
        let m = unsigned(RELEASE_KEY_ID);
        let tools_of = |id: &str| {
            m["apps"]
                .as_array()
                .unwrap()
                .iter()
                .find(|a| a["id"] == id)
                .unwrap()["permissions"]["tools"]
                .clone()
        };
        assert!(
            tools_of("mail")
                .as_array()
                .unwrap()
                .contains(&json!("gws_read_mail"))
        );
        let calendar = tools_of("calendar");
        let calendar = calendar.as_array().unwrap();
        assert!(calendar.contains(&json!("gws_event_save")));
        assert!(
            !calendar
                .iter()
                .any(|t| t.as_str().unwrap().contains("mail") || t == "gws_people_suggest"),
            "approving Calendar grants no read of Gmail: {calendar:?}"
        );
        let drive = tools_of("drive");
        let drive = drive.as_array().unwrap();
        assert!(drive.contains(&json!("gws_drive_list")));
        assert!(
            !drive.iter().any(|t| {
                let t = t.as_str().unwrap();
                ["mail", "cal_", "event", "people", "upload"]
                    .iter()
                    .any(|other| t.contains(other))
            }),
            "approving Drive grants no read of Gmail or Calendar: {drive:?}"
        );
    }

    #[test]
    fn calendar_is_a_card_as_well_as_a_screen_and_has_every_calendar_tool() {
        let m = unsigned(RELEASE_KEY_ID);
        let app = m["apps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["id"] == "calendar")
            .unwrap();
        assert_eq!(app["placement"], json!(["full", "inline"]));
        assert_eq!(app["permissions"]["open"]["apps"], json!(["calendar"]));
        for d in tools::defs() {
            if ["gws_cal_", "gws_event_"]
                .iter()
                .any(|p| d.name.starts_with(p))
            {
                let held = app["permissions"]["tools"].as_array().unwrap();
                assert!(held.contains(&json!(d.name)), "{}", d.name);
            }
        }
        let scopes = m["scopes"].as_array().unwrap();
        assert!(scopes.iter().any(|s| s["name"] == "calendar"));
        let commands = m["commands"].as_array().unwrap();
        assert!(commands.iter().any(|c| c["name"] == "gws-next"));
    }

    #[test]
    fn drive_is_a_card_as_well_as_a_screen_and_has_every_drive_tool() {
        let m = unsigned(RELEASE_KEY_ID);
        let app = m["apps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["id"] == "drive")
            .unwrap();
        assert_eq!(app["placement"], json!(["full", "inline"]));
        assert_eq!(app["permissions"]["open"]["apps"], json!(["drive"]));
        let held = app["permissions"]["tools"].as_array().unwrap();
        for d in tools::defs() {
            if d.name.starts_with("gws_drive_")
                || [tools::SEARCH_DRIVE, tools::READ_DOC_TEXT].contains(&d.name)
            {
                assert!(held.contains(&json!(d.name)), "{}", d.name);
            }
        }
        assert!(app["props"]["properties"]["folder"].is_object());
        let named = |list: &str| {
            m[list]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["name"] == "file")
        };
        assert!(named("commands") && named("scopes"));
    }

    #[test]
    fn the_version_is_the_packages() {
        assert_eq!(
            unsigned(RELEASE_KEY_ID)["version"],
            env!("CARGO_PKG_VERSION")
        );
    }
}
