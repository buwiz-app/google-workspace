//! Which way a mail's attachment goes to the Bot: its own viewer, the
//! computer's own app, or neither. The Bot says what it can take at
//! `initialize`, in `capabilities.experimental["buwiz/documents"]`. An older
//! Bot says nothing, and takes what it always took: the Word and Spreadsheet
//! views.
//!
//! The Bot decides the kind from the name on disk, so the name has to end as
//! the bytes are. An image or a PDF is checked by its first bytes, since a
//! stranger's word for it is no proof. The other kinds go only to the app their
//! extension names, so that list holds only what opens without running
//! anything, and matches the Bot's: png, jpg, jpeg, gif, webp, heic, pdf, txt,
//! csv, docx, xlsx, pptx, pages, numbers, key and ics. No legacy Office file,
//! which can carry macros, and no zip, which Archive Utility would unpack here,
//! beyond the Bot's checks: those are saved. The Mac's own app gets the Bot's
//! own read-only copy and never the plugin's file, so nothing it does comes
//! back here or goes to Drive.

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
    /// Pages, Numbers, Keynote, TextEdit, Calendar or Archive Utility.
    system: bool,
    /// Whether the Bot's view can edit it, and the edit go to Drive: what the
    /// Word and Spreadsheet views take, as their own check does. Drive opens
    /// these alone.
    pub editable: bool,
    /// Its first bytes, for a kind an open checks by them.
    magic: Option<fn(&[u8]) -> bool>,
    /// Whether bytes look like this kind at all: a zip header for what's a zip
    /// inside, the old Office header for doc and xls, text for txt, csv and
    /// ics. An open needs it, and a saved copy's name is kept by it.
    fits: fn(&[u8]) -> bool,
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
        editable: false,
        magic: None,
        fits: zip,
    }
}

/// An editable kind of the Bot's view, which is never the Mac's app's.
const fn edited(
    mime: &'static str,
    exts: &'static [&'static str],
    view: &'static str,
    fits: fn(&[u8]) -> bool,
) -> Kind {
    Kind {
        editable: true,
        fits,
        ..kind(mime, exts, Some(view))
    }
}

/// A kind checked by its first bytes.
const fn sniffed_kind(
    mime: &'static str,
    exts: &'static [&'static str],
    view: &'static str,
    magic: fn(&[u8]) -> bool,
) -> Kind {
    Kind {
        magic: Some(magic),
        fits: magic,
        ..kind(mime, exts, Some(view))
    }
}

/// A zip, which every Office Open XML and iWork file is.
fn zip(b: &[u8]) -> bool {
    [b"PK\x03\x04", b"PK\x05\x06", b"PK\x07\x08"]
        .iter()
        .any(|m| b.starts_with(*m))
}

