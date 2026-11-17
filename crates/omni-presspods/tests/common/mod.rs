//! Shared fakes for the PressPods integration tests. Nothing here reaches the
//! network or a real TTS/STT host: retrievers, TTS, the URL guard, Karakeep
//! and the worker kick are in-process fakes, models are `FakeModels`, and
//! ffmpeg/ffprobe are recording shell scripts.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use omni_http::Url;
use omni_presspods::agents::Agents;
use omni_presspods::error::PressPodsError;
use omni_presspods::karakeep::{Bookmarker, KarakeepError};
use omni_presspods::model::{AuthorGender, PressPodsEpisode};
use omni_presspods::persistence::{Persistence, secure_id};
use omni_presspods::retrievers::ArticleRetriever;
use omni_presspods::service::{PressPods, PressPodsDeps, RetrieverSource, UrlGuard, WorkerKick};
use omni_presspods::speech::audio_chain::AudioChain;
use omni_presspods::speech::providers::{TtsFactory, TtsProvider};
use omni_presspods::speech::stt::SttClient;
use omni_presspods::storage::AudioStore;
use omni_presspods::types::Article;
use omni_testkit::TestApp;

/// A `bash` script at `dir/name` with `body`, made executable.
pub fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/usr/bin/env bash\n{body}\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

/// A recording fake ffmpeg: appends each argv (args separated by `\x1f`) to
/// `log`, prints a loudnorm measurement for measure passes and writes a small
/// file at the output path (the last argument) otherwise.
pub fn fake_ffmpeg(dir: &Path, log: &Path) -> PathBuf {
    script(
        dir,
        "ffmpeg",
        &format!(
            r#"printf '%s\x1f' "$@" >> '{log}'
printf '\n' >> '{log}'
for arg in "$@"; do last="$arg"; case "$arg" in *print_format=json*) measure=1;; esac; done
if [ -n "$measure" ]; then
  printf '[Parsed_loudnorm_0]\n{{\n "input_i" : "-27.61",\n "input_tp" : "-4.47",\n "input_lra" : "18.06",\n "input_thresh" : "-39.20",\n "output_i" : "-16.58",\n "target_offset" : "0.58"\n}}\n' >&2
  exit 0
fi
printf 'FAKE-AUDIO' > "$last""#,
            log = log.display()
        ),
    )
}

/// A fake ffprobe printing a fixed duration.
pub fn fake_ffprobe(dir: &Path, seconds: &str) -> PathBuf {
    script(dir, "ffprobe", &format!("echo {seconds}"))
}

/// The recorded ffmpeg invocations.
pub fn ffmpeg_calls(log: &Path) -> Vec<Vec<String>> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.is_empty())
        .map(|line| {
            line.trim_end_matches('\x1f')
                .split('\x1f')
                .map(str::to_owned)
                .collect()
        })
        .collect()
}

/// A TTS provider returning fixed bytes and counting calls.
pub struct FakeTts {
    pub name: &'static str,
    pub denoise: bool,
    pub verify_length: bool,
    pub verify_content: bool,
    pub calls: AtomicUsize,
    pub fail_with: Mutex<Option<fn() -> PressPodsError>>,
    /// Makes the factory's preflight report the speech server unreachable.
    pub unreachable: AtomicBool,
    pub preflights: AtomicUsize,
}

/// What the fake preflight reports while [`FakeTts::unreachable`] is set.
pub const UNREACHABLE: &str =
    "speech server unreachable at http://tts.test:8000: tcp connect error: No route to host";

impl FakeTts {
    /// Higgs-like: denoised and verified (length and content).
    pub fn verified() -> Self {
        Self {
            name: "FakeHiggs",
            denoise: true,
            verify_length: true,
            verify_content: true,
            ..Self::clean()
        }
    }

