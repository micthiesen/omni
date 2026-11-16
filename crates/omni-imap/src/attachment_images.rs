//! Image attachments for `email_attachment_get`: format identity, bounded
//! decoding and the returned rendition.
//!
//! Attachment bytes are untrusted. Identity is decided from magic bytes and
//! must agree with the declared MIME type; decoding runs in memory with pixel
//! and allocation limits, never through a subprocess or a file.

use std::io::Cursor;
use std::sync::Arc;
use std::time::Duration;

use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::{CompressionType, FilterType as PngFilter, PngEncoder};
use image::codecs::webp::WebPEncoder;
use image::imageops::FilterType;
use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder as _, ImageEncoder as _, ImageFormat, ImageReader, Limits};
use tokio::sync::Semaphore;

pub const DEFAULT_MAX_DIMENSION: u32 = 2048;
pub const MIN_MAX_DIMENSION: u32 = 256;
pub const MAX_MAX_DIMENSION: u32 = 8192;
/// Decoded pixels allowed for one image.
pub const MAX_IMAGE_PIXELS: u64 = 100_000_000;
/// Returned image bytes; a larger rendition is re-encoded as JPEG.
pub const MAX_RETURNED_IMAGE_BYTES: usize = 4 * 1024 * 1024;

const MAX_DECODE_ALLOC: u64 = 768 * 1024 * 1024;
const PROCESS_TIMEOUT: Duration = Duration::from_secs(60);
const JPEG_QUALITY: u8 = 95;
const JPEG_FALLBACK_QUALITY: u8 = 85;
const LOG: &str = "EmailAttachmentImages";
const HEIF_BRANDS: [&[u8; 4]; 4] = [b"heic", b"heix", b"mif1", b"msf1"];
const AVIF_BRANDS: [&[u8; 4]; 2] = [b"avif", b"avis"];

/// One decode at a time: a single image may hold several hundred MiB.
static DECODE_SLOT: Semaphore = Semaphore::const_new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageKind {
    Jpeg,
    Png,
    Gif,
    Webp,
    /// HEIC and HEIF stills (HEVC in ISO-BMFF).
    Heif,
}

impl ImageKind {
    pub fn mime(self) -> &'static str {
        match self {
            Self::Jpeg => "image/jpeg",
            Self::Png => "image/png",
            Self::Gif => "image/gif",
            Self::Webp => "image/webp",
            Self::Heif => "image/heic",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Jpeg => "JPEG",
            Self::Png => "PNG",
            Self::Gif => "GIF",
            Self::Webp => "WebP",
            Self::Heif => "HEIC",
        }
    }

    fn declared_as(mime_type: &str) -> Option<Self> {
        match mime_type.to_ascii_lowercase().as_str() {
            "image/jpeg" | "image/jpg" | "image/pjpeg" => Some(Self::Jpeg),
            "image/png" | "image/x-png" => Some(Self::Png),
            "image/gif" => Some(Self::Gif),
            "image/webp" => Some(Self::Webp),
            "image/heic" | "image/heif" | "image/heic-sequence" | "image/heif-sequence" => {
                Some(Self::Heif)
            }
            _ => None,
        }
    }

    fn from_extension(filename: &str) -> Option<Self> {
        let (_, extension) = filename.rsplit_once('.')?;
        match extension.to_ascii_lowercase().as_str() {
            "jpg" | "jpeg" | "jpe" | "jfif" => Some(Self::Jpeg),
            "png" => Some(Self::Png),
            "gif" => Some(Self::Gif),
            "webp" => Some(Self::Webp),
            "heic" | "heif" | "hif" => Some(Self::Heif),
            _ => None,
        }
    }
}

/// The image format the leading bytes identify, if any.
pub fn sniff_image(data: &[u8]) -> Option<ImageKind> {
    if data.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some(ImageKind::Jpeg);
    }
    if data.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        return Some(ImageKind::Png);
    }
    if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        return Some(ImageKind::Gif);
    }
    if data.len() >= 12 && data.starts_with(b"RIFF") && &data[8..12] == b"WEBP" {
        return Some(ImageKind::Webp);
    }
    is_heif(data).then_some(ImageKind::Heif)
}

/// A leading `ftyp` box whose major or compatible brands name HEIF and not AVIF.
fn is_heif(data: &[u8]) -> bool {
    if data.len() < 16 || &data[4..8] != b"ftyp" {
        return false;
    }
    let declared = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
    if !(16..=4096).contains(&declared) || declared > data.len() {
        return false;
    }
    let brands = || std::iter::once(&data[8..12]).chain(data[16..declared].chunks_exact(4));
    brands().any(|brand| HEIF_BRANDS.iter().any(|known| brand == *known))
        && !brands().any(|brand| AVIF_BRANDS.iter().any(|known| brand == *known))
}

