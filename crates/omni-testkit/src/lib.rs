//! Test support. Dev-dependency only.
//!
//! Helpers follow the usual test-fixture convention: setup failures panic with
//! a message naming the step (`TestStore::new`, `TestApp::new`, `golden`), so
//! tests read straight-line. Nothing here reaches the network: HTTP clients
//! refuse DNS, Pushover and SMTP run in `SideEffectMode::Record`, and models
//! are scripted.
//!
//! `TestStore` and `TestApp` use the real `Store::open` and `Config::from_env`;
//! setup failures panic with the step name.

pub mod events;

pub use events::RecordedEvents;

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use futures::future::BoxFuture;
use omni_ai::{
    AiError, AiTool, GenerateRequest, GenerateResponse, LanguageModel, ModelId, ModelOverride,
    ModelRole, ToolSpec,
};
use omni_alerts::{Pushover, RecordedPush};
use omni_config::Config;
use omni_core::clock::{SharedClock, TestClock};
use omni_http::{HttpClient, HttpConfig, SideEffectMode};
use omni_mailer::Mailer;
use omni_runtime::{AppContext, AppPaths, Ports, Subsystem};
use omni_store::{Store, StoreOptions};
use omni_tasks::{EventBus, RunLogs, TaskRegistry};
use serde_json::Value;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tower::ServiceExt as _;

/// 2026-01-01T00:00:00Z, the default test epoch.
pub const TEST_EPOCH_MS: i64 = 1_767_225_600_000;

fn setup<T, E: std::fmt::Display>(step: &str, result: Result<T, E>) -> T {
    result.unwrap_or_else(|error| panic!("{step} failed: {error}"))
}

/// A file-backed WAL store in a temporary directory.
pub struct TestStore {
    pub store: Store,
    _dir: TempDir,
}

impl TestStore {
    pub async fn new(clock: SharedClock) -> Self {
        let dir = setup("TestStore::new tempdir", tempfile::tempdir());
        let store = setup(
            "TestStore::new Store::open",
            Store::open(&dir.path().join("docstore.db"), StoreOptions::new(clock)).await,
        );
        Self { store, _dir: dir }
    }

    /// Opens a private copy of a fixture database (the original is never written).
    pub async fn from_fixture(path: &Path) -> Self {
        let dir = setup("TestStore::from_fixture tempdir", tempfile::tempdir());
        let copy = dir.path().join("docstore.db");
        setup("TestStore::from_fixture copy", std::fs::copy(path, &copy));
        let clock: SharedClock = test_clock(TEST_EPOCH_MS);
        let store = setup(
            "TestStore::from_fixture Store::open",
            Store::open(&copy, StoreOptions::new(clock)).await,
        );
        Self { store, _dir: dir }
    }
}

/// Pair with `#[tokio::test(start_paused = true)]`.
pub fn test_clock(epoch_ms: i64) -> Arc<TestClock> {
    TestClock::new(epoch_ms)
}

/// Pushover messages captured by the test app.
#[derive(Clone)]
pub struct RecordedPushes(Pushover);

impl RecordedPushes {
    pub fn all(&self) -> Vec<RecordedPush> {
        self.0.recorded()
    }
}

/// SMTP submissions captured by the test app.
#[derive(Clone)]
pub struct RecordedMails(Option<Mailer>);

impl RecordedMails {
    pub fn all(&self) -> Vec<(Vec<String>, Vec<u8>)> {
        self.0.as_ref().map(Mailer::recorded).unwrap_or_default()
    }
}

/// Scripted language models. Responses are scripted per role (models resolved with
/// `Ai::model_for`) or per model id (`Ai::model`); each call pops the next one, and an
/// exhausted script fails the call with a 599 provider error.
#[derive(Clone, Default)]
pub struct FakeModels {
    scripts: Arc<Mutex<Scripts>>,
    requests: Arc<Mutex<Vec<FakeRequest>>>,
}

type Scripts = HashMap<ScriptKey, VecDeque<Result<GenerateResponse, FakeFailure>>>;

/// A request a fake model received, with the role it was resolved for (if any).
pub type FakeRequest = (Option<ModelRole>, GenerateRequest);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum ScriptKey {
    Role(ModelRole),
    Model(String),
}

/// A scripted provider failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FakeFailure {
    pub status: u16,
    pub message: String,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

impl FakeModels {
    /// Appends responses for `role`.
    pub fn script(&self, role: ModelRole, responses: Vec<GenerateResponse>) {
        lock(&self.scripts)
            .entry(ScriptKey::Role(role))
            .or_default()
            .extend(responses.into_iter().map(Ok));
    }

