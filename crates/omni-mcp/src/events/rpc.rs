//! The `events/*` JSON-RPC methods.
//!
//! Params are validated as strict objects, with stable issue wording, before
//! the delegated-principal headers are read; service errors map to fixed
//! JSON-RPC codes: `-32001` for an invalid principal, `-32015`
//! `CallbackEndpointError` for callback failures, `-32602` otherwise.

use axum::http::HeaderMap;
use serde_json::{Map, Value, json};

use super::catalog::event_catalog;
use super::executor_auth::{is_bearer_authorization, is_executor_owner};
use super::service::{
    EventPrincipal, EventServiceError, McpEventService, SubscribeInput, SubscriptionRejection,
    UnsubscribeInput,
};
use crate::json::received_type;
use crate::rpc::RpcError;

const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

pub const EVENT_METHODS: [&str; 3] = ["events/list", "events/subscribe", "events/unsubscribe"];

/// One validation issue: dotted path and message.
struct Issue {
    path: Vec<String>,
    message: String,
}

#[derive(Default)]
struct Issues(Vec<Issue>);

impl Issues {
    fn push(&mut self, path: &[&str], message: String) {
        self.0.push(Issue {
            path: path.iter().map(|p| (*p).to_owned()).collect(),
            message,
        });
    }

    fn expected(&mut self, path: &[&str], expected: &str, value: Option<&Value>) {
        self.push(
            path,
            format!(
                "Invalid input: expected {expected}, received {}",
                received_type(value)
            ),
        );
    }

    fn string(&mut self, object: &Map<String, Value>, key: &str, path: &[&str]) {
        let value = object.get(key);
        if !matches!(value, Some(Value::String(_))) {
            self.expected(path, "string", value);
        }
    }

    /// `.strict()`: unknown keys are one issue at the object's path.
    fn strict(&mut self, object: &Map<String, Value>, known: &[&str], path: &[&str]) {
        let unknown: Vec<String> = object
            .keys()
            .filter(|key| !known.contains(&key.as_str()))
            .map(|key| format!("\"{key}\""))
            .collect();
        match unknown.len() {
            0 => {}
            1 => self.push(path, format!("Unrecognized key: {}", unknown[0])),
            _ => self.push(path, format!("Unrecognized keys: {}", unknown.join(", "))),
        }
    }

