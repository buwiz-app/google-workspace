//! workspace-mcp's text → items, as server.mjs parsed it. The patterns are
//! server.mjs's regexes with JavaScript's meaning spelled out: `\s` is JS
//! space (with U+FEFF, without U+0085), `.` stops at any JS line end, and in
//! multiline mode `^`/`$` sit at `\n`, `\r`, U+2028 and U+2029.

use std::sync::LazyLock;

use fancy_regex::Regex;

use crate::js;

pub const BODY_MAX: usize = 64 * 1024;
pub const SNIPPET_MAX: usize = 200;

const S: &str = r"[\t\n\x0B\x0C\r \x{A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}\x{FEFF}]";
const NOT_S: &str = r"[^\t\n\x0B\x0C\r \x{A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}\x{FEFF}]";
const DOT: &str = r"[^\n\r\x{2028}\x{2029}]";
/// `^` and `$` with the `m` flag.
const BOL: &str = r"(?:\A|(?<=[\n\r\x{2028}\x{2029}]))";
const EOL: &str = r"(?=[\n\r\x{2028}\x{2029}]|\z)";

fn re(pattern: &str) -> Regex {
    let p = pattern.replace("{S}", S).replace("{NS}", NOT_S).replace("{DOT}", DOT).replace("{BOL}", BOL).replace("{EOL}", EOL);
    Regex::new(&p).expect("a fixed pattern")
}

// search_gmail_messages: "  1. Message ID: <id>" per message
static SEARCH_ID: LazyLock<Regex> = LazyLock::new(|| re(r"{BOL}{S}*[0-9]+\.{S}+Message ID: ({NS}+){S}*{EOL}"));
// a message's start in a batch: its id, or "⚠️ Message <id>: <error>"
static BATCH_START: LazyLock<Regex> = LazyLock::new(|| re(r"{BOL}(?:Message ID: ({NS}+)|\x{26A0}\x{FE0F} Message {NS}+:{DOT}*){EOL}"));
static BLOCK_END: LazyLock<Regex> = LazyLock::new(|| re(r"\n+---\n*\z"));
static HEADERS: LazyLock<[(&str, Regex); 4]> = LazyLock::new(|| ["From", "To", "Subject", "Date"].map(|n| (n, re(&format!("{{BOL}}{n}: ({{DOT}}*){{EOL}}")))));

/// The message ids of a search's text, in order ("unknown" left out).
pub fn search_ids(text: &str) -> Vec<String> {
    SEARCH_ID.captures_iter(text).filter_map(Result::ok).filter_map(|c| c.get(1).map(|m| m.as_str().to_owned())).filter(|id| id != "unknown").collect()
}

/// One message of a batch.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Message {
    pub id: String,
    pub from: String,
    pub to: String,
    pub subject: String,
    pub date: String,
    pub body: String,
}

fn header(head: &str, name: &str) -> String {
    let re = &HEADERS.iter().find(|(n, _)| *n == name).expect("a known header").1;
    let v = re.captures(head).ok().flatten().and_then(|c| c.get(1).map(|m| js::trim(m.as_str()).to_owned())).unwrap_or_default();
    if v.starts_with("[not present") {
        String::new()
    } else {
        v
    }
}

// get_gmail_messages_content_batch: per message "Message ID: <id>", header
// lines, "--- BODY ---", the body, maybe "--- ATTACHMENTS ---"; messages
// joined by "\n---\n\n"; a failed one is "⚠️ Message <id>: <error>".
pub fn batch_messages(text: &str) -> Vec<Message> {
    let starts: Vec<(usize, Option<String>)> = BATCH_START.captures_iter(text).filter_map(Result::ok).map(|c| (c.get(0).map_or(0, |m| m.start()), c.get(1).map(|m| m.as_str().to_owned()))).collect();
    starts
        .iter()
        .enumerate()
        .filter_map(|(i, (at, id))| {
            let id = id.clone()?;
            let end = starts.get(i + 1).map_or(text.len(), |s| s.0);
            let mut block = &text[*at..end];
            if let Ok(Some(m)) = BLOCK_END.find(block) {
                block = &block[..m.start()];
            }
            // headers come before the body: a forwarded mail's "From:" is not one
            let (head, mut body) = match block.find("\n--- BODY ---\n") {
                Some(b) => (&block[..b], &block[b + 14..]),
                None => (block, ""),
            };
            if let Some(a) = body.rfind("\n--- ATTACHMENTS ---\n") {
                body = &body[..a];
            }
            Some(Message { id, from: header(head, "From"), to: header(head, "To"), subject: header(head, "Subject"), date: header(head, "Date"), body: body.trim_end_matches('\n').to_owned() })
        })
        .collect()
}

/// The body on one line, at most 200 UTF-16 units with the ellipsis.
pub fn snippet_of(body: &str) -> String {
    let mut flat = String::new();
    let mut gap = false;
    for c in body.chars() {
        if js::is_space(c) {
            gap = true;
        } else {
            if gap && !flat.is_empty() {
                flat.push(' ');
            }
            gap = false;
            flat.push(c);
        }
    }
    if js::len_utf16(&flat) > SNIPPET_MAX {
        format!("{}…", js::slice_utf16(&flat, SNIPPET_MAX - 1))
    } else {
        flat
    }
}

/// `text` within `max` UTF-8 bytes, marked when cut; never half a character.
pub fn cap_bytes(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    const MARKER: &str = "…[truncated]";
    let n = max - MARKER.len();
    let mut end = n;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut cut = &text[..end];
    // Node decodes the cut bytes and drops one trailing U+FFFD: the half
    // character, or (cut on a boundary) a real one that ends the cut
    if end == n {
        cut = cut.strip_suffix('\u{fffd}').unwrap_or(cut);
    }
    format!("{cut}{MARKER}")
}

