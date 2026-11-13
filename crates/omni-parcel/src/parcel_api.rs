//! Parcel `add-delivery` submission, shared by the email pipeline and
//! `parcels_add`. In `SideEffectMode::Record` nothing is sent: the payload is
//! recorded and reported as accepted.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use omni_core::js::{json_stringify, json_stringify_pretty2, to_iso_string};
use omni_http::{HttpClient, HttpError, Method, SideEffectMode, Url};
use omni_store::Store;
use serde::Deserialize;
use serde_json::json;

use crate::add_ledger;
use crate::log_file::{LogFile, code_block};

const LOG: &str = "Main:ParcelTracker";
pub const ADD_DELIVERY_URL: &str = "https://api.parcel.app/external/add-delivery/";
const SUBMIT_TIMEOUT: Duration = Duration::from_secs(10);
const RESPONSE_LIMIT: usize = 1024 * 1024;

/// `SubmitResult`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubmitResult {
    Success,
    /// A 4xx answer.
    Rejected {
        status: u16,
    },
    /// Network failure or any other status: transient.
    Error,
}

/// A 4xx rejection other than auth or rate limit
/// plausibly means the wrong carrier was picked.
pub fn should_try_next_candidate(result: SubmitResult) -> bool {
    match result {
        SubmitResult::Rejected { status } => !matches!(status, 401 | 403 | 429),
        SubmitResult::Success | SubmitResult::Error => false,
    }
}

/// What Parcel answered: the classified result, the HTTP status and Parcel's
/// own `error_message`, when it sent one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AddResponse {
    pub result: SubmitResult,
    pub status: Option<u16>,
    pub error_message: Option<String>,
    /// `Retry-After` in milliseconds, on a 429.
    pub retry_after_ms: Option<i64>,
}

/// Parcel's documented response body (`success` is always present;
/// `error_message` only on errors).
#[derive(Deserialize)]
struct AddBody {
    success: Option<bool>,
    error_message: Option<String>,
}

fn decode_body(body: &[u8]) -> Option<AddBody> {
    serde_json::from_slice(body).ok()
}

/// One submission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubmitParams {
    pub tracking_number: String,
    pub carrier_code: String,
    pub description: String,
}

/// The Parcel call (a test seam).
pub trait ParcelSubmitter: Send + Sync {
    fn submit<'a>(
        &'a self,
        params: &'a SubmitParams,
        rejection_log: Option<&'a LogFile>,
    ) -> BoxFuture<'a, SubmitResult>;
}

/// The HTTP submitter.
pub struct ParcelApi {
    http: HttpClient,
    api_key: String,
    mode: SideEffectMode,
    url: Url,
    clock: omni_core::clock::SharedClock,
    recorded: Arc<Mutex<Vec<serde_json::Value>>>,
    /// Where pipeline submissions are counted against Parcel's daily limit.
    ledger: Option<Store>,
}

impl ParcelApi {
    pub fn new(
        http: HttpClient,
        api_key: String,
        mode: SideEffectMode,
        clock: omni_core::clock::SharedClock,
    ) -> Result<Self, HttpError> {
        let url = Url::parse(ADD_DELIVERY_URL).map_err(|e| HttpError::InvalidUrl(e.to_string()))?;
        Ok(Self {
            http,
            api_key,
            mode,
            url,
            clock,
            recorded: Arc::new(Mutex::new(Vec::new())),
            ledger: None,
        })
    }

    /// Counts every pipeline submission in the add ledger (`add_ledger`),
    /// so `parcels_add` leaves room for email under Parcel's daily limit.
    #[must_use]
    pub fn with_ledger(mut self, store: Store) -> Self {
        self.ledger = Some(store);
        self
    }

    pub fn mode(&self) -> SideEffectMode {
        self.mode
    }

    pub fn clock(&self) -> &omni_core::clock::SharedClock {
        &self.clock
    }