    /// Appends responses for a model id such as `openai:gpt-6-luna`.
    pub fn script_model(&self, id: &str, responses: Vec<GenerateResponse>) {
        lock(&self.scripts)
            .entry(ScriptKey::Model(id.to_owned()))
            .or_default()
            .extend(responses.into_iter().map(Ok));
    }

    /// Appends a provider failure (e.g. 503 to exercise retries) for `role`.
    pub fn script_failure(&self, role: ModelRole, failure: FakeFailure) {
        lock(&self.scripts)
            .entry(ScriptKey::Role(role))
            .or_default()
            .push_back(Err(failure));
    }

    /// Every request the fake models received, in order, with the role when known.
    pub fn requests(&self) -> Vec<FakeRequest> {
        lock(&self.requests).clone()
    }

    fn has_script(&self, key: &ScriptKey) -> bool {
        lock(&self.scripts).contains_key(key)
    }
}

struct FakeModel {
    id: ModelId,
    role: Option<ModelRole>,
    key: ScriptKey,
    models: FakeModels,
}

impl LanguageModel for FakeModel {
    fn id(&self) -> &ModelId {
        &self.id
    }

    fn role(&self) -> Option<ModelRole> {
        self.role
    }

    fn generate<'a>(
        &'a self,
        req: &'a GenerateRequest,
    ) -> BoxFuture<'a, Result<GenerateResponse, AiError>> {
        lock(&self.models.requests).push((self.role, req.clone()));
        let next = lock(&self.models.scripts)
            .get_mut(&self.key)
            .and_then(VecDeque::pop_front);
        let key = self.key.clone();
        Box::pin(async move {
            match next {
                Some(Ok(response)) => Ok(response),
                Some(Err(failure)) => Err(AiError::Provider {
                    status: failure.status,
                    message: failure.message,
                }),
                None => Err(AiError::Provider {
                    status: 599,
                    message: format!("no scripted response left for {key:?}"),
                }),
            }
        })
    }
}

impl ModelOverride for FakeModels {
    /// Every role-resolved model is faked; an id-resolved model is faked when that id
    /// has a script.
    fn model(&self, role: Option<ModelRole>, id: &ModelId) -> Option<Arc<dyn LanguageModel>> {
        let key = match role {
            Some(role) => ScriptKey::Role(role),
            None => {
                let key = ScriptKey::Model(id.to_string());
                if !self.has_script(&key) {
                    return None;
                }
                key
            }
        };
        Some(Arc::new(FakeModel {
            id: id.clone(),
            role,
            key,
            models: self.clone(),
        }))
    }
}

/// A tool for tool-loop tests: records its arguments and replies from a script.
#[derive(Clone)]
pub struct FakeTool {
    spec: ToolSpec,
    replies: Arc<Mutex<VecDeque<Result<Value, String>>>>,
    calls: Arc<Mutex<Vec<Value>>>,
}

impl FakeTool {
    pub fn new(name: &str, replies: Vec<Result<Value, String>>) -> Self {
        Self {
            spec: ToolSpec {
                name: name.to_owned(),
                description: format!("fake {name}"),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
            },
            replies: Arc::new(Mutex::new(replies.into())),
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Arguments of every call, in order.
    pub fn calls(&self) -> Vec<Value> {
        lock(&self.calls).clone()
    }
}

impl AiTool for FakeTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn call<'a>(&'a self, args: Value) -> BoxFuture<'a, Result<Value, String>> {
        lock(&self.calls).push(args);
        let reply = lock(&self.replies)
            .pop_front()
            .unwrap_or_else(|| Err(format!("no scripted reply left for {}", self.spec.name)));
        Box::pin(async move { reply })
    }
}

/// The `OMNI_DEVICE_LINK_TOKEN` [`TestApp`] boots with (strong, as config requires).
pub const TEST_DEVICE_LINK_TOKEN: &str = "test-device-link-token-ZYXWVUTSRQPONMLK-9876543210";

