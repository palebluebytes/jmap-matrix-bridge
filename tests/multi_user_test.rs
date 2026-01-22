use wiremock::{MockServer, Mock, ResponseTemplate};
use wiremock::matchers::{method, path};
use serde_json::json;
use std::sync::Arc;
use jmap_matrix_bridge::{client_manager::ClientManager, events, matrix, store};
use axum::{Router, routing::put};
use tower::util::ServiceExt;
use axum::http::{Request, StatusCode};
use axum::body::Body;

#[tokio::test]
async fn test_multi_user_login_integration() {
    // 1. Setup Mock JMAP Server
    let mock_server = MockServer::start().await;
    let url = mock_server.uri();

    Mock::given(method("GET"))
        .and(path("/jmap/session"))
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
    let store = store::Store::new_in_memory().await.expect("Failed to create store");
    let matrix_client = matrix::MatrixClient::new("http://localhost:8008", "as_token"); // Dummy matrix
    let client_manager = Arc::new(ClientManager::new(store.clone(), matrix_client));
    
    // 3. Setup Router
    let state = events::AppState { client_manager };
    let app = Router::new()
        .route("/transactions/:txn_id", put(events::handle_transactions))
        .with_state(state);

    // 4. Simulate !login command via Matrix Transaction
    let login_cmd = format!("!login testuser secret {}/jmap/session", url);
    let txn_body = json!({
        "txn_id": "txn_mut_1",
        "events": [
            {
                "type": "m.room.message",
                "sender": "@matrix_user:localhost",
                "content": {
                    "msgtype": "m.text",
                    "body": login_cmd
                }
            }
        ]
    });

    let response = app.oneshot(
        Request::builder()
            .method("PUT")
            .uri("/transactions/txn_mut_1")
            .header("Content-Type", "application/json")
            .body(Body::from(serde_json::to_string(&txn_body).unwrap()))
            .unwrap()
    ).await.expect("Failed to send request");

    assert_eq!(response.status(), StatusCode::OK);

    // 5. Verify Persistence
    let users = store.get_all_users().await.expect("Failed to fetch users");
    assert_eq!(users.len(), 1, "Expected 1 user to be registered");
    assert_eq!(users[0].matrix_user_id, "@matrix_user:localhost");
    assert_eq!(users[0].jmap_username, "testuser");
}
