//! Image attachments for `email_attachment_get`: format identity and the
//! returned rendition.
//!
//! Attachment bytes are untrusted. Identity is decided from magic bytes and
//! must agree with the declared MIME type; decoding is `omni-image`'s, in
//! memory with pixel and allocation limits, never through a subprocess or a
//! file.

use std::sync::Arc;

pub use omni_image::{
    DEFAULT_MAX_DIMENSION, HEIC_SELF_TEST, ImageError, ImageKind, MAX_IMAGE_PIXELS,
    MAX_MAX_DIMENSION, MAX_RETURNED_IMAGE_BYTES, MIN_MAX_DIMENSION, ProcessedImage, heic_self_test,
    sniff_image,
};

const LOG: &str = "EmailAttachmentImages";

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
    let declared = ImageKind::from_mime(declared_mime_type);
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

/// Decodes, orients and bounds one image off the async runtime.
pub async fn process_image(
    kind: ImageKind,
    data: Arc<Vec<u8>>,
    max_dimension: u32,
) -> Result<ProcessedImage, ImageError> {
    let result = omni_image::process_image(kind, data, max_dimension).await;
    if let Err(error) = &result {
        tracing::warn!(target: LOG, format = kind.label(), %error, "Image attachment was not returned");
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ftyp(major: &[u8; 4]) -> Vec<u8> {
        let mut data = 16u32.to_be_bytes().to_vec();
        data.extend_from_slice(b"ftyp");
        data.extend_from_slice(major);
        data.extend_from_slice(&[0; 36]);
        data
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
            identify_image("image/heif", "a", &ftyp(b"heic")),
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
