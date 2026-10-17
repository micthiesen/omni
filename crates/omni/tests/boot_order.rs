//! Boot order with stub subsystems, server-only mode,
//! and a clean shutdown.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use omni::boot::{self, BootTrace, ServeOptions};
use omni_runtime::{AppContext, BackgroundService, BootPhase, BootStep, Subsystem};
use omni_store::cbor::JsValue;
use omni_store::{DocMeta, DocOps as _, DocWrite as _};
use omni_tasks::{CronSchedule, RunContext, Task, TaskError, TaskOptions};
use omni_testkit::TestApp;

struct Noop(CronSchedule);

impl Task for Noop {
    fn name(&self) -> &str {
        "Noop"
    }
    fn schedule(&self) -> &CronSchedule {
        &self.0
    }
    fn options(&self) -> TaskOptions {
        TaskOptions::default()
    }
    fn run<'a>(&'a self, _cx: &'a RunContext) -> BoxFuture<'a, Result<(), TaskError>> {
        Box::pin(async { Ok(()) })
    }
}

fn step(phase: BootPhase, name: &'static str, log: Arc<Mutex<Vec<String>>>) -> BootStep {
    BootStep {
        phase,
        name,
        run: Box::new(move |_ctx: AppContext| {
            Box::pin(async move {
                log.lock().unwrap().push(name.to_owned());
                Ok(())
            })
        }),
    }
}

fn stub(log: &Arc<Mutex<Vec<String>>>) -> Subsystem {
    let mut subsystem = Subsystem::named("stub");
    // Declared out of order: phases, not declaration order, decide.
    subsystem.boot_steps = vec![
        step(BootPhase::AfterServer, "after-server", log.clone()),
        step(BootPhase::Reconcile, "reconcile", log.clone()),
        step(BootPhase::Migrate, "migrate", log.clone()),
        step(BootPhase::Services, "services", log.clone()),
    ];
    subsystem.tasks = vec![Arc::new(Noop(
        CronSchedule::parse("0 0 0 1 1 *", &jiff::tz::TimeZone::UTC).unwrap(),
    ))];
    let service_log = log.clone();
    subsystem.services = vec![BackgroundService {
        name: "stub-service",
        start: Box::new(move |ctx: AppContext| {
            let log = service_log.clone();
            Box::pin(async move {
                log.lock().unwrap().push("service-started".to_owned());
                ctx.shutdown.cancelled().await;
            })
        }),
        retry: None,
    }];
    subsystem
}

async fn boot(server_only: bool) -> (Vec<String>, Vec<String>, Vec<String>) {
    let app = TestApp::new().await;
    let log: Arc<Mutex<Vec<String>>> = Arc::default();
    let trace = BootTrace::default();
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let ctx = app.ctx.clone();
    let run = tokio::spawn(boot::run(
        ctx.clone(),
        vec![stub(&log)],
        ServeOptions {
            server_only,
            web_dist: "/nonexistent".into(),
        },
        listener,
        Arc::default(),
        trace.clone(),
    ));
    let last = if server_only {
        "step:AfterServer:after-server"
    } else {
        "catch_up"
    };
    for _ in 0..200 {
        if trace.events().iter().any(|e| e == last) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    let tasks: Vec<String> = ctx.tasks.names();
    ctx.shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(10), run)
        .await
        .expect("shutdown completes")
        .unwrap()
        .unwrap();
    let steps = log.lock().unwrap().clone();
    (trace.events(), steps, tasks)
}

#[tokio::test]
async fn boots_in_order_and_shuts_down() {
    let (events, steps, tasks) = boot(false).await;
    assert_eq!(
        events,
        vec![
            "delete_retired_entities",
            "migrate_all",
            "import_historical_costs",
            "step:Migrate:migrate",
            "step:Services:services",
            "registry.initialize",
            "step:Reconcile:reconcile",
            "server",
            "step:AfterServer:after-server",
            "tasks",
            "services",
            "scheduler",
            "catch_up",
        ]
    );
    assert_eq!(
        steps,
        vec![
            "migrate",
            "services",
            "reconcile",
            "after-server",
            "service-started"
        ]
    );
    assert_eq!(tasks, vec!["Noop"]);
}

#[tokio::test]
async fn server_only_registers_no_tasks_and_starts_no_services() {
    let (events, steps, tasks) = boot(true).await;
    assert_eq!(
        events.last().map(String::as_str),
        Some("step:AfterServer:after-server")
    );
    assert!(!events.iter().any(|e| e == "tasks" || e == "scheduler"));
    assert!(!steps.iter().any(|s| s == "service-started"));
    assert!(tasks.is_empty());
}

#[tokio::test]
async fn boot_deletes_rows_of_retired_entities_only() {
    let app = TestApp::new().await;
    let meta = |entity: &str| DocMeta {
        entity: Some(entity.to_owned()),
        ..DocMeta::default()
    };
    app.ctx
        .store
        .write(move |tx| {
            tx.upsert_doc(
                "workspace-subject:a",
                &JsValue::Null,
                meta("workspace-subject"),
            )?;
            tx.upsert_doc(
                "briefing-history:b",
                &JsValue::Null,
                meta("briefing-history"),
            )?;
            tx.upsert_doc("kept:c", &JsValue::Null, meta("kept"))
        })
        .await
        .unwrap();
    boot::migrate(&app.ctx, Vec::new(), &BootTrace::default())
        .await
        .unwrap();
    let counts = app
        .ctx
        .store
        .read(|docs| {
            Ok::<_, omni_store::StoreError>((
                docs.count_by_entity("workspace-subject")?,
                docs.count_by_entity("briefing-history")?,
                docs.count_by_entity("kept")?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(counts, (0, 0, 1));
}
