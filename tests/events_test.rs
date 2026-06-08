#![allow(
    clippy::unwrap_used,
    clippy::str_to_string,
    clippy::too_many_lines,
    clippy::unreadable_literal,
    clippy::uninlined_format_args
)]

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
    routing::put,
};
use jmap_matrix_bridge::client_manager::ClientManager;
use jmap_matrix_bridge::matrix::MatrixClient;
use jmap_matrix_bridge::routes::{AppState, handle_transactions};
use jmap_matrix_bridge::state::StateStore;
use jmap_matrix_bridge::store::Store;
use std::sync::Arc;
use tower::util::ServiceExt;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn test_help_command() {
    let mock_server = MockServer::start().await;

    Mock::given(method("PUT"))
        .and(path_regex(
            r"^/_matrix/client/v3/rooms/.*/send/m.room.message/.*",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "event_id": "$event123"
        })))
        .mount(&mock_server)
        .await;

    let store = Store::new_in_memory(None).await.unwrap();
    let matrix = MatrixClient::new(&mock_server.uri(), "token", "localhost")
        .await
        .unwrap();
    let client_manager = Arc::new(ClientManager::new(store, matrix, 10));
    let state_store = Arc::new(StateStore::new());

    let state = AppState {
        client_manager,
        state_store,
        hs_token: "hs_token".to_string(),
    };

    let app = Router::new()
        .route(
            "/_matrix/app/v1/transactions/{txn_id}",
            put(handle_transactions),
        )
        .with_state(state);

    let json_body = serde_json::json!({
        "txn_id": "txn_help",
        "events": [
            {
                "content": {
                    "body": "help",
                    "msgtype": "m.text"
                },
                "event_id": "$event:localhost",
                "origin_server_ts": 1600000000000i64,
                "room_id": "!room:localhost",
                "sender": "@user:localhost",
                "type": "m.room.message"
            }
        ]
    });

    let response = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/_matrix/app/v1/transactions/txn_help")
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_string(&json_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_invite_handling() {
    let mock_server = MockServer::start().await;

    // Mock join room
    Mock::given(method("POST"))
        .and(path_regex(r"^/_matrix/client/v3/rooms/.*/join"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&mock_server)
        .await;

    // Mock send message (greeting)
    Mock::given(method("PUT"))
        .and(path_regex(
            r"^/_matrix/client/v3/rooms/.*/send/m.room.message/.*",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "event_id": "$event123"
        })))
        .mount(&mock_server)
        .await;

    let store = Store::new_in_memory(None).await.unwrap();
    let matrix = MatrixClient::new(&mock_server.uri(), "token", "localhost")
        .await
        .unwrap();
    let client_manager = Arc::new(ClientManager::new(store, matrix, 10));
    let state_store = Arc::new(StateStore::new());

    let state = AppState {
        client_manager,
        state_store,
        hs_token: "hs_token".to_string(),
    };

    let app = Router::new()
        .route(
            "/_matrix/app/v1/transactions/{txn_id}",
            put(handle_transactions),
        )
        .with_state(state);

    let json_body = serde_json::json!({
        "txn_id": "txn_invite",
        "events": [
            {
                "content": {
                    "membership": "invite"
                },
                "event_id": "$event:localhost",
                "origin_server_ts": 1600000000000i64,
                "room_id": "!room:localhost",
                "sender": "@user:localhost",
                "state_key": "@_jmap_bot:localhost",
                "type": "m.room.member"
            }
        ]
    });

    let response = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/_matrix/app/v1/transactions/txn_invite")
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_string(&json_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_interactive_login_flow() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_test_writer()
        .try_init();

    let mock_server = MockServer::start().await;

    // Mock Matrix send message (for prompts)
    Mock::given(method("PUT"))
        .and(path_regex(
            r"^/_matrix/client/v3/rooms/.*/send/m.room.message/.*",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "event_id": "$event123"
        })))
        .mount(&mock_server)
        .await;

    // Mock JMAP session for the final step
    Mock::given(method("GET"))
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

    let store = Store::new_in_memory(None).await.unwrap();
    let matrix = MatrixClient::new(&mock_server.uri(), "token", "localhost")
        .await
        .unwrap();
    let client_manager = Arc::new(ClientManager::new(store.clone(), matrix, 10));
    let state_store = Arc::new(StateStore::new());

    let state = AppState {
        client_manager,
        state_store: state_store.clone(),
        hs_token: "hs_token".to_string(),
    };

    let app = Router::new()
        .route(
            "/_matrix/app/v1/transactions/{txn_id}",
            put(handle_transactions),
        )
        .with_state(state);

    let sender = "@user:localhost";

    // 1. Send "login"
    let json_body = serde_json::json!({
        "txn_id": "txn_login_1",
        "events": [
            {
                "content": {
                    "body": "login",
                    "msgtype": "m.text"
                },
                "event_id": "$event1",
                "origin_server_ts": 1600000000000i64,
                "room_id": "!room:localhost",
                "sender": sender,
                "type": "m.room.message"
            }
        ]
    });

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/_matrix/app/v1/transactions/txn_login_1")
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_string(&json_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    // Check state
    let login_state = state_store.get_login_state(sender).await;
    assert!(matches!(
        login_state,
        jmap_matrix_bridge::state::LoginState::WaitingForEmail
    ));

    // 2. Send email
    let json_body = serde_json::json!({
        "txn_id": "txn_login_2",
        "events": [
            {
                "content": {
                    "body": "user@example.com",
                    "msgtype": "m.text"
                },
                "event_id": "$event2",
                "origin_server_ts": 1600000000000i64,
                "room_id": "!room:localhost",
                "sender": sender,
                "type": "m.room.message"
            }
        ]
    });

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/_matrix/app/v1/transactions/txn_login_2")
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_string(&json_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    // Check state
    let login_state = state_store.get_login_state(sender).await;
    if let jmap_matrix_bridge::state::LoginState::WaitingForPassword { email } = login_state {
        assert_eq!(email, "user@example.com");
    } else {
        panic!("Expected WaitingForPassword state");
    }

    // 3. Send password
    let json_body = serde_json::json!({
        "txn_id": "txn_login_3",
        "events": [
            {
                "content": {
                    "body": "secret",
                    "msgtype": "m.text"
                },
                "event_id": "$event3",
                "origin_server_ts": 1600000000000i64,
                "room_id": "!room:localhost",
                "sender": sender,
                "type": "m.room.message"
            }
        ]
    });

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/_matrix/app/v1/transactions/txn_login_3")
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_string(&json_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    // Check state
    let login_state = state_store.get_login_state(sender).await;
    if let jmap_matrix_bridge::state::LoginState::WaitingForUrl { email, password } = login_state {
        assert_eq!(email, "user@example.com");
        assert_eq!(password, "secret");
    } else {
        panic!("Expected WaitingForUrl state");
    }

    // 4. Send URL
    let json_body = serde_json::json!({
        "txn_id": "txn_login_4",
        "events": [
            {
                "content": {
                    "body": format!("{}/jmap/session", mock_server.uri()),
                    "msgtype": "m.text"
                },
                "event_id": "$event4",
                "origin_server_ts": 1600000000000i64,
                "room_id": "!room:localhost",
                "sender": sender,
                "type": "m.room.message"
            }
        ]
    });

    let response = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/_matrix/app/v1/transactions/txn_login_4")
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_string(&json_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    // Check state (should be cleared after successful login)
    let login_state = state_store.get_login_state(sender).await;
    assert!(matches!(
        login_state,
        jmap_matrix_bridge::state::LoginState::None
    ));

    // Check if user is registered in store
    let users = store.get_all_users().await.unwrap();
    assert_eq!(users.len(), 1);
    assert_eq!(users[0].matrix_user_id, sender);
}

#[tokio::test]
async fn test_email_command_not_logged_in() {
    let mock_server = MockServer::start().await;

    // Mock Matrix send message (for error response)
    Mock::given(method("PUT"))
        .and(path_regex(
            r"^/_matrix/client/v3/rooms/.*/send/m.room.message/.*",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "event_id": "$event123"
        })))
        .mount(&mock_server)
        .await;

    let store = Store::new_in_memory(None).await.unwrap();
    let matrix = MatrixClient::new(&mock_server.uri(), "token", "localhost")
        .await
        .unwrap();
    let client_manager = Arc::new(ClientManager::new(store, matrix, 10));
    let state_store = Arc::new(StateStore::new());

    let state = AppState {
        client_manager,
        state_store,
        hs_token: "hs_token".to_string(),
    };

    let app = Router::new()
        .route(
            "/_matrix/app/v1/transactions/{txn_id}",
            put(handle_transactions),
        )
        .with_state(state);

    let json_body = serde_json::json!({
        "txn_id": "txn_email_fail",
        "events": [
            {
                "content": {
                    "body": "!email to@example.com Subject Body",
                    "msgtype": "m.text"
                },
                "event_id": "$event:localhost",
                "origin_server_ts": 1600000000000i64,
                "room_id": "!room:localhost",
                "sender": "@user:localhost",
                "type": "m.room.message"
            }
        ]
    });

    let response = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/_matrix/app/v1/transactions/txn_email_fail")
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_string(&json_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_unknown_command() {
    let store = Store::new_in_memory(None).await.unwrap();
    let matrix = MatrixClient::new("http://localhost", "token", "localhost")
        .await
        .unwrap();
    let client_manager = Arc::new(ClientManager::new(store, matrix, 10));
    let state_store = Arc::new(StateStore::new());

    let state = AppState {
        client_manager,
        state_store,
        hs_token: "hs_token".to_string(),
    };

    let app = Router::new()
        .route(
            "/_matrix/app/v1/transactions/{txn_id}",
            put(handle_transactions),
        )
        .with_state(state);

    let json_body = serde_json::json!({
        "txn_id": "txn_unknown",
        "events": [
            {
                "content": {
                    "body": "unknown_command",
                    "msgtype": "m.text"
                },
                "event_id": "$event:localhost",
                "origin_server_ts": 1600000000000i64,
                "room_id": "!room:localhost",
                "sender": "@user:localhost",
                "type": "m.room.message"
            }
        ]
    });

    let response = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/_matrix/app/v1/transactions/txn_unknown")
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_string(&json_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_reply_with_attachment_streaming() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_test_writer()
        .try_init();

    let mock_server = MockServer::start().await;
    let url = mock_server.uri();

    // 1. Mock JMAP session discovery
    let url_clone = url.clone();
    Mock::given(method("GET"))
        .and(path("/.well-known/jmap"))
        .respond_with(move |req: &wiremock::Request| {
            println!("DEBUG: GET /.well-known/jmap request: {:?}", req);
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
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
             "apiUrl": format!("{}/api", url_clone),
             "downloadUrl": format!("{}/download", url_clone),
             "uploadUrl": format!("{}/upload", url_clone),
             "eventSourceUrl": format!("{}/events", url_clone),
             "capabilities": {
                "urn:ietf:params:jmap:core": {},
                "urn:ietf:params:jmap:mail": {},
                "urn:ietf:params:jmap:submission": {}
             },
             "state": "s1"
            }))
        })
        .mount(&mock_server)
        .await;

    // 2. Mock JMAP API Endpoint for Email/get and sending
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(|request: &wiremock::Request| {
            let json: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let method_calls = json.get("methodCalls").unwrap().as_array().unwrap();
            method_calls
                .iter()
                .any(|call| call.as_array().unwrap()[0].as_str().unwrap() == "Email/get")
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [
                ["Email/get", {
                    "accountId": "A123",
                    "state": "s1",
                    "list": [
                        {
                            "id": "email-id-123",
                            "threadId": "thread-123",
                            "subject": "Original Subject",
                            "from": [{"name": "Bob", "email": "bob@example.com"}]
                        }
                    ],
                    "notFound": []
                }, "0"]
            ]
        })))
        .mount(&mock_server)
        .await;

    Mock::given(method("POST"))
        .and(path("/api"))
        .and(|request: &wiremock::Request| {
            let json: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let method_calls = json.get("methodCalls").unwrap().as_array().unwrap();
            method_calls.iter().any(|call| {
                let method = call.as_array().unwrap()[0].as_str().unwrap();
                method == "Email/set" || method == "EmailSubmission/set"
            })
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [
                ["Email/set", {
                    "accountId": "A123",
                    "created": {
                        "draft": {
                            "id": "email-id-456"
                        }
                    }
                }, "0"],
                ["EmailSubmission/set", {
                    "accountId": "A123",
                    "created": {
                        "submission": {
                            "id": "sub-id-789"
                        }
                    }
                }, "1"]
            ]
        })))
        .mount(&mock_server)
        .await;

    // 3. Mock JMAP upload endpoint
    Mock::given(method("POST"))
        .and(path("/upload"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "accountId": "A123",
            "blobId": "blob-uploaded-stream-reply",
            "type": "text/plain",
            "size": 21
        })))
        .mount(&mock_server)
        .await;

    // 4. Mock JMAP events source
    Mock::given(method("GET"))
        .and(path("/events"))
        .respond_with(ResponseTemplate::new(200).set_body_string("retry: 10000\n\n"))
        .mount(&mock_server)
        .await;

    // 5. Mock Matrix media download
    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/.*media/download/.*"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "text/plain")
                .insert_header(
                    "Content-Disposition",
                    "attachment; filename=\"reply_attachment.txt\"",
                )
                .set_body_string("streamed reply file"),
        )
        .mount(&mock_server)
        .await;

    // 6. Mock Matrix bot notify
    Mock::given(method("PUT"))
        .and(path_regex(
            r"^/_matrix/client/v3/rooms/.*/send/m.room.message/.*",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "event_id": "$bot_response_id"
        })))
        .mount(&mock_server)
        .await;

    let store = Store::new_in_memory(None).await.unwrap();
    // Associate Matrix event ID "$original_event_id" with JMAP email ID "email-id-123"
    store
        .save_message_mapping("email-id-123", "$original_event_id")
        .await
        .unwrap();

    let matrix = MatrixClient::new(&mock_server.uri(), "token", "localhost")
        .await
        .unwrap();

    let client_manager = Arc::new(ClientManager::new(store.clone(), matrix, 10));

    // Log in / connect user client
    client_manager
        .login(
            "@user:localhost".to_string(),
            "user".to_string(),
            "secret".to_string(),
            url,
        )
        .await
        .unwrap();

    let state_store = Arc::new(StateStore::new());

    let state = AppState {
        client_manager,
        state_store,
        hs_token: "hs_token".to_string(),
    };

    let app = Router::new()
        .route(
            "/_matrix/app/v1/transactions/{txn_id}",
            put(handle_transactions),
        )
        .with_state(state);

    // 7. Send transaction with a reply containing a media attachment (e.g. m.file)
    let json_body = serde_json::json!({
        "txn_id": "txn_reply_attachment",
        "events": [
            {
                "content": {
                    "body": "reply_attachment.txt",
                    "filename": "reply_attachment.txt",
                    "info": {
                        "mimetype": "text/plain",
                        "size": 19
                    },
                    "msgtype": "m.file",
                    "url": "mxc://localhost/media123",
                    "m.relates_to": {
                        "m.in_reply_to": {
                            "event_id": "$original_event_id"
                        }
                    }
                },
                "event_id": "$reply_event_id",
                "origin_server_ts": 1600000000000i64,
                "room_id": "!room:localhost",
                "sender": "@user:localhost",
                "type": "m.room.message"
            }
        ]
    });

    let response = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/_matrix/app/v1/transactions/txn_reply_attachment")
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_string(&json_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}
