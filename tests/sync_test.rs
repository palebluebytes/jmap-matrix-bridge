#![allow(
    clippy::unwrap_used,
    clippy::str_to_string,
    clippy::too_many_lines,
    clippy::unreadable_literal,
    clippy::uninlined_format_args
)]

use jmap_matrix_bridge::ingest::JmapPoller;
use jmap_matrix_bridge::matrix::MatrixClient;
use jmap_matrix_bridge::store::{Store, ThreadRepository};
use std::sync::Arc;
use wiremock::matchers::{body_string_contains, header, method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn test_poll_hits_jmap_and_matrix_endpoints() {
    let mock_server = MockServer::start().await;

    // Mock JMAP session discovery
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
             "username": "user",
             "accounts": {},
             "primaryAccounts": {},
             "apiUrl": format!("{}/api", mock_server.uri()),
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

    // Mock Mailbox/query (called first by poll -> sync_mailboxes)
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(|request: &wiremock::Request| {
            let json: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let method_calls = json.get("methodCalls").unwrap().as_array().unwrap();
            method_calls[0].as_array().unwrap()[0].as_str().unwrap() == "Mailbox/query"
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [["Mailbox/query", {"accountId": "A123", "ids": [], "queryState": "s1", "canCalculateChanges": false, "position": 0}, "0"]]
        })))
        .mount(&mock_server)
        .await;

    // Mock Mailbox/get (called during bootstrap changesState calculation)
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(|request: &wiremock::Request| {
            let json: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let method_calls = json.get("methodCalls").unwrap().as_array().unwrap();
            method_calls[0].as_array().unwrap()[0].as_str().unwrap() == "Mailbox/get"
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [["Mailbox/get", {"list": [], "accountId": "A123", "state": "s1", "notFound": []}, "0"]]
        })))
        .mount(&mock_server)
        .await;

    // Mock Email/query (called by poll -> sync_emails). Returns a real id: an
    // empty `ids` list short-circuits the `if !email_ids.is_empty()` guard in
    // sync_emails, so the entire email→Matrix path never runs and the Matrix
    // mocks below become unreachable — which is what this test used to do.
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(|request: &wiremock::Request| {
            let json: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let method_calls = json.get("methodCalls").unwrap().as_array().unwrap();
            method_calls[0].as_array().unwrap()[0].as_str().unwrap() == "Email/query"
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [["Email/query", {"accountId": "A123", "ids": ["E1"], "queryState": "s1", "canCalculateChanges": false, "position": 0}, "0"]]
        })))
        .expect(1)
        .mount(&mock_server)
        .await;

    // Mock the Email/get that FETCHES the email (ids: ["E1"]). Mounted before the
    // bootstrap mock below: sync_emails issues two distinct Email/get calls — this
    // one, and a bootstrap with `ids: []` purely to read the changes state — and
    // they need different responses.
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(|request: &wiremock::Request| {
            let json: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let method_calls = json.get("methodCalls").unwrap().as_array().unwrap();
            let call = method_calls[0].as_array().unwrap();
            call[0].as_str().unwrap() == "Email/get"
                && call[1]
                    .get("ids")
                    .and_then(|ids| ids.as_array())
                    .is_some_and(|ids| !ids.is_empty())
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [["Email/get", {
                "accountId": "A123",
                "state": "s1",
                "notFound": [],
                "list": [{
                    "id": "E1",
                    "threadId": "T1",
                    "subject": "Hello from Alice",
                    "from": [{"name": "Alice", "email": "alice@example.com"}],
                    "to": [{"email": "user@example.com"}],
                    "receivedAt": "2026-01-01T00:00:00Z",
                    "textBody": [{"partId": "b1", "type": "text/plain"}],
                    "bodyValues": {"b1": {"value": "Hello world from Alice"}}
                }]
            }, "0"]]
        })))
        .expect(1)
        .mount(&mock_server)
        .await;

    // Mock the Email/get state bootstrap (ids: []).
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(|request: &wiremock::Request| {
            let json: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let method_calls = json.get("methodCalls").unwrap().as_array().unwrap();
            method_calls[0].as_array().unwrap()[0].as_str().unwrap() == "Email/get"
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [["Email/get", {"list": [], "accountId": "A123", "state": "s1", "notFound": []}, "0"]]
        })))
        .mount(&mock_server)
        .await;

    // Mock Matrix ensure_user_exists — registering the ghost for alice@example.com.
    Mock::given(method("POST"))
        .and(path("/_matrix/client/v3/register"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .expect(1..)
        .mount(&mock_server)
        .await;

    // Mock Matrix createRoom — the email space and the contact room.
    Mock::given(method("POST"))
        .and(path("/_matrix/client/v3/createRoom"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "room_id": "!room1:localhost"
        })))
        .expect(1..)
        .mount(&mock_server)
        .await;

    // THE ORACLE this test is named for: the email's body must actually reach a
    // Matrix room. Pinning the rendered content proves the whole JMAP→Matrix path
    // ran — not merely that poll() returned Ok, which it does even when every
    // email fails to bridge (sync/email.rs swallows per-email errors by design).
    Mock::given(method("PUT"))
        .and(body_string_contains("Hello world from Alice"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "event_id": "$msg1" })),
        )
        .expect(1)
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
        store.clone(),
        10,
        true,
        jmap_matrix_bridge::services::content::RenderMode::default(),
    );

    // poll() should run without error, hitting JMAP endpoints and the mock demo section
    let result = poller.poll().await;
    assert!(result.is_ok(), "poll() should succeed with all mocks");
}

