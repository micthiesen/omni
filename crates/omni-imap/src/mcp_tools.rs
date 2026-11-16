//! MCP tools: compose, archive and PDF and image attachments. Each tool's contract is
//! declared in [`defs`]; handlers trim inputs before validating against the
//! derived input schema, so inputs valid after trimming are accepted.

pub mod defs;

use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use futures::future::BoxFuture;
use omni_mcp_kit::{
    Content, McpTool, SchemaValidator, ToolContext, ToolDefinition, ToolError, ToolHandler,
    ToolMetaError, ToolOutput, raw_tool,
};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use sha2::{Digest as _, Sha256};
use tokio_util::task::TaskTracker;

use crate::archive_service::{ArchiveMailbox, ArchiveService};
use crate::archive_store::{
    ArchiveAction, ArchiveActionError, ArchiveActionStatus, ArchivePatch, get_archive_action,
    queue_archive_action, update_archive_action,
};
use crate::attachment_images::{DEFAULT_MAX_DIMENSION, ImageKind, identify_image, process_image};
pub use crate::attachments::AttachmentReader;
use crate::attachments::{MAX_ATTACHMENT_BYTES, is_pdf_attachment, safe_attachment_filename};
use crate::compose::{ComposeRequest, ComposeService};
use crate::compose_attachments::{OutgoingAttachmentReference, validate_references};
use crate::ops::archive::ArchiveIdentity;
use crate::transport::{DownloadedAttachment, ImapTransport};

impl AttachmentReader for ImapTransport {
    fn available(&self) -> bool {
        self.is_active()
    }

    fn fetch_attachment<'a>(
        &'a self,
        message_id: &'a str,
        attachment_id: &'a str,
        max_bytes: usize,
    ) -> BoxFuture<'a, Result<Option<DownloadedAttachment>, crate::protocol::ImapError>> {
        Box::pin(self.fetch_attachment_by_id(message_id, attachment_id, Some(max_bytes)))
    }
}

/// What the archive tools need from the mailbox side.
pub trait ArchiveTransport: ArchiveMailbox {
    fn available(&self) -> bool;
}

impl ArchiveTransport for ImapTransport {
    fn available(&self) -> bool {
        self.is_active()
    }
}

/// Everything the tool handlers use.
#[derive(Clone)]
pub struct ToolDeps {
    pub compose: ComposeService,
    pub archive: ArchiveService,
    pub archive_transport: Option<Arc<dyn ArchiveTransport>>,
    pub attachments: Option<Arc<dyn AttachmentReader>>,
    pub tracker: TaskTracker,
}

