//! Whisker sign-in through Cognito `USER_SRP_AUTH`.
//!
//! The ID token is cached in memory and reused until five minutes before it
//! expires. SRP math comes from `aws-cognito-srp`; the two Cognito calls
//! (`InitiateAuth`, `RespondToAuthChallenge`) go through the shared client.

use std::time::Duration;

use aws_cognito_srp::{SrpClient, User, UserAuthenticationParameters, VerificationParameters};
use base64::Engine as _;
use omni_core::clock::SharedClock;
use omni_http::{HttpClient, Method, Url};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;

pub const USER_POOL_ID: &str = "us-east-1_rjhNnZVAm";
pub const CLIENT_ID: &str = "4552ujeu3aic90nf8qn53levmn";
pub const COGNITO_ENDPOINT: &str = "https://cognito-idp.us-east-1.amazonaws.com/";
/// Re-authenticate five minutes before expiry.
const TOKEN_EXPIRY_BUFFER_MS: i64 = 5 * 60 * 1000;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const LOG: &str = "WhiskerAuth";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("Whisker authentication failed: {0}")]
pub struct WhiskerAuthenticationError(pub String);

/// An authenticated session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WhiskerSession {
    pub id_token: String,
    pub user_id: String,
}

#[derive(Clone, Debug)]
struct Cached {
    session: WhiskerSession,
    expires_at: i64,
}

