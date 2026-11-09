//! Gmail's REST API (`gmail/v1/users/me`), with the token Buwiz Bot pushed (`crate::accounts`), and what the tools make of its answers:
//! headers, a message's plain-text body (text/plain, else text/html with
//! the tags stripped), one-line snippets, the 64 KiB cap.
//!
//! The HTTP client is ureq (rustls with Mozilla's roots, so no OpenSSL in it),
//! blocking, on the tool's own thread. It follows no redirects, so the token
//! only ever goes to the API base.

use std::time::Duration;

use base64::Engine as _;
use serde_json::Value;

use crate::accounts::Bearer;
use crate::{clean, js};

/// A thread's messages by id, each with its labels.
pub type Messages = Vec<(String, Vec<String>)>;

/// The most of one header anything shows or hands on.
pub const HEADER_MAX: usize = 1024;
/// The production API base.
pub const GMAIL_API: &str = "https://gmail.googleapis.com/gmail/v1/users/me";
pub const BODY_MAX: usize = 64 * 1024;
pub const SNIPPET_MAX: usize = 200;
pub const PEOPLE_API: &str = "https://people.googleapis.com/v1";
const TIMEOUT: Duration = Duration::from_secs(30);
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(600);
/// A 25 MB attachment as Gmail answers it, in base64 inside JSON.
const ATTACHMENT_MAX: u64 = 40 * 1024 * 1024;
/// Requests in flight for one search.
const METADATA_AT_ONCE: usize = 8;
/// A list or metadata answer.
const SMALL_MAX: u64 = 1024 * 1024;
/// A `format=full` message (attachments are not in it, only their ids).
const FULL_MAX: u64 = 32 * 1024 * 1024;

/// Why a Gmail call gave nothing.
#[derive(Debug, PartialEq)]
pub enum Failure {
    /// No usable token: none pushed, expired, or Gmail answered 401.
    AccountRequired,
    /// The sign-in doesn't cover this: Gmail answered 403 for its scopes.
    NeedsAccess,
    /// Gmail couldn't be reached.
    Offline(String),
    /// Anything else, as a plain message.
    Plain(String),
}

/// The Gmail API at a base URL.
#[derive(Clone)]
pub struct Gmail {
    /// Where address suggestions come from.
    people: String,
    agent: ureq::Agent,
    /// For what's big: a whole attachment each way.
    slow: ureq::Agent,
    base: String,
}

impl Default for Gmail {
    fn default() -> Self {
        Self::new(GMAIL_API)
    }
}

