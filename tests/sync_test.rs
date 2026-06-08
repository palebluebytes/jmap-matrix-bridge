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

    // Mock Email/query (called by poll -> sync_emails)
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(|request: &wiremock::Request| {
            let json: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let method_calls = json.get("methodCalls").unwrap().as_array().unwrap();
            method_calls[0].as_array().unwrap()[0].as_str().unwrap() == "Email/query"
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

    // Mock Matrix ensure_user_exists (called by poll mock demo section)
    Mock::given(method("POST"))
        .and(path("/_matrix/client/v3/register"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&mock_server)
        .await;

    // Mock Matrix createRoom (called by poll mock demo section)
    Mock::given(method("POST"))
        .and(path("/_matrix/client/v3/createRoom"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "room_id": "!room1:localhost"
        })))
        .mount(&mock_server)
        .await;

    let store = Store::new_in_memory(None).await.unwrap();
    store.save_user(&jmap_matrix_bridge::store::RegisteredUser {
        matrix_user_id: "@user:localhost".to_string(),
        jmap_username: "user".to_string(),
        jmap_token: "secret".to_string(),
        jmap_url: mock_server.uri(),
    }).await.unwrap();
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
    store.save_user(&jmap_matrix_bridge::store::RegisteredUser {
        matrix_user_id: "@user:localhost".to_string(),
        jmap_username: "user".to_string(),
        jmap_token: "secret".to_string(),
        jmap_url: mock_server.uri(),
    }).await.unwrap();
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
    store.save_user(&jmap_matrix_bridge::store::RegisteredUser {
        matrix_user_id: "@user:localhost".to_string(),
        jmap_username: "user".to_string(),
        jmap_token: "secret".to_string(),
        jmap_url: mock_server.uri(),
    }).await.unwrap();
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

    // Mock Download Endpoint
    Mock::given(method("GET"))
        .and(path("/download/A123/blob1/test.png"))
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
                 "A123": { "name": "user", "isPersonal": true, "isReadOnly": false, "accountCapabilities": { "urn:ietf:params:jmap:core": {} } }
             },
             "primaryAccounts": { "urn:ietf:params:jmap:core": "A123" },
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
    );

    let result = poller.poll().await;
    assert!(
        result.is_ok(),
        "poll() should succeed when handling attachments"
    );
}