#[tokio::test]
async fn test_poll_respects_sync_limit() {
    let mock_server = MockServer::start().await;

    // Mock JMAP session discovery
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
             "username": "user",
             "accounts": {},
             "primaryAccounts": {},
             "apiUrl": format!("{}/api", mock_server.uri()),
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

    // Mock Mailbox/query
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(|request: &wiremock::Request| {
            let json: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let method_calls = json.get("methodCalls").unwrap().as_array().unwrap();
            method_calls[0].as_array().unwrap()[0].as_str().unwrap() == "Mailbox/query"
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [["Mailbox/query", {"accountId": "A123", "ids": [], "queryState": "s1", "canCalculateChanges": false, "position": 0}, "0"]]
        })))
        .mount(&mock_server)
        .await;

    // Mock Mailbox/get
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(|request: &wiremock::Request| {
            let json: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let method_calls = json.get("methodCalls").unwrap().as_array().unwrap();
            method_calls[0].as_array().unwrap()[0].as_str().unwrap() == "Mailbox/get"
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [["Mailbox/get", {"list": [], "accountId": "A123", "state": "s1", "notFound": []}, "0"]]
        })))
        .mount(&mock_server)
        .await;

    // Mock Email/query and VERIFY LIMIT
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(|request: &wiremock::Request| {
            let json: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let method_calls = json.get("methodCalls").unwrap().as_array().unwrap();
            for call in method_calls {
                let arr = call.as_array().unwrap();
                if arr[0].as_str().unwrap() == "Email/query" {
                    let args = arr[1].as_object().unwrap();
                    if args.get("limit").unwrap().as_u64().unwrap() == 5 {
                        return true;
                    }
                }
            }
            false
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [["Email/query", {"accountId": "A123", "ids": [], "queryState": "s1", "canCalculateChanges": false, "position": 0}, "0"]]
        })))
        .mount(&mock_server)
        .await;

    // Mock Email/get
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(|request: &wiremock::Request| {
            let json: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let method_calls = json.get("methodCalls").unwrap().as_array().unwrap();
            method_calls[0].as_array().unwrap()[0].as_str().unwrap() == "Email/get"
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [["Email/get", {"list": [], "accountId": "A123", "state": "s1", "notFound": []}, "0"]]
        })))
        .mount(&mock_server)
        .await;

    // Mock Matrix register
    Mock::given(method("POST"))
        .and(path("/_matrix/client/v3/register"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
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

    // Set limit to 5
    let poller = JmapPoller::new(
        "@user:localhost".to_string(),
        Arc::new(client),
        matrix,
        store.clone(),
        5,
        true,
        jmap_matrix_bridge::services::content::RenderMode::default(),
    );

    let result = poller.poll().await;
    assert!(
        result.is_ok(),
        "poll() should succeed when limit is respected"
    );
}

#[tokio::test]
async fn test_poll_handles_html_email() {
    let mock_server = MockServer::start().await;

    // Mock JMAP session discovery
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
             "username": "user",
             "accounts": {},
             "primaryAccounts": {},
             "apiUrl": format!("{}/api", mock_server.uri()),
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

    // Mock Mailbox/query
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(|request: &wiremock::Request| {
            let json: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let method_calls = json.get("methodCalls").unwrap().as_array().unwrap();
            method_calls[0].as_array().unwrap()[0].as_str().unwrap() == "Mailbox/query"
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [["Mailbox/query", {"accountId": "A123", "ids": [], "queryState": "s1", "canCalculateChanges": false, "position": 0}, "0"]]
        })))
        .mount(&mock_server)
        .await;

    // Mock Mailbox/get
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(|request: &wiremock::Request| {
            let json: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let method_calls = json.get("methodCalls").unwrap().as_array().unwrap();
            method_calls[0].as_array().unwrap()[0].as_str().unwrap() == "Mailbox/get"
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [["Mailbox/get", {"list": [], "accountId": "A123", "state": "s1", "notFound": []}, "0"]]
        })))
        .mount(&mock_server)
        .await;

    // Mock Email/query to return ONE email ID
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(|request: &wiremock::Request| {
            let json: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let method_calls = json.get("methodCalls").unwrap().as_array().unwrap();
            method_calls[0].as_array().unwrap()[0].as_str().unwrap() == "Email/query"
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [["Email/query", {"accountId": "A123", "ids": ["email1"], "queryState": "s1", "canCalculateChanges": false, "position": 0}, "0"]]
        })))
        .mount(&mock_server)
        .await;

    // Mock Email/get to return the HTML email
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(|request: &wiremock::Request| {
            let json: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let method_calls = json.get("methodCalls").unwrap().as_array().unwrap();
            method_calls[0].as_array().unwrap()[0].as_str().unwrap() == "Email/get"
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [["Email/get", {
                "accountId": "A123",
                "state": "s1",
                "list": [
                    {
                        "id": "email1",
                        "threadId": "thread1",
                        "mailboxIds": {"inbox": true},
                        "subject": "Test HTML",
                        "from": [{"name": "User", "email": "user@example.com"}],
                        "htmlBody": [{"partId": "p1", "type": "text/html"}],
                        "bodyValues": {
                            "p1": {"value": "Hello <b>world</b>!", "isTruncated": false}
                        },
                        "receivedAt": "2023-01-01T00:00:00Z"
                    }
                ],
                "notFound": []
            }, "0"]]
        })))
        .mount(&mock_server)
        .await;

    // Mock Matrix create room
    Mock::given(method("POST"))
        .and(path("/_matrix/client/v3/createRoom"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "room_id": "!room1"
        })))
        .mount(&mock_server)
        .await;

    // Mock Matrix join room
    Mock::given(method("POST"))
        .and(path("/_matrix/client/v3/rooms/!room1/join"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&mock_server)
        .await;

    // Mock Matrix send message and VERIFY TEXT
    Mock::given(method("POST"))
        .and(|request: &wiremock::Request| {
            request
                .url
                .path()
                .starts_with("/_matrix/client/v3/rooms/!room1/send/m.room.message/")
        })
        .and(|request: &wiremock::Request| {
            let json: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let body = json.get("body").unwrap().as_str().unwrap();
            // html2text converts "Hello <b>world</b>!" to "Hello world!"
            body.trim() == "Hello world!"
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "event_id": "$event1"
        })))
        .mount(&mock_server)
        .await;

    // Mock Matrix register
    Mock::given(method("POST"))
        .and(path("/_matrix/client/v3/register"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
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
        store.clone(),
        10,
        true,
        jmap_matrix_bridge::services::content::RenderMode::default(),
    );

    let result = poller.poll().await;
    if let Err(e) = &result {
        println!("Poll error: {:?}", e);
    }
    assert!(
        result.is_ok(),
        "poll() should succeed when handling HTML email"
    );
}