#[cfg(test)]
mod tests {
    use super::*;

    // fixtures/fake-gws-upstream.mjs's text for two messages, as
    // workspace-mcp 2.2 prints it
    const SEARCH: &str = "Found 2 messages matching 'from:bir.gov.ph':\n\n📧 MESSAGES:\n  1. Message ID: 18f1a\n     Web Link: https://mail.google.com/mail/u/0/#all/18f1a\n     Thread ID: t-18f1a\n     Thread Link: https://mail.google.com/mail/u/0/#all/t-18f1a\n\n  2. Message ID: 18f1b\n     Web Link: https://mail.google.com/mail/u/0/#all/18f1b\n     Thread ID: t-18f1b\n     Thread Link: https://mail.google.com/mail/u/0/#all/t-18f1b\n\n💡 USAGE:\n  • Pass the Message IDs **as a list** to get_gmail_messages_content_batch()\n    e.g. get_gmail_messages_content_batch(message_ids=[...])";
    const BATCH: &str = "Retrieved 3 messages:\n\nMessage ID: 18f1a\nSubject: Tax Return Receipt Confirmation\nFrom: BIR eFPS <efps@bir.gov.ph>\nDate: Mon, 26 Oct 2026 09:12:00 +0800\nMessage-ID: <18f1a@mail.example>\nTo: owner@example.com\nCc: [not present in Gmail response]\nWeb Link: https://mail.google.com/mail/u/0/#all/18f1a\n\n--- BODY ---\nYour 1701Q for TIN 000-000-000 was received.\n\nReference: 2026-ABC-123\nFrom: a forwarded line, not a header\n\n---\n\n⚠️ Message gone: <HttpError 404 \"Requested entity was not found.\">\n\n---\n\nMessage ID: 18f1b\nSubject: Lunch\nFrom: Ana <ana@example.com>\nDate: Tue, 27 Oct 2026 12:00:00 +0800\nMessage-ID: <18f1b@mail.example>\nTo: [not present in Gmail response]\nCc: [not present in Gmail response]\nWeb Link: https://mail.google.com/mail/u/0/#all/18f1b\n\n--- BODY ---\n  See you   at noon.\n\n--- ATTACHMENTS ---\nmenu.pdf\n";

    #[test]
    fn search_text_gives_the_ids_in_order() {
        assert_eq!(search_ids(SEARCH), ["18f1a", "18f1b"]);
        assert_eq!(search_ids("No messages found for query: 'x'"), Vec::<String>::new());
        assert_eq!(search_ids("  1. Message ID: unknown\n  2. Message ID: a1\r\n3.\u{2028}Message ID: b2 \n 4. Message ID: two words\n"), ["a1", "b2"]);
    }

    #[test]
    fn batch_text_gives_each_message_with_its_headers_and_body() {
        let m = batch_messages(BATCH);
        assert_eq!(m.len(), 2, "the failed one is left out");
        assert_eq!(
            m[0],
            Message {
                id: "18f1a".into(),
                from: "BIR eFPS <efps@bir.gov.ph>".into(),
                to: "owner@example.com".into(),
                subject: "Tax Return Receipt Confirmation".into(),
                date: "Mon, 26 Oct 2026 09:12:00 +0800".into(),
                body: "Your 1701Q for TIN 000-000-000 was received.\n\nReference: 2026-ABC-123\nFrom: a forwarded line, not a header".into(),
            }
        );
        assert_eq!((m[1].id.as_str(), m[1].to.as_str(), m[1].body.as_str()), ("18f1b", "", "  See you   at noon."));
        let no_body = batch_messages("Message ID: x\nFrom:  Ann  \nSubject: [not present]\n");
        assert_eq!((no_body[0].from.as_str(), no_body[0].subject.as_str(), no_body[0].body.as_str()), ("Ann", "", ""));
    }

    #[test]
    fn snippets_are_one_line_and_capped() {
        assert_eq!(snippet_of("  See you \u{a0} at\tnoon.\r\n\n"), "See you at noon.");
        let long = format!("{}abcd😀tail", "word ".repeat(39));
        assert_eq!(snippet_of(&long), format!("{}abcd…", "word ".repeat(39)));
        let split = format!("{}abc😀tail", "word ".repeat(39));
        assert_eq!(snippet_of(&split), format!("{}abc\u{fffd}…", "word ".repeat(39)));
        assert_eq!(js::len_utf16(&snippet_of(&"x".repeat(500))), 200);
    }

    #[test]
    fn bodies_are_capped_at_64_kib_on_a_character_boundary() {
        let big = cap_bytes(&"é".repeat(70_000), BODY_MAX);
        assert!(big.len() <= BODY_MAX && big.ends_with("…[truncated]"));
        assert_eq!(big.len(), 65_522 + "…[truncated]".len());
        let three = cap_bytes(&format!("a{}", "€".repeat(30_000)), BODY_MAX);
        assert_eq!(three.len(), 1 + 3 * 21_840 + "…[truncated]".len());
        // a real U+FFFD that ends the cut goes too, as in Node
        let fffd = cap_bytes(&format!("{}\u{fffd}{}", "a".repeat(65_519), "b".repeat(100)), BODY_MAX);
        assert_eq!(fffd, format!("{}…[truncated]", "a".repeat(65_519)));
        assert_eq!(cap_bytes("short", BODY_MAX), "short");
    }
}
