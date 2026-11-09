//! A mail's HTML, made safe to show. Every body goes through [`clean`] before
//! a web view sees it: the view's policy and its JavaScript switch don't stop
//! `<meta http-equiv=refresh>`, which navigates, so the markup itself has to go.
//!
//! An allowlist, on ammonia's HTML5 parser. What comes out loads nothing and
//! goes nowhere: links become `gws-link:<n>`, which no network serves (the real URLs come back in
//! [`Cleaned::links`], for the person to confirm), remote images are dropped
//! and counted, and no `style` keeps a `url()`.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use ammonia::{Builder, UrlRelative};
use url::Url;

use crate::gmail;

/// What [`clean`] makes of a body.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Cleaned {
    pub html: String,
    /// Link `n` is `links[n]`.
    pub links: Vec<String>,
    /// The remote images left out: what "Show images" fetches.
    pub images: Vec<String>,
}

const TAGS: &[&str] = &[
    "a",
    "abbr",
    "b",
    "big",
    "blockquote",
    "br",
    "caption",
    "center",
    "cite",
    "code",
    "col",
    "colgroup",
    "dd",
    "del",
    "div",
    "dl",
    "dt",
    "em",
    "font",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "hr",
    "i",
    "img",
    "ins",
    "kbd",
    "li",
    "mark",
    "ol",
    "p",
    "pre",
    "q",
    "s",
    "small",
    "span",
    "strike",
    "strong",
    "sub",
    "sup",
    "table",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "tr",
    "tt",
    "u",
    "ul",
];
const ATTRIBUTES: &[&str] = &[
    "align",
    "alt",
    "bgcolor",
    "border",
    "cellpadding",
    "cellspacing",
    "class",
    "color",
    "colspan",
    "dir",
    "face",
    "height",
    "lang",
    "rowspan",
    "size",
    "style",
    "title",
    "valign",
    "width",
];
/// `data:` images a web view may show. Not SVG: it can carry markup of its own.
const INLINE_IMAGES: &[&str] = &[
    "data:image/png;",
    "data:image/jpeg;",
    "data:image/gif;",
    "data:image/webp;",
];

/// Characters that make an address read differently from where it goes.
fn deceptive(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{ad}'
                | '\u{61c}'
                | '\u{180e}'
                | '\u{200b}'..='\u{200f}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2060}'..='\u{2069}'
                | '\u{feff}'
        )
}

/// A link's address in the one form the person confirms and the browser
/// opens: a real host, in ASCII, and no `name@` in front of it to pass for one.
fn link_of(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.chars().any(deceptive) {
        return None;
    }
    let url = Url::parse(raw).ok()?;
    let plain = url.username().is_empty() && url.password().is_none();
    match url.scheme() {
        "https" | "http" if url.host_str().is_some() && plain => Some(url.into()),
        "mailto" => Some(url.into()),
        _ => None,
    }
}

/// Whether `value` starts with `prefix`, whatever its case. Bytes, not a
/// slice of the text: a mail's value can have a character across the cut.
fn starts_with_any_case(value: &str, prefix: &str) -> bool {
    value
        .as_bytes()
        .get(..prefix.len())
        .is_some_and(|start| start.eq_ignore_ascii_case(prefix.as_bytes()))
}

/// `style` without its CSS comments: `display:/**/none` hides as
/// `display:none` does.
fn no_comments(style: &str) -> String {
    let mut out = String::with_capacity(style.len());
    let mut rest = style;
    while let Some(at) = rest.find("/*") {
        out.push_str(&rest[..at]);
        rest = rest[at + 2..]
            .find("*/")
            .map_or("", |end| &rest[at + 2 + end + 2..]);
    }
    out.push_str(rest);
    out
}

/// A declaration without `!important`, in any case or spacing.
fn no_important(declaration: &str) -> String {
    let lower = declaration.to_ascii_lowercase();
    let mut out = String::with_capacity(declaration.len());
    let mut at = 0;
    while let Some(bang) = lower[at..].find('!').map(|n| at + n) {
        out.push_str(&declaration[at..bang]);
        let after = lower[bang + 1..].trim_start();
        if after.starts_with("important") {
            at = declaration.len() - after.len() + "important".len();
        } else {
            out.push('!');
            at = bang + 1;
        }
    }
    out.push_str(&declaration[at..]);
    out
}

