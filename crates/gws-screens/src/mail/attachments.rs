//! A conversation's files as cards, under the letter: each a picture or a kind
//! mark, the name, the kind and size, and two presses, Open and Save.
//!
//! Each card's words are a stranger's, so they're texts and accessibility
//! values and never a label (see the label rule in `mail.rs`). A picture shows
//! only as the plugin's `data:` address, checked here as Drive checks its own,
//! and only one is asked for at a time, for a card the person can see.

use std::collections::HashMap;

use buwiz_app::Bridge;
use nsh_core::AccessibilityValue;
use nsh_core::geometry::{Bounds, Points};
use nsh_core::prelude::*;
use serde_json::{Value, json};

use super::{Attachment, Mail};
use crate::drive::file::{allowed_image, size_text};
use crate::shell::{INK, LINE, MUTE, RAIL};

/// The largest file a card asks a picture of: the plugin's limit, a megabyte
/// less what base64 adds, so a bigger one is never fetched for nothing.
const PREVIEW_MAX: u64 = (1024 * 1024 - 1024) / 4 * 3;

/// How a card shows what it is.
#[derive(Clone, PartialEq, Debug)]
enum Look {
    Asking,
    Picture(String),
    Mark,
}

/// The cards' own state, for as long as the letter is shown.
#[derive(Clone, Copy)]
struct Cards {
    /// By message and part, as the files come in the same order each read
    /// only by chance.
    looks: Signal<HashMap<String, Look>>,
    /// Each card's top and height inside the list, by its place in it.
    placed: Signal<HashMap<usize, (f32, f32)>>,
    /// How far the list is scrolled, and how much of it shows.
    window: Signal<(f32, f32)>,
    /// A picture is on its way: the next waits for it.
    asking: Signal<bool>,
}

fn key(a: &Attachment) -> String {
    format!("{}/{}", a.message, a.part)
}

fn extension(name: &str) -> String {
    name.rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default()
}

/// The mark and the word for a file's kind, from its name or its type. Both
/// are the sender's, and only pick what's drawn.
fn kind(a: &Attachment) -> (&'static str, &'static str) {
    let ext = extension(&a.name);
    let mime = a.mime.to_ascii_lowercase();
    let is = |exts: &[&str], mimes: &[&str]| {
        exts.contains(&ext.as_str()) || mimes.iter().any(|m| mime.contains(m))
    };
    if is(
        &[
            "png", "jpg", "jpeg", "gif", "webp", "heic", "bmp", "tif", "tiff",
        ],
        &["image/"],
    ) {
        ("IMG", "Image")
    } else if is(&["pdf"], &["application/pdf"]) {
        ("PDF", "PDF")
    } else if is(
        &["doc", "docx", "odt", "rtf", "pages"],
        &["wordprocessingml", "msword"],
    ) {
        ("DOC", "Word")
    } else if is(
        &["xls", "xlsx", "csv", "ods", "numbers"],
        &["spreadsheetml", "ms-excel", "text/csv"],
    ) {
        ("XLS", "Sheet")
    } else if is(
        &["ppt", "pptx", "odp", "key"],
        &["presentationml", "ms-powerpoint"],
    ) {
        ("PPT", "Slides")
    } else if is(&["ics"], &["text/calendar", "application/ics"]) {
        ("ICS", "Calendar")
    } else if is(
        &["zip", "rar", "7z", "tar", "gz", "tgz", "bz2", "xz"],
        &["zip", "x-rar", "x-7z", "gzip", "x-tar"],
    ) {
        ("ZIP", "Archive")
    } else {
        ("FILE", "File")
    }
}

/// "Image · 2.1 MB", or the kind alone when Google gave no size.
fn kind_line(a: &Attachment) -> String {
    let (_, word) = kind(a);
    match a.size {
        Some(size) => format!("{word} · {}", size_text(size)),
        None => word.to_owned(),
    }
}

/// The message and the part are what a press fetches by. A small file can have
/// no attachment id of its own: the plugin reads it from the message.
fn can_press(a: &Attachment) -> bool {
    !a.message.is_empty() && !a.part.is_empty()
}

/// Worth asking a picture of: a small file that says it's one. The plugin
/// judges its bytes, and this screen the address it sends back.
fn previewable(a: &Attachment) -> bool {
    let named = ["png", "jpg", "jpeg", "gif", "webp"].contains(&extension(&a.name).as_str());
    let typed = ["image/png", "image/jpeg", "image/gif", "image/webp"]
        .contains(&a.mime.to_ascii_lowercase().as_str());
    can_press(a) && a.size.is_some_and(|s| s <= PREVIEW_MAX) && (named || typed)
}

/// What a press's answer says, on the line over the cards.
fn said(answer: Result<Value, buwiz_app::BuwizError>, done: Option<&str>) -> Option<String> {
    match answer {
        Ok(v) if v.get("refused").is_some() => Some(
            v.get("message")
                .and_then(Value::as_str)
                .unwrap_or("That didn't work.")
                .to_owned(),
        ),
        Ok(v) if v.get("saved") == Some(&Value::Bool(true)) => done.map(str::to_owned),
        Ok(_) => None,
        Err(e) if e.code == "timeout" => {
            Some("This attachment is taking a while. Press it again in a moment.".to_owned())
        }
        Err(e) => Some(e.message),
    }
}