impl ToolDeps {
    fn archive_mailbox(&self) -> Option<Arc<dyn ArchiveTransport>> {
        self.archive_transport.clone().filter(|t| t.available())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    DraftCreate,
    Send,
    SendStatus,
    SentCopyRepair,
    ArchiveQueue,
    ArchiveStatus,
    ArchiveCancel,
    ArchiveRestore,
    AttachmentGet,
}

impl Kind {
    const ALL: [(Kind, &'static dyn ToolDefinition); 9] = [
        (Kind::DraftCreate, &defs::EMAIL_DRAFT_CREATE),
        (Kind::Send, &defs::EMAIL_SEND),
        (Kind::SendStatus, &defs::EMAIL_SEND_STATUS),
        (Kind::SentCopyRepair, &defs::EMAIL_SENT_COPY_REPAIR),
        (Kind::ArchiveQueue, &defs::EMAIL_ARCHIVE_QUEUE),
        (Kind::ArchiveStatus, &defs::EMAIL_ARCHIVE_STATUS),
        (Kind::ArchiveCancel, &defs::EMAIL_ARCHIVE_CANCEL),
        (Kind::ArchiveRestore, &defs::EMAIL_ARCHIVE_RESTORE),
        (Kind::AttachmentGet, &defs::EMAIL_ATTACHMENT_GET),
    ];
}

struct Handler {
    kind: Kind,
    input: SchemaValidator,
    output: SchemaValidator,
    deps: ToolDeps,
}

fn trim_string(map: &mut Map<String, Value>, key: &str) {
    if let Some(Value::String(s)) = map.get_mut(key) {
        *s = omni_core::js::trim(s).to_owned();
    }
}

fn trim_list(map: &mut Map<String, Value>, key: &str) {
    match map.get_mut(key) {
        Some(Value::Array(items)) => {
            for item in items {
                if let Value::String(s) = item {
                    *s = omni_core::js::trim(s).to_owned();
                }
            }
        }
        Some(Value::String(s)) => *s = omni_core::js::trim(s).to_owned(),
        _ => {}
    }
}

/// Input transforms applied before validation.
fn normalize(kind: Kind, mut input: Value) -> Value {
    let Some(map) = input.as_object_mut() else {
        return input;
    };
    match kind {
        Kind::DraftCreate | Kind::Send => {
            trim_string(map, "idempotencyKey");
            trim_list(map, "to");
            trim_list(map, "cc");
            trim_list(map, "bcc");
            trim_string(map, "subject");
            trim_string(map, "inReplyTo");
            trim_list(map, "references");
            if let Some(Value::Array(items)) = map.get_mut("attachments") {
                for item in items {
                    if let Some(reference) = item.as_object_mut() {
                        trim_string(reference, "messageId");
                    }
                }
            }
        }
        Kind::SendStatus | Kind::SentCopyRepair | Kind::ArchiveQueue => {
            trim_string(map, "idempotencyKey")
        }
        _ => {}
    }
    input
}

/// Decodes validated input; integral floats decode into integer fields (JS has
/// one number type).
fn decode<T: DeserializeOwned>(value: Value) -> Result<T, ToolError> {
    serde_json::from_value(omni_core::js::normalize_numbers(value))
        .map_err(|e| ToolError::input(e.to_string()))
}

fn execute_error(error: &(dyn std::error::Error + 'static)) -> ToolError {
    ToolError::execute_from(error)
}

#[derive(Deserialize)]
#[serde(untagged)]
enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ComposeFields {
    idempotency_key: String,
    to: OneOrMany,
    #[serde(default)]
    cc: Option<Vec<String>>,
    #[serde(default)]
    bcc: Option<Vec<String>>,
    subject: String,
    text: String,
    #[serde(default)]
    in_reply_to: Option<String>,
    #[serde(default)]
    references: Option<Vec<String>>,
    #[serde(default)]
    attachments: Option<Vec<OutgoingAttachmentReference>>,
}

impl ComposeFields {
    /// The schema refinements and the empty-list transform on `attachments`.
    fn request(input: Value) -> Result<ComposeRequest, ToolError> {
        let mut fields: ComposeFields = decode(input)?;
        if let Some(references) = &fields.attachments {
            validate_references(references).map_err(ToolError::input)?;
        }
        // An empty list keeps the attachment-free fingerprint and Message-ID.
        fields.attachments = fields.attachments.filter(|list| !list.is_empty());
        Ok(fields.into())
    }
}

impl From<ComposeFields> for ComposeRequest {
    fn from(fields: ComposeFields) -> Self {
        Self {
            idempotency_key: fields.idempotency_key,
            to: match fields.to {
                OneOrMany::One(one) => vec![one],
                OneOrMany::Many(many) => many,
            },
            cc: fields.cc,
            bcc: fields.bcc,
            subject: fields.subject,
            text: fields.text,
            in_reply_to: fields.in_reply_to,
            references: fields.references,
            attachments: fields.attachments,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct KeyInput {
    idempotency_key: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Origin {
    folder: String,
    uid_validity: String,
    uid: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct QueueInput {
    idempotency_key: String,
    origin: Origin,
    message_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActionInput {
    action_id: String,
}

fn default_max_bytes() -> usize {
    MAX_ATTACHMENT_BYTES
}

fn default_max_dimension() -> u32 {
    DEFAULT_MAX_DIMENSION
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AttachmentInput {
    message_id: String,
    attachment_id: String,
    #[serde(default = "default_max_bytes")]
    max_bytes: usize,
    #[serde(default = "default_max_dimension")]
    max_dimension: u32,
}

pub fn serialize_action(action: &ArchiveAction) -> Value {
    json!({
        "actionId": action.action_id,
        "status": action.status.as_str(),
        "messageId": action.identity.message_id,
        "source": {
            "folder": action.identity.folder,
            "uidValidity": action.identity.uid_validity,
            "uid": action.identity.uid,
            "messageId": action.identity.message_id,
        },
        "destination": action.destination.as_ref().map(|d| json!({"folder": d.folder, "uidValidity": d.uid_validity, "uid": d.uid})),
        "restoredLocation": action.restored_location.as_ref().map(|d| json!({"folder": d.folder, "uidValidity": d.uid_validity, "uid": d.uid})),
        "attempts": action.attempts,
        "nextAttemptAt": action.next_attempt_at,
        "reason": action.reason.map(|r| serde_json::to_value(r).unwrap_or(Value::Null)),
        "createdAt": action.created_at,
        "updatedAt": action.updated_at,
    })
}

fn archive_error(error: ArchiveActionError) -> ToolError {
    execute_error(&error)
}

impl Handler {
    async fn run(&self, input: Value) -> Result<ToolOutput, ToolError> {
        let input = normalize(self.kind, input);
        self.input.check(&input).map_err(ToolError::input)?;
        let deps = self.deps.clone();
        let value: Value = match self.kind {
            Kind::DraftCreate => {
                let request = ComposeFields::request(input)?;
                let out = deps
                    .compose
                    .create_draft(&request)
                    .await
                    .map_err(|e| execute_error(&e))?;
                serde_json::to_value(out).map_err(|e| ToolError::output(e.to_string()))?
            }
            Kind::Send => {
                let request = ComposeFields::request(input)?;
                let compose = deps.compose.clone();
                // An SMTP submission and its receipt complete even if the caller goes away.
                let out = omni_core::spawn::must_complete(&deps.tracker, async move {
                    compose.send(&request).await
                })
                .await
                .map_err(|e| execute_error(&e))?;
                serde_json::to_value(out).map_err(|e| ToolError::output(e.to_string()))?
            }
            Kind::SendStatus => {
                let key: KeyInput = decode(input)?;
                let out = deps
                    .compose
                    .send_status(&key.idempotency_key)
                    .await
                    .map_err(|e| execute_error(&e))?;
                serde_json::to_value(out).map_err(|e| ToolError::output(e.to_string()))?
            }
            Kind::SentCopyRepair => {
                let key: KeyInput = decode(input)?;
                let compose = deps.compose.clone();
                let out = omni_core::spawn::must_complete(&deps.tracker, async move {
                    compose.sent_copy_repair(&key.idempotency_key).await
                })
                .await
                .map_err(|e| execute_error(&e))?;
                json!({ "sentCopy": out })
            }
            Kind::ArchiveQueue => {
                let queue: QueueInput = decode(input)?;
                let identity = ArchiveIdentity {
                    folder: queue.origin.folder,
                    uid_validity: queue.origin.uid_validity,
                    uid: queue.origin.uid,
                    message_id: queue.message_id,
                };
                let action =
                    queue_archive_action(deps.archive.store(), &queue.idempotency_key, identity)
                        .await
                        .map_err(archive_error)?;
                serialize_action(&action)
            }
            Kind::ArchiveStatus => {
                let input: ActionInput = decode(input)?;
                let archive = deps.archive.clone();
                let mailbox = deps.archive_mailbox();
                let action = omni_core::spawn::must_complete(&deps.tracker, async move {
                    let mailbox = mailbox.as_deref().map(|m| m as &dyn ArchiveMailbox);
                    archive.status(&input.action_id, mailbox).await
                })
                .await
                .map_err(archive_error)?;
                serialize_action(&action)
            }
            Kind::ArchiveCancel => {
                let input: ActionInput = decode(input)?;
                let store = deps.archive.store();
                let existing = get_archive_action(store, &input.action_id)
                    .await
                    .map_err(|e| archive_error(e.into()))?;
                let action = match existing {
                    Some(existing) if existing.status == ArchiveActionStatus::Cancelled => existing,
                    _ => update_archive_action(
                        store,
                        &input.action_id,
                        ArchiveActionStatus::Queued,
                        ArchiveActionStatus::Cancelled,
                        ArchivePatch::default(),
                    )
                    .await
                    .map_err(archive_error)?,
                };
                serialize_action(&action)
            }
            Kind::ArchiveRestore => {
                let input: ActionInput = decode(input)?;
                let Some(mailbox) = deps.archive_mailbox() else {
                    return Err(ToolError::execute("Email monitoring is not active"));
                };
                let archive = deps.archive.clone();
                let action = omni_core::spawn::must_complete(&deps.tracker, async move {
                    archive.restore(&input.action_id, mailbox.as_ref()).await
                })
                .await
                .map_err(archive_error)?;
                serialize_action(&action)
            }
            Kind::AttachmentGet => return self.attachment(decode(input)?).await,
        };
        self.output.check(&value).map_err(ToolError::output)?;
        match value {
            Value::Object(map) => Ok(ToolOutput::Structured(map)),
            _ => Err(ToolError::output("tool output must be a JSON object")),
        }
    }

    async fn attachment(&self, input: AttachmentInput) -> Result<ToolOutput, ToolError> {
        let Some(reader) = self.deps.attachments.clone().filter(|r| r.available()) else {
            return Err(ToolError::execute("Attachment retrieval is unavailable"));
        };
        let found = reader
            .fetch_attachment(&input.message_id, &input.attachment_id, input.max_bytes)
            .await
            .map_err(|e| execute_error(&e))?;
        let Some(attachment) = found else {
            return Err(ToolError::execute(
                "Message or attachment not found in the readable mailboxes",
            ));
        };
        if attachment.data.len() > input.max_bytes {
            return Err(ToolError::execute(
                "Attachment exceeds the requested byte limit",
            ));
        }
        if !is_pdf_attachment(&attachment.mime_type, &attachment.data) {
            let filename = safe_attachment_filename(Some(&attachment.name));
            return match identify_image(&attachment.mime_type, &filename, &attachment.data)
                .map_err(ToolError::execute)?
            {
                Some(kind) => self.image(input, kind, filename, attachment.data).await,
                None => Err(ToolError::execute(
                    "Attachment is not a PDF or a supported image (JPEG, PNG, GIF, WebP, HEIC) whose declared MIME type matches its content",
                )),
            };
        }
        let blob = STANDARD.encode(&attachment.data);
        let mut metadata = Map::new();
        metadata.insert(
            "messageId".to_owned(),
            Value::from(input.message_id.clone()),
        );
        metadata.insert(
            "attachmentId".to_owned(),
            Value::from(input.attachment_id.clone()),
        );
        metadata.insert(
            "filename".to_owned(),
            Value::from(safe_attachment_filename(Some(&attachment.name))),
        );
        metadata.insert("mimeType".to_owned(), Value::from("application/pdf"));
        metadata.insert("size".to_owned(), Value::from(attachment.data.len()));
        let sha256 = hex::encode(Sha256::digest(&attachment.data));
        metadata.insert("sha256".to_owned(), Value::from(sha256.clone()));
        metadata.insert(
            "attachmentReference".to_owned(),
            json!({
                "messageId": input.message_id,
                "attachmentId": input.attachment_id,
                "sha256": sha256,
            }),
        );
        let mut structured = metadata.clone();
        structured.insert("blob".to_owned(), Value::from(blob.clone()));
        self.output
            .check(&Value::Object(structured.clone()))
            .map_err(ToolError::output)?;
        let mut text: Content = Map::new();
        text.insert("type".to_owned(), Value::from("text"));
        text.insert(
            "text".to_owned(),
            Value::from(omni_core::js::json_stringify(&Value::Object(metadata))),
        );
        let mut resource: Content = Map::new();
        resource.insert("type".to_owned(), Value::from("resource"));
        resource.insert(
            "resource".to_owned(),
            json!({
                "uri": format!("omni-email-attachment:{}", input.attachment_id),
                "mimeType": "application/pdf",
                "blob": blob,
            }),
        );
        Ok(ToolOutput::Custom {
            structured,
            content: vec![text, resource],
        })
    }

    /// An image comes back as an MCP image block, bounded to `maxDimension`.
    /// It carries no `attachmentReference`: sends and drafts attach PDFs only.
    async fn image(
        &self,
        input: AttachmentInput,
        kind: ImageKind,
        filename: String,
        data: Vec<u8>,
    ) -> Result<ToolOutput, ToolError> {
        let original = Arc::new(data);
        let processed = process_image(kind, original.clone(), input.max_dimension)
            .await
            .map_err(|e| ToolError::execute(e.to_string()))?;
        let mut metadata = Map::new();
        metadata.insert("messageId".to_owned(), Value::from(input.message_id));
        metadata.insert("attachmentId".to_owned(), Value::from(input.attachment_id));
        metadata.insert("filename".to_owned(), Value::from(filename));
        metadata.insert("mimeType".to_owned(), Value::from(kind.mime()));
        metadata.insert("size".to_owned(), Value::from(original.len()));
        metadata.insert(
            "sha256".to_owned(),
            Value::from(hex::encode(Sha256::digest(original.as_slice()))),
        );
        metadata.insert(
            "image".to_owned(),
            json!({
                "mimeType": processed.mime_type,
                "width": processed.width,
                "height": processed.height,
                "size": processed.data.len(),
                "sha256": hex::encode(Sha256::digest(&processed.data)),
                "originalWidth": processed.original_width,
                "originalHeight": processed.original_height,
                "maxDimension": input.max_dimension,
                "resized": processed.resized,
                "reencoded": processed.reencoded,
                "notes": processed.notes,
            }),
        );
        self.output
            .check(&Value::Object(metadata.clone()))
            .map_err(ToolError::output)?;
        let mut text: Content = Map::new();
        text.insert("type".to_owned(), Value::from("text"));
        text.insert(
            "text".to_owned(),
            Value::from(omni_core::js::json_stringify(&Value::Object(
                metadata.clone(),
            ))),
        );
        let mut image: Content = Map::new();
        image.insert("type".to_owned(), Value::from("image"));
        image.insert(
            "data".to_owned(),
            Value::from(STANDARD.encode(&processed.data)),
        );
        image.insert("mimeType".to_owned(), Value::from(processed.mime_type));
        Ok(ToolOutput::Custom {
            structured: metadata,
            content: vec![text, image],
        })
    }
}

impl ToolHandler for Handler {
    fn call<'a>(
        &'a self,
        input: Value,
        _cx: ToolContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(self.run(input))
    }
}

/// The nine mail transport tools in serving group order
/// (email-compose, email-archive, email-attachments).
pub fn email_tools(deps: ToolDeps) -> Result<Vec<McpTool>, ToolMetaError> {
    Kind::ALL
        .iter()
        .map(|&(kind, def)| {
            let meta = def.meta()?;
            let handler = Handler {
                kind,
                input: SchemaValidator::new(&meta.name, &meta.input_schema)?,
                output: SchemaValidator::new(&meta.name, &meta.output_schema)?,
                deps: deps.clone(),
            };
            raw_tool(def, Arc::new(handler))
        })
        .collect()
}
