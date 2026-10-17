//! Boot order:
//!
//! config + redacted log -> open store -> delete retired entities ->
//! `migrate_all` over every entity -> `import_historical_costs` -> subsystem construction (intelligence,
//! live streamers, iOS controls, tasks, Reminders; `Migrate` and `Services`
//! boot steps) -> `registry.initialize` (interrupted runs) -> `Reconcile`
//! boot steps (`markInterruptedCalls`, calendar hash reconcile) -> HTTP
//! server -> `AfterServer` boot steps -> register tasks -> background
//! services (email features with 30 s..300 s restarts, MCP delivery worker,
//! Reminders health check) -> scheduler -> catch-up recovery (tracked).
//!
//! Subsystems are constructed before `migrate_all` because the entity list
//! comes from them; constructors do not read the docstore.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use omni_alerts::AlertGate;
use omni_runtime::{AppContext, BootError, BootPhase, BootStep, Subsystem};
use omni_store::DocWrite as _;
use omni_store::entity::{EntityDescriptor, migrate_all};
use omni_tasks::{CronSchedule, Scheduler, Task};
use tokio::net::TcpListener;

use crate::data_manager::{DataManager, foundation_entities, order_managed};
use crate::ops::{OpsState, router as ops_router};

const LOG: &str = "Main";
/// How long shutdown waits for tracked work.
pub const SHUTDOWN_BOUND: Duration = Duration::from_secs(30);

/// Records boot milestones in order (the boot-order test reads it).
#[derive(Clone, Default)]
pub struct BootTrace(Arc<Mutex<Vec<String>>>);

impl BootTrace {
    pub fn record(&self, event: impl Into<String>) {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(event.into());
    }

