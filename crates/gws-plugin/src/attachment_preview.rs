//! `gws_mail_attachment_preview`: a small picture on a mail, for its card in
//! Mail. It's the screen's alone, as the bytes are a stranger's.
//!
//! What a picture is comes from its bytes, never from the mail: a sender names
//! a file and its type as they like. Only png, jpeg, gif and webp show, as a
//! `data:` address of the kind the bytes are, which the screen checks again.

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
    let id = found.get("id").and_then(Value::as_str).unwrap_or_default();
    if !attachment_ok(id) {
        return Ok(gone());
    }
    let Some(bytes) = core.gmail.attachment(&auth, &message, id)? else {
        return Ok(gone());
    };
    if bytes.len() as u64 > PREVIEW_MAX {
        return Ok(too_big());
    }
    let Some(kind) = image_type(&bytes) else {
        return Ok(refusal("unsupported", "This file has no preview."));
    };
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

    // the largest preview, as its address, still fits in a result
    const _: () = assert!(
        "data:image/jpeg;base64,".len() as u64 + PREVIEW_MAX.div_ceil(3) * 4 + 200 < 1024 * 1024
    );
}
