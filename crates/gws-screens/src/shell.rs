//! What every screen starts from: it says whether this feature works with the
//! signed-in account, which is more than being signed in. Each screen's own
//! content comes in its slice.

use buwiz_app::Bridge;
use nsh_core::element::{Element, ParentElement, Root, dynamic, signal, text, view};
use serde_json::{Value, json};

const BG: &str = "bg-[#f6f6f4] dark:bg-[#151515]";
const MUTE: &str = "text-[#5c5c5c] dark:text-[#a0a0a0]";

#[derive(Clone, PartialEq)]
enum Account {
    Loading,
    Covered,
    NotCovered,
    SignedOut,
    Failed(String),
}

/// What `gws_status` says of the feature `name`.
fn account_of(status: &Value, name: &str) -> Account {
    if status.get("signed_in") != Some(&Value::Bool(true)) {
        return Account::SignedOut;
    }
    if status.pointer(&format!("/features/{}", name.to_lowercase())) == Some(&Value::Bool(true)) { Account::Covered } else { Account::NotCovered }
}

fn line_of(account: Account, name: &str) -> String {
    match account {
        Account::Loading => "Loading…".to_owned(),
        Account::Covered => format!("{name} is connected to your Google account."),
        Account::NotCovered => format!("Your Google sign-in doesn't cover {name} yet."),
        Account::SignedOut => "Connect your Google account on this app's page.".to_owned(),
        Account::Failed(message) => message,
    }
}

pub fn build(bridge: Bridge, name: &'static str) -> Root {
    buwiz_app::root(bridge, move |b| {
        let account = signal(Account::Loading);
        let reader = b.clone();
        b.spawn(async move {
            match reader.call("gws_status", json!({})).await {
                Ok(v) => account.set(account_of(&v, name)),
                Err(e) => account.set(Account::Failed(e.message)),
            }
        });
        let body = dynamic(move || {
            let line = line_of(account.get(), name);
            view().app_class(Some("flex-1 items-center justify-center".into())).child(text(line).app_class(Some(format!("text-[14px] {MUTE}").into())))
        });
        view().app_class(Some(format!("flex-1 {BG}").into())).child(body)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_screen_says_connected_only_when_the_sign_in_covers_it() {
        let status = json!({"signed_in": true, "features": {"mail": true, "calendar": false, "drive": false}});
        assert_eq!(line_of(account_of(&status, "Mail"), "Mail"), "Mail is connected to your Google account.");
        assert_eq!(line_of(account_of(&status, "Calendar"), "Calendar"), "Your Google sign-in doesn't cover Calendar yet.");
        assert_eq!(line_of(account_of(&status, "Drive"), "Drive"), "Your Google sign-in doesn't cover Drive yet.");
        assert!(account_of(&json!({"signed_in": false}), "Mail") == Account::SignedOut);
        assert!(account_of(&json!({"signed_in": true}), "Mail") == Account::NotCovered, "no word is no");
    }
}
