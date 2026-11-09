//! A thread folded as Gmail folds it: each message a `<details>` row, open for
//! the newest and the unread, a long run of read ones behind "N more
//! messages", and quoted history behind a ••• pill. No script: the web view
//! toggles `<details>` itself.
//!
//! Every fold is markup built here. The cleaner keeps no `details` or
//! `summary` of a mail's, and what it gives back closes each element it opens,
//! so a mail can neither write a fold nor close one of these.

use crate::mail::escape;
use crate::{clean, gmail};

/// What ends a fold of any kind.
pub const END: &str = "</details>";
const QUOTE: &str = "<details class=\"gws-quote\"><summary>•••</summary>";

/// A message's row, which opens its fold: who, the start of what they said,
/// and when. `open` for the newest and every unread one.
pub fn message(name: &str, snippet: &str, day: &str, open: bool) -> String {
    format!(
        "<details class=\"gws-msg\"{}><summary><span class=\"gws-date\">{}</span><span class=\"gws-from\">{}</span><span class=\"gws-snip\">{}</span></summary>",
        if open { " open" } else { "" },
        escape(day),
        escape(name),
        escape(snippet)
    )
}

/// The row that stands for `n` folded messages in the middle of a thread.
/// Opened, it says "Fewer messages", and folds them again.
pub fn more(n: usize) -> String {
    format!(
        "<details class=\"gws-more\"><summary><span class=\"gws-n\">{n} more messages</span><span class=\"gws-less\">Fewer messages</span></summary>"
    )
}

/// The runs of folded messages that go behind a "N more messages" row, as
/// ranges into `open`: in a run of more than 3, all but the first.
pub fn hidden(open: &[bool]) -> Vec<std::ops::Range<usize>> {
    let mut runs = Vec::new();
    let mut at = 0;
    while at < open.len() {
        let len = open[at..].iter().take_while(|o| !**o).count();
        if len > 3 {
            runs.push(at + 1..at + len);
        }
        at += len.max(1);
    }
    runs
}

/// A text's lines, each with where it starts, trimmed.
fn lines(text: &str) -> Vec<(usize, &str)> {
    let mut at = 0;
    text.split('\n')
        .map(|line| {
            let start = at;
            at += line.len() + 1;
            (start, line.trim())
        })
        .collect()
}

/// Where the "On … wrote:" line ending at line `i` starts: Gmail can wrap it
/// onto a second line.
fn attribution(lines: &[(usize, &str)], i: usize) -> Option<usize> {
    let line = lines[i].1;
    if !line.ends_with("wrote:") {
        return None;
    }
    if line.starts_with("On ") {
        Some(i)
    } else {
        (i > 0 && lines[i - 1].1.starts_with("On ")).then(|| i - 1)
    }
}

/// Where a plain text part's quoted history starts: its trailing "On …
/// wrote:" line with only `>` lines after it, and the sender's own words
/// before it. The row's snippet stops there too.
pub fn plain_cut(text: &str) -> Option<usize> {
    let lines = lines(text);
    let mut first = lines.len();
    while first > 0 && (lines[first - 1].1.is_empty() || lines[first - 1].1.starts_with('>')) {
        first -= 1;
    }
    let quoted = lines[first..].iter().any(|(_, l)| l.starts_with('>'));
    let from = (first > 0 && quoted)
        .then(|| attribution(&lines, first - 1))
        .flatten()?;
    let cut = lines[from].0;
    (!text[..cut].trim().is_empty()).then_some(cut)
}

/// A plain text part, its quoted history behind a fold.
pub fn plain(text: &str) -> String {
    match plain_cut(text) {
        Some(cut) => format!(
            "{}{QUOTE}{}{END}",
            pre(text[..cut].trim_end_matches(['\n', '\r'])),
            pre(&text[cut..])
        ),
        None => pre(text),
    }
}

pub fn pre(text: &str) -> String {
    format!(
        "<pre style=\"white-space:pre-wrap;font:inherit;margin:0\">{}</pre>",
        escape(text)
    )
}

/// Cleaned HTML with its trailing quoted history behind folds: Gmail's
/// `gmail_quote` (its `gmail_attr` line is inside it) and Apple's
/// `blockquote type=cite`. A quote with the sender's own words after it is an
/// answer in between, and stays; a mail that is nothing but a quote isn't
/// hidden. Also where the folds start, which is where the row's snippet stops.
pub fn quotes(cleaned: &str) -> (String, Option<usize>) {
    let spans = spans(cleaned);
    // from the last quote back, while only quotes and blank space follow
    let mut first = spans.len();
    while first > 0 {
        let end = spans[first - 1].1;
        let next = spans.get(first).map_or(cleaned.len(), |s| s.0);
        if !blank(&cleaned[end..next]) {
            break;
        }
        first -= 1;
    }
    let cut = spans.get(first).map(|s| s.0);
    if cut.is_none_or(|c| clean::visible_text(&cleaned[..c]).is_empty()) {
        return (cleaned.to_owned(), None);
    }
    let mut out = String::with_capacity(cleaned.len() + 64);
    let mut at = 0;
    for &(start, end) in &spans[first..] {
        out.push_str(&cleaned[at..start]);
        out.push_str(QUOTE);
        out.push_str(&cleaned[start..end]);
        out.push_str(END);
        at = end;
    }
    out.push_str(&cleaned[at..]);
    (out, cut)
}

