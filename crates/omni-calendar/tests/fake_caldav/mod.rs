//! A small in-process CalDAV server (axum) that behaves like iCloud for the
//! primary-calendar tests: RFC 6764 discovery, sync-collection with tokens,
//! 507 truncation and invalid-token errors, calendar-multiget, and
//! conditional PUT/DELETE with 412 and `no-uid-conflict` 403 responses.
//! The client reaches it through iCloud URLs rewritten to its address.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use omni_http::{HttpClient, HttpOverrides, Url};

pub const OWNER: &str = "michael@thiesen.dev";
pub const HOME: &str = "/123/calendars/";
pub const PRIMARY: &str = "/123/calendars/work/";
pub const OTHER: &str = "/123/calendars/home/";

#[derive(Clone, Debug)]
pub struct Collection {
    pub path: String,
    pub name: String,
    pub components: Vec<&'static str>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seen {
    pub method: String,
    pub path: String,
    pub if_match: Option<String>,
    pub if_none_match: Option<String>,
    pub body: String,
}

#[derive(Debug)]
pub struct State {
    pub collections: Vec<Collection>,
    pub default_calendar: Option<String>,
    /// Resources by full path: (etag, body).
    pub resources: BTreeMap<String, (String, String)>,
    /// (sequence, path) of every change, for sync-collection.
    pub log: Vec<(u64, String)>,
    pub seq: u64,
    pub etag_seq: u64,
    pub requests: Vec<Seen>,
    /// Rejects every sync token with `valid-sync-token`.
    pub reject_tokens: bool,
    /// Max changed hrefs per sync-collection response (507 beyond).
    pub page_size: Option<usize>,
    /// Paths changed by "someone else" just before the next PUT to them.
    pub race_next_put: BTreeSet<String>,
    /// UIDs that live in another calendar (PUT answers no-uid-conflict).
    pub foreign_uids: BTreeSet<String>,
    /// Status to answer the next write with instead of applying it.
    pub fail_next_write: Option<u16>,
    /// Status to answer the next write with after applying it (a lost
    /// acknowledgement).
    pub lose_next_write: Option<u16>,
    /// Collections whose REPORTs answer 500.
    pub broken_reports: BTreeSet<String>,
    pub supports_sync: bool,
    /// Holds the response to the next PUT or DELETE, after applying it,
    /// until the test releases it.
    pub hold_next_write: Option<WriteGate>,
}

/// Signals that a held write was applied; the response waits for `release`.
#[derive(Clone, Default)]
pub struct WriteGate {
    pub applied: Arc<tokio::sync::Notify>,
    pub release: Arc<tokio::sync::Notify>,
}

impl std::fmt::Debug for WriteGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WriteGate")
    }
}

#[derive(Clone)]
pub struct FakeCaldav {
    pub state: Arc<Mutex<State>>,
    pub base: Url,
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn multistatus(inner: &str) -> Response {
    (
        StatusCode::MULTI_STATUS,
        [("Content-Type", "application/xml; charset=utf-8")],
        format!(
            r#"<?xml version="1.0" encoding="utf-8"?><d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:cs="http://calendarserver.org/ns/">{inner}</d:multistatus>"#
        ),
    )
        .into_response()
}

fn prop_response(href: &str, props: &str) -> String {
    format!(
        "<d:response><d:href>{href}</d:href><d:propstat><d:prop>{props}</d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>"
    )
}

fn between<'a>(text: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = text.find(open)? + open.len();
    let end = text[start..].find(close)? + start;
    Some(&text[start..end])
}

fn uid_of(body: &str) -> Option<String> {
    body.lines()
        .find_map(|l| l.trim_end_matches('\r').strip_prefix("UID:"))
        .map(str::to_owned)
}

impl State {
    fn bump(&mut self, path: &str) {
        self.seq += 1;
        let seq = self.seq;
        self.log.push((seq, path.to_owned()));
    }

    fn new_etag(&mut self) -> String {
        self.etag_seq += 1;
        format!("\"etag-{}\"", self.etag_seq)
    }

