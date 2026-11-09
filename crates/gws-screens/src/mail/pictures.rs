//! The bar over a mail whose web pictures are hidden: "Pictures are hidden."
//! with Show pictures and Always from the sender, and once a sender is
//! trusted, a quiet line saying so with a way to stop.
//!
//! A fetch here is always the person's: a press of Show pictures or Always,
//! or their opening a conversation from a sender they trust. Each call is
//! made at that moment, as who pressed is judged by it. The presses keep a
//! label of their own that isn't what they show, so the Bot's filter names
//! none of them and the AI can't reach them; the sender's address is in the
//! words and the value, never in a label.

use buwiz_app::Bridge;
use nsh_core::AccessibilityValue;
use nsh_core::prelude::*;
use serde_json::{Value, json};

use super::Mail;
use crate::shell::{INK, LINE, MUTE};

/// What the open conversation's page leaves out of its pictures.
#[derive(Clone, PartialEq, Default, Debug)]
pub(super) struct Pictures {
    /// The conversation, by its own id: a card opens it by ticket.
    thread: String,
    /// The open the page came from: an answer for an earlier one goes.
    turn: u64,
    /// How many the page leaves out.
    hidden: i64,
    /// Who they're from, the address a person can trust.
    from: Option<String>,
    trusted: bool,
    /// The plugin has already fetched what it could.
    asked: bool,
    /// A fetch is on its way.
    getting: bool,
    /// Why the last one didn't work.
    said: Option<String>,
}

impl Pictures {
    fn of(v: &Value, turn: u64) -> Self {
        let pictures = v.get("pictures");
        let flag = |key: &str| {
            pictures
                .and_then(|p| p.get(key))
                .and_then(Value::as_bool)
                .unwrap_or(false)
        };
        Self {
            thread: v
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            turn,
            hidden: v.get("images").and_then(Value::as_i64).unwrap_or(0),
            from: pictures
                .and_then(|p| p.get("from"))
                .and_then(Value::as_str)
                .filter(|a| !a.is_empty())
                .map(str::to_owned),
            trusted: flag("trusted"),
            asked: flag("asked"),
            getting: false,
            said: None,
        }
    }
}

/// A conversation's view came, from the person's press on it: what the bar
/// shows, and for a sender they trust, the pictures asked for at once.
pub(super) fn opened(b: &Bridge, m: Mail, v: &Value, turn: u64) {
    let pictures = Pictures::of(v, turn);
    let ask = pictures.trusted && pictures.hidden > 0 && !pictures.thread.is_empty();
    m.images.set(pictures);
    if ask {
        fetch(b, m, json!({"trusted": true}));
    }
}

/// Asks the plugin for the page with its pictures in, now, and puts the answer
/// in when it comes, if the person is still on that conversation. `only` says
/// whose: `{}` for every message, `{"from": address}` for that sender's, or
/// `{"trusted": true}` for a trusted sender's.
fn fetch(b: &Bridge, m: Mail, only: Value) {
    let shown = m.images.get_untracked();
    if shown.thread.is_empty() || shown.getting {
        return;
    }
    let mut args = json!({"thread": shown.thread});
    for (k, v) in only.as_object().into_iter().flatten() {
        args[k] = v.clone();
    }
    let call = b.call("gws_mail_pictures", args);
    let turn = shown.turn;
    m.images.update(|p| {
        p.getting = true;
        p.said = None;
    });
    let job = Box::pin(async move {
        let answer = call.await;
        if !m.open_turn.counts(turn) {
            return;
        }
        match answer {
            Ok(v) => {
                m.links.set(
                    v.get("links")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|l| l.as_str().map(str::to_owned))
                        .collect(),
                );
                // whether the sender is trusted is the trust presses' to say
                let mut shown = Pictures::of(&v, turn);
                shown.trusted = m.images.with_untracked(|p| p.trusted);
                m.images.set(shown);
                m.page.set(Some(
                    v.get("page")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                ));
            }
            Err(e) => m.images.update(|p| {
                p.getting = false;
                p.said = Some(if e.code == "timeout" {
                    "The pictures are taking a while. Try again in a moment.".to_owned()
                } else {
                    e.message
                });
            }),
        }
    });
    m.beside.with_untracked(|beside| drop(beside.send(job)));
}

