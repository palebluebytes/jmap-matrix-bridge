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

async fn setup_mock_server() -> (MockServer, Store, MatrixClient, jmap_client::client::Client) {
    let mock_server = MockServer::start().await;

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

    // Mock Matrix register
    Mock::given(method("POST"))
        .and(path("/_matrix/client/v3/register"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&mock_server)
        .await;

    // Mock Matrix profile check for ghost
    Mock::given(method("GET"))
        .and(path(
            "/_matrix/client/v3/profile/@_jmap_sender=40example.com:localhost",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&mock_server)
        .await;

    // Mock Matrix createRoom
    Mock::given(method("POST"))
        .and(path("/_matrix/client/v3/createRoom"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "room_id": "!room1"
        })))
        .mount(&mock_server)
        .await;

    // Mock Matrix send message (PUT)
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "event_id": "$event_id"
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

    (mock_server, store, matrix, client)
}

#[tokio::test]
async fn test_initial_sync_registers_backfill_when_limit_is_reached() {
    let (mock_server, store, matrix, client) = setup_mock_server().await;

    // Mock Email/query to return a full page of 5 email IDs
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(|request: &wiremock::Request| {
            let json: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let method_calls = json.get("methodCalls").unwrap().as_array().unwrap();
            method_calls[0].as_array().unwrap()[0].as_str().unwrap() == "Email/query"
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [["Email/query", {
                "accountId": "A123",
                "ids": ["e1", "e2", "e3", "e4", "e5"],
                "queryState": "q_state_1",
                "canCalculateChanges": false,
                "position": 0
            }, "0"]]
        })))
        .mount(&mock_server)
        .await;

    // Mock Email/get for the 5 emails
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
                        "id": "e1", "threadId": "t1", "subject": "S1",
                        "from": [{"name": "Sender", "email": "sender@example.com"}],
                        "textBody": [{"partId": "p1", "type": "text/plain"}],
                        "bodyValues": {"p1": {"value": "Body 1"}}
                    },
                    {
                        "id": "e2", "threadId": "t2", "subject": "S2",
                        "from": [{"name": "Sender", "email": "sender@example.com"}],
                        "textBody": [{"partId": "p1", "type": "text/plain"}],
                        "bodyValues": {"p1": {"value": "Body 2"}}
                    },
                    {
                        "id": "e3", "threadId": "t3", "subject": "S3",
                        "from": [{"name": "Sender", "email": "sender@example.com"}],
                        "textBody": [{"partId": "p1", "type": "text/plain"}],
                        "bodyValues": {"p1": {"value": "Body 3"}}
                    },
                    {
                        "id": "e4", "threadId": "t4", "subject": "S4",
                        "from": [{"name": "Sender", "email": "sender@example.com"}],
                        "textBody": [{"partId": "p1", "type": "text/plain"}],
                        "bodyValues": {"p1": {"value": "Body 4"}}
                    },
                    {
                        "id": "e5", "threadId": "t5", "subject": "S5",
                        "from": [{"name": "Sender", "email": "sender@example.com"}],
                        "textBody": [{"partId": "p1", "type": "text/plain"}],
                        "bodyValues": {"p1": {"value": "Body 5"}}
                    }
                ],
                "notFound": []
            }, "0"]]
        })))
        .mount(&mock_server)
        .await;

    let poller = JmapPoller::new(
        "@user:localhost".to_string(),
        Arc::new(client),
        matrix,
        store.clone(),
        5, // sync_limit is 5
    );

    poller.sync_emails().await.unwrap();

    // Verify backfill position was registered at 5
    let backfill_val = store
        .get_jmap_state("@user:localhost", "backfill_position")
        .await
        .unwrap();
    assert_eq!(backfill_val, Some("5".to_string()));
}