    pub fn events(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

/// A boot failure; the process exits 1.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error(transparent)]
    Step(#[from] BootError),
    #[error("{0}")]
    Other(String),
}

impl AppError {
    fn other(context: &str, error: impl std::fmt::Display) -> Self {
        AppError::Other(format!("{context}: {error}"))
    }
}

/// Foundation entities (task runs, costs) plus every subsystem's.
pub fn all_entities(subsystems: &[Subsystem]) -> Vec<EntityDescriptor> {
    let mut entities = vec![
        EntityDescriptor::of::<omni_tasks::persistence::TaskRunData>(),
        EntityDescriptor::of::<omni_tasks::persistence::TaskRunLog>(),
        EntityDescriptor::of::<omni_tasks::persistence::TaskScheduleState>(),
        EntityDescriptor::of::<omni_tasks::health::TaskHealthIncident>(),
        EntityDescriptor::of::<omni_ai::costs::CostEventData>(),
        EntityDescriptor::of::<omni_ai::costs::CostMigrationData>(),
    ];
    let mut seen: std::collections::HashSet<&'static str> =
        entities.iter().map(|e| e.name).collect();
    for descriptor in subsystems.iter().flat_map(|s| &s.entities) {
        if seen.insert(descriptor.name) {
            entities.push(*descriptor);
        }
    }
    entities
}

/// Entities of removed features (briefings, workspaces). Their rows are
/// deleted at boot; nothing reads them.
pub const RETIRED_ENTITIES: &[&str] = &[
    "briefing-history",
    "briefing-delivery",
    "workspace-subject",
    "workspace-artifact-revision",
    "workspace-message",
    "workspace-source",
    "workspace-action",
    "workspace-email-scope",
    "workspace-papercut",
    "workspace-notification",
];

/// Deletes retired entities, then `Entity.migrateAll()` and
/// `importHistoricalCosts()`.
pub async fn migrate(
    ctx: &AppContext,
    entities: Vec<EntityDescriptor>,
    trace: &BootTrace,
) -> Result<(), AppError> {
    let retired = ctx
        .store
        .write(|tx| {
            RETIRED_ENTITIES.iter().try_fold(0, |sum, name| {
                Ok::<_, omni_store::StoreError>(sum + tx.delete_docs_by_entity(name)?)
            })
        })
        .await
        .map_err(|e| AppError::other("delete retired entities", e))?;
    trace.record("delete_retired_entities");
    if retired > 0 {
        tracing::info!(target: LOG, "Deleted {retired} row(s) of retired entities");
    }
    let report = ctx
        .store
        .write(move |tx| migrate_all(tx, &entities))
        .await
        .map_err(|e| AppError::other("migrate entities", e))?;
    trace.record("migrate_all");
    if report.migrated > 0 {
        tracing::info!(target: LOG, "Migrated {} entity row(s)", report.migrated);
    }
    if report.failed > 0 || report.collisions_skipped > 0 {
        tracing::warn!(
            target: LOG,
            "Entity migration left {} row(s) undecodable and skipped {} key collision(s)",
            report.failed,
            report.collisions_skipped
        );
    }
    let imported = omni_ai::costs::import_historical_costs(&ctx.store)
        .await
        .map_err(|e| AppError::other("import historical costs", e))?;
    trace.record("import_historical_costs");
    if imported > 0 {
        tracing::info!(target: LOG, "Imported {imported} historical cost event(s)");
    }
    Ok(())
}

/// Takes every subsystem's boot steps of `phase` and runs them in order.
pub async fn run_steps(
    ctx: &AppContext,
    subsystems: &mut [Subsystem],
    phase: BootPhase,
    trace: &BootTrace,
) -> Result<(), AppError> {
    let mut steps: Vec<BootStep> = Vec::new();
    for subsystem in subsystems.iter_mut() {
        let (matching, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut subsystem.boot_steps)
            .into_iter()
            .partition(|step| step.phase == phase);
        subsystem.boot_steps = rest;
        steps.extend(matching);
    }
    for step in steps {
        let name = step.name;
        (step.run)(ctx.clone()).await?;
        trace.record(format!("step:{phase:?}:{name}"));
    }
    Ok(())
}

/// Options of a serving process.
#[derive(Clone, Debug)]
pub struct ServeOptions {
    pub server_only: bool,
    pub web_dist: PathBuf,
}

/// The fully assembled router (ops routes, every subsystem, SPA fallback).
pub fn app_router(
    ctx: &AppContext,
    subsystems: &mut [Subsystem],
    web_dist: &std::path::Path,
) -> (axum::Router, OpsState) {
    let mut managed = foundation_entities();
    for subsystem in subsystems.iter() {
        managed.extend(subsystem.managed_entities.iter().cloned());
    }
    let data = DataManager::new(ctx.store.clone(), order_managed(managed));
    let ops = OpsState::new(ctx.clone(), data);
    let mut parts = vec![ops_router(ops.clone())];
    for subsystem in subsystems.iter_mut() {
        parts.push(std::mem::take(&mut subsystem.router));
    }
    let dist = web_dist
        .join("index.html")
        .is_file()
        .then(|| web_dist.to_path_buf());
    if dist.is_none() {
        tracing::warn!(
            target: LOG,
            "No frontend build at {}; serving the API only",
            web_dist.display()
        );
    }
    (omni_server_kit::app_router(parts, dist), ops)
}

/// Registers tasks in [`crate::wiring::TASK_ORDER`].
pub fn track_tasks(ctx: &AppContext, subsystems: &mut [Subsystem]) -> Result<(), AppError> {
    let mut tasks: Vec<Arc<dyn Task>> = Vec::new();
    for subsystem in subsystems.iter_mut() {
        tasks.append(&mut subsystem.tasks);
    }
    tasks.sort_by_key(|task| crate::wiring::task_rank(task.name()));
    for task in tasks {
        ctx.tasks
            .track(task)
            .map_err(|e| AppError::other("register task", e))?;
    }
    Ok(())
}

/// The alert gates every ERROR log passes (Castro's consecutive-failure gate).
pub fn install_alert_gates(
    gates: &Arc<RwLock<Vec<Arc<dyn AlertGate>>>>,
    subsystems: &mut [Subsystem],
) {
    let mut installed = gates
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for subsystem in subsystems.iter_mut() {
        installed.append(&mut subsystem.alert_gates);
    }
}

/// Runs the server until `ctx.shutdown`, then drains tracked work.
pub async fn run(
    ctx: AppContext,
    mut subsystems: Vec<Subsystem>,
    options: ServeOptions,
    listener: TcpListener,
    gates: Arc<RwLock<Vec<Arc<dyn AlertGate>>>>,
    trace: BootTrace,
) -> Result<(), AppError> {
    migrate(&ctx, all_entities(&subsystems), &trace).await?;
    run_steps(&ctx, &mut subsystems, BootPhase::Migrate, &trace).await?;
    run_steps(&ctx, &mut subsystems, BootPhase::Services, &trace).await?;
    install_alert_gates(&gates, &mut subsystems);
    ctx.tasks
        .initialize()
        .await
        .map_err(|e| AppError::other("mark interrupted runs", e))?;
    trace.record("registry.initialize");
    run_steps(&ctx, &mut subsystems, BootPhase::Reconcile, &trace).await?;

    let (router, ops) = app_router(&ctx, &mut subsystems, &options.web_dist);
    let port = listener.local_addr().map(|a| a.port()).unwrap_or_default();
    let shutdown = ctx.shutdown.clone();
    let server = omni_core::spawn::spawn_tracked(&ctx.tracker, "http-server", async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(async move { shutdown.cancelled().await })
            .await
    });
    tracing::info!(target: "Main:Server", "Server listening on port {port}");
    trace.record("server");
    omni_core::spawn::spawn_tracked(
        &ctx.tracker,
        "dashboard-hub",
        ops.dashboard.clone().listen(),
    );
    run_steps(&ctx, &mut subsystems, BootPhase::AfterServer, &trace).await?;

    if options.server_only {
        tracing::info!(target: LOG, "Running in server-only mode (tasks disabled)");
    } else {
        track_tasks(&ctx, &mut subsystems)?;
        trace.record("tasks");
        let mut services = Vec::new();
        for subsystem in &mut subsystems {
            services.append(&mut subsystem.services);
        }
        let tz = jiff::tz::TimeZone::get(&ctx.config.tz).unwrap_or(jiff::tz::TimeZone::UTC);
        let schedule = CronSchedule::parse(crate::maintenance::SCHEDULE, &tz)
            .map_err(|e| AppError::other("StoreMaintenance schedule", e))?;
        services.push(crate::maintenance::service(schedule));
        services.push(crate::task_health::service());
        crate::services::start_all(&ctx, services);
        trace.record("services");
        Scheduler::start(ctx.tasks.clone(), ctx.shutdown.clone(), &ctx.tracker);
        trace.record("scheduler");
        let registry = ctx.tasks.clone();
        omni_core::spawn::spawn_tracked(&ctx.tracker, "catch-up", async move {
            if let Err(error) = registry.recover_missed().await {
                tracing::error!(target: LOG, error = %error, "Failed to recover missed task runs");
            }
        });
        trace.record("catch_up");
    }

    ctx.shutdown.cancelled().await;
    tracing::info!(target: LOG, "Shutting down");
    ctx.tracker.close();
    if tokio::time::timeout(SHUTDOWN_BOUND, ctx.tracker.wait())
        .await
        .is_err()
    {
        tracing::warn!(target: LOG, "Shutdown timed out with work still running");
    }
    match server.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(AppError::other("HTTP server", error)),
        Err(error) => Err(AppError::other("HTTP server", error)),
    }
}

