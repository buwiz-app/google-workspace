//! The Google account Buwiz Bot signs the user in to (extension format 2.1
//! §4). The Bot pushes its access token with `buwiz.account.set` after
//! provision, after sign-in and after each refresh, and takes it back with
//! `buwiz.account.clear` after sign-out, removal or a failed refresh. The
//! token lives in memory only: never logged, written or echoed.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::tools::{fail, ok};

/// The account the mail tools read (plugin.json's `buwiz.accounts[0].id`).
pub const GOOGLE: &str = "google";
/// The label the Bot gives the first sign-in of an account.
const DEFAULT_LABEL: &str = "default";

/// A pushed token.
struct Held {
    access_token: String,
    token_type: String,
    /// unix seconds; `None`: the Bot sent none
    expires_at: Option<f64>,
}

/// The pushed accounts, by `(account, label)`.
#[derive(Default)]
pub struct Accounts(Mutex<BTreeMap<(String, String), Held>>);

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

fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

fn text(args: &Value, k: &str) -> Option<String> {
    args.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned)
}

impl Accounts {
    fn held(&self) -> std::sync::MutexGuard<'_, BTreeMap<(String, String), Held>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `buwiz.account.set {account, label, provider, subject, scopes,
    /// access_token, token_type, expires_at}` → `{ok: true}`.
    pub fn set(&self, args: &Value) -> Value {
        let (Some(account), Some(label), Some(access_token)) = (text(args, "account"), text(args, "label"), text(args, "access_token")) else {
            return fail("invalid_args", "send {account, label, access_token, …}");
        };
        let token_type = text(args, "token_type").unwrap_or_else(|| "Bearer".to_owned());
        let expires_at = args.get("expires_at").and_then(Value::as_f64);
        self.held().insert((account, label), Held { access_token, token_type, expires_at });
        ok(json!({"ok": true}))
    }

    /// `buwiz.account.clear {account, label}` → `{ok: true}`.
    pub fn clear(&self, args: &Value) -> Value {
        let (account, label) = (text(args, "account").unwrap_or_default(), text(args, "label").unwrap_or_default());
        self.held().remove(&(account, label));
        ok(json!({"ok": true}))
    }

    /// The Google account's token, unexpired: the `default` label's, else
    /// the first label's.
    pub fn google(&self) -> Option<Bearer> {
        self.google_at(now())
    }

    fn google_at(&self, at: f64) -> Option<Bearer> {
        let held = self.held();
        let live = |h: &&Held| h.expires_at.is_none_or(|e| e > at);
        let h = held.get(&(GOOGLE.to_owned(), DEFAULT_LABEL.to_owned())).filter(live).or_else(|| held.iter().filter(|((a, _), _)| a == GOOGLE).map(|(_, h)| h).find(live))?;
        // RFC 6750: the scheme is case-insensitive; Google's is "Bearer"
        let scheme = if h.token_type.eq_ignore_ascii_case("bearer") { "Bearer" } else { h.token_type.as_str() };
        Some(Bearer(format!("{scheme} {}", h.access_token)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "ya29.a-secret-token";

    fn set(a: &Accounts, label: &str, token: &str, expires_at: Value) -> Value {
        a.set(&json!({"account": "google", "label": label, "provider": "google", "subject": "owner@example.com", "scopes": ["https://www.googleapis.com/auth/gmail.readonly"], "access_token": token, "token_type": "Bearer", "expires_at": expires_at}))
    }

    #[test]
    fn set_holds_the_token_and_clear_drops_it() {
        let a = Accounts::default();
        assert!(a.google().is_none());
        let r = set(&a, "default", TOKEN, json!(now() + 3600.0));
        assert_eq!(r, ok(json!({"ok": true})));
        assert!(!r.to_string().contains(TOKEN), "the answer never echoes the token");
        assert_eq!(a.google().unwrap().header(), format!("Bearer {TOKEN}"));
        assert_eq!(format!("{:?}", a.google().unwrap()), "Bearer(…)", "Debug never shows it");
        assert_eq!(a.clear(&json!({"account": "google", "label": "default"})), ok(json!({"ok": true})));
        assert!(a.google().is_none());
        // clearing what is not there is fine
        assert_eq!(a.clear(&json!({"account": "google", "label": "other"})), ok(json!({"ok": true})));
    }

    #[test]
    fn a_refresh_replaces_the_token_and_an_expired_one_is_missing() {
        let a = Accounts::default();
        set(&a, "default", "old", json!(1000));
        assert!(a.google_at(999.0).is_some());
        assert!(a.google_at(1000.0).is_none(), "expired at expires_at");
        set(&a, "default", "new", json!(5000));
        assert_eq!(a.google_at(1000.0).unwrap().header(), "Bearer new");
        set(&a, "default", "forever", Value::Null);
        assert!(a.google_at(1e12).is_some(), "no expiry sent: held until cleared");
    }

    #[test]
    fn another_label_serves_when_default_is_missing_or_expired() {
        let a = Accounts::default();
        set(&a, "work", "w", json!(5000));
        assert_eq!(a.google_at(1.0).unwrap().header(), "Bearer w");
        set(&a, "default", "d", json!(10));
        assert_eq!(a.google_at(1.0).unwrap().header(), "Bearer d");
        assert_eq!(a.google_at(100.0).unwrap().header(), "Bearer w");
        a.set(&json!({"account": "github", "label": "default", "access_token": "x"}));
        a.clear(&json!({"account": "google", "label": "work"}));
        assert!(a.google_at(100.0).is_none(), "another account's token is not Google's");
    }

    #[test]
    fn a_set_without_its_fields_is_refused_without_echoing() {
        let a = Accounts::default();
        for args in [json!({}), json!({"account": "google", "label": "default"}), json!({"account": "google", "access_token": TOKEN}), json!({"label": "x", "access_token": TOKEN})] {
            let r = a.set(&args);
            assert_eq!(r["isError"], true);
            assert_eq!(r["structuredContent"]["code"], "invalid_args");
            assert!(!r.to_string().contains(TOKEN));
        }
        assert!(a.google().is_none());
    }
}
