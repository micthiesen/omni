//! The rendition returned to a caller that asked for an image: the original
//! bytes when they already fit, otherwise a re-encoding in the nearest format.

use std::sync::Arc;

use image::imageops::FilterType;

use crate::{DecodedImage, ImageError, ImageKind, decode, encode_jpeg, encode_png, encode_webp};

/// Returned image bytes; a larger rendition is re-encoded as JPEG.
pub const MAX_RETURNED_IMAGE_BYTES: usize = 4 * 1024 * 1024;

const JPEG_QUALITY: u8 = 95;
pub(crate) const JPEG_FALLBACK_QUALITY: u8 = 85;

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
    /// False only when `data` is the source bytes unchanged.
    pub reencoded: bool,
    pub notes: Vec<&'static str>,
}

/// Decodes, orients and bounds one image off the async runtime (see
/// [`crate::with_decoded`] for the bounds). An image that already fits
/// `max_dimension` and [`MAX_RETURNED_IMAGE_BYTES`] comes back unchanged.
pub async fn process_image(
    kind: ImageKind,
    data: Arc<Vec<u8>>,
    max_dimension: u32,
) -> Result<ProcessedImage, ImageError> {
    crate::bounded(move || render(kind, &data, max_dimension)).await
}

pub(crate) fn render(
    kind: ImageKind,
    data: &[u8],
    max_dimension: u32,
) -> Result<ProcessedImage, ImageError> {
    let DecodedImage {
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
