//! Events from other MCP servers (`EVENT_UPSTREAMS`), offered alongside Omni's.
//!
//! Each upstream (an artifacts.boo workspace, say) implements MCP Events itself:
//! it checks the subscriber's callback with its own challenge, signs and retries
//! deliveries, and keeps its own log. The adapter only lists those events, with a
//! `workspace` argument naming the upstream, and routes `events/subscribe` and
//! `events/unsubscribe` for them to it, with that upstream's own token instead of
//! the caller's Executor session (which has already been checked). Nothing here
//! polls: an upstream is only called when a client lists or subscribes.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use serde_json::{Map, Value, json};
use tokio::sync::Mutex;

use crate::config::{EventUpstream, USER_AGENT};

/// The argument that picks the upstream.
pub const WORKSPACE_ARG: &str = "workspace";
/// How long a fetched upstream catalog is reused.
const CATALOG_TTL: Duration = Duration::from_secs(300);
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_UPSTREAM_RESPONSE_BYTES: usize = 1024 * 1024;

/// The configured upstreams and their cached event catalog.
#[derive(Clone)]
pub struct Upstreams {
    list: Arc<Vec<EventUpstream>>,
    http: reqwest::Client,
    cache: Arc<Mutex<Option<(Instant, Catalog)>>>,
}

/// The upstreams' events, merged: one definition per event name, and which upstreams offer it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Catalog {
    routes: BTreeMap<String, Vec<String>>,
    events: Vec<Value>,
}

/// Where an upstream event's subscription goes, and its arguments without `workspace`.
#[derive(Debug, PartialEq)]
pub struct Route {
    pub key: String,
    pub arguments: Map<String, Value>,
}

impl Upstreams {
    pub fn new(list: Vec<EventUpstream>, http: reqwest::Client) -> Self {
        Self {
            list: Arc::new(list),
            http,
            cache: Arc::new(Mutex::new(None)),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    /// The upstreams' events, fetched at most every few minutes. An upstream that can't be
    /// reached is left out of this catalog (and tried again next time), never failing the list.
    pub async fn catalog(&self) -> Catalog {
        let mut cache = self.cache.lock().await;
        if let Some((at, catalog)) = cache.as_ref()
            && at.elapsed() < CATALOG_TTL
        {
            return catalog.clone();
        }
        let mut per = Vec::with_capacity(self.list.len());
        for upstream in self.list.iter() {
            match self.call(upstream, "events/list", Map::new()).await {
                Ok(rpc) => match rpc.get("result").and_then(|r| r.get("events")) {
                    Some(Value::Array(events)) => per.push((upstream.key.clone(), events.clone())),
                    _ => {
                        tracing::warn!(upstream = %upstream.key, "upstream events/list had no events")
                    }
                },
                Err(error) => {
                    tracing::warn!(upstream = %upstream.key, %error, "upstream events/list failed")
                }
            }
        }
        let catalog = merge(per);
        *cache = Some((Instant::now(), catalog.clone()));
        catalog
    }

    pub fn get(&self, key: &str) -> Option<&EventUpstream> {
        self.list.iter().find(|u| u.key == key)
    }

    /// One JSON-RPC call to an upstream with its token; the whole response object (result or error).
    pub async fn call(
        &self,
        upstream: &EventUpstream,
        method: &str,
        params: Map<String, Value>,
    ) -> Result<Map<String, Value>, String> {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let response = self
            .http
            .post(upstream.url.clone())
            .bearer_auth(&upstream.token)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(
                reqwest::header::ACCEPT,
                "application/json, text/event-stream",
            )
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .json(&body)
            .timeout(UPSTREAM_TIMEOUT)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = response.status();
        if status.is_redirection()
            || status == reqwest::StatusCode::UNAUTHORIZED
            || status == reqwest::StatusCode::FORBIDDEN
        {
            return Err(format!("upstream answered {status}"));
        }
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| e.to_string())?;
            if bytes.len() + chunk.len() > MAX_UPSTREAM_RESPONSE_BYTES {
                return Err("upstream response too large".into());
            }
            bytes.extend_from_slice(&chunk);
        }
        match serde_json::from_slice::<Value>(&bytes) {
            Ok(Value::Object(rpc)) if rpc.contains_key("result") || rpc.contains_key("error") => {
                Ok(rpc)
            }
            _ => Err(format!(
                "upstream answered {status} without a JSON-RPC response"
            )),
        }
    }
}

