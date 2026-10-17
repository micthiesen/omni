//! Personal services: Whisker pet weights, the LAN printer, the Hister
//! browser-history archive, Codex / Claude Code reset alerts, and agent push
//! notifications to the owner.
//!
//! [`subsystem`] builds the routes (`/api/pets`, `/api/pets/health`, CSV
//! export), the tasks (`PetTracker`, `CodexResets`, `ClaudeResets`), the MCP
//! tools (`pets_read`, `costs_read`, printer, browser-history and
//! `notify_owner_*` tools), the PetTracker data-gap alert gate and the entity
//! descriptors (`codex-reset-delivery`, `claude-reset-delivery`,
//! `printer-accepted-job`, `pet-health-alert`, `pet-health-dismissal`,
//! `owner-notification`) for app wiring.

use std::sync::Arc;
use std::time::Duration;

use jiff::tz::TimeZone;
use omni_alerts::TaskFailureGate;
use omni_runtime::{AppContext, Subsystem};
use omni_store::entity::EntityDescriptor;
use omni_tasks::Task;
use omni_tasks::health::SUSTAINED_FAILURE;

pub mod claude_resets;
pub mod codex_resets;
pub mod hister;
pub mod mcp;
pub mod owner_notify;
pub mod pets;
pub mod printer;
pub mod reset_alerts;

use crate::claude_resets::task::{ClaudeSource, claude_reset_task};
use crate::codex_resets::task::{CodexSource, codex_reset_task};
use crate::hister::HisterService;
use crate::owner_notify::{OwnerNotification, OwnerNotifier};
use crate::pets::alerts::{
    HealthLedger, HealthNotifier, PetGapAlertGate, PetHealthAlert, PushoverHealthNotifier,
};
use crate::pets::api::WhiskerApi;
use crate::pets::auth::WhiskerAuth;
use crate::pets::dismissals::PetHealthDismissal;
use crate::pets::persistence::PetStore;
use crate::pets::task::PetTrackerTask;
use crate::printer::ipp::IppPrinterClient;
use crate::printer::pipeline::{PublicPdfDownloader, SystemProcesses};
use crate::printer::service::{
    DEFAULT_POLL_INTERVAL, DurableAcceptedPrintStore, MAX_PRINT_DATA_BYTES,
};
use crate::printer::{AcceptedPrintRecord, PrinterDependencies, PrinterService};
use crate::reset_alerts::{
    ClaudeResetDelivery, CodexResetDelivery, PushoverNotifier, ResetDeliveryLedger, ResetNotifier,
};

/// The subsystem name.
pub const NAME: &str = "personal";
const PRINTER_TIMEOUT: Duration = Duration::from_secs(120);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(20);

