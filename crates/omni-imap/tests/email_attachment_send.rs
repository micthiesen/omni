//! Retrieve, review and
//! send a received PDF through the real transport over an in-memory mailbox.
//! SMTP is a recorder and draft APPEND a fake that must never be reached.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::sync::{Arc, Mutex};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use futures::future::BoxFuture;
use omni_imap::archive_service::ArchiveService;
use omni_imap::attachments::{AttachmentReader, encode_stable_attachment_id};
use omni_imap::compose::{ComposeMailbox, ComposeSender, ComposeService};
use omni_imap::fake::{FakeCall, FakeMessage, FakeServer};
use omni_imap::mcp_tools::{ToolDeps, email_tools};
use omni_imap::ops::drafts::{EmailDraftInput, EmailDraftResult};
use omni_imap::ops::sent::{BeforeAppend, SentCopyInput, SentCopyResult};
use omni_imap::protocol::ImapError;
use omni_mcp_kit::{McpTool, ToolContext, ToolError, ToolOutput};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

const MESSAGE_ID: &str = "<synthetic-source@example.test>";

fn sha256(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// A synthetic received email: text plus one PDF part with an awkward filename.
fn source_message(pdf: &[u8]) -> Vec<u8> {
    [
        format!("Message-ID: {MESSAGE_ID}"),
        "From: clinic@example.test".to_owned(),
        "Subject: Your synthetic results".to_owned(),
        "Content-Type: multipart/mixed; boundary=\"outer\"".to_owned(),
        String::new(),
        "--outer".to_owned(),
        "Content-Type: text/plain".to_owned(),
        String::new(),
        "Synthetic results attached.".to_owned(),
        "--outer".to_owned(),
        "Content-Type: application/pdf; name=\"Résumé de test.pdf\"".to_owned(),
        "Content-Disposition: attachment; filename*=utf-8''R%C3%A9sum%C3%A9%20de%20test.pdf"
            .to_owned(),
        "Content-Transfer-Encoding: base64".to_owned(),
        String::new(),
        STANDARD.encode(pdf),
        "--outer--".to_owned(),
        String::new(),
    ]
    .join("\r\n")
    .into_bytes()
}

#[derive(Default)]
struct Smtp {
    submitted: Mutex<Vec<Vec<u8>>>,
}

impl ComposeSender for Smtp {
    fn send<'a>(&'a self, _recipients: &'a [String], raw: &'a [u8]) -> BoxFuture<'a, bool> {
        self.submitted.lock().unwrap().push(raw.to_vec());
        Box::pin(async { true })
    }
}

/// Drafts are recorded (and must not be reached); Sent copies are unavailable.
#[derive(Default)]
struct Mailbox {
    drafts: Mutex<usize>,
}

impl ComposeMailbox for Mailbox {
    fn create_draft<'a>(
        &'a self,
        _input: &'a EmailDraftInput,
        _allow_append: bool,
    ) -> BoxFuture<'a, Result<EmailDraftResult, ImapError>> {
        *self.drafts.lock().unwrap() += 1;
        Box::pin(async {
            Ok(EmailDraftResult {
                draft_id: "<draft@omni-notify>".to_owned(),
                already_existed: false,
            })
        })
    }

    fn save_sent_copy<'a>(
        &'a self,
        _input: &'a SentCopyInput,
        _allow_append: bool,
        _before_append: Option<BeforeAppend>,
    ) -> BoxFuture<'a, Result<SentCopyResult, ImapError>> {
        Box::pin(async { Err(ImapError::new("discover Sent mailbox", "unavailable")) })
    }
}

struct Fixture {
    h: common::Harness,
    smtp: Arc<Smtp>,
    mailbox: Arc<Mailbox>,
    tools: Vec<McpTool>,
}

async fn mailbox(pdf: &[u8]) -> Fixture {
    let server = FakeServer::default();
    server.folder("INBOX", 1, None);
    server.put(
        "INBOX",
        7,
        FakeMessage::new(source_message(pdf)).envelope_id(Some(MESSAGE_ID)),
    );
    let h = common::harness(server).await;
    let smtp = Arc::new(Smtp::default());
    let mailbox = Arc::new(Mailbox::default());
    let reader: Arc<dyn AttachmentReader> = Arc::new(h.transport.clone());
    let compose = ComposeService::new(
        h.store.store.clone(),
        h.clock.clone(),
        Some(smtp.clone() as Arc<dyn ComposeSender>),
        Some(mailbox.clone() as Arc<dyn ComposeMailbox>),
    )
    .with_attachment_reader(Some(reader.clone()));
    let tools = email_tools(ToolDeps {
        compose,
        archive: ArchiveService::new(h.store.store.clone(), h.clock.clone()),
        archive_transport: None,
        attachments: Some(reader),
        tracker: TaskTracker::new(),
    })
    .unwrap();
    Fixture {
        h,
        smtp,
        mailbox,
        tools,
    }
}

impl Fixture {
    async fn call(&self, name: &str, input: Value) -> Result<ToolOutput, ToolError> {
        let tool = self.tools.iter().find(|t| t.meta.name == name).unwrap();
        let cx = ToolContext {
            call_id: "test".to_owned(),
            cancel: CancellationToken::new(),
        };
        tool.handler.call(input, cx).await
    }

