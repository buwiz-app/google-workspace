//! Drafts and sending. A draft is built here, into the message Gmail keeps, and
//! Send sends the very message the person read: a draft that changed since
//! has a new revision, and refuses until it's read again.

use std::collections::HashSet;
use std::sync::MutexGuard;

use base64::Engine as _;
use serde_json::{Value, json};

use crate::accounts::Bearer;
use crate::gmail::{self, Failure};
use crate::tools::{MAIL_DRAFT_SAVE, fail, ok};
use crate::{Core, js};

/// The most recipients one message takes.
const RECIPIENTS_MAX: usize = 100;
/// The longest body a draft holds, before attachments.
const BODY_MAX: usize = 1024 * 1024;

/// A message being written.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Draft {
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub body: String,
    /// The conversation a reply belongs to.
    pub thread: Option<String>,
    pub in_reply_to: String,
    pub references: String,
}

/// An address alone, as `a@b.c` or `Name <a@b.c>`, if it's one a header can hold.
fn address(text: &str) -> Option<String> {
    let text = text.trim();
    let bare = text
        .rsplit_once('<')
        .and_then(|(_, rest)| rest.strip_suffix('>'))
        .unwrap_or(text);
    let (name, host) = bare.split_once('@')?;
    let clean = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| !c.is_control() && !c.is_whitespace() && !"<>,;\"".contains(c))
    };
    (clean(name) && clean(host) && host.contains('.')).then(|| bare.to_owned())
}

fn addresses(v: Option<&Value>) -> Result<Vec<String>, String> {
    let list: Vec<String> = match v {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|i| i.as_str().map(str::to_owned))
            .collect(),
        Some(Value::String(s)) => s.split(',').map(str::to_owned).collect(),
        _ => Vec::new(),
    };
    list.iter()
        .filter(|a| !a.trim().is_empty())
        .map(|a| address(a).ok_or_else(|| format!("{} isn't an address.", js::slice_utf16(a, 80))))
        .collect()
}

/// A line of a header: nothing in it may start another.
fn one_line(text: &str) -> String {
    text.replace(['\r', '\n'], " ")
}

/// A header value in words any mail system reads.
fn header_text(text: &str) -> String {
    let text = one_line(text);
    if text.is_ascii() {
        return text;
    }
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    format!("=?UTF-8?B?{encoded}?=")
}

impl Draft {
    pub fn of(args: &Value) -> Result<Draft, String> {
        let text = |k: &str| js::string_or_empty(args.get(k));
        let draft = Draft {
            to: addresses(args.get("to"))?,
            cc: addresses(args.get("cc"))?,
            bcc: addresses(args.get("bcc"))?,
            subject: text("subject"),
            body: text("body"),
            thread: Some(text("thread")).filter(|t| gmail::id_ok(t)),
            in_reply_to: one_line(&text("in_reply_to")),
            references: one_line(&text("references")),
        };
        if draft.recipients().count() > RECIPIENTS_MAX {
            return Err("That's too many recipients for one message.".to_owned());
        }
        if draft.body.len() > BODY_MAX {
            return Err("That message is too long.".to_owned());
        }
        Ok(draft)
    }

    pub fn recipients(&self) -> impl Iterator<Item = &String> {
        self.to.iter().chain(&self.cc).chain(&self.bcc)
    }

    /// The message, as Gmail takes it. It names no From: Gmail fills that from
    /// the sign-in, so a draft can't claim to be from anyone else.
    pub fn raw(&self, message_id: &str) -> String {
        let mut head = String::new();
        let mut line = |name: &str, value: &str| {
            if !value.is_empty() {
                head.push_str(&format!("{name}: {value}\r\n"));
            }
        };
        line("To", &self.to.join(", "));
        line("Cc", &self.cc.join(", "));
        line("Bcc", &self.bcc.join(", "));
        line("Subject", &header_text(&self.subject));
        line("In-Reply-To", &self.in_reply_to);
        line("References", &self.references);
        line("Message-ID", message_id);
        let body = base64::engine::general_purpose::STANDARD.encode(&self.body);
        let wrapped: Vec<&str> = body
            .as_bytes()
            .chunks(76)
            .map(|c| std::str::from_utf8(c).unwrap_or_default())
            .collect();
        let message = format!(
            "{head}MIME-Version: 1.0\r\nContent-Type: text/plain; charset=UTF-8\r\nContent-Transfer-Encoding: base64\r\n\r\n{}\r\n",
            wrapped.join("\r\n")
        );
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(message)
    }
}