    pub fn clean() -> Self {
        Self {
            name: "FakeClean",
            denoise: false,
            verify_length: false,
            verify_content: false,
            calls: AtomicUsize::new(0),
            fail_with: Mutex::new(None),
            unreachable: AtomicBool::new(false),
            preflights: AtomicUsize::new(0),
        }
    }
}

impl TtsProvider for FakeTts {
    fn provider_name(&self) -> &str {
        self.name
    }
    fn voice_name(&self) -> &str {
        "Fake Voice"
    }
    fn model_id(&self) -> &str {
        "fake-tts"
    }
    fn needs_denoise(&self) -> bool {
        self.denoise
    }
    fn verify_chunk_length(&self) -> bool {
        self.verify_length
    }
    fn verify_chunk_content(&self) -> bool {
        self.verify_content
    }
    /// The "audio" is the text itself, so [`EchoStt`] can transcribe it.
    fn synthesize_chunk<'a>(
        &'a self,
        text: &'a str,
    ) -> BoxFuture<'a, Result<Vec<u8>, PressPodsError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let failure = self.fail_with.lock().unwrap().map(|f| f());
        Box::pin(async move {
            match failure {
                Some(error) => Err(error),
                None => Ok(text.as_bytes().to_vec()),
            }
        })
    }
}

pub struct FakeTtsFactory(pub Arc<FakeTts>);

impl TtsFactory for FakeTtsFactory {
    fn create(
        &self,
        _gender: Option<AuthorGender>,
    ) -> Result<Arc<dyn TtsProvider>, PressPodsError> {
        Ok(self.0.clone())
    }

    fn preflight(&self) -> BoxFuture<'_, Result<(), PressPodsError>> {
        self.0.preflights.fetch_add(1, Ordering::SeqCst);
        let unreachable = self.0.unreachable.load(Ordering::SeqCst);
        Box::pin(async move {
            if unreachable {
                return Err(PressPodsError::SpeechUnreachable {
                    operation: "TTS preflight".to_owned(),
                    url: "http://tts.test:8000".to_owned(),
                    reason: "tcp connect error: No route to host".to_owned(),
                    timed_out: false,
                });
            }
            Ok(())
        })
    }
}

/// A scripted STT returning transcripts in order (repeating the last).
pub struct FakeStt {
    pub transcripts: Mutex<Vec<Result<String, String>>>,
}

impl SttClient for FakeStt {
    fn model_id(&self) -> &str {
        "fake-stt"
    }
    fn transcribe<'a>(&'a self, _mp3: &'a [u8]) -> BoxFuture<'a, Result<String, PressPodsError>> {
        let mut transcripts = self.transcripts.lock().unwrap();
        let next = if transcripts.len() > 1 {
            transcripts.remove(0)
        } else {
            transcripts
                .first()
                .cloned()
                .unwrap_or_else(|| Ok(String::new()))
        };
        Box::pin(async move {
            next.map_err(|m| PressPodsError::failed("transcribe PressPods chunk", m))
        })
    }
}

/// Transcribes [`FakeTts`] "audio" back to its text; the first
/// `truncate_first` calls return only the first 30% of the words (a
/// truncated read), and `fail` makes every call fail.
#[derive(Default)]
pub struct EchoStt {
    pub truncate_first: AtomicUsize,
    pub fail: bool,
    pub calls: AtomicUsize,
}

impl SttClient for EchoStt {
    fn model_id(&self) -> &str {
        "echo-stt"
    }
    fn transcribe<'a>(&'a self, mp3: &'a [u8]) -> BoxFuture<'a, Result<String, PressPodsError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let text = String::from_utf8_lossy(mp3).into_owned();
        let truncate = self
            .truncate_first
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok();
        let fail = self.fail;
        Box::pin(async move {
            if fail {
                return Err(PressPodsError::failed(
                    "transcribe PressPods chunk",
                    "STT 500: down",
                ));
            }
            if truncate {
                let words: Vec<&str> = text.split_whitespace().collect();
                return Ok(words[..words.len() * 3 / 10].join(" "));
            }
            Ok(text)
        })
    }
}

