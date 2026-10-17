//! `omni --preview [--port N]`:
//! the real HTTP server, subsystems and registry over a throwaway database
//! seeded with fake streamers, statuses, viewer history, runs,
//! recommendations, email activity and a pet, plus fake tasks that
//! take a few seconds to "run" so the realtime flow is observable. No
//! credentials are configured, outgoing HTTP is refused and every side effect
//! is recorded, so nothing leaves the machine.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use omni_api::streamer_config::StreamerConfigCreate;
use omni_api::streamers::StreamerTier;
use omni_core::clock::{SharedClock, SystemClock};
use omni_http::{HttpClient, HttpConfig, SideEffectMode};
use omni_runtime::{AppContext, BootPhase};
use omni_store::{DocMeta, DocWrite as _};
use omni_tasks::{CronSchedule, RunContext, Scheduler, Task, TaskError, TaskOptions};
use serde_json::{Value, json};

use crate::boot::{self, BootTrace};
use crate::context::{Foundation, app_paths, context_over};
use crate::json::json_to_js_value;
use crate::logging::{self, LoggingSetup};

const LOG: &str = "Preview";
const DEFAULT_PORT: u16 = 3999;
const MIN: i64 = 60_000;
const HOUR: i64 = 3_600_000;
const DAY: i64 = 24 * HOUR;

/// A task that logs a spread of levels over `duration`, optionally failing.
pub struct FakeTask {
    name: String,
    schedule: CronSchedule,
    duration: Duration,
    summary: Option<String>,
    fail: bool,
    manual_input: bool,
}

impl FakeTask {
    pub fn new(
        name: &str,
        schedule: &str,
        duration_ms: u64,
        summary: Option<&str>,
        fail: bool,
    ) -> Result<Self, omni_tasks::InvalidScheduleError> {
        Ok(Self {
            name: name.to_owned(),
            schedule: CronSchedule::parse(schedule, &jiff::tz::TimeZone::system())?,
            duration: Duration::from_millis(duration_ms),
            summary: summary.map(str::to_owned),
            fail,
            manual_input: false,
        })
    }

    /// Mirrors the real Recommendations task's manual input.
    pub fn with_manual_input(mut self) -> Self {
        self.manual_input = true;
        self
    }

    async fn steps(&self) -> Result<(), TaskError> {
        let target = format!("{LOG}:{}", self.name);
        let ms = u64::try_from(self.duration.as_millis()).unwrap_or(u64::MAX);
        let steps = (ms / 400).max(3);
        tracing::info!(target: "Preview", task = %target, "Starting {} ({steps} steps)", self.name);
        for i in 1..=steps {
            tokio::time::sleep(self.duration / u32::try_from(steps).unwrap_or(1)).await;
            if i % 4 == 0 {
                tracing::info!(target: "Preview", "Step {i}/{steps}: fetched upstream page {i}");
            } else if i % 7 == 0 {
                tracing::warn!(target: "Preview", "Step {i}/{steps}: upstream slow, retrying");
            } else {
                tracing::debug!(target: "Preview", items = i * 3, "Step {i}/{steps}: processed batch");
            }
        }
        if self.fail {
            // WARN, not ERROR: the alert layer would send a (recorded) Pushover.
            tracing::warn!(target: "Preview", "Upstream returned 503 after retries, giving up");
            return Err(TaskError::new("Simulated failure: upstream returned 503"));
        }
        tracing::info!(target: "Preview", "Finished {}", self.name);
        Ok(())
    }
}

impl Task for FakeTask {
    fn name(&self) -> &str {
        &self.name
    }

    fn schedule(&self) -> &CronSchedule {
        &self.schedule
    }

    fn options(&self) -> TaskOptions {
        TaskOptions::default()
    }

    fn run<'a>(&'a self, _cx: &'a RunContext) -> BoxFuture<'a, Result<(), TaskError>> {
        Box::pin(self.steps())
    }

    fn accepts_manual_input(&self) -> bool {
        self.manual_input
    }

