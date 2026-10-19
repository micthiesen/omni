//! Adapter configuration from the environment.

use std::fmt;

use url::Url;

/// The modern MCP protocol version this adapter serves.
pub const PROTOCOL_VERSION: &str = "2026-07-28";
/// User agent for every outgoing request the adapter originates or forwards.
pub const USER_AGENT: &str = "OpenAI File Downloader, XaiImageApiFetch/1.0";
/// Listen port when `PORT` is unset.
pub const DEFAULT_PORT: u16 = 4789;

const DEFAULT_EXECUTOR_BASE_URL: &str = "http://executor:4788";
const DEFAULT_OMNI_BASE_URL: &str = "http://omni:8080";

/// Validated adapter options. `Debug` redacts the Omni token.
#[derive(Clone)]
pub struct AdapterOptions {
    pub executor_base_url: Url,
    pub omni_base_url: Url,
    pub omni_mcp_token: String,
    pub allowed_user_id: String,
    pub public_mcp_origin: Url,
    /// Other MCP servers whose events this adapter offers alongside Omni's (see `upstreams`).
    pub event_upstreams: Vec<EventUpstream>,
}

/// An MCP server with its own events (an artifacts.boo workspace, say), reached with its own token.
/// Its events are listed with a `workspace` argument naming `key`, and subscriptions to them go
/// straight to `url`, which then delivers to the subscriber's callback itself.
#[derive(Clone)]
pub struct EventUpstream {
    pub key: String,
    pub url: Url,
    pub token: String,
}

impl fmt::Debug for EventUpstream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventUpstream")
            .field("key", &self.key)
            .field("url", &self.url.as_str())
            .field("token", &"<redacted>")
            .finish()
    }
}

impl fmt::Debug for AdapterOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AdapterOptions")
            .field("executor_base_url", &self.executor_base_url.as_str())
            .field("omni_base_url", &self.omni_base_url.as_str())
            .field("omni_mcp_token", &"<redacted>")
            .field("allowed_user_id", &self.allowed_user_id)
            .field("public_mcp_origin", &self.public_mcp_origin.as_str())
            .field("event_upstreams", &self.event_upstreams)
            .finish()
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error("Missing adapter configuration: {0}")]
    Missing(&'static str),
    #[error("Invalid URL in {0}")]
    InvalidUrl(&'static str),
    #[error("Invalid PORT")]
    InvalidPort,
    #[error("Invalid EVENT_UPSTREAMS: {0}")]
    InvalidUpstreams(String),
}

/// Everything the binary needs to start.
#[derive(Debug, Clone)]
pub struct EnvConfig {
    pub options: AdapterOptions,
    pub port: u16,
}

impl AdapterOptions {
    /// Builds options from raw strings, rejecting empty or unparsable values so
    /// a bad configuration fails boot rather than each request.
    pub fn new(
        executor_base_url: &str,
        omni_base_url: &str,
        omni_mcp_token: &str,
        allowed_user_id: &str,
        public_mcp_origin: &str,
    ) -> Result<Self, ConfigError> {
        let required = |value: &str, name: &'static str| {
            if value.is_empty() {
                Err(ConfigError::Missing(name))
            } else {
                Ok(value.to_owned())
            }
        };
        let url = |value: &str, name: &'static str| {
            if value.is_empty() {
                return Err(ConfigError::Missing(name));
            }
            Url::parse(value).map_err(|_| ConfigError::InvalidUrl(name))
        };
        Ok(Self {
            executor_base_url: url(executor_base_url, "EXECUTOR_BASE_URL")?,
            omni_base_url: url(omni_base_url, "OMNI_BASE_URL")?,
            omni_mcp_token: required(omni_mcp_token, "OMNI_MCP_TOKEN")?,
            allowed_user_id: required(allowed_user_id, "EXECUTOR_ALLOWED_USER_ID")?,
            public_mcp_origin: url(public_mcp_origin, "PUBLIC_MCP_ORIGIN")?,
            event_upstreams: Vec::new(),
        })
    }
}