/// Whether the attachment is an image this tool returns.
///
/// `Ok(None)` means it neither claims to be nor looks like a supported image.
/// The declared MIME type must name the format the bytes have. A part
/// declared `application/octet-stream` (also what a missing or malformed
/// Content-Type becomes) is accepted only when the filename extension names
/// that same format.
pub fn identify_image(
    declared_mime_type: &str,
    filename: &str,
    data: &[u8],
) -> Result<Option<ImageKind>, String> {
    let declared = ImageKind::declared_as(declared_mime_type);
    let Some(actual) = sniff_image(data) else {
        return match declared {
            Some(declared) => Err(format!(
                "Attachment is declared as {} but its content is not a {} image",
                declared.mime(),
                declared.label()
            )),
            None => Ok(None),
        };
    };
    if declared == Some(actual) {
        return Ok(Some(actual));
    }
    if declared_mime_type.eq_ignore_ascii_case("application/octet-stream") {
        return if ImageKind::from_extension(filename) == Some(actual) {
            Ok(Some(actual))
        } else {
            Err(format!(
                "Attachment content is {} but it is declared as application/octet-stream and its filename extension does not confirm that format",
                actual.label()
            ))
        };
    }
    Err(format!(
        "Attachment content is {} but its declared MIME type does not match",
        actual.label()
    ))
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ImageError {
    #[error("Image could not be decoded as {0}")]
    Decode(&'static str),
    #[error("Image exceeds the {} megapixel decode limit", MAX_IMAGE_PIXELS / 1_000_000)]
    TooManyPixels,
    #[error("Image could not be encoded")]
    Encode,
    #[error(
        "Image is still over {} MiB as JPEG quality {JPEG_FALLBACK_QUALITY}; request a smaller maxDimension",
        MAX_RETURNED_IMAGE_BYTES / (1024 * 1024)
    )]
    TooLarge,
    #[error("Image processing timed out")]
    Timeout,
    #[error("Image processing failed")]
    Failed,
}

/// The rendition returned to the caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessedImage {
    pub data: Vec<u8>,
    pub mime_type: &'static str,
    pub width: u32,
    pub height: u32,
    /// Dimensions as displayed, after EXIF or HEIF orientation.
    pub original_width: u32,
    pub original_height: u32,
    pub resized: bool,
    /// False only when `data` is the attachment's bytes unchanged.
    pub reencoded: bool,
    pub notes: Vec<&'static str>,
}

/// Decodes, orients and bounds one image off the async runtime.
pub async fn process_image(
    kind: ImageKind,
    data: Arc<Vec<u8>>,
    max_dimension: u32,
) -> Result<ProcessedImage, ImageError> {
    // One deadline covers the wait for the slot and the decode.
    let work = async {
        let permit = DECODE_SLOT
            .acquire()
            .await
            .map_err(|_| ImageError::Failed)?;
        // The permit moves into the blocking task: a timed-out decode keeps
        // the slot until its thread really finishes.
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            render(kind, &data, max_dimension)
        })
        .await
        .map_err(|_| ImageError::Failed)?
    };
    let result = match tokio::time::timeout(PROCESS_TIMEOUT, work).await {
        Err(_) => Err(ImageError::Timeout),
        Ok(result) => result,
    };
    if let Err(error) = &result {
        tracing::warn!(target: LOG, format = kind.label(), %error, "Image attachment was not returned");
    }
    result
}

/// A synthetic 64x48 gradient encoded by macOS `sips`.
pub const HEIC_SELF_TEST: &[u8] = include_bytes!("../assets/heic-self-test-64x48.heic");

/// Converts the bundled HEIC to JPEG and returns its size, for `omni doctor`:
/// a build whose HEIC decoder does not work fails the image.
pub fn heic_self_test() -> Result<(u32, u32), ImageError> {
    let converted = render(ImageKind::Heif, HEIC_SELF_TEST, DEFAULT_MAX_DIMENSION)?;
    if converted.mime_type != "image/jpeg" || (converted.width, converted.height) != (64, 48) {
        return Err(ImageError::Failed);
    }
    decode(ImageKind::Jpeg, &converted.data)?;
    Ok((converted.width, converted.height))
}

struct Decoded {
    image: DynamicImage,
    icc: Option<Vec<u8>>,
    /// The stored pixels were rotated or mirrored to display orientation.
    reoriented: bool,
}

