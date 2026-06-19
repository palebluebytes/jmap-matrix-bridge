#![allow(clippy::unwrap_used)]

//! Tests for the email-space helpers: creating a room with `type: m.space` and
//! linking a child room into it via `m.space.child` / `m.space.parent`.

use jmap_matrix_bridge::matrix::MatrixClient;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn create_space_sends_space_creation_content() {
    let server = MockServer::start().await;

    // Registration of the bot (send_as_ghost may ensure the sender exists).
    Mock::given(method("POST"))
        .and(path("/_matrix/client/v3/register"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&server)
        .await;

    // createRoom must carry `creation_content.type = m.space`; if it doesn't,
    // this mock won't match and create_space will error, failing the test.
    Mock::given(method("POST"))
        .and(path("/_matrix/client/v3/createRoom"))
        .and(body_partial_json(
            serde_json::json!({ "creation_content": { "type": "m.space" } }),
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "room_id": "!space:localhost" })),
        )
        .expect(1)
        .mount(&server)
        .await;

    let matrix = MatrixClient::new(&server.uri(), "as_token", "localhost")
        .await
        .unwrap();

    let space = matrix
        .create_space("email me@example.com", "All my mail", "@alice:localhost")
        .await
        .unwrap();
    assert_eq!(space, "!space:localhost");
}

#[tokio::test]
async fn add_room_to_space_writes_child_and_parent() {
    let server = MockServer::start().await;

    // m.space.child carries "suggested"; m.space.parent carries "canonical".
    // Distinguishing by body avoids brittle percent-encoded path matching.
    Mock::given(method("PUT"))
        .and(body_partial_json(serde_json::json!({ "suggested": true })))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "event_id": "$c" })),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(body_partial_json(serde_json::json!({ "canonical": true })))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "event_id": "$p" })),
        )
        .expect(1)
        .mount(&server)
        .await;

    let matrix = MatrixClient::new(&server.uri(), "as_token", "localhost")
        .await
        .unwrap();

    matrix
        .add_room_to_space("!space:localhost", "!room:localhost")
        .await
        .unwrap();
    // Server drop verifies both the child and parent state events were written.
}