/// One definition per event name (the first upstream's), with a `workspace` argument listing every
/// upstream that offers it: required when there are several, the only one otherwise.
pub fn merge(per: Vec<(String, Vec<Value>)>) -> Catalog {
    let mut routes: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut definitions: BTreeMap<String, Value> = BTreeMap::new();
    for (key, events) in per {
        for event in events {
            let Some(name) = event.get("name").and_then(Value::as_str).map(str::to_owned) else {
                continue;
            };
            routes.entry(name.clone()).or_default().push(key.clone());
            definitions.entry(name).or_insert(event);
        }
    }
    let events = definitions
        .into_iter()
        .map(|(name, mut event)| {
            let keys = routes.get(&name).cloned().unwrap_or_default();
            if let Value::Object(e) = &mut event {
                let description = e.get("description").and_then(Value::as_str).unwrap_or("").to_owned();
                e.insert(
                    "description".into(),
                    Value::String(format!("{description} (From {}; pick one with the `{WORKSPACE_ARG}` argument.)", keys.join(", "))),
                );
                let schema = e.entry("inputSchema").or_insert_with(|| json!({"type": "object"}));
                if let Value::Object(schema) = schema {
                    let properties = schema.entry("properties").or_insert_with(|| json!({}));
                    if let Value::Object(properties) = properties {
                        properties.insert(
                            WORKSPACE_ARG.into(),
                            json!({"type": "string", "enum": keys, "description": "Which workspace's events."}),
                        );
                    }
                    if keys.len() > 1 {
                        let required = schema.entry("required").or_insert_with(|| json!([]));
                        if let Value::Array(required) = required {
                            required.push(Value::String(WORKSPACE_ARG.into()));
                        }
                    }
                }
            }
            event
        })
        .collect();
    Catalog { routes, events }
}

impl Catalog {
    /// The merged definitions, except names in `taken` (Omni's own events win a clash).
    pub fn events_except(&self, taken: &[String]) -> Vec<Value> {
        self.events
            .iter()
            .filter(|e| {
                e.get("name")
                    .and_then(Value::as_str)
                    .is_none_or(|n| !taken.iter().any(|t| t == n))
            })
            .cloned()
            .collect()
    }

    /// Where a subscription to `name` goes: none for an event that isn't an upstream's (Omni's).
    pub fn route(
        &self,
        name: &str,
        arguments: &Map<String, Value>,
    ) -> Option<Result<Route, String>> {
        let keys = self.routes.get(name)?;
        let chosen = match arguments.get(WORKSPACE_ARG) {
            Some(Value::String(key)) => key.clone(),
            None if keys.len() == 1 => keys[0].clone(),
            None => {
                return Some(Err(format!(
                    "`{WORKSPACE_ARG}` is required: one of {}",
                    keys.join(", ")
                )));
            }
            Some(_) => return Some(Err(format!("`{WORKSPACE_ARG}` must be a string"))),
        };
        if !keys.contains(&chosen) {
            return Some(Err(format!(
                "no workspace `{chosen}` offers {name}: one of {}",
                keys.join(", ")
            )));
        }
        let mut stripped = arguments.clone();
        stripped.remove(WORKSPACE_ARG);
        Some(Ok(Route {
            key: chosen,
            arguments: stripped,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(name: &str) -> Value {
        json!({"name": name, "description": "Something happened.", "inputSchema": {"type": "object", "properties": {"artifact": {"type": "string"}}, "additionalProperties": false}})
    }

    #[test]
    fn merged_events_carry_the_workspaces_that_offer_them() {
        let catalog = merge(vec![
            (
                "michael".into(),
                vec![event("comment.created"), event("artifact.answered")],
            ),
            ("support".into(), vec![event("comment.created")]),
        ]);
        let events = catalog.events_except(&[]);
        let comment = events
            .iter()
            .find(|e| e["name"] == "comment.created")
            .unwrap();
        assert_eq!(
            comment["inputSchema"]["properties"]["workspace"]["enum"],
            json!(["michael", "support"])
        );
        assert_eq!(comment["inputSchema"]["required"], json!(["workspace"]));
        let answered = events
            .iter()
            .find(|e| e["name"] == "artifact.answered")
            .unwrap();
        assert_eq!(
            answered["inputSchema"]["properties"]["workspace"]["enum"],
            json!(["michael"])
        );
        assert!(answered["inputSchema"].get("required").is_none());
        assert!(
            catalog
                .events_except(&["comment.created".into()])
                .iter()
                .all(|e| e["name"] != "comment.created")
        );
    }

    #[test]
    fn routing_picks_the_workspace_and_strips_it() {
        let catalog = merge(vec![
            ("michael".into(), vec![event("comment.created")]),
            ("support".into(), vec![event("comment.created")]),
        ]);
        let args = |v: Value| v.as_object().cloned().unwrap();
        assert_eq!(
            catalog.route(
                "comment.created",
                &args(json!({"workspace": "support", "artifact": "a1"}))
            ),
            Some(Ok(Route {
                key: "support".into(),
                arguments: args(json!({"artifact": "a1"}))
            }))
        );
        assert!(matches!(
            catalog.route("comment.created", &args(json!({}))),
            Some(Err(_))
        ));
        assert!(matches!(
            catalog.route("comment.created", &args(json!({"workspace": "nope"}))),
            Some(Err(_))
        ));
        assert_eq!(catalog.route("email.received", &args(json!({}))), None);
        let single = merge(vec![("michael".into(), vec![event("artifact.answered")])]);
        assert_eq!(
            single
                .route("artifact.answered", &args(json!({})))
                .unwrap()
                .unwrap()
                .key,
            "michael"
        );
    }
}