/// A `style` value without any declaration that could load something. A
/// backslash goes too: CSS escapes spell `url(` other ways.
fn safe_style(style: &str) -> String {
    let style = no_comments(style);
    let risky = |d: &&str| {
        let d: String = d
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>()
            .to_ascii_lowercase();
        [
            "\\",
            "url(",
            "://",
            "image(",
            "image-set(",
            "cross-fade(",
            "expression(",
            "@import",
            "behavior:",
            "-moz-binding",
        ]
        .iter()
        .any(|w| d.contains(w))
    };
    // a mail's `!important` must not outrank the page's theme
    style
        .split(';')
        .filter(|d| !risky(d))
        .map(no_important)
        .collect::<Vec<_>>()
        .join(";")
}

/// `html` cleaned. `inline` swaps a remote image's URL for a `data:` one, once
/// the person has asked for images.
fn inline_ok(data: &str) -> bool {
    INLINE_IMAGES.iter().any(|p| data.starts_with(p))
        && data
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"+/=;:,-".contains(&b))
}

/// What the cleaner found: ammonia's filter is Send + Sync + 'static, so it
/// reports through shared cells.
type Found = Arc<Mutex<Vec<String>>>;

/// The one set of rules, for what the reader shows and what the model reads.
fn builder(links: Found, images: Found) -> Builder<'static> {
    let (l, i) = (links, images);
    let mut builder = Builder::empty();
    builder.tags(TAGS.iter().copied().collect::<HashSet<_>>());
    builder.generic_attributes(ATTRIBUTES.iter().copied().collect::<HashSet<_>>());
    builder
        .add_tag_attributes("a", &["href"])
        .add_tag_attributes("img", &["src"])
        // Apple Mail's quoted history, which the reader folds
        .add_tag_attributes("blockquote", &["type"]);
    // let through to the filter below, which keeps nothing of them as they are
    builder.url_schemes(
        ["http", "https", "mailto", "data", "cid"]
            .into_iter()
            .collect(),
    );
    builder.url_relative(UrlRelative::Deny);
    builder.link_rel(None);
    builder.strip_comments(true);
    builder.clean_content_tags(
        [
            "script",
            "style",
            "title",
            "head",
            "noscript",
            "template",
            "textarea",
            "xmp",
            "iframe",
            "noembed",
            "noframes",
            "plaintext",
            "svg",
            "math",
            "object",
            "embed",
            "applet",
            "audio",
            "video",
        ]
        .into_iter()
        .collect(),
    );
    builder.attribute_filter(
        move |element, attribute, value| match (element, attribute) {
            ("a", "href") => link_of(value).map(|url| {
                let mut links = l.lock().unwrap_or_else(|e| e.into_inner());
                links.push(url);
                format!("gws-link:{}", links.len() - 1).into()
            }),
            ("img", "src")
                if INLINE_IMAGES
                    .iter()
                    .any(|p| value.len() > p.len() && starts_with_any_case(value, p)) =>
            {
                Some(value.into())
            }
            ("img", "src") => {
                let mut images = i.lock().unwrap_or_else(|e| e.into_inner());
                images.push(value.trim().to_owned());
                Some(format!("gws-image:{}", images.len() - 1).into())
            }
            (_, "style") => Some(safe_style(value).into()),
            _ => Some(value.into()),
        },
    );
    builder
}

/// Walks `html` once, for each `"<prefix><n>"` in it asking `swap` what goes
/// there. `swap` writes it and says true, or says false to leave it as it was.
pub fn rewrite(
    html: &str,
    prefix: &str,
    mut swap: impl FnMut(usize, &mut String) -> bool,
) -> String {
    let marker = format!("\"{prefix}");
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(at) = rest.find(&marker) {
        let (before, found) = rest.split_at(at);
        out.push_str(before);
        let digits = &found[marker.len()..];
        let end = digits.find(|c: char| !c.is_ascii_digit());
        let number = end
            .filter(|&e| e > 0 && digits[e..].starts_with('"'))
            .and_then(|e| Some((digits[..e].parse().ok()?, marker.len() + e + 1)));
        match number {
            Some((n, len)) if swap(n, &mut out) => rest = &found[len..],
            _ => {
                out.push_str(&marker);
                rest = &found[marker.len()..];
            }
        }
    }
    out.push_str(rest);
    out
}

