//! Shared helpers for the omni integration tests.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use axum::Router;
use axum::body::{Body, BodyDataStream};
use axum::http::{Request, StatusCode, header};
use futures::StreamExt as _;
use omni::data_manager::DataManager;
use omni::ops::{OpsState, router};
use omni_runtime::AppContext;
use tower::ServiceExt as _;

/// One parsed SSE frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub event: String,
    pub id: Option<String>,
    pub data: String,
}

/// Reads frames from an SSE response body.
pub struct SseReader {
    body: BodyDataStream,
    buffer: String,
}

impl SseReader {
    pub fn new(body: Body) -> Self {
        Self {
            body: body.into_data_stream(),
            buffer: String::new(),
        }
    }

    fn take_frame(&mut self) -> Option<Frame> {
        let end = self.buffer.find("\n\n")?;
        let raw: String = self.buffer.drain(..end + 2).collect();
        let mut frame = Frame {
            event: "message".to_owned(),
            id: None,
            data: String::new(),
        };
        for line in raw.lines() {
            if let Some(v) = line.strip_prefix("event:") {
                frame.event = v.trim_start().to_owned();
            } else if let Some(v) = line.strip_prefix("id:") {
                frame.id = Some(v.trim_start().to_owned());
            } else if let Some(v) = line.strip_prefix("data:") {
                if !frame.data.is_empty() {
                    frame.data.push('\n');
                }
                frame.data.push_str(v.strip_prefix(' ').unwrap_or(v));
            }
        }
        Some(frame)
    }

    /// The next frame, or `None` when the stream ends or `wait` passes.
    pub async fn next(&mut self, wait: Duration) -> Option<Frame> {
        tokio::time::timeout(wait, async {
            loop {
                if let Some(frame) = self.take_frame() {
                    return Some(frame);
                }
                match self.body.next().await {
                    Some(Ok(chunk)) => self.buffer.push_str(&String::from_utf8_lossy(&chunk)),
                    _ => return None,
                }
            }
        })
        .await
        .ok()
        .flatten()
    }

    /// Whether the stream has ended (within `wait`).
    pub async fn ended(&mut self, wait: Duration) -> bool {
        matches!(tokio::time::timeout(wait, self.body.next()).await, Ok(None))
    }
}

/// The ops router over `ctx` with no managed entities.
pub fn ops(ctx: &AppContext) -> (Router, OpsState) {
    let state = OpsState::new(ctx.clone(), DataManager::new(ctx.store.clone(), Vec::new()));
    (router(state.clone()), state)
}

/// `GET path` returning the status and the streaming body.
pub async fn get(router: &Router, path: &str) -> (StatusCode, Body) {
    let request = Request::get(path)
        .header(header::HOST, "localhost")
        .body(Body::empty())
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    (response.status(), response.into_body())
}
