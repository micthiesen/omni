//! A recipe library on a temp store, driven the way an agent drives it:
//! through the MCP tool handlers.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use futures::future::BoxFuture;
use omni_mcp_kit::{Content, McpTool, ToolContext, ToolOutput};
use omni_recipes::Recipes;
use omni_runtime::AppContext;
use omni_runtime::ports::{FetchedPage, PageFetcher, PortError};
use omni_testkit::{RecordedEvents, TestApp};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt as _;

pub const ORIGIN: &str = "https://recipes.example.com";
pub const HOST: &str = "recipes.example.com";

pub const PICCATA: &str = include_str!("../golden/chicken-piccata.json");
pub const COOKIES: &str = include_str!("../golden/chocolate-chip-cookies.json");
pub const LASAGNE: &str = include_str!("../golden/lasagne-bolognese.json");
pub const ENCHILADAS: &str = include_str!("../golden/enchiladas.json");

pub fn doc(json: &str) -> Value {
    serde_json::from_str(json).unwrap()
}

/// Scripted pages for `recipes_import_url`.
#[derive(Default)]
pub struct FakePages {
    pub pages: Mutex<Option<Result<Vec<FetchedPage>, String>>>,
    pub asked: Mutex<Vec<String>>,
}

impl FakePages {
    pub fn serve(&self, pages: Vec<FetchedPage>) {
        *self.pages.lock().unwrap() = Some(Ok(pages));
    }

    pub fn fail(&self, message: &str) {
        *self.pages.lock().unwrap() = Some(Err(message.to_owned()));
    }
}

impl PageFetcher for FakePages {
    fn fetch_pages<'a>(
        &'a self,
        url: &'a str,
    ) -> BoxFuture<'a, Result<Vec<FetchedPage>, PortError>> {
        self.asked.lock().unwrap().push(url.to_owned());
        let result = match self.pages.lock().unwrap().clone() {
            Some(Ok(pages)) => Ok(pages),
            Some(Err(message)) => Err(PortError::Failed {
                message,
                transient: false,
            }),
            None => Err(PortError::Unavailable("pages")),
        };
        Box::pin(async move { result })
    }
}

pub struct Harness {
    pub app: TestApp,
    pub ctx: AppContext,
    pub recipes: Recipes,
    pub events: RecordedEvents,
    pub pages: Arc<FakePages>,
    tools: Vec<McpTool>,
    pub dist: tempfile::TempDir,
}

/// A failed tool call's text.
pub type ToolResult = Result<(Value, Vec<Content>), String>;

impl Harness {
    pub async fn new() -> Self {
        Self::build(|ctx| ctx, true).await
    }

    /// Without a page fetcher wired.
    pub async fn without_pages() -> Self {
        Self::build(|ctx| ctx, false).await
    }

    /// With the context changed before the service is built.
    pub async fn with(change: impl FnOnce(AppContext) -> AppContext) -> Self {
        Self::build(change, true).await
    }

    async fn build(change: impl FnOnce(AppContext) -> AppContext, with_pages: bool) -> Self {
        let app = TestApp::new().await;
        let ctx = change(app.ctx.clone());
        let events = RecordedEvents::install(&ctx.ports);
        let pages = Arc::new(FakePages::default());
        if with_pages {
            ctx.ports.set_page_fetcher(pages.clone()).unwrap();
        }
        let recipes = Recipes::from_context(&ctx).with_file_grace(Duration::ZERO);
        let tools = omni_recipes::mcp::tools(Arc::new(recipes.clone())).unwrap();
        Self {
            app,
            ctx,
            recipes,
            events,
            pages,
            tools,
            dist: tempfile::tempdir().unwrap(),
        }
    }

    pub async fn call_full(&self, name: &str, arguments: Value) -> ToolResult {
        let tool = self
            .tools
            .iter()
            .find(|tool| tool.meta.name == name)
            .unwrap_or_else(|| panic!("no tool {name}"));
        let context = ToolContext {
            call_id: format!("call-{name}"),
            cancel: CancellationToken::new(),
        };
        match tool.handler.call(arguments, context).await {
            Ok(ToolOutput::Structured(map)) => Ok((Value::Object(map), Vec::new())),
            Ok(ToolOutput::Custom {
                structured,
                content,
            }) => Ok((Value::Object(structured), content)),
            Err(error) => Err(error.message),
        }
    }

