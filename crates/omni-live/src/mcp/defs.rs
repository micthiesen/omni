//! The streamer configuration tools' contracts: metadata, policy and the
//! schema types their input and output schemas derive from. Pushover tokens
//! are UI-only: no input accepts one and outputs report only whether one is
//! set. After changing anything here run `cargo xtask mcp-golden` and review
//! the snapshot diff.

use omni_mcp_kit::{Annotations, ApprovalPolicy, Policy, ToolDef, ToolDefinition, ToolInfo};
use schemars::JsonSchema;

const READ: Annotations = Annotations {
    read_only_hint: true,
    destructive_hint: false,
    idempotent_hint: true,
    open_world_hint: false,
};

const WRITE: Annotations = Annotations {
    read_only_hint: false,
    destructive_hint: false,
    idempotent_hint: false,
    open_world_hint: false,
};

const CONFIG_POLICY: Policy = Policy {
    side_effects: &["Changes which livestreams Omni tracks and notifies about"],
    cost: "None",
    recommended: ApprovalPolicy::Allow,
};

pub static STREAMER_CONFIGS_LIST: ToolDef<EmptyInput, StreamerConfigsListOutput> = ToolDef::new(
    ToolInfo {
        name: "streamer_configs_list",
        title: "List Tracked Streamer Configuration",
        description: "List every tracked streamer's configuration in display order: id, display name, YouTube/Twitch/Kick usernames, tier (primary or background), live-notification override and whether a dedicated Pushover app is set; plus the Destiny.gg top-embeds setting and whether Kick credentials are configured. Use livestreams_list for live state.",
        annotations: READ,
        policy: Policy {
            side_effects: &[],
            cost: "none",
            recommended: ApprovalPolicy::Allow,
        },
    },
);

pub static STREAMER_CONFIG_CREATE: ToolDef<StreamerConfigCreateInput, StreamerConfigOutput> =
    ToolDef::new(ToolInfo {
        name: "streamer_config_create",
        title: "Track a Streamer",
        description: "Start tracking a streamer under a display name with one or more sources: YouTube handles (\"@name\") or channel ids, Twitch logins and Kick slugs. Several usernames per platform are allowed; each source can belong to only one streamer. Tier primary (default) notifies go-live/offline/title changes; background mutes them, records only all-time viewer highs and polls less often. liveNotifications false mutes a primary streamer. Takes effect on the next live check without a restart.",
        annotations: WRITE,
        policy: CONFIG_POLICY,
    });

pub static STREAMER_CONFIG_UPDATE: ToolDef<StreamerConfigUpdateInput, StreamerConfigOutput> =
    ToolDef::new(ToolInfo {
        name: "streamer_config_update",
        title: "Edit a Tracked Streamer",
        description: "Change a tracked streamer by streamerId. Only given fields change; a platform list replaces that platform's usernames (pass [] to remove the platform, keeping at least one source overall). liveNotifications null returns to the tier default. Switching to the background tier clears a liveNotifications override. The id stays the same when the display name changes, so history is kept.",
        annotations: Annotations {
            idempotent_hint: true,
            ..WRITE
        },
        policy: CONFIG_POLICY,
    });

pub static STREAMER_CONFIG_DELETE: ToolDef<StreamerConfigDeleteInput, StreamerConfigOutput> =
    ToolDef::new(ToolInfo {
        name: "streamer_config_delete",
        title: "Stop Tracking a Streamer",
        description: "Stop tracking a streamer by streamerId. A live session is closed without an offline notification. Its sessions and viewer records are kept, so re-adding the same display name resumes them, but a dedicated Pushover app set in the UI is lost.",
        annotations: Annotations {
            destructive_hint: true,
            idempotent_hint: true,
            ..WRITE
        },
        policy: Policy {
            side_effects: &["Stops tracking and notifying about a livestream"],
            cost: "None",
            recommended: ApprovalPolicy::RequireApproval,
        },
    });

pub static STREAMER_CONFIGS_REORDER: ToolDef<
    StreamerConfigsReorderInput,
    StreamerConfigsListOutput,