/// A retriever with a fixed outcome.
pub struct FakeRetriever {
    pub name: &'static str,
    pub result: Result<Article, String>,
}

impl ArticleRetriever for FakeRetriever {
    fn name(&self) -> &str {
        self.name
    }
    fn retrieve<'a>(
        &'a self,
        _url: &'a str,
        _ua: &'a str,
    ) -> BoxFuture<'a, Result<Article, PressPodsError>> {
        let result = self
            .result
            .clone()
            .map_err(|m| PressPodsError::failed(format!("retrieve article with {}", self.name), m));
        Box::pin(async move { result })
    }
}

pub struct FixedRetrievers(pub Vec<Arc<dyn ArticleRetriever>>);

impl RetrieverSource for FixedRetrievers {
    fn for_url(&self, _url: &str) -> Vec<Arc<dyn ArticleRetriever>> {
        self.0.clone()
    }
}

/// Accepts any syntactically valid URL (no DNS).
pub struct SyntaxGuard;

impl UrlGuard for SyntaxGuard {
    fn check<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Result<Url, PressPodsError>> {
        Box::pin(async move {
            omni_http::public::assert_public_http_url_syntax(url)
                .map_err(|e| PressPodsError::http("validate public PressPods URL", e))
        })
    }
}

/// Rejects every URL as DNS-private.
pub struct PrivateGuard;

impl UrlGuard for PrivateGuard {
    fn check<'a>(&'a self, _url: &'a str) -> BoxFuture<'a, Result<Url, PressPodsError>> {
        Box::pin(async move {
            Err(PressPodsError::failed(
                "validate public PressPods URL",
                "host resolves to a private address",
            ))
        })
    }
}

#[derive(Default)]
pub struct RecordingBookmarks(pub Mutex<Vec<String>>);

impl Bookmarker for RecordingBookmarks {
    fn add_bookmark<'a>(
        &'a self,
        url: &'a str,
        _tags: &'a [&'a str],
    ) -> BoxFuture<'a, Result<String, KarakeepError>> {
        self.0.lock().unwrap().push(url.to_owned());
        Box::pin(async move { Ok("https://karakeep.test/b".to_owned()) })
    }
}

#[derive(Default)]
pub struct CountingKick(pub AtomicUsize);