/// Signs in and caches the token per process.
pub struct WhiskerAuth {
    http: HttpClient,
    clock: SharedClock,
    email: String,
    password: String,
    cached: Mutex<Option<Cached>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct InitiateAuthResponse {
    challenge_name: Option<String>,
    #[serde(default)]
    challenge_parameters: std::collections::HashMap<String, String>,
    session: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ChallengeResponse {
    authentication_result: Option<AuthenticationResult>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct AuthenticationResult {
    id_token: String,
}

#[derive(Deserialize)]
struct JwtPayload {
    mid: String,
    exp: Option<f64>,
}

fn fail(message: impl Into<String>) -> WhiskerAuthenticationError {
    WhiskerAuthenticationError(message.into())
}

/// Reads `{mid, exp?}` from a JWT payload without verifying it (the token
/// comes straight from Cognito over TLS).
pub fn decode_jwt_payload(
    token: &str,
) -> Result<(String, Option<f64>), WhiskerAuthenticationError> {
    let parts: Vec<&str> = token.split('.').collect();
    let payload = match parts.as_slice() {
        [_, payload, _] if !payload.is_empty() => *payload,
        _ => return Err(fail("Invalid JWT format")),
    };
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .map_err(|e| fail(e.to_string()))?;
    let decoded: JwtPayload = serde_json::from_slice(&bytes).map_err(|e| fail(e.to_string()))?;
    Ok((decoded.mid, decoded.exp))
}

impl WhiskerAuth {
    pub fn new(http: HttpClient, clock: SharedClock, email: String, password: String) -> Self {
        Self {
            http,
            clock,
            email,
            password,
            cached: Mutex::new(None),
        }
    }

    async fn cognito(
        &self,
        target: &str,
        body: Value,
    ) -> Result<Value, WhiskerAuthenticationError> {
        let url = Url::parse(COGNITO_ENDPOINT).map_err(|e| fail(e.to_string()))?;
        let response = self
            .http
            .request(Method::POST, url)
            .header("content-type", "application/x-amz-json-1.1")
            .header(
                "x-amz-target",
                format!("AWSCognitoIdentityProviderService.{target}"),
            )
            .body(body.to_string())
            .timeout(REQUEST_TIMEOUT)
            .send_bounded(MAX_RESPONSE_BYTES)
            .await
            .map_err(|e| fail(e.to_string()))?;
        let value: Value = serde_json::from_slice(&response.body).unwrap_or(Value::Null);
        if !response.status.is_success() {
            let message = value
                .get("message")
                .or_else(|| value.get("Message"))
                .and_then(Value::as_str)
                .unwrap_or("Cognito request failed");
            return Err(fail(format!(
                "{message} (HTTP {})",
                response.status.as_u16()
            )));
        }
        Ok(value)
    }

    async fn sign_in(&self) -> Result<String, WhiskerAuthenticationError> {
        let client = SrpClient::new(
            User::new(USER_POOL_ID, &self.email, &self.password),
            CLIENT_ID,
            None,
        );
        let UserAuthenticationParameters { a, username } = client.get_auth_parameters();
        let initiate = self
            .cognito(
                "InitiateAuth",
                json!({
                    "AuthFlow": "USER_SRP_AUTH",
                    "ClientId": CLIENT_ID,
                    "AuthParameters": {"USERNAME": username, "SRP_A": a},
                    "ClientMetadata": {},
                }),
            )
            .await?;
        let initiate: InitiateAuthResponse =
            serde_json::from_value(initiate).map_err(|e| fail(e.to_string()))?;
        if initiate.challenge_name.as_deref() != Some("PASSWORD_VERIFIER") {
            return Err(fail(format!(
                "unexpected challenge {:?}",
                initiate.challenge_name
            )));
        }
        let param = |name: &str| {
            initiate
                .challenge_parameters
                .get(name)
                .cloned()
                .ok_or_else(|| fail(format!("missing challenge parameter {name}")))
        };
        let user_id = param("USER_ID_FOR_SRP")?;
        let VerificationParameters {
            password_claim_secret_block,
            password_claim_signature,
            timestamp,
        } = client
            .verify(
                &param("SECRET_BLOCK")?,
                &user_id,
                &param("SALT")?,
                &param("SRP_B")?,
            )
            .map_err(|e| fail(e.to_string()))?;
        let mut request = json!({
            "ChallengeName": "PASSWORD_VERIFIER",
            "ClientId": CLIENT_ID,
            "ChallengeResponses": {
                "USERNAME": user_id,
                "PASSWORD_CLAIM_SECRET_BLOCK": password_claim_secret_block,
                "TIMESTAMP": timestamp,
                "PASSWORD_CLAIM_SIGNATURE": password_claim_signature,
            },
            "ClientMetadata": {},
        });
        if let (Some(session), Some(map)) = (initiate.session, request.as_object_mut()) {
            map.insert("Session".to_owned(), Value::String(session));
        }
        let response = self.cognito("RespondToAuthChallenge", request).await?;
        let response: ChallengeResponse =
            serde_json::from_value(response).map_err(|e| fail(e.to_string()))?;
        response
            .authentication_result
            .map(|result| result.id_token)
            .ok_or_else(|| fail("Cognito returned no authentication result"))
    }

    /// The cached session, or a fresh sign-in.
    pub async fn authenticate(&self) -> Result<WhiskerSession, WhiskerAuthenticationError> {
        let mut cached = self.cached.lock().await;
        let now = self.clock.now_ms();
        if let Some(entry) = cached.as_ref()
            && now < entry.expires_at - TOKEN_EXPIRY_BUFFER_MS
        {
            tracing::debug!(target: LOG, "Using cached credentials");
            return Ok(entry.session.clone());
        }
        let id_token = self.sign_in().await?;
        omni_core::secrets::register_runtime(&id_token);
        let (user_id, exp) = decode_jwt_payload(&id_token)?;
        #[allow(clippy::cast_possible_truncation)]
        let expires_at = exp
            .filter(|exp| *exp != 0.0 && exp.is_finite())
            .map_or(now + 60 * 60 * 1000, |exp| (exp * 1000.0) as i64);
        let session = WhiskerSession { id_token, user_id };
        *cached = Some(Cached {
            session: session.clone(),
            expires_at,
        });
        tracing::debug!(target: LOG, user_id = %session.user_id, "Authenticated (fresh)");
        Ok(session)
    }
}
