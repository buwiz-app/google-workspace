//! What the Drive tools answer, as the screens read it, and the words each
//! value shows as.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use chrono::{DateTime, Datelike, Local, TimeZone};
use serde_json::Value;

const GB: f64 = 1024.0 * 1024.0 * 1024.0;
const MB: f64 = 1024.0 * 1024.0;

/// The only images a preview may show, as the tool promises.
const IMAGE_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

fn text(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn words(list: &[Value]) -> Vec<String> {
    list.iter()
        .filter_map(|w| w.as_str().map(str::to_owned))
        .collect()
}

#[derive(Clone, PartialEq, Default, Debug)]
pub(super) struct Row {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) kind: String,
    pub(super) owner: String,
    pub(super) mine: bool,
    pub(super) changed: String,
    pub(super) size: Option<u64>,
    pub(super) trashed: bool,
    /// What Google says the person may do with it. `None` is that it didn't
    /// say, and then a press is offered and Google answers it.
    pub(super) can: Option<Vec<String>>,
    pub(super) parents: Vec<String>,
}

impl Row {
    pub(super) fn of(v: &Value) -> Option<Row> {
        let flag = |k: &str| v.get(k).and_then(Value::as_bool).unwrap_or(false);
        Some(Row {
            id: v.get("id")?.as_str()?.to_owned(),
            name: text(v, "name"),
            kind: text(v, "kind"),
            owner: text(v, "owner"),
            mine: flag("mine"),
            changed: text(v, "changed"),
            size: v.get("size").and_then(Value::as_u64),
            trashed: flag("trashed"),
            can: v.get("can").and_then(Value::as_array).map(|c| words(c)),
            parents: v
                .get("parents")
                .and_then(Value::as_array)
                .map_or_else(Vec::new, |p| words(p)),
        })
    }

    /// Whether Google lets the person do `what` to it.
    pub(super) fn may(&self, what: &str) -> bool {
        self.can
            .as_ref()
            .is_none_or(|can| can.iter().any(|c| c == what))
    }

    pub(super) fn is_folder(&self) -> bool {
        self.kind == "folder"
    }

    pub(super) fn tag(&self) -> &'static str {
        match self.kind.as_str() {
            "folder" => "Folder",
            "doc" => "Doc",
            "sheet" => "Sheet",
            "slides" => "Slides",
            "pdf" => "PDF",
            "image" => "Image",
            "word" => "Word",
            "spreadsheet" => "Spreadsheet",
            _ => "File",
        }
    }

    /// Whose it is, as the sketch has it: the person's own say "me".
    pub(super) fn owner_line(&self) -> String {
        if self.mine {
            "me".to_owned()
        } else {
            self.owner.clone()
        }
    }

    pub(super) fn size_line(&self) -> String {
        self.size.map_or_else(|| "-".to_owned(), size_text)
    }

    pub(super) fn changed_line(&self) -> String {
        changed_text(&self.changed, Local::now())
    }

    /// What a screen reader says of the row: never its label.
    pub(super) fn spoken(&self) -> String {
        format!(
            "{}, {}, {}, {}",
            self.name,
            self.tag(),
            self.owner_line(),
            self.changed_line()
        )
    }
}

pub(super) fn rows_in(v: &Value, key: &str) -> Vec<Row> {
    v.get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Row::of)
        .collect()
}

/// A file open here and edited, as `gws_file_edits` says.
#[derive(Clone, PartialEq, Debug)]
pub(super) struct Edit {
    pub(super) id: String,
    pub(super) name: String,
    /// A Google Doc, Sheet or Slides, which only ever takes a copy.
    pub(super) google: bool,
    /// From a mail, so it has no file on Drive and goes up as a new one.
    pub(super) attachment: bool,
    /// Drive moved too since it was opened.
    pub(super) changed: bool,
    /// An upload is still going, past what its call waited for.
    pub(super) working: bool,
}

pub(super) fn edits_in(v: &Value) -> Vec<Edit> {
    v.get("files")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|e| {
            let flag = |k: &str| e.get(k).and_then(Value::as_bool).unwrap_or(false);
            Some(Edit {
                id: e.get("id")?.as_str()?.to_owned(),
                name: text(e, "name"),
                google: flag("google"),
                attachment: flag("attachment"),
                changed: flag("changed_on_drive"),
                working: flag("working"),
            })
        })
        .collect()
}

