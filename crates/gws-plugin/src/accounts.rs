//! The Google account Buwiz Bot signs the person in to (protocol 1.0 §20),
//! shared with every tool's thread. The Bot pushes the access token with
//! `buwiz.account.set` and takes it back with `buwiz.account.clear`. The
//! token lives in memory only: never logged, written or echoed.
//!
//! A push from a Bot that has it also lists every signed-in label. One on that
//! list is still the person's after a clear: that's a switch, not a sign-out, so
//! its mail and its unsaved work stay. The Bot does the switching:
//! `gws_account_use` only asks, in `_meta["buwiz/account_use"]`.

use std::collections::BTreeMap;
use std::sync::Mutex;

use serde_json::{Value, json};

use crate::tools::{ACCOUNT_USE, ACCOUNTS, fail, ok};
use crate::{Core, compose, js, mail};

/// The account the tools use (plugin.json's `buwiz.accounts[0].id`).
pub const GOOGLE: &str = "google";

/// The pushed accounts.
#[derive(Default)]
pub struct Accounts(Mutex<Held>);

/// The SDK's accounts, and who each label's sign-in is, live or expired: an
/// expired token is still someone's mail on this Mac, and the SDK can't list
/// what it holds.
#[derive(Default)]
struct Held {
    tokens: buwiz_plugin::Accounts,
    who: BTreeMap<String, String>,
    /// The Bot's list, as its last push had it. `None` from a Bot with no list.
    listed: Option<Vec<Listed>>,
}

/// A signed-in label on the Bot's list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listed {
    pub label: String,
    pub active: bool,
}

/// What a push's `accounts` say of the Google account.
fn listed_in(args: &Value) -> Option<Vec<Listed>> {
    let rows = args.get("accounts")?.as_array()?;
    let google = rows
        .iter()
        .filter(|r| r.get("account").and_then(Value::as_str) == Some(GOOGLE));
    Some(
        google
            .filter_map(|r| {
                Some(Listed {
                    label: r.get("label")?.as_str()?.to_owned(),
                    active: r.get("active") == Some(&Value::Bool(true)),
                })
            })
            .collect(),
    )
}

impl Held {
    fn lists(&self, label: &str) -> bool {
        self.listed
            .as_ref()
            .is_some_and(|l| l.iter().any(|l| l.label == label))
    }
}

/// `gws_accounts`: the labels to choose from. The addresses are for the person,
/// so this tool, like `gws_status`, is never the model's.
fn list(core: &Core) -> Value {
    let filed: BTreeMap<String, String> = mail::with_store(core, |s| s.sign_ins())
        .unwrap_or_default()
        .into_iter()
        .collect();
    let rows = core.accounts.roster().unwrap_or_default();
    let view = |(l, who): (Listed, Option<String>)| {
        let address = who.or_else(|| filed.get(&l.label).cloned());
        json!({"label": l.label, "address": address, "active": l.active})
    };
    ok(json!({"accounts": rows.into_iter().map(view).collect::<Vec<_>>()}))
}

/// `gws_account_use`: asks the Bot to make a listed label the active one. It
/// changes nothing here, and the Bot takes it only from the person's own press.
fn use_one(core: &Core, args: &Value) -> Value {
    let label = js::string_or_empty(args.get("label"));
    let rows = core.accounts.roster().unwrap_or_default();
    let Some((listed, _)) = rows.iter().find(|(l, _)| l.label == label) else {
        return fail("not_found", "That account isn't signed in here.");
    };
    if listed.active {
        return ok(json!({"switched": false}));
    }
    // a Send in its hold goes out on the account it was pressed on, or not at all
    if compose::in_hold(core) {
        return fail("busy", "A message is still sending. Try again in a moment.");
    }
    let mut answer = ok(json!({"switched": true}));
    answer["_meta"] = json!({"buwiz/account_use": {"account": GOOGLE, "label": label}});
    answer
}

