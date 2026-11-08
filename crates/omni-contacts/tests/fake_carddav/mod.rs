//! A small in-process CardDAV server (axum) that behaves like iCloud for the
//! contacts tests: RFC 6764 discovery through a shard redirect, two address
//! books, CTag and ETag listings, addressbook-multiget, and conditional
//! GET/PUT/DELETE with 412 answers. Faults are injected per test. All data is
//! synthetic. The client reaches it through iCloud URLs rewritten to it.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use omni_http::{HttpClient, HttpOverrides, Url};

pub const HOME: &str = "/123/carddavhome/";
pub const CARD: &str = "/123/carddavhome/card/";
pub const WORK: &str = "/123/carddavhome/work/";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seen {
    pub method: String,
    pub path: String,
    pub if_match: Option<String>,
    pub if_none_match: Option<String>,
}

#[derive(Debug, Default)]
pub struct State {
    /// Resources by path: (etag, body bytes).
    pub resources: BTreeMap<String, (String, Vec<u8>)>,
    pub etag_seq: u64,
    pub ctag_seq: u64,
    pub requests: Vec<Seen>,
    pub redirected: bool,
    /// Answer the next PUT/DELETE with this status without applying it.
    pub fail_next_write: Option<u16>,
    /// Apply the next PUT/DELETE, then answer with this status.
    pub apply_then_answer: Option<u16>,
    /// Lines of this property are dropped from every stored PUT body.
    pub drop_on_put: Option<String>,
    /// Where the discovery root redirects (default the p42 shard).
    pub root_redirect: Option<String>,
    /// An href listed for the main address book instead of its own.
    pub book_href_override: Option<String>,
    /// Address book PROPFINDs answer 500.
    pub fail_book_propfind: bool,
    /// New versions get weak ETags (in listings, GET and PUT answers).
    pub weak_etags: bool,
    /// When a PUT body is stored changed, answer with a weak ETag instead of
    /// none. Either is what RFC 6352 6.3.2.3 allows; a strong one is not.
    pub weak_etag_on_rewrite: bool,
    /// Paths another client changes just before our next PUT to them.
    pub race_next_put: BTreeSet<String>,
    /// Paths a multiget leaves out of its answer.
    pub omit_from_multiget: BTreeSet<String>,
    /// Principal href override (to test untrusted redirects).
    pub principal_href: Option<String>,
    /// Holds the answer to the next PUT/DELETE (after applying it) until
    /// the test releases it.
    pub hold_next_write: Option<WriteGate>,
    /// After a PUT, this many GETs of that card still return the previous
    /// version (a lagging replica).
    pub stale_reads_after_put: u32,
    /// Right after the next PUT, another client removes the lines starting
    /// with this text (a later version with a new ETag).
    pub edit_after_next_put: Option<String>,
    stale: Option<(String, (String, Vec<u8>), u32)>,
}

/// Signals that a held write was applied; its answer waits for `release`.
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
pub struct FakeCarddav {
    pub state: Arc<Mutex<State>>,
    pub base: Url,
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('\r', "&#13;")
}

fn multistatus(inner: &str) -> Response {
    (
        StatusCode::MULTI_STATUS,
        [("Content-Type", "application/xml; charset=utf-8")],
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?><multistatus xmlns="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav" xmlns:cs="http://calendarserver.org/ns/">{inner}</multistatus>"#
        ),
    )
        .into_response()
}

fn response(href: &str, props: &str) -> String {
    format!(
        "<response><href>{href}</href><propstat><prop>{props}</prop><status>HTTP/1.1 200 OK</status></propstat></response>"
    )
}

impl State {
    fn new_etag(&mut self) -> String {
        self.etag_seq += 1;
        let etag = format!("\"etag-{}\"", self.etag_seq);
        if self.weak_etags {
            format!("W/{etag}")
        } else {
            etag
        }
    }

    fn bump(&mut self) {
        self.ctag_seq += 1;
    }
}

fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

