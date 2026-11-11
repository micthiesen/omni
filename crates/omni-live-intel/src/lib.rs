//! Livestream intelligence: bounded audio capture through yt-dlp and
//! ffmpeg, local speech through sherpa-onnx, viewer-surge detection, Destiny
//! guest detection, rolling summaries and Pushover alerts.
//!
//! Invariants (AGENTS.md): surges compare against a flat 5-20-minute-old
//! baseline after the post-start ramp, platform surges must also reach the
//! typical session peak, sparse baselines suppress, two consecutive candidates
//! are required, `destiny_guest` and `viewer_surge` alert at most once per
//! session, the 30-minute cooldown rolls back on delivery failure, and model
//! spend is capped by the monthly budget over `cost-event` rows.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use omni_runtime::{AppContext, BootError, BootPhase, BootStep, ManagedEntity, Subsystem};
use omni_store::entity::EntityDescriptor;

pub mod alert_policy;
pub mod anomaly;
pub mod audio;
pub mod classifier;
pub mod enroll;
pub mod js_math;
pub mod model_check;
pub mod observation;
pub mod persistence;
pub mod port;
pub mod presence_policy;
pub mod routes;
pub mod service;
pub mod sherpa;
pub mod speech;
pub mod summary_text;
pub mod transcript;
pub mod types;
pub mod voice_evidence;
pub mod voice_targets;
pub mod work_queue;

/// Log target.
pub const LOG: &str = "Main:LivestreamIntelligence";
/// The user agent passed to yt-dlp.
pub const USER_AGENT: &str = omni_http::USER_AGENT;

use crate::audio::LivestreamAudioCapture;
use crate::classifier::LivestreamClassifier;
use crate::port::IntelligencePort;
use crate::routes::IntelState;
use crate::service::{LivestreamIntelligenceService, ServiceDeps, ServiceSettings};
use crate::sherpa::SherpaBackend;
use crate::speech::{BlockingSpeech, LocalSpeechRuntime, LocalTranscriber, SpeechRecognitionError};
use crate::types::{
    LivestreamDiagnosticsData, LivestreamFeedbackData, LivestreamIntelligenceData,
    LivestreamIntelligenceEventData,
};

/// Entities this package owns.
pub fn entities() -> Vec<EntityDescriptor> {
    vec![
        EntityDescriptor::of::<LivestreamIntelligenceData>(),
        EntityDescriptor::of::<LivestreamFeedbackData>(),
        EntityDescriptor::of::<LivestreamDiagnosticsData>(),
        EntityDescriptor::of::<LivestreamIntelligenceEventData>(),
    ]
}

fn managed_entities() -> Vec<ManagedEntity> {
    let entry = |entity: EntityDescriptor,
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
    vec![
        entry(
            EntityDescriptor::of::<LivestreamIntelligenceData>(),
            "Livestream intelligence",
            "Current semantic metadata, summaries, chapters, and alert state.",
            &["streamerId"],
        ),
        entry(
            EntityDescriptor::of::<LivestreamFeedbackData>(),
            "Livestream alert feedback",
            "Useful, not-useful, and false-positive alert corrections.",
            &["feedbackId"],
        ),
        entry(
            EntityDescriptor::of::<LivestreamDiagnosticsData>(),
            "Livestream diagnostics",
            "Latest metadata, voice, summary, and alert pipeline status per streamer.",
            &["streamerId"],
        ),
        entry(
            EntityDescriptor::of::<LivestreamIntelligenceEventData>(),
            "Livestream intelligence events",
            "Bounded timeline of meaningful livestream intelligence decisions and failures.",
            &["eventId"],
        ),
    ]
}

/// Loads the native speech runtime (blocking; run off the async threads).
pub fn load_speech_runtime(
    model_dir: &std::path::Path,
    voiceprint: Option<&std::path::Path>,
    threshold: f64,
) -> Result<LocalSpeechRuntime<SherpaBackend>, SpeechRecognitionError> {
    LocalSpeechRuntime::create(model_dir, voiceprint, threshold, SherpaBackend::new)
}

