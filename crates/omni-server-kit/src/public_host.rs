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
    Some(host_name(raw))
}

/// A `Host` value or URI authority as a bare name: lowercased, without port
/// or trailing dot.
pub fn host_name(raw: &str) -> String {
    let name = match raw.rsplit_once(':') {
        Some((name, port)) if !name.ends_with(']') && port.bytes().all(|b| b.is_ascii_digit()) => {
            name
        }
        _ => raw,
    };
    name.trim_end_matches('.').to_ascii_lowercase()
}

/// Every host a request names: each `Host` header and, for an absolute-form
/// or HTTP/2 request, the URI authority.
pub fn named_hosts(request: &Request) -> Vec<String> {
    let mut hosts: Vec<String> = request
        .headers()
        .get_all(header::HOST)
        .iter()
        .map(|value| host_name(value.to_str().unwrap_or("")))
        .collect();
    hosts.extend(request.uri().authority().map(|a| host_name(a.as_str())));
    hosts
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

/// The host names Omni answers to. Anything else is refused, so a page on
/// another site that rebinds its DNS name to Omni's address (DNS rebinding)
/// cannot reach the API with the visitor's browser: its requests carry that
/// site's name.
#[derive(Clone, Default)]
pub struct KnownHosts {
    names: Arc<std::collections::HashSet<String>>,
}

impl KnownHosts {
    /// Configured names (public hosts, app hosts, the UI's own URL) as URLs
    /// or bare names; unparsable entries are skipped.
    pub fn new<'a>(urls: impl IntoIterator<Item = &'a str>) -> Self {
        let names = urls
            .into_iter()
            .filter_map(|raw| {
                let raw = raw.trim();
                if raw.is_empty() {
                    return None;
                }
                match url::Url::parse(raw) {
                    Ok(url) => url.host_str().map(str::to_ascii_lowercase),
                    Err(_) => Some(host_name(raw)),
                }
            })
            .collect();
        Self {
            names: Arc::new(names),
        }
    }

    /// Home-network names are always known: IP literals, `localhost`,
    /// single-label names (Docker service names) and `.boris`, `.local`,
    /// `.internal` and `.lan` names. A rebinding page needs a real domain.
    pub fn allows(&self, raw: &str) -> bool {
        let name = bare_name(raw);
        if name.is_empty() || name.parse::<std::net::IpAddr>().is_ok() {
            return true;
        }
        if !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
        {
            return false;
        }
        if name == "localhost" || !name.contains('.') {
            return true;
        }
        [".boris", ".local", ".internal", ".lan"]
            .iter()
            .any(|suffix| name.ends_with(suffix))
            || self.names.contains(&name)
    }
}

/// A `Host` value without port, brackets, case or trailing dot.
fn bare_name(raw: &str) -> String {
    let raw = raw.trim();
    if let Some(rest) = raw.strip_prefix('[') {
        return rest
            .split(']')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
    }
    let name = match raw.rsplit_once(':') {
        Some((name, port)) if port.bytes().all(|b| b.is_ascii_digit()) => name,
        _ => raw,
    };
    name.trim_end_matches('.').to_ascii_lowercase()
}

/// Middleware over the whole application: 421 for a request naming a host
/// Omni does not answer to.
pub async fn only_known_hosts(
    State(known): State<KnownHosts>,
    request: Request,
    next: Next,
) -> Response {
    let mut named = request
        .headers()
        .get_all(header::HOST)
        .iter()
        .map(|value| value.to_str().unwrap_or("\u{0}").to_owned())
        .chain(request.uri().authority().map(|a| a.as_str().to_owned()));
    if named.any(|host| !known.allows(&host)) {
        return crate::api_error(
            StatusCode::MISDIRECTED_REQUEST,
            "Omni does not answer to this host name",
        );
    }
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_home_network_and_configured_hosts_are_known() {
        let known = KnownHosts::new([
            "https://gateway.example.com",
            "https://pods.example.com/pods",
            "omni.example.com",
            "",
        ]);
        for host in [
            "omni.boris",
            "10.10.1.100:8080",
            "[::1]:80",
            "localhost:38080",
            "omni:8080",
            "mac.local",
            "Gateway.Example.com",
            "pods.example.com:443",
            "omni.example.com.",
            "",
        ] {
            assert!(known.allows(host), "{host}");
        }
        for host in [
            "evil.example",
            "rebound.example.com",
            "omni.boris.evil.example",
            "example.com",
            "\u{0}",
        ] {
            assert!(!known.allows(host), "{host}");
        }
    }

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