/// A Gmail message id: what the API hands out (hex), so it is safe in a
/// path as it is.
pub fn id_ok(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

impl Gmail {
    /// `base` is [`GMAIL_API`] in production; tests pass a local server.
    pub fn new(base: &str) -> Self {
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .timeout_global(Some(TIMEOUT))
            .user_agent("buwiz-gws")
            .build();
        let slow = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .timeout_global(Some(UPLOAD_TIMEOUT))
            .user_agent("buwiz-gws")
            .build();
        Self {
            people: PEOPLE_API.to_owned(),
            agent: config.new_agent(),
            slow: slow.new_agent(),
            base: base.trim_end_matches('/').to_owned(),
        }
    }

    /// The same, with the People API at `base`.
    pub fn with_people(mut self, base: &str) -> Self {
        self.people = base.trim_end_matches('/').to_owned();
        self
    }

    /// `(name, address)` of the person's contacts, and of the people Google
    /// keeps from their mail, that match `query`. An empty `query` is the
    /// warm-up Google asks for before the first search.
    pub fn contacts(&self, auth: &Bearer, query: &str) -> Result<Vec<(String, String)>, Failure> {
        let mut found = Vec::new();
        for (path, mask) in [
            ("people:searchContacts", "names,emailAddresses"),
            ("otherContacts:search", "names,emailAddresses"),
        ] {
            let req = self
                .agent
                .get(format!("{}/{path}", self.people))
                .header("authorization", auth.header())
                .header("accept", "application/json")
                .query("query", query)
                .query("readMask", mask)
                .query("pageSize", "10");
            let Some(v) = Self::answer(req.call(), FULL_MAX)? else {
                continue;
            };
            let people = v.get("results").and_then(Value::as_array);
            for person in people.into_iter().flatten() {
                let name = person
                    .pointer("/person/names/0/displayName")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let addresses = person
                    .pointer("/person/emailAddresses")
                    .and_then(Value::as_array);
                for address in addresses.into_iter().flatten() {
                    if let Some(address) = address.get("value").and_then(Value::as_str) {
                        found.push((name.to_owned(), address.to_owned()));
                    }
                }
            }
        }
        Ok(found)
    }

    /// A GET of `path` under the base: its JSON, or `None` for a 404.
    fn get(
        &self,
        auth: &Bearer,
        path: String,
        query: Vec<(&'static str, String)>,
        cap: u64,
    ) -> Result<Option<Value>, Failure> {
        self.get_within(auth, path, query, cap, None)
    }

    /// [`Self::get`], given up on after `within` when that's set.
    fn get_within(
        &self,
        auth: &Bearer,
        path: String,
        query: Vec<(&'static str, String)>,
        cap: u64,
        within: Option<Duration>,
    ) -> Result<Option<Value>, Failure> {
        let url = format!("{}/{path}", self.base);
        let mut req = self
            .agent
            .get(&url)
            .config()
            .timeout_global(within.or(Some(TIMEOUT)))
            .build()
            .header("authorization", auth.header())
            .header("accept", "application/json");
        for (k, v) in &query {
            req = req.query(*k, v);
        }
        Self::answer(req.call(), cap)
    }

    /// A POST: a person's press, or a draft the model filled, which sends nothing.
    fn post(&self, auth: &Bearer, path: String, body: &Value) -> Result<Option<Value>, Failure> {
        let url = format!("{}/{path}", self.base);
        let req = self
            .agent
            .post(&url)
            .header("authorization", auth.header())
            .header("accept", "application/json")
            .header("content-type", "application/json");
        Self::answer(req.send(body.to_string().as_bytes()), FULL_MAX)
    }

    fn answer(
        res: Result<ureq::http::Response<ureq::Body>, ureq::Error>,
        cap: u64,
    ) -> Result<Option<Value>, Failure> {
        let mut res = res.map_err(|e| {
            Failure::Offline(format!(
                "could not reach Gmail: {}",
                js::slice_utf16(&e.to_string(), 200)
            ))
        })?;
        let status = res.status().as_u16();
        let body = res
            .body_mut()
            .with_config()
            .limit(cap)
            .read_to_vec()
            .map_err(|e| {
                Failure::Plain(format!(
                    "Gmail's answer could not be read: {}",
                    js::slice_utf16(&e.to_string(), 200)
                ))
            })?;
        match status {
            // a delete answers with nothing
            200 | 204 if body.is_empty() => Ok(Some(Value::Null)),
            200 => serde_json::from_slice(&body)
                .map(Some)
                .map_err(|_| Failure::Plain("Gmail's answer is not JSON".to_owned())),
            401 => Err(Failure::AccountRequired),
            404 => Ok(None),
            _ => {
                let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let why = v
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("no reason given");
                if status == 403 && why.contains("insufficient") {
                    return Err(Failure::NeedsAccess);
                }
                Err(Failure::Plain(match status {
                    429 => "Gmail is busy right now. Try again in a moment.".to_owned(),
                    500.. => "Gmail had a problem just now. Try again.".to_owned(),
                    _ => format!("Gmail didn't accept that: {}", js::slice_utf16(why, 300)),
                }))
            }
        }
    }

    /// The ids of the first `max` messages `q` finds.
    pub fn search(&self, auth: &Bearer, q: &str, max: usize) -> Result<Vec<String>, Failure> {
        let found = self.get(
            auth,
            "messages".to_owned(),
            vec![("q", q.to_owned()), ("maxResults", max.to_string())],
            SMALL_MAX,
        )?;
        let list = found
            .as_ref()
            .and_then(|v| v.get("messages"))
            .and_then(Value::as_array);
        Ok(list
            .into_iter()
            .flatten()
            .filter_map(|m| m.get("id").and_then(Value::as_str))
            .map(str::to_owned)
            .collect())
    }

    /// Each message's From, Subject, Date and snippet (`format=metadata`),
    /// fetched together; `None` for one Gmail no longer has.
    pub fn metadata(&self, auth: &Bearer, ids: &[String]) -> Result<Vec<Option<Value>>, Failure> {
        self.metadata_with(auth, ids, &["From", "Subject", "Date"])
    }

    /// Each message's `names` headers, in order.
    pub fn metadata_with(
        &self,
        auth: &Bearer,
        ids: &[String],
        names: &[&str],
    ) -> Result<Vec<Option<Value>>, Failure> {
        let headers: Vec<(&str, String)> = names
            .iter()
            .map(|h| ("metadataHeaders", (*h).to_owned()))
            .collect();
        let mut out = Vec::with_capacity(ids.len());
        for batch in ids.chunks(METADATA_AT_ONCE) {
            let done: Result<Vec<_>, Failure> = std::thread::scope(|scope| {
                let gets: Vec<_> = batch
                    .iter()
                    .map(|id| {
                        let mut query = vec![("format", "metadata".to_owned())];
                        query.extend(headers.iter().cloned());
                        id_ok(id).then(|| {
                            scope.spawn(move || {
                                self.get(auth, format!("messages/{id}"), query, SMALL_MAX)
                            })
                        })
                    })
                    .collect();
                let mut got = Vec::with_capacity(gets.len());
                for g in gets {
                    got.push(match g {
                        Some(g) => g.join().unwrap_or_else(|_| {
                            Err(Failure::Plain("the Gmail request failed".to_owned()))
                        })?,
                        None => None,
                    });
                }
                Ok(got)
            });
            out.extend(done?);
        }
        Ok(out)
    }

    /// The labels, each with its counts.
    pub fn labels(&self, auth: &Bearer) -> Result<Vec<Value>, Failure> {
        let list = self.get(auth, "labels".to_owned(), Vec::new(), SMALL_MAX)?;
        let ids: Vec<String> = list
            .iter()
            .flat_map(|v| {
                v.get("labels")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default()
            })
            .filter_map(|l| l.get("id").and_then(Value::as_str).map(str::to_owned))
            .collect();
        let mut out = Vec::with_capacity(ids.len());
        for batch in ids.chunks(METADATA_AT_ONCE) {
            let done: Result<Vec<_>, Failure> = std::thread::scope(|scope| {
                let gets: Vec<_> = batch
                    .iter()
                    .filter(|id| id_ok(id))
                    .map(|id| {
                        scope.spawn(move || {
                            self.get(auth, format!("labels/{id}"), Vec::new(), SMALL_MAX)
                        })
                    })
                    .collect();
                gets.into_iter()
                    .map(|g| {
                        g.join().unwrap_or_else(|_| {
                            Err(Failure::Plain("the Gmail request failed".to_owned()))
                        })
                    })
                    .collect()
            });
            out.extend(done?.into_iter().flatten());
        }
        Ok(out)
    }

    /// The account's latest history id.
    pub fn history_now(&self, auth: &Bearer) -> Result<Option<String>, Failure> {
        let profile = self.get(auth, "profile".to_owned(), Vec::new(), SMALL_MAX)?;
        Ok(profile.and_then(|p| {
            p.get("historyId")
                .and_then(Value::as_str)
                .map(str::to_owned)
        }))
    }

    /// A page of threads: their ids, and the next page's token.
    pub fn thread_ids(
        &self,
        auth: &Bearer,
        label: Option<&str>,
        q: Option<&str>,
        page: Option<&str>,
        max: usize,
    ) -> Result<(Vec<String>, Option<String>), Failure> {
        let mut query = vec![("maxResults", max.to_string())];
        query.extend(label.map(|l| ("labelIds", l.to_owned())));
        query.extend(q.map(|q| ("q", q.to_owned())));
        query.extend(page.map(|p| ("pageToken", p.to_owned())));
        let found = self.get(auth, "threads".to_owned(), query, SMALL_MAX)?;
        let ids = found
            .iter()
            .flat_map(|v| {
                v.get("threads")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default()
            })
            .filter_map(|t| t.get("id").and_then(Value::as_str).map(str::to_owned))
            .collect();
        Ok((
            ids,
            found
                .as_ref()
                .and_then(|v| v.get("nextPageToken"))
                .and_then(Value::as_str)
                .map(str::to_owned),
        ))
    }

    /// Threads by id, as headers only; `None` for one that's gone.
    pub fn threads_metadata(
        &self,
        auth: &Bearer,
        ids: &[String],
    ) -> Result<Vec<Option<Value>>, Failure> {
        let headers = ["From", "To", "Subject", "Date"].map(|h| ("metadataHeaders", h.to_owned()));
        let mut out = Vec::with_capacity(ids.len());
        for batch in ids.chunks(METADATA_AT_ONCE) {
            let done: Result<Vec<_>, Failure> = std::thread::scope(|scope| {
                let gets: Vec<_> = batch
                    .iter()
                    .map(|id| {
                        let mut query = vec![("format", "metadata".to_owned())];
                        query.extend(headers.iter().cloned());
                        id_ok(id).then(|| {
                            scope.spawn(move || {
                                self.get(auth, format!("threads/{id}"), query, SMALL_MAX)
                            })
                        })
                    })
                    .collect();
                gets.into_iter()
                    .map(|g| {
                        g.map_or(Ok(None), |g| {
                            g.join().unwrap_or_else(|_| {
                                Err(Failure::Plain("the Gmail request failed".to_owned()))
                            })
                        })
                    })
                    .collect()
            });
            out.extend(done?);
        }
        Ok(out)
    }

    /// A whole thread, `format=full`.
    pub fn thread_full(&self, auth: &Bearer, id: &str) -> Result<Option<Value>, Failure> {
        self.thread_full_within(auth, id, TIMEOUT)
    }

    /// [`Self::thread_full`], given up on after `within`.
    pub fn thread_full_within(
        &self,
        auth: &Bearer,
        id: &str,
        within: Duration,
    ) -> Result<Option<Value>, Failure> {
        if !id_ok(id) {
            return Ok(None);
        }
        self.get_within(
            auth,
            format!("threads/{id}"),
            vec![("format", "full".to_owned())],
            FULL_MAX,
            Some(within),
        )
    }

    /// Each message of a thread, with its labels now.
    pub fn thread_labels(&self, auth: &Bearer, id: &str) -> Result<Option<Messages>, Failure> {
        let query = vec![("format", "minimal".to_owned())];
        let Some(v) = self.get(auth, format!("threads/{id}"), query, FULL_MAX)? else {
            return Ok(None);
        };
        let text = |l: &Value| l.as_str().map(str::to_owned);
        Ok(Some(
            v.get("messages")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|m| {
                    let labels = m.get("labelIds").and_then(Value::as_array);
                    Some((
                        text(m.get("id")?)?,
                        labels.into_iter().flatten().filter_map(text).collect(),
                    ))
                })
                .collect(),
        ))
    }

    /// A draft write's JSON, without the message when that goes as a part.
    fn draft_metadata(thread: Option<&str>) -> Value {
        match thread {
            Some(thread) => serde_json::json!({"message": {"threadId": thread}}),
            None => serde_json::json!({"message": {}}),
        }
    }

    fn draft_body(raw: &str, thread: Option<&str>) -> Value {
        let mut body = Self::draft_metadata(thread);
        body["message"]["raw"] = raw.into();
        body
    }

    pub fn draft_create(
        &self,
        auth: &Bearer,
        raw: &str,
        thread: Option<&str>,
    ) -> Result<Option<Value>, Failure> {
        self.post(auth, "drafts".to_owned(), &Self::draft_body(raw, thread))
    }

    pub fn draft_update(
        &self,
        auth: &Bearer,
        id: &str,
        raw: &str,
        thread: Option<&str>,
    ) -> Result<Option<Value>, Failure> {
        let req = self
            .agent
            .put(format!("{}/drafts/{id}", self.base))
            .header("authorization", auth.header())
            .header("accept", "application/json")
            .header("content-type", "application/json");
        let body = Self::draft_body(raw, thread).to_string();
        Self::answer(req.send(body.as_bytes()), FULL_MAX)
    }

    /// A draft with attachments, through Gmail's upload door: the JSON one
    /// takes a few megabytes, this one the whole message. `id` replaces a draft.
    pub fn draft_upload(
        &self,
        auth: &Bearer,
        id: Option<&str>,
        message: &[u8],
        thread: Option<&str>,
    ) -> Result<Option<Value>, Failure> {
        let boundary = "buwiz-gws-upload-boundary";
        let metadata = Self::draft_metadata(thread);
        let mut body = format!(
            "--{boundary}\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n{metadata}\r\n--{boundary}\r\nContent-Type: message/rfc822\r\n\r\n"
        )
        .into_bytes();
        body.extend_from_slice(message);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let base = self.base.replacen("/gmail/v1/", "/upload/gmail/v1/", 1);
        let kind = format!("multipart/related; boundary={boundary}");
        let url = match id {
            Some(id) => format!("{base}/drafts/{id}"),
            None => format!("{base}/drafts"),
        };
        let req = match id {
            Some(_) => self.slow.put(&url),
            None => self.slow.post(&url),
        };
        let req = req
            .query("uploadType", "multipart")
            .header("authorization", auth.header())
            .header("accept", "application/json")
            .header("content-type", &kind);
        Self::answer(req.send(&body[..]), FULL_MAX)
    }

    /// An attachment's bytes, from the message that holds it.
    pub fn attachment(
        &self,
        auth: &Bearer,
        message: &str,
        id: &str,
    ) -> Result<Option<Vec<u8>>, Failure> {
        self.attachment_within(auth, message, id, UPLOAD_TIMEOUT)
    }

    /// [`Self::attachment`], given up on after `within`.
    pub fn attachment_within(
        &self,
        auth: &Bearer,
        message: &str,
        id: &str,
        within: Duration,
    ) -> Result<Option<Vec<u8>>, Failure> {
        let url = format!("{}/messages/{message}/attachments/{id}", self.base);
        let req = self
            .slow
            .get(&url)
            .config()
            .timeout_global(Some(within))
            .build()
            .header("authorization", auth.header())
            .header("accept", "application/json");
        let Some(v) = Self::answer(req.call(), ATTACHMENT_MAX)? else {
            return Ok(None);
        };
        let data = v.get("data").and_then(Value::as_str).unwrap_or_default();
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(data.trim_end_matches('='))
            .map_err(|_| Failure::Plain("That attachment came back damaged.".to_owned()))?;
        Ok(Some(bytes))
    }

    /// Marks each invite's text/calendar alternative whose file holds the same
    /// bytes, so [`attachments`] shows that event once. A rescheduled invite
    /// can carry another `invite.ics` of the same size, so the size alone says
    /// nothing. What can't be read stays, as two is better than a lost file.
    ///
    /// All of it keeps to [`TWINS_WITHIN`], each fetch to [`TIMEOUT`] at most:
    /// it comes before the thread shows. Out of time, the pairs not yet read
    /// stay two cards.
    pub fn mark_twins(&self, auth: &Bearer, thread: &mut Value) {
        self.mark_twins_by(auth, thread, std::time::Instant::now() + TWINS_WITHIN);
    }

    /// [`Self::mark_twins`], done by `by`.
    pub fn mark_twins_by(&self, auth: &Bearer, thread: &mut Value, by: std::time::Instant) {
        let Some(messages) = thread.get_mut("messages").and_then(Value::as_array_mut) else {
            return;
        };
        for message in messages {
            let Some(id) = message.get("id").and_then(Value::as_str).map(str::to_owned) else {
                continue;
            };
            let bytes = |p: &Value| match data_of(p) {
                Some(data) => Some(data),
                None => {
                    let att = p.pointer("/body/attachmentId").and_then(Value::as_str)?;
                    let left = by
                        .checked_duration_since(std::time::Instant::now())
                        .filter(|d| !d.is_zero())?;
                    self.attachment_within(auth, &id, att, left.min(TIMEOUT))
                        .ok()
                        .flatten()
                }
            };
            // the calendar part's bytes once, against each file that may be it
            let mut read: Vec<(String, Option<Vec<u8>>)> = Vec::new();
            let same: Vec<String> = twin_pairs(message)
                .into_iter()
                .filter(|(cal, file)| {
                    let key = cal
                        .get("partId")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let at = match read.iter().position(|(k, _)| k == key) {
                        Some(at) => at,
                        None => {
                            read.push((key.to_owned(), bytes(cal)));
                            read.len() - 1
                        }
                    };
                    read[at]
                        .1
                        .as_ref()
                        .is_some_and(|a| bytes(file).is_some_and(|b| *a == b))
                })
                .filter_map(|(cal, _)| cal.get("partId").and_then(Value::as_str).map(str::to_owned))
                .collect();
            for part in same {
                if let Some(cal) = message.get_mut("payload").and_then(|p| part_mut(p, &part)) {
                    cal[TWIN] = Value::Bool(true);
                }
            }
        }
    }

    /// The id of the draft in conversation `thread`, among the newest there are.
    pub fn draft_in_thread(&self, auth: &Bearer, thread: &str) -> Result<Option<String>, Failure> {
        let query = vec![("maxResults", "500".to_owned())];
        let Some(list) = self.get(auth, "drafts".to_owned(), query, FULL_MAX)? else {
            return Ok(None);
        };
        Ok(list
            .get("drafts")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find(|d| d.pointer("/message/threadId").and_then(Value::as_str) == Some(thread))
            .and_then(|d| d.get("id")?.as_str().map(str::to_owned)))
    }

    pub fn draft(&self, auth: &Bearer, id: &str) -> Result<Option<Value>, Failure> {
        let query = vec![("format", "full".to_owned())];
        self.get(auth, format!("drafts/{id}"), query, FULL_MAX)
    }

    pub fn draft_delete(&self, auth: &Bearer, id: &str) -> Result<(), Failure> {
        let req = self
            .agent
            .delete(format!("{}/drafts/{id}", self.base))
            .header("authorization", auth.header());
        Self::answer(req.call(), FULL_MAX).map(drop)
    }

    /// `false` when Gmail has no such draft.
    pub fn draft_send(&self, auth: &Bearer, id: &str) -> Result<bool, Failure> {
        let body = serde_json::json!({"id": id});
        Ok(self.post(auth, "drafts/send".to_owned(), &body)?.is_some())
    }

    pub fn modify_thread(
        &self,
        auth: &Bearer,
        id: &str,
        add: &[String],
        remove: &[String],
    ) -> Result<(), Failure> {
        let body = serde_json::json!({"addLabelIds": add, "removeLabelIds": remove});
        self.post(auth, format!("threads/{id}/modify"), &body)
            .map(drop)
    }

    pub fn modify_message(
        &self,
        auth: &Bearer,
        id: &str,
        add: &[String],
        remove: &[String],
    ) -> Result<(), Failure> {
        let body = serde_json::json!({"addLabelIds": add, "removeLabelIds": remove});
        self.post(auth, format!("messages/{id}/modify"), &body)
            .map(drop)
    }

    pub fn trash_thread(&self, auth: &Bearer, id: &str) -> Result<(), Failure> {
        self.post(auth, format!("threads/{id}/trash"), &Value::Null)
            .map(drop)
    }

    pub fn untrash_thread(&self, auth: &Bearer, id: &str) -> Result<(), Failure> {
        self.post(auth, format!("threads/{id}/untrash"), &Value::Null)
            .map(drop)
    }

    /// What changed since `start`: the threads touched and the newest history
    /// id. `None`: `start` is too old, so a full sync is needed.
    pub fn history(
        &self,
        auth: &Bearer,
        start: &str,
    ) -> Result<Option<(Vec<String>, String)>, Failure> {
        let (mut touched, mut newest, mut page) = (Vec::new(), start.to_owned(), None::<String>);
        loop {
            let mut query = vec![
                ("startHistoryId", start.to_owned()),
                ("maxResults", "500".to_owned()),
            ];
            query.extend(page.take().map(|p| ("pageToken", p)));
            let Some(v) = self.get(auth, "history".to_owned(), query, FULL_MAX)? else {
                return Ok(None);
            };
            for h in v
                .get("history")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                for kind in [
                    "messages",
                    "messagesAdded",
                    "messagesDeleted",
                    "labelsAdded",
                    "labelsRemoved",
                ] {
                    for m in h.get(kind).and_then(Value::as_array).into_iter().flatten() {
                        let thread = m
                            .pointer("/message/threadId")
                            .or_else(|| m.get("threadId"))
                            .and_then(Value::as_str);
                        if let Some(t) = thread.filter(|t| !touched.iter().any(|x: &String| x == t))
                        {
                            touched.push(t.to_owned());
                        }
                    }
                }
            }
            if let Some(id) = v.get("historyId").and_then(Value::as_str) {
                newest = id.to_owned();
            }
            match v.get("nextPageToken").and_then(Value::as_str) {
                Some(next) => page = Some(next.to_owned()),
                None => return Ok(Some((touched, newest))),
            }
        }
    }

    /// One message, `format=full`; `None` when Gmail has no such message.
    pub fn full(&self, auth: &Bearer, id: &str) -> Result<Option<Value>, Failure> {
        if !id_ok(id) {
            return Ok(None);
        }
        self.get(
            auth,
            format!("messages/{id}"),
            vec![("format", "full".to_owned())],
            FULL_MAX,
        )
    }
}

/// A header of a message's `payload` (names are case-insensitive), trimmed;
/// empty when missing.
pub fn header(message: &Value, name: &str) -> String {
    raw_header(message, name).to_owned()
}

/// A header held to [`HEADER_MAX`], without copying more of it than that: the
/// sender writes these, and a mail's can run to megabytes.
pub fn short_header(message: &Value, name: &str) -> String {
    cap_bytes(raw_header(message, name), HEADER_MAX)
}

fn raw_header<'a>(message: &'a Value, name: &str) -> &'a str {
    let headers = message
        .pointer("/payload/headers")
        .and_then(Value::as_array);
    let found = headers.into_iter().flatten().find(|h| {
        h.get("name")
            .and_then(Value::as_str)
            .is_some_and(|n| n.eq_ignore_ascii_case(name))
    });
    js::trim(
        found
            .and_then(|h| h.get("value"))
            .and_then(Value::as_str)
            .unwrap_or_default(),
    )
}

