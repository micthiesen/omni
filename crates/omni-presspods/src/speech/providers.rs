//! TTS providers and voices.
//!
//! A provider turns one narration chunk into MP3 bytes; chunking, per-chunk
//! mastering and stitching are provider-agnostic (`synthesize`). Higgs (the
//! self-hosted default) has a noise floor and an unreliable length, so it is
//! denoised and verified; ElevenLabs is clean and skips both.

use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use omni_config::{Config, TtsProvider as TtsProviderKind};
use omni_http::{HttpClient, HttpError, Method, SideEffectMode, Url};
use serde_json::json;

use crate::error::PressPodsError;
use crate::model::AuthorGender;

const MAX_TTS_CHUNK_BYTES: usize = 25 * 1024 * 1024;
const MAX_PREFLIGHT_BYTES: usize = 1024 * 1024;

/// A TTS backend.
pub trait TtsProvider: Send + Sync {
    fn provider_name(&self) -> &str;
    fn voice_name(&self) -> &str;
    fn model_id(&self) -> &str;
    /// Gates the audio-chain denoise pass (self-hosted models with a noise floor).
    fn needs_denoise(&self) -> bool;
    /// Length-verify each chunk and retry truncation or runaway output.
    fn verify_chunk_length(&self) -> bool;
    /// STT-transcribe each chunk and reject missing content.
    fn verify_chunk_content(&self) -> bool;
    fn synthesize_chunk<'a>(
        &'a self,
        text: &'a str,
    ) -> BoxFuture<'a, Result<Vec<u8>, PressPodsError>>;
}

/// Builds the configured provider for an episode's author gender.
pub trait TtsFactory: Send + Sync {
    fn create(&self, gender: Option<AuthorGender>) -> Result<Arc<dyn TtsProvider>, PressPodsError>;

    /// A cheap check, before a job spends anything, that the speech server
    /// can be reached. Fails only with [`PressPodsError::SpeechUnreachable`];
    /// a server that answers at all passes.
    fn preflight(&self) -> BoxFuture<'_, Result<(), PressPodsError>> {
        Box::pin(async { Ok(()) })
    }
}

/// An ElevenLabs voice.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Voice {
    pub id: String,
    pub name: String,
}

/// Male authors get the male narrator; female and unknown the
/// female one (`ELEVENLABS_VOICE_*` override the ids).
pub fn elevenlabs_voice(config: &Config, gender: Option<AuthorGender>) -> Voice {
    let custom = |id: &Option<String>| {
        id.as_ref().filter(|v| !v.is_empty()).map(|id| Voice {
            id: id.clone(),
            name: "Custom".to_owned(),
        })
    };
    if gender == Some(AuthorGender::Male) {
        custom(&config.elevenlabs_voice_male).unwrap_or_else(|| Voice {
            id: "nPczCjzI2devNBz1zQrb".to_owned(),
            name: "Brian".to_owned(),
        })
    } else {
        custom(&config.elevenlabs_voice_female).unwrap_or_else(|| Voice {
            id: "XrExE9yKIg1WjnnlVkGX".to_owned(),
            name: "Matilda".to_owned(),
        })
    }
}