/// Runs an account tool; `None` for any other tool.
pub fn run(core: &Core, tool: &str, args: &Value) -> Option<Value> {
    match tool {
        ACCOUNTS => Some(list(core)),
        ACCOUNT_USE => Some(use_one(core, args)),
        _ => None,
    }
}

/// The refusal of a write a screen made for an account that's no longer the
/// active one: it would land on the wrong person's Google account. A write that
/// names no account, and any with nobody signed in, goes on to its own answer.
pub fn stale(core: &Core, args: &Value) -> Option<Value> {
    let wanted = args.get("as_account").and_then(Value::as_str)?;
    let (now, _) = core.accounts.signed_in()?;
    (now != wanted).then(|| {
        fail(
            "account_changed",
            "You switched Google accounts, so this wasn't saved. Switch back to save it.",
        )
    })
}

/// The `Authorization` value for a request.
pub struct Bearer(String);

impl Bearer {
    pub fn header(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Bearer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Bearer(…)")
    }
}

impl Accounts {
    fn held(&self) -> std::sync::MutexGuard<'_, Held> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `buwiz.account.set`'s arguments.
    pub fn set(&self, args: &Value) -> Value {
        self.update(args, |tokens, args| tokens.set(args))
    }

    /// `buwiz.account.clear`'s arguments.
    pub fn clear(&self, args: &Value) -> Value {
        self.update(args, |tokens, args| tokens.clear(args))
    }

    fn update(
        &self,
        args: &Value,
        apply: impl FnOnce(&mut buwiz_plugin::Accounts, &serde_json::Map<String, Value>) -> Value,
    ) -> Value {
        let mut held = self.held();
        let result = apply(
            &mut held.tokens,
            args.as_object().unwrap_or(&serde_json::Map::new()),
        );
        // a refused push says nothing about who is signed in
        if result["isError"] != Value::Bool(true)
            && let Some(listed) = listed_in(args)
        {
            held.listed = Some(listed);
        }
        if let Some(label) = args.get("label").and_then(Value::as_str) {
            let now = held
                .tokens
                .labelled(GOOGLE, label)
                .map(|a| a.subject.clone());
            match now {
                Some(who) => {
                    held.who.insert(label.to_owned(), who);
                }
                // only this push's own list makes a clear a switch: an old one
                // can't say this label is still signed in
                None if args.get("accounts").is_some() && held.lists(label) => {}
                None => {
                    held.who.remove(label);
                    if let Some(listed) = &mut held.listed {
                        listed.retain(|l| l.label != label);
                    }
                }
            }
        }
        result
    }

    /// Forgets every token (`buwiz.uninstall`).
    pub fn forget(&self) {
        let mut held = self.held();
        held.tokens.forget();
        held.who.clear();
        held.listed = None;
    }

    /// The Bot's list, each label with whose it is if this run has seen that.
    /// `None` from a Bot with no list.
    pub fn roster(&self) -> Option<Vec<(Listed, Option<String>)>> {
        let held = self.held();
        let listed = held.listed.as_ref()?;
        let known = |l: &Listed| held.who.get(&l.label).cloned();
        Some(listed.iter().map(|l| (l.clone(), known(l))).collect())
    }

    /// Whether any sign-in is `subject`'s, live or expired.
    pub fn holds(&self, subject: &str) -> bool {
        self.held().who.values().any(|who| who == subject)
    }

    /// Whose sign-in `args` name, if it's one held here.
    pub fn subject_of(&self, args: &Value) -> Option<String> {
        let label = args.get("label").and_then(Value::as_str)?;
        self.held().who.get(label).cloned()
    }

    /// Whether anyone is signed in, live or expired.
    pub fn any(&self) -> bool {
        !self.held().who.is_empty()
    }

    /// The live token and whose it is, read together: from two reads a switch
    /// in between would file one account's mail under another.
    pub fn session(&self) -> Option<(Bearer, String)> {
        self.held()
            .tokens
            .get(GOOGLE)
            .map(|a| (Bearer(a.authorization()), a.subject.clone()))
    }