pub fn clean_with(html: &str, inline: &dyn Fn(&str) -> Option<String>) -> Cleaned {
    let (links, images): (Found, Found) = Default::default();
    let html = builder(links.clone(), images.clone())
        .clean(html)
        .to_string();
    let take = |cell: &Found| std::mem::take(&mut *cell.lock().unwrap_or_else(|e| e.into_inner()));
    let (links, images) = (take(&links), take(&images));
    // the lookups in `inline` wait until here, out of the filter
    let found: Vec<Option<String>> = images
        .iter()
        .map(|url| inline(url).filter(|data| inline_ok(data)))
        .collect();
    let html = rewrite(&html, "gws-image:", |n, out| match found.get(n) {
        Some(Some(data)) => {
            out.push('"');
            out.push_str(data);
            out.push('"');
            true
        }
        Some(None) => {
            if out.ends_with(" src=") {
                out.truncate(out.len() - " src=".len());
            }
            true
        }
        None => false,
    });
    let left_out = images
        .into_iter()
        .zip(&found)
        .filter(|(_, data)| data.is_none())
        .map(|(url, _)| url)
        .collect();
    Cleaned {
        html,
        links,
        images: left_out,
    }
}

/// A length a style says is nothing, or next to it.
fn no_size(value: &str, at_most: f32) -> bool {
    let number = value.trim_end_matches(|c: char| c.is_ascii_alphabetic() || c == '%');
    number.parse::<f32>().is_ok_and(|n| n <= at_most)
}

type Rgba = [u8; 4];

/// A CSS colour as red, green, blue and alpha, for the forms mail uses.
fn colour(value: &str) -> Option<Rgba> {
    let value = value.trim().to_ascii_lowercase();
    match value.as_str() {
        "transparent" => return Some([0, 0, 0, 0]),
        "black" => return Some([0, 0, 0, 255]),
        "white" | "snow" | "ivory" | "ghostwhite" | "whitesmoke" | "floralwhite" | "mintcream"
        | "azure" | "aliceblue" | "seashell" | "honeydew" | "lavenderblush" => {
            return Some([255, 255, 255, 255]);
        }
        _ => {}
    }
    if let Some(inside) = value
        .strip_prefix("hsla(")
        .or_else(|| value.strip_prefix("hsl("))
    {
        return hsl(inside.strip_suffix(')')?);
    }
    if let Some(hex) = value.strip_prefix('#') {
        let digit = |c: char| c.to_digit(16).map(|d| d as u8);
        let digits: Vec<u8> = hex.chars().map(digit).collect::<Option<_>>()?;
        let wide: Vec<u8> = match digits.len() {
            3 | 4 => digits.iter().map(|d| d * 17).collect(),
            6 | 8 => digits.chunks(2).map(|p| p[0] * 16 + p[1]).collect(),
            _ => return None,
        };
        return Some([wide[0], wide[1], wide[2], *wide.get(3).unwrap_or(&255)]);
    }
    let inside = value
        .strip_prefix("rgba(")
        .or_else(|| value.strip_prefix("rgb("))?
        .strip_suffix(')')?;
    let parts: Vec<f32> = inside
        .split([',', '/', ' '])
        .filter(|p| !p.is_empty())
        .map(|p| p.parse().ok())
        .collect::<Option<_>>()?;
    let alpha = parts.get(3).map_or(255.0, |a| a * 255.0);
    Some([
        *parts.first()? as u8,
        *parts.get(1)? as u8,
        *parts.get(2)? as u8,
        alpha as u8,
    ])
}

/// A colour from `hsl()`'s arguments, near enough for [`unreadable`]: mail uses
/// it for white and black, so only the lightness and alpha are read.
fn hsl(inside: &str) -> Option<Rgba> {
    let parts: Vec<&str> = inside
        .split([',', '/', ' '])
        .filter(|p| !p.is_empty())
        .collect();
    let light: f32 = parts.get(2)?.trim_end_matches('%').parse().ok()?;
    let alpha = parts
        .get(3)
        .and_then(|a| a.parse::<f32>().ok())
        .unwrap_or(1.0);
    let level = (light / 100.0 * 255.0) as u8;
    Some([level, level, level, (alpha * 255.0) as u8])
}