> = ToolDef::new(ToolInfo {
    name: "streamer_configs_reorder",
    title: "Reorder Tracked Streamers",
    description: "Set the display order of tracked streamers. streamerIds must list every configured id exactly once (see streamer_configs_list). The order applies within each tier: primary streamers always list before background ones, and changing a tier with streamer_config_update moves the streamer to the boundary (last primary or first background). Order breaks ties in the live list and iOS live slots.",
    annotations: Annotations {
        idempotent_hint: true,
        ..WRITE
    },
    policy: Policy {
        side_effects: &["Changes the display order of tracked livestreams"],
        cost: "None",
        recommended: ApprovalPolicy::Allow,
    },
});

pub static LIVESTREAM_SETTINGS_UPDATE: ToolDef<
    LivestreamSettingsUpdateInput,
    LivestreamSettingsOutput,
> = ToolDef::new(ToolInfo {
    name: "livestream_settings_update",
    title: "Update Livestream Settings",
    description: "Set how many of Destiny.gg's current hosted and top embeds Omni tracks as temporary streamers (0 turns discovery off, at most 20).",
    annotations: Annotations {
        idempotent_hint: true,
        ..WRITE
    },
    policy: CONFIG_POLICY,
});

/// Every streamer configuration tool, in serving order.
pub static TOOLS: [&dyn ToolDefinition; 6] = [
    &STREAMER_CONFIGS_LIST,
    &STREAMER_CONFIG_CREATE,
    &STREAMER_CONFIG_UPDATE,
    &STREAMER_CONFIG_DELETE,
    &STREAMER_CONFIGS_REORDER,
    &LIVESTREAM_SETTINGS_UPDATE,
];

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct EmptyInput {}

#[derive(JsonSchema)]
#[schemars(rename_all = "lowercase")]
pub enum Tier {
    Primary,
    Background,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct StreamerConfig {
    pub id: String,
    pub display_name: String,
    pub youtube: Vec<String>,
    pub twitch: Vec<String>,
    pub kick: Vec<String>,
    pub tier: Tier,
    /// `null` uses the tier default (on for primary, off for background)
    pub live_notifications: Option<bool>,
    /// Whether a dedicated Pushover app is set (managed in the UI only)
    pub has_pushover_token: bool,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct Settings {
    #[schemars(range(max = 20))]
    pub dgg_top_embeds: u32,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct StreamerConfigsListOutput {
    pub streamers: Vec<StreamerConfig>,
    pub settings: Settings,
    pub kick_configured: bool,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct StreamerConfigOutput {
    pub streamer: StreamerConfig,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct StreamerConfigCreateInput {
    #[schemars(length(min = 1, max = 100))]
    pub display_name: String,
    #[schemars(length(max = 10), inner(length(min = 1, max = 100)))]
    pub youtube: Option<Vec<String>>,
    #[schemars(length(max = 10), inner(length(min = 1, max = 100)))]
    pub twitch: Option<Vec<String>>,
    #[schemars(length(max = 10), inner(length(min = 1, max = 100)))]
    pub kick: Option<Vec<String>>,
    #[schemars(extend("default" = "primary"))]
    pub tier: Option<Tier>,
    /// Override for a primary streamer; omit for the tier default
    pub live_notifications: Option<bool>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct StreamerConfigUpdateInput {
    #[schemars(length(min = 1, max = 200))]
    pub streamer_id: String,
    #[schemars(length(min = 1, max = 100))]
    pub display_name: Option<String>,
    #[schemars(length(max = 10), inner(length(min = 1, max = 100)))]
    pub youtube: Option<Vec<String>>,
    #[schemars(length(max = 10), inner(length(min = 1, max = 100)))]
    pub twitch: Option<Vec<String>>,
    #[schemars(length(max = 10), inner(length(min = 1, max = 100)))]
    pub kick: Option<Vec<String>>,
    pub tier: Option<Tier>,
    /// true/false override; null returns to the tier default
    pub live_notifications: Option<Option<bool>>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct StreamerConfigDeleteInput {
    #[schemars(length(min = 1, max = 200))]
    pub streamer_id: String,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct StreamerConfigsReorderInput {
    #[schemars(length(min = 1, max = 200), inner(length(min = 1, max = 200)))]
    pub streamer_ids: Vec<String>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct LivestreamSettingsUpdateInput {
    #[schemars(range(max = 20))]
    pub dgg_top_embeds: u32,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
pub struct LivestreamSettingsOutput {
    pub settings: Settings,
}
