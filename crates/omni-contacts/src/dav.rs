//! Bounded CardDAV HTTP for iCloud.
//!
//! Every request has a timeout and a body cap, redirects are followed by hand
//! (PROPFIND only) and credentials go only to `contacts.icloud.com` and its
//! `pNN-contacts.icloud.com` shards over HTTPS. Writes are conditional and,
//! in [`SideEffectMode::Record`], captured instead of sent.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use omni_http::{HttpClient, HttpError, Method, RedirectRule, SideEffectMode, Url};
use quick_xml::Reader;
use quick_xml::events::Event;

/// The discovery root (RFC 6764 context path of iCloud contacts).
pub const ROOT: &str = "https://contacts.icloud.com/";
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// One multistatus answer (a multiget batch carries photos).
pub const XML_MAX_BYTES: usize = 16 * 1024 * 1024;
/// One vCard resource.
pub const CARD_MAX_BYTES: usize = 8 * 1024 * 1024;
const WRITE_RESPONSE_MAX_BYTES: usize = 64 * 1024;
const MAX_REDIRECTS: usize = 5;

/// `contacts.icloud.com` or a `pNN-contacts.icloud.com` shard.
pub fn is_icloud_contacts_host(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    if host == "contacts.icloud.com" {
        return true;
    }
    host.strip_prefix('p')
        .and_then(|rest| rest.strip_suffix("-contacts.icloud.com"))
        .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
}

pub fn is_trusted(url: &Url) -> bool {
    url.scheme() == "https" && url.host_str().is_some_and(is_icloud_contacts_host)
}

/// One response, body read under its cap.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DavResponse {
    pub status: u16,
    pub etag: Option<String>,
    pub location: Option<String>,
    pub body: Vec<u8>,
}

