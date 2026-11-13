//! Parcel tracker email pipeline: candidate filter, tracking-number
//! extraction, carrier validation against the live Parcel list, durable
//! dedup with reservation before submission, and the delivery-forget route.
//! Also the `parcels_add` MCP tool (`add`), which submits through the same
//! client and dedup rows. Omni never reads deliveries back from Parcel.

pub mod add;
pub mod add_ledger;
pub mod carriers;
pub mod error;
pub mod extraction;
pub mod filter;
pub mod log_file;
pub mod mcp;
pub mod parcel_api;
pub mod persistence;
pub mod pipeline;

use std::path::PathBuf;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::routing::delete;
use omni_api::email::DeletedResponse;
use omni_email::triage::EmailTriage;
use omni_http::public::PublicHttpClient;
use omni_runtime::{AppContext, ManagedEntity, Subsystem};
use omni_server_kit::ApiError;
use omni_store::Store;
use omni_store::entity::EntityDescriptor;

use crate::add::ParcelAdder;
use crate::add_ledger::AddLedger;
use crate::carriers::carrier_map::CarrierDirectory;
use crate::extraction::ModelExtractor;
use crate::parcel_api::ParcelApi;
use crate::persistence::SubmittedDelivery;
use crate::pipeline::{DeliveryPipeline, PipelineDeps};

const LOG: &str = "Main:ParcelTracker";
const SERVER_LOG: &str = "Main:Server";

#[derive(Debug, thiserror::Error)]
pub enum ParcelSetupError {
    #[error(transparent)]
    Http(#[from] omni_http::HttpError),
    #[error(transparent)]
    ToolMeta(#[from] omni_mcp_kit::ToolMetaError),
}

pub fn entities() -> Vec<EntityDescriptor> {
    vec![
        EntityDescriptor::of::<SubmittedDelivery>(),
        EntityDescriptor::of::<AddLedger>(),
    ]
}

pub fn managed_entities() -> Vec<ManagedEntity> {
    let entity = EntityDescriptor::of::<SubmittedDelivery>();
    vec![ManagedEntity {
        slug: entity.name,
        label: "Submitted deliveries",
        description: "Parcel tracking numbers already sent to Parcel.app.",
        warning: Some("This is a deduplication gate. Deleted rows may be submitted again."),
        entity,
        primary_key: &["trackingNumber"],
        can_delete: None,
        after_delete: None,
    }]
}

/// `DELETE /api/parcel-tracker/deliveries/:trackingNumber`: forget a submitted
/// tracking number so a future email can resubmit it.
pub fn router(store: Store) -> axum::Router {
    axum::Router::new()
        .route(
            "/api/parcel-tracker/deliveries/{tracking_number}",
            delete(forget_delivery),
        )
        .with_state(store)
}

async fn forget_delivery(
    State(store): State<Store>,
    Path(tracking_number): Path<String>,
) -> Result<Json<DeletedResponse>, ApiError> {
    let deleted = persistence::forget(&store, &tracking_number)
        .await
        .map_err(ApiError::internal)?;
    if !deleted {
        return Err(ApiError::not_found("Unknown tracking number"));
    }
    tracing::info!(target: SERVER_LOG, "Forgot submitted delivery {tracking_number}");
    Ok(Json(DeletedResponse { deleted: true }))
}

fn api_key(ctx: &AppContext) -> Option<String> {
    ctx.config
        .parcel_api_key
        .clone()
        .filter(|key| !key.is_empty())
}

fn parcel_api(ctx: &AppContext, api_key: String) -> Result<Arc<ParcelApi>, ParcelSetupError> {
    Ok(Arc::new(
        ParcelApi::new(
            ctx.http.clone(),
            api_key,
            ctx.side_effects,
            ctx.clock.clone(),
        )?
        .with_ledger(ctx.store.clone()),
    ))
}

/// `None` (logged) without `PARCEL_API_KEY`.
pub fn handler(
    ctx: &AppContext,
    triage: EmailTriage,
) -> Result<Option<Arc<DeliveryPipeline>>, ParcelSetupError> {
    let Some(api_key) = api_key(ctx) else {
        tracing::info!(target: LOG, "Disabled: missing PARCEL_API_KEY");
        return Ok(None);
    };
    let carriers = Arc::new(CarrierDirectory::new(
        PublicHttpClient::new(&ctx.http),
        ctx.clock.clone(),
    )?);
    Ok(Some(pipeline(
        ctx,
        triage,
        parcel_api(ctx, api_key)?,
        carriers,
    )))
}

fn pipeline(
    ctx: &AppContext,
    triage: EmailTriage,
    submitter: Arc<ParcelApi>,
    carriers: Arc<CarrierDirectory>,
) -> Arc<DeliveryPipeline> {
    let deps = PipelineDeps {
        store: ctx.store.clone(),
        run_logs: ctx.run_logs(),
        triage,
        extractor: Arc::new(ModelExtractor::new(
            ctx.ai.clone(),
            ctx.config.clone(),
            carriers.clone(),
        )),
        submitter,
        carriers,
        self_address: ctx.config.email_self_address().map(str::to_owned),
        logs_path: ctx
            .config
            .logs_path
            .clone()
            .filter(|path| !path.is_empty())
            .map(PathBuf::from),
        tz: jiff::tz::TimeZone::get(&ctx.config.tz).unwrap_or(jiff::tz::TimeZone::UTC),
        tracker: ctx.tracker.clone(),
        systemic: omni_email::systemic::SystemicReporter::new(
            ctx.store.clone(),
            ctx.pushover.clone(),
        ),
    };
    tracing::info!(target: LOG, "Pipeline created");
    Arc::new(DeliveryPipeline::new(deps))
}

/// The parcel subsystem: the forget route, `parcels_add`, its entities and,
/// when configured, the `ParcelTracker` email handler (share `triage` with
/// the calendar pipeline). The handler and the tool share one Parcel client
/// and carrier list.
pub fn subsystem(ctx: &AppContext, triage: EmailTriage) -> Result<Subsystem, ParcelSetupError> {
    let mut email_handlers: Vec<Arc<dyn omni_core::email::EmailHandler>> = Vec::new();
    let adder = match api_key(ctx) {
        Some(api_key) => {
            let carriers = Arc::new(CarrierDirectory::new(
                PublicHttpClient::new(&ctx.http),
                ctx.clock.clone(),
            )?);
            let api = parcel_api(ctx, api_key)?;
            email_handlers.push(pipeline(ctx, triage, api.clone(), carriers.clone()));
            ParcelAdder::new(
                ctx.store.clone(),
                Some(api),
                Some(carriers),
                ctx.tracker.clone(),
            )
        }
        None => {
            tracing::info!(target: LOG, "Disabled: missing PARCEL_API_KEY");
            ParcelAdder::new(ctx.store.clone(), None, None, ctx.tracker.clone())
        }
    };
    Ok(Subsystem {
        router: router(ctx.store.clone()),
        mcp_tools: mcp::tools(Arc::new(adder))?,
        entities: entities(),
        managed_entities: managed_entities(),
        email_handlers,
        ..Subsystem::named("parcel")
    })
}
