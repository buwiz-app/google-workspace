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

fn link_ok(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    lower.starts_with("https://") || lower.starts_with("http://") || lower.starts_with("mailto:")
}

/// A `style` value without any declaration that could load something. A
/// backslash goes too: CSS escapes spell `url(` other ways.
fn safe_style(style: &str) -> String {
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
            "image-set(",
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
        .map(|d| d.replace("!important", "").replace("! important", ""))
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

pub fn clean_with(html: &str, inline: &dyn Fn(&str) -> Option<String>) -> Cleaned {
    // ammonia's filter is Send + Sync + 'static, so it reports through shared
    // cells, and the lookups in `inline` happen after it
    let links = Arc::new(Mutex::new(Vec::<String>::new()));
    let images = Arc::new(Mutex::new(Vec::<String>::new()));
    let (l, i) = (links.clone(), images.clone());
    let mut builder = Builder::empty();
    builder.tags(TAGS.iter().copied().collect::<HashSet<_>>());
    builder.generic_attributes(ATTRIBUTES.iter().copied().collect::<HashSet<_>>());
    builder
        .add_tag_attributes("a", &["href"])
        .add_tag_attributes("img", &["src"]);
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
            ("a", "href") if link_ok(value) => {
                let mut links = l.lock().unwrap_or_else(|e| e.into_inner());
                links.push(value.trim().to_owned());
                Some(format!("gws-link:{}", links.len() - 1).into())
            }
            ("a", "href") => None,
            ("img", "src")
                if INLINE_IMAGES
                    .iter()
                    .any(|p| value.len() > p.len() && value[..p.len()].eq_ignore_ascii_case(p)) =>
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
    let mut html = builder.clean(html).to_string();
    let take = |cell: &Arc<Mutex<Vec<String>>>| {
        std::mem::take(&mut *cell.lock().unwrap_or_else(|e| e.into_inner()))
    };
    let (links, images) = (take(&links), take(&images));
    let mut left_out = Vec::new();
    // highest first, so one number is never found inside another
    for (n, url) in images.iter().enumerate().rev() {
        let marker = format!("\"gws-image:{n}\"");
        match inline(url).filter(|data| inline_ok(data)) {
            Some(data) => html = html.replace(&marker, &format!("\"{data}\"")),
            None => {
                html = html.replace(&format!(" src={marker}"), "");
                left_out.push(url.clone());
            }
        }
    }
    left_out.reverse();
    Cleaned {
        html,
        links,
        images: left_out,
    }
}

/// `html` cleaned, remote images left out.
pub fn clean(html: &str) -> Cleaned {
    clean_with(html, &|_| None)
}

#[cfg(test)]
mod tests {
    use super::*;

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
