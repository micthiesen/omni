//! The inbox: stored source material (a fetched page, pasted text, captured
//! photos) that agents turn into recipes. After import an item stays as the
//! recipe's Original.

use std::time::Duration;

use omni_runtime::ports::FetchedPage;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::api::{
    ImportUrlInput, ImportUrlOutput, InboxGetInput, InboxGetOutput, InboxItemInfo, InboxKind,
    InboxListInput, InboxListOutput, InboxResolveInput, InboxResolveOutput, InboxState, PhotoInfo,
    RecipeRef, Render, Resolution, SkippedItem, Structured,
};
use crate::db::{self, iso, next_seq, normal_url, recipe_for_read};
use crate::error::{R, invalid, not_found, reject, sql};
use crate::events;
use crate::files::{Rendition, Stored};
use crate::ids;
use crate::jsonld;
use crate::service::Recipes;

/// Characters of text one call returns.
pub const TEXT_CHUNK: usize = 20_000;
/// Stored page text is cut here.
const MAX_STORED_TEXT: usize = 400_000;
/// Photos one `recipes_inbox_get` call returns.
pub const PHOTOS_PER_CALL: usize = 4;
/// Photos one inbox item holds.
pub const MAX_PHOTOS: u32 = 20;
/// Bytes of one uploaded photo.
pub const MAX_PHOTO_BYTES: usize = 25 * 1024 * 1024;
/// Items waiting in the inbox (pending or still uploading). The app has no
/// login, so this bounds what anyone on the network can pile up.
pub const MAX_WAITING_ITEMS: u32 = 200;

/// Refuses a new inbox item while the inbox is full.
pub fn room_for_item(conn: &Connection) -> R<()> {
    let waiting: u32 = conn
        .query_row(
            "SELECT count(*) FROM recipes_inbox WHERE state IN ('pending', 'uploading')",
            [],
            |row| row.get(0),
        )
        .map_err(sql)?;
    if waiting >= MAX_WAITING_ITEMS {
        return Err(reject(
            "too_many",
            format!(
                "the inbox holds {waiting} waiting items; import or discard some first (recipes_inbox_resolve)"
            ),
        ));
    }
    Ok(())
}
/// Bytes of an image fetched from a URL.
pub const MAX_URL_IMAGE_BYTES: usize = 15 * 1024 * 1024;
const IMAGE_FETCH_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub id: String,
    pub kind: String,
    pub state: String,
    pub title: Option<String>,
    pub payload: Value,
    pub created_at: i64,
    pub recipe_id: Option<String>,
}

/// A page's lead image as stored at import time.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LeadImage {
    pub sha256: String,
    pub mime: String,
    pub bytes: u32,
    pub width: u32,
    pub height: u32,
    pub color: String,
}

impl From<Stored> for LeadImage {
    fn from(stored: Stored) -> Self {
        Self {
            sha256: stored.sha256,
            mime: stored.mime,
            bytes: stored.bytes,
            width: stored.width,
            height: stored.height,
            color: stored.color,
        }
    }
}

impl Item {
    fn field(&self, key: &str) -> Option<&str> {
        self.payload.get(key).and_then(Value::as_str)
    }

    /// Where the page was read, for a `url` item.
    pub fn url(&self) -> Option<String> {
        self.field("finalUrl")
            .or_else(|| self.field("url"))
            .map(str::to_owned)
    }

    pub fn text(&self) -> Option<&str> {
        self.field("text")
    }

    pub fn structured(&self) -> Option<Structured> {
        self.payload
            .get("structured")
            .and_then(|value| serde_json::from_value(value.clone()).ok())
    }

    pub fn lead_image(&self) -> Option<LeadImage> {
        self.payload
            .get("leadImage")
            .and_then(|value| serde_json::from_value(value.clone()).ok())
    }

    fn kind(&self) -> InboxKind {
        match self.kind.as_str() {
            "photo" => InboxKind::Photo,
            "url" => InboxKind::Url,
            _ => InboxKind::Text,
        }
    }

