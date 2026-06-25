#![allow(
    clippy::unwrap_used,
    clippy::str_to_string,
    clippy::too_many_lines,
    clippy::unreadable_literal,
    clippy::uninlined_format_args
)]

use axum::response::IntoResponse;
use jmap_matrix_bridge::client_manager::ClientManager;
use jmap_matrix_bridge::matrix::MatrixClient;
use jmap_matrix_bridge::routes::AppState;
use jmap_matrix_bridge::state::StateStore;
use jmap_matrix_bridge::store::Store;
use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;
use std::sync::Arc;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn test_full_bridge_cycle() {
    let mock_server = MockServer::start().await;

    // 1. Setup mocks for JMAP session discovery
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
             "username": "user",
             "accounts": {
                 "acc1": {
                     "name": "user",
                     "isPrimary": true,
                     "isReadOnly": false,
                     "isPersonal": true,
                     "accountCapabilities": {
                         "urn:ietf:params:jmap:core": {},
                         "urn:ietf:params:jmap:mail": {}
                     }
                 }
             },
             "primaryAccounts": {
                 "urn:ietf:params:jmap:core": "acc1",
                 "urn:ietf:params:jmap:mail": "acc1"
             },
             "apiUrl": format!("{}/api", mock_server.uri()),
             "downloadUrl": "http://127.0.0.1/download",
             "uploadUrl": "http://127.0.0.1/upload",
             "eventSourceUrl": format!("{}/events", mock_server.uri()),
             "capabilities": {
                "urn:ietf:params:jmap:core": {},
                "urn:ietf:params:jmap:mail": {}
            },
             "state": "s1"
        })))
        .mount(&mock_server)
        .await;

    // 2. Mock Email/set (sending an email)
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(body_partial_json(serde_json::json!({
            "methodCalls": [["Email/set", {"create": {}}, "0"]]
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [["Email/set", {
                "accountId": "acc1",
                "oldState": "s1",
                "newState": "s2",
                "created": {"new-email": {"id": "email1"}}
            }, "0"]]
        })))
        .mount(&mock_server)
        .await;

    // 3. Setup bridge state
    let store = Store::new_in_memory(None).await.unwrap();
    let matrix = MatrixClient::new(&mock_server.uri(), "token", "localhost")
        .await
        .unwrap();
    let state_store = StateStore::new();
    let client_manager = ClientManager::new(store.clone(), matrix, 10);

    let state = AppState {
        client_manager: Arc::new(client_manager),
        state_store: Arc::new(state_store),
        puppet_manager: std::sync::Arc::new(jmap_matrix_bridge::puppet::PuppetManager::new(
            String::new(),
            "@_jmap_bot:localhost".to_string(),
        )),
        permissions: std::sync::Arc::new(jmap_matrix_bridge::permissions::Permissions::allow_all()),
        hs_token: "hs_token".to_string(),
    };

    // 4. Perform login (this hits the discovery mock)
    state
        .client_manager
        .login(
            "@user:localhost".to_string(),
            "user".to_string(),
            "pass".to_string(),
            mock_server.uri(),
        )
        .await
        .unwrap();

    // 5. Test outbound email via command
    let body = "!email to@example.com Subject Hello world";
    jmap_matrix_bridge::commands::handle_login_none(
        &state,
        "@user:localhost",
        body,
        Some("!room1:localhost"),
        None,
        &RoomMessageEventContent::text_plain(body),
    )
    .await
    .unwrap();

    // 6. Verify Matrix notification (mocked)
    // In this test, we just check that the command handler finished without error
    // and the JMAP mock was hit (wiremock handles verification if we used expect).
}

