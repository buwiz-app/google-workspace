//! What every screen starts from: it says whether this feature works with the
//! signed-in account, which is more than being signed in.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use buwiz_app::Bridge;
use nsh_core::prelude::*;
use serde_json::Value;
use url::Url;

pub(crate) const BG: &str = "bg-[#f6f6f4] dark:bg-[#151515]";
pub(crate) const MUTE: &str = "text-[#5c5c5c] dark:text-[#a0a0a0]";
pub(crate) const INK: &str = "text-[#1c1c1c] dark:text-[#f0f0f0]";
pub(crate) const RAIL: &str = "bg-[#efefec] dark:bg-[#0f0f0f]";
pub(crate) const PANEL: &str = "bg-white dark:bg-[#1d1d1d]";

/// What a card sits in. The Bot's frame can be taller than a short card, and
/// the rest of it has to be the card and not a dark band.
pub(crate) fn card_frame() -> nsh_core::element::View {
    view().app_class(Some(format!("flex-1 {PANEL}").into()))
}
pub(crate) const LINE: &str = "border-black/10 dark:border-[#2b2b2b]";
pub(crate) const CHOSEN: &str = "bg-[#e1ebfb] dark:bg-[#15253f]";
pub(crate) const DIM: &str = "text-[#8a8a8a] dark:text-[#6e6e6e]";
/// How often an open screen asks whether the store changed.
pub(crate) const POLL: Duration = Duration::from_secs(15);

/// A person's action waiting its turn.
pub(crate) type Job = Pin<Box<dyn Future<Output = ()>>>;

/// Which of several answers out at once still counts: the latest asked for.
#[derive(Clone, Copy)]
pub(crate) struct Latest(Signal<u64>);

impl Latest {
    pub(crate) fn new() -> Self {
        Self(signal(0))
    }

    /// A new question: every earlier one stops counting.
    pub(crate) fn ask(self) -> u64 {
        let turn = self.0.get_untracked() + 1;
        self.0.set(turn);
        turn
    }

    pub(crate) fn counts(self, turn: u64) -> bool {
        self.0.get_untracked() == turn
    }
}

/// How many times a busy answer is tried again before it's shown.
pub(crate) const RETRIES: u32 = 5;

/// The wait before try `attempt + 1`.
pub(crate) fn backoff(attempt: u32) -> Duration {
    Duration::from_millis(250 << attempt.min(4))
}

/// A wait the screen's task can await.
pub(crate) async fn sleep(wait: Duration) {
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        std::thread::sleep(wait);
        let _ = tx.send(());
    });
    let _ = rx.await;
}

/// A write through the bridge, tried once: a person's action can't be sent
/// again on a guess that the first try did nothing.
pub(crate) async fn once(b: &Bridge, tool: &str, args: Value) -> Result<Value, String> {
    b.call(tool, args).await.map_err(|e| {
        if e.message.contains("Sign in to Google") {
            "sign-in".to_owned()
        } else {
            e.message
        }
    })
}

/// A read through the bridge, tried again while the answer is only busy.
pub(crate) async fn read(b: &Bridge, tool: &str, args: Value) -> Result<Value, String> {
    for attempt in 0.. {
        match b.call(tool, args.clone()).await {
            Ok(v) => return Ok(v),
            Err(e) if e.retryable && attempt < RETRIES => sleep(backoff(attempt)).await,
            Err(e) if e.message.contains("Sign in to Google") => return Err("sign-in".to_owned()),
            Err(e) => return Err(e.message),
        }
    }
    Err("That didn't work. Try again.".to_owned())
}

/// Where a link really goes, always shown whole, however long the rest is.
fn goes_to(link: &str) -> String {
    let Ok(url) = Url::parse(link) else {
        return link.to_owned();
    };
    match (url.host_str(), url.port()) {
        (Some(host), Some(port)) => format!("{host}:{port}"),
        (Some(host), None) => host.to_owned(),
        (None, _) => url.path().to_owned(),
    }
}