    fn state(&self) -> InboxState {
        match self.state.as_str() {
            "imported" => InboxState::Imported,
            "discarded" => InboxState::Discarded,
            "duplicate" => InboxState::Duplicate,
            _ => InboxState::Pending,
        }
    }

    pub(crate) fn info(&self, files: u32) -> InboxItemInfo {
        InboxItemInfo {
            item_id: self.id.clone(),
            kind: self.kind(),
            title: self.title.clone(),
            added_at: iso(self.created_at),
            files,
            state: self.state(),
            recipe_id: self.recipe_id.clone(),
        }
    }
}

const ITEM_COLUMNS: &str = "id, kind, state, title, payload, created_at, recipe_id";

fn item_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Item> {
    let payload: String = row.get(4)?;
    Ok(Item {
        id: row.get(0)?,
        kind: row.get(1)?,
        state: row.get(2)?,
        title: row.get(3)?,
        payload: serde_json::from_str(&payload).unwrap_or(Value::Null),
        created_at: row.get(5)?,
        recipe_id: row.get(6)?,
    })
}

pub fn item(conn: &Connection, id: &str) -> R<Option<Item>> {
    conn.query_row(
        &format!("SELECT {ITEM_COLUMNS} FROM recipes_inbox WHERE id = ?1"),
        params![id],
        item_from_row,
    )
    .optional()
    .map_err(sql)
}

/// An item agents can see: it exists and every photo has arrived.
pub fn visible_item(conn: &Connection, id: &str) -> R<Item> {
    item(conn, id)?
        .filter(|item| item.state != "uploading")
        .ok_or_else(|| not_found(format!("no inbox item has id {id:?}")))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileRow {
    pub idx: u32,
    pub sha256: String,
    pub mime: String,
    pub bytes: u32,
    pub width: u32,
    pub height: u32,
    pub color: String,
}

pub fn files(conn: &Connection, item_id: &str) -> R<Vec<FileRow>> {
    let mut statement = conn
        .prepare_cached(
            "SELECT idx, sha256, mime, bytes, width, height, color FROM recipes_inbox_file
             WHERE item_id = ?1 ORDER BY idx",
        )
        .map_err(sql)?;
    let rows = statement
        .query_map(params![item_id], |row| {
            Ok(FileRow {
                idx: row.get(0)?,
                sha256: row.get(1)?,
                mime: row.get(2)?,
                bytes: row.get(3)?,
                width: row.get(4)?,
                height: row.get(5)?,
                color: row.get(6)?,
            })
        })
        .map_err(sql)?;
    rows.collect::<Result<_, _>>().map_err(sql)
}

pub fn file_count(conn: &Connection, item_id: &str) -> R<u32> {
    conn.query_row(
        "SELECT count(*) FROM recipes_inbox_file WHERE item_id = ?1",
        params![item_id],
        |row| row.get(0),
    )
    .map_err(sql)
}

pub fn pending_count(conn: &Connection) -> R<u32> {
    conn.query_row(
        "SELECT count(*) FROM recipes_inbox WHERE state = 'pending'",
        [],
        |row| row.get(0),
    )
    .map_err(sql)
}

/// Stores pasted text as the source snapshot of the recipe being created.
pub fn insert_text(
    conn: &Connection,
    now: i64,
    title: &str,
    text: &str,
    recipe: &str,
) -> R<String> {
    let id = ids::new_id(ids::INBOX);
    conn.execute(
        "INSERT INTO recipes_inbox (id, kind, state, title, payload, created_at, resolved_at, recipe_id)
         VALUES (?1, 'text', 'imported', ?2, ?3, ?4, ?4, ?5)",
        params![id, title, json!({ "text": text }).to_string(), now, recipe],
    )
    .map_err(sql)?;
    Ok(id)
}

pub fn mark_imported(conn: &Connection, now: i64, item: &str, recipe: &str) -> R<()> {
    conn.execute(
        // An item already imported keeps the recipe it first became: a second
        // recipe made from it with allowDuplicate shares the source.
        "UPDATE recipes_inbox SET resolved_at = CASE WHEN state = 'imported' THEN resolved_at ELSE ?2 END,
           recipe_id = CASE WHEN state = 'imported' AND recipe_id IS NOT NULL THEN recipe_id ELSE ?3 END,
           state = 'imported' WHERE id = ?1",
        params![item, now, recipe],
    )
    .map_err(sql)?;
    Ok(())
}

/// What a source holds for the fidelity check and the review.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub kind: String,
    pub text: Option<String>,
    pub structured: Option<Structured>,
    /// Digests of captured photos, in order.
    pub photos: Vec<String>,
}