/// Construction failures (all configuration or schema problems; fail boot).
#[derive(Debug, thiserror::Error)]
pub enum PersonalError {
    #[error("invalid TZ {tz:?}: {reason}")]
    TimeZone { tz: String, reason: String },
    #[error(transparent)]
    Store(#[from] omni_store::StoreError),
    #[error(transparent)]
    Schedule(#[from] omni_tasks::InvalidScheduleError),
    #[error(transparent)]
    ToolMeta(#[from] omni_mcp_kit::ToolMetaError),
    #[error("printer: {0}")]
    Printer(String),
    #[error(transparent)]
    Hister(#[from] hister::HisterError),
}

/// Entity descriptors for `migrate_all` and the compat audit.
pub fn entities() -> Vec<EntityDescriptor> {
    vec![
        EntityDescriptor::of::<CodexResetDelivery>(),
        EntityDescriptor::of::<ClaudeResetDelivery>(),
        EntityDescriptor::of::<AcceptedPrintRecord>(),
        EntityDescriptor::of::<PetHealthAlert>(),
        EntityDescriptor::of::<PetHealthDismissal>(),
        EntityDescriptor::of::<OwnerNotification>(),
    ]
}

fn time_zone(tz: &str) -> Result<TimeZone, PersonalError> {
    TimeZone::get(tz).map_err(|e| PersonalError::TimeZone {
        tz: tz.to_owned(),
        reason: e.to_string(),
    })
}

/// The printer service when `PRINTER_IPP_URL` is configured.
pub fn printer_service(ctx: &AppContext) -> Result<Option<Arc<PrinterService>>, PersonalError> {
    let Some(url) = ctx
        .config
        .printer_ipp_url
        .as_deref()
        .filter(|u| !u.is_empty())
    else {
        return Ok(None);
    };
    #[allow(clippy::cast_possible_truncation)]
    let seed = (ctx.clock.now_ms() & 0x7fff_ffff) as i32;
    let client = IppPrinterClient::new(
        ctx.http.clone(),
        url,
        PRINTER_TIMEOUT,
        ctx.side_effects,
        seed,
    )
    .map_err(PersonalError::Printer)?;
    Ok(Some(Arc::new(PrinterService::new(PrinterDependencies {
        printer: Some(Arc::new(client)),
        download: Arc::new(PublicPdfDownloader::new(
            ctx.public_http.clone(),
            DOWNLOAD_TIMEOUT,
        )),
        processes: Arc::new(SystemProcesses),
        accepted: Arc::new(DurableAcceptedPrintStore::new(ctx.store.clone())),
        clock: ctx.clock.clone(),
        temp_root: std::env::temp_dir(),
        max_print_data_bytes: MAX_PRINT_DATA_BYTES,
        poll_interval: DEFAULT_POLL_INTERVAL,
    }))))
}

/// The Hister client when `HISTER_ACCESS_TOKEN` is configured.
pub fn hister_service(ctx: &AppContext) -> Result<Option<Arc<HisterService>>, PersonalError> {
    let Some(token) = ctx
        .config
        .hister_access_token
        .as_deref()
        .filter(|t| !t.is_empty())
    else {
        return Ok(None);
    };
    Ok(Some(Arc::new(HisterService::new(
        &ctx.config.hister_url,
        token,
        ctx.http.clone(),
        ctx.side_effects,
    )?)))
}

/// Builds the subsystem.
pub async fn subsystem(ctx: &AppContext) -> Result<Subsystem, PersonalError> {
    let tz = time_zone(&ctx.config.tz)?;
    let pets = PetStore::open(&ctx.store).await?;

    let health_notifier = PushoverHealthNotifier::new(ctx.pushover.clone());
    let health_notifier: Option<Arc<dyn HealthNotifier>> = health_notifier
        .enabled()
        .then(|| Arc::new(health_notifier) as Arc<dyn HealthNotifier>);
    let ledger = HealthLedger::new(ctx.store.clone(), health_notifier);

    let mut tasks: Vec<Arc<dyn Task>> = Vec::new();
    if let Some(credentials) = &ctx.config.whisker_credentials {
        let auth = Arc::new(WhiskerAuth::new(
            ctx.http.clone(),
            ctx.clock.clone(),
            credentials.email.clone(),
            credentials.password.clone(),
        ));
        tasks.push(Arc::new(PetTrackerTask::new(
            auth,
            WhiskerApi::new(ctx.http.clone()),
            pets.clone(),
            ledger.clone(),
            ctx.clock.clone(),
            tz.clone(),
        )?));
    }
    let notifier = PushoverNotifier::new(ctx.pushover.clone());
    // Reset alert tasks need both `PUSHOVER_USER` and `PUSHOVER_TOKEN`.
    if notifier.enabled() {
        let notifier: Arc<dyn ResetNotifier> = Arc::new(notifier);
        tasks.push(Arc::new(codex_reset_task(
            Arc::new(CodexSource {
                http: ctx.public_http.clone(),
                clock: ctx.clock.clone(),
                tz: tz.clone(),
            }),
            ResetDeliveryLedger::new(ctx.store.clone(), notifier.clone()),
            &tz,
        )?));
        tasks.push(Arc::new(claude_reset_task(
            Arc::new(ClaudeSource {
                http: ctx.public_http.clone(),
                clock: ctx.clock.clone(),
                tz: tz.clone(),
            }),
            ResetDeliveryLedger::new(ctx.store.clone(), notifier),
            &tz,
        )?));
    }

    let mut mcp_tools = mcp::personal::tools(
        pets.clone(),
        ledger.clone(),
        ctx.store.clone(),
        ctx.clock.clone(),
        tz.clone(),
    )?;
    mcp_tools.extend(mcp::printer::tools(printer_service(ctx)?)?);
    mcp_tools.extend(mcp::browser_history::tools(hister_service(ctx)?)?);
    mcp_tools.extend(mcp::owner_notify::tools(OwnerNotifier::new(
        ctx.store.clone(),
        ctx.clock.clone(),
        ctx.pushover.clone(),
        ctx.tracker.clone(),
    ))?);

    Ok(Subsystem {
        name: NAME,
        router: pets::routes::router(pets, ledger, ctx.clock.clone(), tz),
        tasks,
        mcp_tools,
        entities: entities(),
        alert_gates: vec![
            Arc::new(PetGapAlertGate::new(ctx.store.clone())),
            // Minute pollers of public trackers: one timed-out read is noise.
            Arc::new(TaskFailureGate::new(
                ctx.store.clone(),
                &[
                    codex_resets::task::TASK_NAME,
                    claude_resets::task::TASK_NAME,
                ],
                SUSTAINED_FAILURE,
            )),
        ],
        ..Subsystem::default()
    })
}
