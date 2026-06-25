#![allow(clippy::unwrap_used)]

//! Tests for the trash/junk thread actions (#25): the JMAP `move_thread_to_role`
//! (success + graceful fallback when the account has no such mailbox), the store
//! unbridge teardown, and command/reaction recognition.

use jmap_matrix_bridge::sender::JmapSender;
use jmap_matrix_bridge::store::{RegisteredUser, Store};
use std::sync::Arc;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn session_mock(server: &MockServer) {
    let url = server.uri();
    Mock::given(method("GET"))
        .and(path("/.well-known/jmap"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "username": "user",
            "accounts": { "A1": { "name": "user", "isPersonal": true, "isReadOnly": false,
                "accountCapabilities": {
                    "urn:ietf:params:jmap:core": {},
                    "urn:ietf:params:jmap:mail": {},
                    "urn:ietf:params:jmap:submission": {} } } },
            "primaryAccounts": {
                "urn:ietf:params:jmap:core": "A1",
                "urn:ietf:params:jmap:mail": "A1",
                "urn:ietf:params:jmap:submission": "A1" },
            "apiUrl": format!("{url}/api"),
            "downloadUrl": format!("{url}/download"),
            "uploadUrl": format!("{url}/upload"),
            "eventSourceUrl": format!("{url}/events"),
            "capabilities": {
                "urn:ietf:params:jmap:core": {},
                "urn:ietf:params:jmap:mail": {},
                "urn:ietf:params:jmap:submission": {} },
            "state": "s1"
        })))
        .mount(server)
        .await;
}

async fn connected_sender(server: &MockServer) -> JmapSender {
    let client = jmap_client::client::Client::new()
        .credentials(jmap_client::client::Credentials::Basic(
            "dXNlcjpwYXNz".to_owned(),
        ))
        .connect(&server.uri())
        .await
        .expect("connect mock");
    JmapSender::new(Arc::new(client))
}

#[tokio::test]
async fn move_thread_to_role_moves_emails_to_target_mailbox() {
    let server = MockServer::start().await;
    session_mock(&server).await;

    // Mailbox/query for the Trash role → its id.
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(body_string_contains("Mailbox/query"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [["Mailbox/query", { "accountId": "A1", "queryState": "q", "canCalculateChanges": false, "position": 0, "ids": ["mbTrash"] }, "0"]]
        })))
        .mount(&server)
        .await;
    // Thread/get → the thread's email ids.
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(body_string_contains("Thread/get"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [["Thread/get", { "accountId": "A1", "state": "s", "list": [{ "id": "t1", "emailIds": ["e1", "e2"] }], "notFound": [] }, "0"]]
        })))
        .mount(&server)
        .await;
    // Email/set → success.
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(body_string_contains("Email/set"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [["Email/set", { "accountId": "A1", "oldState": "s", "newState": "s2", "updated": { "e1": null, "e2": null }, "notUpdated": {} }, "0"]]
        })))
        .mount(&server)
        .await;

    let sender = connected_sender(&server).await;
    let moved = sender
        .move_thread_to_role("t1", jmap_client::mailbox::Role::Trash)
        .await
        .unwrap();
    assert!(moved, "should report the move happened");

    // The Email/set carried the target mailbox id for both emails.
    let reqs = server.received_requests().await.unwrap();
    let set = reqs
        .iter()
        .find(|r| String::from_utf8_lossy(&r.body).contains("Email/set"))
        .expect("an Email/set was issued");
    let body = String::from_utf8_lossy(&set.body);
    assert!(
        body.contains("mbTrash"),
        "Email/set must move to the trash mailbox: {body}"
    );
}

#[tokio::test]
async fn move_thread_to_role_returns_false_without_target_mailbox() {
    let server = MockServer::start().await;
    session_mock(&server).await;
    // Mailbox/query finds no mailbox with that role.
    Mock::given(method("POST"))
        .and(path("/api"))
        .and(body_string_contains("Mailbox/query"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "sessionState": "s1",
            "methodResponses": [["Mailbox/query", { "accountId": "A1", "queryState": "q", "canCalculateChanges": false, "position": 0, "ids": [] }, "0"]]
        })))
        .mount(&server)
        .await;

    let sender = connected_sender(&server).await;
    let moved = sender
        .move_thread_to_role("t1", jmap_client::mailbox::Role::Junk)
        .await
        .unwrap();
    assert!(
        !moved,
        "no Junk mailbox → false, so the caller can fall back"
    );
}

#[tokio::test]
async fn unbridge_room_drops_mappings() {
    let store = Store::new_in_memory(None).await.unwrap();
    store
        .save_user(&RegisteredUser {
            matrix_user_id: "@a:localhost".to_owned(),
            jmap_username: "u".to_owned(),
            jmap_token: "t".to_owned(),
            jmap_url: "https://j/".to_owned(),
        })
        .await
        .unwrap();
    store
        .save_room_ghost_mapping("!r:localhost", "bob@example.com", "@a:localhost")
        .await
        .unwrap();
    assert!(
        store
            .get_ghost_email_by_room("!r:localhost")
            .await
            .unwrap()
            .is_some()
    );

    store.unbridge_room("!r:localhost").await.unwrap();
    assert!(
        store
            .get_ghost_email_by_room("!r:localhost")
            .await
            .unwrap()
            .is_none()
    );
}
