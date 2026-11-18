//! Recipe images: adding (from a URL, an inbox photo or bytes in the call),
//! removing, ordering and choosing the cover, and reading one for an agent.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use omni_image::ImageKind;
use rusqlite::{Connection, params};
use serde_json::json;

use crate::api::{
    ImageAdd, ImageGetInput, ImageGetOutput, ImageInfo, ImagesEditInput, ImagesEditOutput,
};
use crate::db::{
    self, Actor, Change, ImageRow, image_infos, images_of, live_recipe_for_write, log_change,
    next_seq,
};
use crate::error::{R, invalid, not_found, reject, sql};
use crate::files::{Rendition, Stored, accepted_kind};
use crate::ids;
use crate::inbox;
use crate::meta::{self, IMAGES};
use crate::service::Recipes;

/// Decoded bytes of an image passed in a tool call.
pub const MAX_INLINE_BYTES: usize = 3_000_000;

struct NewImage {
    stored: Stored,
    role: &'static str,
    caption: Option<String>,
    origin: String,
}

fn images_edit_tx(
    conn: &Connection,
    now: i64,
    input: &ImagesEditInput,
    added: Vec<NewImage>,
    actor: &Actor,
) -> R<(String, u64, Vec<ImageInfo>)> {
    let row = live_recipe_for_write(conn, &input.id)?;
    let ids = [row.id.clone()];
    let before = json!({ "recipes": meta::recipes_facets(conn, &ids, &[IMAGES])? });
    let mut said: Vec<String> = Vec::new();
    let mut position = images_of(conn, &row.id)?
        .iter()
        .map(|image| image.position + 1)
        .max()
        .unwrap_or(0);
    for new in added {
        db::insert_image(
            conn,
            &ImageRow {
                id: ids::new_id(ids::IMAGE),
                recipe_id: row.id.clone(),
                sha256: new.stored.sha256,
                mime: new.stored.mime,
                width: new.stored.width,
                height: new.stored.height,
                bytes: new.stored.bytes,
                role: new.role.to_owned(),
                caption: new.caption,
                origin: new.origin,
                color: new.stored.color,
                position,
                created_at: now,
            },
        )?;
        position += 1;
    }
    if position > 100 {
        return Err(invalid("a recipe holds at most 100 images"));
    }
    let belongs = |image_id: &str| -> R<()> {
        match db::image(conn, image_id)? {
            Some(image) if image.recipe_id == row.id => Ok(()),
            _ => Err(not_found(format!("{} has no image {image_id:?}", row.id))),
        }
    };
    for image_id in input.remove.as_deref().unwrap_or_default() {
        belongs(image_id)?;
        conn.execute("DELETE FROM recipes_image WHERE id = ?1", params![image_id])
            .map_err(sql)?;
        conn.execute(
            "UPDATE recipes_recipe SET cover_image = NULL WHERE id = ?1 AND cover_image = ?2",
            params![row.id, image_id],
        )
        .map_err(sql)?;
        said.push(format!("removed {image_id}"));
    }
    if let Some(order) = &input.order {
        let mut current: Vec<String> = images_of(conn, &row.id)?
            .into_iter()
            .map(|image| image.id)
            .collect();
        let mut wanted = order.clone();
        current.sort();
        wanted.sort();
        wanted.dedup();
        if current != wanted || wanted.len() != order.len() {
            return Err(invalid(
                "order must list every image id of the recipe exactly once (after adds and removes)",
            ));
        }
        for (index, image_id) in order.iter().enumerate() {
            conn.execute(
                "UPDATE recipes_image SET position = ?2 WHERE id = ?1",
                params![image_id, u32::try_from(index).unwrap_or(u32::MAX)],
            )
            .map_err(sql)?;
        }
        said.push("reordered".to_owned());
    }
    if let Some(cover) = &input.cover {
        belongs(cover)?;
        conn.execute(
            "UPDATE recipes_recipe SET cover_image = ?2 WHERE id = ?1",
            params![row.id, cover],
        )
        .map_err(sql)?;
        said.push(format!("cover {cover}"));
    }
    let seq = next_seq(conn)?;
    db::touch(conn, &row.id, seq, now)?;
    let after = json!({ "recipes": meta::recipes_facets(conn, &ids, &[IMAGES])? });
    let added_count = after["recipes"][&row.id][IMAGES]["rows"]
        .as_array()
        .map_or(0, Vec::len);
    let change = log_change(
        conn,
        seq,
        now,
        &Change {
            actor,
            summary: format!(
                "Images of {}: {added_count} now{}{}",
                row.title,
                if said.is_empty() { "" } else { "; " },
                said.join(", ")
            ),
            kind: "meta",
            touched: json!({ row.id.clone(): [IMAGES] }),
            before,
            after,
        },
    )?;
    let fresh = db::recipe(conn, &row.id)?.unwrap_or(row);
    let images = image_infos(&fresh, &images_of(conn, &fresh.id)?);
    Ok((change, seq, images))
}