/// `None` while disabled; a missing
/// or corrupt model file or a bad voiceprint is an error (the boot step
/// logs it and leaves intelligence disabled).
pub async fn create_service(
    ctx: &AppContext,
) -> Result<Option<LivestreamIntelligenceService>, SpeechRecognitionError> {
    let config = &ctx.config;
    if !config.livestream_intelligence_enabled {
        return Ok(None);
    }
    let model_dir = PathBuf::from(&config.livestream_model_dir);
    let voiceprint = config
        .livestream_destiny_voiceprint_path
        .as_deref()
        .filter(|p| !p.is_empty())
        .map(PathBuf::from);
    let threshold = config.livestream_destiny_speaker_threshold;
    let runtime = tokio::task::spawn_blocking(move || {
        load_speech_runtime(&model_dir, voiceprint.as_deref(), threshold)
    })
    .await
    .map_err(|e| SpeechRecognitionError::new("initialize livestream speech models", e))??;
    let runtime = Arc::new(runtime);
    // Other subsystems (YouTube transcripts) transcribe through the same
    // loaded models.
    if ctx
        .ports
        .set_speech_transcriber(Arc::new(LocalTranscriber(Arc::clone(&runtime))))
        .is_err()
    {
        tracing::warn!(target: LOG, "The speech transcriber port was already set");
    }
    let deps = ServiceDeps {
        store: ctx.store.clone(),
        clock: ctx.clock.clone(),
        pushover: ctx.pushover.clone(),
        costs: ctx.costs.clone(),
        capture: Arc::new(LivestreamAudioCapture::new(
            config.yt_dlp_bin(),
            config.ffmpeg_bin(),
            ctx.clock.clone(),
        )),
        speech: Arc::new(BlockingSpeech(runtime)),
        classifier: Arc::new(LivestreamClassifier::new(
            ctx.ai.clone(),
            Arc::clone(&ctx.config),
            ctx.store.clone(),
            ctx.clock.clone(),
        )),
        tracker: ctx.tracker.clone(),
        settings: ServiceSettings::from_config(config),
    };
    Ok(Some(LivestreamIntelligenceService::new(deps)))
}

/// The livestream intelligence subsystem: routes, entities, data-manager rows and a Services-phase
/// boot step that loads the speech runtime (when enabled), drains it on
/// shutdown and installs the `LiveIntelligence` port.
pub fn subsystem(ctx: &AppContext) -> Subsystem {
    let state = IntelState {
        store: ctx.store.clone(),
        clock: ctx.clock.clone(),
        ports: ctx.ports.clone(),
        service: Arc::new(OnceLock::new()),
    };
    let boot_state = state.clone();
    let boot = BootStep {
        phase: BootPhase::Services,
        name: "livestream-intelligence",
        run: Box::new(move |ctx: AppContext| {
            Box::pin(async move {
                const STEP: &str = "livestream-intelligence";
                // A missing or corrupt model or a bad voiceprint fails closed:
                // intelligence stays disabled with an ERROR (which alerts),
                // and the rest of the process keeps running.
                let service = match create_service(&ctx).await {
                    Ok(service) => service,
                    Err(error) => {
                        tracing::error!(
                            target: LOG,
                            "Livestream intelligence disabled: {error}"
                        );
                        None
                    }
                };
                // Disabled: no port, so the live-check task has no observer and
                // MCP has no diagnostics provider (its `livestreamIntelligence`
                // capability is the port's presence).
                let Some(service) = service else {
                    return Ok(());
                };
                let shutdown = ctx.shutdown.clone();
                let draining = service.clone();
                omni_core::spawn::spawn_tracked(
                    &ctx.tracker,
                    "livestream-intelligence-close",
                    async move {
                        shutdown.cancelled().await;
                        draining.close().await;
                    },
                );
                // The cell is only set here, once.
                let _ = boot_state.service.set(service);
                ctx.ports
                    .set_live_intelligence(Arc::new(IntelligencePort::new(boot_state)))
                    .map_err(|e| BootError::new(STEP, e.to_string()))
            })
        }),
    };
    Subsystem {
        name: "livestream-intelligence",
        router: routes::router(state),
        entities: entities(),
        managed_entities: managed_entities(),
        boot_steps: vec![boot],
        ..Subsystem::default()
    }
}