    fn in_primary(path: &str) -> bool {
        path.starts_with(PRIMARY) && path.len() > PRIMARY.len()
    }

    fn in_event_calendar(&self, path: &str) -> bool {
        self.collections.iter().any(|c| {
            c.components.contains(&"VEVENT")
                && path.starts_with(&c.path)
                && path.len() > c.path.len()
        })
    }
}

impl FakeCaldav {
    pub async fn start() -> Self {
        let state = Arc::new(Mutex::new(State {
            collections: vec![
                Collection {
                    path: PRIMARY.to_owned(),
                    name: "iCloud".to_owned(),
                    components: vec!["VEVENT"],
                },
                Collection {
                    path: OTHER.to_owned(),
                    name: "Home".to_owned(),
                    components: vec!["VEVENT"],
                },
                Collection {
                    path: "/123/calendars/tasks/".to_owned(),
                    name: "iCloud".to_owned(),
                    components: vec!["VTODO"],
                },
            ],
            default_calendar: Some(PRIMARY.to_owned()),
            resources: BTreeMap::new(),
            log: Vec::new(),
            seq: 0,
            etag_seq: 0,
            requests: Vec::new(),
            reject_tokens: false,
            page_size: None,
            race_next_put: BTreeSet::new(),
            foreign_uids: BTreeSet::new(),
            fail_next_write: None,
            lose_next_write: None,
            broken_reports: BTreeSet::new(),
            supports_sync: true,
            hold_next_write: None,
        }));
        let app_state = state.clone();
        let app = Router::new().fallback(
            move |method: Method, uri: Uri, headers: HeaderMap, body: Bytes| {
                let state = app_state.clone();
                async move {
                    let gate = if matches!(method.as_str(), "PUT" | "DELETE") {
                        state.lock().unwrap().hold_next_write.take()
                    } else {
                        None
                    };
                    let response = handle(&state, &method, &uri, &headers, &body);
                    if let Some(gate) = gate {
                        gate.applied.notify_one();
                        gate.release.notified().await;
                    }
                    response
                }
            },
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            state,
            base: Url::parse(&format!("http://{addr}")).unwrap(),
        }
    }

    /// A client whose iCloud origins reach this server.
    pub fn http(&self) -> HttpClient {
        let rewrites = ["https://caldav.icloud.com", "https://p42-caldav.icloud.com"]
            .iter()
            .map(|o| (Url::parse(o).unwrap(), self.base.clone()))
            .collect();
        omni_testkit::no_network().with_overrides(HttpOverrides { rewrites })
    }

    pub fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    /// Writes a resource as another client would (new etag, logged change).
    pub fn put(&self, name: &str, body: &str) -> String {
        let mut s = self.lock();
        let path = format!("{PRIMARY}{name}");
        let etag = s.new_etag();
        s.resources
            .insert(path.clone(), (etag.clone(), body.to_owned()));
        s.bump(&path);
        etag
    }

    pub fn remove(&self, name: &str) {
        let mut s = self.lock();
        let path = format!("{PRIMARY}{name}");
        s.resources.remove(&path);
        s.bump(&path);
    }

    pub fn body(&self, name: &str) -> Option<String> {
        self.lock()
            .resources
            .get(&format!("{PRIMARY}{name}"))
            .map(|(_, b)| b.clone())
    }

    pub fn etag(&self, name: &str) -> Option<String> {
        self.lock()
            .resources
            .get(&format!("{PRIMARY}{name}"))
            .map(|(e, _)| e.clone())
    }

    pub fn names(&self) -> Vec<String> {
        self.lock()
            .resources
            .keys()
            .filter_map(|p| p.strip_prefix(PRIMARY).map(str::to_owned))
            .collect()
    }

    pub fn writes(&self) -> Vec<Seen> {
        self.lock()
            .requests
            .iter()
            .filter(|r| r.method == "PUT" || r.method == "DELETE")
            .cloned()
            .collect()
    }