#[tokio::test]
async fn test_poll_handles_attachments() {
    let mock_server = MockServer::start().await;

    // Mock Mailbox/query
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(|request: &wiremock::Request| {
            let json: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let method_calls = json.get("methodCalls").unwrap().as_array().unwrap();
            method_calls[0].as_array().unwrap()[0].as_str().unwrap() == "Mailbox/query"
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [["Mailbox/query", {"accountId": "A123", "ids": [], "queryState": "s1", "canCalculateChanges": false, "position": 0}, "0"]]
        })))
        .mount(&mock_server)
        .await;

    // Mock Mailbox/get
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(|request: &wiremock::Request| {
            let json: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let method_calls = json.get("methodCalls").unwrap().as_array().unwrap();
            method_calls[0].as_array().unwrap()[0].as_str().unwrap() == "Mailbox/get"
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [["Mailbox/get", {"list": [], "accountId": "A123", "state": "s1", "notFound": []}, "0"]]
        })))
        .mount(&mock_server)
        .await;

    // Mock Email/query to return ONE email ID
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(|request: &wiremock::Request| {
            let json: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let method_calls = json.get("methodCalls").unwrap().as_array().unwrap();
            method_calls[0].as_array().unwrap()[0].as_str().unwrap() == "Email/query"
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [["Email/query", {"accountId": "A123", "ids": ["email1"], "queryState": "s1", "canCalculateChanges": false, "position": 0}, "0"]]
        })))
        .mount(&mock_server)
        .await;

    // Mock Email/get to return email with attachments
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(|request: &wiremock::Request| {
            let json: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let method_calls = json.get("methodCalls").unwrap().as_array().unwrap();
            method_calls[0].as_array().unwrap()[0].as_str().unwrap() == "Email/get"
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [["Email/get", {
                "accountId": "A123",
                "state": "s1",
                "list": [{
                    "id": "email1",
                    "threadId": "thread1",
                    "mailboxIds": {"inbox": true},
                    "subject": "Test with attachment",
                    "from": [{"name": "Sender", "email": "sender@example.com"}],
                    "textBody": [{"partId": "p1", "type": "text/plain"}],
                    "bodyValues": {
                        "p1": {"value": "See attached file", "isTruncated": false}
                    },
                    "attachments": [{
                        "partId": "a1",
                        "blobId": "blob1",
                        "type": "image/png",
                        "name": "test.png",
                        "size": 123
                    }],
                    "receivedAt": "2023-01-01T00:00:00Z"
                }],
                "notFound": []
            }, "0"]]
        })))
        .mount(&mock_server)
        .await;

    // Mock Download Endpoint. The blob download must use Basic auth (the bridge
    // connects to JMAP with Basic credentials); requiring it here makes the test
    // fail if the download reverts to Bearer, which Stalwart rejects with 401.
    Mock::given(method("GET"))
        .and(path("/download/A123/blob1/test.png"))
        .and(header("Authorization", "Basic dXNlcjp0b2tlbg=="))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![1, 2, 3, 4]))
        .expect(1)
        .mount(&mock_server)
        .await;

    // Mock Matrix Upload
    Mock::given(method("POST"))
        .and(path("/_matrix/media/v3/upload"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "content_uri": "mxc://localhost/media1"
        })))
        .expect(1)
        .mount(&mock_server)
        .await;

    // Mock Matrix Send File
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "event_id": "$event_id"
        })))
        .mount(&mock_server)
        .await;

    // Mock Matrix create room
    Mock::given(method("POST"))
        .and(path("/_matrix/client/v3/createRoom"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "room_id": "!room1"
        })))
        .mount(&mock_server)
        .await;

    // Mock Matrix join room
    Mock::given(method("POST"))
        .and(path("/_matrix/client/v3/rooms/!room1/join"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&mock_server)
        .await;

    // Mock Matrix ensure_user_exists
    Mock::given(method("GET"))
        .and(path(
            "/_matrix/client/v3/profile/@_jmap_sender=40example.com:localhost",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&mock_server)
        .await;

    Mock::given(method("POST"))
        .and(path("/_matrix/client/v3/register"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&mock_server)
        .await;

    // Mock JMAP session discovery
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
             "username": "user",
             "accounts": {
                 "A123": { "name": "user", "isPersonal": true, "isReadOnly": false, "accountCapabilities": { "urn:ietf:params:jmap:mail": {} } }
             },
             // Real Stalwart advertises the primary account under the `mail`
             // capability and leaves `core` empty. Attachment bridging must
             // resolve the account via the session's default/primary account,
             // not a hard-coded `core` lookup. With the account under `core`
             // here the old code passed while production failed with
             // "No account"; pinning it to `mail` makes the .expect(1) download
             // and upload mocks a real regression guard.
             "primaryAccounts": { "urn:ietf:params:jmap:mail": "A123" },
             "apiUrl": format!("{}/api", mock_server.uri()),
             "downloadUrl": format!("{}/download/{{accountId}}/{{blobId}}/{{name}}", mock_server.uri()),
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

    let store = Store::new_in_memory(None).await.unwrap();
    let matrix = MatrixClient::new(&mock_server.uri(), "token", "localhost")
        .await
        .unwrap();

    // Save user in store so handle_attachments can find it
    let user = jmap_matrix_bridge::store::RegisteredUser {
        matrix_user_id: "@user:localhost".to_string(),
        jmap_username: "user".to_string(),
        jmap_token: "token".to_string(),
        jmap_url: mock_server.uri(),
    };
    store.save_user(&user).await.unwrap();

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
        store.clone(),
        10,
        true,
        jmap_matrix_bridge::services::content::RenderMode::default(),
    );

    let result = poller.poll().await;
    assert!(
        result.is_ok(),
        "poll() should succeed when handling attachments"
    );
}

/// Regression for the "new email shows as already read / last message is
/// '<user> joined the room'" bug. Both symptoms come from the real user joining
/// the room AFTER the email is posted: the message is then pre-join history (not
/// unread) and the join is the newest event. The fix pre-joins the real user via
/// their double-puppet token, synchronously, before the first message is sent —
/// so here the puppet `/join` MUST reach Matrix before the message PUT.
#[tokio::test]
async fn test_new_thread_joins_real_user_before_posting_message() {
    const PUPPET_TOKEN: &str = "puppet-tok-123";
    let mock_server = MockServer::start().await;

    // JMAP session discovery.
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
             "username": "user",
             "accounts": {},
             "primaryAccounts": {},
             "apiUrl": format!("{}/api", mock_server.uri()),
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

    // Mailbox/query (sync_mailboxes runs first) -> no mailboxes.
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(body_string_contains("Mailbox/query"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [["Mailbox/query", {"accountId": "A123", "ids": [], "queryState": "s1", "canCalculateChanges": false, "position": 0}, "0"]]
        })))
        .mount(&mock_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(body_string_contains("Mailbox/get"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [["Mailbox/get", {"list": [], "accountId": "A123", "state": "s1", "notFound": []}, "0"]]
        })))
        .mount(&mock_server)
        .await;

    // Email/query returns one email id.
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(body_string_contains("Email/query"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [["Email/query", {"accountId": "A123", "ids": ["E1"], "queryState": "s1", "canCalculateChanges": false, "position": 0}, "0"]]
        })))
        .mount(&mock_server)
        .await;

    // Email/get that fetches the email content (ids non-empty).
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(|request: &wiremock::Request| {
            let json: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let call = json["methodCalls"].as_array().unwrap()[0]
                .as_array()
                .unwrap();
            call[0].as_str().unwrap() == "Email/get"
                && call[1]["ids"].as_array().is_some_and(|ids| !ids.is_empty())
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [["Email/get", {
                "accountId": "A123",
                "state": "s1",
                "notFound": [],
                "list": [{
                    "id": "E1",
                    "threadId": "T1",
                    "subject": "Hello from Alice",
                    "from": [{"name": "Alice", "email": "alice@example.com"}],
                    "to": [{"email": "user@example.com"}],
                    "receivedAt": "2026-01-01T00:00:00Z",
                    "textBody": [{"partId": "b1", "type": "text/plain"}],
                    "bodyValues": {"b1": {"value": "Hello world from Alice"}}
                }]
            }, "0"]]
        })))
        .mount(&mock_server)
        .await;

    // Email/get state bootstrap (ids: []).
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(body_string_contains("Email/get"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [["Email/get", {"list": [], "accountId": "A123", "state": "s1", "notFound": []}, "0"]]
        })))
        .mount(&mock_server)
        .await;

    // Ghost registration + room creation.
    Mock::given(method("POST"))
        .and(path("/_matrix/client/v3/register"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&mock_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/_matrix/client/v3/createRoom"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "room_id": "!room1:localhost"
        })))
        .mount(&mock_server)
        .await;
    // Any /join (ghost's appservice join AND the puppet's real-user join).
    Mock::given(method("POST"))
        .and(path("/_matrix/client/v3/rooms/!room1:localhost/join"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "room_id": "!room1:localhost"
        })))
        .mount(&mock_server)
        .await;
    // Room name/topic/space state events.
    Mock::given(method("PUT"))
        .and(path_regex(r".*/state/.*"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "event_id": "$st" })),
        )
        .mount(&mock_server)
        .await;
    // The email message itself.
    Mock::given(method("PUT"))
        .and(body_string_contains("Hello world from Alice"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "event_id": "$msg1" })),
        )
        .expect(1)
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
    // Stored double-puppet token: the pre-join path only fires when one exists.
    store
        .set_matrix_puppet_token("@user:localhost", PUPPET_TOKEN)
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
        store.clone(),
        10,
        true,
        jmap_matrix_bridge::services::content::RenderMode::default(),
    );

    poller.poll().await.expect("poll should succeed");

    // ORACLE: the real user's puppet join (bearer = PUPPET_TOKEN) must be recorded
    // BEFORE the message PUT. Under the old code the real user never joins in-band,
    // so the puppet join is absent entirely and this fails.
    let reqs = mock_server.received_requests().await.unwrap();
    let puppet_join_idx = reqs.iter().position(|r| {
        r.url.path() == "/_matrix/client/v3/rooms/!room1:localhost/join"
            && r.headers
                .get("authorization")
                .is_some_and(|v| v.to_str().unwrap_or("") == format!("Bearer {PUPPET_TOKEN}"))
    });
    let message_idx = reqs
        .iter()
        .position(|r| String::from_utf8_lossy(&r.body).contains("Hello world from Alice"));

    let puppet_join_idx =
        puppet_join_idx.expect("real user must be pre-joined via the puppet token");
    let message_idx = message_idx.expect("the email message must be posted");
    assert!(
        puppet_join_idx < message_idx,
        "real user must join (req #{puppet_join_idx}) before the email is posted (req #{message_idx})"
    );
}