    /// The structured result of a call that must succeed.
    pub async fn ok(&self, name: &str, arguments: Value) -> Value {
        match self.call_full(name, arguments).await {
            Ok((value, _)) => value,
            Err(error) => panic!("{name} failed: {error}"),
        }
    }

    /// The error text of a call that must fail.
    pub async fn err(&self, name: &str, arguments: Value) -> String {
        match self.call_full(name, arguments).await {
            Ok((value, _)) => panic!("{name} succeeded: {value}"),
            Err(error) => error,
        }
    }

    /// Creates a recipe from one of the sample documents and returns its id.
    pub async fn create(&self, json: &str) -> String {
        let made = self
            .ok("recipes_create", json!({ "document": doc(json) }))
            .await;
        made["id"].as_str().unwrap().to_owned()
    }

    pub async fn tag(&self, name: &str, group: &str) {
        self.ok(
            "recipes_tags_edit",
            json!({ "ops": [{ "op": "create", "name": name, "group": group }] }),
        )
        .await;
    }

    /// Fond's router, as the host app serves it.
    pub fn router(&self) -> Router {
        omni_recipes::http::router(
            self.recipes.clone(),
            ORIGIN,
            self.dist.path().to_path_buf(),
            Some("0fed18e0aaaa".to_owned()),
        )
    }

    pub async fn advance(&self, duration: Duration) {
        tokio::time::advance(duration).await;
    }

    pub async fn advance_days(&self, days: u64) {
        self.advance(Duration::from_secs(days * 24 * 60 * 60)).await;
    }
}

pub struct HttpReply {
    pub status: StatusCode,
    pub headers: axum::http::HeaderMap,
    pub body: Vec<u8>,
}

impl HttpReply {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }
}

pub async fn send(router: &Router, request: Request<Body>) -> HttpReply {
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    HttpReply {
        status,
        headers,
        body,
    }
}

pub async fn get(router: &Router, path: &str) -> HttpReply {
    send(
        router,
        Request::get(path)
            .header(header::HOST, HOST)
            .body(Body::empty())
            .unwrap(),
    )
    .await
}

pub async fn post_json(router: &Router, path: &str, body: &Value) -> HttpReply {
    send(
        router,
        Request::post(path)
            .header(header::HOST, HOST)
            .header(header::ORIGIN, ORIGIN)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await
}

pub async fn upload(router: &Router, path: &str, bytes: Vec<u8>) -> HttpReply {
    send(
        router,
        Request::post(path)
            .header(header::HOST, HOST)
            .header(header::ORIGIN, ORIGIN)
            .header("x-fond-upload", "1")
            .header(header::CONTENT_TYPE, "image/jpeg")
            .body(Body::from(bytes))
            .unwrap(),
    )
    .await
}

/// A small synthetic photo: a colour gradient, `width` by `height`.
pub fn picture(format: image::ImageFormat, width: u32, height: u32, seed: u8) -> Vec<u8> {
    let pixels = image::RgbImage::from_fn(width, height, |x, y| {
        image::Rgb([
            (x * 255 / width.max(1)) as u8,
            (y * 255 / height.max(1)) as u8,
            seed,
        ])
    });
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(pixels)
        .write_to(&mut out, format)
        .unwrap();
    out.into_inner()
}

pub fn jpeg(seed: u8) -> Vec<u8> {
    picture(image::ImageFormat::Jpeg, 640, 480, seed)
}

pub fn png(seed: u8) -> Vec<u8> {
    picture(image::ImageFormat::Png, 320, 200, seed)
}

/// The base64 payloads of a result's image blocks.
pub fn image_blocks(content: &[Content]) -> Vec<Vec<u8>> {
    use base64::Engine as _;
    content
        .iter()
        .filter(|block| block.get("type") == Some(&json!("image")))
        .map(|block| {
            assert_eq!(block["mimeType"], "image/jpeg");
            base64::engine::general_purpose::STANDARD
                .decode(block["data"].as_str().unwrap())
                .unwrap()
        })
        .collect()
}