#[tokio::test]
async fn test_initial_sync_does_not_register_backfill_when_below_limit() {
    let (mock_server, store, matrix, client) = setup_mock_server().await;

    // Mock Email/query to return only 3 emails (less than the sync_limit of 5)
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(|request: &wiremock::Request| {
            let json: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let method_calls = json.get("methodCalls").unwrap().as_array().unwrap();
            method_calls[0].as_array().unwrap()[0].as_str().unwrap() == "Email/query"
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [["Email/query", {
                "accountId": "A123",
                "ids": ["e1", "e2", "e3"],
                "queryState": "q_state_1",
                "canCalculateChanges": false,
                "position": 0
            }, "0"]]
        })))
        .mount(&mock_server)
        .await;

    // Mock Email/get for the 3 emails
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
                        "id": "e1", "threadId": "t1", "subject": "S1",
                        "from": [{"name": "Sender", "email": "sender@example.com"}],
                        "textBody": [{"partId": "p1", "type": "text/plain"}],
                        "bodyValues": {"p1": {"value": "Body 1"}}
                    },
                    {
                        "id": "e2", "threadId": "t2", "subject": "S2",
                        "from": [{"name": "Sender", "email": "sender@example.com"}],
                        "textBody": [{"partId": "p1", "type": "text/plain"}],
                        "bodyValues": {"p1": {"value": "Body 2"}}
                    },
                    {
                        "id": "e3", "threadId": "t3", "subject": "S3",
                        "from": [{"name": "Sender", "email": "sender@example.com"}],
                        "textBody": [{"partId": "p1", "type": "text/plain"}],
                        "bodyValues": {"p1": {"value": "Body 3"}}
                    }
                ],
                "notFound": []
            }, "0"]]
        })))
        .mount(&mock_server)
        .await;

    let poller = JmapPoller::new(
        "@user:localhost".to_string(),
        Arc::new(client),
        matrix,
        store.clone(),
        5, // sync_limit is 5
    );

    poller.sync_emails().await.unwrap();

    // Verify backfill position was NOT registered
    let backfill_val = store
        .get_jmap_state("@user:localhost", "backfill_position")
        .await
        .unwrap();
    assert_eq!(backfill_val, None);
}

