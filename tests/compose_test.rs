#![allow(clippy::unwrap_used)]

//! Tests for the `!compose` building blocks: the shared `ensure_contact_room`
//! helper (must create a room once and reuse it thereafter) and the room-name
//! state helpers used to carry the subject onto the first outbound email.

use jmap_matrix_bridge::matrix::MatrixClient;
use jmap_matrix_bridge::store::{RegisteredUser, Store};
use wiremock::matchers::{method, path};
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
async fn ensure_contact_room_creates_once_then_reuses() {
    let server = MockServer::start().await;

    // Ghost registration.
    Mock::given(method("POST"))
        .and(path("/_matrix/client/v3/register"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&server)
        .await;

    // Room creation must happen EXACTLY once across both compose calls — the
    // second call has to reuse the stored mapping instead of creating again.
    Mock::given(method("POST"))
        .and(path("/_matrix/client/v3/createRoom"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "room_id": "!room1:localhost" })),
        )
        .expect(1)
        .mount(&server)
        .await;

    // Display-name PUT (and any other PUT) — best-effort, errors ignored.
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&server)
        .await;

    let store = store_with_user(&server.uri()).await;
    let matrix = MatrixClient::new(&server.uri(), "as_token", "localhost")
        .await
        .unwrap();

    let r1 = jmap_matrix_bridge::ghost::ensure_contact_room(
        &matrix,
        &store,
        "@alice:localhost",
        "new@example.com",
        "new@example.com",
    )
    .await
    .unwrap();
    let r2 = jmap_matrix_bridge::ghost::ensure_contact_room(
        &matrix,
        &store,
        "@alice:localhost",
        "new@example.com",
        "new@example.com",
    )
    .await
    .unwrap();

    assert_eq!(r1, "!room1:localhost");
    assert_eq!(r1, r2, "second compose to the same address must reuse the room");
    assert_eq!(
        store
            .get_room_by_ghost("new@example.com", "@alice:localhost")
            .await
            .unwrap()
            .as_deref(),
        Some("!room1:localhost"),
        "exactly one room↔email binding should be persisted"
    );
    // Dropping `server` here asserts createRoom was called exactly once.
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