/// Says whether `address` always shows, at the press, and the bar follows.
fn trust(b: &Bridge, m: Mail, address: String, always: bool) {
    let tool = if always {
        "gws_mail_picture_sender_add"
    } else {
        "gws_mail_picture_sender_remove"
    };
    let call = b.call(tool, json!({"address": address}));
    let turn = m.images.with_untracked(|p| p.turn);
    let job = Box::pin(async move {
        let answer = call.await;
        if !m.open_turn.counts(turn) {
            return;
        }
        m.images.update(|p| match answer {
            Ok(_) => p.trusted = always,
            Err(e) => p.said = Some(e.message),
        });
    });
    m.beside.with_untracked(|beside| drop(beside.send(job)));
}

fn press(label: &'static str, shows: String, value: Option<String>) -> View {
    view()
        .app_class(Some("px-3 py-1".into()))
        .label(label)
        .accessibility_value(value.map(|v| AccessibilityValue {
            text: Some(v.into()),
            ..AccessibilityValue::default()
        }))
        .child(text(shows).app_class(Some(format!("text-[12px] {INK}").into())))
}

fn line(words: String) -> View {
    view().child(text(words).app_class(Some(format!("text-[12px] {MUTE}").into())))
}

/// The bar, over the page; nothing when there's nothing to say.
pub(super) fn bar(b: Bridge, m: Mail) -> impl IntoElement {
    dynamic(move || {
        let p = m.images.get();
        if p.turn != m.open_turn.now() || (p.hidden == 0 && !p.trusted && p.said.is_none()) {
            return view();
        }
        let mut bar = view().app_class(Some(
            format!("flex-row flex-wrap items-center gap-2 px-3 py-1.5 border-b {LINE}").into(),
        ));
        if p.hidden > 0 {
            bar = bar.child(line(
                if p.getting {
                    "Getting pictures…"
                } else if p.asked {
                    "Some pictures couldn't be shown."
                } else {
                    "Pictures are hidden."
                }
                .to_owned(),
            ));
            if !p.getting && !p.asked {
                let b = b.clone();
                bar = bar.child(
                    press(
                        "Show the pictures in this mail",
                        "Show pictures".to_owned(),
                        None,
                    )
                    .on_press(move || fetch(&b, m, json!({}))),
                );
            }
        }
        if let Some(from) = p.from.clone() {
            let b = b.clone();
            if p.trusted {
                bar = bar
                    .child(line(format!("Pictures from {from} always show.")))
                    .child(
                        press(
                            "Stop always showing this sender's pictures",
                            "Stop".to_owned(),
                            Some(from.clone()),
                        )
                        .on_press(move || trust(&b, m, from.clone(), false)),
                    );
            } else if p.hidden > 0 {
                bar = bar.child(
                    press(
                        "Always show this sender's pictures",
                        format!("Always from {from}"),
                        Some(from.clone()),
                    )
                    .on_press(move || {
                        trust(&b, m, from.clone(), true);
                        // only theirs: the press was about this sender
                        fetch(&b, m, json!({"from": from.clone()}));
                    }),
                );
            }
        }
        if let Some(said) = p.said {
            bar = bar.child(line(said));
        }
        bar
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_view_says_what_the_bar_needs() {
        let v = json!({"id": "t9", "images": 2, "pictures": {"from": "news@digest.example", "trusted": true}});
        let p = Pictures::of(&v, 4);
        assert_eq!(
            (
                p.thread.as_str(),
                p.turn,
                p.hidden,
                p.from.as_deref(),
                p.trusted,
                p.asked
            ),
            ("t9", 4, 2, Some("news@digest.example"), true, false)
        );
        let old = Pictures::of(&json!({"id": "t1", "images": 1}), 1);
        assert_eq!((old.from, old.trusted), (None, false));
    }

    /// The page itself never loads from the web: every picture comes in as data.
    #[test]
    fn the_pages_policy_is_unchanged() {
        assert_eq!(
            crate::reader::MAIL_CSP,
            "default-src 'none'; style-src 'unsafe-inline'; img-src data:; font-src data:; form-action 'none'; base-uri 'none'; frame-src 'none'; object-src 'none'; media-src 'none'"
        );
    }
}