/// Decoder messages describe structure (a marker, a box, a size), never
/// pixel content, so the cause is safe to log.
fn decode_failed(format: &'static str, cause: &dyn std::fmt::Display) -> ImageError {
    tracing::warn!(target: LOG, format, %cause, "Image attachment could not be decoded");
    ImageError::Decode(format)
}

fn check_pixels(width: u32, height: u32) -> Result<(), ImageError> {
    if width == 0 || height == 0 {
        return Err(ImageError::Decode("an image with pixels"));
    }
    if u64::from(width) * u64::from(height) > MAX_IMAGE_PIXELS {
        return Err(ImageError::TooManyPixels);
    }
    Ok(())
}

fn decode(kind: ImageKind, data: &[u8]) -> Result<Decoded, ImageError> {
    let format = match kind {
        ImageKind::Heif => return decode_heif(data),
        ImageKind::Jpeg => ImageFormat::Jpeg,
        ImageKind::Png => ImageFormat::Png,
        ImageKind::Gif => ImageFormat::Gif,
        ImageKind::Webp => ImageFormat::WebP,
    };
    let failed = |cause: &dyn std::fmt::Display| decode_failed(kind.label(), cause);
    let mut limits = Limits::default();
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    let mut reader = ImageReader::with_format(Cursor::new(data), format);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().map_err(|e| failed(&e))?;
    let (width, height) = decoder.dimensions();
    check_pixels(width, height)?;
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let icc = decoder.icc_profile().ok().flatten();
    let mut image = DynamicImage::from_decoder(decoder).map_err(|e| failed(&e))?;
    image.apply_orientation(orientation);
    Ok(Decoded {
        image,
        icc,
        reoriented: orientation != Orientation::NoTransforms,
    })
}

fn decode_heif(data: &[u8]) -> Result<Decoded, ImageError> {
    let failed = |cause: &dyn std::fmt::Display| decode_failed("HEIC (HEVC still image)", cause);
    let info = heic_rs::probe(data).map_err(|e| failed(&e))?;
    check_pixels(info.width, info.height)?;
    check_pixels(info.coded_width, info.coded_height)?;
    let layout = if info.has_alpha {
        heic_rs::PixelLayout::Rgba8
    } else {
        heic_rs::PixelLayout::Rgb8
    };
    let options = heic_rs::DecodeOptions::default()
        .with_layout(layout)
        .with_max_pixels(Some(MAX_IMAGE_PIXELS));
    let decoded = heic_rs::decode(data, &options).map_err(|e| failed(&e))?;
    let (width, height) = (decoded.width, decoded.height);
    let image = if info.has_alpha {
        image::RgbaImage::from_raw(width, height, decoded.data).map(DynamicImage::ImageRgba8)
    } else {
        image::RgbImage::from_raw(width, height, decoded.data).map(DynamicImage::ImageRgb8)
    }
    .ok_or_else(|| failed(&"decoded size does not match its layout"))?;
    let icc = heic_rs::context::Context::open(data)
        .ok()
        .and_then(|context| {
            let props = context.props(info.primary_item).ok()?;
            context.icc(&props).map(<[u8]>::to_vec)
        });
    Ok(Decoded {
        image,
        icc,
        reoriented: false,
    })
}

