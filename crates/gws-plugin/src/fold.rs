//! A thread folded as Gmail folds it: each message a `<details>` row, open for
//! the newest and the unread, a long run of read ones behind "N more
//! messages", and quoted history behind a ••• pill. No script: the web view
//! toggles `<details>` itself.
//!
//! Every fold is markup built here. The cleaner keeps no `details` or
//! `summary` of a mail's, and what it gives back closes each element it opens,
//! so a mail can neither write a fold nor close one of these.

use crate::clean;
use crate::mail::escape;

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

/// Whether `first` to `last` is an "On … wrote:" line, which Gmail can wrap
/// onto a second one.
fn wrote(first: &str, last: &str) -> bool {
    first.starts_with("On ") && last.ends_with("wrote:")
}

/// Where the "On … wrote:" line ending at line `i` starts.
fn attribution(lines: &[(usize, &str)], i: usize) -> Option<usize> {
    let line = lines[i].1;
    if wrote(line, line) {
        Some(i)
    } else {
        (i > 0 && wrote(lines[i - 1].1, line)).then(|| i - 1)
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

/// Text with what follows `cut` behind a fold.
pub fn split(text: &str, cut: Option<usize>) -> String {
    match cut {
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

/// A stretch of cleaned markup behind a fold. When it starts inside an
/// element (an "On … wrote:" line after the sender's words, split by `<br>`),
/// those elements close ahead of the fold and open again in it, as `reopen`'s
/// start tags, outermost first, so the markup stays whole.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Region {
    pub start: usize,
    pub end: usize,
    pub reopen: Vec<(usize, usize)>,
}

/// Where a message's quoted history folds.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Quoted {
    /// What goes behind a fold, in order.
    pub regions: Vec<Region>,
    /// What the person sees before the first: the row's snippet.
    pub fresh: String,
}

/// Where cleaned HTML's trailing quoted history folds: Gmail's `gmail_quote`
/// (its `gmail_attr` line is inside it) and Apple's `blockquote type=cite`
/// with the "On … wrote:" line just ahead of it. A quote with the sender's
/// own words after it is an answer in between, and stays; a mail that is
/// nothing but a quote isn't hidden. One walk of the markup, however it's
/// written: a mail can't make this slow.
pub fn quotes(cleaned: &str) -> Option<Quoted> {
    let nodes = nodes(cleaned);
    let spans = spans(cleaned, &nodes);
    // from the last quote back, while only quotes and blank space follow
    let mut first = spans.len();
    while first > 0 {
        let after = spans[first - 1].1 + 1;
        let next = spans.get(first).map_or(nodes.len(), |s| s.0);
        if nodes[after..next].iter().any(|n| words(cleaned, n)) {
            break;
        }
        first -= 1;
    }
    let &(open, close) = spans.get(first)?;
    let (from, reopen) = attributed(cleaned, &nodes, open);
    let fresh = nodes[..from]
        .iter()
        .filter_map(|n| said(cleaned, n))
        .collect::<Vec<_>>()
        .join(" ");
    if fresh.is_empty() {
        return None;
    }
    let reopen = reopen
        .iter()
        .map(|&o| (nodes[o].at, nodes[o].end))
        .collect();
    let mut regions = vec![Region {
        start: nodes[from].at,
        end: nodes[close].end,
        reopen,
    }];
    regions.extend(spans[first + 1..].iter().map(|&(o, c)| Region {
        start: nodes[o].at,
        end: nodes[c].end,
        reopen: Vec::new(),
    }));
    Some(Quoted { regions, fresh })
}

/// Cleaned HTML with each of `regions` behind a ••• fold.
pub fn wrap(cleaned: &str, regions: &[Region]) -> String {
    let mut out = String::with_capacity(cleaned.len() + regions.len() * 64);
    let mut at = 0;
    for r in regions {
        out.push_str(&cleaned[at..r.start]);
        for &(a, b) in r.reopen.iter().rev() {
            out.push_str(&format!("</{}>", name_of(&cleaned[a..b])));
        }
        out.push_str(QUOTE);
        for &(a, b) in &r.reopen {
            out.push_str(&cleaned[a..b]);
        }
        out.push_str(&cleaned[r.start..r.end]);
        out.push_str(END);
        at = r.end;
    }
    out.push_str(&cleaned[at..]);
    out
}

/// Cleaned HTML as the plain text it gives way to, and where in that text
/// the fold at `cut` starts. Read from the whole page, so what an element
/// around the quote hides stays hidden; `None` when the fold is in what's
/// hidden.
pub fn shrunk(cleaned: &str, cut: usize) -> Option<(String, usize)> {
    // a character the mail doesn't have marks the cut
    const FIRST: u32 = 0xe000;
    let mut taken = vec![false; 0x1900];
    for c in cleaned.chars() {
        if let Some(i) = (c as u32).checked_sub(FIRST).filter(|i| *i < 0x1900) {
            taken[i as usize] = true;
        }
    }
    let free = taken.iter().position(|t| !t)?;
    let mark = char::from_u32(FIRST + free as u32)?;
    let text = clean::visible_text(&format!("{}{mark}{}", &cleaned[..cut], &cleaned[cut..]));
    let (fresh, rest) = text.split_once(mark)?;
    let (fresh, rest) = (fresh.trim_end(), rest.trim_start());
    Some((format!("{fresh}\n\n{rest}"), fresh.len() + 2))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Open,
    Close,
    Void,
    Text,
}

/// A tag or a run of text in cleaned markup.
struct Node {
    at: usize,
    end: usize,
    kind: Kind,
    /// How many elements it's in; an element's own tags are at its parent's.
    depth: usize,
    /// A tag's element name; empty for text.
    name: String,
    /// The element it's in.
    parent: Option<usize>,
    /// In an element, itself too, that says `display:none` or
    /// `visibility:hidden`. Colour isn't hiding here: it depends on what's
    /// behind, and a word that might be seen counts.
    hidden: bool,
}

fn name_of(tag: &str) -> String {
    tag.trim_start_matches(['<', '/'])
        .chars()
        .take_while(char::is_ascii_alphanumeric)
        .collect()
}

fn hides(style: &str) -> bool {
    clean::declarations(style)
        .iter()
        .any(|(name, value)| match name.as_str() {
            "display" => value == "none",
            "visibility" => value == "hidden" || value == "collapse",
            _ => false,
        })
}

/// The one walk of cleaned markup, which the serializer has made regular.
fn nodes(html: &str) -> Vec<Node> {
    let mut out: Vec<Node> = Vec::new();
    // the elements open now, as their nodes
    let mut open: Vec<usize> = Vec::new();
    let mut at = 0;
    while at < html.len() {
        let lt = html[at..].find('<').map_or(html.len(), |n| at + n);
        let parent = open.last().copied();
        let hidden = parent.is_some_and(|p| out[p].hidden);
        if lt > at {
            out.push(Node {
                at,
                end: lt,
                kind: Kind::Text,
                depth: open.len(),
                name: String::new(),
                parent,
                hidden,
            });
        }
        if lt == html.len() {
            break;
        }
        let end = lt + clean::tag_end(&html[lt..]);
        let tag = &html[lt..end];
        let name = name_of(tag);
        if tag.starts_with("</") {
            if let Some(i) = open.iter().rposition(|&o| out[o].name == name) {
                open.truncate(i);
            }
            out.push(Node {
                at: lt,
                end,
                kind: Kind::Close,
                depth: open.len(),
                name,
                parent: open.last().copied(),
                hidden,
            });
        } else {
            let hidden = hidden || clean::attribute(tag, "style").is_some_and(|s| hides(&s));
            let void = clean::VOID.contains(&name.as_str());
            if !void {
                open.push(out.len());
            }
            out.push(Node {
                at: lt,
                end,
                kind: if void { Kind::Void } else { Kind::Open },
                depth: open.len() - usize::from(!void),
                name,
                parent,
                hidden,
            });
        }
        at = end;
    }
    out
}

/// A text node's words, if a person may see them.
fn said(html: &str, n: &Node) -> Option<String> {
    if n.kind != Kind::Text || n.hidden {
        return None;
    }
    let text = crate::gmail::decode_entities(&html[n.at..n.end]);
    let words: Vec<&str> = text
        .split(|c: char| c.is_whitespace() || c == '\u{a0}')
        .filter(|w| !w.is_empty())
        .collect();
    (!words.is_empty()).then(|| words.join(" "))
}

fn words(html: &str, n: &Node) -> bool {
    said(html, n).is_some()
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

/// The outermost quotes, each as the nodes of its start tag and its end tag.
fn spans(html: &str, nodes: &[Node]) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut i = 0;
    while i < nodes.len() {
        let n = &nodes[i];
        let tag = &html[n.at..n.end];
        if n.kind == Kind::Open && quote(tag, &name_of(tag)) {
            let close = nodes[i + 1..]
                .iter()
                .position(|m| m.kind == Kind::Close && m.depth == n.depth)
                .map_or(nodes.len() - 1, |p| i + 1 + p);
            spans.push((i, close));
            i = close + 1;
        } else {
            i += 1;
        }
    }
    spans
}

/// Where a person sees a new line start: a `<br>`, or a block's edge.
fn breaks(n: &Node) -> bool {
    !n.hidden && n.kind != Kind::Text && crate::gmail::BLOCKS.contains(&n.name.as_str())
}

/// The node a quote really starts at: the "On … wrote:" line just ahead of
/// it, if the last line before it is one, and the elements of the sender's
/// that line is in, to open again inside the fold. The walk back reads one
/// line and ends: never longer than the page.
fn attributed(html: &str, nodes: &[Node], open: usize) -> (usize, Vec<usize>) {
    let depth = nodes[open].depth;
    let mut line: Vec<String> = Vec::new();
    let mut start = None;
    let mut i = open;
    while i > 0 {
        i -= 1;
        let n = &nodes[i];
        if n.depth < depth {
            break;
        }
        if let Some(w) = said(html, n) {
            line.push(w);
            start = Some(i);
        } else if !line.is_empty() && breaks(n) {
            break;
        }
    }
    line.reverse();
    let line = line.join(" ");
    let Some(start) = start.filter(|_| wrote(&line, &line)) else {
        return (open, Vec::new());
    };
    // the elements beside the quote down to the line, outermost first
    let mut chain = Vec::new();
    let mut up = nodes[start].parent;
    while let Some(p) = up.filter(|&p| nodes[p].depth >= depth) {
        chain.push(p);
        up = nodes[p].parent;
    }
    chain.reverse();
    // an element with nothing of the sender's ahead of the line folds whole
    let mut start = start;
    while let Some(&inner) = chain.last() {
        if nodes[inner..start].iter().any(|n| words(html, n)) {
            break;
        }
        start = inner;
        chain.pop();
    }
    (start, chain)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The markup as it's shown, and where its first fold starts.
    fn quotes(page: &str) -> (String, Option<usize>) {
        match super::quotes(page) {
            Some(q) => (wrap(page, &q.regions), Some(q.regions[0].start)),
            None => (page.to_owned(), None),
        }
    }

    fn plain(text: &str) -> String {
        split(text, plain_cut(text))
    }

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
    fn apples_attribution_line_folds_with_its_quote() {
        let page = clean::clean(
            r#"<div>Sounds good.</div><div>On Nov 6, 2026, Ana wrote:</div><br><blockquote type="cite"><div>Old</div></blockquote>"#,
        )
        .html;
        let (folded, cut) = quotes(&page);
        assert_eq!(clean::visible_text(&page[..cut.unwrap()]), "Sounds good.");
        assert!(
            folded.starts_with(&format!("<div>Sounds good.</div>{QUOTE}<div>On Nov 6")),
            "{folded}"
        );
        assert!(folded.ends_with(&format!("</blockquote>{END}")), "{folded}");
        // as bare text it folds too
        let bare = clean::clean(
            r#"<div>Yes.</div>On Thu, Ana wrote:<br><blockquote type="cite">Old</blockquote>"#,
        )
        .html;
        let cut = quotes(&bare).1.unwrap();
        assert_eq!(clean::visible_text(&bare[..cut]), "Yes.");
        // a line that isn't one stays the sender's
        let said =
            clean::clean(r#"<div>I wrote:</div><blockquote type="cite">Old</blockquote>"#).html;
        assert_eq!(
            clean::visible_text(&said[..quotes(&said).1.unwrap()]),
            "I wrote:"
        );
        // nothing but the line and the quote: nothing of the sender's to show, so no fold
        let bare = clean::clean(
            r#"<div>On Nov 6, 2026, Ana wrote:</div><blockquote type="cite">Old</blockquote>"#,
        )
        .html;
        assert_eq!(quotes(&bare), (bare.clone(), None));
    }

    #[test]
    fn a_mail_of_endless_empty_elements_before_a_quote_reads_in_one_walk() {
        let page = clean::clean(&format!(
            r#"<p>Hi</p>{}<blockquote type="cite">Old</blockquote>"#,
            "<span></span>".repeat(100_000)
        ))
        .html;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(super::quotes(&page).map(|q| q.fresh));
        });
        let fresh = rx.recv_timeout(std::time::Duration::from_secs(30));
        assert_eq!(fresh, Ok(Some("Hi".to_owned())));
    }

    #[test]
    fn words_after_a_quote_count_whatever_their_colour() {
        // white on the black it sits on: a person reads it, so the quote is
        // an answer in between
        let page = clean::clean(
            r#"<div style="background:#000"><p>Hi</p><blockquote type="cite">Old</blockquote><p style="color:#fff">Answer</p></div>"#,
        )
        .html;
        assert_eq!(quotes(&page), (page.clone(), None));
        // and what an element around them hides doesn't count
        let page = clean::clean(
            r#"<p>Hi</p><div style="display:none"><blockquote type="cite">Old</blockquote><p>tracking</p></div>"#,
        )
        .html;
        assert!(quotes(&page).0.contains(QUOTE), "{page}");
    }

    #[test]
    fn an_attribution_after_the_senders_words_in_one_element_folds() {
        let page = clean::clean(
            r#"<div>Sounds good.<br><i>On Nov 6, 2026, Ana wrote:</i></div><blockquote type="cite">Old</blockquote>"#,
        )
        .html;
        let q = super::quotes(&page).unwrap();
        assert_eq!(q.fresh, "Sounds good.");
        assert_eq!(
            wrap(&page, &q.regions),
            format!(
                "<div>Sounds good.<br></div>{QUOTE}<div><i>On Nov 6, 2026, Ana wrote:</i></div><blockquote type=\"cite\">Old</blockquote>{END}"
            )
        );
    }

    #[test]
    fn a_wrote_line_that_isnt_an_attribution_ends_the_look_back() {
        let page = clean::clean(
            r#"<div>On Monday I asked.</div><div>Here is what Ana wrote:</div><blockquote type="cite">Old</blockquote>"#,
        )
        .html;
        let q = super::quotes(&page).unwrap();
        assert_eq!(q.fresh, "On Monday I asked. Here is what Ana wrote:");
        assert!(
            page[q.regions[0].start..].starts_with("<blockquote"),
            "{page}"
        );
    }

    #[test]
    fn words_before_a_quote_count_whatever_their_colour() {
        // the dark theme shows white ink, so a person may read it
        let page = clean::clean(
            r#"<p style="color:#ffffff">Hi</p><blockquote type="cite">Old</blockquote>"#,
        )
        .html;
        assert_eq!(super::quotes(&page).map(|q| q.fresh), Some("Hi".to_owned()));
    }

    #[test]
    fn hidden_words_after_a_quote_dont_keep_it_open() {
        let page = clean::clean(
            r#"<p>Hi</p><blockquote type="cite">Old</blockquote><p style="display:none">tracking</p>"#,
        )
        .html;
        assert!(quotes(&page).0.contains(QUOTE), "{page}");
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
