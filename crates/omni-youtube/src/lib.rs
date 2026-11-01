//! The owner's YouTube account through the YouTube Data API v3: Google OAuth
//! (offline access, sealed refresh token), playlist management, public search
//! and video and channel details, with quota accounting. See docs/youtube.md.
//!
//! The API does not serve Watch Later, watch history, the personalized Home
//! feed or caption text, so nothing here claims them.

pub mod error;
pub mod ids;
pub mod mcp;
pub mod model;
pub mod records;
pub mod routes;
pub mod secrets;
pub mod service;
pub mod wire;

use omni_runtime::{AppContext, Subsystem};
use omni_store::entity::EntityDescriptor;

use crate::records::{
    YoutubeAccountData, YoutubeOauthStateData, YoutubeQuotaData, YoutubeWriteData,
};
use crate::secrets::Sealer;
use crate::service::{Deps, Settings, Youtube};

const LOG: &str = "YouTube";

/// The service over the application context; the token key is `youtube.key`
/// in the data directory.
pub fn build(ctx: &AppContext) -> Youtube {
    let sealer =
        Sealer::load_or_create(&ctx.paths.data_dir.join(secrets::KEY_FILE)).map_err(|error| {
            tracing::warn!(target: LOG, "{error}");
            error.reason
        });
    Youtube::new(Deps {
        settings: Settings::from_config(&ctx.config),
        store: ctx.store.clone(),
        http: ctx.http.clone(),
        sealer,
        mode: ctx.side_effects,
    })
}

pub fn entities() -> Vec<EntityDescriptor> {
    vec![
        EntityDescriptor::of::<YoutubeAccountData>(),
        EntityDescriptor::of::<YoutubeOauthStateData>(),
        EntityDescriptor::of::<YoutubeQuotaData>(),
        EntityDescriptor::of::<YoutubeWriteData>(),
    ]
}

/// The YouTube subsystem: the OAuth routes, the `youtube_*` tools and its entities.
pub fn subsystem(ctx: &AppContext) -> Result<Subsystem, omni_mcp_kit::ToolMetaError> {
    subsystem_with(build(ctx))
}

pub fn subsystem_with(youtube: Youtube) -> Result<Subsystem, omni_mcp_kit::ToolMetaError> {
    Ok(Subsystem {
        name: "youtube",
        router: routes::router(youtube.clone()),
        mcp_tools: mcp::tools(&youtube)?,
        entities: entities(),
        ..Subsystem::named("youtube")
    })
}
