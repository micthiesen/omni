//! Wiring surface: tools, task, entities, data-manager row and handles.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::sync::Arc;

use omni_imap::fake::FakeServer;
use omni_testkit::TestApp;

const TOOLS: [&str; 9] = [
    "email_draft_create",
    "email_send",
    "email_send_status",
    "email_sent_copy_repair",
    "email_archive_queue",
    "email_archive_status",
    "email_archive_cancel",
    "email_archive_restore",
    "email_attachment_get",
];

#[tokio::test]
async fn disabled_without_icloud_credentials_but_tools_and_entities_register() {
    let app = TestApp::new().await;
    let mut ctx = app.ctx.clone();
    let mut config = (*ctx.config).clone();
    config.icloud_username = None;
    config.icloud_app_password = None;
    ctx.config = Arc::new(config);
    let (subsystem, handles) =
        omni_imap::subsystem(&ctx, Arc::new(common::PlainEnricher), None).unwrap();
    let names: Vec<&str> = subsystem
        .mcp_tools
        .iter()
        .map(|t| t.meta.name.as_str())
        .collect();
    assert_eq!(names, TOOLS);
    assert!(subsystem.tasks.is_empty());
    assert!(
        handles.transport.is_none()
            && handles.email_reader().is_none()
            && handles.mail_source().is_none()
    );
    assert_eq!(
        subsystem
            .entities
            .iter()
            .map(|e| e.name)
            .collect::<Vec<_>>(),
        vec!["imap-folder-cursor"]
    );
    assert_eq!(subsystem.managed_entities[0].label, "Email IMAP cursors");
}

#[tokio::test]
async fn configured_transport_registers_the_archive_task_and_recovery_service() {
    let app = TestApp::new().await;
    let mut ctx = app.ctx.clone();
    let mut config = (*ctx.config).clone();
    config.icloud_username = Some("me@icloud.test".to_owned());
    config.icloud_app_password = Some("app-password".to_owned());
    ctx.config = Arc::new(config);
    let server = FakeServer::default();
    server.folder("INBOX", 1, None);
    let (subsystem, handles) = omni_imap::subsystem(
        &ctx,
        Arc::new(common::PlainEnricher),
        Some(server.connector()),
    )
    .unwrap();
    assert_eq!(
        subsystem
            .tasks
            .iter()
            .map(|t| t.name().to_owned())
            .collect::<Vec<_>>(),
        vec!["EmailArchive"]
    );
    assert_eq!(subsystem.services.len(), 1);
    let transport = handles.transport.clone().unwrap();
    assert!(!transport.is_active());
    transport.start().await.unwrap();
    assert!(transport.is_active());
    assert!(handles.email_reader().unwrap().health().search_available);
    transport.stop().await;
}
