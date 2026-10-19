//! Public proxy hosts reach only the paths they exist for.
//!
//! Omni serves the LAN and, through Nginx Proxy Manager, a few public hosts
//! (`OMNI_GATEWAY_PUBLIC_URL`, `PRESSPODS_PUBLIC_URL`). NPM forwards the raw
//! request URI, so its path rules are one layer; [`only_allowed_paths`] is the
//! other: a request whose `Host` is a public host gets a JSON 404 for any raw
//! path its allowlist does not accept, including the UI, `/api/*`, the SPA
//! fallback and encoded, dotted, doubled-slash or trailing-slash variants.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;

/// One public host and the raw paths it may reach.
#[derive(Clone)]
pub struct PublicHost {
    host: Arc<str>,
    allows: fn(&str) -> bool,
}

impl PublicHost {
    /// `None` when `public_url` is unset or has no host.
    pub fn new(public_url: Option<&str>, allows: fn(&str) -> bool) -> Option<Self> {
        let url = url::Url::parse(public_url?).ok()?;
        Some(Self {
            host: Arc::from(url.host_str()?.to_ascii_lowercase()),
            allows,
        })
    }
}

/// The host a request names (`Host`, or the HTTP/2 authority), lowercased,
/// without port or trailing dot.
pub fn request_host(request: &Request) -> Option<String> {
    let raw = request
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .or_else(|| request.uri().authority().map(|a| a.as_str()))?;
    let name = match raw.rsplit_once(':') {
        Some((name, port)) if !name.ends_with(']') && port.bytes().all(|b| b.is_ascii_digit()) => {
            name
        }
        _ => raw,
    };
    Some(name.trim_end_matches('.').to_ascii_lowercase())
}

/// Middleware over the whole application for one [`PublicHost`].
pub async fn only_allowed_paths(
    State(public): State<PublicHost>,
    request: Request,
    next: Next,
) -> Response {
    if request_host(&request).as_deref() == Some(&*public.host)
        && !(public.allows)(request.uri().path())
    {
        return crate::api_error(StatusCode::NOT_FOUND, "Not found");
    }
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_host_is_compared_without_port_case_or_trailing_dot() {
        let request = |host: &str| {
            Request::builder()
                .uri("/x")
                .header(header::HOST, host)
                .body(axum::body::Body::empty())
                .unwrap()
        };
        for host in [
            "pods.example.com",
            "Pods.Example.com:443",
            "pods.example.com.",
        ] {
            assert_eq!(
                request_host(&request(host)).as_deref(),
                Some("pods.example.com")
            );
        }
        let public = PublicHost::new(Some("https://Pods.Example.com"), |_| true).unwrap();
        assert_eq!(&*public.host, "pods.example.com");
        assert!(PublicHost::new(None, |_| true).is_none());
        assert!(PublicHost::new(Some(""), |_| true).is_none());
    }
}
