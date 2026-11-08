//! Discovery of every address book and an in-memory index of their cards.
//!
//! RFC 6764/6352 discovery: the iCloud root → current-user-principal →
//! addressbook-home-set → every collection whose resourcetype is an
//! addressbook. Each book is refreshed by its CTag; when that changed, a
//! Depth 1 ETag listing names the cards to fetch by addressbook-multiget.
//! The index keeps read projections only (no photo bytes) and lives in
//! memory. The docstore holds no contact data from reads; the only card
//! text it holds is the bounded private recovery copy a write keeps
//! (`contacts-recovery-copy`, see `store`).

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use omni_core::clock::SharedClock;
use omni_http::Url;
use tokio::sync::Mutex;

use crate::dav::{self, Dav, MsResponse, ReadError, xml_escape};
use crate::error::ContactsError;
use crate::model::{self, Projection};
use crate::vcard::Card;

const LOG: &str = "Contacts";

/// Reads refresh the index when it is older than this.
pub const STALE_AFTER_MS: i64 = 30_000;
/// Cards per addressbook-multiget (halved when an answer is too large).
const MULTIGET_BATCH: usize = 50;

const PRINCIPAL_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:"><d:prop><d:current-user-principal/></d:prop></d:propfind>"#;
const HOME_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav"><d:prop><card:addressbook-home-set/></d:prop></d:propfind>"#;
const BOOKS_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/><d:displayname/></d:prop></d:propfind>"#;
const CTAG_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:" xmlns:cs="http://calendarserver.org/ns/"><d:prop><cs:getctag/><d:sync-token/><d:displayname/></d:prop></d:propfind>"#;
const ETAG_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:"><d:prop><d:getetag/><d:resourcetype/></d:prop></d:propfind>"#;

/// Path segments usable in ids (iCloud names cards `<UUID>.vcf`). Percent
/// escapes are decoded for the check: an encoded dot segment, slash or
/// backslash would name another path once the URL is resolved.
pub fn valid_segment(segment: &str) -> bool {
    if segment.is_empty()
        || segment.len() > 200
        || !segment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._~%@+=:,-".contains(&b))
    {
        return false;
    }
    let Some(decoded) = percent_decode(segment) else {
        return false;
    };
    decoded != b"."
        && decoded != b".."
        && !decoded
            .iter()
            .any(|b| matches!(b, b'/' | b'\\' | 0..=0x1f | 0x7f))
}

fn percent_decode(segment: &str) -> Option<Vec<u8>> {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = segment.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Some(out)
}