/// Whether a start tag opens quoted history.
fn quote(tag: &str, name: &str) -> bool {
    let class = clean::attribute(tag, "class").unwrap_or_default();
    // only where a block may stand: a fold in a table row is moved out of it
    let block = name == "div" || name == "blockquote";
    (block && class.split_ascii_whitespace().any(|c| c == "gmail_quote"))
        || (name == "blockquote"
            && clean::attribute(tag, "type").is_some_and(|t| t.eq_ignore_ascii_case("cite")))
}

/// The outermost quotes in cleaned markup, each from its start tag to the end
/// of its closing one.
fn spans(html: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    // the quote open now: its name, how many of that name are open, its start
    let mut inside: Option<(String, usize, usize)> = None;
    let mut at = 0;
    while let Some(lt) = html[at..].find('<').map(|n| at + n) {
        let end = lt + clean::tag_end(&html[lt..]);
        let tag = &html[lt..end];
        at = end;
        let closing = tag.starts_with("</");
        let name: String = tag
            .trim_start_matches(['<', '/'])
            .chars()
            .take_while(char::is_ascii_alphanumeric)
            .collect();
        if clean::VOID.contains(&name.as_str()) {
            continue;
        }
        match &mut inside {
            Some((open, depth, start)) => {
                if *open == name {
                    *depth = if closing { *depth - 1 } else { *depth + 1 };
                    if *depth == 0 {
                        spans.push((*start, end));
                        inside = None;
                    }
                }
            }
            None if !closing && quote(tag, &name) => inside = Some((name, 1, lt)),
            None => {}
        }
    }
    spans
}