/// The old Office container, of .doc, .xls and .ppt.
fn ole(b: &[u8]) -> bool {
    b.starts_with(b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1")
}

/// Text, in whatever encoding: UTF-16 with its mark, or anything with no NUL in
/// its first block. A csv from Excel is often Latin-1 or UTF-16, and the app its
/// name picks reads either.
fn text(b: &[u8]) -> bool {
    let head = &b[..b.len().min(8192)];
    head.starts_with(b"\xff\xfe") || head.starts_with(b"\xfe\xff") || !head.contains(&0)
}

fn ics(b: &[u8]) -> bool {
    let b = b.strip_prefix(b"\xef\xbb\xbf").unwrap_or(b);
    let start = b
        .iter()
        .position(|c| !c.is_ascii_whitespace())
        .unwrap_or(b.len());
    text(b)
        && b[start..]
            .get(..15)
            .is_some_and(|head| head.eq_ignore_ascii_case(b"BEGIN:VCALENDAR"))
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

/// `%PDF-` and a version, such as `1.7`.
fn pdf(b: &[u8]) -> bool {
    b.len() >= 8
        && b.starts_with(b"%PDF-")
        && b[5].is_ascii_digit()
        && b[6] == b'.'
        && b[7].is_ascii_digit()
}

/// Every kind an attachment opens as, and the only kinds Drive opens: the
/// editable ones. Anything else is only saved.
pub(crate) const KINDS: [Kind; 19] = [
    edited(super::files::DOCX, &["docx"], WORD, zip),
    Kind {
        system: false,
        ..edited("application/msword", &["doc"], WORD, ole)
    },
    edited(super::files::XLSX, &["xlsx"], SHEET, zip),
    Kind {
        system: false,
        ..edited("application/vnd.ms-excel", &["xls"], SHEET, ole)
    },
    edited("text/csv", &["csv"], SHEET, text),
    sniffed_kind("image/png", &["png"], IMAGE, png),
    Kind {
        aliases: &["image/jpg", "image/pjpeg"],
        ..sniffed_kind("image/jpeg", &["jpg", "jpeg"], IMAGE, jpeg)
    },
    sniffed_kind("image/gif", &["gif"], IMAGE, gif),
    sniffed_kind("image/webp", &["webp"], IMAGE, webp),
    Kind {
        aliases: &["image/heif"],
        ..sniffed_kind("image/heic", &["heic", "heif"], IMAGE, heic)
    },
    Kind {
        aliases: &["application/x-pdf"],
        ..sniffed_kind("application/pdf", &["pdf"], PDF, pdf)
    },
    Kind {
        fits: text,
        ..kind("text/plain", &["txt"], None)
    },
    kind(super::files::PPTX, &["pptx"], None),
    // saved only: it can carry macros
    Kind {
        system: false,
        fits: ole,
        ..kind("application/vnd.ms-powerpoint", &["ppt"], None)
    },
    kind("application/vnd.apple.pages", &["pages"], None),
    kind("application/vnd.apple.numbers", &["numbers"], None),
    kind("application/vnd.apple.keynote", &["key"], None),
    Kind {
        aliases: &["application/ics"],
        fits: ics,
        ..kind("text/calendar", &["ics"], None)
    },
    // saved only: Archive Utility would unpack it beside itself
    Kind {
        aliases: &["application/x-zip-compressed", "application/x-zip"],
        system: false,
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
    let mime = mime.trim().to_ascii_lowercase();
    if super::files::NO_TYPE.contains(&mime.as_str()) {
        return ext.and_then(by_ext);
    }
    by_mime(&mime)
}

/// The kind a name's extension says.
pub(crate) fn by_ext(ext: &str) -> Option<&'static Kind> {
    let ext = ext.to_lowercase();
    KINDS.iter().find(|k| k.exts.contains(&ext.as_str()))
}

impl Kind {
    /// Whether `bytes` look like this kind.
    pub(crate) fn fits(&self, bytes: &[u8]) -> bool {
        (self.fits)(bytes)
    }
}

/// What the bytes show a kind to be. One checked by its bytes is what they
/// are, if they're the same sort of thing: a PNG sent as a JPEG is a PNG. One
/// they don't bear out is nothing that can open.
pub(crate) fn checked(kind: &'static Kind, bytes: &[u8]) -> Option<&'static Kind> {
    if kind.magic.is_none() {
        return kind.fits(bytes).then_some(kind);
    }
    KINDS
        .iter()
        .find(|k| k.view == kind.view && k.magic.is_some_and(|magic| magic(bytes)))
}

/// The kind the bytes are, whatever a mail says: an image or a PDF, else a
/// zip. Nothing else can be told by its bytes alone.
pub(crate) fn sniffed(bytes: &[u8]) -> Option<&'static Kind> {
    KINDS
        .iter()
        .find(|k| k.magic.is_some_and(|magic| magic(bytes)))
        .or_else(|| zip(bytes).then(|| by_mime("application/zip")).flatten())
}

/// How a kind gets to the person.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum Way {
    /// The Bot's own view, through `buwiz/document`.
    Viewer,
    /// The computer's own app, through `buwiz/open_with`.
    System,
    /// This Bot can't, and a newer one could.
    Update,
    /// Only saved: no Bot hands it to the Mac's app, and this one has no
    /// view for it.
    SaveOnly,
}

pub(crate) fn way(docs: &Documents, kind: &Kind) -> Way {
    if kind.view.is_some_and(|view| docs.can(view)) {
        Way::Viewer
    } else if kind.system && docs.can(OPEN_WITH) {
        Way::System
    } else if kind.system {
        Way::Update
    } else {
        Way::SaveOnly
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
            ("text/calendar", false),
            ("text/plain", false),
        ] {
            let kind = by_mime(mime).unwrap();
            let want = if viewer { Way::Viewer } else { Way::System };
            assert_eq!(way(&all, kind), want, "{mime}");
            assert_eq!(way(&system, kind), Way::System, "{mime}");
            assert_eq!(way(&none, kind), Way::Update, "{mime}");
        }
        // legacy Office can carry macros: its own view, or Save alone
        for mime in [
            "application/msword",
            "application/vnd.ms-excel",
            "application/vnd.ms-powerpoint",
            "application/zip",
        ] {
            let kind = by_mime(mime).unwrap();
            assert_eq!(way(&system, kind), Way::SaveOnly, "{mime}");
            assert_eq!(way(&none, kind), Way::SaveOnly, "{mime}");
        }
        assert_eq!(
            way(&all, by_mime("application/msword").unwrap()),
            Way::Viewer
        );
        assert_eq!(
            way(&all, by_mime("application/vnd.ms-powerpoint").unwrap()),
            Way::SaveOnly
        );
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
        assert!(checked(pdf_kind, b"%PDF-").is_none(), "a version follows");
        assert!(checked(by_mime("image/webp").unwrap(), b"RIFF\0\0\0\0WEBPVP8 ").is_some());
        assert!(checked(by_mime("image/heic").unwrap(), b"\0\0\0\x18ftypheic").is_some());
        assert!(checked(by_mime("image/heic").unwrap(), b"\0\0\0\x18ftypisom").is_none());
        assert!(checked(by_mime("image/gif").unwrap(), b"GIF89a").is_some());
        let txt = by_mime("text/plain").unwrap();
        assert!(
            checked(txt, b"anything").is_some(),
            "the app its name picks"
        );
    }

    #[test]
    fn text_in_any_encoding_is_text_and_a_nul_isnt() {
        let csv = by_mime("text/csv").unwrap();
        let txt = by_mime("text/plain").unwrap();
        for bytes in [
            &b"caf\xe9,cr\xe8me\r\n"[..],
            b"\xff\xfea\0,\0b\0",
            b"\xfe\xff\0a\0,\0b",
            b"\xef\xbb\xbfa,b\n",
            b"",
        ] {
            assert!(checked(csv, bytes).is_some(), "{bytes:?}");
            assert!(checked(txt, bytes).is_some(), "{bytes:?}");
        }
        for bytes in [&b"MZ\0\0"[..], b"a,b\n\0"] {
            assert!(checked(csv, bytes).is_none(), "{bytes:?}");
            assert!(checked(txt, bytes).is_none(), "{bytes:?}");
        }
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
