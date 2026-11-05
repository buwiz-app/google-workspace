//! What every screen starts from: it says whether a Google account is
//! signed in. Each screen's own content comes in its slice.

use buwiz_app::Bridge;
use nsh_core::element::{Element, ParentElement, Root, dynamic, signal, text, view};
use serde_json::{Value, json};

const BG: &str = "bg-[#f6f6f4] dark:bg-[#151515]";
const MUTE: &str = "text-[#5c5c5c] dark:text-[#a0a0a0]";

#[derive(Clone, PartialEq)]
enum Account {
    Loading,
    SignedIn,
    SignedOut,
    Failed(String),
}

pub fn build(bridge: Bridge, name: &'static str) -> Root {
    buwiz_app::root(bridge, move |b| {
        let account = signal(Account::Loading);
        let reader = b.clone();
        b.spawn(async move {
            match reader.call("gws_status", json!({})).await {
                Ok(v) if v.get("signed_in") == Some(&Value::Bool(true)) => account.set(Account::SignedIn),
                Ok(_) => account.set(Account::SignedOut),
                Err(e) => account.set(Account::Failed(e.message)),
            }
        });
        let body = dynamic(move || {
            let line = match account.get() {
                Account::Loading => "Loading…".to_owned(),
                Account::SignedIn => format!("{name} is connected to your Google account."),
                Account::SignedOut => "Connect your Google account on this app's page.".to_owned(),
                Account::Failed(message) => message,
            };
            view().app_class(Some("flex-1 items-center justify-center".into())).child(text(line).app_class(Some(format!("text-[14px] {MUTE}").into())))
        });
        view().app_class(Some(format!("flex-1 {BG}").into())).child(body)
    })
}