impl DavResponse {
    /// The body as UTF-8. Bytes that are not UTF-8 are refused: a lossy
    /// decode would change them on the next write.
    pub fn utf8(&self) -> Result<&str, std::str::Utf8Error> {
        std::str::from_utf8(&self.body)
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ReadError {
    #[error("response exceeds the {0}-byte limit")]
    TooLarge(usize),
    #[error("refusing to send iCloud credentials to {0}")]
    Untrusted(String),
    #[error("{0}")]
    Transport(String),
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WriteError {
    /// Refused locally; nothing was sent.
    #[error("not sent: {0}")]
    NotSent(String),
    /// The request may have reached the server.
    #[error("{0}")]
    Uncertain(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Precondition {
    IfMatch(String),
    IfNoneMatchAny,
}

impl Precondition {
    pub fn describe(&self) -> String {
        match self {
            Precondition::IfMatch(etag) => format!("if-match:{etag}"),
            Precondition::IfNoneMatchAny => "if-none-match".to_owned(),
        }
    }
}

/// A write captured in record mode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedWrite {
    pub method: &'static str,
    pub url: String,
    pub precondition: String,
    pub body: Option<String>,
}

/// Authenticated CardDAV requests.
#[derive(Clone)]
pub struct Dav {
    http: HttpClient,
    auth: Arc<str>,
    mode: SideEffectMode,
    recorded: Arc<Mutex<Vec<RecordedWrite>>>,
}

impl Dav {
    pub fn new(http: HttpClient, username: &str, password: &str, mode: SideEffectMode) -> Self {
        use base64::Engine as _;
        let credential =
            base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));
        Self {
            http,
            auth: Arc::from(format!("Basic {credential}")),
            mode,
            recorded: Arc::default(),
        }
    }

    pub fn is_recording(&self) -> bool {
        self.mode == SideEffectMode::Record
    }

    /// Writes captured in record mode.
    pub fn recorded(&self) -> Vec<RecordedWrite> {
        self.recorded
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    async fn send(
        &self,
        method: Method,
        url: &Url,
        headers: &[(&'static str, &str)],
        body: Option<String>,
        max_bytes: usize,
    ) -> Result<DavResponse, HttpError> {
        let mut builder = self
            .http
            .request(method, url.clone())
            .timeout(REQUEST_TIMEOUT)
            .redirect(RedirectRule::None)
            .header("Authorization", &*self.auth);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        if let Some(body) = body {
            builder = builder.body(body);
        }
        let response = builder.send_bounded(max_bytes).await?;
        let header = |name: &str| {
            response
                .headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        Ok(DavResponse {
            status: response.status.as_u16(),
            etag: header("etag"),
            location: header("location"),
            body: response.body.to_vec(),
        })
    }

    fn read_error(error: HttpError) -> ReadError {
        match error {
            HttpError::TooLarge { limit } => ReadError::TooLarge(limit),
            other => ReadError::Transport(other.to_string()),
        }
    }

    fn trusted(url: &Url) -> Result<(), ReadError> {
        if is_trusted(url) {
            Ok(())
        } else {
            Err(ReadError::Untrusted(url.origin().ascii_serialization()))
        }
    }

    fn method(name: &str) -> Result<Method, ReadError> {
        Method::from_bytes(name.as_bytes()).map_err(|e| ReadError::Transport(e.to_string()))
    }

    /// A PROPFIND, following redirects between trusted hosts (iCloud sends
    /// the discovery root to the account's shard). Returns the response and
    /// the URL that answered.
    pub async fn propfind(
        &self,
        url: &Url,
        depth: &str,
        body: &str,
    ) -> Result<(DavResponse, Url), ReadError> {
        let mut current = url.clone();
        for _ in 0..=MAX_REDIRECTS {
            Self::trusted(&current)?;
            let response = self
                .send(
                    Self::method("PROPFIND")?,
                    &current,
                    &[
                        ("Content-Type", "application/xml; charset=utf-8"),
                        ("Depth", depth),
                    ],
                    Some(body.to_owned()),
                    XML_MAX_BYTES,
                )
                .await
                .map_err(Self::read_error)?;
            if (300..400).contains(&response.status) {
                let location = response.location.as_deref().ok_or_else(|| {
                    ReadError::Transport(format!("redirect {} without Location", response.status))
                })?;
                current = current
                    .join(location)
                    .map_err(|e| ReadError::Transport(format!("invalid redirect: {e}")))?;
                continue;
            }
            return Ok((response, current));
        }
        Err(ReadError::Transport(format!(
            "more than {MAX_REDIRECTS} redirects"
        )))
    }

    pub async fn report(&self, url: &Url, body: &str) -> Result<DavResponse, ReadError> {
        Self::trusted(url)?;
        self.send(
            Self::method("REPORT")?,
            url,
            &[
                ("Content-Type", "application/xml; charset=utf-8"),
                ("Depth", "1"),
            ],
            Some(body.to_owned()),
            XML_MAX_BYTES,
        )
        .await
        .map_err(Self::read_error)
    }

    pub async fn get(&self, url: &Url) -> Result<DavResponse, ReadError> {
        Self::trusted(url)?;
        self.send(Method::GET, url, &[], None, CARD_MAX_BYTES)
            .await
            .map_err(Self::read_error)
    }

    fn write_error(error: HttpError) -> WriteError {
        match error {
            HttpError::InvalidUrl(m) | HttpError::Blocked(m) => WriteError::NotSent(m),
            other => WriteError::Uncertain(other.to_string()),
        }
    }

    fn record(&self, method: &'static str, url: &Url, precondition: String, body: Option<String>) {
        self.recorded
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(RecordedWrite {
                method,
                url: url.to_string(),
                precondition,
                body,
            });
    }

    /// A conditional PUT of one vCard.
    pub async fn put(
        &self,
        url: &Url,
        body: String,
        precondition: &Precondition,
    ) -> Result<DavResponse, WriteError> {
        if !is_trusted(url) {
            return Err(WriteError::NotSent(format!(
                "untrusted URL {}",
                url.origin().ascii_serialization()
            )));
        }
        if self.is_recording() {
            self.record("PUT", url, precondition.describe(), Some(body));
            return Ok(DavResponse {
                status: 204,
                etag: None,
                location: None,
                body: Vec::new(),
            });
        }
        let (name, value) = match precondition {
            Precondition::IfMatch(etag) => ("If-Match", etag.as_str()),
            Precondition::IfNoneMatchAny => ("If-None-Match", "*"),
        };
        self.send(
            Method::PUT,
            url,
            &[("Content-Type", "text/vcard; charset=utf-8"), (name, value)],
            Some(body),
            WRITE_RESPONSE_MAX_BYTES,
        )
        .await
        .map_err(Self::write_error)
    }

    /// A DELETE with If-Match.
    pub async fn delete(&self, url: &Url, etag: &str) -> Result<DavResponse, WriteError> {
        if !is_trusted(url) {
            return Err(WriteError::NotSent(format!(
                "untrusted URL {}",
                url.origin().ascii_serialization()
            )));
        }
        if self.is_recording() {
            self.record("DELETE", url, format!("if-match:{etag}"), None);
            return Ok(DavResponse {
                status: 204,
                etag: None,
                location: None,
                body: Vec::new(),
            });
        }
        self.send(
            Method::DELETE,
            url,
            &[("If-Match", etag)],
            None,
            WRITE_RESPONSE_MAX_BYTES,
        )
        .await
        .map_err(Self::write_error)
    }
}

// ---- multistatus ----

/// A minimal element tree (local names, lowercased).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct XmlNode {
    pub name: String,
    pub text: String,
    pub children: Vec<XmlNode>,
}

impl XmlNode {
    pub fn child(&self, name: &str) -> Option<&XmlNode> {
        self.children.iter().find(|c| c.name == name)
    }

    pub fn find(&self, name: &str) -> Option<&XmlNode> {
        for child in &self.children {
            if child.name == name {
                return Some(child);
            }
            if let Some(found) = child.find(name) {
                return Some(found);
            }
        }
        None
    }

    pub fn trimmed(&self) -> String {
        self.text.trim().to_owned()
    }

    /// The text of the first `href` below this node.
    pub fn href(&self) -> Option<String> {
        self.find("href")
            .map(XmlNode::trimmed)
            .filter(|h| !h.is_empty())
    }
}

fn local_name(name: &str) -> String {
    name.rsplit(':')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
}

fn resolve_entity(name: &str) -> String {
    if let Some(number) = name.strip_prefix('#') {
        let code = match number.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok(),
            None => number.parse::<u32>().ok(),
        };
        return code
            .and_then(char::from_u32)
            .map_or_else(|| format!("&{name};"), String::from);
    }
    match name {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" => "'",
        other => return format!("&{other};"),
    }
    .to_owned()
}

pub fn parse_xml(xml: &str) -> Result<XmlNode, String> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);
    let mut stack: Vec<XmlNode> = vec![XmlNode::default()];
    loop {
        match reader.read_event().map_err(|e| e.to_string())? {
            Event::Start(start) => stack.push(XmlNode {
                name: local_name(AsRef::<str>::as_ref(&start.name())),
                ..XmlNode::default()
            }),
            Event::Empty(start) => {
                let node = XmlNode {
                    name: local_name(AsRef::<str>::as_ref(&start.name())),
                    ..XmlNode::default()
                };
                if let Some(top) = stack.last_mut() {
                    top.children.push(node);
                }
            }
            Event::End(_) => {
                if stack.len() < 2 {
                    return Err("unbalanced end tag".to_owned());
                }
                if let Some(node) = stack.pop()
                    && let Some(parent) = stack.last_mut()
                {
                    parent.children.push(node);
                }
            }
            Event::Text(text) => {
                if let Some(top) = stack.last_mut() {
                    top.text.push_str(AsRef::<str>::as_ref(&text));
                }
            }
            Event::CData(data) => {
                if let Some(top) = stack.last_mut() {
                    top.text.push_str(AsRef::<str>::as_ref(&data));
                }
            }
            Event::GeneralRef(reference) => {
                if let Some(top) = stack.last_mut() {
                    top.text
                        .push_str(&resolve_entity(AsRef::<str>::as_ref(&reference)));
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if stack.len() != 1 {
        return Err("unterminated element".to_owned());
    }
    stack
        .pop()
        .and_then(|root| root.children.into_iter().next())
        .ok_or_else(|| "empty document".to_owned())
}

/// `HTTP/1.1 404 Not Found` → 404.
fn status_code(text: &str) -> Option<u16> {
    text.split_whitespace().nth(1)?.parse().ok()
}

/// One `<response>` of a multistatus.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MsResponse {
    pub href: String,
    /// The response-level status (a multiget's 404 for a missing resource).
    pub status: Option<u16>,
    /// Properties from successful propstats.
    pub props: Vec<XmlNode>,
}

impl MsResponse {
    pub fn prop(&self, name: &str) -> Option<&XmlNode> {
        self.props.iter().find(|p| p.name == name)
    }

    pub fn text(&self, name: &str) -> Option<String> {
        self.prop(name)
            .map(XmlNode::trimmed)
            .filter(|t| !t.is_empty())
    }

    /// The raw (untrimmed) text of a property, for address data.
    pub fn raw(&self, name: &str) -> Option<&str> {
        self.prop(name).map(|p| p.text.as_str())
    }

    /// Element names inside `resourcetype`.
    pub fn resource_types(&self) -> Vec<String> {
        self.prop("resourcetype")
            .map(|r| r.children.iter().map(|c| c.name.clone()).collect())
            .unwrap_or_default()
    }
}

pub fn multistatus(xml: &str) -> Result<Vec<MsResponse>, String> {
    let root = parse_xml(xml)?;
    if root.name != "multistatus" {
        return Err(format!("expected multistatus, got {}", root.name));
    }
    let mut out = Vec::new();
    for response in root.children.iter().filter(|c| c.name == "response") {
        let Some(href) = response.child("href").map(XmlNode::trimmed) else {
            continue;
        };
        let mut props = Vec::new();
        for propstat in response.children.iter().filter(|c| c.name == "propstat") {
            let ok = propstat
                .child("status")
                .and_then(|s| status_code(&s.text))
                .is_none_or(|s| (200..300).contains(&s));
            if ok && let Some(prop) = propstat.child("prop") {
                props.extend(prop.children.iter().cloned());
            }
        }
        out.push(MsResponse {
            href,
            status: response.child("status").and_then(|s| status_code(&s.text)),
            props,
        });
    }
    Ok(out)
}

/// Escapes text for an XML body.
pub fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trusts_only_icloud_contacts_hosts() {
        assert!(is_icloud_contacts_host("contacts.icloud.com"));
        assert!(is_icloud_contacts_host("p57-contacts.icloud.com"));
        assert!(!is_icloud_contacts_host("p57-caldav.icloud.com"));
        assert!(!is_icloud_contacts_host("contacts.icloud.com.example"));
        assert!(!is_trusted(
            &Url::parse("http://contacts.icloud.com/").unwrap()
        ));
    }

    #[test]
    fn parses_address_data_with_entities_and_cdata() {
        let xml = r#"<d:multistatus xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav">
<d:response><d:href>/1/carddavhome/card/a.vcf</d:href><d:propstat><d:prop>
<d:getetag>"e1"</d:getetag><card:address-data>BEGIN:VCARD&#13;
FN:A &amp; B&#13;
END:VCARD&#13;
</card:address-data></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>
<d:response><d:href>/1/carddavhome/card/b.vcf</d:href><d:status>HTTP/1.1 404 Not Found</d:status></d:response>
<d:response><d:href>/1/carddavhome/card/c.vcf</d:href><d:propstat><d:prop><card:address-data><![CDATA[BEGIN:VCARD
END:VCARD]]></card:address-data></d:prop></d:propstat></d:response>
</d:multistatus>"#;
        let responses = multistatus(xml).unwrap();
        assert_eq!(responses[0].text("getetag").as_deref(), Some("\"e1\""));
        assert!(
            responses[0]
                .raw("address-data")
                .unwrap()
                .contains("FN:A & B\r\n")
        );
        assert_eq!(responses[1].status, Some(404));
        assert!(
            responses[2]
                .raw("address-data")
                .unwrap()
                .starts_with("BEGIN:VCARD")
        );
    }
}