#[tokio::test]
async fn test_handle_users_endpoint() {
    let mock_server = MockServer::start().await;

    // Mock register endpoint
    Mock::given(method("POST"))
        .and(path("/_matrix/client/v3/register"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&mock_server)
        .await;

    let store = Store::new_in_memory(None).await.unwrap();
    let matrix = MatrixClient::new(&mock_server.uri(), "token", "localhost")
        .await
        .unwrap();
    let state_store = StateStore::new();
    let client_manager = ClientManager::new(store.clone(), matrix, 10);

    let state = AppState {
        client_manager: Arc::new(client_manager),
        state_store: Arc::new(state_store),
        puppet_manager: std::sync::Arc::new(jmap_matrix_bridge::puppet::PuppetManager::new(
            String::new(),
            "@_jmap_bot:localhost".to_string(),
        )),
        permissions: std::sync::Arc::new(jmap_matrix_bridge::permissions::Permissions::allow_all()),
        hs_token: "hs_token".to_string(),
    };

    // Case 1: Valid bridge namespace user ID
    let response = jmap_matrix_bridge::routes::handle_users(
        axum::extract::State(state.clone()),
        axum::extract::Path("@_jmap_user=40example.com:localhost".to_string()),
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::OK);

    // Case 2: User ID not in namespace
    let response = jmap_matrix_bridge::routes::handle_users(
        axum::extract::State(state.clone()),
        axum::extract::Path("@someone_else:localhost".to_string()),
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND);

    // Case 3: Invalid User ID format in namespace (can't parse)
    let response = jmap_matrix_bridge::routes::handle_users(
        axum::extract::State(state.clone()),
        axum::extract::Path("@_jmap_invalid_no_colon".to_string()),
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_handle_transactions_database_error() {
    let mock_server = MockServer::start().await;

    let store = Store::new_in_memory(None).await.unwrap();
    // Register the user to satisfy foreign key constraint
    store
        .save_user(&jmap_matrix_bridge::store::RegisteredUser {
            matrix_user_id: "@user:localhost".to_string(),
            jmap_username: "user".to_string(),
            jmap_token: "secret".to_string(),
            jmap_url: mock_server.uri(),
        })
        .await
        .unwrap();
    // Link room to a ghost email
    store
        .save_room_ghost_mapping("!room1:localhost", "ghost@example.com", "@user:localhost")
        .await
        .unwrap();

    let matrix = MatrixClient::new(&mock_server.uri(), "token", "localhost")
        .await
        .unwrap();
    let state_store = StateStore::new();
    let client_manager = ClientManager::new(store.clone(), matrix, 10);

    let state = AppState {
        client_manager: Arc::new(client_manager),
        state_store: Arc::new(state_store),
        puppet_manager: std::sync::Arc::new(jmap_matrix_bridge::puppet::PuppetManager::new(
            String::new(),
            "@_jmap_bot:localhost".to_string(),
        )),
        permissions: std::sync::Arc::new(jmap_matrix_bridge::permissions::Permissions::allow_all()),
        hs_token: "hs_token".to_string(),
    };

    // Construct a valid message transaction
    let txn_json = serde_json::json!({
        "events": [
            {
                "type": "m.room.message",
                "sender": "@user:localhost",
                "room_id": "!room1:localhost",
                "event_id": "$event1",
                "origin_server_ts": 12345678,
                "content": {
                    "msgtype": "m.text",
                    "body": "hello world"
                }
            }
        ]
    });
    let txn: jmap_matrix_bridge::routes::MatrixTransaction =
        serde_json::from_value(txn_json).unwrap();

    // Now corrupt the database by dropping the room_ghost_mapping table
    sqlx::query("DROP TABLE room_ghost_mapping")
        .execute(store.pool())
        .await
        .unwrap();

    // Call handle_transactions and expect 500 Internal Server Error
    let response = jmap_matrix_bridge::routes::handle_transactions(
        axum::extract::State(state),
        axum::extract::Path("txn1".to_string()),
        axum::extract::Json(txn),
    )
    .await
    .into_response();

    assert_eq!(
        response.status(),
        axum::http::StatusCode::INTERNAL_SERVER_ERROR
    );
}
