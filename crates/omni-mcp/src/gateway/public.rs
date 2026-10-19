//! The public gateway host serves the gateway's public paths and nothing
//! else.
//!
//! The proxy host for `OMNI_GATEWAY_PUBLIC_URL` forwards only these exact
//! paths (`deploy/gateway/route.mjs` checks the raw request URI against the
//! same list). [`omni_server_kit::public_host`] enforces it again inside Omni: a request
//! that names the public host gets a 404 for any other path, including the
//! management page, `/api/*`, `/mcp`, the SPA fallback and encoded, dotted,
//! doubled-slash or trailing-slash variants of an allowed path.

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

/// The guard for the host of `OMNI_GATEWAY_PUBLIC_URL`, when it is set.
pub fn public_host(public_url: Option<&str>) -> Option<omni_server_kit::public_host::PublicHost> {
    omni_server_kit::public_host::PublicHost::new(public_url, is_public_path)
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
}