/// A press on a card. Open lands in the Bot's viewer and Save in Downloads,
/// both from the answer's `_meta`, which never reaches here. The call starts at
/// the press, as who pressed is judged by that moment.
fn press(b: &Bridge, m: Mail, a: &Attachment, tool: &str, now: &str, done: Option<&'static str>) {
    let call = b.call(tool, json!({"message": a.message, "part": a.part}));
    m.attachment_said.set(Some(now.to_owned()));
    let job = Box::pin(async move {
        m.attachment_said.set(said(call.await, done));
    });
    m.beside.with_untracked(|beside| drop(beside.send(job)));
}

/// Asks the picture of the first card on view that has none yet, if none is
/// on its way. Its answer asks the next.
fn next_picture(b: &Bridge, m: Mail, c: Cards) {
    if c.asking.try_get() != Some(false) {
        return;
    }
    let (Some(placed), Some(looks)) = (c.placed.try_get(), c.looks.try_get()) else {
        return;
    };
    let (top, room) = c.window.get_untracked();
    let files = m.attachments.get_untracked();
    let on_view = |i: &usize| {
        placed
            .get(i)
            .is_some_and(|&(y, h)| y < top + room && y + h > top)
    };
    let Some(a) = files
        .iter()
        .enumerate()
        .find(|(i, a)| previewable(a) && !looks.contains_key(&key(a)) && on_view(i))
        .map(|(_, a)| a.clone())
    else {
        return;
    };
    c.asking.set(true);
    c.looks.update(|l| {
        l.insert(key(&a), Look::Asking);
    });
    let call = b.call(
        "gws_mail_attachment_preview",
        json!({"message": a.message, "part": a.part}),
    );
    let again = b.clone();
    b.spawn(async move {
        let look = match call.await {
            Ok(v) => v
                .get("preview")
                .and_then(Value::as_str)
                .filter(|url| allowed_image(url))
                .map_or(Look::Mark, |url| Look::Picture(url.to_owned())),
            Err(_) => Look::Mark,
        };
        // the letter went meanwhile, and these cards with it
        let Some(mut looks) = c.looks.try_get() else {
            return;
        };
        looks.insert(key(&a), look);
        c.looks.try_set(looks);
        c.asking.try_set(false);
        next_picture(&again, m, c);
    });
}

fn mark(badge: &'static str) -> View {
    view()
        .app_class(Some(
            format!("w-[40px] h-[40px] rounded-md items-center justify-center {RAIL}").into(),
        ))
        .child(text(badge).app_class(Some(format!("text-[10px] font-semibold {MUTE}").into())))
}

fn card(b: Bridge, m: Mail, c: Cards, at: usize, a: Attachment) -> View {
    let (badge, _) = kind(&a);
    let shown = key(&a);
    let face = dynamic(move || match c.looks.with(|l| l.get(&shown).cloned()) {
        Some(Look::Picture(url)) => view().child(
            image(url)
                .label("Preview of this attachment")
                .app_class(Some("w-[40px] h-[40px] rounded-md object-cover".into())),
        ),
        _ => mark(badge),
    });
    let name = || {
        Some(AccessibilityValue {
            text: Some(a.name.clone().into()),
            ..AccessibilityValue::default()
        })
    };
    let words = view()
        .app_class(Some("flex-1 min-w-0 gap-[2px]".into()))
        .child(
            text(a.name.clone())
                .number_of_lines(1)
                .app_class(Some(format!("text-[12px] {INK}").into())),
        )
        .child(
            text(kind_line(&a))
                .number_of_lines(1)
                .app_class(Some(format!("text-[11px] {MUTE}").into())),
        );
    let top = view()
        .app_class(Some("flex-row items-center gap-2".into()))
        .child(face)
        .child(words);
    let laid = b.clone();
    let card = view()
        .app_class(Some(
            format!("w-[200px] gap-1 p-2 rounded-lg border {LINE}").into(),
        ))
        .on_layout(move |bounds: &Bounds<Points>| {
            let spot = (bounds.origin.y.0, bounds.size.height.0);
            if c.placed.with_untracked(|p| p.get(&at) != Some(&spot)) {
                c.placed.update(|p| {
                    p.insert(at, spot);
                });
                next_picture(&laid, m, c);
            }
        })
        .child(top);
    // one that doesn't say where it is has no way to fetch it
    if !can_press(&a) {
        return card;
    }
    let (opens, saves) = (b.clone(), b);
    let (opened, saved) = (a.clone(), a.clone());
    card.child(
        view()
            .app_class(Some("flex-row gap-1".into()))
            .child(
                view()
                    .app_class(Some("px-3 py-1".into()))
                    .label("Open this attachment")
                    .accessibility_value(name())
                    .on_press(move || {
                        press(
                            &opens,
                            m,
                            &opened,
                            "gws_mail_attachment_open",
                            "Opening…",
                            None,
                        );
                    })
                    .child(text("Open").app_class(Some(format!("text-[12px] {INK}").into()))),
            )
            .child(
                view()
                    .app_class(Some("px-3 py-1".into()))
                    .label("Save this attachment")
                    .accessibility_value(name())
                    .on_press(move || {
                        press(
                            &saves,
                            m,
                            &saved,
                            "gws_mail_attachment_save",
                            "Saving…",
                            // the Bot copies it after this answer, and says how that went
                            Some("Saving to Downloads…"),
                        );
                    })
                    .child(text("Save").app_class(Some(format!("text-[12px] {INK}").into()))),
            ),
    )
}

