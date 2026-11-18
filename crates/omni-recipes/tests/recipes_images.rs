//! Images and the inbox: adding images by bytes or from an inbox photo,
//! refusals, cover and order, removal with undo, photos captured in the app
//! arriving as image blocks, and resolving items.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::cloned_ref_to_slice_refs
)]

mod common;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use common::{Harness, PICCATA, image_blocks, jpeg, png, upload};
use omni_api::events::RECIPES_INBOX_ADDED;
use serde_json::{Value, json};

fn inline(bytes: &[u8], mime: &str) -> Value {
    json!({ "dataBase64": STANDARD.encode(bytes), "mimeType": mime })
}

fn image_ids(result: &Value) -> Vec<String> {
    result["images"]
        .as_array()
        .unwrap()
        .iter()
        .map(|image| image["imageId"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test(start_paused = true)]
async fn images_are_checked_by_their_bytes() {
    let h = Harness::new().await;
    let id = h.create(PICCATA).await;
    let add = |image: Value| json!({ "id": id, "add": [image] });

    let mismatch = h
        .err("recipes_images_edit", add(inline(&png(1), "image/jpeg")))
        .await;
    assert_eq!(
        mismatch,
        "[unsupported_media] mimeType says image/jpeg but the bytes are PNG; send the type the bytes have"
    );
    let gif = h
        .err(
            "recipes_images_edit",
            add(inline(b"GIF89a\x01\x00\x01\x00\x00\x00\x00;", "image/gif")),
        )
        .await;
    assert!(
        gif.starts_with(
            "[unsupported_media] expected a JPEG, PNG, WebP or HEIC image; the bytes are GIF"
        ),
        "{gif}"
    );
    let text = h
        .err(
            "recipes_images_edit",
            add(inline(b"<html>not an image</html>", "image/png")),
        )
        .await;
    assert!(text.starts_with("[unsupported_media]"), "{text}");
    let truncated = h
        .err(
            "recipes_images_edit",
            add(inline(&jpeg(1)[..200], "image/jpeg")),
        )
        .await;
    assert!(truncated.starts_with("[unsupported_media]"), "{truncated}");

    let mut big = jpeg(1);
    big.resize(3_000_001, 0);
    let too_large = h
        .err("recipes_images_edit", add(inline(&big, "image/jpeg")))
        .await;
    assert!(
        too_large.starts_with("[too_large] dataBase64 may hold at most 3,000,000 decoded bytes"),
        "{too_large}"
    );
    assert!(too_large.contains("2048 px"));

    for (image, why) in [
        (json!({ "dataBase64": STANDARD.encode(png(1)) }), "mimeType"),
        (json!({ "role": "dish" }), "exactly one"),
        (
            json!({ "url": "https://example.com/a.jpg", "dataBase64": "AAAA", "mimeType": "image/png" }),
            "exactly one",
        ),
        (
            json!({ "dataBase64": "not base64!", "mimeType": "image/png" }),
            "base64",
        ),
    ] {
        let refused = h.err("recipes_images_edit", add(image)).await;
        assert!(
            refused.starts_with("[validation]") && refused.contains(why),
            "{refused}"
        );
    }
    // The test app has no network: a URL fetch fails as a fetch failure.
    let offline = h
        .err(
            "recipes_images_edit",
            add(json!({ "url": "https://images.example.com/a.jpg" })),
        )
        .await;
    assert!(
        offline.starts_with("[fetch_failed] the image at images.example.com"),
        "{offline}"
    );
    let nothing = h.err("recipes_images_edit", json!({ "id": id })).await;
    assert!(nothing.starts_with("[validation]"), "{nothing}");

    // Nothing was added by any of it.
    assert_eq!(
        h.ok("recipes_get", json!({ "id": id })).await["images"],
        json!([])
    );
}

#[tokio::test(start_paused = true)]
async fn cover_order_removal_and_undo() {
    let h = Harness::new().await;
    let id = h.create(PICCATA).await;
    let added = h
        .ok(
            "recipes_images_edit",
            json!({ "id": id, "add": [
                inline(&png(1), "image/png"),
                { "dataBase64": STANDARD.encode(jpeg(2)), "mimeType": "image/jpg", "role": "step", "caption": "After searing" }
            ] }),
        )
        .await;
    let ids = image_ids(&added);
    assert_eq!(ids.len(), 2);
    assert!(ids[0].starts_with("img_"));
    // The first image is the cover until one is chosen.
    assert_eq!(added["images"][0]["cover"], true);
    assert_eq!(
        added["images"][1],
        json!({
            "imageId": ids[1], "width": 640, "height": 480, "role": "step", "caption": "After searing", "cover": false
        })
    );
    assert_eq!(
        h.ok("recipes_search", json!({ "hasImage": true })).await["total"],
        1
    );

    let arranged = h
        .ok(
            "recipes_images_edit",
            json!({ "id": id, "order": [ids[1], ids[0]], "cover": ids[1] }),
        )
        .await;
    assert_eq!(image_ids(&arranged), [ids[1].clone(), ids[0].clone()]);
    assert_eq!(arranged["images"][0]["cover"], true);
    let partial = h
        .err(
            "recipes_images_edit",
            json!({ "id": id, "order": [ids[0]] }),
        )
        .await;
    assert!(
        partial.starts_with("[validation] order must list every image id"),
        "{partial}"
    );
    let foreign = h
        .err(
            "recipes_images_edit",
            json!({ "id": id, "cover": "img_zzzzzzzz" }),
        )
        .await;
    assert!(foreign.starts_with("[not_found]"), "{foreign}");

    // An image block for the agent, bounded by maxDimension.
    let (meta, content) = h
        .call_full(
            "recipes_image_get",
            json!({ "imageId": ids[1], "maxDimension": 256 }),
        )
        .await
        .unwrap();
    assert_eq!(meta["recipeId"], id);
    assert_eq!(
        (meta["width"].clone(), meta["height"].clone()),
        (json!(256), json!(192))
    );
    assert_eq!(meta["caption"], "After searing");
    let blocks = image_blocks(&content);
    assert_eq!(blocks.len(), 1);
    let decoded = image::load_from_memory(&blocks[0]).unwrap();
    assert_eq!((decoded.width(), decoded.height()), (256, 192));
    assert_eq!(content[0]["type"], "text");

    // A dry-run removal keeps the image; a real one can be undone.
    let dry = h
        .ok(
            "recipes_images_edit",
            json!({ "id": id, "remove": [ids[1]], "dryRun": true }),
        )
        .await;
    assert_eq!(image_ids(&dry), [ids[0].clone()]);
    assert_eq!(
        h.ok("recipes_get", json!({ "id": id })).await["images"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let removed = h
        .ok(
            "recipes_images_edit",
            json!({ "id": id, "remove": [ids[1]] }),
        )
        .await;
    assert_eq!(image_ids(&removed), [ids[0].clone()]);
    assert_eq!(
        removed["images"][0]["cover"], true,
        "the cover falls back to the first image"
    );
    assert!(
        h.err("recipes_image_get", json!({ "imageId": ids[1] }))
            .await
            .starts_with("[not_found]")
    );

    // Twenty days on, a purge has run: the file is kept for the undo.
    h.advance_days(20).await;
    assert_eq!(h.recipes.purge().await.unwrap().files, 0);
    h.ok("recipes_undo", json!({ "changeId": removed["changeId"] }))
        .await;
    let back = h.ok("recipes_get", json!({ "id": id })).await;
    assert_eq!(image_ids(&back), [ids[1].clone(), ids[0].clone()]);
    assert_eq!(back["images"][0]["cover"], true);
    assert_eq!(back["images"][0]["caption"], "After searing");
    assert!(
        h.call_full("recipes_image_get", json!({ "imageId": ids[1] }))
            .await
            .is_ok()
    );
}

#[tokio::test(start_paused = true)]
async fn photos_captured_in_the_app_reach_agents_as_image_blocks() {
    let h = Harness::new().await;
    let router = h.router();
    let path = |index: u32| {
        format!("/api/inbox?item=capture-7&index={index}&count=5&title=Nonna%27s%20card")
    };

    // Until the last photo arrives the item does not exist for agents.
    for index in [0, 1, 2, 3] {
        let reply = upload(&router, &path(index), jpeg(index as u8)).await;
        assert_eq!(reply.status, 200, "{}", reply.text());
        assert_eq!(reply.json()["received"], index + 1);
        assert_eq!(reply.json()["complete"], false);
    }
    // A retried upload changes nothing.
    let retry = upload(&router, &path(1), jpeg(1)).await;
    assert_eq!(retry.json()["received"], 4);
    assert_eq!(h.ok("recipes_inbox_list", json!({})).await["total"], 0);
    assert!(h.events.published().is_empty());
    let item_id = retry.json()["itemId"].as_str().unwrap().to_owned();
    assert!(
        h.err("recipes_inbox_get", json!({ "itemId": item_id }))
            .await
            .starts_with("[not_found]")
    );

    let last = upload(&router, &path(4), png(4)).await;
    assert_eq!(
        last.json(),
        json!({ "itemId": item_id, "received": 5, "complete": true })
    );
    let again = upload(&router, &path(4), png(4)).await;
    assert_eq!(again.json()["complete"], true);

    // One event, with a schema-shaped payload.
    let published = h.events.published();
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].name, RECIPES_INBOX_ADDED);
    assert_eq!(published[0].dedup_key, format!("inbox:{item_id}"));
    assert_eq!(
        Value::Object(published[0].data.clone()),
        json!({ "itemId": item_id, "kind": "photo", "title": "Nonna's card" })
    );

    let list = h.ok("recipes_inbox_list", json!({})).await;
    assert_eq!(list["items"][0]["itemId"], item_id);
    assert_eq!(list["items"][0]["files"], 5);
    assert_eq!(list["items"][0]["kind"], "photo");
    assert_eq!(list["items"][0]["title"], "Nonna's card");
    assert_eq!(
        h.ok("recipes_inbox_list", json!({ "kind": "url" })).await["total"],
        0
    );

    // Four photos per call, then the rest.
    let (first, content) = h
        .call_full(
            "recipes_inbox_get",
            json!({ "itemId": item_id, "maxDimension": 320 }),
        )
        .await
        .unwrap();
    assert_eq!(first["photos"].as_array().unwrap().len(), 4);
    assert_eq!(
        first["photos"][0],
        json!({ "index": 0, "width": 320, "height": 240 })
    );
    assert_eq!(first["nextPhotoOffset"], 4);
    assert_eq!(first["truncated"], false);
    let blocks = image_blocks(&content);
    assert_eq!(blocks.len(), 4);
    assert_eq!(image::load_from_memory(&blocks[0]).unwrap().width(), 320);
    let (rest, content) = h
        .call_full(
            "recipes_inbox_get",
            json!({ "itemId": item_id, "photoOffset": 4 }),
        )
        .await
        .unwrap();
    assert_eq!(
        rest["photos"],
        json!([{ "index": 4, "width": 320, "height": 200 }])
    );
    assert!(rest.get("nextPhotoOffset").is_none());
    assert_eq!(image_blocks(&content).len(), 1);

    // An inbox photo can become a recipe image without creating from the item.
    let id = h.create(PICCATA).await;
    let added = h
        .ok(
            "recipes_images_edit",
            json!({ "id": id, "add": [{ "inbox": { "itemId": item_id, "index": 2 }, "role": "source" }] }),
        )
        .await;
    assert_eq!(added["images"][0]["role"], "source");
    let missing = h
        .err(
            "recipes_images_edit",
            json!({ "id": id, "add": [{ "inbox": { "itemId": item_id, "index": 9 } }] }),
        )
        .await;
    assert!(missing.starts_with("[not_found]"), "{missing}");

    // Resolving: discarded or duplicate, once.
    let resolved = h
        .ok(
            "recipes_inbox_resolve",
            json!({ "itemIds": [item_id], "resolution": "duplicate", "duplicateOf": id }),
        )
        .await;
    assert_eq!(resolved["resolved"], 1);
    let twice = h
        .ok(
            "recipes_inbox_resolve",
            json!({ "itemIds": [item_id], "resolution": "discarded" }),
        )
        .await;
    assert_eq!(twice["resolved"], 0);
    assert_eq!(h.ok("recipes_inbox_list", json!({})).await["total"], 0);
    let duplicates = h
        .ok("recipes_inbox_list", json!({ "state": "duplicate" }))
        .await;
    assert_eq!(duplicates["items"][0]["recipeId"], id);
    let unknown = h
        .err(
            "recipes_inbox_resolve",
            json!({ "itemIds": ["inb_zzzzzzzz"], "resolution": "discarded" }),
        )
        .await;
    assert!(unknown.starts_with("[not_found]"), "{unknown}");
}

#[tokio::test(start_paused = true)]
async fn uploads_are_bounded_and_typed_by_their_bytes() {
    let h = Harness::new().await;
    let router = h.router();
    let not_image = upload(
        &router,
        "/api/inbox?item=a&index=0&count=1",
        b"%PDF-1.7 not a photo".to_vec(),
    )
    .await;
    assert_eq!(not_image.status, 415);
    assert!(
        not_image.json()["error"]
            .as_str()
            .unwrap()
            .starts_with("[unsupported_media]")
    );
    for bad in [
        "/api/inbox?item=a&index=1&count=1",
        "/api/inbox?item=a&index=0&count=21",
        "/api/inbox?item=a&index=0&count=0",
        "/api/inbox?item=bad%20name&index=0&count=1",
    ] {
        let reply = upload(&router, bad, jpeg(1)).await;
        assert_eq!(reply.status, 400, "{bad}: {}", reply.text());
    }
    let missing = upload(&router, "/api/inbox?item=a", jpeg(1)).await;
    assert_eq!(missing.status, 400);
    let mut huge = jpeg(1);
    huge.resize(25 * 1024 * 1024 + 1, 0);
    let too_large = upload(&router, "/api/inbox?item=a&index=0&count=1", huge).await;
    assert_eq!(too_large.status, 413);
    assert_eq!(h.ok("recipes_inbox_list", json!({})).await["total"], 0);
    assert!(h.events.published().is_empty());

    // A stale upload that never completes is dropped by the purge.
    let partial = upload(&router, "/api/inbox?item=b&index=0&count=3", jpeg(5)).await;
    assert_eq!(partial.status, 200);
    h.advance_days(2).await;
    let purged = h.recipes.purge().await.unwrap();
    assert_eq!(purged.inbox_items, 1);
    assert_eq!(purged.files, 1);
}