/// The message id a draft goes out under, made here so a Send that lost its
/// answer can look in Sent for it.
fn new_message_id() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("<{now:x}.{:x}@buwiz.local>", std::process::id())
}

fn session(core: &Core) -> Result<Bearer, Failure> {
    core.accounts
        .session()
        .map(|(auth, _)| auth)
        .ok_or(Failure::AccountRequired)
}

fn model_drafts(core: &Core) -> MutexGuard<'_, HashSet<String>> {
    core.model_drafts.lock().unwrap_or_else(|e| e.into_inner())
}

/// `gws_mail_draft_save` from the model: a new draft, or one it wrote and the
/// person hasn't touched. Every other draft is the person's work.
pub fn save_for_model(core: &Core, args: &Value) -> Value {
    let held = js::string_or_empty(args.get("draft"));
    if !held.is_empty() && !model_drafts(core).contains(&held) {
        return fail(
            "not_allowed",
            "That draft is the person's. Write a new one instead.",
        );
    }
    let result = core.run(MAIL_DRAFT_SAVE, args);
    if let Some(id) = result
        .pointer("/structuredContent/draft")
        .and_then(Value::as_str)
    {
        model_drafts(core).insert(id.to_owned());
    }
    result
}

/// `gws_mail_draft_save`: writes a draft, or replaces one. Sends nothing.
pub fn save(core: &Core, args: &Value) -> Result<Value, Failure> {
    let auth = session(core)?;
    let draft = match Draft::of(args) {
        Ok(draft) => draft,
        Err(why) => return Ok(fail("invalid_args", &why)),
    };
    let held = js::string_or_empty(args.get("draft"));
    if !held.is_empty() && !gmail::id_ok(&held) {
        return Ok(fail("invalid_args", "That isn't a draft."));
    }
    let message_id = Some(js::string_or_empty(args.get("message_id")))
        .filter(|m| m.starts_with('<') && m.ends_with('>') && !m.contains(['\r', '\n']))
        .unwrap_or_else(new_message_id);
    let raw = draft.raw(&message_id);
    let thread = draft.thread.as_deref();
    let saved = if held.is_empty() {
        core.gmail.draft_create(&auth, &raw, thread)?
    } else {
        core.gmail.draft_update(&auth, &held, &raw, thread)?
    };
    let Some(saved) = saved else {
        return Ok(fail("not_found", "That draft is gone."));
    };
    // a person's edit makes it theirs: `save_for_model` claims it back only for itself
    model_drafts(core).remove(&held);
    Ok(ok(json!({
        "draft": saved.pointer("/id"),
        "revision": saved.pointer("/message/id"),
        "message_id": message_id,
    })))
}

/// `gws_mail_draft_read`: a draft as it stands, with the revision Send needs.
pub fn read(core: &Core, args: &Value) -> Result<Value, Failure> {
    let auth = session(core)?;
    let id = js::string_or_empty(args.get("draft"));
    if !gmail::id_ok(&id) {
        return Ok(fail("invalid_args", "That isn't a draft."));
    }
    let Some(held) = core.gmail.draft(&auth, &id)? else {
        return Ok(fail("not_found", "That draft is gone."));
    };
    let message = held.get("message").cloned().unwrap_or(Value::Null);
    let h = |name| gmail::short_header(&message, name);
    Ok(ok(json!({
        "draft": id,
        "revision": message.get("id"),
        "message_id": h("Message-ID"),
        "to": h("To"),
        "cc": h("Cc"),
        "bcc": h("Bcc"),
        "subject": h("Subject"),
        "body": gmail::body(&message),
        "thread": message.get("threadId"),
    })))
}

