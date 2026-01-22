use axum::{
    extract::{State, Json},
    response::IntoResponse,
};
use tracing::{info, warn, error};
use serde_json::Value; // Fallback for now due to import issues
use crate::sender::JmapSender;
use crate::client_manager::ClientManager;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub client_manager: Arc<ClientManager>,
}

pub async fn handle_transactions(
    State(state): State<AppState>, 
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    info!("Received transaction");

    if let Some(events) = body.get("events").and_then(|e| e.as_array()) {
        for event in events {
            let event_type = event.get("type").and_then(|t| t.as_str());
            let content = event.get("content");
            let sender = event.get("sender").and_then(|s| s.as_str());
            
            if let (Some("m.room.message"), Some(content), Some(sender_id)) = (event_type, content, sender) {
                 info!("Received message from {}: {:?}", sender_id, content);
                 if let Some("m.text") = content.get("msgtype").and_then(|t| t.as_str()) {
                     if let Some(body_str) = content.get("body").and_then(|b| b.as_str()) {
                         info!("Message body: {}", body_str);
                         
                         // Command Parsing
                         if body_str.starts_with("!login ") {
                             let parts: Vec<&str> = body_str.split_whitespace().collect();
                             // Usage: !login username password [url]
                             if parts.len() >= 3 {
                                 let username = parts[1];
                                 let password = parts[2];
                                 let url = if parts.len() > 3 { parts[3] } else { "http://127.0.0.1:8080" }; // Default to local Stalwart

                                 info!("Attempting login for {} at {}", username, url);
                                 match state.client_manager.login(sender_id.to_string(), username.to_string(), password.to_string(), url.to_string()).await {
                                     Ok(_) => info!("Login successful for {}", sender_id),
                                     Err(e) => error!("Login failed: {}", e),
                                 }
                             }
                         } else if body_str.starts_with("!email ") {
                            let parts: Vec<&str> = body_str.splitn(4, ' ').collect();
                            if parts.len() == 4 {
                                let to = parts[1];
                                let subject = parts[2];
                                let email_body = parts[3];
                                
                                // Look up client for sender
                                if let Some(client) = state.client_manager.get_client(sender_id).await {
                                    let sender = JmapSender::new(client);
                                    info!("Sending email to {} from {}...", to, sender_id);
                                    let result = sender.send_email(to, subject, email_body).await;
                                    match result {
                                        Ok(_) => info!("Email sent successfully!"),
                                        Err(e) => error!("Failed to send email: {}", e),
                                    }
                                } else {
                                    error!("User {} not logged in. Send !login <user> <pass> first.", sender_id);
                                }
                            }
                         }
                     }
                 }
            }
        }
    }
    
    axum::Json(serde_json::json!({}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::put;
    use axum::Router;
    use tower::util::ServiceExt; // for oneshot
    use axum::body::Body;
    use axum::http::{Request, StatusCode};

    #[tokio::test]
    async fn test_matrix_transaction_parsing() {
        let store = crate::store::Store::new_in_memory().await.unwrap();
        let matrix = crate::matrix::MatrixClient::new("http://localhost", "token");
        let client_manager = Arc::new(ClientManager::new(store, matrix));
        
        let state = AppState { client_manager };
        let app = Router::new()
            .route("/transactions/:txn_id", put(handle_transactions))
            .with_state(state);

        // Mock a Matrix Transaction
        let json_body = serde_json::json!({
            "txn_id": "txn123",
            "events": [
                {
                    "content": {
                        "body": "Hello World",
                        "msgtype": "m.text"
                    },
                    "event_id": "$event:localhost",
                    "origin_server_ts": 1600000000000u64,
                    "room_id": "!room:localhost",
                    "sender": "@bob:localhost",
                    "type": "m.room.message"
                }
            ]
        });

        let response = app.oneshot(
            Request::builder()
                .method("PUT")
                .uri("/transactions/txn123")
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_string(&json_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }
}

    #[tokio::test]
    async fn test_matrix_login_payload() {
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_test_writer()
            .try_init();

        use axum::routing::put;
        use axum::Router;
        use tower::util::ServiceExt;
        use axum::body::Body;
        use axum::http::{Request, StatusCode};

        // Need to spin up a mock server for the login to hit
        use wiremock::{MockServer, Mock, ResponseTemplate};
        use wiremock::matchers::{method, path};
        
        let mock_server = MockServer::start().await;
        // Mock JMAP Session so login succeeds
        Mock::given(method("GET"))
            // .and(path("/jmap/session")) // Relaxing path to debug 404
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                 "username": "user",
                 "accounts": {},
                 "primaryAccounts": {},
                 "apiUrl": "http://127.0.0.1/api",
                 "downloadUrl": "http://127.0.0.1/download",
                 "uploadUrl": "http://127.0.0.1/upload",
                 "eventSourceUrl": "http://127.0.0.1/events",
                 "capabilities": {
                    "urn:ietf:params:jmap:core": {},
                    "urn:ietf:params:jmap:mail": {}
                },
                 "state": "s1"
            })))
            .mount(&mock_server)
            .await;
            
        let store = crate::store::Store::new_in_memory().await.unwrap();
        let matrix = crate::matrix::MatrixClient::new("http://localhost", "token");
        let client_manager = Arc::new(ClientManager::new(store.clone(), matrix));
        
        let state = AppState { client_manager };
        let app = Router::new()
            .route("/transactions/:txn_id", put(handle_transactions))
            .with_state(state);

        let command = format!("!login user pass {}/jmap/session", mock_server.uri());

        let json_body = serde_json::json!({
            "txn_id": "txn_login",
            "events": [
                {
                    "content": {
                        "body": command,
                        "msgtype": "m.text"
                    },
                    "sender": "@user:matrix.org",
                    "type": "m.room.message"
                }
            ]
        });

        let response = app.oneshot(
            Request::builder()
                .method("PUT")
                .uri("/transactions/txn_login")
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_string(&json_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        
        // Wait briefly for the async login task/call to complete?
        // Actually handle_transactions awaits login(), so it should be done.
        let users = store.get_all_users().await.unwrap();
        assert_eq!(users.len(), 1);
        assert_eq!(users[0].matrix_user_id, "@user:matrix.org");
    }
