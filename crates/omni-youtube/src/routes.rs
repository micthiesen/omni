//! `GET /api/youtube/oauth/start` and `GET /api/youtube/oauth/callback`.
//!
//! Both live under `/api`, which only the LAN-only hosts serve (the public
//! gateway host forwards only `/gateway/*`). Start refuses cross-site
//! navigations (`Sec-Fetch-Site`), so another page cannot open a consent
//! screen; the callback accepts only a state Omni issued in the last ten
//! minutes, once. Pages carry no scripts or external resources, and the
//! callback URL (it holds the code) is never sent on as a referrer.

use std::collections::HashMap;

use axum::Router;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

use crate::LOG;
use crate::error::YoutubeError;
use crate::service::{CALLBACK_PATH, START_PATH, Youtube};

pub fn router(youtube: Youtube) -> Router {
    Router::new()
        .route(START_PATH, get(start))
        .route(CALLBACK_PATH, get(callback))
        .with_state(youtube)
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn page(status: StatusCode, title: &str, body: &str) -> Response {
    let html = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width\"><title>{title}</title><style>body{{font:16px/1.5 system-ui,sans-serif;background:#0b0d10;color:#e6e8eb;max-width:36rem;margin:3rem auto;padding:0 1rem}}</style></head><body><h1>{title}</h1><p>{body}</p></body></html>",
        title = escape(title),
        body = escape(body),
    );
    let mut response = (status, html).into_response();
    let headers = response.headers_mut();
    for (name, value) in [
        (header::CONTENT_TYPE, "text/html; charset=utf-8"),
        (header::CACHE_CONTROL, "no-store"),
        (header::REFERRER_POLICY, "no-referrer"),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (
            header::CONTENT_SECURITY_POLICY,
            "default-src 'none'; style-src 'unsafe-inline'; frame-ancestors 'none'",
        ),
    ] {
        headers.insert(name, HeaderValue::from_static(value));
    }
    response
}

fn failure(error: &YoutubeError) -> Response {
    let status = match error {
        YoutubeError::NotConfigured | YoutubeError::Client(_) => StatusCode::SERVICE_UNAVAILABLE,
        YoutubeError::Refused(_) | YoutubeError::Invalid(_) => StatusCode::BAD_REQUEST,
        _ => StatusCode::BAD_GATEWAY,
    };
    page(status, "YouTube was not connected", &error.to_string())
}

async fn start(State(youtube): State<Youtube>, headers: HeaderMap) -> Response {
    let site = headers
        .get("sec-fetch-site")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("none");
    if !matches!(site, "none" | "same-origin") {
        return page(
            StatusCode::FORBIDDEN,
            "Open this link directly",
            "Connecting YouTube starts only from a link opened directly in the browser.",
        );
    }
    // The state cookie must land on the callback's host, so start there.
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    if let Some(expected) = youtube.callback_host()
        && host.is_some_and(|h| !h.eq_ignore_ascii_case(expected))
    {
        return redirect(youtube.connect_url(), None);
    }
    match youtube.start_authorization().await {
        Ok(url) => {
            let state = url::Url::parse(&url).ok().and_then(|u| {
                u.query_pairs()
                    .find(|(k, _)| k == "state")
                    .map(|(_, v)| v.into_owned())
            });
            let cookie = state.map(|state| {
                format!("{STATE_COOKIE}={state}; Path=/api/youtube/oauth; Max-Age=600; HttpOnly; Secure; SameSite=Lax")
            });
            redirect(&url, cookie)
        }
        Err(error) => failure(&error),
    }
}

/// Holds the state of the authorization this browser started, so a callback
/// carrying another browser's state (someone else's Google account) is refused.
const STATE_COOKIE: &str = "omni_youtube_state";

fn redirect(location: &str, cookie: Option<String>) -> Response {
    let mut response = StatusCode::SEE_OTHER.into_response();
    let headers = response.headers_mut();
    if let Ok(location) = HeaderValue::from_str(location) {
        headers.insert(header::LOCATION, location);
    }
    if let Some(cookie) = cookie.and_then(|c| HeaderValue::from_str(&c).ok()) {
        headers.insert(header::SET_COOKIE, cookie);
    }
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn cookie_state(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|pair| pair.trim().strip_prefix(STATE_COOKIE)?.strip_prefix('='))
}

async fn callback(
    State(youtube): State<Youtube>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if let Some(error) = params.get("error") {
        let reason = if error == "access_denied" {
            "Access was declined on Google's consent screen; nothing changed.".to_owned()
        } else {
            format!("Google reported {:?}; nothing changed.", short(error))
        };
        return page(
            StatusCode::BAD_REQUEST,
            "YouTube was not connected",
            &reason,
        );
    }
    let (Some(code), Some(state)) = (params.get("code"), params.get("state")) else {
        return page(
            StatusCode::BAD_REQUEST,
            "YouTube was not connected",
            "The callback is missing its code or state.",
        );
    };
    if params
        .get("iss")
        .is_some_and(|iss| iss != "https://accounts.google.com")
    {
        return page(
            StatusCode::BAD_REQUEST,
            "YouTube was not connected",
            "The callback names an issuer other than Google.",
        );
    }
    if cookie_state(&headers) != Some(state.as_str()) {
        return page(
            StatusCode::BAD_REQUEST,
            "YouTube was not connected",
            "This sign-in was not started in this browser; start again from the connect link.",
        );
    }
    let mut response = match youtube.complete_authorization(code, state).await {
        Ok(channel) => {
            let who = channel.title.map_or_else(
                || "the Google account".to_owned(),
                |t| format!("the channel {t}"),
            );
            page(
                StatusCode::OK,
                "YouTube connected",
                &format!("Omni can now manage playlists of {who}. You can close this tab."),
            )
        }
        Err(error) => {
            tracing::warn!(target: LOG, "YouTube connection failed: {error}");
            failure(&error)
        }
    };
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_static(
            "omni_youtube_state=; Path=/api/youtube/oauth; Max-Age=0; HttpOnly; Secure; SameSite=Lax",
        ),
    );
    response
}

fn short(text: &str) -> String {
    crate::wire::cut(text, 80).0
}