/// A mail that is ALREADY read in the client ($seen) when the bridge first
/// ingests it — the common case for backfilled/initial-sync history — must have
/// its read state mirrored to Matrix immediately, so it doesn't sit falsely
/// unread. Oracle: an `m.read` receipt is sent as the user's double-puppet
/// (bearer = puppet token) for the just-bridged event. Without the first-bridge
/// mirror this receipt is never sent (Email/changes never re-surfaces unchanged
/// history), so the test fails on the old code.
#[tokio::test]
async fn test_seen_email_gets_read_receipt_on_first_bridge() {
    const PUPPET_TOKEN: &str = "puppet-tok-seen";
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
             "username": "user",
             "accounts": {},
             "primaryAccounts": {},
             "apiUrl": format!("{}/api", mock_server.uri()),
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

    Mock::given(method("POST"))
        .and(path("/api"))
        .and(body_string_contains("Mailbox/query"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [["Mailbox/query", {"accountId": "A123", "ids": [], "queryState": "s1", "canCalculateChanges": false, "position": 0}, "0"]]
        })))
        .mount(&mock_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(body_string_contains("Mailbox/get"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [["Mailbox/get", {"list": [], "accountId": "A123", "state": "s1", "notFound": []}, "0"]]
        })))
        .mount(&mock_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(body_string_contains("Email/query"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [["Email/query", {"accountId": "A123", "ids": ["E1"], "queryState": "s1", "canCalculateChanges": false, "position": 0}, "0"]]
        })))
        .mount(&mock_server)
        .await;

    // Email/get for the content — note keywords carry $seen (already read).
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(|request: &wiremock::Request| {
            let json: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let call = json["methodCalls"].as_array().unwrap()[0]
                .as_array()
                .unwrap();
            call[0].as_str().unwrap() == "Email/get"
                && call[1]["ids"].as_array().is_some_and(|ids| !ids.is_empty())
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [["Email/get", {
                "accountId": "A123",
                "state": "s1",
                "notFound": [],
                "list": [{
                    "id": "E1",
                    "threadId": "T1",
                    "subject": "Already read",
                    "keywords": {"$seen": true},
                    "from": [{"name": "Alice", "email": "alice@example.com"}],
                    "to": [{"email": "user@example.com"}],
                    "receivedAt": "2026-01-01T00:00:00Z",
                    "textBody": [{"partId": "b1", "type": "text/plain"}],
                    "bodyValues": {"b1": {"value": "Seen mail body"}}
                }]
            }, "0"]]
        })))
        .mount(&mock_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(body_string_contains("Email/get"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [["Email/get", {"list": [], "accountId": "A123", "state": "s1", "notFound": []}, "0"]]
        })))
        .mount(&mock_server)
        .await;

    Mock::given(method("POST"))
        .and(path("/_matrix/client/v3/register"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&mock_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/_matrix/client/v3/createRoom"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "room_id": "!room1:localhost"
        })))
        .mount(&mock_server)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(r".*/rooms/.*/join"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "room_id": "!room1:localhost"
        })))
        .mount(&mock_server)
        .await;
    Mock::given(method("PUT"))
        .and(path_regex(r".*/state/.*"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "event_id": "$st" })),
        )
        .mount(&mock_server)
        .await;
    // The email message; returns the event id the read receipt must target.
    Mock::given(method("PUT"))
        .and(body_string_contains("Seen mail body"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "event_id": "$msg1" })),
        )
        .mount(&mock_server)
        .await;
    // THE ORACLE endpoint: an m.read receipt for the bridged event.
    Mock::given(method("POST"))
        .and(path_regex(r".*/receipt/m\.read/.*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .expect(1)
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
    store
        .set_matrix_puppet_token("@user:localhost", PUPPET_TOKEN)
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
        store.clone(),
        10,
        true,
        jmap_matrix_bridge::services::content::RenderMode::default(),
    );

    poller.poll().await.expect("poll should succeed");

    // The receipt must target the bridged event and carry the puppet token.
    let reqs = mock_server.received_requests().await.unwrap();
    let receipt = reqs.iter().find(|r| {
        r.url.path().contains("/receipt/m.read/")
            && r.headers
                .get("authorization")
                .is_some_and(|v| v.to_str().unwrap_or("") == format!("Bearer {PUPPET_TOKEN}"))
    });
    let receipt = receipt.expect("a $seen email must get an m.read receipt on first bridge");
    assert!(
        receipt.url.path().contains("!room1:localhost"),
        "receipt must target the bridged room, got {}",
        receipt.url.path()
    );
}