/// Whether markup shows no words.
fn blank(html: &str) -> bool {
    let mut rest = html;
    while !rest.is_empty() {
        let lt = rest.find('<').unwrap_or(rest.len());
        if !gmail::decode_entities(&rest[..lt])
            .chars()
            .all(|c| c.is_whitespace() || c == '\u{a0}')
        {
            return false;
        }
        rest = &rest[lt..];
        rest = &rest[clean::tag_end(rest).min(rest.len())..];
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_of_more_than_three_read_messages_folds_into_one_row() {
        let o = |s: &str| s.chars().map(|c| c == 'o').collect::<Vec<_>>();
        assert_eq!(hidden(&o("ccco")), vec![]);
        assert_eq!(hidden(&o("cccco")), vec![1..4]);
        assert_eq!(hidden(&o("occccocccccco")), vec![2..5, 7..12]);
        assert_eq!(hidden(&o("ooo")), vec![]);
        assert_eq!(hidden(&o("cccc")), vec![1..4]);
    }

    #[test]
    fn gmails_quote_folds_with_its_attribution_line() {
        let page = clean::clean(
            r#"<div dir="ltr"><p>Thursday works.</p></div><div class="gmail_quote gmail_quote_container"><div class="gmail_attr">On Nov 6, 2026, Ana wrote:</div><blockquote class="gmail_quote"><p>Can we?</p></blockquote></div>"#,
        )
        .html;
        let folded = quotes(&page).0;
        let fold = folded.find(QUOTE).expect("folded");
        assert!(folded[..fold].contains("Thursday works."));
        assert!(folded[fold..].starts_with(&format!("{QUOTE}<div class=\"gmail_quote")));
        assert!(
            folded.ends_with(&format!("</blockquote></div>{END}")),
            "{folded}"
        );
        assert_eq!(
            folded.matches(QUOTE).count(),
            1,
            "the inner quote is in the outer one"
        );
    }

    #[test]
    fn apples_cited_quote_folds() {
        let page = clean::clean(
            r#"<div>Sounds good.</div><div>On Nov 6, Ana wrote:</div><blockquote type="cite"><div>Can we?</div><blockquote type="cite">Older</blockquote></blockquote>"#,
        )
        .html;
        assert!(
            page.contains(r#"<blockquote type="cite">"#),
            "the cleaner keeps it: {page}"
        );
        let folded = quotes(&page).0;
        assert_eq!(folded.matches(QUOTE).count(), 1, "{folded}");
        assert!(
            folded.ends_with(&format!("Older</blockquote></blockquote>{END}")),
            "{folded}"
        );
        // a plain blockquote is the sender quoting something, not history
        let plain = clean::clean("<blockquote>A saying</blockquote>").html;
        assert_eq!(quotes(&plain).0, plain);
    }

    #[test]
    fn a_quote_answered_in_between_stays_open() {
        let page = clean::clean(
            r#"<blockquote type="cite">Can we?</blockquote><p>Yes.</p><blockquote type="cite">And Friday?</blockquote><p>&nbsp;</p><br>"#,
        )
        .html;
        let folded = quotes(&page).0;
        assert_eq!(folded.matches(QUOTE).count(), 1, "{folded}");
        assert!(folded.find(QUOTE).unwrap() > folded.find("Yes.").unwrap());
    }

    #[test]
    fn a_plain_reply_folds_its_quoted_lines() {
        let text = "Thursday works.\n\nOn Nov 6, 2026, Ana <ana@example.com> wrote:\n> Can we?\n>\n> > Older\n";
        let folded = plain(text);
        assert_eq!(
            folded,
            format!(
                "{}{QUOTE}{}{END}",
                pre("Thursday works."),
                pre("On Nov 6, 2026, Ana <ana@example.com> wrote:\n> Can we?\n>\n> > Older\n")
            )
        );
        // Gmail wraps a long attribution line
        let wrapped =
            "Yes.\nOn Thu, Nov 6, 2026 at 9:00 AM Ana Reyes <\nana@example.com> wrote:\n> Can we?";
        assert_eq!(
            plain(wrapped),
            format!(
                "{}{QUOTE}{}{END}",
                pre("Yes."),
                pre(
                    "On Thu, Nov 6, 2026 at 9:00 AM Ana Reyes <\nana@example.com> wrote:\n> Can we?"
                )
            )
        );
        // words after the quote, or no attribution, leave it as it is
        for open in [
            "Hi\nOn Nov 6, Ana wrote:\n> Can we?\nYes.",
            "Hi\n> Can we?",
            "> all quote",
        ] {
            assert_eq!(plain(open), pre(open), "{open}");
        }
    }

    #[test]
    fn a_row_stops_where_the_fold_starts() {
        let text = "Thursday works.\n\nOn Nov 6, 2026, Ana <ana@example.com> wrote:\n> Can we?";
        assert_eq!(text[..plain_cut(text).unwrap()].trim(), "Thursday works.");
        let wrapped = "Yes.\nOn Thu, Ana <\nana@example.com> wrote:\n> x";
        assert_eq!(&wrapped[..plain_cut(wrapped).unwrap()], "Yes.\n");
        let html = clean::clean(
            r#"<div>Sounds good.</div><blockquote type="cite">Old words</blockquote>"#,
        )
        .html;
        let cut = quotes(&html).1.unwrap();
        assert_eq!(clean::visible_text(&html[..cut]), "Sounds good.");
    }

    #[test]
    fn a_mail_that_is_all_quote_stays_in_sight() {
        for html in [
            r#"<div class="gmail_quote"><p>Forwarded words</p></div>"#,
            r#"<br><blockquote type="cite">Old words</blockquote><p>&nbsp;</p>"#,
            r#"<p style="display:none">hidden</p><blockquote type="cite">Old words</blockquote>"#,
        ] {
            let page = clean::clean(html).html;
            assert_eq!(quotes(&page), (page.clone(), None), "{html}");
        }
        let text = "\nOn Nov 6, Ana wrote:\n> Can we?";
        assert_eq!((plain_cut(text), plain(text)), (None, pre(text)));
    }

    #[test]
    fn a_gmail_quote_class_in_a_table_folds_nothing() {
        for html in [
            r#"<p>Hi</p><table><tbody><tr class="gmail_quote"><td>Old</td></tr></tbody></table>"#,
            r#"<p>Hi</p><table><tbody><tr><td class="gmail_quote">Old</td></tr></tbody></table>"#,
            r#"<p>Hi</p><ul><li class="gmail_quote">Old</li></ul>"#,
        ] {
            let page = clean::clean(html).html;
            assert_eq!(quotes(&page).0, page, "{html}");
        }
    }

    #[test]
    fn a_mail_can_neither_write_a_fold_nor_break_out_of_one() {
        let hostile = r#"<details open><summary>Fake row</summary>inside</details></details></summary><summary>•••</summary><div class="gws-quote">x</div></div></blockquote>"#;
        let page = clean::clean(hostile).html;
        assert!(
            !page.contains("<details") && !page.contains("<summary"),
            "{page}"
        );
        assert!(
            !page.contains("</details") && !page.contains("</summary"),
            "{page}"
        );
        let folded = quotes(&page).0;
        assert_eq!(folded, page, "no quote, no fold");
        // the row is text, however it's written
        let row = message("</summary></details><b>x", "<details open>", "Nov 6", false);
        assert_eq!(row.matches("<details").count(), 1, "{row}");
        assert_eq!(row.matches("<summary>").count(), 1, "{row}");
        assert!(!row.contains("<b>") && !row.contains(" open>"), "{row}");
        // a quote that never closes in the mail is closed by the cleaner
        let open = quotes(&clean::clean(r#"<p>Hi</p><div class="gmail_quote"><p>Old"#).html).0;
        assert!(open.ends_with(&format!("</div>{END}")), "{open}");
    }
}
