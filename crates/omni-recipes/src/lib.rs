//! Fond, the agent-first recipe library (see docs/recipes.md).
//!
//! A recipe is a validated document (`omni-recipe-format`) with immutable
//! revisions; tags, state, notes, images and cook history are record metadata
//! covered by a change log with 30 days of undo. Agents manage the library
//! through the `recipes_*` tools; the app cooks from it over an HTTP API that
//! is served only on its own host.

pub mod api;
pub mod archive;
pub mod cook;
pub mod db;
pub mod error;
pub mod events;
pub mod fidelity;
pub mod files;
pub mod guide;
pub mod household;
pub mod http;
pub mod hub;
pub mod ids;
pub mod images;
pub mod inbox;
pub mod jsonld;
pub mod library;
pub mod mcp;
pub mod meta;
pub mod organize;
pub mod requests;
pub mod review;
pub mod schema;
pub mod search;
pub mod service;
pub mod sync;
pub mod tags;
pub mod tasks;

use std::sync::Arc;

use omni_runtime::{AppContext, BackgroundService, BootError, BootPhase, BootStep, Subsystem};

pub use error::RecipesError;
pub use service::Recipes;

const LOG: &str = "Recipes";

#[derive(Debug, thiserror::Error)]
pub enum SubsystemError {
    #[error(transparent)]
    Tool(#[from] omni_mcp_kit::ToolMetaError),
    #[error(transparent)]
    Schedule(#[from] omni_tasks::InvalidScheduleError),
    #[error("RECIPES_APP_URL: {0}")]
    AppOrigin(#[from] omni_server_kit::host_app::InvalidAppOrigin),
}

/// A service over the app's store, clock, ports and recipes directory. Each
/// call makes a separate handle with its own change feed: writes through it
/// are not announced on the app's event stream, which belongs to the handle
/// [`subsystem`] builds.
pub fn service(ctx: &AppContext) -> Recipes {
    Recipes::from_context(ctx)
}

pub fn subsystem(ctx: &AppContext) -> Result<Subsystem, SubsystemError> {
    let recipes = service(ctx);
    let tz = recipes.tz.clone();
    let init = recipes.clone();
    let timers = recipes.clone();
    let requests = recipes.clone();
    let tasks: Vec<Arc<dyn omni_tasks::Task>> = vec![
        Arc::new(tasks::BackupTask::new(recipes.clone(), &tz)?),
        Arc::new(tasks::PurgeTask::new(recipes.clone(), &tz)?),
    ];
    // The app exists only on its own host: its router is a host app and is
    // never merged into Omni's.
    let host_apps = match &ctx.config.recipes_app_url {
        Some(origin) => vec![http::host_app(
            recipes.clone(),
            origin,
            ctx.paths.recipes_web_dist.clone(),
        )?],
        None => Vec::new(),
    };
    Ok(Subsystem {
        tasks,
        mcp_tools: mcp::tools(Arc::new(recipes))?,
        host_apps,
        // A failure here must not stop the rest of Omni: the tools retry the
        // same migration on every call and report the error.
        boot_steps: vec![BootStep {
            phase: BootPhase::Migrate,
            name: "recipes-tables",
            run: Box::new(move |_ctx| {
                Box::pin(async move {
                    if let Err(error) = init.ready().await {
                        tracing::warn!(target: LOG, "Recipe library unavailable: {}", error.tool_text());
                    }
                    Ok::<(), BootError>(())
                })
            }),
        }],
        services: vec![
            BackgroundService {
                name: "recipes-timers",
                start: Box::new(move |ctx| {
                    let timers = timers.clone();
                    Box::pin(async move { timers.run_timers(ctx.shutdown.clone()).await })
                }),
                retry: None,
            },
            BackgroundService {
                name: "recipes-requests",
                start: Box::new(move |ctx| {
                    let requests = requests.clone();
                    Box::pin(async move { requests.run_requests(ctx.shutdown.clone()).await })
                }),
                retry: None,
            },
        ],
        ..Subsystem::named("recipes")
    })
}
