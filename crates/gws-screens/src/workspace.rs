//! The window: a rail of pages and the page. The pages come in later slices;
//! here the window opens and says whether a Google account is signed in.

use buwiz_app::Bridge;
use nsh_core::element::{Element, Interactive as _, ParentElement, Root, dynamic, signal, text, view};
use serde_json::{Value, json};

const BG: &str = "bg-[#f6f6f4] dark:bg-[#151515]";
const RAIL: &str = "bg-[#efefec] dark:bg-[#0f0f0f]";
const INK: &str = "text-[#1c1c1c] dark:text-[#f0f0f0]";
const MUTE: &str = "text-[#5c5c5c] dark:text-[#a0a0a0]";

const PAGES: [(&str, &str); 3] = [("mail", "Mail"), ("calendar", "Calendar"), ("drive", "Drive")];

#[derive(Clone, PartialEq)]
enum Account {
    Loading,
    SignedIn,
    SignedOut,
    Failed(String),
}

pub fn build(bridge: Bridge) -> Root {
    buwiz_app::root(bridge, |b| {
        let props = b.props();
        let start = props.get("page").and_then(Value::as_str).unwrap_or("mail").to_owned();
        let page = signal(start);
        let account = signal(Account::Loading);
        let reader = b.clone();
        b.spawn(async move {
            match reader.call("gws_status", json!({})).await {
                Ok(v) if v.get("signed_in") == Some(&Value::Bool(true)) => account.set(Account::SignedIn),
                Ok(_) => account.set(Account::SignedOut),
                Err(e) => account.set(Account::Failed(e.message)),
            }
        });
        let rail = PAGES.iter().fold(view().app_class(Some(format!("w-[68px] gap-1 pt-3 items-center {RAIL}").into())), |rail, (id, label)| {
            let id = (*id).to_owned();
            rail.child(view().app_class(Some("px-2 py-2".into())).on_press(move || page.set(id.clone())).child(text(*label).app_class(Some(format!("text-[11px] {INK}").into()))))
        });
        let body = dynamic(move || {
            let line = match account.get() {
                Account::Loading => "Loading…".to_owned(),
                Account::SignedIn => "Your Google account is connected.".to_owned(),
                Account::SignedOut => "Connect your Google account on this app's page.".to_owned(),
                Account::Failed(message) => message,
            };
            view().app_class(Some("flex-1 items-center justify-center".into())).child(text(line).app_class(Some(format!("text-[14px] {MUTE}").into())))
        });
        view().app_class(Some(format!("flex-1 flex-row {BG}").into())).child(rail).child(body)
    })
}
