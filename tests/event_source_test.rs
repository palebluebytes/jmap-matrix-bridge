#![allow(
    clippy::unwrap_used,
    clippy::str_to_string,
    clippy::too_many_lines,
    clippy::unreadable_literal,
    clippy::uninlined_format_args
)]

use jmap_matrix_bridge::ingest::JmapPoller;
use jmap_matrix_bridge::matrix::MatrixClient;
use jmap_matrix_bridge::store::Store;
use std::sync::Arc;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// An empty mailbox must bridge nothing: `poll()` succeeds and creates no ghost,
/// no room, and no message.
///
/// This is the negative half of the `if !email_ids.is_empty()` guard in
/// `sync_emails`; `tests/sync_test.rs` covers the positive half with a real
/// email. The pairing is deliberate — this test used to return empty lists AND
/// assert only that `poll()` was `Ok`, which made it a weaker copy of that test
/// rather than a complement: the Matrix mocks could never fire, so nothing was
/// asserted in either direction. Pinning them at `.expect(0)` is what makes the
/// empty case a real property — it now catches eager space/room creation on
/// every poll.
#[tokio::test]
async fn test_poll_with_empty_mailbox_bridges_nothing() {
    let mock_server = MockServer::start().await;

    // Mock JMAP session discovery
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

    // Mock Mailbox/query - returns empty (no mailboxes)
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(|request: &wiremock::Request| {
            let json: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let method_calls = json.get("methodCalls").unwrap().as_array().unwrap();
            method_calls[0].as_array().unwrap()[0].as_str().unwrap() == "Mailbox/query"
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [["Mailbox/query", {"ids": [], "accountId": "acc1", "queryState": "s1", "position": 0, "total": 0}, "0"]]
        })))
        .mount(&mock_server)
        .await;

    // Mock Mailbox/get - returns empty (bootstrap state)
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(|request: &wiremock::Request| {
            let json: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let method_calls = json.get("methodCalls").unwrap().as_array().unwrap();
            method_calls[0].as_array().unwrap()[0].as_str().unwrap() == "Mailbox/get"
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [["Mailbox/get", {"list": [], "accountId": "acc1", "state": "s1", "notFound": []}, "0"]]
        })))
        .mount(&mock_server)
        .await;

    // Mock Email/query - returns empty (no emails)
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(|request: &wiremock::Request| {
            let json: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let method_calls = json.get("methodCalls").unwrap().as_array().unwrap();
            method_calls[0].as_array().unwrap()[0].as_str().unwrap() == "Email/query"
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [["Email/query", {"ids": [], "accountId": "acc1", "queryState": "s1", "position": 0, "total": 0}, "0"]]
        })))
        .mount(&mock_server)
        .await;

    // Mock Email/get - returns empty (bootstrap state)
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(|request: &wiremock::Request| {
            let json: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let method_calls = json.get("methodCalls").unwrap().as_array().unwrap();
            method_calls[0].as_array().unwrap()[0].as_str().unwrap() == "Email/get"
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [["Email/get", {"list": [], "accountId": "acc1", "state": "s1", "notFound": []}, "0"]]
        })))
        .mount(&mock_server)
        .await;

    // The Matrix mocks below are NEGATIVE assertions: with an empty mailbox,
    // nothing should be bridged, so none of them may ever fire. `.expect(0)` is
    // what states that. Previously they were mounted unpinned and were simply
    // unreachable — present, never hit, asserting nothing either way.

    // Matrix ensure_user_exists — no ghost should be registered.
    Mock::given(method("POST"))
        .and(path("/_matrix/client/v3/register"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .expect(0)
        .mount(&mock_server)
        .await;

    // Matrix createRoom — no space and no contact room should be created.
    Mock::given(method("POST"))
        .and(path("/_matrix/client/v3/createRoom"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "room_id": "!room1:localhost"
        })))
        .expect(0)
        .mount(&mock_server)
        .await;

    // And no message may be sent into any room.
    Mock::given(method("PUT"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "event_id": "$none" })),
        )
        .expect(0)
        .mount(&mock_server)
        .await;

    let store = Store::new_in_memory(None).await.unwrap();
    store
        .save_user(&jmap_matrix_bridge::store::RegisteredUser {
            matrix_user_id: "@user:localhost".to_string(),
            jmap_username: "user".to_string(),
            jmap_token: "secret".to_string(),
            jmap_url: mock_server.uri(),
        })
        .await
        .unwrap();
    let matrix = MatrixClient::new(&mock_server.uri(), "token", "localhost")
        .await
        .unwrap();

    let client = jmap_client::client::Client::new()
        .credentials(jmap_client::client::Credentials::Basic(
            "dXNlcjpwYXNz".to_string(),
        ))
        .connect(&mock_server.uri())
        .await
        .unwrap();

    let poller = JmapPoller::new(
        "@user:localhost".to_string(),
        Arc::new(client),
        matrix,
        store,
        10,
        true,
        jmap_matrix_bridge::services::content::RenderMode::default(),
    );

    poller
        .poll()
        .await
        .expect("poll() should succeed against an empty mailbox");
    // Verification on server drop: every `.expect(0)` above must have stayed at
    // zero. poll() returning Ok proves little on its own — per-email failures are
    // swallowed by design — so the negative expectations are the real assertion.
}
