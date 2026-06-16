#![allow(clippy::unwrap_used)]

//! Tests for the `!compose` building blocks: the shared `create_contact_room`
//! helper (one room per email chain — a fresh room every call) and the room-name
//! state helpers used to carry the subject onto the room / outbound email.

use jmap_matrix_bridge::matrix::MatrixClient;
use jmap_matrix_bridge::store::{RegisteredUser, Store};
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn store_with_user(jmap_url: &str) -> Store {
    let store = Store::new_in_memory(None).await.unwrap();
    store
        .save_user(&RegisteredUser {
            matrix_user_id: "@alice:localhost".to_owned(),
            jmap_username: "alice".to_owned(),
            jmap_token: "secret".to_owned(),
            jmap_url: jmap_url.to_owned(),
        })
        .await
        .unwrap();
    store
}

#[tokio::test]
async fn create_contact_room_creates_a_fresh_room_each_call() {
    let server = MockServer::start().await;

    // Ghost registration.
    Mock::given(method("POST"))
        .and(path("/_matrix/client/v3/register"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&server)
        .await;

    // The email space is created on the first room only (more specific match on
    // the m.space creation_content, higher priority) and reused afterwards.
    Mock::given(method("POST"))
        .and(path("/_matrix/client/v3/createRoom"))
        .and(body_partial_json(
            serde_json::json!({ "creation_content": { "type": "m.space" } }),
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "room_id": "!space1:localhost" })),
        )
        .with_priority(1)
        .expect(1)
        .mount(&server)
        .await;

    // The contact room is created EVERY call (one room per email chain), so two
    // calls => two createRoom requests, NOT a reuse.
    Mock::given(method("POST"))
        .and(path("/_matrix/client/v3/createRoom"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "room_id": "!room1:localhost" })),
        )
        .with_priority(5)
        .expect(2)
        .mount(&server)
        .await;

    // Display-name PUT and m.space.child / m.space.parent state PUTs — accept all.
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&server)
        .await;

    let store = store_with_user(&server.uri()).await;
    let matrix = MatrixClient::new(&server.uri(), "as_token", "localhost")
        .await
        .unwrap();

    for _ in 0..2 {
        jmap_matrix_bridge::ghost::create_contact_room(
            &matrix,
            &store,
            "@alice:localhost",
            "new@example.com",
            "new@example.com",
        )
        .await
        .unwrap();
    }

    // The space is created once and remembered for reuse.
    assert_eq!(
        store
            .get_email_space_room("@alice:localhost")
            .await
            .unwrap()
            .as_deref(),
        Some("!space1:localhost"),
        "the email space should be created once and stored"
    );
    // Dropping `server` asserts the contact createRoom fired twice (no reuse) and
    // the space createRoom fired exactly once.
}

#[tokio::test]
async fn room_name_reads_the_state_event() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "name": "Project X" })),
        )
        .mount(&server)
        .await;
    let matrix = MatrixClient::new(&server.uri(), "as_token", "localhost")
        .await
        .unwrap();
    assert_eq!(
        matrix.room_name("!r:localhost").await.as_deref(),
        Some("Project X")
    );
}

#[tokio::test]
async fn room_name_is_none_when_unset() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(404)
                .set_body_json(serde_json::json!({ "errcode": "M_NOT_FOUND" })),
        )
        .mount(&server)
        .await;
    let matrix = MatrixClient::new(&server.uri(), "as_token", "localhost")
        .await
        .unwrap();
    assert_eq!(matrix.room_name("!r:localhost").await, None);
}

#[tokio::test]
async fn set_room_name_succeeds_on_2xx() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({ "event_id": "$e" })),
        )
        .mount(&server)
        .await;
    let matrix = MatrixClient::new(&server.uri(), "as_token", "localhost")
        .await
        .unwrap();
    matrix.set_room_name("!r:localhost", "Hello").await.unwrap();
}
