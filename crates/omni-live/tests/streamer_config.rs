//! Tracked streamers are configured in the database through the REST routes
//! (UI) and MCP tools; changes reach the live roster immediately, and Pushover
//! tokens are write-only and never accepted over MCP.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use axum::Router;
use axum::http::Method;
use omni_api::streamer_config::{StreamerConfigCreate, StreamerConfigPatch};
use omni_api::streamers::StreamerTier;
use omni_live::{LiveModule, Roster, StreamerConfigService};
use omni_mcp_kit::registry::standalone_context;
use omni_mcp_kit::{McpTool, ToolOutput, ToolPhase};
use omni_testkit::TestApp;
use serde_json::{Value, json};

struct Fixture {
    app: TestApp,
    router: Router,
    roster: Roster,
    service: StreamerConfigService,
    tools: Vec<McpTool>,
}

/// A booted module over an empty store.
async fn fixture() -> Fixture {
    let app = TestApp::new().await;
    let module = LiveModule::load(&app.ctx).unwrap();
    let (roster, service) = (module.roster(), module.config_service());
    let mut subsystem = module.into_subsystem(None).unwrap();
    let step = subsystem.boot_steps.remove(0);
    (step.run)(app.ctx.clone()).await.unwrap();
    let router = app.router(&subsystem);
    let tools = std::mem::take(&mut subsystem.mcp_tools);
    Fixture {
        app,
        router,
        roster,
        service,
        tools,
    }
}

impl Fixture {
    async fn send(&self, method: Method, path: &str, body: Option<Value>) -> (u16, Value) {
        let (status, body) = self
            .app
            .request_json(&self.router, method, path, body.as_ref())
            .await;
        (status.as_u16(), body)
    }

    async fn call(&self, name: &str, input: Value) -> Result<Value, (ToolPhase, String)> {
        let tool = self.tools.iter().find(|t| t.meta.name == name).unwrap();
        match tool.handler.call(input, standalone_context("test")).await {
            Ok(ToolOutput::Structured(map)) => Ok(Value::Object(map)),
            Ok(ToolOutput::Custom { structured, .. }) => Ok(Value::Object(structured)),
            Err(e) => Err((e.phase, e.message)),
        }
    }

    fn roster_names(&self) -> Vec<String> {
        self.roster
            .snapshot()
            .into_iter()
            .map(|s| s.display_name)
            .collect()
    }
}

#[tokio::test]
async fn only_a_trusted_browser_changes_the_configuration() {
    let f = fixture().await;
    let body = json!({"displayName": "Destiny", "twitch": ["destiny"], "pushoverToken": "abc123"});
    let (status, refused) = f
        .app
        .untrusted_json(
            &f.router,
            Method::POST,
            "/api/streamer-config/streamers",
            Some(&body),
        )
        .await;
    assert_eq!(status.as_u16(), 403, "{refused}");
    assert_eq!(refused["code"], "untrusted_browser");
    assert!(f.roster_names().is_empty());
    let (status, _) = f
        .app
        .untrusted_json(&f.router, Method::GET, "/api/streamer-config", None)
        .await;
    assert_eq!(status.as_u16(), 200, "reading needs no trust");
}

