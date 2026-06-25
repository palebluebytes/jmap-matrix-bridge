#![allow(clippy::unwrap_used)]

//! Test for the send-state reaction primitive (#26): the bot reacts to the
//! user's outbound message with a status glyph, returning the reaction event id
//! (so it can later be redacted on the ⏳→✅ transition).

use jmap_matrix_bridge::matrix::MatrixClient;
use serde_json::json;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn send_reaction_posts_annotation_and_returns_event_id() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/_matrix/client/v3/register"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .mount(&server)
        .await;
    // The bot is already joined (it owns the room), so the reaction PUT succeeds
    // directly; mount a join just in case the join-on-403 path is taken.
    Mock::given(method("POST"))
        .and(path_regex(r".*/join.*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"room_id": "!r:localhost"})))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path_regex(r".*/send/m\.reaction/.*"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"event_id": "$react:localhost"})),
        )
        .mount(&server)
        .await;

    let matrix = MatrixClient::new(&server.uri(), "as_token", "localhost")
        .await
        .unwrap();
    let id = matrix
        .send_reaction("!r:localhost", "$target:localhost", "⏳")
        .await
        .unwrap();
    assert_eq!(id, "$react:localhost");

    let reqs = server.received_requests().await.unwrap();
    let react = reqs
        .iter()
        .find(|r| r.url.path().contains("/send/m.reaction/"))
        .expect("a reaction was sent");
    let body = String::from_utf8_lossy(&react.body);
    assert!(
        body.contains("$target:localhost") && body.contains('⏳'),
        "reaction must annotate the target with the glyph: {body}"
    );
}
