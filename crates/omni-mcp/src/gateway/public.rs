//! The public gateway host serves the gateway's public paths and nothing
//! else.
//!
//! The proxy host for `OMNI_GATEWAY_PUBLIC_URL` forwards only these exact
//! paths (`deploy/gateway/route.mjs` checks the raw request URI against the
//! same list). [`only_public_paths`] enforces it again inside Omni: a request
//! that names the public host gets a 404 for any other path, including the
//! management page, `/api/*`, `/mcp`, the SPA fallback and encoded, dotted,
//! doubled-slash or trailing-slash variants of an allowed path.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;

/// Every path the public host serves, compared exactly with the raw path.
pub const PUBLIC_PATHS: [&str; 10] = [
    "/gateway/mcp",
    "/gateway/oauth/authorize",
    "/gateway/oauth/authorize/status",
    "/gateway/oauth/authorize/complete",
    "/gateway/oauth/token",
    "/gateway/oauth/register",
    "/gateway/oauth/callback",
    "/.well-known/oauth-authorization-server",
    "/.well-known/oauth-protected-resource",
    "/.well-known/oauth-protected-resource/gateway/mcp",
];

pub fn is_public_path(path: &str) -> bool {
    PUBLIC_PATHS.contains(&path)
}

/// The host of `OMNI_GATEWAY_PUBLIC_URL`, lowercased, when it is set.
pub fn public_host(public_url: Option<&str>) -> Option<Arc<str>> {
    let url = url::Url::parse(public_url?).ok()?;
    Some(Arc::from(url.host_str()?.to_ascii_lowercase()))
}

/// The host a request names (`Host`, or the HTTP/2 authority), lowercased,
/// without port or trailing dot.
fn request_host(request: &Request) -> Option<String> {
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

/// Middleware over the whole application: requests naming the public host
/// reach only [`PUBLIC_PATHS`].
pub async fn only_public_paths(
    State(public): State<Arc<str>>,
    request: Request,
    next: Next,
) -> Response {
    if request_host(&request).as_deref() == Some(&*public) && !is_public_path(request.uri().path())
    {
        return omni_server_kit::api_error(StatusCode::NOT_FOUND, "Not found");
    }
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_paths_are_the_routes_the_public_side_needs() {
        use super::super::{connect, endpoint, oauth::server};
        for path in [
            endpoint::PATH,
            server::AUTHORIZE_PATH,
            server::STATUS_PATH,
            server::COMPLETE_PATH,
            server::TOKEN_PATH,
            server::REGISTER_PATH,
            connect::CALLBACK_PATH,
        ] {
            assert!(is_public_path(path), "{path}");
        }
        for path in [
            "/gateway",
            "/gateway/",
            "/gateway/mcp/",
            "/gateway//mcp",
            "/gateway/%6Dcp",
            "/gateway/x/../mcp",
            "/GATEWAY/mcp",
            "/api/gateway",
            "/mcp",
            "/",
        ] {
            assert!(!is_public_path(path), "{path}");
        }
    }

    /// `deploy/gateway/route.mjs` lists the same paths for the proxy.
    #[test]
    fn the_proxy_route_allows_the_same_paths() {
        let script = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/gateway/route.mjs"),
        )
        .unwrap();
        let start = script.find("const publicPaths = [").unwrap();
        let end = start + script[start..].find("];").unwrap();
        let listed: Vec<&str> = script[start..end]
            .lines()
            .skip(1)
            .filter_map(|line| line.trim().strip_prefix('"')?.strip_suffix("\","))
            .collect();
        assert_eq!(listed, PUBLIC_PATHS);
    }

    #[test]
    fn the_public_host_is_compared_without_port_case_or_trailing_dot() {
        let request = |host: &str| {
            Request::builder()
                .uri("/gateway/mcp")
                .header(header::HOST, host)
                .body(axum::body::Body::empty())
                .unwrap()
        };
        for host in [
            "gateway.example.com",
            "Gateway.Example.com:443",
            "gateway.example.com.",
        ] {
            assert_eq!(
                request_host(&request(host)).as_deref(),
                Some("gateway.example.com")
            );
        }
        assert_eq!(
            public_host(Some("https://Gateway.Example.com")).as_deref(),
            Some("gateway.example.com")
        );
        assert_eq!(public_host(None), None);
    }
}