/// `gws_mail_draft_delete`: the person's, and it can't be undone.
pub fn delete(core: &Core, args: &Value) -> Result<Value, Failure> {
    let auth = session(core)?;
    let id = js::string_or_empty(args.get("draft"));
    if !gmail::id_ok(&id) {
        return Ok(fail("invalid_args", "That isn't a draft."));
    }
    core.gmail.draft_delete(&auth, &id)?;
    Ok(ok(json!({"deleted": true})))
}

/// Who may be written to, when a test against a real account says: nobody but
/// this address, checked before a request leaves.
fn only_to() -> Option<String> {
    if cfg!(feature = "stub") {
        std::env::var("GWS_ONLY_TO").ok().filter(|a| !a.is_empty())
    } else {
        None
    }
}

fn outside<'a>(allowed: &str, recipients: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    let mut recipients = recipients;
    recipients.find(|r| !r.eq_ignore_ascii_case(allowed))
}

/// `gws_mail_send`: the person's press, on the revision they read. A Send whose
/// answer was lost is safe to press again: the draft has gone by then, and the
/// message is found in Sent instead of going out twice.
pub fn send(core: &Core, args: &Value) -> Result<Value, Failure> {
    let auth = session(core)?;
    let id = js::string_or_empty(args.get("draft"));
    let revision = js::string_or_empty(args.get("revision"));
    let message_id = js::string_or_empty(args.get("message_id"));
    if !gmail::id_ok(&id) || revision.is_empty() {
        return Ok(fail("invalid_args", "That isn't a draft."));
    }
    let Some(held) = core.gmail.draft(&auth, &id)? else {
        let sent = !message_id.is_empty()
            && !core
                .gmail
                .search(&auth, &format!("rfc822msgid:{message_id}"), 1)?
                .is_empty();
        return Ok(if sent {
            ok(json!({"sent": true, "already": true}))
        } else {
            fail("not_found", "That draft is gone.")
        });
    };
    let message = held.get("message").cloned().unwrap_or(Value::Null);
    if message.get("id").and_then(Value::as_str) != Some(revision.as_str()) {
        return Ok(fail(
            "changed",
            "This draft changed after you read it. Read it again before sending.",
        ));
    }
    if let Some(allowed) = only_to() {
        let all = ["To", "Cc", "Bcc"].map(|h| gmail::header(&message, h));
        let names = all.iter().flat_map(|h| h.split(',')).map(str::trim);
        if let Some(other) = outside(&allowed, names.filter(|n| !n.is_empty())) {
            return Ok(fail(
                "not_allowed",
                &format!("This test run writes only to {allowed}, not {other}."),
            ));
        }
    }
    core.gmail.draft_send(&auth, &id)?;
    Ok(ok(json!({"sent": true})))
}

/// `gws_mail_reply_start`: a reply, reply-all or forward, filled in from the
/// conversation, for the person to go on from. It saves nothing.
pub fn reply_start(core: &Core, args: &Value) -> Result<Value, Failure> {
    let auth = session(core)?;
    let id = js::string_or_empty(args.get("thread"));
    let mode = js::string_or_empty(args.get("mode"));
    if !gmail::id_ok(&id) {
        return Ok(fail("invalid_args", "That isn't a thread."));
    }
    let Some(thread) = core.gmail.thread_full(&auth, &id)? else {
        return Ok(fail("not_found", "That conversation is gone."));
    };
    let list = thread.get("messages").and_then(Value::as_array);
    let Some(last) = list.and_then(|l| l.last()) else {
        return Ok(fail("not_found", "That conversation is gone."));
    };
    let h = |name| gmail::short_header(last, name);
    let me = core
        .accounts
        .signed_in()
        .map(|(who, _)| who)
        .unwrap_or_default();
    let others = |list: &str| {
        list.split(',')
            .filter_map(address)
            .filter(|a| !a.eq_ignore_ascii_case(&me))
            .collect::<Vec<_>>()
    };
    let subject = h("Subject");
    let quoted: String = gmail::body(last)
        .lines()
        .map(|l| format!("> {l}\n"))
        .collect();
    let said = format!("\n\nOn {}, {} wrote:\n{quoted}", h("Date"), h("From"));
    let answer = match mode.as_str() {
        "forward" => json!({
            "to": [], "cc": [],
            "subject": prefixed("Fwd: ", &subject),
            "body": format!("\n\n---------- Forwarded message ----------\nFrom: {}\nDate: {}\nSubject: {subject}\nTo: {}\n\n{}", h("From"), h("Date"), h("To"), gmail::body(last)),
        }),
        _ => {
            let from = h("Reply-To");
            let from = if from.is_empty() { h("From") } else { from };
            let mut to = others(&from);
            // the last word was the person's own: it goes on to whoever that went to
            if to.is_empty() {
                to = others(&h("To"));
            }
            let mut cc = Vec::new();
            if mode == "all" {
                to.extend(others(&h("To")));
                cc = others(&h("Cc"));
            }
            let message_id = h("Message-ID");
            let references = format!("{} {message_id}", h("References"));
            json!({
                "to": to, "cc": cc,
                "subject": prefixed("Re: ", &subject),
                "body": said,
                "thread": id,
                "in_reply_to": message_id,
                "references": references.trim(),
            })
        }
    };
    Ok(ok(answer))
}

