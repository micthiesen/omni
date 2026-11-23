//! Subsystem wiring: what the app receives with and without CalDAV credentials,
//! and the `CalendarConnection` port.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::sync::Arc;

use common::{FakeSupport, NoAttachments};
use omni_calendar::{CalendarDeps, subsystem};
use omni_runtime::BootPhase;
use omni_testkit::TestApp;

fn deps() -> CalendarDeps {
    CalendarDeps {
        email: FakeSupport::new(FakeSupport::calendar_yes()),
        attachments: Arc::new(NoAttachments),
    }
}

#[tokio::test]
async fn without_credentials_only_tools_and_entities_are_registered() {
    let app = TestApp::new().await;
    let s = subsystem(&app.ctx, deps()).unwrap();
    assert_eq!(s.name, "CalendarEvents");
    assert_eq!(s.mcp_tools.len(), 11);
    let names: Vec<&str> = s.entities.iter().map(|e| e.name).collect();
    assert_eq!(
        names,
        [
            "calendar-created-event",
            "calendar-primary-pin",
            "calendar-primary-state",
            "calendar-primary-resource",
            "calendar-primary-change",
            "calendar-primary-write-echo",
            "calendar-mcp-operation",
        ]
    );
    assert_eq!(s.managed_entities[0].slug, "calendar-created-event");
    assert_eq!(s.managed_entities[0].primary_key, ["eventHash"]);
    assert!(s.email_handlers.is_empty());
    assert!(s.boot_steps.is_empty());
    assert!(s.tasks.is_empty());
    let status = omni_calendar::calendar_connection(&app.ctx).status();
    assert!(!status.configured);
}

#[tokio::test]
async fn with_credentials_the_handler_and_reconcile_step_are_registered() {
    let mut app = TestApp::new().await;
    let mut config = (*app.ctx.config).clone();
    config.icloud_username = Some("user@icloud.com".to_owned());
    config.icloud_app_password = Some("app-password".to_owned());
    app.ctx.config = Arc::new(config);
    let s = subsystem(&app.ctx, deps()).unwrap();
    assert_eq!(s.email_handlers.len(), 1);
    assert_eq!(s.email_handlers[0].name(), "CalendarEvents");
    assert_eq!(s.boot_steps.len(), 2);
    assert!(s.boot_steps.iter().all(|b| b.phase == BootPhase::Reconcile));
    let tasks: Vec<&str> = s.tasks.iter().map(|t| t.name()).collect();
    assert_eq!(tasks, ["CalendarPrimarySync", "CalendarStartingEvents"]);
    for step in s.boot_steps {
        (step.run)(app.ctx.clone()).await.unwrap();
    }
}

/// Never consulted: the subsystem is only built here.
struct UnusedClassifier;

impl omni_email::triage::TriageClassifier for UnusedClassifier {
    fn classify(
        &self,
        email: omni_email::triage::TriageEmail,
    ) -> futures::future::BoxFuture<
        'static,
        Result<omni_email::triage::Classified, omni_email::triage::TriageError>,
    > {
        Box::pin(async move {
            Err(omni_email::triage::TriageError {
                email_id: email.id,
                message: "unused".to_owned(),
            })
        })
    }
}

#[tokio::test]
async fn production_deps_wire_the_handler_over_the_email_core() {
    let mut app = TestApp::new().await;
    let mut config = (*app.ctx.config).clone();
    config.icloud_username = Some("user@icloud.com".to_owned());
    config.icloud_app_password = Some("app-password".to_owned());
    app.ctx.config = Arc::new(config);
    let deps = CalendarDeps::new(
        &app.ctx,
        omni_email::triage::EmailTriage::new(Arc::new(UnusedClassifier)),
    );
    let s = subsystem(&app.ctx, deps).unwrap();
    assert_eq!(s.email_handlers.len(), 1);
    assert_eq!(s.email_handlers[0].name(), "CalendarEvents");
}
