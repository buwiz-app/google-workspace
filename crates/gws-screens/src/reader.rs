//! A mail's HTML in a sealed web view. The page is written by a stranger, so
//! it gets what `buwiz.preview` gets and one thing more: no script, a policy
//! that loads nothing, and links the plugin's cleaner rewrote to `gws-link:<n>`.
//! A web view may only go to http or https, so nsh cancels that navigation
//! before any request and reports it as an error naming the URL. That report is
//! how a press reaches the screen; nothing here navigates.

use nsh_core::element::{
    Element as _, IntoReactive, Styled as _, WebNavigation, WebView, element_ref, web_view_html,
};

/// The policy a mail's page gets, before anything in it.
pub const MAIL_CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; img-src data:; font-src data:; form-action 'none'; base-uri 'none'; frame-src 'none'; object-src 'none'; media-src 'none'";

/// The page's look, in the Bot's theme. Dark overrides the mail's own colours,
/// inline ones too: a mail written for a white page must not light up a dark
/// app. Images keep theirs.
fn theme(dark: bool) -> &'static str {
    if dark {
        "html,body{background:#1d1d1d!important;color:#f0f0f0!important}\
         *{color:#f0f0f0!important;background-color:transparent!important;background-image:none!important;border-color:#3a3a3a!important}\
         a[href],a[href] *{color:#7fb2ff!important}\
         .gws-meta,.gws-meta *{color:#a0a0a0!important}\
         details>summary>.gws-snip,details>summary>.gws-date,details.gws-more>summary{color:#a0a0a0!important}\
         details.gws-quote>summary{background-color:#3a3a3a!important;color:#d0d0d0!important}"
    } else {
        "html,body{background:#fff;color:#1c1c1c}\
         .gws-meta{color:#5c5c5c}\
         details.gws-msg,details.gws-more>summary{border-color:#e5e5e2}\
         details>summary>.gws-snip,details>summary>.gws-date,details.gws-more>summary{color:#5f6368}\
         details.gws-quote>summary{background:#e8eaed;color:#3c4043}"
    }
}

/// The plugin's folds, as Gmail draws them: a message is a row of who, the
/// start of what they said and when, with no disclosure triangle; quoted
/// history is a ••• pill. Each rule names `details` or `summary`, which no
/// mail can write, so a mail's own `class` can't dress up as one.
const FOLDS: &str = "details>summary{list-style:none;cursor:default}\
     details>summary::-webkit-details-marker{display:none}\
     details>summary::marker{content:\"\"}\
     details.gws-msg{border-top:1px solid;padding-bottom:4px}\
     details.gws-msg>summary{display:block;padding:10px 0;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}\
     details.gws-msg>summary>.gws-date{float:right;margin-left:12px;font-size:12px}\
     details.gws-msg>summary>.gws-from{font-weight:600;margin-right:12px}\
     details.gws-msg[open]>summary>.gws-snip{display:none}\
     details.gws-more>summary{display:block;padding:8px 0;border-top:1px solid;font-size:12px;text-align:center}\
     details.gws-more[open]>summary{display:none}\
     details.gws-quote>summary{display:inline-block;margin:8px 0;padding:0 8px;border-radius:8px;font-size:11px;line-height:14px;letter-spacing:1px}";

/// `html` with [`MAIL_CSP`] ahead of everything else in it, in the Bot's theme.
pub fn seal(html: &str, dark: bool) -> String {
    let theme = theme(dark);
    format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><meta http-equiv=\"Content-Security-Policy\" content=\"{MAIL_CSP}\"><style>html,body{{margin:0;padding:12px;font:14px -apple-system,sans-serif;overflow-wrap:anywhere}}img{{max-width:100%;height:auto}}.gws-meta{{font-size:12px;margin:0 0 10px}}{FOLDS}{theme}</style></head><body>{html}</body></html>"
    )
}

