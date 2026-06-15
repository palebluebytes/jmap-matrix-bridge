#![allow(
    clippy::unwrap_used,
    clippy::str_to_string,
    clippy::too_many_lines,
    clippy::unreadable_literal,
    clippy::uninlined_format_args
)]

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
    response::IntoResponse,
    routing::get,
};
use jmap_matrix_bridge::client_manager::ClientManager;
use jmap_matrix_bridge::matrix::MatrixClient;
use jmap_matrix_bridge::routes::{AppState, auth_middleware};
use jmap_matrix_bridge::state::StateStore;
use jmap_matrix_bridge::store::Store;
use std::sync::Arc;
use tower::util::ServiceExt;

async fn dummy_handler() -> impl IntoResponse {
    StatusCode::OK
}

#[tokio::test]
async fn test_auth_middleware_rejects_as_token() {
    let store = Store::new_in_memory(None).await.unwrap();
    let matrix = MatrixClient::new("http://localhost", "as_token_123", "localhost")
        .await
        .unwrap();
    let client_manager = Arc::new(ClientManager::new(store, matrix, 10));
    let state_store = Arc::new(StateStore::new());

    let state = AppState {
        client_manager,
        state_store,
        puppet_manager: std::sync::Arc::new(jmap_matrix_bridge::puppet::PuppetManager::new(String::new(), "@_jmap_bot:localhost".to_string())),
        hs_token: "hs_token_456".to_string(),
    };

    let app = Router::new()
        .route("/", get(dummy_handler))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/")
                .header("Authorization", "Bearer as_token_123")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_auth_middleware_valid_hs_token() {
    let store = Store::new_in_memory(None).await.unwrap();
    let matrix = MatrixClient::new("http://localhost", "as_token_123", "localhost")
        .await
        .unwrap();
    let client_manager = Arc::new(ClientManager::new(store, matrix, 10));
    let state_store = Arc::new(StateStore::new());

    let state = AppState {
        client_manager,
        state_store,
        puppet_manager: std::sync::Arc::new(jmap_matrix_bridge::puppet::PuppetManager::new(String::new(), "@_jmap_bot:localhost".to_string())),
        hs_token: "hs_token_456".to_string(),
    };

    let app = Router::new()
        .route("/", get(dummy_handler))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/")
                .header("Authorization", "Bearer hs_token_456")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_auth_middleware_invalid_token() {
    let store = Store::new_in_memory(None).await.unwrap();
    let matrix = MatrixClient::new("http://localhost", "as_token_123", "localhost")
        .await
        .unwrap();
    let client_manager = Arc::new(ClientManager::new(store, matrix, 10));
    let state_store = Arc::new(StateStore::new());

    let state = AppState {
        client_manager,
        state_store,
        puppet_manager: std::sync::Arc::new(jmap_matrix_bridge::puppet::PuppetManager::new(String::new(), "@_jmap_bot:localhost".to_string())),
        hs_token: "hs_token_456".to_string(),
    };

    let app = Router::new()
        .route("/", get(dummy_handler))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/")
                .header("Authorization", "Bearer wrong_token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_auth_middleware_missing_header() {
    let store = Store::new_in_memory(None).await.unwrap();
    let matrix = MatrixClient::new("http://localhost", "as_token_123", "localhost")
        .await
        .unwrap();
    let client_manager = Arc::new(ClientManager::new(store, matrix, 10));
    let state_store = Arc::new(StateStore::new());

    let state = AppState {
        client_manager,
        state_store,
        puppet_manager: std::sync::Arc::new(jmap_matrix_bridge::puppet::PuppetManager::new(String::new(), "@_jmap_bot:localhost".to_string())),
        hs_token: "hs_token_456".to_string(),
    };

    let app = Router::new()
        .route("/", get(dummy_handler))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_auth_middleware_valid_hs_token_query_param() {
    let store = Store::new_in_memory(None).await.unwrap();
    let matrix = MatrixClient::new("http://localhost", "as_token_123", "localhost")
        .await
        .unwrap();
    let client_manager = Arc::new(ClientManager::new(store, matrix, 10));
    let state_store = Arc::new(StateStore::new());

    let state = AppState {
        client_manager,
        state_store,
        puppet_manager: std::sync::Arc::new(jmap_matrix_bridge::puppet::PuppetManager::new(String::new(), "@_jmap_bot:localhost".to_string())),
        hs_token: "hs_token_456".to_string(),
    };

    let app = Router::new()
        .route("/", get(dummy_handler))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/?access_token=hs_token_456")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_auth_middleware_rejects_as_token_query_param() {
    let store = Store::new_in_memory(None).await.unwrap();
    let matrix = MatrixClient::new("http://localhost", "as_token_123", "localhost")
        .await
        .unwrap();
    let client_manager = Arc::new(ClientManager::new(store, matrix, 10));
    let state_store = Arc::new(StateStore::new());

    let state = AppState {
        client_manager,
        state_store,
        puppet_manager: std::sync::Arc::new(jmap_matrix_bridge::puppet::PuppetManager::new(String::new(), "@_jmap_bot:localhost".to_string())),
        hs_token: "hs_token_456".to_string(),
    };

    let app = Router::new()
        .route("/", get(dummy_handler))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/?access_token=as_token_123")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_auth_middleware_invalid_query_param() {
    let store = Store::new_in_memory(None).await.unwrap();
    let matrix = MatrixClient::new("http://localhost", "as_token_123", "localhost")
        .await
        .unwrap();
    let client_manager = Arc::new(ClientManager::new(store, matrix, 10));
    let state_store = Arc::new(StateStore::new());

    let state = AppState {
        client_manager,
        state_store,
        puppet_manager: std::sync::Arc::new(jmap_matrix_bridge::puppet::PuppetManager::new(String::new(), "@_jmap_bot:localhost".to_string())),
        hs_token: "hs_token_456".to_string(),
    };

    let app = Router::new()
        .route("/", get(dummy_handler))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/?access_token=wrong_token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}