impl Recipes {
    async fn new_image(&self, add: &ImageAdd) -> R<NewImage> {
        let role = add.role.map_or("dish", crate::api::ImageRole::as_str);
        let caption = add
            .caption
            .as_ref()
            .map(|caption| caption.trim().to_owned())
            .filter(|caption| !caption.is_empty());
        let given = usize::from(add.url.is_some())
            + usize::from(add.data_base64.is_some())
            + usize::from(add.inbox.is_some());
        if given != 1 {
            return Err(invalid(
                "each added image needs exactly one of url, dataBase64 and inbox",
            ));
        }
        if let Some(url) = &add.url {
            let host = url::Url::parse(url)
                .ok()
                .and_then(|url| url.host_str().map(str::to_owned))
                .unwrap_or_default();
            return Ok(NewImage {
                stored: self.fetch_image(url).await?,
                role,
                caption,
                origin: format!("url:{host}"),
            });
        }
        if let Some(data) = &add.data_base64 {
            // Four base64 characters carry three bytes.
            if data.len() / 4 * 3 > MAX_INLINE_BYTES + 3 {
                return Err(too_large());
            }
            let bytes = STANDARD
                .decode(data.trim())
                .map_err(|_| invalid("dataBase64 is not valid base64"))?;
            if bytes.len() > MAX_INLINE_BYTES {
                return Err(too_large());
            }
            let declared = add
                .mime_type
                .as_deref()
                .ok_or_else(|| invalid("dataBase64 needs mimeType"))?;
            let kind = accepted_kind(&bytes)?;
            if ImageKind::from_mime(declared) != Some(kind) {
                return Err(reject(
                    "unsupported_media",
                    format!(
                        "mimeType says {declared} but the bytes are {}; send the type the bytes have",
                        kind.label()
                    ),
                ));
            }
            let _held = self.files_lock.lock().await;
            return Ok(NewImage {
                stored: self.files.put(bytes).await?,
                role,
                caption,
                origin: "inline".to_owned(),
            });
        }
        let photo = add
            .inbox
            .clone()
            .ok_or_else(|| invalid("each added image needs a source"))?;
        let (item_id, index) = (photo.item_id.clone(), photo.index);
        let stored = self
            .query(move |conn| {
                let item = inbox::visible_item(conn, &item_id)?;
                if let Some(lead) = item
                    .lead_image()
                    .filter(|_| index == 0 && item.kind == "url")
                {
                    return Ok(Stored {
                        sha256: lead.sha256,
                        mime: lead.mime,
                        bytes: lead.bytes,
                        width: lead.width,
                        height: lead.height,
                        color: lead.color,
                    });
                }
                inbox::files(conn, &item.id)?
                    .into_iter()
                    .find(|file| file.idx == index)
                    .map(|file| Stored {
                        sha256: file.sha256,
                        mime: file.mime,
                        bytes: file.bytes,
                        width: file.width,
                        height: file.height,
                        color: file.color,
                    })
                    .ok_or_else(|| not_found(format!("{} has no photo {index}", item.id)))
            })
            .await?;
        Ok(NewImage {
            stored,
            role,
            caption,
            origin: format!("inbox:{}:{}", photo.item_id, photo.index),
        })
    }

    pub async fn images_edit(&self, input: ImagesEditInput, actor: Actor) -> R<ImagesEditOutput> {
        self.ready().await?;
        let dry = input.dry_run.unwrap_or(false);
        let adds = input.add.as_deref().unwrap_or_default();
        if adds.is_empty()
            && input.remove.as_ref().is_none_or(Vec::is_empty)
            && input.order.is_none()
            && input.cover.is_none()
        {
            return Err(invalid("pass add, remove, order or cover"));
        }
        // Refuse a merged or missing recipe before fetching anything.
        let id = input.id.clone();
        self.query(move |conn| live_recipe_for_write(conn, &id).map(|_| ()))
            .await?;
        let mut added = Vec::new();
        for add in adds {
            added.push(self.new_image(add).await?);
        }
        let (change, seq, images) = self
            .transact(dry, move |conn, now| {
                images_edit_tx(conn, now, &input, added, &actor)
            })
            .await?;
        if !dry {
            self.hub.library(seq);
        }
        Ok(ImagesEditOutput {
            change_id: (!dry).then_some(change),
            images,
        })
    }

    /// One image's metadata and its JPEG for an image block.
    pub async fn image_get(&self, input: ImageGetInput) -> R<(ImageGetOutput, Vec<u8>)> {
        let image_id = input.image_id.clone();
        let image = self
            .query(move |conn| {
                db::image(conn, &image_id)?
                    .ok_or_else(|| not_found(format!("no image has id {image_id:?}")))
            })
            .await?;
        let original = self
            .files
            .read(&image.sha256, Rendition::Original)
            .await?
            .ok_or_else(|| not_found(format!("the file of {} is missing", image.id)))?;
        let max_edge = input.max_dimension.unwrap_or(1024).clamp(256, 2048);
        let rendition = crate::files::for_agent(original, max_edge).await?;
        Ok((
            ImageGetOutput {
                image_id: image.id,
                recipe_id: image.recipe_id,
                width: rendition.width,
                height: rendition.height,
                role: image.role,
                caption: image.caption,
                mime_type: "image/jpeg".to_owned(),
            },
            rendition.jpeg,
        ))
    }
}

fn too_large() -> crate::error::RecipesError {
    reject(
        "too_large",
        "dataBase64 may hold at most 3,000,000 decoded bytes; resize the image to about 2048 px on the long edge, or pass a url or an inbox photo",
    )
}