/// The last non-empty path segment of an href.
pub fn last_segment(href: &str) -> Option<&str> {
    let path = href.split(['?', '#']).next().unwrap_or(href);
    path.trim_end_matches('/').rsplit('/').next()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BookInfo {
    /// The book's final path segment (e.g. `card`).
    pub id: String,
    pub name: String,
    /// Ends with `/`.
    pub url: Url,
}

impl BookInfo {
    pub fn resource_url(&self, resource: &str) -> Result<Url, ContactsError> {
        if !valid_segment(resource) {
            return Err(ContactsError::invalid(format!(
                "invalid resource name {resource:?}"
            )));
        }
        let url = self
            .url
            .join(resource)
            .map_err(|e| ContactsError::invalid(format!("invalid resource name: {e}")))?;
        // The resolved URL must be exactly a direct child of the book.
        let direct = url.origin() == self.url.origin()
            && url.path() == format!("{}{resource}", self.url.path())
            && url.query().is_none()
            && url.fragment().is_none();
        if !direct {
            return Err(ContactsError::invalid(format!(
                "invalid resource name {resource:?}"
            )));
        }
        Ok(url)
    }
}

/// One card in the index.
#[derive(Clone, Debug)]
pub struct Entry {
    pub resource: String,
    pub href: String,
    pub etag: String,
    /// `None` when the card could not be read (see `problem`).
    pub view: Option<Arc<Projection>>,
    pub problem: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Book {
    pub info: BookInfo,
    pub ctag: Option<String>,
    /// By resource name.
    pub entries: BTreeMap<String, Entry>,
    /// Listed resources whose names cannot be used as ids.
    pub skipped: u32,
    loaded: bool,
    dirty: bool,
}

impl Book {
    fn new(info: BookInfo) -> Self {
        Self {
            info,
            ctag: None,
            entries: BTreeMap::new(),
            skipped: 0,
            loaded: false,
            dirty: true,
        }
    }

    /// The resource whose card has this UID.
    pub fn by_uid(&self, uid: &str) -> Option<&Entry> {
        self.entries.values().find(|e| {
            e.view
                .as_ref()
                .and_then(|v| v.uid.as_deref())
                .is_some_and(|u| u.eq_ignore_ascii_case(uid))
        })
    }

    pub fn groups(&self) -> impl Iterator<Item = (&Entry, &Projection)> {
        self.entries.values().filter_map(|e| {
            e.view
                .as_deref()
                .filter(|v| v.kind == model::Kind::Group)
                .map(|v| (e, v))
        })
    }
}

/// The index at one moment.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub books: Arc<Vec<Book>>,
    pub synced_at: Option<i64>,
    /// The last refresh failed; the data is from `synced_at`.
    pub stale: bool,
    pub error: Option<String>,
}

impl Snapshot {
    pub fn book(&self, id: &str) -> Option<&Book> {
        self.books.iter().find(|b| b.info.id == id)
    }
}

#[derive(Default)]
struct State {
    books: Option<Arc<Vec<Book>>>,
    synced_at: Option<i64>,
    last_error: Option<String>,
}

pub struct Directory {
    dav: Dav,
    clock: SharedClock,
    state: Mutex<State>,
}

fn check_status(response: &dav::DavResponse, what: &str) -> Result<(), ContactsError> {
    if response.status == 207 || response.is_success() {
        Ok(())
    } else {
        Err(ContactsError::status(response.status, what))
    }
}

fn parse(response: &dav::DavResponse, what: &str) -> Result<Vec<MsResponse>, ContactsError> {
    let text = response
        .utf8()
        .map_err(|_| ContactsError::protocol(format!("{what}: the answer is not UTF-8")))?;
    dav::multistatus(text).map_err(|e| ContactsError::protocol(format!("{what}: {e}")))
}

fn same_path(a: &str, b: &str) -> bool {
    a.trim_end_matches('/') == b.trim_end_matches('/')
}

impl Directory {
    pub fn new(dav: Dav, clock: SharedClock) -> Self {
        Self {
            dav,
            clock,
            state: Mutex::new(State::default()),
        }
    }

    pub fn dav(&self) -> &Dav {
        &self.dav
    }

    /// Marks every book for refresh on the next read (after a write).
    pub async fn invalidate(&self) {
        let mut state = self.state.lock().await;
        if let Some(books) = &state.books {
            let mut next = (**books).clone();
            for book in &mut next {
                book.dirty = true;
            }
            state.books = Some(Arc::new(next));
        }
    }

    /// Forgets discovery (after a 404 on a book or an authorization failure).
    pub async fn forget(&self) {
        let mut state = self.state.lock().await;
        state.books = None;
    }

    /// The index, refreshed when older than [`STALE_AFTER_MS`], when `fresh`,
    /// or after a write. A failed refresh serves the previous index as stale.
    pub async fn snapshot(&self, fresh: bool) -> Result<Snapshot, ContactsError> {
        let mut state = self.state.lock().await;
        let now = self.clock.now_ms();
        let due = fresh
            || state.synced_at.is_none_or(|at| now - at >= STALE_AFTER_MS)
            || state
                .books
                .as_ref()
                .is_some_and(|books| books.iter().any(|b| b.dirty));
        if !due && let Some(books) = &state.books {
            return Ok(Snapshot {
                books: books.clone(),
                synced_at: state.synced_at,
                stale: false,
                error: None,
            });
        }
        match self.refresh(state.books.clone(), fresh).await {
            Ok(books) => {
                let books = Arc::new(books);
                state.books = Some(books.clone());
                state.synced_at = Some(now);
                state.last_error = None;
                Ok(Snapshot {
                    books,
                    synced_at: Some(now),
                    stale: false,
                    error: None,
                })
            }
            Err(error) => {
                tracing::warn!(target: LOG, "Contacts refresh failed: {error}");
                if error.code() == "unauthorized" || error.code() == "not_found" {
                    state.books = None;
                }
                state.last_error = Some(error.to_string());
                match &state.books {
                    Some(books) if books.iter().all(|b| b.loaded) => Ok(Snapshot {
                        books: books.clone(),
                        synced_at: state.synced_at,
                        stale: true,
                        error: Some(error.to_string()),
                    }),
                    _ => Err(error),
                }
            }
        }
    }

    /// Refreshes every book; `rediscover` (a `fresh` read) lists the books
    /// again, keeping the index of books that still exist.
    async fn refresh(
        &self,
        current: Option<Arc<Vec<Book>>>,
        rediscover: bool,
    ) -> Result<Vec<Book>, ContactsError> {
        let mut books = match current {
            Some(books) if !rediscover => (*books).clone(),
            current => {
                let known: Vec<Book> = current.map(|b| (*b).clone()).unwrap_or_default();
                self.discover()
                    .await?
                    .into_iter()
                    .map(|info| match known.iter().find(|b| b.info == info) {
                        Some(book) => book.clone(),
                        None => Book::new(info),
                    })
                    .collect()
            }
        };
        for book in &mut books {
            self.refresh_book(book, rediscover).await?;
        }
        Ok(books)
    }

    /// Every address book of the account.
    pub async fn discover(&self) -> Result<Vec<BookInfo>, ContactsError> {
        let root = Url::parse(dav::ROOT).map_err(|e| ContactsError::protocol(e.to_string()))?;
        let (response, at) = self.dav.propfind(&root, "0", PRINCIPAL_BODY).await?;
        check_status(&response, "the principal lookup")?;
        let principal = parse(&response, "principal lookup")?
            .iter()
            .find_map(|r| r.prop("current-user-principal").and_then(|p| p.href()))
            .ok_or_else(|| ContactsError::protocol("iCloud returned no current-user-principal"))?;
        let principal = at
            .join(&principal)
            .map_err(|e| ContactsError::protocol(format!("invalid principal href: {e}")))?;
        let (response, at) = self.dav.propfind(&principal, "0", HOME_BODY).await?;
        check_status(&response, "the address book home lookup")?;
        let home = parse(&response, "home lookup")?
            .iter()
            .find_map(|r| r.prop("addressbook-home-set").and_then(|p| p.href()))
            .ok_or_else(|| ContactsError::protocol("iCloud returned no addressbook-home-set"))?;
        let home = at
            .join(&home)
            .map_err(|e| ContactsError::protocol(format!("invalid home href: {e}")))?;
        let (response, at) = self.dav.propfind(&home, "1", BOOKS_BODY).await?;
        check_status(&response, "the address book listing")?;
        let mut books = Vec::new();
        for item in parse(&response, "address book listing")? {
            if !item.resource_types().iter().any(|t| t == "addressbook") {
                continue;
            }
            let Some(id) = last_segment(&item.href).filter(|s| valid_segment(s)) else {
                tracing::warn!(target: LOG, "Skipping an address book with an unusable path");
                continue;
            };
            let href = if item.href.ends_with('/') {
                item.href.clone()
            } else {
                format!("{}/", item.href)
            };
            let url = at
                .join(&href)
                .map_err(|e| ContactsError::protocol(format!("invalid book href: {e}")))?;
            books.push(BookInfo {
                id: id.to_owned(),
                name: item.text("displayname").unwrap_or_else(|| id.to_owned()),
                url,
            });
        }
        if books.is_empty() {
            return Err(ContactsError::coded(
                "no_address_books",
                "the iCloud account has no address books",
            ));
        }
        Ok(books)
    }

    /// Refreshes one book. An unchanged CTag skips the listing, except on a
    /// `fresh` read while some cards are unreadable: those are retried.
    async fn refresh_book(&self, book: &mut Book, fresh: bool) -> Result<(), ContactsError> {
        let (response, _) = self.dav.propfind(&book.info.url, "0", CTAG_BODY).await?;
        check_status(&response, "the address book")?;
        let ctag = parse(&response, "address book")?
            .first()
            .and_then(|r| r.text("getctag").or_else(|| r.text("sync-token")));
        let retry_unreadable = fresh && book.entries.values().any(|e| e.view.is_none());
        if book.loaded && !book.dirty && !retry_unreadable && ctag.is_some() && ctag == book.ctag {
            return Ok(());
        }
        let (response, _) = self.dav.propfind(&book.info.url, "1", ETAG_BODY).await?;
        check_status(&response, "the address book listing")?;
        let book_path = book.info.url.path().to_owned();
        let mut listed: BTreeMap<String, (String, String)> = BTreeMap::new();
        let mut skipped = 0;
        for item in parse(&response, "address book listing")? {
            let path = Url::parse(&item.href)
                .map(|u| u.path().to_owned())
                .unwrap_or_else(|_| item.href.clone());
            if same_path(&path, &book_path)
                || item.resource_types().iter().any(|t| t == "collection")
            {
                continue;
            }
            let Some(etag) = item.text("getetag") else {
                continue;
            };
            match last_segment(&item.href).filter(|s| valid_segment(s)) {
                Some(resource) => {
                    listed.insert(resource.to_owned(), (item.href.clone(), etag));
                }
                None => skipped += 1,
            }
        }
        let mut entries = BTreeMap::new();
        let mut wanted = Vec::new();
        for (resource, (href, etag)) in &listed {
            match book.entries.get(resource) {
                // An unreadable entry is fetched again, so a transient
                // failure does not hide a card (or a group) for good.
                Some(entry) if entry.etag == *etag && entry.view.is_some() => {
                    entries.insert(resource.clone(), entry.clone());
                }
                _ => wanted.push((resource.clone(), href.clone(), etag.clone())),
            }
        }
        for entry in self.fetch(&book.info.url, wanted).await? {
            entries.insert(entry.resource.clone(), entry);
        }
        book.entries = entries;
        book.ctag = ctag;
        book.skipped = skipped;
        book.loaded = true;
        book.dirty = false;
        Ok(())
    }

    /// Fetches cards by addressbook-multiget, halving a batch whose answer is
    /// too large or not UTF-8; a single such card, or one the server leaves
    /// out of its answer, is indexed as unreadable rather than dropped.
    async fn fetch(
        &self,
        book_url: &Url,
        wanted: Vec<(String, String, String)>,
    ) -> Result<Vec<Entry>, ContactsError> {
        let mut out = Vec::new();
        let mut queue: VecDeque<Vec<(String, String, String)>> =
            wanted.chunks(MULTIGET_BATCH).map(<[_]>::to_vec).collect();
        while let Some(batch) = queue.pop_front() {
            let mut body = String::from(
                r#"<?xml version="1.0" encoding="utf-8"?><card:addressbook-multiget xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav"><d:prop><d:getetag/><card:address-data/></d:prop>"#,
            );
            for (_, href, _) in &batch {
                body.push_str(&format!("<d:href>{}</d:href>", xml_escape(href)));
            }
            body.push_str("</card:addressbook-multiget>");
            let response = match self.dav.report(book_url, &body).await {
                Ok(response) => response,
                Err(ReadError::TooLarge(limit)) if batch.len() > 1 => {
                    let (a, b) = batch.split_at(batch.len() / 2);
                    queue.push_front(b.to_vec());
                    queue.push_front(a.to_vec());
                    tracing::debug!(target: LOG, "Multiget over {limit} bytes; halving");
                    continue;
                }
                Err(ReadError::TooLarge(limit)) => {
                    let (resource, href, etag) = batch[0].clone();
                    out.push(Entry {
                        resource,
                        href,
                        etag,
                        view: None,
                        problem: Some(format!("the card exceeds {limit} bytes")),
                    });
                    continue;
                }
                Err(other) => return Err(other.into()),
            };
            check_status(&response, "the card download")?;
            if response.utf8().is_err() {
                if batch.len() > 1 {
                    let (a, b) = batch.split_at(batch.len() / 2);
                    queue.push_front(b.to_vec());
                    queue.push_front(a.to_vec());
                } else {
                    let (resource, href, etag) = batch[0].clone();
                    out.push(Entry {
                        resource,
                        href,
                        etag,
                        view: None,
                        problem: Some("the card is not valid UTF-8".to_owned()),
                    });
                }
                continue;
            }
            let mut answered = BTreeMap::new();
            for item in parse(&response, "card download")? {
                if let Some(resource) = last_segment(&item.href) {
                    answered.insert(resource.to_owned(), item);
                }
            }
            for (resource, href, etag) in batch {
                let Some(item) = answered.remove(&resource) else {
                    out.push(Entry {
                        resource,
                        href,
                        etag,
                        view: None,
                        problem: Some("the server did not return this card".to_owned()),
                    });
                    continue;
                };
                // 404: deleted between the listing and the download.
                if item.status == Some(404) {
                    continue;
                }
                if item.status.is_some_and(|s| !(200..300).contains(&s)) {
                    out.push(Entry {
                        resource,
                        href,
                        etag,
                        view: None,
                        problem: Some(format!("the server answered HTTP {:?}", item.status)),
                    });
                    continue;
                }
                let etag = item.text("getetag").unwrap_or(etag);
                let (view, problem) = match item.raw("address-data").map(Card::parse) {
                    Some(Ok(card)) => (Some(Arc::new(model::project(&card))), None),
                    Some(Err(error)) => (None, Some(error.to_string())),
                    None => (None, Some("the server returned no card data".to_owned())),
                };
                out.push(Entry {
                    resource,
                    href,
                    etag,
                    view,
                    problem,
                });
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_urls_stay_direct_children_of_their_book() {
        let book = BookInfo {
            id: "card".to_owned(),
            name: "Contacts".to_owned(),
            url: Url::parse("https://p42-contacts.icloud.com/123/carddavhome/card/").unwrap(),
        };
        let ok = book.resource_url("0D5D-AB.vcf").unwrap();
        assert_eq!(ok.path(), "/123/carddavhome/card/0D5D-AB.vcf");
        assert!(book.resource_url("name%20x.vcf").is_ok());
        for bad in [
            "..", ".", "%2e%2e", "%2E%2e", "%2e", "a%2fb", "a%2Fb", "a%5cb", "%zz", "%2", "a/b",
            "a?b", "a#b", "",
        ] {
            assert!(book.resource_url(bad).is_err(), "{bad:?} accepted");
        }
    }

    #[test]
    fn segments() {
        assert_eq!(last_segment("/1/carddavhome/card/"), Some("card"));
        assert_eq!(last_segment("https://h/1/card/A-B.vcf"), Some("A-B.vcf"));
        assert!(valid_segment("0D5D0C5F-0A6E-4E4A-9D1B-1C4B5E6A7B8C.vcf"));
        assert!(!valid_segment(".."));
        assert!(!valid_segment("a b"));
        assert!(!valid_segment("a/b"));
    }
}