/// A declaration's words, leaving a function's arguments in one piece.
fn words(value: &str) -> Vec<&str> {
    let (mut words, mut depth, mut start) = (Vec::new(), 0, 0);
    for (at, c) in value.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ' ' if depth == 0 => {
                words.push(&value[start..at]);
                start = at + 1;
            }
            _ => {}
        }
    }
    words.push(&value[start..]);
    words
}

/// Whether text in `ink` can't be told from `paper`.
fn unreadable(ink: Rgba, paper: Rgba) -> bool {
    ink[3] < 13 || (0..3).all(|c| ink[c].abs_diff(paper[c]) <= 16)
}

/// A `style`, reduced to the declarations a browser would read.
fn declarations(style: &str) -> Vec<(String, String)> {
    let style = no_comments(style).to_ascii_lowercase();
    style
        .split(';')
        .filter_map(|d| d.split_once(':'))
        .map(|(name, value)| {
            let value = no_important(value);
            let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
            (name.trim().to_owned(), value)
        })
        .collect()
}

/// Whether a `style` hides what's in it from a person looking at the page.
fn hidden(style: &str) -> bool {
    let far_off = |v: &str| {
        v.strip_prefix('-')
            .is_some_and(|v| !no_size(v, 100.0) && v.starts_with(|c: char| c.is_ascii_digit()))
    };
    declarations(style)
        .iter()
        .any(|(name, value)| match name.as_str() {
            "display" => value == "none",
            "visibility" => value == "hidden" || value == "collapse",
            "opacity" => no_size(value, 0.05),
            "font-size" => no_size(value, 1.0),
            "font" => words(value).iter().any(|w| {
                let size = w.split('/').next().unwrap_or_default();
                size.starts_with(|c: char| c.is_ascii_digit()) && no_size(size, 1.0)
            }),
            "mso-hide" => value == "all",
            "width" | "height" | "max-width" | "max-height" => no_size(value, 0.0),
            "clip" => value.starts_with("rect(0"),
            "clip-path" => value.starts_with("inset(50%") || value.starts_with("inset(100%"),
            "transform" => value.contains("scale(0"),
            "text-indent" | "left" | "top" | "margin-left" | "margin-top" => far_off(value),
            _ => false,
        })
}

/// The last value `style` gives any of `names`.
fn declared(style: &str, names: &[&str]) -> Option<String> {
    declarations(style)
        .into_iter()
        .rev()
        .find(|(name, _)| names.contains(&name.as_str()))
        .map(|(_, value)| value)
}

/// The reader's light page, which a mail with no background of its own is on.
const PAGE: Rgba = [255, 255, 255, 255];
/// Elements with no closing tag.
pub(crate) const VOID: &[&str] = &["area", "br", "col", "hr", "img", "wbr"];

/// A tag of cleaned markup, which the serializer has made regular: every
/// attribute in double quotes with any `"` inside it escaped, `<` escaped in
/// text, and each element closed. Its end is the first `>` outside quotes.
pub(crate) fn tag_end(html: &str) -> usize {
    let mut quoted = false;
    html.bytes()
        .position(|b| {
            quoted ^= b == b'"';
            b == b'>' && !quoted
        })
        .map_or(html.len(), |at| at + 1)
}

/// An attribute of a cleaned tag.
pub(crate) fn attribute(tag: &str, name: &str) -> Option<String> {
    let value = tag.split_once(&format!(" {name}=\""))?.1;
    Some(gmail::decode_entities(value.split('"').next()?))
}

/// The tag's own background, if it gives one that can be read.
fn background(tag: &str) -> Option<Rgba> {
    let style = attribute(tag, "style").unwrap_or_default();
    let value = declared(&style, &["background-color", "background"])
        .or_else(|| attribute(tag, "bgcolor"))?;
    words(&value)
        .iter()
        .find_map(|w| colour(w))
        .filter(|c| c[3] > 0)
}

/// The tag's own text colour: its style's, else `<font color>`'s.
fn ink(tag: &str) -> Option<Rgba> {
    let style = attribute(tag, "style").unwrap_or_default();
    let value = declared(&style, &["-webkit-text-fill-color"])
        .or_else(|| declared(&style, &["color"]))
        .or_else(|| attribute(tag, "color"))?;
    colour(&value)
}