/// A full `AppContext` over a temp store, with recorders and fake models.
/// The environment `TestApp` boots with: fake Pushover and SMTP credentials so
/// pushes and mails reach the recorders (`SideEffectMode::Record`, nothing is sent),
/// and a device-link token.
pub fn test_app_env() -> BTreeMap<String, String> {
    [
        ("PUSHOVER_USER", "test-user"),
        ("PUSHOVER_TOKEN", "test-token"),
        ("OMNI_DEVICE_LINK_TOKEN", TEST_DEVICE_LINK_TOKEN),
        ("SMTP_HOST", "smtp.invalid"),
        ("SMTP_PORT", "587"),
        ("SMTP_USER", "test-user"),
        ("SMTP_PASS", "test-pass"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect()
}

pub struct TestApp {
    pub ctx: AppContext,
    pub pushes: RecordedPushes,
    pub mails: RecordedMails,
    pub ai: FakeModels,
    _dir: TempDir,
}

impl TestApp {
    pub async fn new() -> Self {
        let clock: SharedClock = test_clock(TEST_EPOCH_MS);
        let dir = setup("TestApp::new tempdir", tempfile::tempdir());
        let config = Arc::new(setup(
            "TestApp::new Config::from_env",
            Config::from_env(&test_app_env()),
        ));
        let store = setup(
            "TestApp::new Store::open",
            Store::open(
                &dir.path().join("docstore.db"),
                StoreOptions::new(clock.clone()),
            )
            .await,
        );
        let http = no_network();
        let public_http = omni_http::public::PublicHttpClient::new(&http);
        let pushover = Pushover::new(http.clone(), &config, SideEffectMode::Record);
        let mailer = omni_mailer::resolve_compose_config(&config)
            .map(|cfg| Mailer::new(cfg, SideEffectMode::Record));
        let costs = omni_ai::costs::CostRecorder::new(store.clone(), clock.clone());
        let fakes = FakeModels::default();
        let ai = omni_ai::Ai::new(http.clone(), &config, costs.clone())
            .with_override(Arc::new(fakes.clone()));
        let bus = EventBus::default();
        let tracker = TaskTracker::new();
        let logs = RunLogs::new(bus.clone(), clock.clone());
        let tasks = TaskRegistry::new(
            store.clone(),
            clock.clone(),
            bus.clone(),
            tracker.clone(),
            logs,
        );
        let root: PathBuf = dir.path().to_path_buf();
        let ctx = AppContext {
            config,
            store,
            clock,
            http,
            public_http,
            pushover: pushover.clone(),
            mailer: mailer.clone(),
            ai,
            costs,
            tasks,
            bus,
            shutdown: CancellationToken::new(),
            tracker,
            ports: Ports::default(),
            side_effects: SideEffectMode::Record,
            paths: AppPaths {
                data_dir: root.join("data"),
                assets_dir: root.join("assets"),
                web_dist: root.join("web"),
                reminders_private: root.join("reminders-private"),
                presspods_audio: root.join("presspods-audio"),
                stylist_private: root.join("stylist-private"),
            },
        };
        Self {
            ctx,
            pushes: RecordedPushes(pushover),
            mails: RecordedMails(mailer),
            ai: fakes,
            _dir: dir,
        }
    }

    /// The subsystem's router (state already applied).
    pub fn router(&self, s: &Subsystem) -> Router {
        s.router.clone()
    }

    /// `GET path`, returning the status and JSON body (`Null` when not JSON).
    pub async fn get_json(&self, router: &Router, path: &str) -> (StatusCode, Value) {
        let request = Request::builder()
            .method(Method::GET)
            .uri(path)
            .header(header::HOST, "localhost")
            .body(Body::empty());
        send_json(router, setup("TestApp::get_json request", request)).await
    }

    /// `POST path` with a JSON body and a same-origin `Origin`.
    pub async fn post_json(
        &self,
        router: &Router,
        path: &str,
        body: &Value,
    ) -> (StatusCode, Value) {
        let request = Request::builder()
            .method(Method::POST)
            .uri(path)
            .header(header::HOST, "localhost")
            .header(header::ORIGIN, "http://localhost")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()));
        send_json(router, setup("TestApp::post_json request", request)).await
    }

    /// `method path` with an optional JSON body and a same-origin `Origin`.
    pub async fn request_json(
        &self,
        router: &Router,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> (StatusCode, Value) {
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, "localhost")
            .header(header::ORIGIN, "http://localhost")
            .header(header::CONTENT_TYPE, "application/json")
            .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())));
        send_json(router, setup("TestApp::request_json request", request)).await
    }
}