    pub fn clear_requests(&self) {
        self.lock().requests.clear();
    }
}

fn handle(
    state: &Arc<Mutex<State>>,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    body: &Bytes,
) -> Response {
    let mut s = state.lock().unwrap();
    let path = uri.path().to_owned();
    let body = String::from_utf8_lossy(body).into_owned();
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    s.requests.push(Seen {
        method: method.as_str().to_owned(),
        path: path.clone(),
        if_match: header("if-match"),
        if_none_match: header("if-none-match"),
        body: body.clone(),
    });
    if header("authorization").as_deref() != Some("Basic dXNlckBpY2xvdWQuY29tOmFwcC1wYXNzd29yZA==")
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match method.as_str() {
        "PROPFIND" => propfind(&mut s, &path, header("depth").as_deref()),
        "REPORT" => report(&mut s, &path, &body),
        "GET" => match s.resources.get(&path) {
            Some((etag, text)) => (
                StatusCode::OK,
                [("ETag", etag.as_str()), ("Content-Type", "text/calendar")],
                text.clone(),
            )
                .into_response(),
            None => StatusCode::NOT_FOUND.into_response(),
        },
        "PUT" => {
            let lost = s.lose_next_write.take();
            let response = put(
                &mut s,
                &path,
                &body,
                header("if-match"),
                header("if-none-match"),
            );
            lose(response, lost)
        }
        "DELETE" => {
            let lost = s.lose_next_write.take();
            let response = delete(&mut s, &path, header("if-match"));
            lose(response, lost)
        }
        _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
    }
}

/// An applied write whose acknowledgement is replaced by `lost`.
fn lose(response: Response, lost: Option<u16>) -> Response {
    match lost {
        Some(status) if response.status().is_success() => {
            StatusCode::from_u16(status).unwrap().into_response()
        }
        _ => response,
    }
}