/// Gmail's `snippet` (HTML-escaped) as plain text on one line.
pub fn snippet(message: &Value) -> String {
    snippet_of(&decode_entities(
        message
            .get("snippet")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    ))
}

/// The message's body as plain text: what its first text/html part shows,
/// else its first text/plain part; attachments are skipped. HTML first: that's
/// what the reader shows, and a plain part nobody sees could say anything.
pub fn body(message: &Value) -> String {
    let mut leaves = Vec::new();
    if let Some(p) = message.get("payload") {
        collect_leaves(p, &mut leaves);
    }
    let mime = |p: &Value| {
        p.get("mimeType")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_ascii_lowercase()
    };
    let text = if let Some(p) = leaves.iter().find(|p| mime(p) == "text/html") {
        clean::visible_text(&decode_part(p))
    } else if let Some(p) = leaves.iter().find(|p| mime(p) == "text/plain") {
        decode_part(p)
    } else {
        String::new()
    };
    text.replace("\r\n", "\n")
        .trim_end_matches(['\n', '\r', ' ', '\t'])
        .to_owned()
}

/// The message's HTML part, as text; `None` when it has none.
pub fn html_body(message: &Value) -> Option<String> {
    let mut leaves = Vec::new();
    if let Some(p) = message.get("payload") {
        collect_leaves(p, &mut leaves);
    }
    let is_html = |p: &&&Value| {
        p.get("mimeType")
            .and_then(Value::as_str)
            .is_some_and(|m| m.eq_ignore_ascii_case("text/html"))
    };
    leaves.iter().find(is_html).map(|p| decode_part(p))
}