#[tokio::test]
async fn the_ui_creates_edits_reorders_and_deletes_streamers() {
    let f = fixture().await;
    let (status, created) = f
        .send(
            Method::POST,
            "/api/streamer-config/streamers",
            Some(json!({
                "displayName": " Destiny ",
                "youtube": ["@destiny", "@DESTINY", "@destinyclips"],
                "twitch": ["destiny"],
                "pushoverToken": "abc123"
            })),
        )
        .await;
    assert_eq!(status, 200, "{created}");
    assert_eq!(
        created,
        json!({
            "id": "destiny", "displayName": "Destiny",
            "youtube": ["@destiny", "@destinyclips"], "twitch": ["destiny"], "kick": [],
            "tier": "primary", "liveNotifications": null, "hasPushoverToken": true
        })
    );
    let (status, _) = f
        .send(
            Method::POST,
            "/api/streamer-config/streamers",
            Some(json!({"displayName": "Jerma", "twitch": ["jerma985"], "tier": "background"})),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(f.roster_names(), ["Destiny", "Jerma"]);
    let destiny = f.roster.snapshot().into_iter().next().unwrap();
    assert_eq!(destiny.bindings.len(), 3);
    assert_eq!(destiny.pushover_token.as_deref(), Some("abc123"));

    // A binding already owned by another streamer is rejected.
    let (status, body) = f
        .send(
            Method::POST,
            "/api/streamer-config/streamers",
            Some(json!({"displayName": "Copy", "twitch": ["JERMA985"]})),
        )
        .await;
    assert_eq!(status, 400, "{body}");

    // Background with an explicit live-notification override is contradictory.
    let (status, _) = f
        .send(
            Method::PATCH,
            "/api/streamer-config/streamers/jerma",
            Some(json!({"liveNotifications": true})),
        )
        .await;
    assert_eq!(status, 400);

    // Promoting to primary, clearing the token and renaming keep the id.
    let (status, edited) = f
        .send(
            Method::PATCH,
            "/api/streamer-config/streamers/destiny",
            Some(json!({"displayName": "Steven", "pushoverToken": "", "liveNotifications": false})),
        )
        .await;
    assert_eq!(status, 200, "{edited}");
    assert_eq!(edited["id"], "destiny");
    assert_eq!(edited["hasPushoverToken"], false);
    assert_eq!(edited["liveNotifications"], false);
    assert_eq!(f.roster.snapshot()[0].pushover_token, None);

    let (status, listed) = f
        .send(
            Method::PUT,
            "/api/streamer-config/order",
            Some(json!({"ids": ["jerma", "destiny"]})),
        )
        .await;
    assert_eq!(status, 200);
    // Jerma is background, so primary Steven still lists first.
    assert_eq!(listed["streamers"][0]["id"], "destiny");
    assert_eq!(f.roster_names(), ["Steven", "Jerma"]);
    let (status, _) = f
        .send(
            Method::PUT,
            "/api/streamer-config/order",
            Some(json!({"ids": ["jerma"]})),
        )
        .await;
    assert_eq!(status, 400);

    let (status, settings) = f
        .send(
            Method::PUT,
            "/api/streamer-config/settings",
            Some(json!({"dggTopEmbeds": 3})),
        )
        .await;
    assert_eq!((status, settings), (200, json!({"dggTopEmbeds": 3})));

    let (status, _) = f
        .send(Method::DELETE, "/api/streamer-config/streamers/jerma", None)
        .await;
    assert_eq!(status, 200);
    let (status, _) = f
        .send(Method::DELETE, "/api/streamer-config/streamers/jerma", None)
        .await;
    assert_eq!(status, 404);
    assert_eq!(f.roster_names(), ["Steven"]);

    let (status, listed) = f.app.get_json(&f.router, "/api/streamer-config").await;
    assert_eq!(status, 200);
    assert_eq!(listed["settings"], json!({"dggTopEmbeds": 3}));
    assert_eq!(listed["streamers"].as_array().unwrap().len(), 1);
    assert!(!listed.to_string().contains("abc123"));
}

#[tokio::test]
async fn mcp_tools_manage_streamers_but_never_accept_pushover_tokens() {
    let f = fixture().await;
    f.service
        .create(omni_api::streamer_config::StreamerConfigCreate {
            display_name: "Vinesauce".into(),
            twitch: vec!["vinesauce".into()],
            pushover_token: Some("secret1".into()),
            ..Default::default()
        })
        .await
        .unwrap();

    let error = f
        .call(
            "streamer_config_create",
            json!({"displayName": "Hutch", "youtube": ["@hutch"], "pushoverToken": "x1"}),
        )
        .await
        .unwrap_err();
    assert!(matches!(error.0, ToolPhase::Input), "{error:?}");
    let error = f
        .call(
            "streamer_config_update",
            json!({"streamerId": "vinesauce", "pushoverToken": ""}),
        )
        .await
        .unwrap_err();
    assert!(matches!(error.0, ToolPhase::Input), "{error:?}");

    let created = f
        .call(
            "streamer_config_create",
            json!({"displayName": "Hutch", "youtube": ["@hutch"], "kick": ["hutch"]}),
        )
        .await
        .unwrap();
    assert_eq!(created["streamer"]["id"], "hutch");

    // An MCP edit keeps the token set in the UI.
    let updated = f
        .call(
            "streamer_config_update",
            json!({"streamerId": "vinesauce", "tier": "background", "twitch": ["vinesauce", "vinny"]}),
        )
        .await
        .unwrap();
    assert_eq!(updated["streamer"]["tier"], "background");
    assert_eq!(updated["streamer"]["hasPushoverToken"], true);
    assert_eq!(updated["streamer"]["twitch"], json!(["vinesauce", "vinny"]));

    let error = f
        .call(
            "streamer_config_update",
            json!({"streamerId": "nobody", "tier": "primary"}),
        )
        .await
        .unwrap_err();
    assert!(error.1.contains("nobody"), "{error:?}");

    let listed = f
        .call(
            "streamer_configs_reorder",
            json!({"streamerIds": ["hutch", "vinesauce"]}),
        )
        .await
        .unwrap();
    assert_eq!(listed["streamers"][0]["id"], "hutch");
    // TestApp has no Kick credentials: the Kick source is stored but not polled.
    assert_eq!(listed["kickConfigured"], false);
    assert_eq!(f.roster.snapshot()[0].bindings.len(), 1);

    let settings = f
        .call("livestream_settings_update", json!({"dggTopEmbeds": 1}))
        .await
        .unwrap();
    assert_eq!(settings, json!({"settings": {"dggTopEmbeds": 1}}));

    let deleted = f
        .call("streamer_config_delete", json!({"streamerId": "hutch"}))
        .await
        .unwrap();
    assert_eq!(deleted["streamer"]["displayName"], "Hutch");
    let listed = f.call("streamer_configs_list", json!({})).await.unwrap();
    assert_eq!(listed["streamers"].as_array().unwrap().len(), 1);
    assert!(!listed.to_string().contains("secret1"));
    assert_eq!(f.roster_names(), ["Vinesauce"]);
}

#[tokio::test]
async fn primary_streamers_list_first_and_a_tier_change_crosses_the_boundary() {
    let f = fixture().await;
    let ids = || async {
        f.service
            .list()
            .await
            .unwrap()
            .streamers
            .into_iter()
            .map(|s| s.id)
            .collect::<Vec<_>>()
    };
    for (name, tier) in [
        ("a", StreamerTier::Primary),
        ("x", StreamerTier::Background),
        ("b", StreamerTier::Primary),
        ("y", StreamerTier::Background),
        ("c", StreamerTier::Primary),
    ] {
        f.service
            .create(StreamerConfigCreate {
                display_name: name.into(),
                twitch: vec![name.into()],
                tier,
                ..Default::default()
            })
            .await
            .unwrap();
    }
    assert_eq!(ids().await, ["a", "b", "c", "x", "y"]);

    // Demoting lands first among background; promoting lands last among primary.
    let demote = StreamerConfigPatch {
        tier: Some(StreamerTier::Background),
        ..Default::default()
    };
    f.service.update("b", demote).await.unwrap();
    assert_eq!(ids().await, ["a", "c", "b", "x", "y"]);
    let promote = StreamerConfigPatch {
        tier: Some(StreamerTier::Primary),
        ..Default::default()
    };
    f.service.update("y", promote).await.unwrap();
    assert_eq!(ids().await, ["a", "c", "y", "b", "x"]);

    // An interleaved order applies within each tier.
    let order = ["x", "y", "b", "c", "a"].map(String::from).to_vec();
    f.service.reorder(order).await.unwrap();
    assert_eq!(ids().await, ["y", "c", "a", "x", "b"]);
    assert_eq!(f.roster_names(), ["y", "c", "a", "x", "b"]);
}

#[tokio::test]
async fn configuration_survives_a_restart() {
    let f = fixture().await;
    f.service
        .create(omni_api::streamer_config::StreamerConfigCreate {
            display_name: "Jerma".into(),
            twitch: vec!["jerma985".into()],
            ..Default::default()
        })
        .await
        .unwrap();
    let module = LiveModule::load(&f.app.ctx).unwrap();
    let roster = module.roster();
    let mut subsystem = module.into_subsystem(None).unwrap();
    let step = subsystem.boot_steps.remove(0);
    (step.run)(f.app.ctx.clone()).await.unwrap();
    let names: Vec<String> = roster
        .snapshot()
        .into_iter()
        .map(|s| s.display_name)
        .collect();
    assert_eq!(names, ["Jerma"]);
}

#[tokio::test]
async fn usernames_are_checked_per_platform() {
    let f = fixture().await;
    for (body, ok) in [
        (json!({"displayName": "A", "twitch": ["bad/name"]}), false),
        (json!({"displayName": "A", "kick": ["a?b"]}), false),
        (json!({"displayName": "A", "youtube": ["../x"]}), false),
        (json!({"displayName": "A", "youtube": ["@Whick-TV"]}), true),
        // Channel ids are case-sensitive, so these are two different sources.
        (
            json!({"displayName": "B", "youtube": ["channel/UCabc"]}),
            true,
        ),
        (
            json!({"displayName": "C", "youtube": ["channel/UCABC"]}),
            true,
        ),
        (json!({"displayName": "D", "youtube": ["@whick-tv"]}), false),
    ] {
        let (status, response) = f
            .send(
                Method::POST,
                "/api/streamer-config/streamers",
                Some(body.clone()),
            )
            .await;
        assert_eq!(status == 200, ok, "{body} -> {response}");
    }
}