/// Posts JSON and returns the bounded body; non-2xx is a status error.
async fn post_audio(
    http: &HttpClient,
    url: Url,
    headers: &[(&'static str, String)],
    body: &serde_json::Value,
    timeout: Duration,
    operation: &'static str,
) -> Result<Vec<u8>, PressPodsError> {
    let mut request = http.request(Method::POST, url).timeout(timeout).json(body);
    for (name, value) in headers {
        request = request.header(*name, value.as_str());
    }
    let response = request
        .send_bounded(MAX_TTS_CHUNK_BYTES)
        .await
        .map_err(|e| PressPodsError::http(operation, e))?;
    if !response.status.is_success() {
        return Err(PressPodsError::http(
            operation,
            HttpError::Status {
                status: response.status.as_u16(),
                body: String::from_utf8_lossy(&response.body[..response.body.len().min(4096)])
                    .into_owned(),
            },
        ));
    }
    Ok(response.body.to_vec())
}

/// Retries `attempt` after transient failures: `retries` extra tries,
/// exponential from `base`.
async fn with_retries<F, Fut>(
    retries: u32,
    base: Duration,
    label: &str,
    attempt: F,
) -> Result<Vec<u8>, PressPodsError>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<Vec<u8>, PressPodsError>>,
{
    let mut tries = 0u32;
    loop {
        match attempt().await {
            Ok(bytes) => return Ok(bytes),
            Err(error) => {
                tracing::warn!(target: "PressPods", error = %error, "{label} chunk request failed");
                if tries < retries && error.is_retryable() {
                    tokio::time::sleep(base * 2u32.pow(tries)).await;
                    tries += 1;
                } else {
                    return Err(error);
                }
            }
        }
    }
}

fn refuse_recorded(operation: &'static str) -> PressPodsError {
    PressPodsError::Failed {
        operation: operation.to_owned(),
        message: "TTS synthesis is disabled in record mode".to_owned(),
        retryable: Some(false),
    }
}

/// Higgs Audio v3 on a self-hosted mlx-audio server (OpenAI-shaped API).
pub struct HiggsProvider {
    http: HttpClient,
    mode: SideEffectMode,
    base_url: String,
    model_id: String,
    voice_name: String,
    voice: serde_json::Map<String, serde_json::Value>,
}

/// Higgs reads ~20 chars/s at 1.0; 0.9 is nearer a natural narration pace.
const HIGGS_SPEED: f64 = 0.9;
/// Output token cap: a complete ~900-char chunk needs ~1700; bounds a runaway.
const HIGGS_MAX_TOKENS: u32 = 3000;
const HIGGS_TIMEOUT: Duration = Duration::from_secs(4 * 60);
/// A reachable mlx-audio server lists its models in well under a second.
pub const SPEECH_PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(5);
pub const SPEECH_PREFLIGHT_OPERATION: &str = "TTS preflight";
pub const HIGGS_DEFAULT_MODEL: &str = "bosonai/higgs-audio-v3-tts-4b";

impl HiggsProvider {
    pub fn new(
        http: HttpClient,
        config: &Config,
        gender: Option<AuthorGender>,
        mode: SideEffectMode,
    ) -> Result<Self, PressPodsError> {
        let base_url = config
            .presspods_tts_url
            .clone()
            .filter(|u| !u.is_empty())
            .ok_or_else(|| {
                PressPodsError::failed(
                    "create PressPods TTS provider",
                    "PRESSPODS_TTS_URL is not set (required for the Higgs provider)",
                )
            })?;
        let gender = if gender == Some(AuthorGender::Male) {
            "male"
        } else {
            "female"
        };
        let reference = match (
            &config.presspods_higgs_ref_audio,
            &config.presspods_higgs_ref_text,
        ) {
            (Some(audio), Some(text)) if !audio.is_empty() && !text.is_empty() => {
                Some((audio.clone(), text.clone()))
            }
            _ => None,
        };
        let mut voice = serde_json::Map::new();
        let voice_name = match reference {
            Some((audio, text)) => {
                voice.insert("ref_audio".to_owned(), json!(audio));
                voice.insert("ref_text".to_owned(), json!(text));
                "Higgs (cloned)".to_owned()
            }
            None => {
                voice.insert("gender".to_owned(), json!(gender));
                format!("Higgs ({gender})")
            }
        };
        Ok(Self {
            http,
            mode,
            base_url,
            model_id: config
                .presspods_tts_model
                .clone()
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| HIGGS_DEFAULT_MODEL.to_owned()),
            voice_name,
            voice,
        })
    }
}

impl TtsProvider for HiggsProvider {
    fn provider_name(&self) -> &str {
        "Higgs"
    }
    fn voice_name(&self) -> &str {
        &self.voice_name
    }
    fn model_id(&self) -> &str {
        &self.model_id
    }
    fn needs_denoise(&self) -> bool {
        true
    }
    fn verify_chunk_length(&self) -> bool {
        true
    }
    fn verify_chunk_content(&self) -> bool {
        true
    }

    fn synthesize_chunk<'a>(
        &'a self,
        text: &'a str,
    ) -> BoxFuture<'a, Result<Vec<u8>, PressPodsError>> {
        const OPERATION: &str = "synthesize Higgs chunk";
        Box::pin(async move {
            if self.mode == SideEffectMode::Record {
                return Err(refuse_recorded(OPERATION));
            }
            let url = Url::parse(&format!("{}/v1/audio/speech", self.base_url)).map_err(|e| {
                PressPodsError::http(OPERATION, HttpError::InvalidUrl(e.to_string()))
            })?;
            let mut body = serde_json::Map::new();
            body.insert("model".to_owned(), json!(self.model_id));
            body.insert("input".to_owned(), json!(text));
            body.extend(self.voice.clone());
            body.insert("speed".to_owned(), json!(HIGGS_SPEED));
            body.insert("max_tokens".to_owned(), json!(HIGGS_MAX_TOKENS));
            body.insert("response_format".to_owned(), json!("mp3"));
            let body = serde_json::Value::Object(body);
            // Only network blips retry here (5xx/429/transport, never 4xx);
            // content-quality retries are the verifier's job.
            with_retries(1, Duration::from_secs(2), "Higgs", || {
                post_audio(
                    &self.http,
                    url.clone(),
                    &[],
                    &body,
                    HIGGS_TIMEOUT,
                    OPERATION,
                )
            })
            .await
            .map_err(|error| match &error {
                PressPodsError::Http { source, .. } => PressPodsError::speech_unreachable(
                    OPERATION,
                    &self.base_url,
                    source,
                    HIGGS_TIMEOUT,
                )
                .unwrap_or(error),
                _ => error,
            })
        })
    }
}

/// ElevenLabs v3: "Natural" stability, fixed seed for reproducibility.
pub struct ElevenLabsProvider {
    http: HttpClient,
    mode: SideEffectMode,
    api_key: Option<String>,
    voice: Voice,
}