/// What [`Gmail::mark_twins`] sets on a calendar part whose bytes are a file's
/// too. Google never sends this key, and a sender writes MIME, not this JSON.
const TWIN: &str = "buwizTwin";
/// The largest calendar part compared byte for byte; past it both stay.
const TWIN_MAX: u64 = 256 * 1024;
/// The most (calendar part, file) pairs compared in one message. Each is a
/// fetch, and the calendar part's own one more: an invite has one pair, and a
/// mail with more than this many files of one name and size isn't one. Past it,
/// the rest stay as cards.
const TWINS_MAX: usize = 16;
/// How long the twin check may take for a whole thread, before it shows.
const TWINS_WITHIN: Duration = Duration::from_secs(5);

/// Every part with a file name, and whether it sits anywhere under a
/// multipart/alternative.
fn named_parts(message: &Value) -> Vec<(&Value, bool)> {
    fn walk<'a>(part: &'a Value, alternative: bool, out: &mut Vec<(&'a Value, bool)>) {
        let named = part
            .get("filename")
            .and_then(Value::as_str)
            .is_some_and(|n| !n.is_empty());
        if named {
            out.push((part, alternative));
        }
        let inside = alternative
            || part
                .get("mimeType")
                .and_then(Value::as_str)
                .is_some_and(|m| m.eq_ignore_ascii_case("multipart/alternative"));
        for p in part
            .get("parts")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            walk(p, inside, out);
        }
    }
    let mut found = Vec::new();
    if let Some(p) = message.get("payload") {
        walk(p, false, &mut found);
    }
    found
}