/// `--run-task <Name>`: runs one task once outside the registry (no run
/// history), after the same migration and construction steps as a boot.
pub async fn run_task_once(
    ctx: &AppContext,
    mut subsystems: Vec<Subsystem>,
    name: &str,
) -> Result<bool, AppError> {
    let trace = BootTrace::default();
    migrate(ctx, all_entities(&subsystems), &trace).await?;
    run_steps(ctx, &mut subsystems, BootPhase::Migrate, &trace).await?;
    run_steps(ctx, &mut subsystems, BootPhase::Services, &trace).await?;
    let mut tasks: Vec<Arc<dyn Task>> = subsystems
        .iter_mut()
        .flat_map(|s| std::mem::take(&mut s.tasks))
        .filter(|t| crate::wiring::runnable_from_cli(t.name()))
        .collect();
    tasks.sort_by_key(|task| crate::wiring::task_rank(task.name()));
    let Some(task) = tasks
        .iter()
        .find(|t| t.name().to_lowercase() == name.to_lowercase())
    else {
        let available: Vec<&str> = tasks.iter().map(|t| t.name()).collect();
        tracing::error!(
            target: LOG,
            "Unknown task \"{name}\". Available: {}",
            available.join(", ")
        );
        return Ok(false);
    };
    tracing::info!(target: LOG, "Running task \"{}\" once...", task.name());
    let cx = omni_tasks::RunContext {
        run_id: omni_tasks::persistence::make_run_id(task.name()),
        task_name: task.name().to_owned(),
        trigger: omni_tasks::Trigger::Manual,
        scheduled_for: None,
        cancel: ctx.shutdown.clone(),
    };
    let result = task.run(&cx).await;
    ctx.shutdown.cancel();
    ctx.tracker.close();
    let _ = tokio::time::timeout(SHUTDOWN_BOUND, ctx.tracker.wait()).await;
    match result {
        Ok(()) => {
            tracing::info!(target: LOG, "Task \"{}\" complete", task.name());
            Ok(true)
        }
        Err(error) => Err(AppError::other(&format!("Task \"{}\"", task.name()), error)),
    }
}