/// The link number in nsh's report of a refused navigation:
/// `navigation to gws-link:<n> is not granted`.
pub fn link_of(report: &str) -> Option<usize> {
    report
        .strip_prefix("navigation to gws-link:")?
        .strip_suffix(" is not granted")?
        .parse()
        .ok()
}

/// Whether `url` is still this page: a string loaded with no base URL is
/// about:blank, and a fragment of it stays in it.
fn own(url: &str) -> bool {
    url.is_empty() || url.starts_with("about:")
}

/// The web view for a sealed page; `on_link` gets a pressed link's number.
/// Anything that starts to leave the page is stopped.
pub fn view(html: impl IntoReactive<String>, on_link: impl Fn(usize) + 'static) -> WebView {
    let page = element_ref();
    web_view_html(html, None)
        .element_ref(&page)
        .java_script_enabled(false)
        .flex_1()
        .on_load_start(move |n: &WebNavigation| {
            if !own(&n.url) {
                page.stop_loading();
            }
        })
        .on_error(move |report: &str| {
            if let Some(link) = link_of(report) {
                on_link(link);
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_follows_the_bots_theme_whatever_the_mail_says() {
        let dark = seal(r#"<p style="background:#ffffff;color:#000">hi</p>"#, true);
        assert!(
            dark.contains("background:#1d1d1d!important")
                && dark.contains("background-color:transparent!important")
                && dark.contains("*{color:#f0f0f0!important"),
            "{dark}"
        );
        assert!(
            !theme(true).contains("#fff"),
            "no white page in the dark theme"
        );
        assert!(
            theme(true).contains("a[href]") && !theme(true).contains("a,a"),
            "a dead link is plain text, not blue"
        );
        assert!(seal("x", false).contains("html,body{background:#fff"));
    }

    #[test]
    fn folds_look_like_gmails_rows_in_both_themes() {
        for dark in [false, true] {
            let page = seal("x", dark);
            assert!(
                page.contains("details>summary::-webkit-details-marker{display:none}")
                    && page.contains("details>summary{list-style:none"),
                "no disclosure triangle"
            );
            assert!(page.contains("details.gws-msg[open]>summary>.gws-snip{display:none}"));
            assert!(page.contains("details.gws-more[open]>summary{display:none}"));
            let pill = if dark {
                "details.gws-quote>summary{background-color:#3a3a3a!important"
            } else {
                "details.gws-quote>summary{background:#e8eaed"
            };
            assert!(page.contains(pill), "the ••• pill shows on {dark}");
            // the folds' rules come after the mail's base, so the theme wins
            assert!(
                page.find("details.gws-quote>summary{display").unwrap() < page.find(pill).unwrap()
            );
        }
        // every fold rule is on markup only the plugin writes
        for rule in FOLDS.split('}').filter(|r| !r.is_empty()) {
            assert!(rule.starts_with("details"), "{rule}");
        }
    }

    #[test]
    fn the_policy_comes_before_anything_in_the_mail() {
        let page = seal("<p>hi</p>", false);
        let policy = page.find("Content-Security-Policy").unwrap();
        assert!(policy < page.find("<p>hi</p>").unwrap());
        assert!(
            MAIL_CSP.contains("default-src 'none'")
                && MAIL_CSP.contains("img-src data:")
                && !MAIL_CSP.contains("script-src")
        );
    }

    #[test]
    fn only_a_gws_link_is_a_link() {
        assert_eq!(link_of("navigation to gws-link:7 is not granted"), Some(7));
        assert_eq!(link_of("navigation to about:blank is not granted"), None);
        assert_eq!(
            link_of("navigation to https://a.example/gws-link:7 is not granted"),
            None
        );
        assert_eq!(link_of("navigation to gws-link:x is not granted"), None);
        assert_eq!(link_of("gws-link:7"), None);
    }

    #[test]
    fn only_the_blank_page_is_this_page() {
        assert!(own("") && own("about:blank"));
        assert!(!own("gws-link:1") && !own("https://a.example/"));
    }
}
