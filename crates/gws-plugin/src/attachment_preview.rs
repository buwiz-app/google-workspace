//! `gws_mail_attachment_preview`: a small picture on a mail, for its card in
//! Mail. It's the screen's alone, as the bytes are a stranger's.
//!
//! What a picture is comes from its bytes, never from the mail: a sender names
//! a file and its type as they like. Only png, jpeg, gif and webp show, as a
//! `data:` address of the kind the bytes are, which the screen checks again.
//! HEIC has no preview: the card shows its kind mark, and Open still opens it.
//! A picture also has to say it's no more than [`PIXELS_MAX`] pixels.

use base64::Engine as _;
use serde_json::{Value, json};

use crate::files::{attachment_ok, part_ok};
use crate::gmail::{self, Failure};
use crate::mail::signed_in;
use crate::tools::{MAIL_ATTACHMENT_PREVIEW, failed, ok};
use crate::{Core, js};

/// The largest picture a card shows: a megabyte, less what base64 adds, as a
/// tool's whole result can't pass 1 MiB (protocol 13.2) and this one carries
/// the picture once, with room for the rest.
pub const PREVIEW_MAX: u64 = (1024 * 1024 - 1024) / 4 * 3;

/// The most pixels a preview may hold: plenty for a thumbnail. A small file
/// can say it's 30000 by 30000, and the Bot would make room for every pixel.
pub const PIXELS_MAX: u64 = 16_000_000;

fn refusal(code: &str, why: &str) -> Value {
    ok(json!({"refused": code, "message": why}))
}

/// The kind `bytes` really are, from their signature.
fn image_type(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n', ..] => Some("image/png"),
        [0xff, 0xd8, 0xff, ..] => Some("image/jpeg"),
        [b'G', b'I', b'F', b'8', ..] => Some("image/gif"),
        [
            b'R',
            b'I',
            b'F',
            b'F',
            _,
            _,
            _,
            _,
            b'W',
            b'E',
            b'B',
            b'P',
            ..,
        ] => Some("image/webp"),
        _ => None,
    }
}

fn be16(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from(u16::from_be_bytes(
        b.get(at..at + 2)?.try_into().ok()?,
    )))
}

fn le16(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from(u16::from_le_bytes(
        b.get(at..at + 2)?.try_into().ok()?,
    )))
}

fn le24(b: &[u8], at: usize) -> Option<u32> {
    let p = b.get(at..at + 3)?;
    Some(u32::from(p[0]) | u32::from(p[1]) << 8 | u32::from(p[2]) << 16)
}

/// Width and height, as the picture's own header says them; `None` when it
/// can't be read, which shows no preview.
fn size_of(kind: &str, b: &[u8]) -> Option<(u32, u32)> {
    match kind {
        "image/png" => {
            // the first chunk must be IHDR
            if b.get(12..16)? != b"IHDR" {
                return None;
            }
            let w = u32::from_be_bytes(b.get(16..20)?.try_into().ok()?);
            let h = u32::from_be_bytes(b.get(20..24)?.try_into().ok()?);
            Some((w, h))
        }
        "image/jpeg" => jpeg_size(b),
        "image/gif" => gif_size(b),
        "image/webp" => webp_size(b),
        _ => None,
    }
}

/// From the first start-of-frame marker, walking the segments to it.
fn jpeg_size(b: &[u8]) -> Option<(u32, u32)> {
    let mut at = 2;
    loop {
        if *b.get(at)? != 0xff {
            return None;
        }
        let marker = *b.get(at + 1)?;
        match marker {
            // fill bytes, and markers with no length
            0xff => at += 1,
            0x01 | 0xd0..=0xd7 => at += 2,
            // the scan starts with no frame said: nothing to go by
            0xd9 | 0xda => return None,
            0xc0..=0xcf if !matches!(marker, 0xc4 | 0xc8 | 0xcc) => {
                return Some((be16(b, at + 7)?, be16(b, at + 5)?));
            }
            _ => at += 2 + usize::try_from(be16(b, at + 2)?).ok()?,
        }
    }
}

