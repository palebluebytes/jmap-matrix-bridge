#![allow(clippy::unwrap_used)]

//! Tests for bidirectional read/unread-state: the email→event reverse lookup and
//! the puppet `m.read` receipt call (#27), plus the mail-side primitives the
//! unread reflection is built on — resolving a room's latest email and clearing
//! `$seen` via `Email/set`. The puppet-loop orchestration (parse + dedup) is
//! unit-tested in `puppet.rs`; this covers the I/O primitives it calls.

use jmap_matrix_bridge::matrix::MatrixClient;
use jmap_matrix_bridge::sender::JmapSender;
use jmap_matrix_bridge::store::{Store, ThreadRepository};
use serde_json::json;
use wiremock::matchers::{header, method, path, path_regex};
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

#[tokio::test]
async fn get_latest_email_id_by_room_resolves_via_thread_latest() {
    let store = Store::new_in_memory(None).await.unwrap();
    // New thread: root event maps to the root email.
    store
        .save_thread_mapping_atomic("t1", "$root", "!room:localhost", "Subject")
        .await
        .unwrap();
    store
        .save_message_mapping("email-root", "$root")
        .await
        .unwrap();
    assert_eq!(
        store
            .get_latest_email_id_by_room("!room:localhost")
            .await
            .unwrap()
            .as_deref(),
        Some("email-root"),
    );

    // A reply arrives: latest_event advances, so the room's "latest email" moves
    // to the reply — that's what a Matrix mark-unread should target.
    store
        .save_message_mapping("email-reply", "$reply")
        .await
        .unwrap();
    store
        .update_thread_latest_event("t1", "$reply")
        .await
        .unwrap();
    assert_eq!(
        store
            .get_latest_email_id_by_room("!room:localhost")
            .await
            .unwrap()
            .as_deref(),
        Some("email-reply"),
    );

    assert!(
        store
            .get_latest_email_id_by_room("!unknown:localhost")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn mark_as_unread_clears_the_seen_keyword() {
    let server = MockServer::start().await;

    // JMAP session discovery.
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "username": "user",
            "accounts": {},
            "primaryAccounts": {},
            "apiUrl": format!("{}/api", server.uri()),
            "downloadUrl": "http://127.0.0.1/d",
            "uploadUrl": "http://127.0.0.1/u",
            "eventSourceUrl": "http://127.0.0.1/e",
            "capabilities": { "urn:ietf:params:jmap:core": {}, "urn:ietf:params:jmap:mail": {} },
            "state": "s1"
        })))
        .mount(&server)
        .await;

    // Oracle: an Email/set patching keywords/$seen to FALSE for our email.
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(|req: &wiremock::Request| {
            let json: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            json["methodCalls"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|c| {
                    c[0] == "Email/set"
                        && c[1]["update"]["email-x"]["keywords/$seen"] == json!(false)
                })
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "sessionState": "s1",
            "methodResponses": [["Email/set", {"accountId": "acc1", "updated": {"email-x": null}}, "0"]]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let client = jmap_client::client::Client::new()
        .credentials(jmap_client::client::Credentials::bearer("dXNlcjpwYXNz"))
        .connect(&format!("{}/.well-known/jmap", server.uri()))
        .await
        .unwrap();
    JmapSender::new(std::sync::Arc::new(client))
        .mark_as_unread("email-x")
        .await
        .unwrap();
    // Mock `.expect(1)` verifies on drop that the $seen=false patch was sent.
}