fn file_name(part: &Value) -> &str {
    part.get("filename")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn listed(part: &Value) -> Value {
    let id = match part.get("partId").and_then(Value::as_str) {
        Some("") => Some("root"),
        id => id,
    };
    serde_json::json!({"name": file_name(part), "mime": part.get("mimeType"), "size": part.pointer("/body/size"), "id": part.pointer("/body/attachmentId"), "part": id})
}

/// Whether `html` says `cid:<cid>` whole: `cid:logo` isn't `cid:logo2`.
fn names_cid(html: &str, cid: &str) -> bool {
    let wanted = format!("cid:{cid}");
    html.match_indices(&wanted).any(|(at, _)| {
        html[at + wanted.len()..]
            .chars()
            .next()
            .is_none_or(|c| c.is_whitespace() || "\"'>);&".contains(c))
    })
}

/// A picture the letter shows: an image whose Content-ID its HTML names as
/// `cid:`, marked inline or not marked at all (inline too, RFC 2183). Any other
/// part with a Content-ID, a PDF or a picture the letter never shows, is a file.
/// `html` is the message's HTML in lower case.
fn shown_inline(part: &Value, html: &str) -> bool {
    let image = part
        .get("mimeType")
        .and_then(Value::as_str)
        .is_some_and(|m| m.to_ascii_lowercase().starts_with("image/"));
    let cid = part_header(part, "content-id")
        .map(|c| {
            c.trim()
                .trim_start_matches('<')
                .trim_end_matches('>')
                .trim()
        })
        .filter(|c| !c.is_empty());
    image
        && cid.is_some_and(|c| names_cid(html, &c.to_ascii_lowercase()))
        && part_header(part, "content-disposition")
            .is_none_or(|d| d.trim_start().to_ascii_lowercase().starts_with("inline"))
}

/// Attachments as the reader shows them: `{name, mime, size, id, part}`.
/// `part` is what stays the same from read to read, as Google calls a part's
/// id immutable and promises nothing of `attachmentId`. A mail that's one
/// attachment and nothing else has the empty id, and an empty one can't be told
/// from none, so that's `root`.
///
/// A picture the letter shows by its Content-ID is the letter's, and not a
/// file. An invite's event comes twice, as Google sends it: the text/calendar
/// alternative of the letter and an application/ics file. The alternative goes
/// once [`Gmail::mark_twins`] has found the file holds the same bytes. Two files
/// are never made one, whatever their names.
pub fn attachments(message: &Value) -> Vec<Value> {
    let html = html_body(message).unwrap_or_default().to_ascii_lowercase();
    named_parts(message)
        .into_iter()
        .filter(|(p, _)| !shown_inline(p, &html) && p.get(TWIN) != Some(&Value::Bool(true)))
        .map(|(p, _)| listed(p))
        .collect()
}

/// Every part a draft or a forward carries on: each with a file name, the
/// pictures the letter shows by Content-ID too, and nothing made one, so a
/// draft saved again loses none of what it held.
pub fn carried(message: &Value) -> Vec<Value> {
    named_parts(message)
        .into_iter()
        .map(|(p, _)| listed(p))
        .collect()
}

/// The pairs that may be one event twice: a text/calendar part under an
/// alternative, and each application/ics file outside one with its name and
/// its size. Their bytes decide. `(the calendar part, the file)`.
fn twin_pairs(message: &Value) -> Vec<(&Value, &Value)> {
    let parts = named_parts(message);
    let typed = |p: &Value, kind: &str| {
        p.get("mimeType")
            .and_then(Value::as_str)
            .is_some_and(|m| m.eq_ignore_ascii_case(kind))
    };
    let size = |p: &Value| p.pointer("/body/size").and_then(Value::as_u64);
    let mut pairs = Vec::new();
    for (cal, _) in parts
        .iter()
        .filter(|(p, alt)| *alt && typed(p, "text/calendar"))
    {
        let Some(n) = size(cal).filter(|n| *n <= TWIN_MAX) else {
            continue;
        };
        for (file, _) in parts.iter().filter(|(p, alt)| {
            !*alt
                && typed(p, "application/ics")
                && file_name(p) == file_name(cal)
                && size(p) == Some(n)
        }) {
            if pairs.len() == TWINS_MAX {
                return pairs;
            }
            pairs.push((*cal, *file));
        }
    }
    pairs
}

/// A part's own `body.data` as bytes: how Gmail can send a small file, with
/// no attachment id to fetch it by.
pub fn data_of(part: &Value) -> Option<Vec<u8>> {
    let data = part.pointer("/body/data").and_then(Value::as_str)?;
    let clean: String = data
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '=')
        .map(|c| match c {
            '+' => '-',
            '/' => '_',
            c => c,
        })
        .collect();
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(clean)
        .ok()
}

