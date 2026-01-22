use wiremock::{MockServer, Mock, ResponseTemplate};
use wiremock::matchers::{method, path};
use serde_json::json;
use std::sync::Arc;
use jmap_matrix_bridge::{ingest::JmapPoller, matrix::MatrixClient, store::Store};

async fn mock_jmap_session(server: &MockServer) {
    let url = server.uri();
    Mock::given(method("GET"))
        .and(path("/.well-known/jmap"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "username": "user",
            "primaryAccounts": {
                "urn:ietf:params:jmap:mail": "A123"
            },
            "apiUrl": format!("{}/api", url),
            "downloadUrl": format!("{}/download", url),
            "uploadUrl": format!("{}/upload", url),
            "eventSourceUrl": format!("{}/events", url),
            "capabilities": {
                "urn:ietf:params:jmap:core": {},
                "urn:ietf:params:jmap:mail": {}
            },
            "state": "s1",
             "accounts": {
                "A123": { "name": "user", "isPersonal": true, "isReadOnly": false, "accountCapabilities": { "urn:ietf:params:jmap:mail": {} } }
            }
        })))
        .mount(server)
        .await;
}

#[tokio::test]
async fn test_mailbox_sync() {
    let mock_server = MockServer::start().await;
    mock_jmap_session(&mock_server).await;

    // Mock Mailbox Query
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(wiremock::matchers::body_string_contains("Mailbox/query"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "sessionState": "s1",
            "methodResponses": [
                ["Mailbox/query", { "accountId": "A123", "ids": ["mb1"], "position": 0, "total": 1 }, "0"]
            ]
        })))
        .mount(&mock_server)
        .await;

    // Mock Mailbox Get
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(wiremock::matchers::body_string_contains("Mailbox/get"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "sessionState": "s1",
            "methodResponses": [
                ["Mailbox/get", { 
                    "accountId": "A123", 
                    "state": "s1", 
                    "list": [{ "id": "mb1", "name": "Inbox", "role": "inbox" }] 
                }, "0"]
            ]
        })))
        .mount(&mock_server)
        .await;

    // Mock Matrix Create Room
    Mock::given(method("POST"))
        .and(path("/_matrix/client/v3/createRoom"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "room_id": "!room:localhost"
        })))
        .mount(&mock_server)
        .await;

    // Setup Components
    let store = Store::new_in_memory().await.unwrap();
    let client = jmap_client::client::Client::new()
        .credentials(jmap_client::client::Credentials::Basic("u:p".to_string()))
        .connect(&mock_server.uri())
        .await
        .unwrap();
    let matrix = MatrixClient::new(&mock_server.uri(), "token");
    let poller = JmapPoller::new(Arc::new(client), matrix, store.clone()).await.unwrap();

    // Execute
    poller.sync_mailboxes().await.unwrap();

    // Verify Store
    let room_id = store.get_room_id("mb1").await.unwrap();
    assert_eq!(room_id, Some("!room:localhost".to_string()));
}

#[tokio::test]
async fn test_email_sync() {
    let mock_server = MockServer::start().await;
    mock_jmap_session(&mock_server).await;

    // 1. Setup Mailbox Query & Get (Prerequisite for room lookup)
    // We can either run sync_mailboxes first OR pre-populate the store.
    // Pre-populating store is faster and cleaner for unit tests.
    let store = Store::new_in_memory().await.unwrap();
    store.save_room_mapping("mb1", "!room:localhost").await.unwrap();

    // Mock Email Query
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(wiremock::matchers::body_string_contains("Email/query"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "sessionState": "s1",
            "methodResponses": [
                ["Email/query", { "accountId": "A123", "ids": ["msg1"], "position": 0, "total": 1 }, "0"]
            ]
        })))
        .mount(&mock_server)
        .await;

    // Mock Email Get
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(wiremock::matchers::body_string_contains("Email/get"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "sessionState": "s1",
            "methodResponses": [
                ["Email/get", { 
                    "accountId": "A123", 
                    "state": "s1", 
                    "list": [{ 
                        "id": "msg1", 
                        "threadId": "th1", 
                        "mailboxIds": {"mb1": true}, 
                        "subject": "Hello", 
                        "textBody": [{ "partId": "p1", "value": "World" }] // Updated to match structure ingestion expects
                    }] 
                }, "0"]
            ]
        })))
        .mount(&mock_server)
        .await;

    // Mock Matrix Send Message
    Mock::given(method("PUT")) 
        .and(wiremock::matchers::path_regex(r"^/_matrix/client/v3/rooms/!room:localhost/send/m.room.message/.*$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "event_id": "$event1"
        })))
        .mount(&mock_server)
        .await;

    // Setup Components
    let client = jmap_client::client::Client::new()
        .credentials(jmap_client::client::Credentials::Basic("u:p".to_string()))
        .connect(&mock_server.uri())
        .await
        .unwrap();
    let matrix = MatrixClient::new(&mock_server.uri(), "token");
    let poller = JmapPoller::new(Arc::new(client), matrix, store.clone()).await.unwrap();

    // Execute
    poller.sync_emails().await.unwrap();

    // Verify Store
    // Thread mapping shuld exist
    let thread_info = store.get_thread_info("th1").await.unwrap();
    assert!(thread_info.is_some());
    let (root_event, room_id) = thread_info.unwrap();
    assert_eq!(root_event, "$event1");
    assert_eq!(room_id, "!room:localhost");
    
    // Message mapping should exist (added in last step)
    // We don't have a public get_message_mapping, but we can verify via side effect or add a getter if needed.
    // Ideally we trust it works if thread mapping works, or we add `get_message_event_id` to store for testing.
}