/// The screen's size, and each frame's reach past it: a frame can say it's
/// bigger than the screen it's drawn on.
fn gif_size(b: &[u8]) -> Option<(u32, u32)> {
    let (mut w, mut h) = (le16(b, 6)?, le16(b, 8)?);
    let flags = *b.get(10)?;
    let mut at = 13;
    if flags & 0x80 != 0 {
        at += 3 << ((flags & 0x07) + 1);
    }
    let skip_blocks = |mut at: usize| -> Option<usize> {
        loop {
            let n = usize::from(*b.get(at)?);
            at += 1 + n;
            if n == 0 {
                return Some(at);
            }
        }
    };
    loop {
        match *b.get(at)? {
            0x3b => return Some((w, h)),
            0x21 => at = skip_blocks(at + 2)?,
            0x2c => {
                let (left, top) = (le16(b, at + 1)?, le16(b, at + 3)?);
                let (fw, fh) = (le16(b, at + 5)?, le16(b, at + 7)?);
                w = w.max(left + fw);
                h = h.max(top + fh);
                let local = *b.get(at + 9)?;
                at += 10;
                if local & 0x80 != 0 {
                    at += 3 << ((local & 0x07) + 1);
                }
                // the code size, then the picture's blocks
                at = skip_blocks(at + 1)?;
            }
            _ => return None,
        }
    }
}