fn prefixed(prefix: &str, subject: &str) -> String {
    if subject.to_lowercase().starts_with(&prefix.to_lowercase()) {
        subject.to_owned()
    } else {
        format!("{prefix}{subject}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decoded(raw: &str) -> String {
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(raw)
            .unwrap();
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn a_header_cannot_be_smuggled_in() {
        let draft = Draft::of(&json!({
            "to": "ana@example.com",
            "subject": "hi\r\nBcc: spy@example.com",
            "in_reply_to": "<a>\r\nBcc: spy@example.com",
        }))
        .unwrap();
        let message = decoded(&draft.raw("<id@x>"));
        let head = message.split("\r\n\r\n").next().unwrap();
        assert!(!head.lines().any(|l| l.starts_with("Bcc:")), "{head}");
        for bad in [
            "a\r\nBcc: spy@example.com",
            "not an address",
            "a@b",
            "<a@b.c",
            "a b@c.d",
            "\"a\"@b.c",
        ] {
            assert!(Draft::of(&json!({"to": [bad]})).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_message_names_no_sender_and_says_what_was_written() {
        let draft = Draft::of(&json!({
            "to": ["Ana <ana@example.com>", "ben@example.com"],
            "cc": "cara@example.com",
            "subject": "Lunch ñ",
            "body": "Hello é",
        }))
        .unwrap();
        assert_eq!(draft.to, ["ana@example.com", "ben@example.com"]);
        let message = decoded(&draft.raw("<id@x>"));
        assert!(!message.contains("\nFrom:") && !message.starts_with("From:"));
        assert!(message.contains("To: ana@example.com, ben@example.com\r\n"));
        assert!(message.contains("Cc: cara@example.com\r\n"));
        assert!(message.contains("Subject: =?UTF-8?B?"));
        let body = message
            .split("\r\n\r\n")
            .nth(1)
            .unwrap()
            .replace("\r\n", "");
        let sent = base64::engine::general_purpose::STANDARD
            .decode(body)
            .unwrap();
        assert_eq!(String::from_utf8(sent).unwrap(), "Hello é");
    }

    #[test]
    fn too_many_or_too_long_is_refused() {
        let many: Vec<String> = (0..101).map(|n| format!("a{n}@example.com")).collect();
        assert!(Draft::of(&json!({"to": many})).is_err());
        assert!(Draft::of(&json!({"body": "x".repeat(BODY_MAX + 1)})).is_err());
    }

    #[test]
    fn the_test_guard_finds_anyone_else() {
        let allowed = "support@buwiz.com";
        assert_eq!(outside(allowed, ["SUPPORT@buwiz.com"].into_iter()), None);
        assert_eq!(
            outside(allowed, ["support@buwiz.com", "x@example.com"].into_iter()),
            Some("x@example.com")
        );
    }

    #[test]
    fn a_subject_is_prefixed_once() {
        assert_eq!(prefixed("Re: ", "Lunch"), "Re: Lunch");
        assert_eq!(prefixed("Re: ", "RE: Lunch"), "RE: Lunch");
    }
}