/// `html` as plain text, read from the markup the reader shows, without what
/// that markup plainly hides. No filter can catch every way to hide text, so
/// this narrows what a mail can slip to the model and proves nothing: what
/// protects the model is that each result says these are the sender's words.
pub fn visible_text(html: &str) -> String {
    let (links, images): (Found, Found) = Default::default();
    let html = builder(links, images).clean(html).to_string();
    let mut text = String::new();
    // inside a hidden element: its name, and how many of that name are open
    let mut hiding: Option<(String, usize)> = None;
    // the backgrounds and text colours of the elements open now, innermost last
    let mut open: Vec<(String, Option<Rgba>, Option<Rgba>)> = Vec::new();
    let mut rest = html.as_str();
    while !rest.is_empty() {
        let lt = rest.find('<').unwrap_or(rest.len());
        if hiding.is_none() {
            text.push_str(&gmail::decode_entities(&rest[..lt]).replace(['\n', '\r'], " "));
        }
        rest = &rest[lt..];
        if rest.is_empty() {
            break;
        }
        let end = tag_end(rest);
        let (tag, after) = rest.split_at(end);
        rest = after;
        let closing = tag.starts_with("</");
        let name: String = tag
            .trim_start_matches(['<', '/'])
            .chars()
            .take_while(char::is_ascii_alphanumeric)
            .collect();
        let void = VOID.contains(&name.as_str());
        if let Some((hid, open)) = &mut hiding {
            if *hid == name && !void {
                *open = if closing { *open - 1 } else { *open + 1 };
            }
            if *open == 0 {
                hiding = None;
            }
            continue;
        }
        if closing {
            if let Some(at) = open.iter().rposition(|(n, ..)| *n == name) {
                open.truncate(at);
            }
        } else if !void {
            let paper = background(tag)
                .or_else(|| open.iter().rev().find_map(|(_, bg, _)| *bg))
                .unwrap_or(PAGE);
            // a colour is inherited, and can meet a background further in
            let text = ink(tag).or_else(|| open.iter().rev().find_map(|(.., ink)| *ink));
            let unseen = text.is_some_and(|ink| unreadable(ink, paper));
            if unseen || attribute(tag, "style").is_some_and(|s| hidden(&s)) {
                hiding = Some((name, 1));
                continue;
            }
            open.push((name.clone(), background(tag), ink(tag)));
        }
        if gmail::BLOCKS.contains(&name.as_str()) {
            text.push('\n');
        } else if name == "td" || name == "th" {
            text.push(' ');
        }
    }
    gmail::tidy(&text)
}

