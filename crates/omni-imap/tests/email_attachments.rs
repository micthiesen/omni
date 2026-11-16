//! Private email PDF and image retrieval through MCP, with a scripted
//! attachment reader. Every image is generated here, except the bundled
//! synthetic HEIC gradient (`HEIC_SELF_TEST`).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Cursor;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use futures::future::BoxFuture;
use image::{DynamicImage, ImageFormat, RgbImage};
use omni_core::clock::{SharedClock, TestClock};
use omni_imap::archive_service::ArchiveService;
use omni_imap::compose::ComposeService;
use omni_imap::mcp_tools::{AttachmentReader, ToolDeps, email_tools};
use omni_imap::protocol::ImapError;
use omni_imap::transport::DownloadedAttachment;
use omni_mcp_kit::{McpTool, ToolContext, ToolError, ToolOutput};
use omni_testkit::TestStore;
use serde_json::{Map, Value, json};
use sha2::{Digest as _, Sha256};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

const MESSAGE_ID: &str = "<attachment@example.test>";
const DATA: &[u8] = b"%PDF-1.7\nsynthetic fixture";

struct Reader {
    result: Mutex<Option<DownloadedAttachment>>,
    calls: Mutex<Vec<(String, String, usize)>>,
}

impl AttachmentReader for Reader {
    fn fetch_attachment<'a>(
        &'a self,
        message_id: &'a str,
        attachment_id: &'a str,
        max_bytes: usize,
    ) -> BoxFuture<'a, Result<Option<DownloadedAttachment>, ImapError>> {
        self.calls.lock().unwrap().push((
            message_id.to_owned(),
            attachment_id.to_owned(),
            max_bytes,
        ));
        let result = self.result.lock().unwrap().clone();
        Box::pin(async move { Ok(result) })
    }
}

fn pdf(name: &str, mime: &str, data: &[u8]) -> Option<DownloadedAttachment> {
    Some(DownloadedAttachment {
        name: name.to_owned(),
        mime_type: mime.to_owned(),
        data: data.to_vec(),
    })
}

async fn fixture(result: Option<DownloadedAttachment>) -> (McpTool, Arc<Reader>, TestStore) {
    let clock: SharedClock = TestClock::new(1_790_769_600_000);
    let store = TestStore::new(clock.clone()).await;
    let reader = Arc::new(Reader {
        result: Mutex::new(result),
        calls: Mutex::new(Vec::new()),
    });
    let tools = email_tools(ToolDeps {
        compose: ComposeService::new(store.store.clone(), clock.clone(), None, None),
        archive: ArchiveService::new(store.store.clone(), clock),
        archive_transport: None,
        attachments: Some(reader.clone() as Arc<dyn AttachmentReader>),
        tracker: TaskTracker::new(),
    })
    .unwrap();
    let tool = tools
        .into_iter()
        .find(|t| t.meta.name == "email_attachment_get")
        .unwrap();
    (tool, reader, store)
}

async fn call(tool: &McpTool, input: Value) -> Result<ToolOutput, ToolError> {
    tool.handler
        .call(
            input,
            ToolContext {
                call_id: "t".to_owned(),
                cancel: CancellationToken::new(),
            },
        )
        .await
}