    fn run_manual<'a>(
        &'a self,
        _cx: &'a RunContext,
        input: Value,
    ) -> BoxFuture<'a, Result<(), TaskError>> {
        Box::pin(async move {
            tracing::info!(target: "Preview", input = %input, "Manual run input");
            self.steps().await
        })
    }

    fn last_run_summary(&self) -> Option<String> {
        self.summary.clone()
    }
}

/// The fake tasks the preview registers.
pub fn fake_tasks() -> Result<Vec<Arc<dyn Task>>, omni_tasks::InvalidScheduleError> {
    Ok(vec![
        Arc::new(FakeTask::new(
            "LiveCheckTask",
            "*/20 * * * * *",
            1_500,
            None,
            false,
        )?),
        Arc::new(FakeTask::new(
            "PressPods",
            "0 */15 * * * *",
            6_000,
            Some("Narrated 2 articles: chip exports and GPU supply."),
            false,
        )?),
        Arc::new(FakeTask::new(
            "CastroInboxCleanup",
            "0 0 */6 * * *",
            5_000,
            None,
            true,
        )?),
        Arc::new(FakeTask::new(
            "PetTrackerTask",
            "0 */30 * * * *",
            2_500,
            None,
            false,
        )?),
        Arc::new(
            FakeTask::new(
                "Recommendations",
                "0 0 17 * * 1,3,5",
                9_000,
                Some("Picked Harbor Lights S2 (2026); added to watchlist."),
                false,
            )?
            .with_manual_input(),
        ),
        Arc::new(FakeTask::new(
            "PodcastRecs",
            "0 0 8 * * *",
            2_000,
            Some("Queued one episode in Castro."),
            false,
        )?),
    ])
}

/// The preview's tracked streamers.
pub fn preview_streamers() -> Vec<StreamerConfigCreate> {
    let streamer = |name: &str, youtube: &[&str], twitch: &[&str], kick: &[&str]| {
        let names = |list: &[&str]| list.iter().map(|&n| n.to_owned()).collect();
        StreamerConfigCreate {
            display_name: name.to_owned(),
            youtube: names(youtube),
            twitch: names(twitch),
            kick: names(kick),
            ..StreamerConfigCreate::default()
        }
    };
    vec![
        streamer("PixelDust", &[], &["pixeldust"], &[]),
        streamer("NovaByte", &["@novabyte"], &["novabyte"], &[]),
        streamer("RetroRex", &[], &[], &["retrorex"]),
        StreamerConfigCreate {
            tier: StreamerTier::Background,
            ..streamer("LoopStation", &[], &["loopstation"], &[])
        },
    ]
}

fn iso_date(ms: i64) -> String {
    omni_core::js::to_iso_string(ms)[..10].to_owned()
}

fn viewer_metrics(now: i64, streamer_id: &str, base: f64, days: i64) -> Value {
    let mut buckets = Vec::new();
    for day in (0..=days).rev() {
        if (day * 7919) % 7 < 2 {
            continue;
        }
        #[allow(clippy::cast_precision_loss)]
        let (wave, jitter) = (
            0.75 + 0.25 * (day as f64 / 5.0).sin(),
            ((day * 104_729) % 23) as f64 / 23.0 * 0.3,
        );
        let t = now - day * DAY;
        buckets.push(json!({
            "date": iso_date(t),
            "maxViewers": (base * (wave + jitter)).round(),
            "timestamp": t,
        }));
    }
    let max = buckets
        .iter()
        .filter_map(|b| b["maxViewers"].as_f64())
        .fold(0.0, f64::max);
    json!({
        "streamerId": streamer_id,
        "dailyBuckets": buckets,
        "allTimeMax": (max * 1.15).round(),
        "allTimeMaxTimestamp": now - (days + 30) * DAY,
    })
}

