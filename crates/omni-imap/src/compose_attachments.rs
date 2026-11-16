//! PDFs attached to `email_send` and `email_draft_create` by pinned
//! reference.
//!
//! Callers never upload bytes: they pass the `attachmentReference` returned by
//! `email_attachment_get`, and Omni re-reads each PDF fresh, refusing bytes
//! whose SHA-256 differs from the reviewed copy before any reservation,
//! draft APPEND or SMTP submission.

use std::sync::Arc;

use omni_mailer::OutgoingEmailAttachment;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::attachments::{
    AttachmentReader, MAX_ATTACHMENT_BYTES, pdf_start, safe_attachment_filename,
    valid_attachment_message_id,
};

pub const MAX_OUTGOING_ATTACHMENTS: usize = 5;
pub const MAX_OUTGOING_ATTACHMENT_TOTAL_BYTES: usize = 10 * 1024 * 1024;
const MIB: usize = 1024 * 1024;
const PDF: &str = "application/pdf";

/// `{messageId, attachmentId, sha256}` exactly as `email_attachment_get`
/// returned it. Field order is the schema's (it feeds the fingerprint).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OutgoingAttachmentReference {
    pub message_id: String,
    pub attachment_id: String,
    pub sha256: String,
}

/// The refinements the JSON Schema cannot express: an exact RFC
/// Message-ID per reference and each attachment referenced only once.
pub fn validate_references(references: &[OutgoingAttachmentReference]) -> Result<(), String> {
    if references
        .iter()
        .any(|reference| !valid_attachment_message_id(&reference.message_id))
    {
        return Err("Expected an exact RFC Message-ID".to_owned());
    }
    let mut seen = std::collections::HashSet::new();
    if !references
        .iter()
        .all(|r| seen.insert((r.message_id.as_str(), r.attachment_id.as_str())))
    {
        return Err("Each attachment may be referenced only once".to_owned());
    }
    Ok(())
}

/// Receipt and tool-output metadata for one attached PDF.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutgoingAttachmentMetadata {
    pub message_id: String,
    pub attachment_id: String,
    pub filename: String,
    pub mime_type: String,
    pub size: u64,
    pub sha256: String,
}

/// Verified bytes plus their metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedOutgoingAttachment {
    pub file: OutgoingEmailAttachment,
    pub metadata: OutgoingAttachmentMetadata,
}

/// A refused attachment; nothing was sent or saved.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct OutgoingAttachmentError(pub String);

/// A PDF filename that is never used as a path. Literal RFC 2047 markers are
/// broken up so recipients cannot decode them into header-like text.
pub fn outgoing_pdf_filename(name: Option<&str>) -> String {
    let filename = safe_attachment_filename(name).replace("=?", "=_");
    if filename.to_ascii_lowercase().ends_with(".pdf") {
        filename
    } else {
        format!("{filename}.pdf")
    }
}

/// Re-reads each referenced PDF fresh before any reservation or delivery.
/// Errors name attachments by position and opaque id, never by filename or
/// content.
pub async fn resolve_outgoing_attachments(
    reader: Option<&Arc<dyn AttachmentReader>>,
    references: &[OutgoingAttachmentReference],
) -> Result<Vec<ResolvedOutgoingAttachment>, OutgoingAttachmentError> {
    if references.is_empty() {
        return Ok(Vec::new());
    }
    let fail =
        |message: String| OutgoingAttachmentError(format!("{message}. Nothing was sent or saved"));
    let Some(reader) = reader.filter(|r| r.available()) else {
        return Err(fail("Attachment retrieval is unavailable".to_owned()));
    };
    let mut resolved = Vec::with_capacity(references.len());
    let mut total_bytes = 0;
    for (index, reference) in references.iter().enumerate() {
        let label = format!("Attachment {} ({})", index + 1, reference.attachment_id);
        let mut attachment = match reader
            .fetch_attachment(
                &reference.message_id,
                &reference.attachment_id,
                MAX_ATTACHMENT_BYTES,
            )
            .await
        {
            Err(error) => return Err(fail(format!("{label} could not be read: {error}"))),
            Ok(None) => {
                return Err(fail(format!(
                    "{label} was not found in Inbox, Archive or Sent; the source email may have moved or been deleted. Re-read it with email_get for a current attachmentId"
                )));
            }
            Ok(Some(attachment)) => attachment,
        };
        if attachment.data.is_empty() {
            return Err(fail(format!("{label} is empty")));
        }
        if attachment.data.len() > MAX_ATTACHMENT_BYTES {
            return Err(fail(format!(
                "{label} exceeds the {} MiB attachment limit",
                MAX_ATTACHMENT_BYTES / MIB
            )));
        }
        // The same identity rule and leading-byte trim as `email_attachment_get`,
        // so the reviewed SHA-256 describes these bytes.
        let Some(start) = pdf_start(&attachment.mime_type, &attachment.name, &attachment.data)
        else {
            return Err(fail(format!(
                "{label} is not a PDF with matching MIME type and PDF header"
            )));
        };
        attachment.data.drain(..start);
        let sha256 = hex::encode(Sha256::digest(&attachment.data));
        if sha256 != reference.sha256 {
            return Err(fail(format!(
                "{label} no longer matches the reviewed sha256; the part changed or another email shares its Message-ID. Re-read it with email_attachment_get"
            )));
        }
        total_bytes += attachment.data.len();
        if total_bytes > MAX_OUTGOING_ATTACHMENT_TOTAL_BYTES {
            return Err(fail(format!(
                "Attachments exceed the {} MiB total limit",
                MAX_OUTGOING_ATTACHMENT_TOTAL_BYTES / MIB
            )));
        }
        let filename = outgoing_pdf_filename(Some(&attachment.name));
        let size = attachment.data.len() as u64;
        resolved.push(ResolvedOutgoingAttachment {
            file: OutgoingEmailAttachment {
                filename: filename.clone(),
                content_type: PDF.to_owned(),
                content: attachment.data,
            },
            metadata: OutgoingAttachmentMetadata {
                message_id: reference.message_id.clone(),
                attachment_id: reference.attachment_id.clone(),
                filename,
                mime_type: PDF.to_owned(),
                size,
                sha256,
            },
        });
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pdf_filenames_gain_an_extension_and_lose_encoded_word_markers() {
        assert_eq!(outgoing_pdf_filename(Some("scan")), "scan.pdf");
        assert_eq!(outgoing_pdf_filename(Some("Scan.PDF")), "Scan.PDF");
        assert_eq!(
            outgoing_pdf_filename(Some("=?utf-8?Q?evil=0D=0ABcc:x@y?=.pdf")),
            "=_utf-8?Q?evil=0D=0ABcc:x@y?=.pdf"
        );
    }
}
