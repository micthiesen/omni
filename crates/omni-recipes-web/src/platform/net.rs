//! Requests to Omni. A request that gets no answer is `Offline`; the caller
//! decides whether to retry.

use omni_api::fond::paths::UPLOAD_HEADER;
use serde::Serialize;
use serde::de::DeserializeOwned;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

use super::window;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetError {
    /// No answer: no connection, or the deadline passed.
    Offline,
    Status(u16),
    /// A success whose body the app cannot read.
    Decode,
}

impl NetError {
    /// The HTTP status, when the server answered.
    pub fn status(self) -> Option<u16> {
        match self {
            NetError::Offline => None,
            NetError::Status(status) => Some(status),
            NetError::Decode => Some(200),
        }
    }
}

const JSON_DEADLINE_MS: i32 = 20_000;
const UPLOAD_DEADLINE_MS: i32 = 120_000;

async fn send(
    method: &str,
    path: &str,
    body: Option<&JsValue>,
    headers: &[(&str, &str)],
    deadline_ms: i32,
) -> Result<String, NetError> {
    let init = web_sys::RequestInit::new();
    init.set_method(method);
    if let Some(body) = body {
        init.set_body(body);
    }
    let head = web_sys::Headers::new().map_err(|_| NetError::Offline)?;
    for (name, value) in headers {
        let _ = head.set(name, value);
    }
    init.set_headers(&head);
    let abort = web_sys::AbortController::new().map_err(|_| NetError::Offline)?;
    init.set_signal(Some(&abort.signal()));
    let cancel = Closure::once_into_js(move || abort.abort());
    let timer = window()
        .set_timeout_with_callback_and_timeout_and_arguments_0(cancel.unchecked_ref(), deadline_ms)
        .unwrap_or(0);
    let result = async {
        let response = JsFuture::from(window().fetch_with_str_and_init(path, &init))
            .await
            .map_err(|_| NetError::Offline)?;
        let response: web_sys::Response = response.dyn_into().map_err(|_| NetError::Offline)?;
        if !response.ok() {
            return Err(NetError::Status(response.status()));
        }
        let text = JsFuture::from(response.text().map_err(|_| NetError::Decode)?)
            .await
            .map_err(|_| NetError::Offline)?;
        Ok(text.as_string().unwrap_or_default())
    }
    .await;
    window().clear_timeout_with_handle(timer);
    result
}

pub async fn get_json<T: DeserializeOwned>(path: &str) -> Result<T, NetError> {
    let text = send(
        "GET",
        path,
        None,
        &[("accept", "application/json")],
        JSON_DEADLINE_MS,
    )
    .await?;
    serde_json::from_str(&text).map_err(|_| NetError::Decode)
}

pub async fn post_json<B: Serialize, T: DeserializeOwned>(
    path: &str,
    body: &B,
) -> Result<T, NetError> {
    let body = serde_json::to_string(body).map_err(|_| NetError::Decode)?;
    let text = send(
        "POST",
        path,
        Some(&JsValue::from_str(&body)),
        &[("content-type", "application/json")],
        JSON_DEADLINE_MS,
    )
    .await?;
    serde_json::from_str(&text).map_err(|_| NetError::Decode)
}

/// Uploads one JPEG; the body of the answer is returned as text.
pub async fn post_jpeg(path: &str, bytes: &js_sys::Uint8Array) -> Result<String, NetError> {
    send(
        "POST",
        path,
        Some(bytes),
        &[("content-type", "image/jpeg"), (UPLOAD_HEADER, "1")],
        UPLOAD_DEADLINE_MS,
    )
    .await
}
