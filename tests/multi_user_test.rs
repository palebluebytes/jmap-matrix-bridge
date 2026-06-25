#![allow(
    clippy::unwrap_used,
    clippy::str_to_string,
    clippy::too_many_lines,
    clippy::unreadable_literal,
    clippy::uninlined_format_args
)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::{Router, routing::put};
use jmap_matrix_bridge::{client_manager::ClientManager, matrix, routes, store};
use serde_json::json;
use std::sync::Arc;
use tower::util::ServiceExt;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn test_multi_user_login_integration() {
    // 1. Setup Mock JMAP Server
    let mock_server = MockServer::start().await;
    let url = mock_server.uri();

    Mock::given(method("GET"))
        // .and(path("/jmap/session"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "username": "user",
            "accounts": {},
            "primaryAccounts": {},
            "apiUrl": format!("{}/api", url),
            "downloadUrl": format!("{}/download", url),
            "uploadUrl": format!("{}/upload", url),
            "eventSourceUrl": format!("{}/events", url),
            "capabilities": {
                "urn:ietf:params:jmap:core": {},
                "urn:ietf:params:jmap:mail": {}
            },
            "state": "s1"
        })))
        .mount(&mock_server)
        .await;

    // 2. Setup Bridge Components
    let store = store::Store::new_in_memory(None)
        .await
        .expect("Failed to create store");
    let matrix_client = matrix::MatrixClient::new("http://localhost:8008", "as_token", "localhost")
        .await
        .unwrap(); // Dummy matrix
    let client_manager = Arc::new(ClientManager::new(store.clone(), matrix_client, 10));
    let state_store = std::sync::Arc::new(jmap_matrix_bridge::state::StateStore::new());

    // 3. Setup Router
    let state = routes::AppState {
        client_manager,
        state_store,
        puppet_manager: std::sync::Arc::new(jmap_matrix_bridge::puppet::PuppetManager::new(
            String::new(),
            "@_jmap_bot:localhost".to_string(),
        )),
        permissions: std::sync::Arc::new(jmap_matrix_bridge::permissions::Permissions::allow_all()),
        hs_token: "hs_token".to_string(),
    };
    let app = Router::new()
        .route(
            "/_matrix/app/v1/transactions/{txn_id}",
            put(routes::handle_transactions),
        )
        .with_state(state);

    // 4. Simulate !login command via Matrix Transaction
    let login_cmd = format!("!login testuser secret {}/jmap/session", url);
    let txn_body = json!({
        "txn_id": "txn_mut_1",
        "events": [
            {
                "type": "m.room.message",
                "sender": "@matrix_user:localhost",
                "event_id": "$event1",
                "origin_server_ts": 1600000000000i64,
                "room_id": "!room:localhost",
                "content": {
                    "msgtype": "m.text",
                    "body": login_cmd
                }
            }
        ]
    });

    let response = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/_matrix/app/v1/transactions/txn_mut_1")
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_string(&txn_body).unwrap()))
                .unwrap(),
        )
        .await
        .expect("Failed to send request");

    assert_eq!(response.status(), StatusCode::OK);

    // 5. Verify Persistence
    let users = store.get_all_users().await.expect("Failed to fetch users");
    assert_eq!(users.len(), 1, "Expected 1 user to be registered");
    assert_eq!(users[0].matrix_user_id, "@matrix_user:localhost");
    assert_eq!(users[0].jmap_username, "testuser");
}

#[tokio::test]
async fn test_ghost_room_mapping_isolation() {
    let store = store::Store::new_in_memory(None)
        .await
        .expect("Failed to create store");

    // Register users to satisfy foreign key constraints
    store
        .save_user(&jmap_matrix_bridge::store::RegisteredUser {
            matrix_user_id: "@alice:localhost".to_string(),
            jmap_username: "alice".to_string(),
            jmap_token: "secret".to_string(),
            jmap_url: "http://localhost".to_string(),
        })
        .await
        .unwrap();
    store
        .save_user(&jmap_matrix_bridge::store::RegisteredUser {
            matrix_user_id: "@charlie:localhost".to_string(),
            jmap_username: "charlie".to_string(),
            jmap_token: "secret".to_string(),
            jmap_url: "http://localhost".to_string(),
        })
        .await
        .unwrap();

    // Add room ghost mapping for User A
    store
        .save_room_ghost_mapping(
            "!room_alice:localhost",
            "bob@example.com",
            "@alice:localhost",
        )
        .await
        .expect("Failed to save mapping");

    // Add room ghost mapping for User B
    store
        .save_room_ghost_mapping(
            "!room_charlie:localhost",
            "bob@example.com",
            "@charlie:localhost",
        )
        .await
        .expect("Failed to save mapping");

    // Query room for Alice
    let room_alice = store
        .get_room_by_ghost("bob@example.com", "@alice:localhost")
        .await
        .expect("Failed to get room")
        .expect("Expected mapping for Alice");
    assert_eq!(room_alice, "!room_alice:localhost");

    // Query room for Charlie
    let room_charlie = store
        .get_room_by_ghost("bob@example.com", "@charlie:localhost")
        .await
        .expect("Failed to get room")
        .expect("Expected mapping for Charlie");
    assert_eq!(room_charlie, "!room_charlie:localhost");

    // Query room for non-existent mapping
    let room_nonexistent = store
        .get_room_by_ghost("bob@example.com", "@nobody:localhost")
        .await
        .expect("Failed to get room");
    assert!(room_nonexistent.is_none());
}