/// Reverse read-state (direction B): an already-bridged email that was mirrored
/// read, then loses `$seen` in the mailbox (marked unread in another client),
/// must flag its room `m.marked_unread` in Matrix. Driven through the
/// `Email/changes` path with the email pre-mapped and the read gate pre-set, so
/// `process_email` takes the already-mapped branch into `sync_read_state`.
/// Oracle: a PUT of `m.marked_unread` `{unread:true}` as the puppet. Absent on
/// the old code (which only mirrored the seen direction), so it fails without B.
#[tokio::test]
async fn test_email_losing_seen_marks_room_unread_in_matrix() {
    const PUPPET_TOKEN: &str = "puppet-tok-unread";
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
             "username": "user",
             "accounts": {},
             "primaryAccounts": {},
             "apiUrl": format!("{}/api", mock_server.uri()),
             "downloadUrl": "http://127.0.0.1/download",
             "uploadUrl": "http://127.0.0.1/upload",
             "eventSourceUrl": "http://127.0.0.1/events",
             "capabilities": { "urn:ietf:params:jmap:core": {}, "urn:ietf:params:jmap:mail": {} },
             "state": "s1"
        })))
        .mount(&mock_server)
        .await;
    // sync_mailboxes (runs first) -> no mailboxes.
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(body_string_contains("Mailbox/query"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [["Mailbox/query", {"accountId": "A123", "ids": [], "queryState": "s1", "canCalculateChanges": false, "position": 0}, "0"]]
        })))
        .mount(&mock_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(body_string_contains("Mailbox/get"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [["Mailbox/get", {"list": [], "accountId": "A123", "state": "s1", "notFound": []}, "0"]]
        })))
        .mount(&mock_server)
        .await;
    // Email/changes reports E1 updated (its $seen was just cleared).
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(body_string_contains("Email/changes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [["Email/changes", {
                "accountId": "A123", "oldState": "state0", "newState": "state1",
                "hasMoreChanges": false, "created": [], "updated": ["E1"], "destroyed": []
            }, "0"]]
        })))
        .mount(&mock_server)
        .await;
    // Email/get for E1 — keywords now EMPTY (no $seen).
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(body_string_contains("Email/get"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [["Email/get", {
                "accountId": "A123", "state": "state1", "notFound": [],
                "list": [{
                    "id": "E1", "threadId": "T1", "subject": "Was read",
                    "keywords": {},
                    "from": [{"email": "alice@example.com"}],
                    "receivedAt": "2026-01-01T00:00:00Z"
                }]
            }, "0"]]
        })))
        .mount(&mock_server)
        .await;
    // THE ORACLE: m.marked_unread set true for the room, as the puppet.
    Mock::given(method("PUT"))
        .and(path_regex(r".*/account_data/m\.marked_unread$"))
        .and(header("authorization", &*format!("Bearer {PUPPET_TOKEN}")))
        .and(body_string_contains("\"unread\":true"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .expect(1)
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
    store
        .set_matrix_puppet_token("@user:localhost", PUPPET_TOKEN)
        .await
        .unwrap();
    // E1 is already bridged into !room1, and we previously mirrored it as read.
    store
        .save_thread_mapping_atomic("T1", "$evt:localhost", "!room1:localhost", "Was read")
        .await
        .unwrap();
    store
        .save_message_mapping("E1", "$evt:localhost")
        .await
        .unwrap();
    store
        .save_jmap_state("@user:localhost", "read_synced:E1", "1")
        .await
        .unwrap();
    // Put the poller on the Email/changes path (not the initial query).
    store
        .save_jmap_state("@user:localhost", "changes", "state0")
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
        store.clone(),
        10,
        true,
        jmap_matrix_bridge::services::content::RenderMode::default(),
    );

    poller.poll().await.expect("poll should succeed");
    // Mock `.expect(1)` verifies the marked_unread PUT landed on drop.
}
