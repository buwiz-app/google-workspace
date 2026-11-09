//! Which way a mail's attachment goes to the Bot: its own viewer, the
//! computer's own app, or neither. The Bot says what it can take at
//! `initialize`, in `capabilities.experimental["buwiz/documents"]`. An older
//! Bot says nothing, and takes what it always took: the Word and Spreadsheet
//! views.
//!
//! The Bot decides the kind from the name on disk, so the name has to end as
//! the bytes are. An image or a PDF is checked by its first bytes, since a
//! stranger's word for it is no proof; the other kinds go only to the app their
//! extension names, which is why that list is short.

use std::sync::Mutex;

use serde_json::Value;

/// What the Bot can do with a file, as it said at `initialize`.
#[derive(Default)]
pub struct Documents(Mutex<Option<Vec<String>>>);

impl Documents {
    /// Keeps the list an `initialize` message carries. Anything but a list of
    /// words is an older Bot's silence.
    pub fn heard(&self, initialize: &Value) {
        let list = initialize
            .pointer("/params/capabilities/experimental/buwiz~1documents")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            });
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = list;
    }

    /// Whether the Bot takes `what`. One that never said takes what every Bot
    /// before this list did.
    pub(crate) fn can(&self, what: &str) -> bool {
        match &*self.0.lock().unwrap_or_else(|e| e.into_inner()) {
            Some(list) => list.iter().any(|w| w == what),
            None => matches!(what, WORD | SHEET),
        }
    }
}

const WORD: &str = "word";
const SHEET: &str = "sheet";
const IMAGE: &str = "image";
const PDF: &str = "pdf";
pub(crate) const OPEN_WITH: &str = "open_with";
pub(crate) const SAVE: &str = "save";

/// A kind of file an attachment can open as.
pub(crate) struct Kind {
    /// What it's called on its way to the Bot.
    pub mime: &'static str,
    /// What else a mail may call it.
    aliases: &'static [&'static str],
    /// Its extensions, the one a name gets first.
    pub exts: &'static [&'static str],
    /// The Bot's view that shows it, if any.
    view: Option<&'static str>,
    /// Whether the computer's own app may have it: what opens it is then
    /// Pages, Numbers, Preview, Calendar or Archive Utility, and nothing runs.
    system: bool,
    /// Its first bytes, for a kind that's checked by them.
    magic: Option<fn(&[u8]) -> bool>,
}

const fn kind(
    mime: &'static str,
    exts: &'static [&'static str],
    view: Option<&'static str>,
) -> Kind {
    Kind {
        mime,
        aliases: &[],
        exts,
        view,
        system: true,
        magic: None,
    }
}

fn png(b: &[u8]) -> bool {
    b.starts_with(b"\x89PNG\r\n\x1a\n")
}

fn jpeg(b: &[u8]) -> bool {
    b.starts_with(b"\xff\xd8\xff")
}

fn gif(b: &[u8]) -> bool {
    b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a")
}

fn webp(b: &[u8]) -> bool {
    b.len() >= 12 && b.starts_with(b"RIFF") && &b[8..12] == b"WEBP"
}

/// An ISO box whose brand is HEIF's.
fn heic(b: &[u8]) -> bool {
    const BRANDS: [&[u8]; 8] = [
        b"heic", b"heix", b"heim", b"heis", b"hevc", b"hevx", b"mif1", b"msf1",
    ];
    b.len() >= 12 && &b[4..8] == b"ftyp" && BRANDS.contains(&&b[8..12])
}

fn pdf(b: &[u8]) -> bool {
    b.starts_with(b"%PDF-")
}

/// Every kind an attachment opens as. Anything else is only saved.
pub(crate) const KINDS: [Kind; 19] = [
    kind(super::files::DOCX, &["docx"], Some(WORD)),
    kind("application/msword", &["doc"], Some(WORD)),
    kind(super::files::XLSX, &["xlsx"], Some(SHEET)),
    kind("application/vnd.ms-excel", &["xls"], Some(SHEET)),
    kind("text/csv", &["csv"], Some(SHEET)),
    Kind {
        magic: Some(png),
        ..kind("image/png", &["png"], Some(IMAGE))
    },
    Kind {
        aliases: &["image/jpg", "image/pjpeg"],
        magic: Some(jpeg),
        ..kind("image/jpeg", &["jpg", "jpeg"], Some(IMAGE))
    },
    Kind {
        magic: Some(gif),
        ..kind("image/gif", &["gif"], Some(IMAGE))
    },
    Kind {
        magic: Some(webp),
        ..kind("image/webp", &["webp"], Some(IMAGE))
    },
    Kind {
        aliases: &["image/heif"],
        magic: Some(heic),
        ..kind("image/heic", &["heic", "heif"], Some(IMAGE))
    },
    Kind {
        aliases: &["application/x-pdf"],
        magic: Some(pdf),
        ..kind("application/pdf", &["pdf"], Some(PDF))
    },
    kind("text/plain", &["txt"], None),
    kind(super::files::PPTX, &["pptx"], None),
    kind("application/vnd.ms-powerpoint", &["ppt"], None),
    kind("application/vnd.apple.pages", &["pages"], None),
    kind("application/vnd.apple.numbers", &["numbers"], None),
    kind("application/vnd.apple.keynote", &["key"], None),
    Kind {
        aliases: &["application/ics"],
        ..kind("text/calendar", &["ics"], None)
    },
    Kind {
        aliases: &["application/x-zip-compressed", "application/x-zip"],
        ..kind("application/zip", &["zip"], None)
    },
];

