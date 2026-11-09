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

/// What an attachment whose bytes won't decode fails with: a try again
/// brings the same.
pub const DAMAGED: &str = "That attachment came back damaged.";

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
            .map_err(|_| Failure::Plain(DAMAGED.to_owned()))?;
        Ok(Some(bytes))
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

/// Attachments: `{name, mime, size, id, part}`. `part` is what stays the same
/// from read to read, as Google calls a part's id immutable and promises nothing
/// of `attachmentId`. A mail that's one attachment and nothing else has the empty
/// id, and an empty one can't be told from none, so that's `root`.
pub fn attachments(message: &Value) -> Vec<Value> {
    fn walk(part: &Value, out: &mut Vec<Value>) {
        let name = part
            .get("filename")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !name.is_empty() {
            let id = match part.get("partId").and_then(Value::as_str) {
                Some("") => Some("root"),
                id => id,
            };
            out.push(serde_json::json!({"name": name, "mime": part.get("mimeType"), "size": part.pointer("/body/size"), "id": part.pointer("/body/attachmentId"), "part": id}));
        }
        for p in part
            .get("parts")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            walk(p, out);
        }
    }
    let mut out = Vec::new();
    if let Some(p) = message.get("payload") {
        walk(p, &mut out);
    }
    out
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

/// A part's `body.data` (base64url, padded or not, or standard base64) as
/// bytes; none when it isn't base64.
pub(crate) fn part_bytes(part: &Value) -> Vec<u8> {
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
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(clean)
        .unwrap_or_default()
}

/// A part's charset, from its Content-Type, lower case; empty when it names none.
pub(crate) fn part_charset(part: &Value) -> String {
    let ctype = part_header(part, "content-type")
        .unwrap_or_default()
        .to_ascii_lowercase();
    ctype
        .split(';')
        .find_map(|p| p.trim().strip_prefix("charset="))
        .map(|c| c.trim().trim_matches('"').to_owned())
        .unwrap_or_default()
}

/// What windows-1252 puts at 0x80 to 0x9F, where Latin-1 has controls.
const CP1252: [char; 32] = [
    '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8d}', 'Ž', '\u{8f}',
    '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9d}', 'ž', 'Ÿ',
];

/// `bytes` as text in `charset`: Latin-1 or windows-1252 when it says so,
/// else UTF-8 with invalid bytes replaced.
pub(crate) fn in_charset(bytes: &[u8], charset: &str) -> String {
    match charset {
        "iso-8859-1" | "iso8859-1" | "latin1" | "latin-1" => {
            bytes.iter().map(|&b| char::from(b)).collect()
        }
        "windows-1252" | "cp1252" => bytes
            .iter()
            .map(|&b| match b {
                0x80..=0x9f => CP1252[usize::from(b - 0x80)],
                _ => char::from(b),
            })
            .collect(),
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// A part's `body.data` as text in its charset.
fn decode_part(part: &Value) -> String {
    in_charset(&part_bytes(part), &part_charset(part))
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
    #[test]
    fn a_windows_1252_body_keeps_its_curly_quotes() {
        let body =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"\x93Noon?\x94 \x96 Ana\x92s");
        let message = serde_json::json!({"payload": {"mimeType": "text/plain", "headers": [{"name": "Content-Type", "value": "text/plain; charset=windows-1252"}], "body": {"data": body}}});
        assert_eq!(
            super::body(&message),
            "\u{201c}Noon?\u{201d} \u{2013} Ana\u{2019}s"
        );
    }

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