fn render(kind: ImageKind, data: &[u8], max_dimension: u32) -> Result<ProcessedImage, ImageError> {
    let Decoded {
        image,
        icc,
        reoriented,
    } = decode(kind, data)?;
    let (original_width, original_height) = (image.width(), image.height());
    let resized = original_width.max(original_height) > max_dimension;
    let mut notes = Vec::new();
    if kind != ImageKind::Heif && !resized && !reoriented && data.len() <= MAX_RETURNED_IMAGE_BYTES
    {
        if kind == ImageKind::Gif {
            notes.push("GIF returned unchanged; an animation keeps all of its frames");
        }
        return Ok(ProcessedImage {
            data: data.to_vec(),
            mime_type: kind.mime(),
            width: original_width,
            height: original_height,
            original_width,
            original_height,
            resized: false,
            reencoded: false,
            notes,
        });
    }
    let image = if resized {
        image.resize(max_dimension, max_dimension, FilterType::Lanczos3)
    } else {
        image
    };
    if reoriented {
        notes.push("Rotated to the EXIF orientation");
    }
    let (mut bytes, mut mime_type) = match kind {
        ImageKind::Jpeg => (
            encode_jpeg(&image, icc.as_deref(), JPEG_QUALITY)?,
            "image/jpeg",
        ),
        ImageKind::Heif => {
            notes.push("HEIC converted to JPEG quality 95 with 4:4:4 chroma");
            (
                encode_jpeg(&image, icc.as_deref(), JPEG_QUALITY)?,
                "image/jpeg",
            )
        }
        ImageKind::Png => (encode_png(&image, icc.as_deref())?, "image/png"),
        ImageKind::Gif => {
            notes.push("Only the first GIF frame is returned, as PNG");
            (encode_png(&image, icc.as_deref())?, "image/png")
        }
        ImageKind::Webp => {
            notes.push("Re-encoded as lossless WebP; an animation keeps only its first frame");
            (encode_webp(&image, icc.as_deref())?, "image/webp")
        }
    };
    if bytes.len() > MAX_RETURNED_IMAGE_BYTES {
        bytes = encode_jpeg(&image, icc.as_deref(), JPEG_FALLBACK_QUALITY)?;
        mime_type = "image/jpeg";
        notes.retain(|note| !note.starts_with("HEIC converted") && !note.starts_with("Re-encoded"));
        notes.push(
            "Over 4 MiB in the original format; re-encoded as JPEG quality 85 with 4:4:4 chroma",
        );
        if bytes.len() > MAX_RETURNED_IMAGE_BYTES {
            return Err(ImageError::TooLarge);
        }
    }
    Ok(ProcessedImage {
        data: bytes,
        mime_type,
        width: image.width(),
        height: image.height(),
        original_width,
        original_height,
        resized,
        reencoded: true,
        notes,
    })
}

/// 8-bit grey or RGB; transparency is flattened onto white.
fn jpeg_pixels(image: &DynamicImage) -> DynamicImage {
    let color = image.color();
    if color.has_alpha() {
        let rgba = image.to_rgba8();
        let mut flat = image::RgbImage::new(rgba.width(), rgba.height());
        for (out, pixel) in flat.pixels_mut().zip(rgba.pixels()) {
            let alpha = u32::from(pixel[3]);
            for channel in 0..3 {
                let value = u32::from(pixel[channel]) * alpha + 255 * (255 - alpha);
                out[channel] = u8::try_from((value + 127) / 255).unwrap_or(u8::MAX);
            }
        }
        DynamicImage::ImageRgb8(flat)
    } else if color.has_color() {
        DynamicImage::ImageRgb8(image.to_rgb8())
    } else {
        DynamicImage::ImageLuma8(image.to_luma8())
    }
}

fn encode_jpeg(
    image: &DynamicImage,
    icc: Option<&[u8]>,
    quality: u8,
) -> Result<Vec<u8>, ImageError> {
    let pixels = jpeg_pixels(image);
    let mut out = Vec::new();
    let mut encoder = JpegEncoder::new_with_quality(&mut out, quality);
    set_icc(&mut encoder, icc, pixels.color().has_color());
    encoder
        .write_image(
            pixels.as_bytes(),
            pixels.width(),
            pixels.height(),
            pixels.color().into(),
        )
        .map_err(|_| ImageError::Encode)?;
    Ok(out)
}

fn encode_png(image: &DynamicImage, icc: Option<&[u8]>) -> Result<Vec<u8>, ImageError> {
    let pixels = match image {
        DynamicImage::ImageRgb32F(_) => DynamicImage::ImageRgb16(image.to_rgb16()),
        DynamicImage::ImageRgba32F(_) => DynamicImage::ImageRgba16(image.to_rgba16()),
        other => other.clone(),
    };
    let mut out = Vec::new();
    let mut encoder =
        PngEncoder::new_with_quality(&mut out, CompressionType::Default, PngFilter::Adaptive);
    set_icc(&mut encoder, icc, pixels.color().has_color());
    pixels
        .write_with_encoder(encoder)
        .map_err(|_| ImageError::Encode)?;
    Ok(out)
}

fn encode_webp(image: &DynamicImage, icc: Option<&[u8]>) -> Result<Vec<u8>, ImageError> {
    let pixels = if image.color().has_alpha() {
        DynamicImage::ImageRgba8(image.to_rgba8())
    } else {
        DynamicImage::ImageRgb8(image.to_rgb8())
    };
    let mut out = Vec::new();
    let mut encoder = WebPEncoder::new_lossless(&mut out);
    set_icc(&mut encoder, icc, true);
    pixels
        .write_with_encoder(encoder)
        .map_err(|_| ImageError::Encode)?;
    Ok(out)
}