async fn send_json(router: &Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = match router.clone().oneshot(request).await {
        Ok(response) => response,
        Err(never) => match never {},
    };
    let status = response.status();
    let bytes = setup(
        "read response body",
        axum::body::to_bytes(response.into_body(), usize::MAX).await,
    );
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// An HTTP client that refuses every DNS lookup; use wiremock's IP base URLs.
pub fn no_network() -> HttpClient {
    setup(
        "no_network HttpClient::new",
        HttpClient::new(HttpConfig {
            offline: true,
            ..HttpConfig::default()
        }),
    )
}

/// An offline client that sends requests for each of `origins` (e.g.
/// `https://api.openai.com`) to `server` instead, keeping path and query.
pub fn mock_http(server: &wiremock::MockServer, origins: &[&str]) -> HttpClient {
    let to = setup("mock_http server URL", omni_http::Url::parse(&server.uri()));
    let rewrites = origins
        .iter()
        .map(|origin| {
            (
                setup("mock_http origin", omni_http::Url::parse(origin)),
                to.clone(),
            )
        })
        .collect();
    no_network().with_overrides(omni_http::HttpOverrides { rewrites })
}

/// A local wiremock server.
pub async fn mock_server() -> wiremock::MockServer {
    wiremock::MockServer::start().await
}

/// One captured log event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapturedLog {
    pub level: tracing::Level,
    pub target: String,
    pub message: String,
}

/// Log events recorded while the capture is alive (thread-local default subscriber).
pub struct LogCapture {
    events: Arc<Mutex<Vec<CapturedLog>>>,
    _guard: tracing::subscriber::DefaultGuard,
}

impl LogCapture {
    pub fn events(&self) -> Vec<CapturedLog> {
        self.events
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}

struct CaptureLayer {
    events: Arc<Mutex<Vec<CapturedLog>>>,
}

struct MessageVisitor(String);

impl tracing::field::Visit for MessageVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{value:?}");
        } else {
            self.0.push_str(&format!(" {}={value:?}", field.name()));
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.0 = value.to_owned();
        } else {
            self.0.push_str(&format!(" {}={value}", field.name()));
        }
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CaptureLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut visitor = MessageVisitor(String::new());
        event.record(&mut visitor);
        self.events
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(CapturedLog {
                level: *event.metadata().level(),
                target: event.metadata().target().to_owned(),
                message: visitor.0,
            });
    }
}

/// Captures every event on the current thread until the returned value drops.
/// Use with `#[tokio::test]` (current-thread runtime).
pub fn capture_logs() -> LogCapture {
    use tracing_subscriber::layer::SubscriberExt as _;
    let events = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(CaptureLayer {
        events: events.clone(),
    });
    LogCapture {
        events,
        _guard: tracing::subscriber::set_default(subscriber),
    }
}

/// `crates/<calling crate>/tests/golden/<path>` parsed as JSON.
pub fn golden(path: &str) -> Value {
    let manifest = setup(
        "golden CARGO_MANIFEST_DIR",
        std::env::var("CARGO_MANIFEST_DIR"),
    );
    let file = PathBuf::from(manifest).join("tests/golden").join(path);
    let text = setup(
        &format!("golden read {}", file.display()),
        std::fs::read_to_string(&file),
    );
    setup(
        &format!("golden parse {}", file.display()),
        serde_json::from_str(&text),
    )
}

/// The committed node-cbor golden vectors (`crates/omni-store/tests/golden/cbor.json`).
pub mod node_cbor_fixtures {
    use std::path::PathBuf;

    use serde_json::Value;

    use super::setup;

    /// The `encode` case `name` of `crates/omni-store/tests/golden/cbor.json`: the
    /// bytes node-cbor wrote and the described JS value (the `{"t": ..}` view
    /// that `crates/omni-store/tests/cbor_golden.rs` describes).
    pub fn load(name: &str) -> (Vec<u8>, Value) {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../omni-store/tests/golden/cbor.json");
        let text = setup(
            &format!("read {}", path.display()),
            std::fs::read_to_string(&path),
        );
        let golden: Value = setup("parse cbor.json", serde_json::from_str(&text));
        let case = golden["encode"]
            .as_array()
            .and_then(|cases| cases.iter().find(|case| case["name"] == name))
            .unwrap_or_else(|| panic!("no cbor encode case named {name:?}"));
        let hex = case["hex"]
            .as_str()
            .unwrap_or_else(|| panic!("cbor case {name:?} has no hex"));
        let bytes = setup(&format!("cbor case {name:?} hex"), decode_hex(hex));
        (bytes, case["value"].clone())
    }