/// Seed documents as `(entity, JSON payload)` in their stored shapes.
pub fn seed_documents(now: i64) -> Vec<(&'static str, Value)> {
    let mut docs = vec![
        (
            "streamer-status",
            json!({
                "streamerId": "pixeldust", "isLive": true,
                "primary": { "platform": "twitch", "username": "pixeldust" },
                "primaryTitle": "Ranked grind to Diamond — day 12, chat picks the loadout",
                "startedAt": now - 24 * HOUR / 10, "maxViewerCount": 4230, "viewerCount": 3980,
                "category": "VALORANT",
            }),
        ),
        (
            "streamer-status",
            json!({
                "streamerId": "novabyte", "isLive": true,
                "primary": { "platform": "youtube", "username": "@novabyte" },
                "primaryTitle": "Building a mechanical keyboard from scratch (live soldering)",
                "startedAt": now - 47 * MIN, "maxViewerCount": 812, "viewerCount": 690,
            }),
        ),
        (
            "streamer-status",
            json!({
                "streamerId": "retrorex", "isLive": false,
                "lastStartedAt": now - 29 * HOUR, "lastEndedAt": now - 26 * HOUR,
                "lastMaxViewerCount": 1890,
            }),
        ),
        (
            "streamer-status",
            json!({
                "streamerId": "loopstation", "isLive": true,
                "primary": { "platform": "twitch", "username": "loopstation" },
                "primaryTitle": "24/7 lo-fi beats to code to",
                "startedAt": now - 6 * HOUR, "maxViewerCount": 340, "viewerCount": 310,
                "category": "Music",
            }),
        ),
        (
            "streamer-viewer-metrics",
            viewer_metrics(now, "pixeldust", 4200.0, 95),
        ),
        (
            "streamer-viewer-metrics",
            viewer_metrics(now, "novabyte", 850.0, 60),
        ),
        (
            "streamer-viewer-metrics",
            viewer_metrics(now, "retrorex", 1900.0, 25),
        ),
        (
            "streamer-viewer-metrics",
            viewer_metrics(now, "loopstation", 330.0, 40),
        ),
    ];
    let mut seq = 0;
    let mut run = |task: &str, started: i64, duration: i64, status: &str, extra: Value| {
        let mut row = json!({
            "runId": format!("{task}:{started}:{seq}"),
            "taskName": task,
            "trigger": "schedule",
            "startedAt": started,
            "finishedAt": started + duration,
            "status": status,
        });
        seq += 1;
        if let (Some(row), Some(extra)) = (row.as_object_mut(), extra.as_object()) {
            row.extend(extra.clone());
        }
        ("task-run", row)
    };
    for i in 1..=12 {
        docs.push(run(
            "LiveCheckTask",
            now - i * 20_000,
            700 + i * 13,
            "success",
            json!({}),
        ));
    }
    docs.push(run(
        "LiveCheckTask",
        now - 42 * MIN,
        1400,
        "error",
        json!({ "error": "Twitch GQL returned 502 for pixeldust" }),
    ));
    docs.push(run(
        "PressPods",
        now - 2 * HOUR,
        34_000,
        "success",
        json!({ "summary": "Narrated 2 articles: GPU supply and tape-out delays." }),
    ));
    docs.push(run(
        "CastroInboxCleanup",
        now - 14 * HOUR,
        41_000,
        "error",
        json!({ "error": "Castro returned HTTP 500 after 3 retries" }),
    ));
    docs.push(run(
        "PetTrackerTask",
        now - 34 * MIN,
        2_300,
        "success",
        json!({ "summary": "Mochi: 11.3 lbs, 3 visits today." }),
    ));
    docs.push(run(
        "Recommendations",
        now - 22 * HOUR,
        96_000,
        "success",
        json!({ "trigger": "manual", "summary": "Picked The Iron Harvest (2025); added to watchlist." }),
    ));
    docs.push(run(
        "Recommendations",
        now - 70 * HOUR,
        4_000,
        "degraded",
        json!({
            "error": "media state unavailable: Plex request failed: connection refused",
            "summary": "skipped: Plex request failed: connection refused",
        }),
    ));
    let rec = |id: &str,
               canonical: &str,
               tmdb: i64,
               media: &str,
               title: &str,
               year: i64,
               status: &str,
               extra: Value| {
        let mut row = json!({
            "recommendationId": id, "canonicalId": canonical, "tmdbId": tmdb,
            "mediaType": media, "title": title, "year": year, "status": status,
        });
        if let (Some(row), Some(extra)) = (row.as_object_mut(), extra.as_object()) {
            row.extend(extra.clone());
        }
        ("recs-recommendation-attempt", row)
    };
    docs.push(rec(
        "preview-iron-harvest", "tmdb:movie:100001", 100_001, "movie", "The Iron Harvest", 2025, "notified",
        json!({
            "whyForUser": "Slow-burn sci-fi with a strong ensemble cast — matches your recent run of cerebral thrillers and clocks in under two hours.",
            "caveats": ["Only on physical rental in some regions"], "confidence": 0.82,
            "runDate": "2026-07-14", "recommendedAt": now - 22 * HOUR,
            "notifiedAt": now - 22 * HOUR + MIN, "watchlistResult": "added",
        }),
    ));
    docs.push(rec(
        "preview-glass-orchard",
        "tmdb:tv:400004",
        400_004,
        "tv",
        "The Glass Orchard",
        2026,
        "notified",
        json!({
            "whyForUser": "Prestige family saga with a heist spine; two seasons, both tight.",
            "confidence": 0.77, "runDate": "2026-07-16", "recommendedAt": now - 3 * DAY,
            "notifiedAt": now - 3 * DAY + MIN, "watchlistResult": "added",
        }),
    ));
    docs.push(rec(
        "preview-north-of-nowhere",
        "tmdb:movie:500005",
        500_005,
        "movie",
        "North of Nowhere",
        2025,
        "notified",
        json!({
            "confidence": 0.58, "runDate": "2026-07-10", "recommendedAt": now - 6 * DAY,
            "notifiedAt": now - 6 * DAY + MIN, "watchlistResult": "already_exists",
        }),
    ));
    docs.push(rec(
        "preview-harbor-lights", "tmdb:tv:200002", 200_002, "tv", "Harbor Lights", 2024, "watched",
        json!({
            "whyForUser": "Character-driven mystery, one tight 8-episode season.",
            "confidence": 0.74, "runDate": "2026-06-20", "recommendedAt": now - 25 * DAY,
            "notifiedAt": now - 25 * DAY + MIN, "resolvedAt": now - 4 * DAY, "watchlistResult": "added",
        }),
    ));
    docs.push((
        "email-activity",
        json!({
            "activityId": "ParcelTracker#preview-1",
            "pipeline": "ParcelTracker",
            "emailId": "preview-1",
            "subject": "Your order has shipped!",
            "from": "orders@example-store.com",
            "receivedAt": now - 3 * HOUR,
            "processedAt": now - 3 * HOUR + 30_000,
            "outcome": "processed",
            "admitReason": "triage: shipment notification with a UPS tracking number",
            "items": ["1Z999AA10123456784 (ups): submitted"],
        }),
    ));
    docs.push((
        "parcel-submitted-delivery",
        json!({
            "trackingNumber": "1Z999AA10123456784",
            "carrierCode": "ups",
            "description": "Your order has shipped!",
            "submittedAt": now - 3 * HOUR + 20_000,
            "emailId": "preview-1",
            "status": "submitted",
            "attempts": 1,
        }),
    ));
    docs.push((
        "parcel-deliveries-snapshot",
        json!({
            "key": "parcel",
            "fetchedAt": now - 12 * MIN,
            "deliveries": [
                {
                    "trackingNumber": "1Z999AA10123456784",
                    "carrierCode": "ups",
                    "carrierName": "UPS",
                    "description": "Camera",
                    "statusCode": 4,
                    "expected": "2026-10-09 00:00:00",
                    "events": [
                        {"description": "Out for delivery", "date": "2026-10-09 08:12:00", "location": "Springfield ST"},
                        {"description": "Arrived at facility", "date": "2026-10-08 21:40:00", "location": "Shelbyville ST"},
                    ],
                    "eventCount": 2,
                },
                {
                    "trackingNumber": "LP00123456789012CN",
                    "carrierCode": "aliex",
                    "carrierName": "AliExpress",
                    "description": "Cable kit",
                    "statusCode": 0,
                    "events": [{"description": "Delivered", "date": "07.10.2026 15:44"}],
                    "eventCount": 1,
                },
            ],
        }),
    ));
    docs.push((
        "parcel-deliveries-read-state",
        json!({
            "key": "parcel",
            "attempts": [now - 12 * MIN],
            "lastAttemptAt": now - 12 * MIN,
            "lastSuccessAt": now - 12 * MIN,
            "lastStatus": 200,
        }),
    ));
    docs
}