/// The bytes a part holds in the message itself, by the id
/// [`attachments`] and [`carried`] give it (`root` for the whole message).
pub fn part_bytes(message: &Value, part: &str) -> Option<Vec<u8>> {
    let id = if part == "root" { "" } else { part };
    fn find<'a>(p: &'a Value, id: &str) -> Option<&'a Value> {
        if p.get("partId").and_then(Value::as_str) == Some(id) {
            return Some(p);
        }
        p.get("parts")?.as_array()?.iter().find_map(|q| find(q, id))
    }
    data_of(find(message.get("payload")?, id)?)
}

fn part_mut<'a>(part: &'a mut Value, id: &str) -> Option<&'a mut Value> {
    if part.get("partId").and_then(Value::as_str) == Some(id) {
        return Some(part);
    }
    part.get_mut("parts")?
        .as_array_mut()?
        .iter_mut()
        .find_map(|p| part_mut(p, id))
}

fn collect_leaves<'a>(part: &'a Value, out: &mut Vec<&'a Value>) {
    match part.get("parts").and_then(Value::as_array) {
        Some(parts) if !parts.is_empty() => parts.iter().for_each(|p| collect_leaves(p, out)),
        _ => {
            let named = part
                .get("filename")
                .and_then(Value::as_str)
                .is_some_and(|f| !f.is_empty());
            let attached = part_header(part, "content-disposition").is_some_and(|d| {
                d.trim_start()
                    .to_ascii_lowercase()
                    .starts_with("attachment")
            });
            if !named && !attached {
                out.push(part);
            }
        }
    }
}

fn part_header<'a>(part: &'a Value, name: &str) -> Option<&'a str> {
    let headers = part.get("headers").and_then(Value::as_array)?;
    headers
        .iter()
        .find(|h| {
            h.get("name")
                .and_then(Value::as_str)
                .is_some_and(|n| n.eq_ignore_ascii_case(name))
        })
        .and_then(|h| h.get("value"))
        .and_then(Value::as_str)
}