/// From its first chunk: lossy VP8, lossless VP8L, or the VP8X canvas, which
/// every frame of an animation must fit.
fn webp_size(b: &[u8]) -> Option<(u32, u32)> {
    match b.get(12..16)? {
        b"VP8 " => {
            if b.get(23..26)? != [0x9d, 0x01, 0x2a] {
                return None;
            }
            Some((le16(b, 26)? & 0x3fff, le16(b, 28)? & 0x3fff))
        }
        b"VP8L" => {
            if *b.get(20)? != 0x2f {
                return None;
            }
            let bits = u32::from_le_bytes(b.get(21..25)?.try_into().ok()?);
            Some(((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1))
        }
        b"VP8X" => Some((le24(b, 24)? + 1, le24(b, 27)? + 1)),
        _ => None,
    }
}

fn preview(core: &Core, args: &Value) -> Result<Value, Failure> {
    let (auth, _) = signed_in(core)?;
    let message = js::trim(&js::string_or_empty(args.get("message"))).to_owned();
    let part = js::trim(&js::string_or_empty(args.get("part"))).to_owned();
    if !gmail::id_ok(&message) || !part_ok(&part) {
        return Ok(refusal("invalid_args", "That isn't an attachment."));
    }
    let gone = || refusal("not_found", "That attachment is gone from the mail.");
    let too_big = || refusal("too_big", "This picture is too big to preview.");
    let Some(mail) = core.gmail.full(&auth, &message)? else {
        return Ok(gone());
    };
    let listed = gmail::attachments(&mail);
    let Some(found) = listed
        .iter()
        .find(|a| a.get("part").and_then(Value::as_str) == Some(&part))
    else {
        return Ok(gone());
    };
    // Google's count of the part, so nothing big is fetched to find it out
    match found.get("size").and_then(Value::as_u64) {
        Some(size) if size <= PREVIEW_MAX => {}
        _ => return Ok(too_big()),
    }
    // a small file can come in its own part, with no id to fetch it by
    let id = found.get("id").and_then(Value::as_str).unwrap_or_default();
    let bytes = if id.is_empty() {
        gmail::part_bytes(&mail, &part)
    } else if attachment_ok(id) {
        core.gmail.attachment(&auth, &message, id)?
    } else {
        None
    };
    let Some(bytes) = bytes else {
        return Ok(gone());
    };
    if bytes.len() as u64 > PREVIEW_MAX {
        return Ok(too_big());
    }
    let Some(kind) = image_type(&bytes) else {
        return Ok(refusal("unsupported", "This file has no preview."));
    };
    // what a decoder would take: the picture's size as it says it, read whole
    match size_of(kind, &bytes) {
        Some((w, h)) if w > 0 && h > 0 && u64::from(w) * u64::from(h) <= PIXELS_MAX => {}
        Some(_) => return Ok(too_big()),
        None => return Ok(refusal("unsupported", "This file has no preview.")),
    }
    let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
    // not `ok`: it would carry the picture twice, and a result has a limit
    Ok(json!({
        "content": [{"type": "text", "text": "A picture."}],
        "structuredContent": {"preview": format!("data:{kind};base64,{data}")},
    }))
}

/// Runs the preview tool; `None` for any other tool.
pub fn run(core: &Core, tool: &str, args: &Value) -> Option<Value> {
    if tool != MAIL_ATTACHMENT_PREVIEW {
        return None;
    }
    let result = preview(core, args);
    core.sync.seen();
    Some(result.unwrap_or_else(failed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_picture_is_known_by_its_bytes_alone() {
        assert_eq!(image_type(b"\x89PNG\r\n\x1a\n...."), Some("image/png"));
        assert_eq!(image_type(&[0xff, 0xd8, 0xff, 0xe0]), Some("image/jpeg"));
        assert_eq!(image_type(b"GIF89a"), Some("image/gif"));
        assert_eq!(image_type(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        for not in [
            &b"%PDF-1.4"[..],
            b"<svg>",
            b"\x89PNG",
            b"RIFF\0\0\0\0WAVE",
            b"",
        ] {
            assert_eq!(image_type(not), None, "{not:?}");
        }
    }

    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut b = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
        b.extend(w.to_be_bytes());
        b.extend(h.to_be_bytes());
        b
    }

    fn jpeg(w: u16, h: u16) -> Vec<u8> {
        // an APP0 segment first, then the frame
        let mut b = vec![0xff, 0xd8, 0xff, 0xe0, 0, 4, 0, 0, 0xff, 0xc0, 0, 11, 8];
        b.extend(h.to_be_bytes());
        b.extend(w.to_be_bytes());
        b.extend([1, 1, 0x11, 0]);
        b
    }

    fn gif(w: u16, h: u16, frame: (u16, u16)) -> Vec<u8> {
        let mut b = b"GIF89a".to_vec();
        b.extend(w.to_le_bytes());
        b.extend(h.to_le_bytes());
        // a global colour table of two
        b.extend([0x80, 0, 0, 0, 0, 0, 0xff, 0xff, 0xff]);
        // a comment, then a frame at 0,0
        b.extend([0x21, 0xfe, 2, b'h', b'i', 0, 0x2c, 0, 0, 0, 0]);
        b.extend(frame.0.to_le_bytes());
        b.extend(frame.1.to_le_bytes());
        b.extend([0, 2, 1, 0x44, 0, 0x3b]);
        b
    }

    fn webp(chunk: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut b = b"RIFF\0\0\0\0WEBP".to_vec();
        b.extend(chunk);
        b.extend([0, 0, 0, 0]);
        b.extend(body);
        b
    }

    fn size(b: &[u8]) -> Option<(u32, u32)> {
        size_of(image_type(b)?, b)
    }

    #[test]
    fn a_pictures_size_is_read_from_its_own_header() {
        assert_eq!(size(&png(640, 480)), Some((640, 480)));
        assert_eq!(size(&jpeg(800, 600)), Some((800, 600)));
        assert_eq!(size(&gif(10, 20, (10, 20))), Some((10, 20)));
        let lossy = [0, 0, 0, 0x9d, 0x01, 0x2a, 0x20, 0x03, 0x58, 0x02];
        assert_eq!(size(&webp(b"VP8 ", &lossy)), Some((800, 600)));
        // 14 bits each, less one: 800 by 600
        let bits: u32 = 799 | 599 << 14;
        let mut lossless = vec![0x2f];
        lossless.extend(bits.to_le_bytes());
        assert_eq!(size(&webp(b"VP8L", &lossless)), Some((800, 600)));
        let canvas = [0, 0, 0, 0, 0x1f, 0x03, 0, 0x57, 0x02, 0];
        assert_eq!(size(&webp(b"VP8X", &canvas)), Some((800, 600)));
    }

    #[test]
    fn a_header_that_lies_or_stops_short_is_no_picture_to_show() {
        // a frame bigger than the screen it says it's on
        assert_eq!(size(&gif(10, 10, (30000, 30000))), Some((30000, 30000)));
        // no IHDR where it must be
        let mut odd = png(1, 1);
        odd[12..16].copy_from_slice(b"IDAT");
        assert_eq!(size(&odd), None);
        // cut off in the middle of a header
        assert_eq!(size(&png(1, 1)[..20]), None);
        assert_eq!(size(&jpeg(1, 1)[..12]), None);
        assert_eq!(size(&webp(b"VP8 ", &[0, 0, 0, 0, 0, 0])), None);
        // a scan before any frame
        assert_eq!(size(&[0xff, 0xd8, 0xff, 0xda, 0, 2]), None);
    }

    #[test]
    fn past_sixteen_million_pixels_is_too_big_whatever_the_file_weighs() {
        let fits = |b: &[u8]| {
            size(b)
                .is_some_and(|(w, h)| w > 0 && h > 0 && u64::from(w) * u64::from(h) <= PIXELS_MAX)
        };
        assert!(fits(&png(4000, 4000)));
        assert!(!fits(&png(4000, 4001)));
        assert!(!fits(&png(8000, 5000)));
        assert!(!fits(&png(30000, 30000)));
        assert!(!fits(&png(0, 10)));
        assert!(!fits(&jpeg(65535, 65535)));
        assert!(!fits(&gif(10, 10, (30000, 30000))));
        let canvas = [0, 0, 0, 0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff];
        assert!(!fits(&webp(b"VP8X", &canvas)));
    }

    // the largest preview, as its address, still fits in a result
    const _: () = assert!(
        "data:image/jpeg;base64,".len() as u64 + PREVIEW_MAX.div_ceil(3) * 4 + 200 < 1024 * 1024
    );
}