/// Writes each seed document through its entity's typed model (a shape the
/// Rust model rejects fails the preview instead of hiding the row).
pub async fn seed(ctx: &AppContext, now: i64) -> anyhow::Result<()> {
    let catalog = crate::compat_audit::entity_catalog();
    let mut rows = Vec::new();
    for (entity, payload) in seed_documents(now) {
        let descriptor = catalog
            .iter()
            .find(|d| d.name == entity)
            .ok_or_else(|| anyhow::anyhow!("unknown seed entity {entity}"))?;
        let typed = (descriptor.roundtrip)(&json_to_js_value(&payload))
            .map_err(|e| anyhow::anyhow!("seed {entity}: {e}"))?;
        let pk =
            (descriptor.recompute_pk)(&typed).map_err(|e| anyhow::anyhow!("seed {entity}: {e}"))?;
        rows.push((pk, typed, descriptor.name, descriptor.version));
    }
    ctx.store
        .write(move |tx| {
            for (pk, data, entity, version) in &rows {
                tx.upsert_doc(
                    pk,
                    data,
                    DocMeta {
                        entity: Some((*entity).to_owned()),
                        version: *version,
                        expires_at: None,
                        updated_at: None,
                    },
                )?;
            }
            Ok::<_, omni_store::StoreError>(())
        })
        .await?;
    let pets = omni_personal::pets::persistence::PetStore::open(&ctx.store).await?;
    pets.upsert_pet(&omni_store::table::pets::PetRow {
        pet_id: "mochi".to_owned(),
        name: "Mochi".to_owned(),
        current_weight: 11.3,
        updated_at: omni_core::js::to_iso_string(now),
    })
    .await?;
    for day in (0..=90i64).rev() {
        #[allow(clippy::cast_precision_loss)]
        let d = day as f64;
        let weight = 11.6 - d * 0.004
            + (d / 6.0).sin() * 0.15
            + (((day * 7919) % 13) as f64 / 13.0 - 0.5) * 0.2;
        pets.insert_weight_reading(&omni_store::table::pets::WeightHistoryRow {
            pet_id: "mochi".to_owned(),
            timestamp: omni_core::js::to_iso_string(now - day * DAY),
            weight: (weight * 100.0).round() / 100.0,
        })
        .await?;
    }
    Ok(())
}

