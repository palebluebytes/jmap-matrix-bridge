//! Axum web server routes and Matrix transaction handlers.
//!
//! This module serves as the primary entry point for Matrix homeserver events
//! sent via the Application Service API.

use crate::client_manager::ClientManager;
use crate::puppet::PuppetManager;
use crate::state::StateStore;
use axum::{
    extract::{Json, Path, Request, State},
    http::StatusCode,
    http::header::AUTHORIZATION,
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::sync::Arc;
use tracing::{debug, error, info, warn};

// Re-export MatrixTransaction and notify for backward-compatibility
pub use crate::services::transactions::{MatrixTransaction, notify};

// ─── Data Types ───────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct AppState {
    pub client_manager: Arc<ClientManager>,
    pub state_store: Arc<StateStore>,
    pub puppet_manager: Arc<PuppetManager>,
    pub hs_token: String,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("client_manager", &self.client_manager)
            .field("state_store", &self.state_store)
            .field("hs_token", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

// ─── Middleware ───────────────────────────────────────────────────────────────

pub async fn auth_middleware(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let auth_header = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .map(str::trim);

    // Parse access_token from query string.  The hs_token is always hex, so
    // no percent-decoding is needed, but we use split_once to be robust.
    let query_token_owned: Option<String> = request.uri().query().and_then(|q| {
        q.split('&')
            .filter_map(|pair| pair.split_once('='))
            .find(|(k, _)| *k == "access_token")
            .map(|(_, v)| v.to_owned())
    });
    let query_token = query_token_owned.as_deref();

    let token = auth_header.or(query_token);
    let hs_token = &state.hs_token;

    match token {
        Some(token) if token == hs_token => {
            debug!("Authenticated request");
            next.run(request).await
        }
        Some(_) => {
            // Per Matrix AS spec §7.1, invalid tokens → 403 Forbidden.
            warn!("Forbidden request: token does not match hs_token");
            (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({
                    "errcode": "M_FORBIDDEN",
                    "error": "Bad hs_token"
                })),
            )
                .into_response()
        }
        None => {
            // Per Matrix AS spec §7.1, missing tokens → 401 Unauthorized.
            warn!(
                "Unauthorized request: missing token in Authorization header or query parameters"
            );
            (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({
                    "errcode": "M_MISSING_TOKEN",
                    "error": "Missing access token"
                })),
            )
                .into_response()
        }
    }
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

pub async fn handle_transactions(
    State(state): State<AppState>,
    Path(txn_id): Path<String>,
    Json(txn): Json<MatrixTransaction>,
) -> impl IntoResponse {
    match state
        .client_manager
        .store
        .is_transaction_processed(&txn_id)
        .await
    {
        Ok(true) => {
            debug!(%txn_id, "Transaction already processed, skipping");
            return Json(serde_json::json!({})).into_response();
        }
        Err(e) => {
            error!(%txn_id, error = %e, "Failed to check transaction status");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        _ => {}
    }

    info!(%txn_id, "Received Matrix transaction with {} events", txn.events.len());

    if let Err(err) = crate::services::transactions::process_transaction(&state, &txn_id, txn).await
    {
        error!(%txn_id, error = %err, "Transaction processing failed");
        if err.downcast_ref::<sqlx::Error>().is_some() {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }

    if let Err(e) = state
        .client_manager
        .store
        .mark_transaction_processed(&txn_id)
        .await
    {
        error!(%txn_id, error = %e, "Failed to mark transaction as processed");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    Json(serde_json::json!({})).into_response()
}

pub async fn handle_users(State(state): State<AppState>, Path(user_id): Path<String>) -> Response {
    if user_id.starts_with("@_jmap_") {
        debug!(%user_id, "User query matched bridge namespace");

        let parsed = match matrix_sdk::ruma::UserId::parse(&user_id) {
            Ok(uid) => uid,
            Err(e) => {
                warn!(%user_id, error = %e, "Invalid UserId in user query");
                return StatusCode::NOT_FOUND.into_response();
            }
        };

        let localpart = parsed.localpart();
        if let Err(e) = state
            .client_manager
            .matrix
            .ensure_user_exists(localpart)
            .await
        {
            error!(%user_id, %localpart, error = %e, "Failed to register ghost user");
            return StatusCode::NOT_FOUND.into_response();
        }

        Json(serde_json::json!({})).into_response()
    } else {
        debug!(%user_id, "User query did not match bridge namespace");
        StatusCode::NOT_FOUND.into_response()
    }
}

#[allow(clippy::unused_async)]
pub async fn handle_rooms(
    State(_state): State<AppState>,
    Path(room_alias): Path<String>,
) -> Response {
    debug!(%room_alias, "Room alias query not supported by this bridge");
    StatusCode::NOT_FOUND.into_response()
}

#[derive(serde::Deserialize, Debug)]
pub struct PingRequest {
    pub transaction_id: Option<String>,
}

#[allow(clippy::unused_async)]
pub async fn handle_ping(
    State(_state): State<AppState>,
    Json(ping): Json<PingRequest>,
) -> Response {
    debug!(
        "Received ping from homeserver: transaction_id={:?}",
        ping.transaction_id
    );
    Json(serde_json::json!({})).into_response()
}