    /// The Google account's live token.
    pub fn google(&self) -> Option<Bearer> {
        self.held()
            .tokens
            .get(GOOGLE)
            .map(|a| Bearer(a.authorization()))
    }

    /// Who is signed in and what the sign-in covers.
    pub fn signed_in(&self) -> Option<(String, Vec<String>)> {
        self.held()
            .tokens
            .get(GOOGLE)
            .map(|a| (a.subject.clone(), a.scopes.clone()))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn a_token_and_its_owner_are_read_together() {
        let sign_in = |who: &str| json!({"account": "google", "label": "default", "provider": "google", "subject": who, "scopes": [], "access_token": who, "token_type": "Bearer", "expires_at": 4_000_000_000u64});
        let accounts = Accounts::default();
        accounts.set(&sign_in("a"));
        let done = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|s| {
            s.spawn(|| {
                for n in 0..200_000 {
                    accounts.set(&sign_in(if n % 2 == 0 { "b" } else { "a" }));
                }
                done.store(true, std::sync::atomic::Ordering::Relaxed);
            });
            while !done.load(std::sync::atomic::Ordering::Relaxed) {
                let (auth, who) = accounts.session().expect("always signed in");
                assert!(
                    auth.header().ends_with(&who),
                    "{who}'s mail under another token"
                );
            }
        });
    }

    use super::*;

    const TOKEN: &str = "ya29.a-secret-token";

    fn set(a: &Accounts, label: &str, token: &str, expires_at: Value) -> Value {
        a.set(&json!({"account": "google", "label": label, "provider": "google", "subject": "owner@example.com", "scopes": ["https://www.googleapis.com/auth/gmail.readonly"], "access_token": token, "token_type": "Bearer", "expires_at": expires_at}))
    }

    fn in_an_hour() -> Value {
        json!(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
                + 3600
        )
    }

    #[test]
    fn set_holds_the_token_and_clear_drops_it() {
        let a = Accounts::default();
        assert!(a.google().is_none());
        let r = set(&a, "default", TOKEN, in_an_hour());
        assert!(
            !r.to_string().contains(TOKEN),
            "the answer never echoes the token"
        );
        assert_eq!(a.google().unwrap().header(), format!("Bearer {TOKEN}"));
        assert_eq!(
            format!("{:?}", a.google().unwrap()),
            "Bearer(…)",
            "Debug never shows it"
        );
        a.clear(&json!({"account": "google", "label": "default"}));
        assert!(a.google().is_none());
        // clearing what is not there is fine
        a.clear(&json!({"account": "google", "label": "other"}));
    }

    #[test]
    fn a_refresh_replaces_the_token_and_an_expired_one_is_missing() {
        let a = Accounts::default();
        set(&a, "default", "old", json!(1000));
        assert!(a.google().is_none(), "expired");
        set(&a, "default", "new", in_an_hour());
        assert_eq!(a.google().unwrap().header(), "Bearer new");
    }

    #[test]
    fn another_label_serves_when_default_is_missing_and_another_accounts_token_never_does() {
        let a = Accounts::default();
        a.set(&json!({"account": "github", "label": "default", "access_token": "x"}));
        assert!(
            a.google().is_none(),
            "another account's token is not Google's"
        );
        set(&a, "work", "w", in_an_hour());
        assert_eq!(a.google().unwrap().header(), "Bearer w");
    }

    #[test]
    fn a_set_without_its_fields_is_refused_without_echoing() {
        let a = Accounts::default();
        for args in [
            json!({}),
            json!({"account": "google", "label": "default"}),
            json!({"account": "google", "access_token": TOKEN}),
            json!({"label": "x", "access_token": TOKEN}),
        ] {
            let r = a.set(&args);
            assert_eq!(r["isError"], true);
            assert!(!r.to_string().contains(TOKEN));
        }
        assert!(a.google().is_none());
    }
}
