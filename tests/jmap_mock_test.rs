#![allow(
    clippy::unwrap_used,
    clippy::str_to_string,
    clippy::too_many_lines,
    clippy::unreadable_literal,
    clippy::uninlined_format_args
)]

use jmap_matrix_bridge::sender::JmapSender;
use serde_json::json;
use std::sync::Arc;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn test_sender_flow() {
    // 1. Start Mock Server
    let mock_server = MockServer::start().await;
    let url = mock_server.uri();

    // 2. Mock Session Endpoint (.well-known/jmap)
    Mock::given(method("GET"))
        .and(path("/.well-known/jmap"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "username": "user",
            "accounts": {
                "A123": {
                    "name": "user",
                    "isPersonal": true,
                    "isReadOnly": false,
                     "accountCapabilities": {
                        "urn:ietf:params:jmap:core": {},
                        "urn:ietf:params:jmap:mail": {},
                        "urn:ietf:params:jmap:submission": {}
                     }
                }
            },
            "primaryAccounts": {
                "urn:ietf:params:jmap:core": "A123",
                "urn:ietf:params:jmap:mail": "A123",
                "urn:ietf:params:jmap:submission": "A123"
            },
            "apiUrl": format!("{}/api", url),
            "downloadUrl": format!("{}/download", url),
            "uploadUrl": format!("{}/upload", url),
            "eventSourceUrl": format!("{}/events", url),
            "capabilities": {
                "urn:ietf:params:jmap:core": {},
                "urn:ietf:params:jmap:mail": {},
                "urn:ietf:params:jmap:submission": {}
            },
            "state": "s1"
        })))
        .mount(&mock_server)
        .await;

    // 3. Mock API Endpoint (/api)
    // We expect 3 distinct calls or a batch.
    // Call 1: Identity Query (fetch primary identity)
    Mock::given(method("POST"))
        .and(path("/api"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "sessionState": "s1",
            "methodResponses": [
                ["Email/set", { 
                    "accountId": "A123", 
                    "oldState": "s0",
                    "newState": "s1",
                    "created": { "draft": { "id": "MSG_NEW", "blobId": "b1", "threadId": "t1", "size": 100 } },
                    "updated": {},
                    "destroyed": [],
                    "notCreated": {},
                    "notUpdated": {},
                    "notDestroyed": {}
                }, "0"],
                ["EmailSubmission/set", { 
                    "accountId": "A123",
                    "oldState": "s0",
                    "newState": "s1",
                    "created": { "sub": { "id": "SUB_NEW" } },
                    "updated": {},
                    "destroyed": [],
                    "notCreated": {},
                    "notUpdated": {},
                    "notDestroyed": {}
                }, "1"]
            ]
        })))
        .mount(&mock_server)
        .await;

    // 4. Setup Client
    let client = jmap_client::client::Client::new()
        .credentials(jmap_client::client::Credentials::Basic(
            "dXNlcjpwYXNz".to_string(),
        ))
        .connect(&url)
        .await
        .expect("Failed to connect to mock server");

    let client = Arc::new(client);
    let sender = JmapSender::new(client);

    // 5. Test Sending
    // Note: The mock response above is a "catch-all" that returns success for Identity, Email, and Submission.
    // In reality, these might be separate requests. But correct generic matching should work for a happy path.
    // Ideally we match specific method calls in the body, but wiremock body matching is complex.
    // We assume the client makes calls that our catch-all satisfies.

    let result = sender
        .send_email("alice@example.com", "Hello", "Body content", vec![])
        .await;

    assert!(result.is_ok(), "Failed to send email: {:?}", result.err());
}