/// The preview's environment: no credentials, throwaway paths.
pub fn preview_env(dir: &Path, port: u16) -> std::collections::BTreeMap<String, String> {
    let mut env = std::collections::BTreeMap::new();
    env.insert("FRONTEND_PORT".to_owned(), port.to_string());
    env.insert(
        "DB_NAME".to_owned(),
        dir.join("docstore.db").display().to_string(),
    );
    // Lets RetroRex's Kick binding through; nothing ever calls Kick.
    env.insert("KICK_CLIENT_ID".to_owned(), "preview".to_owned());
    env.insert("KICK_CLIENT_SECRET".to_owned(), "preview".to_owned());
    if let Ok(tz) = std::env::var("TZ") {
        env.insert("TZ".to_owned(), tz);
    }
    env
}

async fn run(port: u16, web_dist: PathBuf, dir: &Path) -> anyhow::Result<()> {
    let config = Arc::new(omni_config::Config::from_env(&preview_env(dir, port))?);
    let clock: SharedClock = Arc::new(SystemClock);
    let http = HttpClient::new(HttpConfig {
        offline: true,
        ..HttpConfig::default()
    })?;
    let foundation =
        Foundation::with_http(config.clone(), clock.clone(), SideEffectMode::Record, http);
    let _ = logging::install(
        LoggingSetup {
            level: omni_core::LogLevel::Info,
            logs_path: None,
            tz: jiff::tz::TimeZone::UTC,
            clock: clock.clone(),
        },
        Some(foundation.run_logs.clone()),
        None,
    );
    let store = omni_store::Store::open(
        &config.db_path(),
        omni_store::StoreOptions::new(clock.clone()),
    )
    .await?;
    let mut paths = app_paths(&config, web_dist.clone());
    // Stylist evidence and archives stay in the throwaway directory too.
    paths.stylist_private = dir.join("stylist-private");
    let ctx = context_over(&foundation, store, paths);
    let mut subsystems = crate::wiring::wire(&ctx, clock.now_ms()).await?.subsystems;
    let trace = BootTrace::default();
    boot::migrate(&ctx, boot::all_entities(&subsystems), &trace).await?;
    let streamers = omni_live::config::rows_for(preview_streamers(), clock.now_ms())?;
    ctx.store
        .write(move |tx| {
            for row in &streamers {
                omni_store::entity::EntityWrite::upsert(
                    tx,
                    row,
                    omni_store::entity::UpsertOpts::default(),
                )?;
            }
            Ok::<_, omni_store::StoreError>(())
        })
        .await?;
    for phase in [BootPhase::Migrate, BootPhase::Services] {
        boot::run_steps(&ctx, &mut subsystems, phase, &trace).await?;
    }
    ctx.tasks.initialize().await?;
    boot::run_steps(&ctx, &mut subsystems, BootPhase::Reconcile, &trace).await?;
    seed(&ctx, clock.now_ms()).await?;
    crate::preview_stylist::seed(&ctx).await?;
    for task in fake_tasks()? {
        ctx.tasks.track(task)?;
    }
    let (router, ops) = boot::app_router(&ctx, &mut subsystems, &web_dist);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    let shutdown = ctx.shutdown.clone();
    let server = omni_core::spawn::spawn_tracked(&ctx.tracker, "http-server", async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(async move { shutdown.cancelled().await })
            .await
    });
    omni_core::spawn::spawn_tracked(
        &ctx.tracker,
        "dashboard-hub",
        ops.dashboard.clone().listen(),
    );
    Scheduler::start(ctx.tasks.clone(), ctx.shutdown.clone(), &ctx.tracker);
    tracing::info!(target: LOG, "Preview server ready on http://127.0.0.1:{port}/");
    let _ = tokio::signal::ctrl_c().await;
    ctx.shutdown.cancel();
    ctx.tracker.close();
    let _ = tokio::time::timeout(boot::SHUTDOWN_BOUND, ctx.tracker.wait()).await;
    server.await??;
    Ok(())
}

/// Entry for `--preview`.
pub async fn main(port: Option<u16>, web_dist: PathBuf) -> u8 {
    let dir = std::env::temp_dir().join(format!("omni-preview-{}", omni_core::ids::uuid_v4()));
    if let Err(error) = std::fs::create_dir_all(&dir) {
        tracing::error!(target: LOG, error = %error, "Cannot create the preview directory");
        return 1;
    }
    let result = run(port.unwrap_or(DEFAULT_PORT), web_dist, &dir).await;
    let _ = std::fs::remove_dir_all(&dir);
    match result {
        Ok(()) => 0,
        Err(error) => {
            tracing::error!(target: LOG, error = %format!("{error:#}"), "Preview failed");
            1
        }
    }
}