/// One step of the way to a folder.
#[derive(Clone, PartialEq, Debug)]
pub(super) struct Crumb {
    pub(super) id: String,
    pub(super) name: String,
}

pub(super) fn path_in(v: &Value) -> Vec<Crumb> {
    v.get("path")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|c| {
            Some(Crumb {
                id: c.get("id")?.as_str()?.to_owned(),
                name: text(c, "name"),
            })
        })
        .collect()
}

#[derive(Clone, PartialEq, Debug)]
pub(super) struct Person {
    pub(super) who: String,
    pub(super) role: String,
}

/// Someone with a file, as `gws_drive_access` says: the list a Share shows.
#[derive(Clone, PartialEq, Debug)]
pub(super) struct Access {
    pub(super) id: String,
    pub(super) who: String,
    pub(super) email: String,
    pub(super) role: String,
    /// This screen may take them off.
    pub(super) removable: bool,
}

fn who_of(name: String, email: String) -> String {
    match (name.is_empty(), email.is_empty()) {
        (true, _) => email,
        (false, true) => name,
        (false, false) => format!("{name} <{email}>"),
    }
}

pub(super) fn access_in(v: &Value) -> Vec<Access> {
    v.get("people")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|p| {
            let email = text(p, "email");
            let who = who_of(text(p, "name"), email.clone());
            let you = p.get("you") == Some(&Value::Bool(true));
            Some(Access {
                id: p.get("id")?.as_str()?.to_owned(),
                who: if you { format!("{who} (you)") } else { who },
                email,
                role: role_line(&text(p, "role")),
                removable: p.get("removable") == Some(&Value::Bool(true)),
            })
        })
        .collect()
}

/// What `gws_drive_file` adds to a row.
#[derive(Clone, PartialEq, Default, Debug)]
pub(super) struct Facts {
    pub(super) path: String,
    pub(super) description: String,
    pub(super) people: Vec<Person>,
}

pub(super) fn role_line(role: &str) -> String {
    match role {
        "owner" | "organizer" => "Owner".to_owned(),
        "fileOrganizer" | "writer" => "Can edit".to_owned(),
        "commenter" => "Can comment".to_owned(),
        "reader" => "Can view".to_owned(),
        other => other.to_owned(),
    }
}

impl Facts {
    pub(super) fn of(v: &Value) -> Facts {
        // the contract doesn't say if a file's path is the list's parts or a line
        let path = match v.get("path") {
            Some(Value::String(line)) => line.clone(),
            _ => path_in(v)
                .into_iter()
                .map(|c| c.name)
                .collect::<Vec<_>>()
                .join(" / "),
        };
        let people = v
            .get("people")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|p| Person {
                who: who_of(text(p, "name"), text(p, "email")),
                role: role_line(&text(p, "role")),
            })
            .collect();
        Facts {
            path,
            description: text(v, "description"),
            people,
        }
    }
}

/// What the details pane shows of a file's content.
#[derive(Clone, PartialEq, Debug)]
pub(super) enum Preview {
    Reading,
    Text(String),
    Image(String),
    Reason(String),
    /// An answer this screen won't show.
    Nothing,
}

impl Preview {
    pub(super) fn of(v: &Value) -> Preview {
        match v.get("kind").and_then(Value::as_str) {
            Some("text") => Preview::Text(text(v, "text")),
            Some("image") => {
                let url = text(v, "data_url");
                if allowed_image(&url) {
                    Preview::Image(url)
                } else {
                    Preview::Nothing
                }
            }
            Some("none") => Preview::Reason(text(v, "why")),
            _ => Preview::Nothing,
        }
    }
}