#[tokio::test]
async fn returns_an_embedded_binary_resource_and_safe_metadata_without_a_public_url() {
    let (tool, reader, _store) = fixture(pdf("../private.pdf", "application/pdf", DATA)).await;
    let output = call(
        &tool,
        json!({"messageId": MESSAGE_ID, "attachmentId": "stable-id"}),
    )
    .await
    .unwrap();
    assert_eq!(
        *reader.calls.lock().unwrap(),
        vec![(
            MESSAGE_ID.to_owned(),
            "stable-id".to_owned(),
            5 * 1024 * 1024
        )]
    );
    let ToolOutput::Custom {
        structured,
        content,
    } = output
    else {
        panic!("expected custom content");
    };
    assert_eq!(content[0]["type"], json!("text"));
    assert!(
        !content[0]["text"]
            .as_str()
            .unwrap()
            .contains(&STANDARD.encode(DATA))
    );
    let mut resource = Map::new();
    resource.insert("type".to_owned(), json!("resource"));
    resource.insert(
        "resource".to_owned(),
        json!({"uri": "omni-email-attachment:stable-id", "mimeType": "application/pdf", "blob": STANDARD.encode(DATA)}),
    );
    assert_eq!(content[1], resource);
    let filename = structured["filename"].as_str().unwrap();
    assert!(!filename.contains(['\\', '/']) && !filename.chars().any(|c| (c as u32) < 0x20));
    assert_eq!(structured["size"], json!(DATA.len()));
    let sha = structured["sha256"].as_str().unwrap();
    assert!(sha.len() == 64 && sha.chars().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(structured["blob"], json!(STANDARD.encode(DATA)));
    assert_eq!(structured["mimeType"], json!("application/pdf"));
    assert_eq!(
        structured["attachmentReference"],
        json!({"messageId": MESSAGE_ID, "attachmentId": "stable-id", "sha256": sha}),
    );
    assert!(!structured.contains_key("image"));
    let mut metadata = structured.clone();
    metadata.remove("blob");
    assert_eq!(
        serde_json::from_str::<Value>(content[0]["text"].as_str().unwrap()).unwrap(),
        Value::Object(metadata)
    );
    assert_eq!(content.len(), 2);
}

#[tokio::test]
async fn rejects_excessive_bounds_and_header_injection_before_touching_imap() {
    let (tool, reader, _store) = fixture(pdf("x.pdf", "application/pdf", DATA)).await;
    for input in [
        json!({"messageId": MESSAGE_ID, "attachmentId": "id", "maxBytes": 5 * 1024 * 1024 + 1}),
        json!({"messageId": "<id>\r\nInjected: header", "attachmentId": "id"}),
        json!({"messageId": MESSAGE_ID, "attachmentId": "id", "destination": "/tmp/file.pdf"}),
    ] {
        assert!(call(&tool, input).await.is_err());
    }
    assert!(reader.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn rejects_missing_parts_mismatched_mime_invalid_pdf_bytes_and_decoded_oversize() {
    for result in [
        None,
        pdf("x.pdf", "text/html", DATA),
        pdf("x.pdf", "application/pdf", b"not pdf"),
    ] {
        let (tool, _, _store) = fixture(result).await;
        assert!(
            call(
                &tool,
                json!({"messageId": MESSAGE_ID, "attachmentId": "id"})
            )
            .await
            .is_err()
        );
    }
    let (tool, _, _store) = fixture(pdf("x.pdf", "application/pdf", DATA)).await;
    let error = call(
        &tool,
        json!({"messageId": MESSAGE_ID, "attachmentId": "id", "maxBytes": 5}),
    )
    .await
    .unwrap_err();
    assert!(error.message.contains("limit"), "{}", error.message);
}

const HEIC: &[u8] = omni_imap::attachment_images::HEIC_SELF_TEST;

fn gradient(width: u32, height: u32) -> DynamicImage {
    DynamicImage::ImageRgb8(RgbImage::from_fn(width, height, |x, y| {
        image::Rgb([
            u8::try_from(x * 255 / width).unwrap(),
            u8::try_from(y * 255 / height).unwrap(),
            u8::try_from((x / 8 + y / 8) % 2 * 200).unwrap(),
        ])
    }))
}

fn encoded(image: &DynamicImage, format: ImageFormat) -> Vec<u8> {
    let mut out = Cursor::new(Vec::new());
    image.write_to(&mut out, format).unwrap();
    out.into_inner()
}

/// A JPEG whose EXIF says "rotate 90 degrees clockwise to display".
fn jpeg_with_orientation_6(width: u32, height: u32) -> Vec<u8> {
    let jpeg = encoded(&gradient(width, height), ImageFormat::Jpeg);
    let mut tiff = b"Exif\0\0MM\0*\0\0\0\x08\0\x01".to_vec();
    tiff.extend_from_slice(&[0x01, 0x12, 0, 3, 0, 0, 0, 1, 0, 6, 0, 0, 0, 0, 0, 0]);
    let mut out = jpeg[..2].to_vec();
    out.extend_from_slice(&[0xff, 0xe1]);
    out.extend_from_slice(&u16::try_from(tiff.len() + 2).unwrap().to_be_bytes());
    out.extend_from_slice(&tiff);
    out.extend_from_slice(&jpeg[2..]);
    out
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

/// A PNG that declares its size and holds no pixel data: the header is
/// enough to refuse it.
fn png_header(width: u32, height: u32) -> Vec<u8> {
    let mut chunk = b"IHDR".to_vec();
    chunk.extend_from_slice(&width.to_be_bytes());
    chunk.extend_from_slice(&height.to_be_bytes());
    chunk.extend_from_slice(&[8, 0, 0, 0, 0]);
    let mut out = b"\x89PNG\r\n\x1a\n\0\0\0\x0d".to_vec();
    out.extend_from_slice(&chunk);
    out.extend_from_slice(&crc32(&chunk).to_be_bytes());
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.extend_from_slice(b"IDAT");
    out.extend_from_slice(&crc32(b"IDAT").to_be_bytes());
    out
}

struct ImageResult {
    structured: Map<String, Value>,
    bytes: Vec<u8>,
    block_mime: String,
}

impl ImageResult {
    fn image(&self) -> &Value {
        &self.structured["image"]
    }

    fn decoded(&self) -> DynamicImage {
        image::load_from_memory(&self.bytes).unwrap()
    }
}

async fn image_call(name: &str, mime: &str, data: &[u8], input: Value) -> ImageResult {
    let (tool, _, _store) = fixture(pdf(name, mime, data)).await;
    let ToolOutput::Custom {
        structured,
        content,
    } = call(&tool, input).await.unwrap()
    else {
        panic!("expected custom content");
    };
    assert_eq!(content.len(), 2);
    assert_eq!(content[0]["type"], json!("text"));
    assert_eq!(
        serde_json::from_str::<Value>(content[0]["text"].as_str().unwrap()).unwrap(),
        Value::Object(structured.clone())
    );
    assert_eq!(content[1]["type"], json!("image"));
    assert_eq!(content[1].len(), 3, "an image block, not a resource");
    let bytes = STANDARD
        .decode(content[1]["data"].as_str().unwrap())
        .unwrap();
    assert!(!structured.contains_key("blob"));
    assert!(!structured.contains_key("attachmentReference"));
    assert_eq!(structured["size"], json!(data.len()));
    assert_eq!(
        structured["sha256"],
        json!(hex::encode(Sha256::digest(data)))
    );
    assert_eq!(structured["image"]["size"], json!(bytes.len()));
    assert_eq!(
        structured["image"]["sha256"],
        json!(hex::encode(Sha256::digest(&bytes)))
    );
    assert_eq!(structured["image"]["mimeType"], content[1]["mimeType"]);
    ImageResult {
        block_mime: content[1]["mimeType"].as_str().unwrap().to_owned(),
        structured,
        bytes,
    }
}

fn default_input() -> Value {
    json!({"messageId": MESSAGE_ID, "attachmentId": "stable-id"})
}

async fn image_error(name: &str, mime: &str, data: &[u8], input: Value) -> String {
    let (tool, _, _store) = fixture(pdf(name, mime, data)).await;
    call(&tool, input).await.unwrap_err().message
}

#[tokio::test]
async fn returns_a_jpeg_within_the_limit_as_an_image_block_of_its_original_bytes() {
    let jpeg = encoded(&gradient(640, 480), ImageFormat::Jpeg);
    let result = image_call("../scan.jpg", "image/jpeg", &jpeg, default_input()).await;
    assert_eq!(result.bytes, jpeg);
    assert_eq!(result.block_mime, "image/jpeg");
    assert_eq!(result.structured["mimeType"], json!("image/jpeg"));
    assert_eq!(result.structured["filename"], json!("scan.jpg"));
    assert_eq!(
        *result.image(),
        json!({
            "mimeType": "image/jpeg",
            "width": 640,
            "height": 480,
            "size": jpeg.len(),
            "sha256": hex::encode(Sha256::digest(&jpeg)),
            "originalWidth": 640,
            "originalHeight": 480,
            "maxDimension": 2048,
            "resized": false,
            "reencoded": false,
            "notes": [],
        })
    );
}

#[tokio::test]
async fn returns_a_png_within_the_limit_unchanged_and_never_upscales() {
    let png = encoded(&gradient(100, 50), ImageFormat::Png);
    let mut input = default_input();
    input["maxDimension"] = json!(256);
    let result = image_call("chart.png", "image/png", &png, input).await;
    assert_eq!(result.bytes, png);
    assert_eq!(result.block_mime, "image/png");
    assert_eq!(result.image()["width"], json!(100));
    assert_eq!(result.image()["resized"], json!(false));
}

#[tokio::test]
async fn downscales_an_oversize_image_to_the_default_long_edge_keeping_aspect_and_format() {
    let png = encoded(&gradient(3000, 1500), ImageFormat::Png);
    let result = image_call("wide.png", "image/png", &png, default_input()).await;
    assert_eq!(result.block_mime, "image/png");
    let decoded = result.decoded();
    assert_eq!((decoded.width(), decoded.height()), (2048, 1024));
    assert_eq!(result.image()["width"], json!(2048));
    assert_eq!(result.image()["height"], json!(1024));
    assert_eq!(result.image()["originalWidth"], json!(3000));
    assert_eq!(result.image()["originalHeight"], json!(1500));
    assert_eq!(result.image()["resized"], json!(true));
    assert_eq!(result.image()["reencoded"], json!(true));
    assert_ne!(result.image()["sha256"], result.structured["sha256"]);
}

#[tokio::test]
async fn max_dimension_bounds_the_long_edge_of_a_portrait_jpeg() {
    let jpeg = encoded(&gradient(400, 1000), ImageFormat::Jpeg);
    let mut input = default_input();
    input["maxDimension"] = json!(512);
    let result = image_call("tall.jpeg", "image/jpeg", &jpeg, input).await;
    assert_eq!(result.block_mime, "image/jpeg");
    let decoded = result.decoded();
    assert_eq!((decoded.width(), decoded.height()), (205, 512));
    assert_eq!(result.image()["maxDimension"], json!(512));
    assert_eq!(result.image()["resized"], json!(true));
}

#[tokio::test]
async fn applies_exif_orientation_before_returning_the_image() {
    let jpeg = jpeg_with_orientation_6(40, 20);
    let result = image_call("phone.jpg", "image/jpeg", &jpeg, default_input()).await;
    let decoded = result.decoded();
    assert_eq!((decoded.width(), decoded.height()), (20, 40));
    assert_eq!(result.image()["originalWidth"], json!(20));
    assert_eq!(result.image()["originalHeight"], json!(40));
    assert_eq!(result.image()["resized"], json!(false));
    assert_eq!(result.image()["reencoded"], json!(true));
    // Stored left column (red 0) becomes the displayed top row.
    let pixels = decoded.to_rgb8();
    assert!(pixels.get_pixel(10, 1)[0] < 40 && pixels.get_pixel(10, 38)[0] > 200);
}

#[tokio::test]
async fn converts_heic_to_jpeg() {
    for (name, mime) in [
        ("IMG_0001.HEIC", "image/heic"),
        ("IMG_0001.heif", "image/heif"),
        ("IMG_0001.heic", "application/octet-stream"),
    ] {
        let result = image_call(name, mime, HEIC, default_input()).await;
        assert_eq!(result.structured["mimeType"], json!("image/heic"));
        assert_eq!(result.block_mime, "image/jpeg");
        assert!(result.bytes.starts_with(&[0xff, 0xd8, 0xff]));
        let decoded = result.decoded().to_rgb8();
        assert_eq!(decoded.dimensions(), (64, 48));
        assert_eq!(result.image()["reencoded"], json!(true));
        assert_eq!(result.image()["resized"], json!(false));
        assert!(
            result.image()["notes"][0]
                .as_str()
                .unwrap()
                .contains("HEIC converted to JPEG")
        );
        // The source gradient runs dark to light, left to right, in red.
        assert!(decoded.get_pixel(2, 24)[0] < 40 && decoded.get_pixel(61, 24)[0] > 100);
    }
}

#[tokio::test]
async fn a_resized_gif_returns_its_first_frame_as_png_and_webp_stays_webp() {
    let gif = encoded(&gradient(600, 300), ImageFormat::Gif);
    let mut input = default_input();
    input["maxDimension"] = json!(256);
    let result = image_call("anim.gif", "image/gif", &gif, input.clone()).await;
    assert_eq!(result.structured["mimeType"], json!("image/gif"));
    assert_eq!(result.block_mime, "image/png");
    assert_eq!(result.decoded().width(), 256);
    assert!(
        result.image()["notes"][0]
            .as_str()
            .unwrap()
            .contains("first GIF frame")
    );
    let unchanged = image_call("anim.gif", "image/gif", &gif, default_input()).await;
    assert_eq!(unchanged.bytes, gif);

    let webp = encoded(&gradient(600, 300), ImageFormat::WebP);
    let result = image_call("photo.webp", "image/webp", &webp, input).await;
    assert_eq!(result.block_mime, "image/webp");
    let decoded = result.decoded();
    assert_eq!((decoded.width(), decoded.height()), (256, 128));
}

#[tokio::test]
async fn rejects_a_declared_type_that_disagrees_with_the_magic_bytes() {
    let jpeg = encoded(&gradient(32, 32), ImageFormat::Jpeg);
    let png = encoded(&gradient(32, 32), ImageFormat::Png);
    for (name, mime, data) in [
        ("a.png", "image/png", jpeg.as_slice()),
        ("a.jpg", "image/jpeg", png.as_slice()),
        ("a.jpg", "application/pdf", jpeg.as_slice()),
        (
            "a.jpg",
            "image/jpeg",
            b"<html>not an image</html>".as_slice(),
        ),
        ("a.png", "application/octet-stream", jpeg.as_slice()),
        ("a", "application/octet-stream", jpeg.as_slice()),
        ("a.jpg", "text/plain", jpeg.as_slice()),
    ] {
        let message = image_error(name, mime, data, default_input()).await;
        assert!(
            message.contains("declared") || message.contains("does not match"),
            "{name} {mime}: {message}"
        );
    }
    let accepted = image_call("a.jpg", "application/octet-stream", &jpeg, default_input()).await;
    assert_eq!(accepted.block_mime, "image/jpeg");
    let accepted = image_call("a.bin", "image/jpg", &jpeg, default_input()).await;
    assert_eq!(accepted.structured["mimeType"], json!("image/jpeg"));
}

#[tokio::test]
async fn refuses_undecodable_and_oversize_pixel_images_without_panicking() {
    let mut truncated = encoded(&gradient(64, 64), ImageFormat::Jpeg);
    truncated.truncate(40);
    let message = image_error("a.jpg", "image/jpeg", &truncated, default_input()).await;
    assert!(message.contains("could not be decoded"), "{message}");

    let mut broken_heic = HEIC.to_vec();
    broken_heic.truncate(200);
    let message = image_error("a.heic", "image/heic", &broken_heic, default_input()).await;
    assert!(message.contains("could not be decoded"), "{message}");

    let huge = png_header(20_000, 20_000);
    let message = image_error("a.png", "image/png", &huge, default_input()).await;
    assert!(message.contains("megapixel"), "{message}");
}

#[tokio::test]
async fn rejects_out_of_range_max_dimension_before_touching_imap() {
    let (tool, reader, _store) = fixture(pdf("x.pdf", "application/pdf", DATA)).await;
    for value in [json!(255), json!(8193), json!("2048"), json!(1024.5)] {
        let mut input = default_input();
        input["maxDimension"] = value;
        assert!(call(&tool, input).await.is_err());
    }
    assert!(reader.calls.lock().unwrap().is_empty());
    let mut input = default_input();
    input["maxDimension"] = json!(256);
    assert!(call(&tool, input).await.is_ok(), "ignored for a PDF");
}