pub const ELEVENLABS_MODEL: &str = "eleven_v3";
const ELEVENLABS_ENDPOINT: &str = "https://api.elevenlabs.io/v1/text-to-speech";
const ELEVENLABS_OUTPUT_FORMAT: &str = "mp3_44100_128";
const ELEVENLABS_SEED: u32 = 4242;
const ELEVENLABS_TIMEOUT: Duration = Duration::from_secs(5 * 60);

impl ElevenLabsProvider {
    pub fn new(
        http: HttpClient,
        config: &Config,
        gender: Option<AuthorGender>,
        mode: SideEffectMode,
    ) -> Self {
        Self {
            http,
            mode,
            api_key: config.elevenlabs_api_key.clone().filter(|k| !k.is_empty()),
            voice: elevenlabs_voice(config, gender),
        }
    }
}

impl TtsProvider for ElevenLabsProvider {
    fn provider_name(&self) -> &str {
        "ElevenLabs"
    }
    fn voice_name(&self) -> &str {
        &self.voice.name
    }
    fn model_id(&self) -> &str {
        ELEVENLABS_MODEL
    }
    fn needs_denoise(&self) -> bool {
        false
    }
    fn verify_chunk_length(&self) -> bool {
        false
    }
    fn verify_chunk_content(&self) -> bool {
        false
    }

    fn synthesize_chunk<'a>(
        &'a self,
        text: &'a str,
    ) -> BoxFuture<'a, Result<Vec<u8>, PressPodsError>> {
        const OPERATION: &str = "synthesize ElevenLabs chunk";
        Box::pin(async move {
            let Some(api_key) = self.api_key.as_deref() else {
                return Err(PressPodsError::failed(
                    OPERATION,
                    "ELEVENLABS_API_KEY is not set",
                ));
            };
            if self.mode == SideEffectMode::Record {
                return Err(refuse_recorded(OPERATION));
            }
            let url = Url::parse(&format!(
                "{ELEVENLABS_ENDPOINT}/{}?output_format={ELEVENLABS_OUTPUT_FORMAT}",
                self.voice.id
            ))
            .map_err(|e| PressPodsError::http(OPERATION, HttpError::InvalidUrl(e.to_string())))?;
            let body = json!({
                "text": text,
                "model_id": ELEVENLABS_MODEL,
                "seed": ELEVENLABS_SEED,
                "voice_settings": { "stability": 0.5, "use_speaker_boost": true },
            });
            let headers = [("xi-api-key", api_key.to_owned())];
            with_retries(2, Duration::from_secs(2), "ElevenLabs", || {
                post_audio(
                    &self.http,
                    url.clone(),
                    &headers,
                    &body,
                    ELEVENLABS_TIMEOUT,
                    OPERATION,
                )
            })
            .await
        })
    }
}

/// The production factory: `PRESSPODS_TTS_PROVIDER` picks Higgs or ElevenLabs.
pub struct ConfiguredTts {
    pub http: HttpClient,
    pub config: Arc<Config>,
    pub mode: SideEffectMode,
}

impl TtsFactory for ConfiguredTts {
    fn create(&self, gender: Option<AuthorGender>) -> Result<Arc<dyn TtsProvider>, PressPodsError> {
        Ok(match self.config.presspods_tts_provider {
            TtsProviderKind::Elevenlabs => Arc::new(ElevenLabsProvider::new(
                self.http.clone(),
                &self.config,
                gender,
                self.mode,
            )),
            TtsProviderKind::Higgs => Arc::new(HiggsProvider::new(
                self.http.clone(),
                &self.config,
                gender,
                self.mode,
            )?),
        })
    }

    /// Higgs only: `GET /v1/models` on the mlx-audio server. ElevenLabs is a
    /// hosted API, and an unset URL keeps its own error from `create`.
    fn preflight(&self) -> BoxFuture<'_, Result<(), PressPodsError>> {
        Box::pin(async move {
            if self.config.presspods_tts_provider != TtsProviderKind::Higgs {
                return Ok(());
            }
            let Some(base_url) = self
                .config
                .presspods_tts_url
                .as_deref()
                .filter(|u| !u.is_empty())
            else {
                return Ok(());
            };
            let Ok(url) = Url::parse(&format!("{base_url}/v1/models")) else {
                return Ok(());
            };
            let answer = self
                .http
                .request(Method::GET, url)
                .timeout(SPEECH_PREFLIGHT_TIMEOUT)
                .send_bounded(MAX_PREFLIGHT_BYTES)
                .await;
            match answer {
                Ok(_) => Ok(()),
                Err(source) => match PressPodsError::speech_unreachable(
                    SPEECH_PREFLIGHT_OPERATION,
                    base_url,
                    &source,
                    SPEECH_PREFLIGHT_TIMEOUT,
                ) {
                    Some(unreachable) => Err(unreachable),
                    None => Ok(()),
                },
            }
        })
    }
}