#[tokio::test]
async fn test_backfill_batch_progresses_and_completes() {
    let (mock_server, store, matrix, client) = setup_mock_server().await;

    // Manually register initial backfill position in the database
    store
        .save_jmap_state("@user:localhost", "backfill_position", "5")
        .await
        .unwrap();

    // 1. Mock first backfill batch (position 5, limit 5) -> returns 5 emails (indicates there is more)
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(|request: &wiremock::Request| {
            let json: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let method_calls = json.get("methodCalls").unwrap().as_array().unwrap();
            let call = method_calls[0].as_array().unwrap();
            if call[0].as_str().unwrap() == "Email/query" {
                let args = call[1].as_object().unwrap();
                return args.get("position").unwrap().as_i64().unwrap() == 5;
            }
            false
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [["Email/query", {
                "accountId": "A123",
                "ids": ["e6", "e7", "e8", "e9", "e10"],
                "queryState": "q_state_1",
                "canCalculateChanges": false,
                "position": 5
            }, "0"]]
        })))
        .mount(&mock_server)
        .await;

    // Mock Email/get for the first batch of 5 backfill emails
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(|request: &wiremock::Request| {
            let json: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let method_calls = json.get("methodCalls").unwrap().as_array().unwrap();
            let call = method_calls[0].as_array().unwrap();
            if call[0].as_str().unwrap() == "Email/get" {
                let args = call[1].as_object().unwrap();
                let ids = args.get("ids").unwrap().as_array().unwrap();
                return ids[0].as_str().unwrap() == "e6";
            }
            false
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [["Email/get", {
                "accountId": "A123",
                "state": "s1",
                "list": [
                    {
                        "id": "e6", "threadId": "t6", "subject": "S6",
                        "from": [{"name": "Sender", "email": "sender@example.com"}],
                        "textBody": [{"partId": "p1", "type": "text/plain"}],
                        "bodyValues": {"p1": {"value": "Body 6"}}
                    },
                    {
                        "id": "e7", "threadId": "t7", "subject": "S7",
                        "from": [{"name": "Sender", "email": "sender@example.com"}],
                        "textBody": [{"partId": "p1", "type": "text/plain"}],
                        "bodyValues": {"p1": {"value": "Body 7"}}
                    },
                    {
                        "id": "e8", "threadId": "t8", "subject": "S8",
                        "from": [{"name": "Sender", "email": "sender@example.com"}],
                        "textBody": [{"partId": "p1", "type": "text/plain"}],
                        "bodyValues": {"p1": {"value": "Body 8"}}
                    },
                    {
                        "id": "e9", "threadId": "t9", "subject": "S9",
                        "from": [{"name": "Sender", "email": "sender@example.com"}],
                        "textBody": [{"partId": "p1", "type": "text/plain"}],
                        "bodyValues": {"p1": {"value": "Body 9"}}
                    },
                    {
                        "id": "e10", "threadId": "t10", "subject": "S10",
                        "from": [{"name": "Sender", "email": "sender@example.com"}],
                        "textBody": [{"partId": "p1", "type": "text/plain"}],
                        "bodyValues": {"p1": {"value": "Body 10"}}
                    }
                ],
                "notFound": []
            }, "0"]]
        })))
        .mount(&mock_server)
        .await;

    let poller = JmapPoller::new(
        "@user:localhost".to_string(),
        Arc::new(client),
        matrix,
        store.clone(),
        5, // sync_limit is 5
    );

    // Run first backfill batch
    let has_more = poller.backfill_batch(5).await.unwrap();
    assert!(
        has_more,
        "First backfill batch should indicate there are more emails"
    );

    // Verify backfill position progressed to 10 in the database
    let backfill_val = store
        .get_jmap_state("@user:localhost", "backfill_position")
        .await
        .unwrap();
    assert_eq!(backfill_val, Some("10".to_string()));

    // 2. Mock second backfill batch (position 10, limit 5) -> returns only 2 emails (indicates completion)
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(|request: &wiremock::Request| {
            let json: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let method_calls = json.get("methodCalls").unwrap().as_array().unwrap();
            let call = method_calls[0].as_array().unwrap();
            if call[0].as_str().unwrap() == "Email/query" {
                let args = call[1].as_object().unwrap();
                return args.get("position").unwrap().as_i64().unwrap() == 10;
            }
            false
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [["Email/query", {
                "accountId": "A123",
                "ids": ["e11", "e12"],
                "queryState": "q_state_1",
                "canCalculateChanges": false,
                "position": 10
            }, "0"]]
        })))
        .mount(&mock_server)
        .await;

    // Mock Email/get for the second batch of backfill emails
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(|request: &wiremock::Request| {
            let json: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let method_calls = json.get("methodCalls").unwrap().as_array().unwrap();
            let call = method_calls[0].as_array().unwrap();
            if call[0].as_str().unwrap() == "Email/get" {
                let args = call[1].as_object().unwrap();
                let ids = args.get("ids").unwrap().as_array().unwrap();
                return ids[0].as_str().unwrap() == "e11";
            }
            false
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [["Email/get", {
                "accountId": "A123",
                "state": "s1",
                "list": [
                    {
                        "id": "e11", "threadId": "t11", "subject": "S11",
                        "from": [{"name": "Sender", "email": "sender@example.com"}],
                        "textBody": [{"partId": "p1", "type": "text/plain"}],
                        "bodyValues": {"p1": {"value": "Body 11"}}
                    },
                    {
                        "id": "e12", "threadId": "t12", "subject": "S12",
                        "from": [{"name": "Sender", "email": "sender@example.com"}],
                        "textBody": [{"partId": "p1", "type": "text/plain"}],
                        "bodyValues": {"p1": {"value": "Body 12"}}
                    }
                ],
                "notFound": []
            }, "0"]]
        })))
        .mount(&mock_server)
        .await;

    // Run second backfill batch
    let has_more = poller.backfill_batch(10).await.unwrap();
    assert!(
        !has_more,
        "Second backfill batch should indicate no more emails"
    );
}