/// The conversation's files, under the letter. Past a few they scroll, so the
/// letter keeps its room.
pub(super) fn cards(b: Bridge, m: Mail) -> impl IntoElement {
    let c = Cards {
        looks: signal(HashMap::new()),
        placed: signal(HashMap::new()),
        window: signal((0.0, 0.0)),
        asking: signal(false),
    };
    dynamic(move || {
        if m.attachments.with(Vec::is_empty) {
            return view();
        }
        let (listed, scrolled, sized) = (b.clone(), b.clone(), b.clone());
        // each its own, so a line said or a picture come rebuilds no card
        let list = dynamic(move || {
            let files = m.attachments.get();
            c.placed.set(HashMap::new());
            files.into_iter().enumerate().fold(
                view().app_class(Some("flex-row flex-wrap gap-2 p-2".into())),
                |list, (at, a)| list.child(card(listed.clone(), m, c, at, a)),
            )
        });
        let said = dynamic(move || {
            m.attachment_said.get().map_or_else(view, |line| {
                view()
                    .app_class(Some("px-3 pt-1".into()))
                    .child(text(line).app_class(Some(format!("text-[11px] {MUTE}").into())))
            })
        });
        view()
            .app_class(Some(format!("border-t {LINE}").into()))
            .child(said)
            .child(
                scroll()
                    .app_class(Some("max-h-[150px]".into()))
                    .on_layout(move |bounds: &Bounds<Points>| {
                        let room = bounds.size.height.0;
                        if c.window.with_untracked(|w| w.1 != room) {
                            c.window.update(|w| w.1 = room);
                            next_picture(&sized, m, c);
                        }
                    })
                    .on_scroll(move |e: &nsh_core::element::ScrollEvent| {
                        let top = e.offset.y.0;
                        if c.window.with_untracked(|w| w.0 != top) {
                            c.window.update(|w| w.0 = top);
                            next_picture(&scrolled, m, c);
                        }
                    })
                    .child(list),
            )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str, mime: &str, size: Option<u64>) -> Attachment {
        Attachment {
            message: "m1".into(),
            part: "1".into(),
            id: "att-1".into(),
            name: name.into(),
            mime: mime.into(),
            size,
        }
    }

    #[test]
    fn a_card_says_its_kind_and_a_readable_size() {
        let line = |name, mime, size| kind_line(&file(name, mime, size));
        assert_eq!(
            line("IMG_4250.png", "image/png", Some(2_202_009)),
            "Image · 2.1 MB"
        );
        assert_eq!(
            line("receipt.pdf", "application/pdf", Some(90_000)),
            "PDF · 88 KB"
        );
        assert_eq!(
            line("site-plans.zip", "application/zip", Some(22)),
            "Archive · 22 B"
        );
        assert_eq!(line("invite.ics", "application/ics", None), "Calendar");
        assert_eq!(line("Q3.xlsx", "application/octet-stream", None), "Sheet");
        assert_eq!(line("deck.pptx", "", None), "Slides");
        assert_eq!(line("Roof quote.docx", "", None), "Word");
        assert_eq!(line("run.sh", "text/x-sh", None), "File");
    }

    #[test]
    fn only_a_small_file_that_says_it_is_a_picture_is_asked_for_one() {
        assert!(previewable(&file("a.png", "image/png", Some(1000))));
        assert!(previewable(&file(
            "a.JPG",
            "application/octet-stream",
            Some(1000)
        )));
        assert!(!previewable(&file(
            "a.png",
            "image/png",
            Some(PREVIEW_MAX + 1)
        )));
        assert!(!previewable(&file(
            "a.png",
            "image/png",
            Some(2 * 1024 * 1024)
        )));
        assert!(!previewable(&file("a.png", "image/png", None)));
        assert!(!previewable(&file("a.svg", "image/svg+xml", Some(1000))));
        assert!(!previewable(&file("a.pdf", "application/pdf", Some(1000))));
        // a small one in its message's own part still has a preview
        let mut small = file("a.png", "image/png", Some(1000));
        small.id.clear();
        assert!(previewable(&small));
        small.part.clear();
        assert!(!previewable(&small));
    }
}