/// `html` cleaned, remote images left out.
pub fn clean(html: &str) -> Cleaned {
    clean_with(html, &|_| None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_character_across_a_cut_is_no_panic() {
        // each puts a multi-byte character where a prefix's end falls
        for src in [
            "data:image/pñg;base64,AA",
            "data:image/jpe€;base64,AA",
            "data:imag😀/gif;base64,AA",
            "dataé",
            "€€€€€€€€€€€€€€€€",
        ] {
            let page = format!(r#"<p>before</p><img src="{src}"><a href="{src}">x</a>"#);
            assert!(clean(&page).html.contains("before"), "{src}");
            assert!(visible_text(&page).contains("before"), "{src}");
        }
        let cut = "<p style=\"opacity:0\">é€😀</p><p>ñ & é</p>";
        assert_eq!(visible_text(cut), "ñ & é");
    }

    #[test]
    fn the_model_reads_only_what_a_person_sees() {
        for hide in [
            "display:none",
            "DISPLAY : NONE",
            "display:none!important",
            "visibility:hidden",
            "opacity:0",
            "opacity:.01",
            "font-size:0",
            "font-size:0px",
            "font-size:1px",
            "display:/**/none",
            "visibility:/* x */hidden",
            "opacity:/**/0",
            "display:none!IMPORTANT",
            "font:0/0 a",
            "font:italic 0/0 a",
            "font:normal 0px arial",
            "color:hsl(0,0%,100%)",
            "color:whitesmoke",
            "-webkit-text-fill-color:transparent",
            "-webkit-text-fill-color:#fff",
            "background:#123 no-repeat;color:#123",
            "mso-hide:all",
            "color:#fff",
            "color:#FFFFFF",
            "color:white",
            "color:rgb(255,255,255)",
            "color:#fefefe",
            "color:rgba(0,0,0,0)",
            "color:#000;background:#000",
            "color:#123456;background-color:#123456",
            "height:0;overflow:hidden",
            "max-height:0px",
            "width:0",
            "color:transparent",
            "position:absolute;left:-9999px",
            "text-indent:-9999px",
            "clip:rect(0,0,0,0)",
            "transform:scale(0)",
        ] {
            let page = format!(
                r#"<p>seen</p><div style="{hide}"><p>IGNORE <b>this</b></p><div>too</div></div><p>after</p>"#
            );
            assert_eq!(visible_text(&page), "seen\n\nafter", "{hide}");
        }
        // a quoted `>` doesn't end the tag early, and no attribute is text
        let quoted = r#"<div title="a>b" style="display:none">IGNORE</div><span title="x>IGNORE">seen</span>"#;
        assert_eq!(visible_text(quoted), "seen");
        // a font's colour counts, and what it's on
        assert_eq!(
            visible_text(r##"<font color="#ffffff">IGNORE</font><p>seen</p>"##),
            "seen"
        );
        assert_eq!(
            visible_text(
                r##"<table bgcolor="#000000"><tr><td><font color="#fff">seen</font></td></tr></table>"##
            ),
            "seen"
        );
        assert_eq!(
            visible_text(r##"<div style="background:#123"><p style="color:white">seen</p></div>"##),
            "seen"
        );
        assert_eq!(
            visible_text(
                r##"<div style="background:#000"><p style="color:#000">IGNORE</p></div><p>seen</p>"##
            ),
            "seen"
        );
        // a colour carries down onto a background further in
        assert_eq!(
            visible_text(
                r##"<div style="color:#123456"><p style="background:#123456">IGNORE</p></div><p>seen</p>"##
            ),
            "seen"
        );
        // on a shorthand's background it's readable
        assert_eq!(
            visible_text(
                r##"<div style="background:#000 no-repeat"><p style="color:#fff">seen</p></div>"##
            ),
            "seen"
        );
        // a size in a real font stays, and so does a colour nobody can read here
        assert_eq!(
            visible_text(r##"<p style="font:700 12px arial">seen</p>"##),
            "seen"
        );
        assert_eq!(
            visible_text(r##"<p style="color:lab(100 0 0)">seen</p>"##),
            "seen"
        );
        // what the reader shows stays
        assert_eq!(
            visible_text(r#"<p style="color:#123;font-size:12px;height:20px">seen</p>"#),
            "seen"
        );
    }

    #[test]
    fn an_address_that_reads_differently_is_no_link() {
        for bad in [
            "https://real.example/\u{202e}elpmaxe.live",
            "https://real.example\u{2066}.evil.example/",
            "https://real\u{200b}.example/",
            "https://real.example/\u{7}",
            "https://real.example/\u{feff}",
            "https://real.example/\u{ad}x",
            "https://real.example@evil.example/",
            "https://user:pw@evil.example/",
            "javascript:alert(1)",
            "https://",
        ] {
            assert_eq!(link_of(bad), None, "{bad:?}");
        }
        assert_eq!(
            link_of(" https://a.example/x?y=1 ").as_deref(),
            Some("https://a.example/x?y=1")
        );
        assert_eq!(
            link_of("https://пример.com/").as_deref(),
            Some("https://xn--e1afmkfd.com/")
        );
        assert_eq!(
            link_of("mailto:ana@example.com").as_deref(),
            Some("mailto:ana@example.com")
        );
    }

    #[test]
    fn no_important_survives_in_any_case() {
        for style in [
            "color:red!IMPORTANT",
            "color:red ! Important",
            "color:red!/**/important",
        ] {
            assert!(
                !safe_style(style).to_ascii_lowercase().contains("important"),
                "{style}"
            );
        }
        assert_eq!(safe_style("color:/* x */red"), "color:red");
    }

    #[test]
    fn image_functions_are_risky_too() {
        assert_eq!(safe_style("background:image(a.png);color:red"), "color:red");
        assert_eq!(safe_style("background:cross-fade(a.png,b.png)"), "");
    }

    #[test]
    fn a_page_of_many_links_and_images_cleans_in_one_pass() {
        let page = "<a href=\"https://a.example/\">x</a><img src=\"https://a.example/i.png\">"
            .repeat(20_000);
        let at = std::time::Instant::now();
        let out = clean(&page);
        assert_eq!((out.links.len(), out.images.len()), (20_000, 20_000));
        assert!(!out.html.contains("gws-image"));
        assert!(at.elapsed().as_secs() < 20, "took {:?}", at.elapsed());
    }

    #[test]
    fn a_marker_nothing_issued_stays_as_it_was() {
        let html = r#"<a href="gws-link:0">a</a> "gws-link:9" "gws-link:x" "gws-link:"#;
        let out = rewrite(html, "gws-link:", |n, out| {
            let known = n == 0;
            if known {
                out.push_str("\"gws-link:5\"");
            }
            known
        });
        assert_eq!(
            out,
            r#"<a href="gws-link:5">a</a> "gws-link:9" "gws-link:x" "gws-link:"#
        );
    }

    const EVIL: &str = "evil.example";

    /// Each way a page can reach out or run something, one per line. None of
    /// them may leave the host's name in the output.
    const HOSTILE: &[&str] = &[
        r#"<meta http-equiv="refresh" content="0;url=https://evil.example/a">"#,
        r#"<script>new Image().src='https://evil.example/b'</script>"#,
        r#"<img src="https://evil.example/pixel.png">"#,
        r#"<img src=x onerror="location='https://evil.example/c'">"#,
        r#"<img srcset="https://evil.example/d.png 2x">"#,
        r#"<iframe src="https://evil.example/e"></iframe>"#,
        r#"<object data="https://evil.example/f"></object>"#,
        r#"<embed src="https://evil.example/g">"#,
        r#"<form action="https://evil.example/h"><input name=a></form>"#,
        r#"<link rel=stylesheet href="https://evil.example/i.css">"#,
        r#"<style>@import url(https://evil.example/j.css); body{background:url(https://evil.example/k)}</style>"#,
        r#"<div style="background:url(https://evil.example/l)">x</div>"#,
        r#"<div style="background:u\72l(https://evil.example/m)">x</div>"#,
        r#"<div style="background-image:image-set('https://evil.example/n' 1x)">x</div>"#,
        r#"<table background="https://evil.example/o"><tr><td background="https://evil.example/p">x</td></tr></table>"#,
        r#"<base href="https://evil.example/">"#,
        r#"<svg><image href="https://evil.example/q"/></svg>"#,
        r#"<svg onload="fetch('https://evil.example/r')"></svg>"#,
        r#"<video poster="https://evil.example/s" src="https://evil.example/t"></video>"#,
        r#"<audio src="https://evil.example/u"></audio>"#,
        r#"<body background="https://evil.example/v">"#,
        r#"<a href="javascript:location='https://evil.example/w'">x</a>"#,
        r#"<a href="data:text/html,<script>top.location='https://evil.example/x'</script>">x</a>"#,
        r#"<img src="data:image/svg+xml,<svg onload=alert('https://evil.example/y')>">"#,
        r#"<math><mtext><img src="https://evil.example/z"></mtext></math>"#,
        r#"<!--[if gte mso 9]><img src="https://evil.example/aa"><![endif]-->"#,
        r#"<noscript><img src="https://evil.example/bb"></noscript>"#,
        r#"<template><img src="https://evil.example/cc"></template>"#,
    ];

    #[test]
    fn nothing_hostile_survives_in_the_markup() {
        for page in HOSTILE {
            let out = clean(page);
            assert!(!out.html.contains(EVIL), "{page} left {}", out.html);
            for word in [
                "<script",
                "<meta",
                "<iframe",
                "<object",
                "<embed",
                "<form",
                "<link",
                "<base",
                "<svg",
                "<style",
                "onerror",
                "onload",
                "srcset",
                "javascript:",
                "background=",
            ] {
                assert!(
                    !out.html.to_ascii_lowercase().contains(word),
                    "{page} left {word}: {}",
                    out.html
                );
            }
        }
    }

    #[test]
    fn the_spikes_refresh_is_gone() {
        assert_eq!(
            clean(r#"<p>hi</p><meta http-equiv=refresh content="0;url=http://127.0.0.1:8822/x">"#)
                .html,
            "<p>hi</p>"
        );
    }

    #[test]
    fn links_become_numbers_and_their_urls_come_back() {
        let out = clean(
            r#"<a href="https://one.example/a?b=1">one</a> <a href=" mailto:ana@example.com ">two</a> <a href="ftp://x">three</a>"#,
        );
        assert_eq!(
            out.links,
            ["https://one.example/a?b=1", "mailto:ana@example.com"]
        );
        assert!(
            out.html.contains(r#"<a href="gws-link:0">one</a>"#)
                && out.html.contains(r#"<a href="gws-link:1">two</a>"#),
            "{}",
            out.html
        );
        assert!(
            out.html.contains("<a>three</a>"),
            "a link it won't follow is plain text: {}",
            out.html
        );
        assert!(!out.html.contains("one.example"));
    }

    /// A mail can't aim a link at a number of its own: `gws-link:` is ours to
    /// write, so one already in the source goes, and every number we write
    /// is a link the cleaner saw.
    #[test]
    fn a_forged_link_is_refused() {
        for page in [
            r#"<a href="gws-link:0">x</a>"#,
            r#"<a href="GWS-LINK:0">x</a>"#,
            r#"<a href=" gws-link:0 ">x</a>"#,
            "<a href=\"gws-link&#58;0\">x</a>",
        ] {
            let out = clean(page);
            assert!(
                out.links.is_empty() && !out.html.to_ascii_lowercase().contains("gws-link"),
                "{page}: {}",
                out.html
            );
        }
        let out =
            clean(r#"<a href="gws-link:0">forged</a><a href="https://real.example/">real</a>"#);
        assert_eq!(out.links, ["https://real.example/"]);
        assert_eq!(out.html.matches("gws-link:").count(), 1);
        assert!(
            out.html.contains(r#"<a href="gws-link:0">real</a>"#),
            "{}",
            out.html
        );
    }

    #[test]
    fn an_inline_image_cannot_leave_its_attribute() {
        let out = clean_with(r#"<img src="https://a.example/1.png">"#, &|_| {
            Some(r#"data:image/png;base64,AA" onerror="alert(1)"#.to_owned())
        });
        assert!(!out.html.contains("onerror"), "{}", out.html);
    }

    #[test]
    fn remote_images_are_left_out_and_counted_and_inline_ones_stay() {
        let dot = "data:image/gif;base64,R0lGODlhAQABAAAAACw=";
        let out = clean(&format!(
            r#"<img src="https://a.example/1.png" alt="logo"><img src="{dot}"><img src="cid:abc">"#
        ));
        assert_eq!(out.images, ["https://a.example/1.png", "cid:abc"]);
        assert!(
            out.html.contains(dot) && !out.html.contains("a.example") && !out.html.contains("cid:"),
            "{}",
            out.html
        );
        assert!(out.html.contains(r#"alt="logo""#), "{}", out.html);
    }

    #[test]
    fn show_images_swaps_in_only_data_images_and_many_images_dont_collide() {
        let dot = "data:image/png;base64,iVBORw0KGgo=";
        let page: String = (0..12)
            .map(|n| format!(r#"<img src="https://a.example/{n}.png">"#))
            .collect();
        let out = clean_with(&page, &|url| {
            url.ends_with("/10.png").then(|| dot.to_owned())
        });
        assert_eq!(out.html.matches(dot).count(), 1);
        assert_eq!(out.images.len(), 11, "the rest are still out");
        assert!(!out.html.contains("gws-image"), "{}", out.html);
        let evil = clean_with(r#"<img src="https://a.example/1.png">"#, &|_| {
            Some("https://evil.example/x".to_owned())
        });
        assert!(
            !evil.html.contains(EVIL),
            "a fetch can't bring a URL back in: {}",
            evil.html
        );
    }

    #[test]
    fn a_mail_cant_outrank_the_theme() {
        let out = clean(r#"<p style="color:#000 !important;background:#fff!important">x</p>"#);
        assert!(!out.html.contains("important"), "{}", out.html);
    }

    #[test]
    fn layout_survives() {
        let page = r##"<table width="600" cellpadding="4"><tr><td bgcolor="#eeeeee" style="color:#333;padding:4px"><b>Total</b></td></tr></table>"##;
        let out = clean(page);
        for kept in [
            "<table",
            r#"width="600""#,
            r##"bgcolor="#eeeeee""##,
            "color:#333",
            "<b>Total</b>",
        ] {
            assert!(out.html.contains(kept), "{kept}: {}", out.html);
        }
    }
}
