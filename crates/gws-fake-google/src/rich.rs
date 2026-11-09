//! Mail as a real inbox has it, for a live run of the reader: a long back and
//! forth with quoted history, a Google Calendar invite (whose `invite.ics`
//! comes twice, as Google sends it), a photo and a PDF, and a newsletter with
//! remote and embedded images.

use crate::{FAKE_EMAIL, Mail, NOW_MS, Seed};

const ICS: &str = "BEGIN:VCALENDAR\r\nPRODID:-//Google Inc//Google Calendar 70.9054//EN\r\nVERSION:2.0\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nDTSTART:20261110T020000Z\r\nDTEND:20261110T030000Z\r\nUID:fake-roof-inspection@google.com\r\nORGANIZER;CN=Ana Reyes:mailto:ana@example.com\r\nATTENDEE;PARTSTAT=NEEDS-ACTION;CN=You:mailto:owner@example.com\r\nSUMMARY:Roof inspection\r\nLOCATION:12 Mabini St\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
const PDF: &str = "%PDF-1.4\n1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n2 0 obj<</Type/Pages/Kids[3 0 R]/Count 1>>endobj\n3 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 300 200]/Contents 4 0 R/Resources<</Font<</F1 5 0 R>>>>>>endobj\n4 0 obj<</Length 44>>stream\nBT /F1 18 Tf 40 100 Td (Fake receipt) Tj ET\nendstream endobj\n5 0 obj<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>endobj\ntrailer<</Root 1 0 R>>\n%%EOF\n";
const PHOTO: &[u8] = include_bytes!("site-photo.png");
/// A 1×1 PNG, a second web picture that differs from the first.
const DOT: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xf8, 0xcf, 0xc0, 0xf0,
    0x1f, 0x00, 0x05, 0x00, 0x01, 0xff, 0x89, 0x99, 0x3d, 0x1d, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];
/// An empty zip: the end-of-directory record and nothing else.
const ZIP: &[u8] = b"PK\x05\x06\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0";

fn mail(
    n: u64,
    thread: u64,
    from: &str,
    subject: &str,
    hours: i64,
    text: &str,
    html: Option<String>,
) -> Mail {
    Mail {
        id: format!("{:016x}", 0x19f0_0000_0000_u64 + n),
        thread: format!("{:016x}", 0x19f0_0000_0000_u64 + thread),
        labels: vec!["INBOX".into(), "CATEGORY_PERSONAL".into(), "UNREAD".into()],
        from: from.into(),
        to: FAKE_EMAIL.into(),
        subject: subject.into(),
        at: NOW_MS + hours * 3_600_000,
        text: text.into(),
        html,
        attachments: Vec::new(),
        message_id: String::new(),
        inline: Vec::new(),
    }
}

/// Each message quotes the one before, as mail clients do.
fn back_and_forth() -> Vec<Mail> {
    let said = [
        (
            "Ana Reyes <ana@example.com>",
            "Can we move the site visit to Thursday? The roofer is free then.",
        ),
        (
            "You <owner@example.com>",
            "Thursday works. Morning or afternoon?",
        ),
        (
            "Ana Reyes <ana@example.com>",
            "Morning, 9 am. I'll bring the updated quote.",
        ),
        (
            "Ben Cruz <ben@example.com>",
            "I'll join too. Should I bring the ladder?",
        ),
        (
            "Ana Reyes <ana@example.com>",
            "Yes please. See you all at 9 on Thursday.",
        ),
    ];
    let mut quoted = String::new();
    let mut quoted_html = String::new();
    let mut before = "";
    said.iter()
        .enumerate()
        .map(|(i, (from, line))| {
            let subject = if i == 0 { "Site visit schedule" } else { "Re: Site visit schedule" };
            let text = format!("{line}\n\n{quoted}");
            let html = format!(
                "<div dir=\"ltr\"><p>{line}</p></div>{}",
                if quoted_html.is_empty() {
                    String::new()
                } else {
                    format!("<div class=\"gmail_quote\"><div class=\"gmail_attr\">On Nov 6, 2026, {before} wrote:</div><blockquote class=\"gmail_quote\" style=\"margin:0 0 0 .8ex;border-left:1px #ccc solid;padding-left:1ex\">{quoted_html}</blockquote></div>")
                }
            );
            quoted = format!("On Nov 6, 2026, {from} wrote:\n> {}", text.replace('\n', "\n> "));
            quoted_html = html.clone();
            before = from;
            mail(10 + i as u64, 10, from, subject, 2 + i as i64, &text, Some(html))
        })
        .collect()
}