    /// Payloads recorded instead of sent (`SideEffectMode::Record`).
    pub fn recorded(&self) -> Vec<serde_json::Value> {
        self.recorded
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// One `parcels_add` request: like a pipeline submission, but a failure
    /// is the caller's answer, so it logs at WARN and never alerts.
    pub async fn add(&self, params: &SubmitParams, send_push_confirmation: bool) -> AddResponse {
        self.send(params, send_push_confirmation, None, false).await
    }

    /// Exactly one `add-delivery` request; never retries.
    async fn send(
        &self,
        params: &SubmitParams,
        send_push_confirmation: bool,
        rejection_log: Option<&LogFile>,
        alert_on_failure: bool,
    ) -> AddResponse {
        let payload = json!({
            "tracking_number": params.tracking_number,
            "carrier_code": params.carrier_code,
            "description": params.description,
            "send_push_confirmation": send_push_confirmation,
        });
        tracing::info!(target: LOG, "Submitting delivery: {}", json_stringify(&payload));
        if self.mode == SideEffectMode::Record {
            self.recorded
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(payload);
            tracing::info!(
                target: LOG,
                "Recorded delivery (side effects recorded): {} ({})",
                params.tracking_number,
                params.carrier_code
            );
            return AddResponse {
                result: SubmitResult::Success,
                status: None,
                error_message: None,
                retry_after_ms: None,
            };
        }
        let response = self
            .http
            .request(Method::POST, self.url.clone())
            .header("api-key", self.api_key.as_str())
            .json(&payload)
            .timeout(SUBMIT_TIMEOUT)
            .send_bounded(RESPONSE_LIMIT)
            .await;
        let (status, body, message, decoded, retry_after_ms) = match response {
            Ok(response) if response.status.is_success() => {
                let status = response.status.as_u16();
                tracing::info!(
                    target: LOG,
                    "Submitted delivery: {} ({}) → {status}",
                    params.tracking_number,
                    params.carrier_code,
                );
                let decoded = decode_body(&response.body);
                return AddResponse {
                    result: SubmitResult::Success,
                    status: Some(status),
                    // A 2xx is success for the pipeline; a `success: false`
                    // body keeps its message for `parcels_add`.
                    error_message: decoded
                        .filter(|body| body.success == Some(false))
                        .map(|body| body.error_message.unwrap_or_default()),
                    retry_after_ms: None,
                };
            }
            Ok(response) => {
                let status = response.status.as_u16();
                let retry_after_ms = response
                    .headers
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.trim().parse::<i64>().ok())
                    .map(|secs| secs.saturating_mul(1000));
                let decoded = decode_body(&response.body);
                let body = String::from_utf8_lossy(&response.body).into_owned();
                (
                    Some(status),
                    body,
                    format!("Response code {status}"),
                    decoded,
                    retry_after_ms,
                )
            }
            Err(error) => (
                None,
                "no response body".to_owned(),
                error.to_string(),
                None,
                None,
            ),
        };
        let error_message = decoded.and_then(|body| body.error_message);
        let detail = format!("Parcel submission failed: {message}\nResponse: {body}");
        if alert_on_failure {
            tracing::error!(
                target: LOG,
                detail = %detail,
                "Failed to submit delivery {}",
                params.tracking_number
            );
        } else {
            tracing::warn!(
                target: LOG,
                detail = %detail,
                "Parcel did not add delivery {}",
                params.tracking_number
            );
        }
        match status {
            Some(status) if (400..500).contains(&status) => {
                if let Some(log) = rejection_log {
                    log.section(
                        &format!(
                            "Rejected: {} ({status}) — {}",
                            params.tracking_number,
                            to_iso_string(self.clock.now_ms())
                        ),
                        &format!(
                            "**Request:**\n{}\n\n**Response:**\n{}",
                            code_block(&json_stringify_pretty2(&payload), Some("json")),
                            code_block(&body, None)
                        ),
                    )
                    .await;
                }
                AddResponse {
                    result: SubmitResult::Rejected { status },
                    status: Some(status),
                    error_message,
                    retry_after_ms,
                }
            }
            _ => AddResponse {
                result: SubmitResult::Error,
                status,
                error_message,
                retry_after_ms: None,
            },
        }
    }
}

impl ParcelSubmitter for ParcelApi {
    fn submit<'a>(
        &'a self,
        params: &'a SubmitParams,
        rejection_log: Option<&'a LogFile>,
    ) -> BoxFuture<'a, SubmitResult> {
        Box::pin(async move {
            if self.mode == SideEffectMode::Live
                && let Some(store) = &self.ledger
                && let Err(error) = add_ledger::note(store, self.clock.now_ms()).await
            {
                tracing::warn!(target: LOG, "Could not count the Parcel add: {error}");
            }
            self.send(params, true, rejection_log, true).await.result
        })
    }
}

#[cfg(test)]
mod parcel_api_spec {
    use super::*;

    #[test]
    fn returns_true_for_a_400_rejection_likely_wrong_carrier() {
        assert!(should_try_next_candidate(SubmitResult::Rejected {
            status: 400
        }));
    }

    #[test]
    fn returns_true_for_other_4xx_rejections_like_404_and_422() {
        assert!(should_try_next_candidate(SubmitResult::Rejected {
            status: 404
        }));
        assert!(should_try_next_candidate(SubmitResult::Rejected {
            status: 422
        }));
    }

    #[test]
    fn returns_false_for_auth_rejections_not_carrier_related() {
        assert!(!should_try_next_candidate(SubmitResult::Rejected {
            status: 401
        }));
        assert!(!should_try_next_candidate(SubmitResult::Rejected {
            status: 403
        }));
    }

    #[test]
    fn returns_false_for_rate_limit_rejections() {
        assert!(!should_try_next_candidate(SubmitResult::Rejected {
            status: 429
        }));
    }

    #[test]
    fn returns_false_for_transient_errors_network_5xx() {
        assert!(!should_try_next_candidate(SubmitResult::Error));
    }

    #[test]
    fn returns_false_for_success() {
        assert!(!should_try_next_candidate(SubmitResult::Success));
    }
}