impl WorkerKick for CountingKick {
    fn kick(&self) -> Result<(), PressPodsError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// A wired service over the test app with in-process fakes.
pub struct Harness {
    pub app: TestApp,
    pub service: PressPods,
    pub tts: Arc<FakeTts>,
    pub bookmarks: Arc<RecordingBookmarks>,
    pub kicks: Arc<CountingKick>,
    pub ffmpeg_log: PathBuf,
    pub dir: tempfile::TempDir,
}

pub struct HarnessOptions {
    pub retrievers: Vec<Arc<dyn ArticleRetriever>>,
    pub tts: FakeTts,
    pub stt: Option<Arc<dyn SttClient>>,
    pub guard: Arc<dyn UrlGuard>,
    pub mode: omni_http::SideEffectMode,
    /// Extra environment on top of `omni_testkit::test_app_env()`.
    pub env: Vec<(&'static str, String)>,
}

impl Default for HarnessOptions {
    fn default() -> Self {
        Self {
            retrievers: Vec::new(),
            tts: FakeTts::clean(),
            stt: None,
            guard: Arc::new(SyntaxGuard),
            mode: omni_http::SideEffectMode::Live,
            env: Vec::new(),
        }
    }
}

pub async fn harness(options: HarnessOptions) -> Harness {
    let app = TestApp::new().await;
    let dir = tempfile::tempdir().unwrap();
    let tools = dir.path().join("bin");
    std::fs::create_dir_all(&tools).unwrap();
    let tmp = dir.path().join("tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    let assets = dir.path().join("assets/press-pods");
    std::fs::create_dir_all(&assets).unwrap();
    std::fs::write(assets.join("intro.mp3"), b"INTRO").unwrap();
    std::fs::write(assets.join("logo.jpeg"), b"\xff\xd8LOGO").unwrap();
    let log = dir.path().join("ffmpeg.log");
    let ffmpeg = fake_ffmpeg(&tools, &log);
    let ffprobe = fake_ffprobe(&tools, "2.5");
    let chain = AudioChain::new(
        ffmpeg.display().to_string(),
        ffprobe.display().to_string(),
        &assets.join("denoise.rnnn"),
        tmp,
    )
    .unwrap();
    let tts = Arc::new(options.tts);
    let bookmarks = Arc::new(RecordingBookmarks::default());
    let kicks = Arc::new(CountingKick::default());
    let ctx = &app.ctx;
    let mut env = omni_testkit::test_app_env();
    for (key, value) in options.env {
        env.insert(key.to_owned(), value);
    }
    let config = Arc::new(omni_config::Config::from_env(&env).unwrap());
    let tz = jiff::tz::TimeZone::get(&config.tz).unwrap();
    let service = PressPods::new(PressPodsDeps {
        config: config.clone(),
        clock: ctx.clock.clone(),
        persistence: Persistence::new(ctx.store.clone()),
        audio: AudioStore::new(dir.path().join("audio")),
        agents: Agents::new(ctx.ai.clone(), config.clone(), tz),
        retrievers: Arc::new(FixedRetrievers(options.retrievers)),
        tts: Arc::new(FakeTtsFactory(tts.clone())),
        stt: options.stt,
        chain,
        public_http: omni_http::public::PublicHttpClient::new(&omni_testkit::no_network()),
        url_guard: options.guard,
        bookmarks: bookmarks.clone(),
        pushover: ctx.pushover.clone(),
        costs: ctx.costs.clone(),
        worker: kicks.clone(),
        intro_path: assets.join("intro.mp3"),
        logo_path: assets.join("logo.jpeg"),
        mode: options.mode,
    });
    Harness {
        app,
        service,
        tts,
        bookmarks,
        kicks,
        ffmpeg_log: log,
        dir,
    }
}

/// A minimal episode row.
pub fn episode(article_url: &str, created_at: i64) -> PressPodsEpisode {
    let id = secure_id();
    PressPodsEpisode {
        audio_file: format!("{id}.mp3"),
        episode_id: id,
        title: "t".into(),
        author: None,
        author_gender: None,
        publication: None,
        domain: None,
        article_url: article_url.into(),
        normalized_url: None,
        lead_image_url: None,
        excerpt: None,
        content: "c".into(),
        voice_name: None,
        voice_provider: None,
        synthesized_seconds: None,
        chapters: None,
        chunks: None,
        duration_seconds: None,
        file_bytes: 1,
        retriever_name: None,
        retriever_seconds: None,
        retriever_attempts: None,
        costs: None,
        created_at,
        published_at: None,
        run_id: None,
        extra: Default::default(),
    }
}

/// An article as a retriever returns it.
pub fn article(text: &str, title: &str) -> Article {
    Article {
        title: Some(title.into()),
        text: text.into(),
        author: None,
        domain: Some("example.com".into()),
        url: format!("https://example.com/{title}"),
        published_at: None,
        lead_image_url: None,
    }
}

/// The metadata model's JSON reply.
pub fn metadata_json(valid: bool, rating: f64) -> String {
    serde_json::json!({
        "isValidArticle": valid,
        "title": "Rated title",
        "author": "Jane Writer",
        "authorGender": "female",
        "coauthors": null,
        "publication": "Example",
        "publishedAtISO": "2026-07-14T14:04:31Z",
        "leadImageUrl": null,
        "shortSummary": "A summary.",
        "contentRating": rating,
    })
    .to_string()
}