impl Seed {
    /// Adds the reader's hard cases, newest of all. `stub` serves the
    /// newsletter's web pictures at `/pictures/`.
    pub fn with_rich_mail(mut self, stub: &str) -> Self {
        let mut rich = back_and_forth();

        let mut invite = mail(20, 20, "Ana Reyes <calendar-notification@google.com>", "Invitation: Roof inspection @ Tue Nov 10, 2026 10am - 11am (PHT)", 8,
            "Ana Reyes has invited you to Roof inspection.\n\nWhen: Tue Nov 10, 2026 10am - 11am\nWhere: 12 Mabini St\n\nReply for owner@example.com: Yes / No / Maybe",
            Some("<div><h3>Roof inspection</h3><p><b>When</b> Tue Nov 10, 2026 10am – 11am (PHT)</p><p><b>Where</b> 12 Mabini St</p><p>Reply: <a href=\"https://calendar.google.com/calendar/event?action=RESPOND&amp;rst=1\">Yes</a> · <a href=\"https://calendar.google.com/calendar/event?action=RESPOND&amp;rst=2\">No</a> · <a href=\"https://calendar.google.com/calendar/event?action=RESPOND&amp;rst=3\">Maybe</a></p></div>".into()));
        // Google sends the event twice: as the text/calendar alternative and
        // as an application/ics file, both named invite.ics
        invite.attachments = vec![
            (
                "invite.ics".into(),
                "text/calendar".into(),
                ICS.as_bytes().to_vec(),
            ),
            (
                "invite.ics".into(),
                "application/ics".into(),
                ICS.as_bytes().to_vec(),
            ),
        ];
        rich.push(invite);

        let mut photos = mail(
            30,
            30,
            "Cara Lim <cara@example.com>",
            "Photos from the site + receipt",
            9,
            "Here are the photos from Saturday and the receipt.",
            Some("<p>Here are the photos from Saturday and the receipt.</p>".into()),
        );
        photos.attachments = vec![
            ("IMG_4250.png".into(), "image/png".into(), PHOTO.to_vec()),
            (
                "receipt.pdf".into(),
                "application/pdf".into(),
                PDF.as_bytes().to_vec(),
            ),
            (
                "site-plans.zip".into(),
                "application/zip".into(),
                ZIP.to_vec(),
            ),
        ];
        rich.push(photos);

        let mut news = mail(
            40,
            40,
            "Weekly Digest <news@digest.example>",
            "Five roofs we loved this week",
            10,
            "Five roofs we loved this week. View in browser.",
            Some(format!(
                "<div><img src=\"cid:logo@digest\" width=\"120\" alt=\"Digest logo\"><h2>Five roofs we loved</h2><img src=\"{stub}/pictures/hero.png\" width=\"480\" alt=\"A red tile roof\"><p>Clay, slate and metal: this week's picks.</p><img src=\"{stub}/pictures/roof2.png\" width=\"480\" alt=\"A slate roof\"><p><a href=\"https://digest.example/read\">Read more</a></p></div>"
            )),
        );
        // the logo travels inside the mail, shown by its Content-ID
        news.inline = vec![(
            "logo@digest".into(),
            "logo.png".into(),
            "image/png".into(),
            PHOTO.to_vec(),
        )];
        rich.push(news);

        self.mails.extend(rich);
        self.newest_first();
        self.pictures = vec![
            ("hero.png".into(), PHOTO.to_vec()),
            ("roof2.png".into(), DOT.to_vec()),
        ];
        self
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use std::io::{Read, Write};

    use crate::{FakeGoogle, Mail, Seed, file_bytes, payload};

    fn rich(subject: &str) -> Mail {
        Seed::default()
            .with_rich_mail("http://127.0.0.1:1")
            .mails
            .into_iter()
            .find(|m| m.subject.starts_with(subject))
            .unwrap()
    }

    fn named(part: &Value, out: &mut Vec<(String, String)>) {
        if let Some(name) = part["filename"].as_str().filter(|n| !n.is_empty()) {
            out.push((
                name.to_owned(),
                part["mimeType"].as_str().unwrap().to_owned(),
            ));
        }
        for p in part["parts"].as_array().into_iter().flatten() {
            named(p, out);
        }
    }

    #[test]
    fn an_invite_carries_invite_ics_twice_one_inside_the_letter() {
        let p = payload(&rich("Invitation:"), true, 0);
        assert_eq!(p["mimeType"], "multipart/mixed");
        let alternative = &p["parts"][0];
        assert_eq!(alternative["mimeType"], "multipart/alternative");
        assert!(
            alternative["parts"]
                .as_array()
                .unwrap()
                .iter()
                .any(|q| q["mimeType"] == "text/calendar" && q["filename"] == "invite.ics")
        );
        let mut files = Vec::new();
        named(&p, &mut files);
        assert_eq!(
            files,
            [
                ("invite.ics".to_owned(), "text/calendar".to_owned()),
                ("invite.ics".to_owned(), "application/ics".to_owned())
            ]
        );
    }

    #[test]
    fn a_picture_inside_the_mail_travels_by_content_id() {
        let m = rich("Five roofs");
        let p = payload(&m, true, 0);
        assert_eq!(p["mimeType"], "multipart/related");
        let logo = &p["parts"][1];
        assert_eq!(logo["filename"], "logo.png");
        assert!(
            logo["headers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|h| h["name"] == "Content-ID" && h["value"] == "<logo@digest>")
        );
        assert_eq!(
            file_bytes(&m, m.attachments.len()),
            Some(&super::PHOTO.to_vec())
        );
        assert!(
            m.html
                .unwrap()
                .contains("http://127.0.0.1:1/pictures/hero.png")
        );
    }

    #[test]
    fn the_photo_mail_has_a_picture_a_pdf_and_a_zip() {
        let mut files = Vec::new();
        named(&payload(&rich("Photos from the site"), true, 0), &mut files);
        let names: Vec<&str> = files.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["IMG_4250.png", "receipt.pdf", "site-plans.zip"]);
    }

    #[test]
    fn the_long_thread_quotes_each_message_before() {
        let thread: Vec<Mail> = Seed::default()
            .with_rich_mail("http://127.0.0.1:1")
            .mails
            .into_iter()
            .filter(|m| m.subject.ends_with("Site visit schedule"))
            .collect();
        assert_eq!(thread.len(), 5);
        assert!(thread.iter().all(|m| m.thread == thread[0].thread));
        assert_eq!(
            thread
                .iter()
                .filter(|m| m.html.as_deref().is_some_and(|h| h.contains("gmail_quote")))
                .count(),
            4
        );
    }

    fn ids(part: &Value, out: &mut Vec<String>) {
        out.push(part["partId"].as_str().unwrap().to_owned());
        for p in part["parts"].as_array().into_iter().flatten() {
            ids(p, out);
        }
    }

    #[test]
    fn every_part_of_every_rich_mail_has_its_own_id() {
        for m in Seed::default().with_rich_mail("http://127.0.0.1:1").mails {
            let mut seen = Vec::new();
            ids(&payload(&m, true, 0), &mut seen);
            let mut unique = seen.clone();
            unique.sort();
            unique.dedup();
            assert_eq!(unique.len(), seen.len(), "{}: {seen:?}", m.subject);
        }
        let mut seen = Vec::new();
        ids(&payload(&rich("Five roofs"), true, 0), &mut seen);
        assert_eq!(seen, ["", "0", "0.0", "0.1", "1"]);
    }

    /// The status line and body of a plain GET to the stub.
    fn get(google: &FakeGoogle, path: &str) -> (String, Vec<u8>) {
        let mut s = std::net::TcpStream::connect(("127.0.0.1", google.port)).unwrap();
        write!(
            s,
            "GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut all = Vec::new();
        s.read_to_end(&mut all).unwrap();
        let split = all.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let head = String::from_utf8_lossy(&all[..split])
            .lines()
            .next()
            .unwrap()
            .to_owned();
        (head, all[split + 4..].to_vec())
    }

    #[test]
    fn the_newsletter_pictures_are_served_each_its_own_and_only_with_rich_mail() {
        let rich = FakeGoogle::start(Seed::default().with_rich_mail("http://127.0.0.1:1"));
        let (ok, hero) = get(&rich, "/pictures/hero.png");
        let (_, roof) = get(&rich, "/pictures/roof2.png");
        assert!(ok.contains(" 200"), "{ok}");
        assert!(hero.starts_with(b"\x89PNG") && roof.starts_with(b"\x89PNG"));
        assert_ne!(hero, roof);
        assert!(get(&rich, "/pictures/other.png").0.contains(" 404"));
        let plain = FakeGoogle::start(Seed::demo(2));
        assert!(get(&plain, "/pictures/hero.png").0.contains(" 404"));
    }

    #[test]
    fn rich_and_hostile_mail_together_stay_newest_first() {
        for seed in [
            Seed::demo(6).with_rich_mail("s").with_hostile("s"),
            Seed::demo(6).with_hostile("s").with_rich_mail("s"),
        ] {
            let at: Vec<i64> = seed.mails.iter().map(|m| m.at).collect();
            assert!(at.windows(2).all(|w| w[0] >= w[1]), "{at:?}");
            assert!(seed.mails.iter().any(|m| m.subject == "Hostile test mail"));
        }
    }
}
