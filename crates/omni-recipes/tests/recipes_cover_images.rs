//! Every recipe gets a cover: a write without a picture of the dish says so,
//! a freely licensed stand-in is marked as a placeholder until the owner's
//! own photo replaces it, and a Commons file link that redirects is fetched.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::common;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use common::{COOKIES, Harness, PICCATA, doc, jpeg};
use omni_http::public::PublicHttpClient;
use omni_runtime::AppContext;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

const PLACEHOLDER: &str =
    "Placeholder: Lemon cutlets by A. Photographer, Wikimedia Commons, CC BY-SA 4.0";

fn codes(warnings: &Value) -> Vec<&str> {
    warnings
        .as_array()
        .unwrap()
        .iter()
        .map(|warning| warning["code"].as_str().unwrap())
        .collect()
}

fn photo(seed: u8, role: &str, caption: Option<&str>) -> Value {
    let mut add = json!({
        "dataBase64": STANDARD.encode(jpeg(seed)), "mimeType": "image/jpeg", "role": role,
    });
    if let Some(caption) = caption {
        add["caption"] = json!(caption);
    }
    add
}

#[tokio::test(start_paused = true)]
async fn a_write_without_a_picture_of_the_dish_warns_until_one_is_added() {
    let h = Harness::new().await;
    let created = h
        .ok(
            "recipes_create",
            json!({ "keepDietConflicts": true, "document": doc(PICCATA) }),
        )
        .await;
    assert!(codes(&created["warnings"]).contains(&"missing_image"));
    let warning = created["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|warning| warning["code"] == "missing_image")
        .unwrap();
    assert_eq!(warning["level"], "warning");
    assert!(warning["hint"].as_str().unwrap().contains("Placeholder:"));
    let id = created["id"].as_str().unwrap().to_owned();

    // A photo of the source page is not a picture of the dish.
    h.ok(
        "recipes_images_edit",
        json!({ "id": id, "add": [photo(1, "source", None)] }),
    )
    .await;
    let replace = |revision: u32| {
        json!({
            "id": id, "expectedRevision": revision, "summary": "Same",
            "keepDietConflicts": true, "document": doc(PICCATA), "review": false,
        })
    };
    let still = h.ok("recipes_replace", replace(1)).await;
    assert!(codes(&still["warnings"]).contains(&"missing_image"));

    h.ok(
        "recipes_images_edit",
        json!({ "id": id, "add": [photo(2, "dish", None)] }),
    )
    .await;
    let pictured = h.ok("recipes_replace", replace(1)).await;
    assert!(!codes(&pictured["warnings"]).contains(&"missing_image"));
}

#[tokio::test(start_paused = true)]
async fn a_placeholder_cover_is_marked_and_found_until_the_owners_photo_replaces_it() {
    let h = Harness::new().await;
    let id = h.create(PICCATA).await;
    let other = h.create(COOKIES).await;
    let added = h
        .ok(
            "recipes_images_edit",
            json!({ "id": id, "add": [photo(3, "dish", Some(PLACEHOLDER))] }),
        )
        .await;
    assert_eq!(added["images"][0]["placeholder"], true);
    assert_eq!(added["images"][0]["cover"], true);

    let search = |placeholder: bool| json!({ "placeholderImage": placeholder, "state": "active" });
    let found = h.ok("recipes_search", search(true)).await;
    assert_eq!(found["total"], 1);
    assert_eq!(found["recipes"][0]["id"], id.as_str());
    assert_eq!(found["recipes"][0]["coverPlaceholder"], true);
    let rest = h.ok("recipes_search", search(false)).await;
    assert_eq!(rest["total"], 1);
    assert_eq!(rest["recipes"][0]["id"], other.as_str());
    assert!(rest["recipes"][0].get("coverPlaceholder").is_none());

    // The owner's photo becomes the cover and the placeholder goes.
    let owned = h
        .ok(
            "recipes_images_edit",
            json!({ "id": id, "add": [photo(4, "dish", Some("Sunday dinner"))] }),
        )
        .await;
    let placeholder = owned["images"]
        .as_array()
        .unwrap()
        .iter()
        .find(|image| image["placeholder"] == true)
        .unwrap()["imageId"]
        .as_str()
        .unwrap()
        .to_owned();
    let own = owned["images"]
        .as_array()
        .unwrap()
        .iter()
        .find(|image| image.get("placeholder").is_none())
        .unwrap()["imageId"]
        .as_str()
        .unwrap()
        .to_owned();
    let mid = h.ok("recipes_search", search(true)).await;
    assert_eq!(mid["total"], 1, "the placeholder is still the cover");
    h.ok(
        "recipes_images_edit",
        json!({ "id": id, "cover": own, "remove": [placeholder] }),
    )
    .await;
    assert_eq!(h.ok("recipes_search", search(true)).await["total"], 0);
    assert_eq!(h.ok("recipes_search", search(false)).await["total"], 2);
}

// Real time: the image is fetched over a local socket.
#[tokio::test]
async fn a_commons_file_link_is_followed_to_the_file() {
    let server = omni_testkit::mock_server().await;
    Mock::given(method("GET"))
        .and(path("/wiki/Special:FilePath/Lemon_cutlets.jpg"))
        // Commons sends the file from upload.wikimedia.org; the mock client
        // rewrites only the first host, so the hop stays on this one.
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", "/wikipedia/commons/a/ab/Lemon_cutlets.jpg"),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/wikipedia/commons/a/ab/Lemon_cutlets.jpg"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "image/jpeg")
                .set_body_bytes(jpeg(5)),
        )
        .mount(&server)
        .await;
    let http = omni_testkit::mock_http(&server, &["https://commons.wikimedia.org"]);
    let h = Harness::with(move |ctx| AppContext {
        public_http: PublicHttpClient::new(&http).allow_loopback_for_tests(),
        ..ctx
    })
    .await;
    let id = h.create(PICCATA).await;
    let added = h
        .ok(
            "recipes_images_edit",
            json!({ "id": id, "add": [{
                "url": "https://commons.wikimedia.org/wiki/Special:FilePath/Lemon_cutlets.jpg",
                "caption": PLACEHOLDER,
            }] }),
        )
        .await;
    assert_eq!(added["images"][0]["placeholder"], true);
    assert_eq!(added["images"][0]["width"], 640);
}
