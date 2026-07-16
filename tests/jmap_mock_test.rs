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
use wiremock::matchers::{body_string_contains, method, path};
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
    //
    // `submit` makes THREE separate round-trips, not one batch, and each must be
    // answered with a response of its own method type — a catch-all fails with
    // "Response type mismatch" as soon as the client unwraps the first one:
    //
    //   1. Mailbox/query  — resolve the Sent mailbox to file the outgoing copy
    //   2. Identity/get   — resolve the account's From identity
    //   3. Email/set + EmailSubmission/set (batched) — save and submit
    //
    // Match on the method name in the request body to route each to its own
    // response. `expect(1)` on each pins that round-trip: if `submit` stops
    // making one, or makes it twice, the test fails on server drop.

    // Call 1: Mailbox/query — the Sent mailbox.
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(body_string_contains("Mailbox/query"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "sessionState": "s1",
            "methodResponses": [
                ["Mailbox/query", {
                    "accountId": "A123",
                    "queryState": "q1",
                    "canCalculateChanges": false,
                    "position": 0,
                    "ids": ["MB_SENT"]
                }, "0"]
            ]
        })))
        .expect(1)
        .mount(&mock_server)
        .await;

    // Call 2: Identity/get — the From identity bound to the submission.
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(body_string_contains("Identity/get"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "sessionState": "s1",
            "methodResponses": [
                ["Identity/get", {
                    "accountId": "A123",
                    "state": "i1",
                    "list": [{
                        "id": "IDENTITY_1",
                        "name": "Test User",
                        "email": "user@example.com"
                    }],
                    "notFound": []
                }, "0"]
            ]
        })))
        .expect(1)
        .mount(&mock_server)
        .await;

    // Call 3: the batched Email/set + EmailSubmission/set.
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(body_string_contains("Email/set"))
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
        .expect(1)
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
    let result = sender
        .send_email("alice@example.com", "Hello", "Body content", vec![])
        .await;

    assert!(result.is_ok(), "Failed to send email: {:?}", result.err());
    // The id comes from the Email/set `created` entry — proves we unpacked the
    // batch response rather than merely reaching the end without erroring.
    assert_eq!(result.unwrap(), "MSG_NEW");
    // Server drop verifies all three round-trips fired exactly once.
}