/// A part's `body.data` (base64url, padded or not) as text in its
/// charset: UTF-8 (invalid bytes replaced), or Latin-1 when it says so.
fn decode_part(part: &Value) -> String {
    let data = part
        .pointer("/body/data")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let clean: String = data
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '=')
        .map(|c| match c {
            '+' => '-',
            '/' => '_',
            c => c,
        })
        .collect();
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(clean)
        .unwrap_or_default();
    let ctype = part_header(part, "content-type")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let charset = ctype
        .split(';')
        .find_map(|p| p.trim().strip_prefix("charset="))
        .map(|c| c.trim_matches('"').to_owned())
        .unwrap_or_default();
    if matches!(
        charset.as_str(),
        "iso-8859-1" | "latin1" | "latin-1" | "windows-1252" | "cp1252"
    ) {
        bytes.iter().map(|&b| char::from(b)).collect()
    } else {
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

/// `&amp;`, `&lt;`, `&gt;`, `&quot;`, `&apos;`, `&nbsp;` and numeric
/// references decoded; any other `&` kept.
pub fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let end = rest[1..]
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '#'))
            .map(|i| i + 1)
            .filter(|&i| rest[i..].starts_with(';'));
        let decoded = end.and_then(|e| {
            let name = &rest[1..e];
            let c = match name {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some('\u{a0}'),
                _ => {
                    let n = name
                        .strip_prefix("#x")
                        .or_else(|| name.strip_prefix("#X"))
                        .map(|h| u32::from_str_radix(h, 16))
                        .or_else(|| name.strip_prefix('#').map(str::parse::<u32>));
                    n.and_then(Result::ok).and_then(char::from_u32)
                }
            };
            c.map(|c| (c, e + 1))
        });
        match decoded {
            Some((c, len)) => {
                out.push(c);
                rest = &rest[len..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

pub const BLOCKS: [&str; 18] = [
    "br",
    "p",
    "div",
    "tr",
    "li",
    "ul",
    "ol",
    "table",
    "blockquote",
    "hr",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "section",
    "article",
];

/// Text with its lines tidied: spaces run together, one blank line at most.
pub fn tidy(text: &str) -> String {
    let mut out = String::new();
    let mut blank = 0;
    for line in text.split('\n') {
        let line = line.replace('\u{a0}', " ");
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.is_empty() {
            blank += 1;
            continue;
        }
        if !out.is_empty() {
            out.push_str(if blank > 0 { "\n\n" } else { "\n" });
        }
        blank = 0;
        out.push_str(&line);
    }
    out
}

/// The text on one line, at most 200 UTF-16 units with the ellipsis.
pub fn snippet_of(body: &str) -> String {
    let mut flat = String::new();
    let mut gap = false;
    for c in body.chars() {
        if js::is_space(c) {
            gap = true;
        } else {
            if gap && !flat.is_empty() {
                flat.push(' ');
            }
            gap = false;
            flat.push(c);
        }
    }
    if js::len_utf16(&flat) > SNIPPET_MAX {
        format!("{}…", js::slice_utf16(&flat, SNIPPET_MAX - 1))
    } else {
        flat
    }
}

/// `text` within `max` UTF-8 bytes, marked when cut; never half a character.
pub fn cap_bytes(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    const MARKER: &str = "…[truncated]";
    let n = max - MARKER.len();
    let mut end = n;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut cut = &text[..end];
    // as Node cut it: one trailing U+FFFD goes (the half character, or, cut
    // on a boundary, a real one that ends the cut)
    if end == n {
        cut = cut.strip_suffix('\u{fffd}').unwrap_or(cut);
    }
    format!("{cut}{MARKER}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn b64(s: &str) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(s)
    }

    #[test]
    fn headers_are_found_case_insensitively() {
        let m = json!({"payload": {"headers": [{"name": "from", "value": "  Ann <ann@example.com> "}, {"name": "Subject", "value": "Hi"}]}});
        assert_eq!(header(&m, "From"), "Ann <ann@example.com>");
        assert_eq!(header(&m, "subject"), "Hi");
        assert_eq!(header(&m, "Date"), "");
        assert_eq!(header(&json!({}), "From"), "");
    }

    #[test]
    fn an_attachment_says_its_part_and_a_mail_that_is_only_one_has_a_word_for_it() {
        let mixed = json!({"payload": {"partId": "", "mimeType": "multipart/mixed", "parts": [
            {"partId": "0", "mimeType": "text/plain", "body": {"data": b64("Hi")}},
            {"partId": "1", "mimeType": "text/csv", "filename": "a.csv", "body": {"attachmentId": "x"}},
            {"partId": "2", "mimeType": "multipart/mixed", "parts": [
                {"partId": "2.0", "mimeType": "text/csv", "filename": "b.csv", "body": {"attachmentId": "y"}},
            ]},
        ]}});
        let parts: Vec<Value> = attachments(&mixed)
            .iter()
            .map(|a| a["part"].clone())
            .collect();
        assert_eq!(parts, [json!("1"), json!("2.0")]);
        let alone = json!({"payload": {"partId": "", "mimeType": "text/csv", "filename": "a.csv", "body": {"attachmentId": "x"}}});
        assert_eq!(attachments(&alone)[0]["part"], "root");
        // one that doesn't say can't pass for another: it opens nothing
        let silent = json!({"payload": {"mimeType": "text/csv", "filename": "a.csv"}});
        assert_eq!(attachments(&silent)[0]["part"], Value::Null);
    }

    fn names(message: &Value) -> Vec<(String, String)> {
        attachments(message)
            .iter()
            .map(|a| {
                (
                    a["name"].as_str().unwrap().to_owned(),
                    a["part"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    }

    fn pair(name: &str, part: &str) -> (String, String) {
        (name.to_owned(), part.to_owned())
    }

    fn twins(message: &Value) -> Vec<(String, String)> {
        twin_pairs(message)
            .into_iter()
            .map(|(c, f)| pair(c["partId"].as_str().unwrap(), f["partId"].as_str().unwrap()))
            .collect()
    }

    fn invite(calendar_size: u64, deeper: bool) -> Value {
        let calendar = json!({"partId": "0.1", "mimeType": "text/calendar", "filename": "invite.ics", "body": {"attachmentId": "x", "size": calendar_size}});
        let calendar = if deeper {
            json!({"partId": "0.1", "mimeType": "multipart/related", "parts": [
                {"partId": "0.1.0", "mimeType": "text/calendar", "filename": "invite.ics", "body": {"attachmentId": "x", "size": calendar_size}},
            ]})
        } else {
            calendar
        };
        json!({"payload": {"partId": "", "mimeType": "multipart/mixed", "parts": [
            {"partId": "0", "mimeType": "multipart/alternative", "parts": [
                {"partId": "0.0", "mimeType": "text/plain", "body": {"data": b64("Hi")}},
                calendar,
            ]},
            {"partId": "1", "mimeType": "application/ics", "filename": "invite.ics", "headers": [{"name": "Content-Disposition", "value": "attachment; filename=\"invite.ics\""}], "body": {"attachmentId": "y", "size": 640}},
        ]}})
    }

    #[test]
    fn an_invites_calendar_part_and_its_file_of_one_size_may_be_one_event() {
        assert_eq!(twins(&invite(640, false)), [pair("0.1", "1")]);
        // a rescheduled invite of another size is another event
        assert_eq!(twins(&invite(651, false)), []);
    }

    #[test]
    fn only_an_ics_file_can_be_the_twin_and_every_one_is_compared() {
        let mut two = invite(640, false);
        let second = json!({"partId": "2", "mimeType": "application/ics", "filename": "invite.ics", "body": {"attachmentId": "z", "size": 640}});
        two["payload"]["parts"].as_array_mut().unwrap().push(second);
        assert_eq!(twins(&two), [pair("0.1", "1"), pair("0.1", "2")]);
        // a file of the same name and size that isn't an event is no twin
        let mut other = invite(640, false);
        other["payload"]["parts"][1]["mimeType"] = json!("application/octet-stream");
        assert_eq!(twins(&other), []);
    }

    #[test]
    fn a_small_file_is_read_from_its_own_part() {
        let mail = json!({"payload": {"partId": "", "mimeType": "multipart/mixed", "parts": [
            {"partId": "0", "mimeType": "text/plain", "body": {"data": b64("Hi")}},
            {"partId": "1", "mimeType": "text/csv", "filename": "a.csv", "body": {"data": b64("a,b"), "size": 3}},
        ]}});
        assert_eq!(part_bytes(&mail, "1"), Some(b"a,b".to_vec()));
        assert_eq!(part_bytes(&mail, "2"), None);
        let alone = json!({"payload": {"partId": "", "mimeType": "text/csv", "filename": "a.csv", "body": {"data": b64("x")}}});
        assert_eq!(part_bytes(&alone, "root"), Some(b"x".to_vec()));
    }

    #[test]
    fn a_calendar_part_deep_inside_the_alternative_is_still_its_part() {
        assert_eq!(twins(&invite(640, true)), [pair("0.1.0", "1")]);
    }

    #[test]
    fn only_a_marked_twin_leaves_the_list_and_a_draft_keeps_both() {
        let mut both = invite(640, false);
        assert_eq!(
            names(&both),
            [pair("invite.ics", "0.1"), pair("invite.ics", "1")],
            "unread, the bytes haven't said they're one"
        );
        let cal = part_mut(both.get_mut("payload").unwrap(), "0.1").unwrap();
        cal[TWIN] = json!(true);
        assert_eq!(names(&both), [pair("invite.ics", "1")]);
        assert_eq!(carried(&both).len(), 2);
    }

    #[test]
    fn two_files_with_one_name_and_size_stay_two() {
        let twice = json!({"payload": {"partId": "", "mimeType": "multipart/mixed", "parts": [
            {"partId": "0", "mimeType": "text/plain", "body": {"data": b64("Hi")}},
            {"partId": "1", "mimeType": "image/png", "filename": "scan.png", "headers": [{"name": "Content-Disposition", "value": "attachment"}], "body": {"attachmentId": "x", "size": 90}},
            {"partId": "2", "mimeType": "image/png", "filename": "scan.png", "body": {"attachmentId": "y", "size": 90}},
            {"partId": "3", "mimeType": "image/png", "filename": "scan.png", "body": {"attachmentId": "z"}},
        ]}});
        assert_eq!(
            names(&twice),
            [
                pair("scan.png", "1"),
                pair("scan.png", "2"),
                pair("scan.png", "3")
            ]
        );
        assert_eq!(twins(&twice), []);
    }

    #[test]
    fn a_picture_the_letter_shows_by_content_id_is_not_a_file_but_is_carried() {
        let shown = |disposition: Option<&str>| {
            let mut headers = vec![json!({"name": "Content-ID", "value": "<logo>"})];
            if let Some(d) = disposition {
                headers.push(json!({"name": "Content-Disposition", "value": d}));
            }
            json!({"payload": {"partId": "", "mimeType": "multipart/related", "parts": [
                {"partId": "0", "mimeType": "text/html", "body": {"data": b64("<img src=cid:logo>")}},
                {"partId": "1", "mimeType": "image/png", "filename": "logo.png", "headers": headers, "body": {"attachmentId": "x", "size": 90}},
            ]}})
        };
        for disposition in [Some("inline; filename=\"logo.png\""), None] {
            let mail = shown(disposition);
            assert!(attachments(&mail).is_empty(), "{disposition:?}");
            // a draft or a forward carries it on
            let carried: Vec<Value> = carried(&mail).iter().map(|a| a["part"].clone()).collect();
            assert_eq!(carried, [json!("1")], "{disposition:?}");
        }
        // sent as a file, it stays one
        assert_eq!(
            names(&shown(Some("attachment; filename=\"logo.png\""))),
            [pair("logo.png", "1")]
        );
    }

    #[test]
    fn a_part_with_a_content_id_is_a_file_unless_the_letter_shows_it() {
        let mail = |html: &str, mime: &str| {
            json!({"payload": {"partId": "", "mimeType": "multipart/related", "parts": [
                {"partId": "0", "mimeType": "text/html", "body": {"data": b64(html)}},
                {"partId": "1", "mimeType": mime, "filename": "receipt.pdf", "headers": [{"name": "Content-ID", "value": "<r1>"}, {"name": "Content-Disposition", "value": "inline"}], "body": {"attachmentId": "x", "size": 90}},
            ]}})
        };
        // a PDF the letter can't show, even named there
        assert_eq!(
            names(&mail("<a href=\"cid:r1\">", "application/pdf")),
            [pair("receipt.pdf", "1")]
        );
        // a picture the letter never shows
        assert_eq!(
            names(&mail("<p>Hi</p>", "image/png")),
            [pair("receipt.pdf", "1")]
        );
        // nor one it shows by another, longer id
        assert_eq!(
            names(&mail("<img src=\"cid:r10\">", "image/png")),
            [pair("receipt.pdf", "1")]
        );
        // shown, in any case, it is the letter's
        assert!(attachments(&mail("<IMG SRC=\"CID:R1\">", "image/png")).is_empty());
    }

    #[test]
    fn up_to_sixteen_files_are_compared_with_a_calendar_part() {
        let mut many = invite(640, false);
        for n in 2..30 {
            let file = json!({"partId": n.to_string(), "mimeType": "application/ics", "filename": "invite.ics", "body": {"attachmentId": format!("z{n}"), "size": 640}});
            many["payload"]["parts"].as_array_mut().unwrap().push(file);
        }
        assert_eq!(twins(&many).len(), 16);
    }

    #[test]
    fn the_body_is_what_the_html_part_shows_else_the_text_plain_part() {
        let plain = json!({"payload": {"mimeType": "multipart/mixed", "parts": [
            {"mimeType": "multipart/alternative", "parts": [
                {"mimeType": "text/plain", "headers": [{"name": "Content-Type", "value": "text/plain; charset=\"UTF-8\""}], "body": {"data": b64("Your 1701Q was received.\r\n\r\nRef: 2026-ABC\r\n\r\n")}},
                {"mimeType": "text/html", "body": {"data": b64("<p>html</p>")}},
            ]},
            {"mimeType": "text/plain", "filename": "notes.txt", "body": {"attachmentId": "a1"}},
        ]}});
        assert_eq!(body(&plain), "html", "the part the person sees");
        let html = json!({"payload": {"mimeType": "multipart/alternative", "parts": [
            {"mimeType": "text/html", "body": {"data": b64("<html><head><title>x</title><style>p{}</style></head><body><p>Hello&nbsp;<b>Ann</b>,</p><p>See   you<br>at noon &amp; bring &#8364;5 &#x1F600;</p><script>evil()</script><!-- c --></body></html>")}},
        ]}});
        assert_eq!(body(&html), "Hello Ann,\n\nSee you\nat noon & bring €5 😀");
        // a single-part message, padded standard base64 and Latin-1
        let latin = json!({"payload": {"mimeType": "text/plain", "headers": [{"name": "content-type", "value": "text/plain; charset=ISO-8859-1"}], "body": {"data": "Y2Fm6Q=="}}});
        assert_eq!(body(&latin), "café");
        let bad_utf8 = json!({"payload": {"mimeType": "text/plain", "body": {"data": "Y2Fm6Q"}}});
        assert_eq!(body(&bad_utf8), "caf\u{fffd}");
        assert_eq!(
            body(&json!({"payload": {"mimeType": "image/png", "body": {"data": "AAAA"}}})),
            ""
        );
        let attached = json!({"payload": {"mimeType": "multipart/mixed", "parts": [{"mimeType": "text/plain", "headers": [{"name": "Content-Disposition", "value": "attachment"}], "body": {"data": b64("no")}}]}});
        assert_eq!(body(&attached), "");
    }

    #[test]
    fn entities_decode_and_unknown_ones_stay() {
        assert_eq!(
            decode_entities("It&#39;s &quot;ok&quot; &lt;b&gt; &amp;amp; &copy; AT&T &#xZZ; &"),
            "It's \"ok\" <b> &amp; &copy; AT&T &#xZZ; &"
        );
    }

    #[test]
    fn html_keeps_one_blank_line_and_drops_skipped_blocks() {
        assert_eq!(
            clean::visible_text("<div>a</div><p></p><p></p><div>b</div>"),
            "a\n\nb"
        );
        assert_eq!(
            clean::visible_text("<table><tr><td>x</td><td>y</td></tr></table>"),
            "x y"
        );
        assert_eq!(clean::visible_text("plain < text"), "plain < text");
        assert_eq!(
            clean::visible_text("<p>if a < b and c > d</p>"),
            "if a < b and c > d"
        );
        assert_eq!(clean::visible_text("one\ntwo<br>\nthree"), "one two\nthree");
        assert_eq!(clean::visible_text("<SCRIPT>a</SCRIPT>ok<style"), "ok");
    }

    #[test]
    fn snippets_are_one_line_and_capped() {
        assert_eq!(
            snippet_of("  See you \u{a0} at\tnoon.\r\n\n"),
            "See you at noon."
        );
        let long = format!("{}abcd😀tail", "word ".repeat(39));
        assert_eq!(snippet_of(&long), format!("{}abcd…", "word ".repeat(39)));
        assert_eq!(js::len_utf16(&snippet_of(&"x".repeat(500))), 200);
        assert_eq!(
            snippet(&json!({"snippet": "Don&#39;t  forget"})),
            "Don't forget"
        );
        assert_eq!(snippet(&json!({})), "");
    }

    #[test]
    fn bodies_are_capped_at_64_kib_on_a_character_boundary() {
        let big = cap_bytes(&"é".repeat(70_000), BODY_MAX);
        assert!(big.len() <= BODY_MAX && big.ends_with("…[truncated]"));
        assert_eq!(big.len(), 65_522 + "…[truncated]".len());
        let three = cap_bytes(&format!("a{}", "€".repeat(30_000)), BODY_MAX);
        assert_eq!(three.len(), 1 + 3 * 21_840 + "…[truncated]".len());
        let fffd = cap_bytes(
            &format!("{}\u{fffd}{}", "a".repeat(65_519), "b".repeat(100)),
            BODY_MAX,
        );
        assert_eq!(fffd, format!("{}…[truncated]", "a".repeat(65_519)));
        assert_eq!(cap_bytes("short", BODY_MAX), "short");
    }

    #[test]
    fn message_ids_are_path_safe() {
        assert!(id_ok("18f1a2b3c4d5e6f7") && id_ok("a-b_c"));
        for bad in ["", "../x", "a/b", "a?b", "a b", "a%2F", &"x".repeat(65)] {
            assert!(!id_ok(bad), "{bad}");
        }
    }
}