    async fn structured(&self, name: &str, input: Value) -> Result<Value, ToolError> {
        Ok(match self.call(name, input).await? {
            ToolOutput::Structured(map)
            | ToolOutput::Custom {
                structured: map, ..
            } => Value::Object(map),
        })
    }

    fn reads(&self) -> usize {
        self.h.server.count(|c| matches!(c, FakeCall::Fetch { .. }))
    }
}

#[tokio::test]
async fn sends_exactly_the_reviewed_bytes_filename_and_mime_type_and_retries_safely() {
    let pdf = b"%PDF-1.7\n% synthetic, not a real document\n%%EOF\n".to_vec();
    let attachment_id = encode_stable_attachment_id(MESSAGE_ID, "2");
    let b = mailbox(&pdf).await;

    let ToolOutput::Custom {
        structured: reviewed,
        content,
    } = b
        .call(
            "email_attachment_get",
            json!({"messageId": MESSAGE_ID, "attachmentId": attachment_id}),
        )
        .await
        .unwrap()
    else {
        panic!("expected a downloadable resource");
    };
    let blob = content[1]["resource"]["blob"].as_str().unwrap();
    assert_eq!(STANDARD.decode(blob).unwrap(), pdf);
    let reference = json!({
        "messageId": MESSAGE_ID,
        "attachmentId": attachment_id,
        "sha256": sha256(&pdf),
    });
    assert_eq!(reviewed["filename"], json!("Résumé de test.pdf"));
    assert_eq!(reviewed["mimeType"], json!("application/pdf"));
    assert_eq!(reviewed["size"], json!(pdf.len()));
    assert_eq!(reviewed["sha256"], json!(sha256(&pdf)));
    assert_eq!(reviewed["attachmentReference"], reference);
    let text: Value = serde_json::from_str(content[0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(text["attachmentReference"], reference);

    let input = json!({
        "idempotencyKey": "e2e-reviewed-send",
        "to": "doctor@example.test",
        "subject": "Re: Your synthetic results",
        "text": "Attached as requested.",
        "inReplyTo": MESSAGE_ID,
        "attachments": [reviewed["attachmentReference"]],
    });
    let sent = b.structured("email_send", input.clone()).await.unwrap();
    assert_eq!(sent["sent"], json!(true));
    assert_eq!(sent["alreadySent"], json!(false));
    assert_eq!(
        sent["attachments"],
        json!([{
            "messageId": MESSAGE_ID,
            "attachmentId": attachment_id,
            "filename": "Résumé de test.pdf",
            "mimeType": "application/pdf",
            "size": pdf.len(),
            "sha256": sha256(&pdf),
        }])
    );
    let raw = b.smtp.submitted.lock().unwrap()[0].clone();
    let wire = omni_imap::mime::parse_message(&raw, common::NOW_MS).unwrap();
    assert_eq!(wire.in_reply_to.as_deref(), Some(MESSAGE_ID));
    assert_eq!(wire.attachments.len(), 1);
    let part = &wire.attachments[0];
    assert_eq!(part.filename.as_deref(), Some("Résumé de test.pdf"));
    assert_eq!(part.content_type, "application/pdf");
    assert_eq!(part.content_disposition.as_deref(), Some("attachment"));
    assert_eq!(part.content, pdf);

    let reads = b.reads();
    let mut expected = sent.clone();
    expected["alreadySent"] = json!(true);
    assert_eq!(b.structured("email_send", input).await.unwrap(), expected);
    assert_eq!(b.reads(), reads);
    assert_eq!(b.smtp.submitted.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn fails_closed_when_a_newer_copy_with_the_same_message_id_has_different_bytes() {
    let pdf = b"%PDF-1.7\n% reviewed synthetic copy\n%%EOF\n".to_vec();
    let attachment_id = encode_stable_attachment_id(MESSAGE_ID, "2");
    let b = mailbox(&pdf).await;
    let reviewed = b
        .structured(
            "email_attachment_get",
            json!({"messageId": MESSAGE_ID, "attachmentId": attachment_id}),
        )
        .await
        .unwrap();
    b.h.server.put(
        "INBOX",
        9,
        FakeMessage::new(source_message(b"%PDF-1.7\n% substituted copy\n%%EOF\n"))
            .envelope_id(Some(MESSAGE_ID)),
    );
    for (name, key) in [
        ("email_send", "e2e-substituted-send"),
        ("email_draft_create", "e2e-substituted-draft"),
    ] {
        let error = b
            .structured(
                name,
                json!({
                    "idempotencyKey": key,
                    "to": "doctor@example.test",
                    "subject": "Results",
                    "text": "Attached.",
                    "attachments": [reviewed["attachmentReference"]],
                }),
            )
            .await
            .unwrap_err();
        assert!(
            error
                .message
                .contains("no longer matches the reviewed sha256")
                && error.message.ends_with("Nothing was sent or saved"),
            "{}",
            error.message
        );
    }
    assert!(b.smtp.submitted.lock().unwrap().is_empty());
    assert_eq!(*b.mailbox.drafts.lock().unwrap(), 0);
}