    fn into_result(self, method: &str) -> Result<(), RpcError> {
        if self.0.is_empty() {
            return Ok(());
        }
        let text = self
            .0
            .iter()
            .map(|issue| {
                if issue.path.is_empty() {
                    issue.message.clone()
                } else {
                    format!("{}: {}", issue.path.join("."), issue.message)
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        Err(RpcError::invalid_params(format!(
            "Invalid params for {method}: {text}"
        )))
    }
}

fn check_arguments(issues: &mut Issues, params: &Map<String, Value>) {
    match params.get("arguments") {
        None | Some(Value::Object(_)) => {}
        other => issues.expected(&["arguments"], "record", other),
    }
}

fn check_delivery(issues: &mut Issues, params: &Map<String, Value>, with_secret: bool) {
    let delivery = params.get("delivery");
    let Some(Value::Object(delivery)) = delivery else {
        issues.expected(&["delivery"], "object", delivery);
        return;
    };
    if delivery.get("mode") != Some(&Value::String("webhook".to_owned())) {
        issues.push(
            &["delivery", "mode"],
            "Invalid input: expected \"webhook\"".to_owned(),
        );
    }
    match delivery.get("url") {
        Some(Value::String(url)) if url::Url::parse(url).is_ok() => {}
        Some(Value::String(_)) => issues.push(&["delivery", "url"], "Invalid URL".to_owned()),
        other => issues.expected(&["delivery", "url"], "string", other),
    }
    if with_secret {
        issues.string(delivery, "secret", &["delivery", "secret"]);
        issues.strict(delivery, &["mode", "url", "secret"], &["delivery"]);
    } else {
        issues.strict(delivery, &["mode", "url"], &["delivery"]);
    }
}

pub(crate) fn subscribe_input(params: &Map<String, Value>) -> Result<SubscribeInput, RpcError> {
    let mut issues = Issues::default();
    issues.string(params, "name", &["name"]);
    check_arguments(&mut issues, params);
    check_delivery(&mut issues, params, true);
    let ttl = params.get("ttlMs");
    match ttl {
        None | Some(Value::Null) => {}
        // An integer check aborts on a fraction and bounds integers to the
        // safe range; the positivity check still runs after a range issue.
        Some(Value::Number(n)) => match n.as_f64() {
            Some(x) if x.fract() != 0.0 => {
                issues.push(
                    &["ttlMs"],
                    "Invalid input: expected int, received number".to_owned(),
                );
            }
            Some(x) => {
                if x > MAX_SAFE_INTEGER {
                    issues.push(
                        &["ttlMs"],
                        "Too big: expected int to be <=9007199254740991".to_owned(),
                    );
                } else if x < -MAX_SAFE_INTEGER {
                    issues.push(
                        &["ttlMs"],
                        "Too small: expected int to be >=-9007199254740991".to_owned(),
                    );
                }
                if x <= 0.0 {
                    issues.push(&["ttlMs"], "Too small: expected number to be >0".to_owned());
                }
            }
            None => {}
        },
        other => issues.expected(&["ttlMs"], "number", other),
    }
    match params.get("cursor") {
        None | Some(Value::Null | Value::String(_)) => {}
        other => issues.expected(&["cursor"], "string", other),
    }
    issues.strict(
        params,
        &["name", "arguments", "delivery", "ttlMs", "cursor"],
        &[],
    );
    issues.into_result("events/subscribe")?;
    let delivery = params.get("delivery").and_then(Value::as_object);
    let text = |key: &str| {
        delivery
            .and_then(|d| d.get(key))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    #[allow(clippy::cast_possible_truncation)]
    Ok(SubscribeInput {
        name: params
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        arguments: params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({})),
        url: text("url"),
        secret: text("secret"),
        ttl_ms: ttl.and_then(Value::as_f64).map(|x| x.min(9.0e15) as i64),
    })
}

pub(crate) fn unsubscribe_input(params: &Map<String, Value>) -> Result<UnsubscribeInput, RpcError> {
    let mut issues = Issues::default();
    issues.string(params, "name", &["name"]);
    check_arguments(&mut issues, params);
    check_delivery(&mut issues, params, false);
    issues.strict(params, &["name", "arguments", "delivery"], &[]);
    issues.into_result("events/unsubscribe")?;
    Ok(UnsubscribeInput {
        name: params
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        arguments: params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({})),
        url: params
            .get("delivery")
            .and_then(|d| d.get("url"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    })
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// Both delegated headers or neither.
fn principal(headers: &HeaderMap) -> Result<Option<EventPrincipal>, RpcError> {
    let owner = header(headers, "x-omni-events-owner").filter(|v| !v.is_empty());
    let authorization = header(headers, "x-omni-events-authorization").filter(|v| !v.is_empty());
    if owner.is_none() && authorization.is_none() {
        return Ok(None);
    }
    match (owner, authorization) {
        (Some(owner), Some(authorization))
            if is_executor_owner(owner) && is_bearer_authorization(authorization) =>
        {
            Ok(Some(EventPrincipal {
                owner: owner.to_owned(),
                authorization: authorization.to_owned(),
            }))
        }
        _ => Err(RpcError::invalid_params("Invalid event principal")),
    }
}

pub(crate) fn service_error(error: EventServiceError) -> RpcError {
    match error {
        EventServiceError::Rejected(reason) => {
            let data = json!({ "reason": reason.as_str() });
            match reason {
                SubscriptionRejection::InvalidPrincipal => RpcError {
                    code: -32001,
                    message: error.to_string(),
                    data: Some(data),
                },
                SubscriptionRejection::InvalidCallback
                | SubscriptionRejection::ChallengeFailed
                | SubscriptionRejection::Timeout => RpcError {
                    code: -32015,
                    message: "CallbackEndpointError".to_owned(),
                    data: Some(data),
                },
                _ => RpcError {
                    code: -32602,
                    message: error.to_string(),
                    data: Some(data),
                },
            }
        }
        other => RpcError::internal(other.to_string()),
    }
}

/// Handles one `events/*` request (params without `_meta`).
pub async fn handle_event_method(
    events: &McpEventService,
    method: &str,
    params: &Map<String, Value>,
    headers: &HeaderMap,
) -> Result<Value, RpcError> {
    match method {
        "events/list" => {
            let mut issues = Issues::default();
            match params.get("cursor") {
                None | Some(Value::String(_)) => {}
                other => issues.expected(&["cursor"], "string", other),
            }
            issues.strict(params, &["cursor"], &[]);
            issues.into_result(method)?;
            let owner = header(headers, "x-omni-events-owner").filter(|o| is_executor_owner(o));
            events.record_discovery(owner).await;
            Ok(json!({ "events": event_catalog() }))
        }
        "events/subscribe" => {
            let input = subscribe_input(params)?;
            let principal = principal(headers)?;
            let result = events
                .subscribe(&input, principal.as_ref())
                .await
                .map_err(service_error)?;
            serde_json::to_value(result).map_err(|e| RpcError::internal(e.to_string()))
        }
        "events/unsubscribe" => {
            let input = unsubscribe_input(params)?;
            let principal = principal(headers)?;
            events
                .unsubscribe(&input, principal.as_ref())
                .await
                .map_err(service_error)?;
            Ok(json!({}))
        }
        _ => Err(RpcError::method_not_found()),
    }
}