    fn decode_hex(hex: &str) -> Result<Vec<u8>, String> {
        if !hex.len().is_multiple_of(2) {
            return Err("odd length".to_owned());
        }
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).map_err(|e| e.to_string()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn no_network_refuses_dns() {
        let client = no_network();
        let url =
            omni_http::Url::parse("http://example.invalid/").unwrap_or_else(|e| panic!("{e}"));
        let result = client.raw().get(url).send().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn fake_models_pop_scripted_responses() {
        let fakes = FakeModels::default();
        let response = GenerateResponse::text("hi");
        fakes.script(ModelRole::Triage, vec![response.clone()]);
        let id = ModelId::parse("openai:gpt-6-luna").unwrap_or_else(|e| panic!("{e}"));
        let model = fakes
            .model(Some(ModelRole::Triage), &id)
            .unwrap_or_else(|| panic!("no fake"));
        let request = GenerateRequest::default();
        assert_eq!(model.generate(&request).await.ok(), Some(response));
        assert!(model.generate(&request).await.is_err());
        assert_eq!(fakes.requests().len(), 2);
    }

    #[tokio::test]
    async fn test_store_opens_a_private_database() {
        let store = TestStore::new(test_clock(TEST_EPOCH_MS)).await;
        let count = store
            .store
            .read(|docs| omni_store::DocOps::count_by_entity(docs, "anything"))
            .await;
        assert_eq!(count.ok(), Some(0));
    }

    #[tokio::test]
    async fn mock_http_rewrites_provider_origins() {
        let server = mock_server().await;
        let client = mock_http(&server, &["https://api.openai.com"]);
        let url = omni_http::Url::parse("https://api.openai.com/v1/responses?x=1")
            .unwrap_or_else(|e| panic!("{e}"));
        let request = client.request(omni_http::Method::POST, url);
        assert_eq!(
            request.url().as_str(),
            format!("{}/v1/responses?x=1", server.uri())
        );
    }

    #[tokio::test]
    async fn fake_models_cover_id_scripts_and_failures() {
        let fakes = FakeModels::default();
        let id = ModelId::parse("google:gemini-3.5-flash").unwrap_or_else(|e| panic!("{e}"));
        assert!(
            fakes.model(None, &id).is_none(),
            "unscripted ids use real clients"
        );
        fakes.script_model("google:gemini-3.5-flash", vec![GenerateResponse::text("x")]);
        let model = fakes
            .model(None, &id)
            .unwrap_or_else(|| panic!("scripted id"));
        let request = GenerateRequest::default();
        assert_eq!(
            model.generate(&request).await.ok().map(|r| r.text),
            Some("x".to_owned())
        );
        fakes.script_failure(
            ModelRole::Triage,
            FakeFailure {
                status: 503,
                message: "busy".to_owned(),
            },
        );
        let triage = fakes
            .model(Some(ModelRole::Triage), &id)
            .unwrap_or_else(|| panic!("role fake"));
        assert!(matches!(
            triage.generate(&request).await,
            Err(AiError::Provider { status: 503, .. })
        ));
    }

    #[test]
    fn loads_node_cbor_fixtures_by_name() {
        let (bytes, value) = node_cbor_fixtures::load("int:24");
        assert_eq!(bytes, vec![0x18, 0x18]);
        assert_eq!(value["t"], "num");
        let decoded = omni_store::cbor::decode(&bytes).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(omni_store::cbor::encode(&decoded), bytes);
    }

    #[tokio::test]
    async fn test_app_builds_a_recording_context() {
        let app = TestApp::new().await;
        assert!(matches!(app.ctx.side_effects, SideEffectMode::Record));
        let pushed = app
            .ctx
            .pushover
            .send(
                omni_alerts::PushoverChannel::General,
                omni_alerts::PushoverMessage {
                    message: "hi".to_owned(),
                    ..Default::default()
                },
            )
            .await;
        assert!(matches!(pushed, Ok(omni_alerts::PushOutcome::Recorded)));
        assert_eq!(app.pushes.all().len(), 1);
        let mailer = app.ctx.mailer.as_ref().unwrap_or_else(|| panic!("mailer"));
        mailer
            .send_notification("me@example.com", "Subject", "<p>hi</p>", "hi")
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(app.mails.all().len(), 1);
        assert!(app.ctx.tasks.names().is_empty());
    }

    #[test]
    fn captures_logs() {
        let capture = capture_logs();
        tracing::warn!(target: "Test", value = 1, "hello");
        assert_eq!(
            capture.events(),
            vec![CapturedLog {
                level: tracing::Level::WARN,
                target: "Test".to_owned(),
                message: "hello value=1".to_owned()
            }]
        );
    }
}
