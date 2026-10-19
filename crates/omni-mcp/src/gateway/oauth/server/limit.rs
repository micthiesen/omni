//! Process-wide token buckets in front of the authorization server routes,
//! so a flood cannot monopolize the store thread or the owner's
//! notifications: one for the token endpoint and one for everything else,
//! so a flood of sign-ins or registrations never blocks token refreshes.

use std::sync::{Mutex, PoisonError};

use axum::Json;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse as _, Response};
use serde_json::json;

use super::{AUTHORIZE_PATH, AuthorizationServer, COMPLETE_PATH, TOKEN_PATH, pages};

/// Sustained requests per second.
pub const RATE_PER_SECOND: f64 = 20.0;
/// Requests allowed at once after a quiet period.
pub const RATE_BURST: f64 = 60.0;

pub(super) struct TokenBucket {
    tokens: f64,
    updated_ms: i64,
}

impl TokenBucket {
    pub(super) fn full(now_ms: i64) -> Mutex<Self> {
        Mutex::new(Self {
            tokens: RATE_BURST,
            updated_ms: now_ms,
        })
    }

    /// Takes one token, or the whole seconds until one is available.
    fn take(&mut self, now_ms: i64) -> Result<(), u64> {
        let elapsed = now_ms.saturating_sub(self.updated_ms).max(0);
        self.updated_ms = self.updated_ms.max(now_ms);
        self.tokens = (self.tokens + elapsed as f64 * RATE_PER_SECOND / 1000.0).min(RATE_BURST);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            Ok(())
        } else {
            Err(((1.0 - self.tokens) / RATE_PER_SECOND).ceil().max(1.0) as u64)
        }
    }
}

pub(super) async fn rate_limit(
    State(server): State<AuthorizationServer>,
    request: Request,
    next: Next,
) -> Response {
    let now = server.now();
    let bucket = if request.uri().path() == TOKEN_PATH {
        &server.inner.token_limiter
    } else {
        &server.inner.limiter
    };
    let taken = bucket
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take(now);
    let Err(retry_after) = taken else {
        return next.run(request).await;
    };
    let path = request.uri().path();
    let mut response = if path == AUTHORIZE_PATH || path == COMPLETE_PATH {
        pages::error(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many requests",
            "Omni is receiving too many authorization requests. Wait a moment and try again.",
        )
    } else {
        (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({
                "error": "temporarily_unavailable",
                "error_description": "too many requests; retry later",
            })),
        )
            .into_response()
    };
    if let Ok(value) = HeaderValue::from_str(&retry_after.to_string()) {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bucket_allows_a_burst_then_refills_at_the_rate() {
        let mut bucket = TokenBucket {
            tokens: RATE_BURST,
            updated_ms: 0,
        };
        for _ in 0..60 {
            assert!(bucket.take(0).is_ok());
        }
        assert_eq!(bucket.take(0), Err(1));
        // 50 ms refills one token at 20 per second.
        assert!(bucket.take(50).is_ok());
        assert!(bucket.take(50).is_err());
        // A clock moving backwards never mints tokens.
        assert!(bucket.take(10).is_err());
        assert!(bucket.take(10_000).is_ok());
        assert!(bucket.tokens <= RATE_BURST);
    }
}
