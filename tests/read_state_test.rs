#![allow(clippy::unwrap_used)]

//! Tests for bidirectional read-state (#27): the email→event reverse lookup and
//! the puppet `m.read` receipt call. The `sync_read_state` orchestration (gated
//! on `$seen` + a stored puppet token, deduped per email) is glue over these.

use jmap_matrix_bridge::matrix::MatrixClient;
use jmap_matrix_bridge::store::Store;
use serde_json::json;
use wiremock::matchers::{header, method, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn get_event_id_by_email_round_trips() {
    let store = Store::new_in_memory(None).await.unwrap();
    store
        .save_message_mapping("email123", "$evt:localhost")
        .await
        .unwrap();
    assert_eq!(
        store
            .get_event_id_by_email("email123")
            .await
            .unwrap()
            .as_deref(),
        Some("$evt:localhost")
    );
    assert!(store.get_event_id_by_email("nope").await.unwrap().is_none());
}

#[tokio::test]
async fn send_read_receipt_posts_as_the_puppet() {
    let server = MockServer::start().await;
    // Receipt must carry the *puppet's* token, not the appservice token.
    Mock::given(method("POST"))
        .and(path_regex(r".*/rooms/.*/receipt/m\.read/.*"))
        .and(header("authorization", "Bearer puppet-tok"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount(&server)
        .await;

    let matrix = MatrixClient::new(&server.uri(), "as_token", "localhost")
        .await
        .unwrap();
    matrix
        .send_read_receipt("!room:localhost", "$evt:localhost", "puppet-tok")
        .await
        .unwrap();
    // Mock `.expect(1)` verifies on drop that exactly one matching request landed.
}
