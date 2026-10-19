//! `POST /gateway/oauth/register`: RFC 7591 dynamic registration of public
//! clients. At [`super::MAX_DCR_CLIENTS`], the oldest client with no grant
//! and no live request makes room; registration is refused only when every
//! client is in use.

use axum::Json;
use axum::body::Body;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse as _, Response};
use omni_http::Url;
use omni_store::StoreError;
use omni_store::entity::{EntityOps as _, EntityWrite as _, UpsertOpts};
use serde::Deserialize;
use serde_json::json;

use super::{AuthorizationServer, DCR_KIND, LOG, clean_text, dcr_evictions, valid_redirect_uris};
use crate::gateway::model::{AuthorizationRow, GrantRow, OAuthClientRow};

const MAX_BODY: usize = 16 * 1024;
const MAX_CLIENT_NAME: usize = 100;

#[derive(Deserialize)]
struct RegistrationRequest {
    redirect_uris: Option<Vec<String>>,
    client_name: Option<String>,
    token_endpoint_auth_method: Option<String>,
    grant_types: Option<Vec<String>>,
    response_types: Option<Vec<String>>,
}

fn refuse(error: &str, description: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"error": error, "error_description": description})),
    )
        .into_response()
}

pub(super) async fn register(State(server): State<AuthorizationServer>, body: Body) -> Response {
    let Ok(bytes) = axum::body::to_bytes(body, MAX_BODY).await else {
        return refuse("invalid_client_metadata", "the request body is too large");
    };
    let Ok(request) = serde_json::from_slice::<RegistrationRequest>(&bytes) else {
        return refuse(
            "invalid_client_metadata",
            "the body must be a JSON object of client metadata",
        );
    };
    if request
        .token_endpoint_auth_method
        .as_deref()
        .is_some_and(|method| method != "none")
    {
        return refuse(
            "invalid_client_metadata",
            "only public clients (token_endpoint_auth_method none) are supported",
        );
    }
    let redirect_uris = request.redirect_uris.unwrap_or_default();
    if !valid_redirect_uris(&redirect_uris) {
        return refuse(
            "invalid_redirect_uri",
            "redirect_uris must list 1 to 10 https (or loopback http) URIs of at most 2048 \
             bytes, without a fragment",
        );
    }
    let grant_types_ok = request.grant_types.as_ref().is_none_or(|types| {
        types
            .iter()
            .all(|t| t == "authorization_code" || t == "refresh_token")
    });
    let response_types_ok = request
        .response_types
        .as_ref()
        .is_none_or(|types| types.iter().all(|t| t == "code"));
    if !grant_types_ok || !response_types_ok {
        return refuse(
            "invalid_client_metadata",
            "only the authorization_code and refresh_token grants with the code response type are supported",
        );
    }
    let client_name = match request.client_name.as_deref() {
        None => None,
        Some(name) => match clean_text(name, MAX_CLIENT_NAME) {
            Some(name) => Some(name),
            None => {
                return refuse(
                    "invalid_client_metadata",
                    "client_name must be 1 to 100 characters",
                );
            }
        },
    };
    let client_name = client_name.unwrap_or_else(|| {
        redirect_uris
            .first()
            .and_then(|uri| Url::parse(uri).ok())
            .and_then(|url| url.host_str().map(str::to_owned))
            .unwrap_or_else(|| "Unnamed app".to_owned())
    });
    let now = server.now();
    let row = OAuthClientRow {
        client_id: omni_core::ids::uuid_v4(),
        kind: DCR_KIND.to_owned(),
        client_name,
        redirect_uris,
        created_at: now,
        extra: Default::default(),
    };
    let stored = row.clone();
    let written = server
        .inner
        .store
        .write(move |tx| {
            let clients = tx.get_all::<OAuthClientRow>()?;
            let grants = tx.get_all::<GrantRow>()?;
            let requests = tx.get_all::<AuthorizationRow>()?;
            let Some(evicted) = dcr_evictions(&clients, &grants, &requests, now) else {
                return Ok::<_, StoreError>(None);
            };
            for client_id in &evicted {
                tx.delete::<OAuthClientRow>(client_id)?;
            }
            tx.upsert(&stored, UpsertOpts::default())?;
            Ok(Some(evicted.len()))
        })
        .await;
    match written {
        Ok(Some(0)) => {}
        Ok(Some(evicted)) => {
            tracing::info!(target: LOG, evicted, "Unused OAuth clients removed to register a new one");
        }
        Ok(None) => {
            return refuse(
                "invalid_client_metadata",
                "too many clients are registered and all are in use",
            );
        }
        Err(error) => {
            tracing::warn!(target: LOG, %error, "OAuth client registration not recorded");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "server_error", "error_description": "registration failed"})),
            )
                .into_response();
        }
    }
    tracing::info!(target: LOG, client = %row.client_name, "OAuth client registered");
    (
        StatusCode::CREATED,
        Json(json!({
            "client_id": row.client_id,
            "client_id_issued_at": now / 1000,
            "client_name": row.client_name,
            "redirect_uris": row.redirect_uris,
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none",
        })),
    )
        .into_response()
}
