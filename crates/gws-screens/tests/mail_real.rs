//! The Mail screen's Save on the real plugin and the stub Google, with no fake
//! in between: the press goes through the tool the screen names, and what the
//! Bot is handed to copy holds the file's bytes. `mail_ui.rs` fakes the tools,
//! so this is what says the two sides still agree, against the stub.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use buwiz_app::{Bridge, BuwizError, Done, Host, Listener};
use gws::Core;
use gws::apis::Apis;
use gws_fake_google::{FAKE_EMAIL, FakeGoogle, Mail, Seed};
use nsh_core::prelude::*;
use nsh_engine::EngineConfig;
use nsh_testing::Headless;
use serde_json::{Map, Value, json};

const WAIT: Duration = Duration::from_secs(10);
const PDF: &[u8] = b"%PDF-1.4\n%a receipt\n";

/// The Bot, as far as a person's press goes, keeping each whole answer.
struct Real {
    core: Core,
    answers: Mutex<Vec<(String, Value)>>,
}

impl Host for Real {
    fn params(&self) -> Map<String, Value> {
        let p = json!({
            "props": {}, "app": {"plugin": "app.buwiz.google-workspace", "app": "mail"},
            "granted": ["buwiz.call", "buwiz.open"],
            "viewport": {"width": 1000, "height": 600, "placement": "full"},
        });
        p.as_object().unwrap().clone()
    }

    fn post(&self, _: &str, _: Value) {}

    fn call(&self, method: &str, data: Value, done: Done) {
        if method != "buwiz.call" {
            return done(Ok(Value::Null));
        }
        let tool = data["tool"].as_str().unwrap_or_default();
        let answer = self.core.run(tool, &data["args"]);
        self.answers
            .lock()
            .unwrap()
            .push((tool.to_owned(), answer.clone()));
        if answer["isError"] == true {
            let words = answer["content"][0]["text"].as_str().unwrap_or_default();
            return done(Err(BuwizError::new("tool_error", false, words)));
        }
        done(Ok(answer["structuredContent"].clone()));
    }

    fn listen(&self, _: Listener) {}

    fn fail(&self, message: &str) {
        panic!("the screen failed: {message}");
    }
}

fn settle(app: &mut Headless) {
    app.wait(Duration::from_millis(300)).expect("time passes");
}

#[test]
fn save_hands_the_bot_the_files_bytes() {
    let google = FakeGoogle::start(Seed::default());
    google.deliver(Mail {
        id: "18f1aa".to_owned(),
        thread: "18f1aa".to_owned(),
        labels: vec!["INBOX".to_owned()],
        from: "Ana Reyes <ana@example.com>".to_owned(),
        to: FAKE_EMAIL.to_owned(),
        subject: "The receipt".to_owned(),
        at: 1_800_000_000_000,
        text: "Here.".to_owned(),
        html: None,
        attachments: vec![(
            "receipt.pdf".to_owned(),
            "application/pdf".to_owned(),
            PDF.to_vec(),
        )],
        message_id: "<a@example.com>".to_owned(),
        inline: Vec::new(),
    });
    let dir = std::env::temp_dir().join(format!("gws-mail-real-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let core = Core::new(
        &Apis::at(&google.base()),
        Ok(gws_store::Paths::in_dir(&dir)),
    );
    core.unlock("seed");
    core.accounts.set(&json!({"account": "google", "label": "default", "provider": "google", "subject": FAKE_EMAIL, "scopes": ["https://www.googleapis.com/auth/gmail.modify"], "access_token": "fake-access-1", "token_type": "Bearer", "expires_at": 4_000_000_000u64}));
    // a Bot that can save
    core.documents.heard(&json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": "2025-11-25", "capabilities": {"experimental": {"buwiz/documents": ["save"]}}}}));
    let real = Arc::new(Real {
        core,
        answers: Mutex::default(),
    });
    let def = gws_screens::screens()
        .iter()
        .find(|s| s.id == "gws.mail")
        .expect("Mail");
    let root = (def.build)(Bridge::new(real.clone()));
    let mut app = Headless::start(EngineConfig::new(root)).expect("the engine starts");
    app.set_viewport(size(px(1000.0), px(600.0)))
        .expect("the viewport sets");
    app.wait_for_within("text:The receipt", WAIT)
        .unwrap_or_else(|e| panic!("the inbox: {e}"));
    app.press("text:The receipt").expect("the row presses");
    app.wait_for_within("label:Save this attachment", WAIT)
        .unwrap_or_else(|e| panic!("the card: {e}: {:?}", app.screen().texts()));
    app.press("label:Save this attachment")
        .expect("Save presses");
    app.wait_for_within("text:Saving to Downloads…", WAIT)
        .unwrap_or_else(|e| panic!("saving: {e}: {:?}", app.screen().texts()));
    settle(&mut app);
    let answers = real.answers.lock().unwrap();
    let (_, saved) = answers
        .iter()
        .find(|(t, _)| t == "gws_mail_attachment_save")
        .expect("the press called Save");
    let handed = &saved["_meta"]["buwiz/save"];
    assert_eq!(handed["name"], "receipt.pdf", "{saved}");
    let path = handed["path"].as_str().unwrap();
    assert_eq!(std::fs::read(path).unwrap(), PDF);
    drop(answers);
    let _ = std::fs::remove_dir_all(&dir);
}