/// The kind called `mime`, as a row or a mail has it.
pub(crate) fn by_mime(mime: &str) -> Option<&'static Kind> {
    let mime = mime.trim().to_ascii_lowercase();
    KINDS
        .iter()
        .find(|k| k.mime == mime || k.aliases.contains(&mime.as_str()))
}

/// The kind an attachment says it is or, when it says nothing, its name does.
pub(crate) fn declared(mime: &str, ext: Option<&str>) -> Option<&'static Kind> {
    if super::files::NO_TYPE.contains(&mime.trim()) {
        let ext = ext?.to_lowercase();
        return KINDS.iter().find(|k| k.exts.contains(&ext.as_str()));
    }
    by_mime(mime)
}

/// What the bytes show a kind to be. One checked by its bytes is what they
/// are, if they're the same sort of thing: a PNG sent as a JPEG is a PNG. One
/// they don't bear out is nothing that can open.
pub(crate) fn checked(kind: &'static Kind, bytes: &[u8]) -> Option<&'static Kind> {
    if kind.magic.is_none() {
        return Some(kind);
    }
    KINDS
        .iter()
        .find(|k| k.view == kind.view && k.magic.is_some_and(|magic| magic(bytes)))
}

/// The kind the bytes are, of those checked by them, whatever a mail says.
pub(crate) fn sniffed(bytes: &[u8]) -> Option<&'static Kind> {
    KINDS
        .iter()
        .find(|k| k.magic.is_some_and(|magic| magic(bytes)))
}

/// How a kind gets to the person.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Way {
    /// The Bot's own view, through `buwiz/document`.
    Viewer,
    /// The computer's own app, through `buwiz/open_with`.
    System,
    /// This Bot can't, and a newer one could.
    Update,
}

pub(crate) fn way(docs: &Documents, kind: &Kind) -> Way {
    if kind.view.is_some_and(|view| docs.can(view)) {
        Way::Viewer
    } else if kind.system && docs.can(OPEN_WITH) {
        Way::System
    } else {
        Way::Update
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn told(list: Value) -> Documents {
        let docs = Documents::default();
        docs.heard(
            &json!({"params": {"capabilities": {"experimental": {"buwiz/documents": list}}}}),
        );
        docs
    }

    #[test]
    fn an_older_bot_that_says_nothing_takes_word_and_sheets_alone() {
        let docs = Documents::default();
        docs.heard(&json!({"params": {"capabilities": {}}}));
        assert!(docs.can(WORD) && docs.can(SHEET));
        for what in [IMAGE, PDF, OPEN_WITH, SAVE] {
            assert!(!docs.can(what), "{what}");
        }
        let odd = told(json!("image"));
        assert!(!odd.can(IMAGE) && odd.can(WORD), "not a list: said nothing");
    }

    #[test]
    fn a_bot_that_says_takes_what_it_said_and_no_more() {
        let docs = told(json!(["image", "save", 7]));
        assert!(docs.can(IMAGE) && docs.can(SAVE));
        assert!(!docs.can(WORD) && !docs.can(PDF) && !docs.can(OPEN_WITH));
    }

    #[test]
    fn each_kind_goes_its_own_way_by_what_the_bot_can_do() {
        let all = told(json!([
            "word",
            "sheet",
            "image",
            "pdf",
            "open_with",
            "save"
        ]));
        let system = told(json!(["open_with"]));
        let none = told(json!([]));
        for (mime, viewer) in [
            ("image/png", true),
            ("application/pdf", true),
            (super::super::files::DOCX, true),
            ("text/csv", true),
            ("application/zip", false),
            ("text/calendar", false),
            ("text/plain", false),
        ] {
            let kind = by_mime(mime).unwrap();
            let want = if viewer { Way::Viewer } else { Way::System };
            assert_eq!(way(&all, kind), want, "{mime}");
            assert_eq!(way(&system, kind), Way::System, "{mime}");
            assert_eq!(way(&none, kind), Way::Update, "{mime}");
        }
    }

    #[test]
    fn an_image_is_what_its_bytes_say() {
        let png_kind = by_mime("image/png").unwrap();
        let jpeg_kind = by_mime("IMAGE/JPG").unwrap();
        assert_eq!(jpeg_kind.mime, "image/jpeg");
        let png_bytes = b"\x89PNG\r\n\x1a\nrest";
        assert_eq!(checked(png_kind, png_bytes).unwrap().mime, "image/png");
        assert_eq!(checked(jpeg_kind, png_bytes).unwrap().mime, "image/png");
        assert!(checked(png_kind, b"<html>").is_none());
        assert!(
            checked(png_kind, b"%PDF-1.4").is_none(),
            "a PDF isn't an image"
        );
        let pdf_kind = by_mime("application/pdf").unwrap();
        assert!(checked(pdf_kind, png_bytes).is_none(), "nor an image a PDF");
        assert!(checked(pdf_kind, b"%PDF-1.7").is_some());
        assert!(checked(by_mime("image/webp").unwrap(), b"RIFF\0\0\0\0WEBPVP8 ").is_some());
        assert!(checked(by_mime("image/heic").unwrap(), b"\0\0\0\x18ftypheic").is_some());
        assert!(checked(by_mime("image/heic").unwrap(), b"\0\0\0\x18ftypisom").is_none());
        assert!(checked(by_mime("image/gif").unwrap(), b"GIF89a").is_some());
        let zip = by_mime("application/zip").unwrap();
        assert!(
            checked(zip, b"anything").is_some(),
            "the app its name picks"
        );
    }

    #[test]
    fn silence_goes_by_the_name() {
        assert_eq!(
            declared("application/octet-stream", Some("PNG"))
                .unwrap()
                .mime,
            "image/png"
        );
        assert!(declared("", Some("exe")).is_none());
        assert!(declared("application/x-msdownload", Some("png")).is_none());
    }
}
