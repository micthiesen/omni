//! Email pipeline core: dispatcher, durable retry queue and task,
//! watchdog, activity and activity logs, shared LLM triage, sender rules,
//! feedback, link metadata, HTML to text, REST routes and MCP tools.
//!
//! Pipelines (`omni-parcel`, `omni-calendar`) use the library functions
//! directly: [`activity::record`], [`retry::enqueue`],
//! [`activity_logs::with_capture`], [`triage::EmailTriage`] and
//! [`sender_rules::find_sender_rule`].
//!
//! App wiring: build one [`triage::EmailTriage`] and hand it to every
//! pipeline; register [`subsystem`]; start the dispatcher with
//! [`dispatcher::service`] once the mail source and the ordered handlers
//! (McpEvents, ParcelTracker, CalendarEvents) exist; set the
//! `EmailReader` and `EmailRetryHandlers` ports for retry and reprocess.

pub mod activity;
pub mod activity_logs;
pub mod builtin;
pub mod dispatch_state;
pub mod dispatcher;
pub mod feedback;
pub mod html_to_text;
pub mod link_metadata;
pub mod mcp_tools;
pub mod reprocess;
pub mod retry;
pub mod retry_task;
pub mod routes;
pub mod sender_rules;
pub mod systemic;
pub mod triage;
pub mod watchdog;

use omni_runtime::{AppContext, BootError, BootPhase, BootStep, ManagedEntity, Subsystem};
use omni_store::entity::EntityDescriptor;

/// Why the email subsystem could not be built.
#[derive(Debug, thiserror::Error)]
pub enum EmailSubsystemError {
    #[error(transparent)]
    Schedule(#[from] omni_tasks::InvalidScheduleError),
    #[error(transparent)]
    Tools(#[from] omni_mcp_kit::ToolMetaError),
}

/// Every entity this crate persists (for `migrate_all` and the compat audit).
pub fn entities() -> Vec<EntityDescriptor> {
    vec![
        EntityDescriptor::of::<activity::EmailActivityData>(),
        EntityDescriptor::of::<activity_logs::EmailActivityLog>(),
        EntityDescriptor::of::<dispatch_state::EmailDispatchData>(),
        EntityDescriptor::of::<retry::EmailRetryData>(),
        EntityDescriptor::of::<sender_rules::EmailRuleData>(),
        EntityDescriptor::of::<feedback::EmailFeedbackData>(),
        EntityDescriptor::of::<systemic::EmailReplayData>(),
        EntityDescriptor::of::<systemic::EmailSystemicAlertData>(),
    ]
}

/// Data manager rows, in display order.
pub fn managed_entities() -> Vec<ManagedEntity> {
    let managed = |entity: EntityDescriptor,
                   label: &'static str,
                   description: &'static str,
                   primary_key: &'static [&'static str]| ManagedEntity {
        slug: entity.name,
        label,
        description,
        warning: None,
        entity,
        primary_key,
        can_delete: None,
        after_delete: None,
    };
    let after_activity_delete: omni_runtime::AfterDelete =
        std::sync::Arc::new(|row, store: omni_store::Store| {
            Box::pin(async move {
                let Some(id) = row
                    .get("activityId")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned)
                else {
                    return Ok(());
                };
                store
                    .write(move |tx| {
                        omni_store::entity::EntityWrite::delete::<activity_logs::EmailActivityLog>(
                            tx, &id,
                        )
                    })
                    .await
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            })
        });
    vec![
        ManagedEntity {
            after_delete: Some(after_activity_delete),
            ..managed(
                EntityDescriptor::of::<activity::EmailActivityData>(),
                "Email activity",
                "Per-email outcomes from the parcel and calendar pipelines.",
                &["activityId"],
            )
        },
        managed(
            EntityDescriptor::of::<activity_logs::EmailActivityLog>(),
            "Email activity logs",
            "Captured log lines for emails that reached pipeline processing.",
            &["activityId"],
        ),
        managed(
            EntityDescriptor::of::<dispatch_state::EmailDispatchData>(),
            "Email dispatch state",
            "Last email dispatch timestamp, used by the pipeline watchdog.",
            &["key"],
        ),
        managed(
            EntityDescriptor::of::<retry::EmailRetryData>(),
            "Email retries",
            "Emails queued for reprocessing after transient failures, or parked until a new build after systemic ones.",
            &["retryKey"],
        ),
        managed(
            EntityDescriptor::of::<systemic::EmailReplayData>(),
            "Email replays",
            "Builds each systemically failed email was replayed under; limits replays to one per build.",
            &["retryKey"],
        ),
        managed(
            EntityDescriptor::of::<systemic::EmailSystemicAlertData>(),
            "Email failure signatures",
            "Systemic extraction failures by error signature; one alert per signature per day.",
            &["alertKey"],
        ),
        managed(
            EntityDescriptor::of::<sender_rules::EmailRuleData>(),
            "Email sender rules",
            "User-defined block/allow rules merged into the email filters.",
            &["ruleId"],
        ),
        managed(
            EntityDescriptor::of::<feedback::EmailFeedbackData>(),
            "Email feedback",
            "Explicit outcome corrections injected into email triage prompts.",
            &["activityId"],
        ),
    ]
}

/// The email subsystem: routes, `EmailWatchdog` and `EmailRetry`, the email
/// MCP tools, entities and data-manager rows. `booted_at` is the process
/// start (epoch ms), the watchdog's stand-in for a first dispatch.
pub fn subsystem(ctx: &AppContext, booted_at: i64) -> Result<Subsystem, EmailSubsystemError> {
    let tz = jiff::tz::TimeZone::get(&ctx.config.tz).unwrap_or(jiff::tz::TimeZone::UTC);
    let state = routes::EmailRoutesState {
        store: ctx.store.clone(),
        ports: ctx.ports.clone(),
    };
    let router =
        routes::router(state.clone()).merge(routes::rules_router(state, ctx.owner.clone()));
    let tools = mcp_tools::tools(&mcp_tools::EmailTools {
        store: ctx.store.clone(),
        ports: ctx.ports.clone(),
        config: ctx.config.clone(),
    })?;
    Ok(Subsystem {
        router,
        tasks: vec![
            watchdog::task(ctx.store.clone(), &tz, booted_at)?,
            retry_task::task(ctx.store.clone(), ctx.ports.clone(), &tz)?,
        ],
        mcp_tools: tools,
        entities: entities(),
        managed_entities: managed_entities(),
        boot_steps: vec![replay_boot_step()],
        ..Subsystem::named("email")
    })
}

/// Releases emails parked by a systemic failure when a different build boots.
/// A failure is logged and never fails boot: the rows stay parked.
fn replay_boot_step() -> BootStep {
    const NAME: &str = "email-systemic-replay";
    BootStep {
        phase: BootPhase::Reconcile,
        name: NAME,
        run: Box::new(|ctx: AppContext| {
            Box::pin(async move {
                let build = systemic::current_build().await;
                match systemic::release_for_build(
                    &ctx.store,
                    &build,
                    systemic::MAX_RELEASES_PER_BOOT,
                )
                .await
                {
                    Ok(report) if report == systemic::ReleaseReport::default() => {}
                    Ok(report) => tracing::info!(
                        target: "Main:EmailRetry",
                        "Build {build}: released {} systemically failed email(s) for replay, {} deferred to a later boot, {} expired",
                        report.released,
                        report.deferred,
                        report.expired
                    ),
                    Err(error) => tracing::warn!(
                        target: "Main:EmailRetry",
                        "Could not release systemically failed emails for replay: {error}"
                    ),
                }
                Ok::<(), BootError>(())
            })
        }),
    }
}
