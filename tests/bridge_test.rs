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
use wiremock::matchers::{body_string_contains, method, path};
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

    // 2. Mock the send. `submit` makes three round-trips, each needing a response
    //    of its own method type — see tests/jmap_mock_test.rs. Mocking only
    //    Email/set (as this test used to) means the *first* call, Mailbox/query,
    //    404s and the send fails. That failure is invisible from the outside:
    //    the `!email` handler deliberately swallows it (src/commands/email.rs) so
    //    a bad send notifies the user instead of crashing the bridge — which is
    //    right for production, but means `.unwrap()` on the handler is guaranteed
    //    to pass and cannot serve as this test's oracle. `.expect(1)` on each mock
    //    is the oracle: it fails the moment a round-trip stops happening.

    // 2a. Mailbox/query — resolve the Sent mailbox.
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(body_string_contains("Mailbox/query"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [["Mailbox/query", {
                "accountId": "acc1",
                "queryState": "q1",
                "canCalculateChanges": false,
                "position": 0,
                "ids": ["MB_SENT"]
            }, "0"]]
        })))
        .expect(1)
        .mount(&mock_server)
        .await;

    // 2b. Identity/get — resolve the From identity. A supporting stub, so no
    //     `.expect`: it is hit twice (login resolves the identity too — see
    //     client_manager.rs — then `submit` resolves it again), and pinning that
    //     count would couple this test to login's internals for no gain. The
    //     oracles below are what make the test load-bearing.
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(body_string_contains("Identity/get"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [["Identity/get", {
                "accountId": "acc1",
                "state": "i1",
                "list": [{ "id": "IDENTITY_1", "name": "Test User", "email": "user@example.com" }],
                "notFound": []
            }, "0"]]
        })))
        .mount(&mock_server)
        .await;

    // 2c. The batched Email/set + EmailSubmission/set.
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(body_string_contains("Email/set"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [
                ["Email/set", {
                    "accountId": "acc1",
                    "oldState": "s1",
                    "newState": "s2",
                    "created": {"draft": {"id": "email1", "blobId": "b1", "threadId": "t1", "size": 42}},
                    "updated": {}, "destroyed": [],
                    "notCreated": {}, "notUpdated": {}, "notDestroyed": {}
                }, "0"],
                ["EmailSubmission/set", {
                    "accountId": "acc1",
                    "oldState": "s1",
                    "newState": "s2",
                    "created": {"sub": {"id": "SUB_NEW"}},
                    "updated": {}, "destroyed": [],
                    "notCreated": {}, "notUpdated": {}, "notDestroyed": {}
                }, "1"]
            ]
        })))
        .expect(1)
        .mount(&mock_server)
        .await;

    // 2d. The user-visible outcome. `notify` reports success or failure into the
    //     room, so pinning the success text asserts what the *user* actually sees:
    //     a failed send says "Failed to send email: …", which will not match here,
    //     so `.expect(1)` goes red. This is the assertion the test always wanted.
    Mock::given(method("PUT"))
        .and(body_string_contains("Email sent successfully!"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "event_id": "$notify" })),
        )
        .expect(1)
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
        double_puppet_secret: None,
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

    // 6. Verification happens on server drop: every `.expect(1)` above must have
    //    fired. Together they pin the whole cycle — the send's three JMAP
    //    round-trips, and the success notification the user sees. The handler's
    //    `.unwrap()` above proves nothing on its own; these do.
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
        double_puppet_secret: None,
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
        double_puppet_secret: None,
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