/// Keeps the colour profile when the encoder can carry one.
/// A profile for another colour model (CMYK, or RGB pixels written as grey)
/// would mislabel the pixels, so it is dropped.
fn set_icc(encoder: &mut impl image::ImageEncoder, icc: Option<&[u8]>, has_color: bool) {
    let space: &[u8] = if has_color { b"RGB " } else { b"GRAY" };
    if let Some(icc) = icc.filter(|icc| icc.get(16..20) == Some(space)) {
        // An encoder that cannot embed a profile still writes correct pixels.
        let _unsupported = encoder.set_icc_profile(icc.to_vec());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ftyp(major: &[u8; 4], compatible: &[&[u8; 4]]) -> Vec<u8> {
        let size = 16 + 4 * compatible.len();
        let mut data = u32::try_from(size).unwrap().to_be_bytes().to_vec();
        data.extend_from_slice(b"ftyp");
        data.extend_from_slice(major);
        data.extend_from_slice(&[0; 4]);
        for brand in compatible {
            data.extend_from_slice(*brand);
        }
        data.extend_from_slice(&[0; 32]);
        data
    }

    #[test]
    fn sniffs_each_format_by_magic_bytes() {
        assert_eq!(
            sniff_image(&[0xff, 0xd8, 0xff, 0xe0]),
            Some(ImageKind::Jpeg)
        );
        assert_eq!(sniff_image(b"\x89PNG\r\n\x1a\n...."), Some(ImageKind::Png));
        assert_eq!(sniff_image(b"GIF89a...."), Some(ImageKind::Gif));
        assert_eq!(sniff_image(b"GIF87a...."), Some(ImageKind::Gif));
        assert_eq!(
            sniff_image(b"RIFF\x10\0\0\0WEBPVP8 "),
            Some(ImageKind::Webp)
        );
        assert_eq!(sniff_image(b"RIFF\x10\0\0\0WAVEfmt "), None);
        assert_eq!(sniff_image(b"%PDF-1.7"), None);
        assert_eq!(sniff_image(b""), None);
    }

    #[test]
    fn heif_needs_a_heif_brand_and_no_avif_brand() {
        assert_eq!(
            sniff_image(&ftyp(b"heic", &[b"mif1"])),
            Some(ImageKind::Heif)
        );
        assert_eq!(
            sniff_image(&ftyp(b"mif1", &[b"heic"])),
            Some(ImageKind::Heif)
        );
        assert_eq!(
            sniff_image(&ftyp(b"isom", &[b"heix"])),
            Some(ImageKind::Heif)
        );
        assert_eq!(sniff_image(&ftyp(b"msf1", &[])), Some(ImageKind::Heif));
        assert_eq!(sniff_image(&ftyp(b"avif", &[b"mif1", b"miaf"])), None);
        assert_eq!(sniff_image(&ftyp(b"isom", &[b"mp41"])), None);
        let mut truncated = ftyp(b"heic", &[b"mif1"]);
        truncated[3] = 0xff;
        assert_eq!(sniff_image(&truncated), None);
    }

    #[test]
    fn declared_type_must_match_the_content() {
        let jpeg = [0xff, 0xd8, 0xff, 0xe0];
        assert_eq!(
            identify_image("image/jpeg", "a.jpg", &jpeg),
            Ok(Some(ImageKind::Jpeg))
        );
        assert_eq!(
            identify_image("IMAGE/JPG", "a", &jpeg),
            Ok(Some(ImageKind::Jpeg))
        );
        assert_eq!(
            identify_image("image/heif", "a", &ftyp(b"heic", &[])),
            Ok(Some(ImageKind::Heif))
        );
        assert!(identify_image("image/png", "a.jpg", &jpeg).is_err());
        assert!(identify_image("application/pdf", "a.jpg", &jpeg).is_err());
        assert!(identify_image("text/html", "a.jpg", &jpeg).is_err());
        assert!(identify_image("image/jpeg", "a.jpg", b"<html>").is_err());
        assert_eq!(identify_image("text/html", "a.html", b"<html>"), Ok(None));
    }

    #[test]
    fn octet_stream_needs_an_agreeing_extension() {
        let jpeg = [0xff, 0xd8, 0xff, 0xe0];
        let generic = "application/octet-stream";
        assert_eq!(
            identify_image(generic, "Scan.JPG", &jpeg),
            Ok(Some(ImageKind::Jpeg))
        );
        assert!(identify_image(generic, "scan.png", &jpeg).is_err());
        assert!(identify_image(generic, "scan", &jpeg).is_err());
        assert_eq!(identify_image(generic, "notes.bin", b"plain"), Ok(None));
    }
}