impl Snapshot {
    pub fn pasted(text: String) -> Self {
        Self {
            kind: "text".to_owned(),
            text: Some(text),
            ..Self::default()
        }
    }

    /// The text a reviewer reads: the page's own recipe data first, then the
    /// readable text.
    pub fn review_text(&self) -> Option<String> {
        let mut parts: Vec<String> = Vec::new();
        if let Some(structured) = &self.structured {
            parts.push(format!(
                "{}\n\nIngredients:\n{}\n\nInstructions:\n{}",
                structured.name,
                structured.ingredients.join("\n"),
                structured
                    .instructions
                    .iter()
                    .map(|step| match &step.section {
                        Some(section) => format!("[{section}] {}", step.text),
                        None => step.text.clone(),
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        } else if let Some(text) = &self.text {
            parts.push(text.clone());
        }
        let text = parts.join("\n\n");
        (!text.trim().is_empty()).then_some(text)
    }
}

/// The snapshot of an inbox item. `any_state` also reads an item that was
/// discarded (a recipe keeps its source).
pub fn snapshot(conn: &Connection, item_id: &str, any_state: bool) -> R<Snapshot> {
    let item = if any_state {
        item(conn, item_id)?
            .ok_or_else(|| not_found(format!("no inbox item has id {item_id:?}")))?
    } else {
        visible_item(conn, item_id)?
    };
    Ok(Snapshot {
        text: item.text().map(str::to_owned),
        structured: item.structured(),
        photos: files(conn, item_id)?
            .into_iter()
            .map(|file| file.sha256)
            .collect(),
        kind: item.kind,
    })
}

/// A chunk of `text` starting at character `offset`.
fn chunk(text: &str, offset: usize) -> (String, bool, Option<u32>) {
    let total = text.chars().count();
    let part: String = text.chars().skip(offset).take(TEXT_CHUNK).collect();
    let end = offset + part.chars().count();
    let truncated = end < total;
    (
        part,
        truncated,
        truncated.then(|| u32::try_from(end).unwrap_or(u32::MAX)),
    )
}

/// Markdown-ish text of a page that came without readable text.
fn html_text(html: &str) -> String {
    htmd::HtmlToMarkdown::builder()
        .skip_tags(vec![
            "script", "style", "noscript", "svg", "iframe", "nav", "footer", "form",
        ])
        .build()
        .convert(html)
        .unwrap_or_default()
}

struct Chosen {
    final_url: String,
    title: Option<String>,
    structured: Option<Structured>,
    text: String,
    image_url: Option<String>,
}

/// Picks what to keep from the page candidates: the first Recipe object any
/// HTML copy holds and the longest readable text. `Render::Browser` puts the
/// browser-rendered copy first for both.
fn choose(url: &str, mut pages: Vec<FetchedPage>, render: Render) -> Chosen {
    if render == Render::Browser {
        pages.sort_by_key(|page| page.source != "obscura");
    }
    let mut structured = None;
    let mut structured_from = None;
    for page in &pages {
        if let Some(html) = &page.html
            && let Some(found) = jsonld::recipe(html)
        {
            structured = Some(found);
            structured_from = Some(page);
            break;
        }
    }
    let readable = |page: &&FetchedPage| page.text.as_deref().is_some_and(|t| !t.trim().is_empty());
    let browser_text = (render == Render::Browser)
        .then(|| {
            pages
                .iter()
                .find(|page| page.source == "obscura" && readable(page))
        })
        .flatten();
    let longest = pages
        .iter()
        .filter(readable)
        .max_by_key(|page| page.text.as_deref().map_or(0, str::len));
    let mut text = browser_text
        .or(longest)
        .and_then(|page| page.text.clone())
        .unwrap_or_default();
    if text.trim().is_empty()
        && let Some(html) = structured_from
            .and_then(|page| page.html.as_deref())
            .or_else(|| pages.iter().find_map(|page| page.html.as_deref()))
    {
        text = html_text(html);
    }
    let text: String = text.trim().chars().take(MAX_STORED_TEXT).collect();
    // The page itself, or where the browser ended up; an archive's URL is
    // not where the recipe lives.
    let direct = pages
        .iter()
        .find(|page| {
            page.source
                == if render == Render::Browser {
                    "obscura"
                } else {
                    "fetch"
                }
        })
        .or_else(|| {
            pages
                .iter()
                .find(|page| matches!(page.source.as_str(), "fetch" | "obscura"))
        });
    let final_url = direct.map_or_else(|| url.to_owned(), |page| page.url.clone());
    let title = structured
        .as_ref()
        .map(|found| found.name.clone())
        .filter(|name| !name.is_empty())
        .or_else(|| pages.iter().find_map(|page| page.title.clone()))
        .map(|title| title.trim().chars().take(200).collect::<String>())
        .filter(|title| !title.is_empty());
    let image_url = structured
        .as_ref()
        .and_then(|found| found.images.first().cloned())
        .or_else(|| pages.iter().find_map(|page| page.lead_image_url.clone()));
    Chosen {
        final_url,
        title,
        structured,
        text,
        image_url,
    }
}

fn duplicates_of(conn: &Connection, urls: &[&str]) -> R<Vec<RecipeRef>> {
    let mut found: Vec<RecipeRef> = Vec::new();
    for url in urls.iter().filter_map(|url| normal_url(url)) {
        let mut statement = conn
            .prepare(
                "SELECT id, title FROM recipes_recipe
                 WHERE source_url = ?1 AND state != 'trashed' ORDER BY created_at",
            )
            .map_err(sql)?;
        let rows = statement
            .query_map(params![url], |row| {
                Ok(RecipeRef {
                    id: row.get(0)?,
                    title: row.get(1)?,
                })
            })
            .map_err(sql)?;
        for row in rows {
            let row = row.map_err(sql)?;
            if !found.contains(&row) {
                found.push(row);
            }
        }
    }
    Ok(found)
}

/// What one uploaded photo did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Upload {
    pub item_id: String,
    pub received: u32,
    pub complete: bool,
}

impl Recipes {
    /// Fetches an image from a public URL and stores it.
    pub(crate) async fn fetch_image(&self, raw: &str) -> R<Stored> {
        let online = self
            .online
            .as_ref()
            .ok_or_else(|| reject("fetch_failed", "fetching is not available here"))?;
        let url = url::Url::parse(raw.trim())
            .ok()
            .filter(|url| matches!(url.scheme(), "http" | "https"))
            .ok_or_else(|| invalid(format!("{raw:?} is not an http(s) URL")))?;
        let host = url.host_str().unwrap_or_default().to_owned();
        let response = online
            .public_http
            .request(omni_http::Method::GET, url)
            .timeout(IMAGE_FETCH_TIMEOUT)
            .header("accept", "image/*")
            .send_bounded(MAX_URL_IMAGE_BYTES)
            .await
            .map_err(|error| match error {
                omni_http::HttpError::TooLarge { .. } => reject(
                    "too_large",
                    format!("the image at {host} is over 15 MB; use a smaller rendition"),
                ),
                omni_http::HttpError::Timeout => reject(
                    "fetch_failed",
                    format!("the image at {host} could not be fetched: the host timed out"),
                ),
                // A status carries the response body; only the code is repeated.
                omni_http::HttpError::Status { status, .. } => reject(
                    "fetch_failed",
                    format!("the image at {host} could not be fetched: HTTP {status}"),
                ),
                other => reject(
                    "fetch_failed",
                    format!("the image at {host} could not be fetched: {other}"),
                ),
            })?;
        if !response.status.is_success() {
            return Err(reject(
                "fetch_failed",
                format!(
                    "{host} answered HTTP {} for the image",
                    response.status.as_u16()
                ),
            ));
        }
        let _held = self.files_lock.lock().await;
        self.files.put(Vec::from(response.body)).await
    }

    pub async fn import_url(&self, input: ImportUrlInput) -> R<ImportUrlOutput> {
        self.ready().await?;
        let url = url::Url::parse(input.url.trim())
            .ok()
            .filter(|url| matches!(url.scheme(), "http" | "https") && url.host_str().is_some())
            .ok_or_else(|| invalid("url must be a public http(s) URL"))?;
        let fetcher = self.ports.page_fetcher().ok_or_else(|| {
            reject(
                "fetch_failed",
                "page fetching is not available: the PressPods page fetcher is not running in this build",
            )
        })?;
        let pages = fetcher
            .fetch_pages(url.as_str())
            .await
            .map_err(|error| reject("fetch_failed", error.to_string()))?;
        let render = input.render.unwrap_or_default();
        let chosen = choose(url.as_str(), pages, render);
        if chosen.structured.is_none() && chosen.text.trim().is_empty() {
            return Err(reject(
                "fetch_failed",
                "the page was fetched but returned no recipe data and no readable text; try render \"browser\"",
            ));
        }
        // The image is a convenience: its failure never fails the import.
        let lead: Option<LeadImage> = match &chosen.image_url {
            Some(image) => self.fetch_image(image).await.ok().map(LeadImage::from),
            None => None,
        };
        let payload = json!({
            "url": url.as_str(),
            "finalUrl": chosen.final_url,
            "structured": chosen.structured,
            "text": chosen.text,
            "leadImage": lead,
            "render": render,
        });
        let id = ids::new_id(ids::INBOX);
        let (item_id, title, final_url, requested) = (
            id.clone(),
            chosen.title.clone(),
            chosen.final_url.clone(),
            url.to_string(),
        );
        let (seq, duplicate_of) = self
            .transact(false, move |conn, now| {
                room_for_item(conn)?;
                conn.execute(
                    "INSERT INTO recipes_inbox (id, kind, state, title, payload, created_at)
                     VALUES (?1, 'url', 'pending', ?2, ?3, ?4)",
                    params![item_id, title, payload.to_string(), now],
                )
                .map_err(sql)?;
                Ok((
                    next_seq(conn)?,
                    duplicates_of(conn, &[&final_url, &requested])?,
                ))
            })
            .await?;
        self.hub.library(seq);
        let (text, truncated, next_offset) = chunk(&chosen.text, 0);
        Ok(ImportUrlOutput {
            inbox_item_id: id,
            final_url: chosen.final_url,
            title: chosen.title,
            structured: chosen.structured,
            text,
            truncated,
            next_offset,
            image_stored: lead.is_some(),
            duplicate_of,
        })
    }

    pub async fn inbox_list(&self, input: InboxListInput) -> R<InboxListOutput> {
        let limit = input.limit.unwrap_or(25).clamp(1, 100);
        let cursor = input.cursor.unwrap_or(0);
        let state = input.state.unwrap_or(InboxState::Pending).as_str();
        let kind = input.kind.map(InboxKind::as_str);
        self.query(move |conn| {
            let total: u32 = conn
                .query_row(
                    "SELECT count(*) FROM recipes_inbox WHERE state = ?1 AND (?2 IS NULL OR kind = ?2)",
                    params![state, kind],
                    |row| row.get(0),
                )
                .map_err(sql)?;
            let mut statement = conn
                .prepare(&format!(
                    "SELECT {ITEM_COLUMNS} FROM recipes_inbox
                     WHERE state = ?1 AND (?2 IS NULL OR kind = ?2)
                     ORDER BY created_at DESC, id LIMIT ?3 OFFSET ?4"
                ))
                .map_err(sql)?;
            let rows = statement
                .query_map(params![state, kind, limit, cursor], item_from_row)
                .map_err(sql)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql)?;
            let mut items = Vec::new();
            for row in rows {
                items.push(row.info(file_count(conn, &row.id)?));
            }
            Ok(InboxListOutput {
                total,
                next_cursor: (cursor + limit < total).then_some(cursor + limit),
                items,
            })
        })
        .await
    }

    /// One item's stored content, and the JPEGs of the photos this call
    /// returns as image blocks.
    pub async fn inbox_get(&self, input: InboxGetInput) -> R<(InboxGetOutput, Vec<Vec<u8>>)> {
        let item_id = input.item_id.clone();
        let (item, all_files) = self
            .query(move |conn| {
                let item = visible_item(conn, &item_id)?;
                let files = files(conn, &item.id)?;
                Ok((item, files))
            })
            .await?;
        let offset = input.offset.unwrap_or(0) as usize;
        let (text, truncated, next_offset) = match item.text() {
            Some(text) => {
                let (part, truncated, next) = chunk(text, offset);
                (Some(part), truncated, next)
            }
            None => (None, false, None),
        };
        let max_edge = input.max_dimension.unwrap_or(1568).clamp(256, 2048);
        let first = input.photo_offset.unwrap_or(0) as usize;
        let mut photos = Vec::new();
        let mut blocks = Vec::new();
        for file in all_files.iter().skip(first).take(PHOTOS_PER_CALL) {
            let original = self
                .files
                .read(&file.sha256, Rendition::Original)
                .await?
                .ok_or_else(|| {
                    not_found(format!("photo {} of {} is missing", file.idx, item.id))
                })?;
            let image = crate::files::for_agent(original, max_edge).await?;
            photos.push(PhotoInfo {
                index: file.idx,
                width: image.width,
                height: image.height,
            });
            blocks.push(image.jpeg);
        }
        let shown = first + photos.len();
        let info = item.info(u32::try_from(all_files.len()).unwrap_or(u32::MAX));
        Ok((
            InboxGetOutput {
                item_id: info.item_id,
                kind: info.kind,
                title: info.title,
                added_at: info.added_at,
                files: info.files,
                state: info.state,
                recipe_id: info.recipe_id,
                url: item.url(),
                structured: item.structured(),
                text,
                truncated,
                next_offset,
                photos,
                next_photo_offset: (shown < all_files.len())
                    .then(|| u32::try_from(shown).unwrap_or(u32::MAX)),
            },
            blocks,
        ))
    }

    pub async fn inbox_resolve(&self, input: InboxResolveInput) -> R<InboxResolveOutput> {
        let state = match input.resolution {
            Resolution::Discarded => "discarded",
            Resolution::Duplicate => "duplicate",
        };
        if input.resolution == Resolution::Discarded && input.duplicate_of.is_some() {
            return Err(invalid("duplicateOf goes with resolution \"duplicate\""));
        }
        let (resolved, skipped, seq) = self
            .transact(false, move |conn, now| {
                let recipe = match &input.duplicate_of {
                    Some(id) => Some(recipe_for_read(conn, id)?.0.id),
                    None => None,
                };
                let mut resolved = 0;
                let mut skipped = Vec::new();
                for id in &input.item_ids {
                    let item = visible_item(conn, id)?;
                    if item.state != "pending" {
                        skipped.push(SkippedItem {
                            state: item.state(),
                            item_id: item.id,
                        });
                        continue;
                    }
                    conn.execute(
                        "UPDATE recipes_inbox SET state = ?2, resolved_at = ?3, recipe_id = ?4
                         WHERE id = ?1",
                        params![item.id, state, now, recipe],
                    )
                    .map_err(sql)?;
                    resolved += 1;
                }
                let seq = if resolved > 0 {
                    next_seq(conn)?
                } else {
                    db::current_seq(conn)?
                };
                Ok((resolved, skipped, seq))
            })
            .await?;
        if resolved > 0 {
            self.hub.library(seq);
        }
        Ok(InboxResolveOutput { resolved, skipped })
    }

    /// Stores photo `index` of `count` for the app's item `token`. The item
    /// becomes visible to agents, and `recipes.inbox_added` fires, when the
    /// last photo arrives. A repeated upload changes nothing.
    pub async fn upload_photo(
        &self,
        token: &str,
        index: u32,
        count: u32,
        title: Option<String>,
        bytes: Vec<u8>,
    ) -> R<Upload> {
        self.ready().await?;
        if !ids::is_client_token(token) {
            return Err(invalid("item must be 1 to 64 letters, digits, _ or -"));
        }
        if !(1..=MAX_PHOTOS).contains(&count) || index >= count {
            return Err(invalid(format!(
                "count must be 1 to {MAX_PHOTOS} and index below count"
            )));
        }
        if bytes.len() > MAX_PHOTO_BYTES {
            return Err(reject("too_large", "a photo may be at most 25 MB"));
        }
        let external = format!("app:{token}");
        // A new item needs room; a photo of an item already begun does not.
        let known = external.clone();
        self.query(move |conn| {
            let begun: i64 = conn
                .query_row(
                    "SELECT count(*) FROM recipes_inbox WHERE external_id = ?1",
                    params![known],
                    |row| row.get(0),
                )
                .map_err(sql)?;
            if begun == 0 {
                room_for_item(conn)?;
            }
            Ok(())
        })
        .await?;
        let _held = self.files_lock.lock().await;
        let stored = self.files.put(bytes).await?;
        let title = title
            .map(|title| title.trim().chars().take(200).collect::<String>())
            .filter(|title| !title.is_empty());
        let (upload, completed) = self
            .transact(false, move |conn, now| {
                let existing: Option<String> = conn
                    .query_row(
                        "SELECT id FROM recipes_inbox WHERE external_id = ?1",
                        params![external],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(sql)?;
                let id = match existing {
                    Some(id) => id,
                    None => {
                        room_for_item(conn)?;
                        let id = ids::new_id(ids::INBOX);
                        conn.execute(
                            "INSERT INTO recipes_inbox (id, kind, state, title, payload, external_id, created_at)
                             VALUES (?1, 'photo', 'uploading', ?2, ?3, ?4, ?5)",
                            params![id, title, json!({ "count": count }).to_string(), external, now],
                        )
                        .map_err(sql)?;
                        id
                    }
                };
                let item = item(conn, &id)?.ok_or_else(|| not_found("the item vanished"))?;
                let expected = item
                    .payload
                    .get("count")
                    .and_then(Value::as_u64)
                    .and_then(|count| u32::try_from(count).ok())
                    .unwrap_or(count);
                let mut completed = None;
                if item.state == "uploading" && index < expected {
                    conn.execute(
                        "INSERT OR IGNORE INTO recipes_inbox_file
                           (item_id, idx, sha256, mime, bytes, width, height, color)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                        params![
                            id,
                            index,
                            stored.sha256,
                            stored.mime,
                            stored.bytes,
                            stored.width,
                            stored.height,
                            stored.color
                        ],
                    )
                    .map_err(sql)?;
                    if file_count(conn, &id)? >= expected {
                        conn.execute(
                            "UPDATE recipes_inbox SET state = 'pending' WHERE id = ?1",
                            params![id],
                        )
                        .map_err(sql)?;
                        completed = Some((next_seq(conn)?, item.title.clone(), now));
                    }
                }
                let received = file_count(conn, &id)?;
                Ok((
                    Upload {
                        complete: item.state != "uploading" || completed.is_some(),
                        item_id: id,
                        received,
                    },
                    completed,
                ))
            })
            .await?;
        drop(_held);
        if let Some((seq, title, at)) = completed {
            self.hub.library(seq);
            events::publish(
                &self.ports,
                events::inbox_added(&upload.item_id, "photo", title.as_deref(), at),
            )
            .await;
        }
        Ok(upload)
    }
}
