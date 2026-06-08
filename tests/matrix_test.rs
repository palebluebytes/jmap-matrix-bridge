#![allow(
    clippy::unwrap_used,
    clippy::str_to_string,
    clippy::too_many_lines,
    clippy::unreadable_literal,
    clippy::uninlined_format_args
)]

use jmap_matrix_bridge::matrix::MatrixClient;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn test_ensure_user_exists() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/_matrix/client/v3/register"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&mock_server)
        .await;

    let client = MatrixClient::new(&mock_server.uri(), "token", "localhost")
        .await
        .unwrap();
    let result = client.ensure_user_exists("test_user").await;

    assert!(result.is_ok());
}

#[tokio::test]
async fn test_send_message() {
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

    let client = MatrixClient::new(&mock_server.uri(), "token", "localhost")
        .await
        .unwrap();
    let result = client
        .send_message("!room:localhost", "Hello", None, None)
        .await;

    assert!(result.is_ok());
    assert_eq!(result.unwrap(), "$event123");
}

#[tokio::test]
async fn test_send_message_threaded() {
    let mock_server = MockServer::start().await;

    Mock::given(method("PUT"))
        .and(path_regex(
            r"^/_matrix/client/v3/rooms/.*/send/m.room.message/.*",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "event_id": "$event456"
        })))
        .mount(&mock_server)
        .await;

    let client = MatrixClient::new(&mock_server.uri(), "token", "localhost")
        .await
        .unwrap();
    let result = client
        .send_message(
            "!room:localhost",
            "Hello",
            Some("<b>Hello</b>"),
            Some("$root_event"),
        )
        .await;

    assert!(result.is_ok());
    assert_eq!(result.unwrap(), "$event456");
}

#[tokio::test]
async fn test_set_display_name() {
    let mock_server = MockServer::start().await;

    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/v3/profile/.*/displayname"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&mock_server)
        .await;

    let client = MatrixClient::new(&mock_server.uri(), "token", "localhost")
        .await
        .unwrap();
    let result = client.set_display_name("@_jmap_bot:localhost", "Bot").await;

    assert!(result.is_ok());
}

#[tokio::test]
async fn test_set_avatar() {
    let mock_server = MockServer::start().await;

    // Mock upload
    Mock::given(method("POST"))
        .and(path("/_matrix/media/v3/upload"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "content_uri": "mxc://localhost/123"
        })))
        .mount(&mock_server)
        .await;

    // Mock set avatar URL
    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/v3/profile/.*/avatar_url"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&mock_server)
        .await;

    let client = MatrixClient::new(&mock_server.uri(), "token", "localhost")
        .await
        .unwrap();
    let result = client
        .set_avatar("@_jmap_bot:localhost", b"image data", "image/png")
        .await;

    assert!(result.is_ok());
}

#[tokio::test]
async fn test_join_room() {
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/_matrix/client/versions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "versions": ["r0.6.0", "v1.1", "v1.5"],
            "unstable_features": {}
        })))
        .mount(&mock_server)
        .await;

    Mock::given(method("POST"))
        .and(path_regex(r"^/_matrix/client/v3/(rooms/.*/join|join/.*)"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "room_id": "!room:localhost"
        })))
        .mount(&mock_server)
        .await;

    let client = MatrixClient::new(&mock_server.uri(), "token", "localhost")
        .await
        .unwrap();
    let result = client.join_room("!room:localhost").await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_create_room_for_thread() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/_matrix/client/v3/createRoom"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "room_id": "!new_room:localhost"
        })))
        .mount(&mock_server)
        .await;

    let client = MatrixClient::new(&mock_server.uri(), "token", "localhost")
        .await
        .unwrap();
    let result = client
        .create_room_for_thread("Subject", "@user:localhost")
        .await;

    assert!(result.is_ok());
    assert_eq!(result.unwrap(), "!new_room:localhost");
}

#[tokio::test]
async fn test_create_room_for_mailbox() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/_matrix/client/v3/createRoom"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "room_id": "!mailbox_room:localhost"
        })))
        .mount(&mock_server)
        .await;

    let client = MatrixClient::new(&mock_server.uri(), "token", "localhost")
        .await
        .unwrap();
    let result = client.create_room_for_mailbox("Inbox").await;

    assert!(result.is_ok());
    assert_eq!(result.unwrap(), "!mailbox_room:localhost");
}

#[tokio::test]
async fn test_redact_event() {
    let mock_server = MockServer::start().await;

    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/v3/rooms/.*/redact/.*/.*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "event_id": "$redaction_event_id"
        })))
        .mount(&mock_server)
        .await;

    let client = MatrixClient::new(&mock_server.uri(), "token", "localhost")
        .await
        .unwrap();
    let result = client
        .redact_event("!room:localhost", "$event123", "Reason")
        .await;

    assert!(result.is_ok());
}