fn propfind(s: &mut State, path: &str, depth: Option<&str>) -> Response {
    match path {
        "/" => multistatus(&prop_response(
            "/",
            "<d:current-user-principal><d:href>/123/principal/</d:href></d:current-user-principal>",
        )),
        "/123/principal/" => multistatus(&prop_response(
            "/123/principal/",
            &format!(
                "<c:calendar-home-set><d:href>{HOME}</d:href></c:calendar-home-set><c:calendar-user-address-set><d:href>mailto:{OWNER}</d:href><d:href>/123/principal/</d:href></c:calendar-user-address-set><c:schedule-inbox-URL><d:href>/123/inbox/</d:href></c:schedule-inbox-URL>"
            ),
        )),
        "/123/inbox/" => match &s.default_calendar {
            Some(default) => multistatus(&prop_response(
                "/123/inbox/",
                &format!(
                    "<c:schedule-default-calendar-URL><d:href>{default}</d:href></c:schedule-default-calendar-URL>"
                ),
            )),
            None => multistatus(
                "<d:response><d:href>/123/inbox/</d:href><d:propstat><d:prop><c:schedule-default-calendar-URL/></d:prop><d:status>HTTP/1.1 404 Not Found</d:status></d:propstat></d:response>",
            ),
        },
        HOME => {
            let mut out = prop_response(HOME, "<d:resourcetype><d:collection/></d:resourcetype>");
            for c in &s.collections {
                let comps: String = c
                    .components
                    .iter()
                    .map(|n| format!(r#"<c:comp name="{n}"/>"#))
                    .collect();
                let sync = if s.supports_sync {
                    "<d:supported-report><d:report><d:sync-collection/></d:report></d:supported-report>"
                } else {
                    ""
                };
                out.push_str(&prop_response(
                    &c.path,
                    &format!(
                        "<d:displayname>{}</d:displayname><d:resourcetype><d:collection/><c:calendar/></d:resourcetype><c:supported-calendar-component-set>{comps}</c:supported-calendar-component-set><d:supported-report-set>{sync}<d:supported-report><d:report><c:calendar-multiget/></d:report></d:supported-report></d:supported-report-set><d:current-user-privilege-set><d:privilege><d:read/></d:privilege><d:privilege><d:write-content/></d:privilege></d:current-user-privilege-set>",
                        xml_escape(&c.name)
                    ),
                ));
            }
            multistatus(&out)
        }
        p if p == PRIMARY && depth == Some("1") => {
            let mut out = prop_response(
                PRIMARY,
                "<d:resourcetype><d:collection/><c:calendar/></d:resourcetype>",
            );
            for (path, (etag, _)) in &s.resources {
                if State::in_primary(path) {
                    out.push_str(&prop_response(
                        path,
                        &format!("<d:getetag>{}</d:getetag>", xml_escape(etag)),
                    ));
                }
            }
            multistatus(&out)
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

fn report(s: &mut State, path: &str, body: &str) -> Response {
    if s.broken_reports.contains(path) {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    if body.contains("calendar-query") {
        if !s.collections.iter().any(|c| c.path == path) {
            return StatusCode::NOT_FOUND.into_response();
        }
        // Like iCloud: only a time-range filter is accepted.
        if !body.contains("<c:time-range") || body.contains("prop-filter") {
            return StatusCode::PRECONDITION_FAILED.into_response();
        }
        let bound = |name: &str| {
            between(body, &format!(r#"{name}=""#), "T000000Z")
                .unwrap_or_default()
                .to_owned()
        };
        let (start, end) = (bound("start"), bound("end"));
        let starts_in_range = |text: &str| {
            text.lines()
                .filter(|l| l.starts_with("DTSTART"))
                .filter_map(|l| l.rsplit(':').next())
                .any(|v| v.len() >= 8 && v[..8] >= *start && v[..8] < *end)
        };
        let mut out = String::new();
        for (resource, (etag, text)) in &s.resources {
            if resource.starts_with(path) && starts_in_range(text) {
                out.push_str(&prop_response(
                    resource,
                    &format!(
                        "<d:getetag>{}</d:getetag><c:calendar-data>{}</c:calendar-data>",
                        xml_escape(etag),
                        xml_escape(text)
                    ),
                ));
            }
        }
        return multistatus(&out);
    }
    if path != PRIMARY {
        return StatusCode::NOT_FOUND.into_response();
    }
    if body.contains("sync-collection") {
        let token = between(body, "<d:sync-token>", "</d:sync-token>").unwrap_or_default();
        let since = if token.is_empty() {
            None
        } else {
            match token
                .strip_prefix("tok-")
                .and_then(|n| n.parse::<u64>().ok())
            {
                Some(n) if !s.reject_tokens && n <= s.seq => Some(n),
                _ => {
                    return (
                        StatusCode::FORBIDDEN,
                        r#"<?xml version="1.0"?><d:error xmlns:d="DAV:"><d:valid-sync-token/></d:error>"#,
                    )
                        .into_response();
                }
            }
        };
        // Changed paths after `since`, latest change per path, in log order.
        let mut changed: Vec<(u64, String)> = Vec::new();
        match since {
            None => {
                for path in s.resources.keys().filter(|p| State::in_primary(p)) {
                    let last = s
                        .log
                        .iter()
                        .rev()
                        .find(|(_, p)| p == path)
                        .map_or(0, |(q, _)| *q);
                    changed.push((last, path.clone()));
                }
                changed.sort();
            }
            Some(since) => {
                let mut seen = BTreeSet::new();
                for (seq, path) in s.log.iter().rev() {
                    if *seq > since && State::in_primary(path) && seen.insert(path.clone()) {
                        changed.push((*seq, path.clone()));
                    }
                }
                changed.sort();
            }
        }
        let (page, truncated) = match s.page_size {
            Some(n) if changed.len() > n => (changed[..n].to_vec(), true),
            _ => (changed, false),
        };
        let next = if truncated {
            page.last().map_or(s.seq, |(q, _)| *q)
        } else {
            s.seq
        };
        let mut out = String::new();
        for (_, path) in &page {
            match s.resources.get(path) {
                Some((etag, _)) => out.push_str(&prop_response(path, &format!("<d:getetag>{}</d:getetag>", xml_escape(etag)))),
                None => out.push_str(&format!(
                    "<d:response><d:href>{path}</d:href><d:status>HTTP/1.1 404 Not Found</d:status></d:response>"
                )),
            }
        }
        if truncated {
            out.push_str(&format!(
                "<d:response><d:href>{PRIMARY}</d:href><d:status>HTTP/1.1 507 Insufficient Storage</d:status></d:response>"
            ));
        }
        out.push_str(&format!("<d:sync-token>tok-{next}</d:sync-token>"));
        return multistatus(&out);
    }
    if body.contains("calendar-multiget") {
        let mut out = String::new();
        let mut rest = body;
        while let Some(href) = between(rest, "<d:href>", "</d:href>") {
            let end = rest.find("</d:href>").unwrap() + "</d:href>".len();
            rest = &rest[end..];
            match s.resources.get(href) {
                Some((etag, text)) => out.push_str(&prop_response(
                    href,
                    &format!(
                        "<d:getetag>{}</d:getetag><c:calendar-data>{}</c:calendar-data>",
                        xml_escape(etag),
                        xml_escape(text)
                    ),
                )),
                None => out.push_str(&format!(
                    "<d:response><d:href>{href}</d:href><d:status>HTTP/1.1 404 Not Found</d:status></d:response>"
                )),
            }
        }
        return multistatus(&out);
    }
    StatusCode::BAD_REQUEST.into_response()
}

fn put(
    s: &mut State,
    path: &str,
    body: &str,
    if_match: Option<String>,
    if_none_match: Option<String>,
) -> Response {
    if !s.in_event_calendar(path) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if let Some(status) = s.fail_next_write.take() {
        return StatusCode::from_u16(status).unwrap().into_response();
    }
    if s.race_next_put.remove(path)
        && let Some((_, text)) = s.resources.get(path).cloned()
    {
        let etag = s.new_etag();
        s.resources.insert(
            path.to_owned(),
            (etag, text.replace("SUMMARY:", "SUMMARY:Edited ")),
        );
        s.bump(path);
    }
    let existing = s.resources.get(path).cloned();
    if if_none_match.as_deref() == Some("*") && existing.is_some() {
        return StatusCode::PRECONDITION_FAILED.into_response();
    }
    if let Some(expected) = &if_match {
        match &existing {
            Some((etag, _)) if etag == expected => {}
            _ => return StatusCode::PRECONDITION_FAILED.into_response(),
        }
    }
    if existing.is_none()
        && let Some(uid) = uid_of(body)
        && s.foreign_uids.contains(&uid)
    {
        return (
            StatusCode::FORBIDDEN,
            format!(
                r#"<?xml version="1.0"?><d:error xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><c:no-uid-conflict><d:href>{OTHER}{uid}.ics</d:href></c:no-uid-conflict></d:error>"#
            ),
        )
            .into_response();
    }
    // Like iCloud, store a normalized copy (CRLF kept, a vendor property added).
    let stored = body.replacen(
        "END:VEVENT",
        "X-APPLE-TRAVEL-ADVISORY-BEHAVIOR:AUTOMATIC\r\nEND:VEVENT",
        1,
    );
    let etag = s.new_etag();
    s.resources.insert(path.to_owned(), (etag.clone(), stored));
    s.bump(path);
    let status = if existing.is_some() {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::CREATED
    };
    (status, [("ETag", etag.as_str())]).into_response()
}

fn delete(s: &mut State, path: &str, if_match: Option<String>) -> Response {
    if let Some(status) = s.fail_next_write.take() {
        return StatusCode::from_u16(status).unwrap().into_response();
    }
    let Some((etag, _)) = s.resources.get(path).cloned() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if if_match.as_deref().is_some_and(|m| m != etag) {
        return StatusCode::PRECONDITION_FAILED.into_response();
    }
    s.resources.remove(path);
    s.bump(path);
    StatusCode::NO_CONTENT.into_response()
}