/// The confirm a link needs: where it goes, and the address, before it opens.
pub(crate) fn link_bar(link: Signal<Option<String>>) -> impl IntoElement {
    dynamic(move || match link.get() {
        None => view(),
        Some(url) => {
            let target = url.clone();
            view()
                .app_class(Some(
                    format!("flex-row items-center gap-3 px-3 py-2 border-t {LINE} {RAIL}").into(),
                ))
                .child(
                    view()
                        .app_class(Some("flex-1 gap-[2px]".into()))
                        .child(
                            text(format!("Open {}?", goes_to(&url)))
                                .app_class(Some(format!("text-[12px] {INK}").into())),
                        )
                        .child(
                            text(url.clone())
                                .number_of_lines(2)
                                .app_class(Some(format!("text-[11px] {MUTE}").into())),
                        ),
                )
                .child(
                    view()
                        .app_class(Some(
                            "px-3 py-1 rounded-md bg-[#2f6fd6] dark:bg-[#7fb2ff]".into(),
                        ))
                        .label("Open this link")
                        .on_press(move || {
                            link.set(None);
                            drop(open_url(target.clone()));
                        })
                        .child(
                            text("Open").app_class(Some(
                                "text-[12px] text-white dark:text-[#10151c]".into(),
                            )),
                        ),
                )
                .child(
                    view()
                        .app_class(Some("px-3 py-1".into()))
                        .label("Don't open this link")
                        .on_press(move || link.set(None))
                        .child(text("Cancel").app_class(Some(format!("text-[12px] {INK}").into()))),
                )
        }
    })
}

#[derive(Clone, PartialEq)]
pub(crate) enum Account {
    Loading,
    Covered,
    NotCovered,
    /// Signed in, with no scopes on record: only trying will tell.
    Unknown,
    SignedOut,
    Failed(String),
}

/// What `gws_status` says of the feature `name`.
pub(crate) fn account_of(status: &Value, name: &str) -> Account {
    if status.get("signed_in") != Some(&Value::Bool(true)) {
        return Account::SignedOut;
    }
    match status.get("features").filter(|f| !f.is_null()) {
        None => Account::Unknown,
        Some(f) if f.get(name.to_lowercase()) == Some(&Value::Bool(true)) => Account::Covered,
        Some(_) => Account::NotCovered,
    }
}

pub(crate) fn line_of(account: Account, name: &str) -> String {
    match account {
        Account::Loading => "Loading…".to_owned(),
        Account::Covered => format!("{name} is connected to your Google account."),
        Account::NotCovered => format!("Your Google sign-in doesn't cover {name} yet."),
        Account::Unknown => {
            format!("You're signed in to Google. {name} will check its access when it needs it.")
        }
        Account::SignedOut => "Connect your Google account on this app's page.".to_owned(),
        Account::Failed(message) => message,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_link_names_its_real_host_however_long_the_rest() {
        let long = format!("https://real.example/{}", "a/".repeat(500));
        assert_eq!(goes_to(&long), "real.example");
        assert_eq!(goes_to("http://127.0.0.1:8822/x?y=1"), "127.0.0.1:8822");
        assert_eq!(goes_to("https://xn--80ak6aa92e.com/"), "xn--80ak6aa92e.com");
        assert_eq!(goes_to("mailto:ana@example.com"), "ana@example.com");
    }

    #[test]
    fn a_busy_answer_waits_longer_each_try() {
        assert_eq!(backoff(0), Duration::from_millis(250));
        assert_eq!(backoff(3), Duration::from_secs(2));
        assert_eq!(backoff(40), Duration::from_secs(4), "and stops growing");
    }

    #[test]
    fn a_screen_says_connected_only_when_the_sign_in_covers_it() {
        let status = json!({"signed_in": true, "features": {"mail": true, "calendar": false, "drive": false}});
        assert_eq!(
            line_of(account_of(&status, "Mail"), "Mail"),
            "Mail is connected to your Google account."
        );
        assert_eq!(
            line_of(account_of(&status, "Calendar"), "Calendar"),
            "Your Google sign-in doesn't cover Calendar yet."
        );
        assert_eq!(
            line_of(account_of(&status, "Drive"), "Drive"),
            "Your Google sign-in doesn't cover Drive yet."
        );
        assert!(account_of(&json!({"signed_in": false}), "Mail") == Account::SignedOut);
        assert!(
            account_of(&json!({"signed_in": true, "features": null}), "Mail") == Account::Unknown,
            "not the same as missing"
        );
        assert!(account_of(&json!({"signed_in": true}), "Mail") == Account::Unknown);
    }
}