/// The allowed type `bytes` really are, from their signature.
fn image_type(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        [0x89, b'P', b'N', b'G', ..] => Some("image/png"),
        [0xFF, 0xD8, 0xFF, ..] => Some("image/jpeg"),
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

/// Whether `url` is an image held in the address itself, of an allowed type,
/// and really that type: a file's type is whatever its uploader said, and an
/// address that goes anywhere would tell a stranger the file was looked at.
pub(crate) fn allowed_image(url: &str) -> bool {
    let Some((mime, data)) = url
        .strip_prefix("data:")
        .and_then(|rest| rest.split_once(";base64,"))
    else {
        return false;
    };
    // 32 characters are whole bytes, and more than any signature needs
    let start = data.get(..data.len().min(32)).unwrap_or_default();
    IMAGE_TYPES.contains(&mime)
        && STANDARD
            .decode(start)
            .is_ok_and(|bytes| image_type(&bytes) == Some(mime))
}

pub(crate) fn size_text(bytes: u64) -> String {
    let n = bytes as f64;
    if n >= GB {
        format!("{:.1} GB", n / GB)
    } else if n >= MB {
        format!("{:.1} MB", n / MB)
    } else if bytes >= 1024 {
        format!("{} KB", bytes.div_ceil(1024))
    } else {
        format!("{bytes} B")
    }
}

/// A file's last change: today's as the clock, this year's as a date, older
/// with its year.
pub(super) fn changed_text<Z: TimeZone>(at: &str, now: DateTime<Z>) -> String
where
    Z::Offset: std::fmt::Display,
{
    let Ok(at) = DateTime::parse_from_rfc3339(at) else {
        return "-".to_owned();
    };
    let at = at.with_timezone(&now.timezone());
    if at.date_naive() == now.date_naive() {
        at.format("%H:%M").to_string()
    } else if at.year() == now.year() {
        at.format("%b %-d").to_string()
    } else {
        at.format("%b %-d, %Y").to_string()
    }
}

/// A whole number alone, or one place: "4", "4.2".
fn gigabytes(bytes: u64) -> String {
    let line = format!("{:.1}", bytes as f64 / GB);
    line.strip_suffix(".0").unwrap_or(&line).to_owned()
}

/// The column's last line: "4 of 15 GB", or what's used when there's no limit.
pub(super) fn storage_line(v: &Value) -> Option<String> {
    let used = v.get("used")?.as_u64()?;
    let limit = v.get("limit").and_then(Value::as_u64);
    Some(match limit {
        Some(limit) if (used as f64) < GB / 10.0 => {
            format!("{} of {} GB", size_text(used), gigabytes(limit))
        }
        Some(limit) => format!("{} of {} GB", gigabytes(used), gigabytes(limit)),
        None if (used as f64) < GB / 10.0 => format!("{} used", size_text(used)),
        None => format!("{} GB used", gigabytes(used)),
    })
}

#[cfg(test)]
mod tests {
    use chrono::{FixedOffset, Utc};
    use serde_json::json;

    use super::*;

    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGNgYGBgAAAABQABpfZFQAAAAABJRU5ErkJggg==";

    #[test]
    fn sizes_read_short_and_a_google_file_has_none() {
        assert_eq!(size_text(512), "512 B");
        assert_eq!(size_text(240 * 1024), "240 KB");
        assert_eq!(size_text(1_258_291), "1.2 MB");
        assert_eq!(size_text(3 * 1024 * 1024 * 1024), "3.0 GB");
        let row = Row::of(&json!({"id": "a", "size": null})).unwrap();
        assert_eq!(row.size_line(), "-");
    }

    #[test]
    fn storage_says_of_how_much_or_only_whats_used() {
        let gb = 1024_u64.pow(3);
        let line = |v| storage_line(&v).unwrap();
        assert_eq!(
            line(json!({"used": 4 * gb, "limit": 15 * gb})),
            "4 of 15 GB"
        );
        assert_eq!(
            line(json!({"used": 4 * gb + gb / 5, "limit": 15 * gb})),
            "4.2 of 15 GB"
        );
        assert_eq!(line(json!({"used": 4 * gb, "limit": null})), "4 GB used");
        assert_eq!(
            line(json!({"used": 2048, "limit": 15 * gb})),
            "2 KB of 15 GB",
            "a new account isn't 0 of 15"
        );
        assert_eq!(storage_line(&json!({})), None);
    }

    #[test]
    fn a_change_reads_as_the_clock_then_a_date_then_a_year() {
        let manila = FixedOffset::east_opt(8 * 3600).unwrap();
        let now = DateTime::parse_from_rfc3339("2026-11-06T10:00:00+08:00")
            .unwrap()
            .with_timezone(&manila);
        assert_eq!(changed_text("2026-11-06T01:02:00Z", now), "09:02");
        assert_eq!(changed_text("2026-10-12T04:00:00Z", now), "Oct 12");
        assert_eq!(changed_text("2024-09-30T04:00:00Z", now), "Sep 30, 2024");
        assert_eq!(
            changed_text("2026-11-05T17:00:00Z", now),
            "01:00",
            "the person's day, not Greenwich's"
        );
        assert_eq!(changed_text("soon", Utc::now()), "-");
    }

    #[test]
    fn every_kind_has_a_tag_of_its_own_and_an_unknown_one_is_a_file() {
        let tag = |kind: &str| Row::of(&json!({"id": "a", "kind": kind})).unwrap().tag();
        let tags = [
            "folder",
            "doc",
            "sheet",
            "slides",
            "pdf",
            "image",
            "word",
            "spreadsheet",
            "file",
        ]
        .map(tag);
        let unique: std::collections::HashSet<_> = tags.iter().collect();
        assert_eq!(unique.len(), tags.len());
        assert_eq!(tag("hologram"), "File");
    }

    #[test]
    fn only_a_real_image_of_an_allowed_type_is_an_image() {
        assert!(allowed_image(&format!("data:image/png;base64,{PNG}")));
        for bad in [
            "https://example.com/a.png".to_owned(),
            format!("data:image/svg+xml;base64,{PNG}"),
            format!("data:text/html;base64,{PNG}"),
            format!("data:image/bmp;base64,{PNG}"),
            format!("data:image/PNG;base64,{PNG}"),
            format!("data:image/png,{PNG}"),
            "data:image/png;base64,".to_owned(),
            "data:image/png;base64,###".to_owned(),
            // says PNG, is a page
            "data:image/png;base64,PHN2ZyB4bWxucz0iaHR0cDovL3d3dy53My5vcmcvMjAwMC9zdmciLz4="
                .to_owned(),
            format!("data:image/jpeg;base64,{PNG}"),
        ] {
            assert!(!allowed_image(&bad), "{bad}");
        }
    }

    #[test]
    fn a_preview_answer_becomes_what_it_says_and_nothing_else() {
        assert_eq!(
            Preview::of(&json!({"kind": "text", "text": "hi"})),
            Preview::Text("hi".to_owned())
        );
        assert_eq!(
            Preview::of(&json!({"kind": "none", "why": "Too big"})),
            Preview::Reason("Too big".to_owned())
        );
        assert_eq!(
            Preview::of(&json!({"kind": "image", "data_url": "https://example.com/a.png"})),
            Preview::Nothing
        );
        assert_eq!(Preview::of(&json!({"kind": "html"})), Preview::Nothing);
    }

    #[test]
    fn a_files_path_reads_from_parts_or_a_line() {
        let parts = json!({"path": [{"id": "a", "name": "Clients"}, {"id": "b", "name": "Acme"}]});
        assert_eq!(Facts::of(&parts).path, "Clients / Acme");
        assert_eq!(
            Facts::of(&json!({"path": "Clients / Acme"})).path,
            "Clients / Acme"
        );
    }

    #[test]
    fn a_row_offers_what_google_allows_and_silence_is_left_to_google() {
        let row = |can: Value| Row::of(&json!({"id": "a", "can": can})).unwrap();
        assert!(row(json!(["rename"])).may("rename"));
        assert!(!row(json!(["rename"])).may("trash"));
        assert!(!row(json!([])).may("rename"), "nothing allowed");
        assert!(row(Value::Null).may("trash"), "nothing said");
    }

    #[test]
    fn who_has_a_file_says_which_one_is_the_person() {
        let v = json!({"people": [
            {"id": "1", "name": "Owner Fake", "email": "owner@example.com", "role": "owner", "you": true},
            {"id": "2", "name": "Anyone with the link", "email": "", "role": "reader", "removable": false},
            {"id": "3", "name": "", "email": "ben@example.com", "role": "writer", "removable": true},
        ]});
        let people = access_in(&v);
        assert_eq!(people[0].who, "Owner Fake <owner@example.com> (you)");
        assert_eq!(people[0].role, "Owner");
        assert_eq!(people[1].who, "Anyone with the link");
        assert_eq!(
            (
                people[2].who.as_str(),
                people[2].role.as_str(),
                people[2].removable
            ),
            ("ben@example.com", "Can edit", true)
        );
    }

    #[test]
    fn people_read_as_a_person_says_them() {
        let v = json!({"people": [
            {"name": "Ana Reyes", "email": "ana@example.com", "role": "writer"},
            {"name": "", "email": "ben@example.com", "role": "reader"},
        ]});
        let people = Facts::of(&v).people;
        assert_eq!(people[0].who, "Ana Reyes <ana@example.com>");
        assert_eq!(people[0].role, "Can edit");
        assert_eq!(people[1].who, "ben@example.com");
        assert_eq!(people[1].role, "Can view");
    }
}