fn handle(
    state: &Mutex<State>,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    body: &Bytes,
) -> Response {
    let mut s = state.lock().unwrap();
    let path = uri.path().to_owned();
    s.requests.push(Seen {
        method: method.to_string(),
        path: path.clone(),
        if_match: header(headers, "if-match"),
        if_none_match: header(headers, "if-none-match"),
    });
    let depth = header(headers, "depth").unwrap_or_default();
    let text = String::from_utf8_lossy(body).into_owned();
    match method.as_str() {
        "PROPFIND" if path == "/" => {
            if !s.redirected {
                s.redirected = true;
                let location = s
                    .root_redirect
                    .clone()
                    .unwrap_or_else(|| "https://p42-contacts.icloud.com/".to_owned());
                return (StatusCode::MOVED_PERMANENTLY, [("Location", location)]).into_response();
            }
            let href = s
                .principal_href
                .clone()
                .unwrap_or_else(|| "/123/principal/".to_owned());
            multistatus(&response(
                "/",
                &format!("<current-user-principal><href>{href}</href></current-user-principal>"),
            ))
        }
        "PROPFIND" if path == "/123/principal/" => multistatus(&response(
            "/123/principal/",
            &format!("<card:addressbook-home-set><href>{HOME}</href></card:addressbook-home-set>"),
        )),
        "PROPFIND" if path == HOME => multistatus(&format!(
            "{}{}{}{}",
            response(HOME, "<resourcetype><collection/></resourcetype>"),
            response(
                s.book_href_override.as_deref().unwrap_or(CARD),
                "<displayname>Contacts</displayname><resourcetype><collection/><card:addressbook/></resourcetype>"
            ),
            response(
                WORK,
                "<displayname>Work</displayname><resourcetype><collection/><card:addressbook/></resourcetype>"
            ),
            response(
                "/123/carddavhome/inbox/",
                "<resourcetype><collection/></resourcetype>"
            ),
        )),
        "PROPFIND" if (path == CARD || path == WORK) && s.fail_book_propfind => {
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
        "PROPFIND" if path == CARD || path == WORK => {
            if depth == "0" {
                let ctag = format!("<cs:getctag>ctag-{}</cs:getctag>", s.ctag_seq);
                return multistatus(&response(&path, &ctag));
            }
            let mut inner = response(
                &path,
                "<resourcetype><collection/><card:addressbook/></resourcetype>",
            );
            for (p, (etag, _)) in s.resources.iter().filter(|(p, _)| p.starts_with(&path)) {
                inner.push_str(&response(
                    p,
                    &format!("<getetag>{etag}</getetag><resourcetype/>"),
                ));
            }
            multistatus(&inner)
        }
        "REPORT" => {
            // Card bytes go out as stored, like a real server: a card that is
            // not UTF-8 makes the whole answer not UTF-8.
            let mut inner: Vec<u8> = Vec::new();
            for chunk in text.split("<d:href>").skip(1) {
                let href = chunk.split("</d:href>").next().unwrap_or_default();
                if s.omit_from_multiget.contains(href) {
                    continue;
                }
                match s.resources.get(href) {
                    Some((etag, data)) => {
                        inner.extend_from_slice(
                            format!(
                                "<response><href>{href}</href><propstat><prop><getetag>{etag}</getetag><card:address-data>"
                            )
                            .as_bytes(),
                        );
                        for byte in data {
                            match byte {
                                b'&' => inner.extend_from_slice(b"&amp;"),
                                b'<' => inner.extend_from_slice(b"&lt;"),
                                b'>' => inner.extend_from_slice(b"&gt;"),
                                b'\r' => inner.extend_from_slice(b"&#13;"),
                                other => inner.push(*other),
                            }
                        }
                        inner.extend_from_slice(
                            b"</card:address-data></prop><status>HTTP/1.1 200 OK</status></propstat></response>",
                        );
                    }
                    None => inner.extend_from_slice(
                        format!(
                            "<response><href>{href}</href><status>HTTP/1.1 404 Not Found</status></response>"
                        )
                        .as_bytes(),
                    ),
                }
            }
            let mut body = br#"<?xml version="1.0" encoding="UTF-8"?><multistatus xmlns="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav">"#.to_vec();
            body.extend_from_slice(&inner);
            body.extend_from_slice(b"</multistatus>");
            (
                StatusCode::MULTI_STATUS,
                [("Content-Type", "application/xml; charset=utf-8")],
                body,
            )
                .into_response()
        }
        "GET"
            if s.stale
                .as_ref()
                .is_some_and(|(p, _, n)| *p == path && *n > 0) =>
        {
            let Some((_, (etag, data), left)) = s.stale.as_mut() else {
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            };
            *left -= 1;
            (StatusCode::OK, [("ETag", etag.clone())], data.clone()).into_response()
        }
        "GET" => match s.resources.get(&path) {
            Some((etag, data)) => (
                StatusCode::OK,
                [
                    ("ETag", etag.clone()),
                    ("Content-Type", "text/vcard".to_owned()),
                ],
                data.clone(),
            )
                .into_response(),
            None => StatusCode::NOT_FOUND.into_response(),
        },
        "PUT" | "DELETE" => {
            if s.race_next_put.remove(&path)
                && let Some((_, data)) = s.resources.get(&path).cloned()
            {
                let etag = s.new_etag();
                s.resources.insert(path.clone(), (etag, data));
                s.bump();
            }
            if let Some(status) = s.fail_next_write.take() {
                return StatusCode::from_u16(status).unwrap().into_response();
            }
            let current = s.resources.get(&path).map(|(e, _)| e.clone());
            if let Some(expected) = header(headers, "if-match")
                && current.as_deref() != Some(expected.as_str())
            {
                return StatusCode::PRECONDITION_FAILED.into_response();
            }
            if header(headers, "if-none-match").as_deref() == Some("*") && current.is_some() {
                return StatusCode::PRECONDITION_FAILED.into_response();
            }
            let answer = s.apply_then_answer.take();
            let response = if method.as_str() == "DELETE" {
                if current.is_none() {
                    return StatusCode::NOT_FOUND.into_response();
                }
                s.resources.remove(&path);
                s.bump();
                StatusCode::NO_CONTENT.into_response()
            } else {
                let mut stored = text.clone();
                if let Some(drop) = &s.drop_on_put {
                    stored = stored
                        .split_inclusive("\r\n")
                        .filter(|line| !line.starts_with(drop.as_str()))
                        .collect();
                }
                let etag = s.new_etag();
                let created = current.is_none();
                if s.stale_reads_after_put > 0
                    && let Some(previous) = s.resources.get(&path).cloned()
                {
                    let n = std::mem::take(&mut s.stale_reads_after_put);
                    s.stale = Some((path.clone(), previous, n));
                }
                s.resources
                    .insert(path.clone(), (etag.clone(), stored.clone().into_bytes()));
                s.bump();
                if let Some(prefix) = s.edit_after_next_put.take() {
                    let edited: String = stored
                        .split_inclusive("\r\n")
                        .filter(|line| !line.starts_with(prefix.as_str()))
                        .collect();
                    let later = s.new_etag();
                    s.resources
                        .insert(path.clone(), (later, edited.into_bytes()));
                    s.bump();
                }
                let status = if created {
                    StatusCode::CREATED
                } else {
                    StatusCode::NO_CONTENT
                };
                if stored == text {
                    (status, [("ETag", etag)]).into_response()
                } else if s.weak_etag_on_rewrite {
                    (status, [("ETag", format!("W/{etag}"))]).into_response()
                } else {
                    status.into_response()
                }
            };
            match answer {
                Some(status) => StatusCode::from_u16(status).unwrap().into_response(),
                None => response,
            }
        }
        _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
    }
}

impl FakeCarddav {
    pub async fn start() -> Self {
        let state = Arc::new(Mutex::new(State::default()));
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

    /// A client whose iCloud contacts origins reach this server.
    pub fn http(&self) -> HttpClient {
        let rewrites = [
            "https://contacts.icloud.com",
            "https://p42-contacts.icloud.com",
        ]
        .iter()
        .map(|o| (Url::parse(o).unwrap(), self.base.clone()))
        .collect();
        omni_testkit::no_network().with_overrides(HttpOverrides { rewrites })
    }

    pub fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    /// Stores a card as another client would; returns its etag.
    pub fn put(&self, path: &str, body: &str) -> String {
        self.put_bytes(path, body.as_bytes())
    }

    pub fn put_bytes(&self, path: &str, body: &[u8]) -> String {
        let mut s = self.lock();
        let etag = s.new_etag();
        s.resources
            .insert(path.to_owned(), (etag.clone(), body.to_vec()));
        s.bump();
        etag
    }

    pub fn body(&self, path: &str) -> Option<String> {
        self.lock()
            .resources
            .get(path)
            .map(|(_, b)| String::from_utf8_lossy(b).into_owned())
    }

    pub fn etag(&self, path: &str) -> Option<String> {
        self.lock().resources.get(path).map(|(e, _)| e.clone())
    }

    /// Writes (PUT/DELETE) received, as (method, path).
    pub fn writes(&self) -> Vec<(String, String)> {
        self.lock()
            .requests
            .iter()
            .filter(|r| r.method == "PUT" || r.method == "DELETE")
            .map(|r| (r.method.clone(), r.path.clone()))
            .collect()
    }

    pub fn write_requests(&self) -> Vec<Seen> {
        self.lock()
            .requests
            .iter()
            .filter(|r| r.method == "PUT" || r.method == "DELETE")
            .cloned()
            .collect()
    }
}