/// `EVENT_UPSTREAMS`: a JSON array of `{key, url, token}`. Keys are short lowercase names, unique;
/// URLs are HTTPS (HTTP only to a loopback host, for tests).
pub fn upstreams_from(raw: Option<String>) -> Result<Vec<EventUpstream>, ConfigError> {
    let Some(raw) = raw.filter(|r| !r.trim().is_empty()) else {
        return Ok(Vec::new());
    };
    let invalid = |m: &str| ConfigError::InvalidUpstreams(m.to_owned());
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Raw {
        key: String,
        url: String,
        token: String,
    }
    let raws: Vec<Raw> =
        serde_json::from_str(&raw).map_err(|_| invalid("not a JSON array of {key, url, token}"))?;
    let mut list = Vec::with_capacity(raws.len());
    for r in raws {
        let url = Url::parse(&r.url).map_err(|_| invalid("an upstream URL doesn't parse"))?;
        list.push(EventUpstream {
            key: r.key,
            url,
            token: r.token,
        });
    }
    let mut keys = std::collections::HashSet::new();
    for u in &list {
        let key_ok = !u.key.is_empty()
            && u.key.len() <= 32
            && u.key
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if !key_ok {
            return Err(invalid(
                "a key is 1-32 lowercase letters, digits and hyphens",
            ));
        }
        if !keys.insert(u.key.clone()) {
            return Err(invalid("keys must be unique"));
        }
        let loopback = matches!(u.url.host_str(), Some("127.0.0.1" | "localhost"));
        if u.url.scheme() != "https" && !(u.url.scheme() == "http" && loopback) {
            return Err(invalid("upstream URLs must be HTTPS"));
        }
        if u.token.is_empty() {
            return Err(invalid("every upstream needs a token"));
        }
    }
    Ok(list)
}

impl EnvConfig {
    /// Reads the adapter's environment variables, with their defaults.
    pub fn from_env(get: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let or = |key: &str, default: &str| get(key).unwrap_or_else(|| default.to_owned());
        let options = AdapterOptions::new(
            &or("EXECUTOR_BASE_URL", DEFAULT_EXECUTOR_BASE_URL),
            &or("OMNI_BASE_URL", DEFAULT_OMNI_BASE_URL),
            &or("OMNI_MCP_TOKEN", ""),
            &or("EXECUTOR_ALLOWED_USER_ID", ""),
            &or("PUBLIC_MCP_ORIGIN", ""),
        )?;
        let mut options = options;
        options.event_upstreams = upstreams_from(get("EVENT_UPSTREAMS"))?;
        let port = port_from(get("PORT"))?;
        Ok(Self { options, port })
    }
}

/// `PORT`, with its default.
pub fn port_from(value: Option<String>) -> Result<u16, ConfigError> {
    match value {
        None => Ok(DEFAULT_PORT),
        Some(raw) => raw.trim().parse().map_err(|_| ConfigError::InvalidPort),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |key| map.get(key).cloned()
    }

    #[test]
    fn defaults_are_applied() {
        let config = EnvConfig::from_env(env(&[
            ("OMNI_MCP_TOKEN", "t"),
            ("EXECUTOR_ALLOWED_USER_ID", "u"),
            ("PUBLIC_MCP_ORIGIN", "https://mcp.syas.ca"),
        ]))
        .unwrap();
        assert_eq!(config.port, 4789);
        assert_eq!(
            config.options.executor_base_url.as_str(),
            "http://executor:4788/"
        );
        assert_eq!(config.options.omni_base_url.as_str(), "http://omni:8080/");
        assert!(!format!("{:?}", config.options).contains("\"t\""));
    }

    #[test]
    fn missing_required_values_fail_boot() {
        let missing = EnvConfig::from_env(env(&[("EXECUTOR_ALLOWED_USER_ID", "u")]));
        assert_eq!(missing.unwrap_err(), ConfigError::Missing("OMNI_MCP_TOKEN"));
        let bad_url = EnvConfig::from_env(env(&[
            ("OMNI_MCP_TOKEN", "t"),
            ("EXECUTOR_ALLOWED_USER_ID", "u"),
            ("PUBLIC_MCP_ORIGIN", "not a url"),
        ]));
        assert_eq!(
            bad_url.unwrap_err(),
            ConfigError::InvalidUrl("PUBLIC_MCP_ORIGIN")
        );
        assert_eq!(port_from(Some("x".into())), Err(ConfigError::InvalidPort));
    }

    #[test]
    fn event_upstreams_parse_and_reject_bad_entries() {
        let ok = upstreams_from(Some(
            r#"[{"key":"michael","url":"https://michael.artifacts.boo/mcp","token":"t"}]"#.into(),
        ))
        .unwrap();
        assert_eq!(ok.len(), 1);
        assert_eq!(ok[0].key, "michael");
        assert!(!format!("{ok:?}").contains("\"t\""));
        assert!(upstreams_from(None).unwrap().is_empty());
        assert!(upstreams_from(Some("  ".into())).unwrap().is_empty());
        for bad in [
            r#"{"key":"x"}"#,
            r#"[{"key":"Bad Key","url":"https://a.test/mcp","token":"t"}]"#,
            r#"[{"key":"a","url":"http://a.test/mcp","token":"t"}]"#,
            r#"[{"key":"a","url":"https://a.test/mcp","token":""}]"#,
            r#"[{"key":"a","url":"https://a.test/mcp","token":"t"},{"key":"a","url":"https://b.test/mcp","token":"t"}]"#,
            r#"[{"key":"a","url":"https://a.test/mcp","token":"t","extra":1}]"#,
        ] {
            assert!(upstreams_from(Some(bad.into())).is_err(), "{bad}");
        }
    }
}
