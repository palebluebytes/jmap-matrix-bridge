#![allow(
    clippy::unwrap_used,
    clippy::str_to_string,
    clippy::too_many_lines,
    clippy::unreadable_literal,
    clippy::uninlined_format_args
)]

use jmap_matrix_bridge::client_manager::ClientManager;
use jmap_matrix_bridge::matrix::MatrixClient;
use jmap_matrix_bridge::routes::AppState;
use jmap_matrix_bridge::state::StateStore;
use jmap_matrix_bridge::store::Store;
use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;
use std::sync::Arc;
use wiremock::MockServer;

#[tokio::test]
async fn test_signature_database_operations() {
    let store = Store::new_in_memory(None).await.unwrap();
    let user_id = "@alice:localhost";

    // Register user to satisfy foreign key constraint
    store.save_user(&jmap_matrix_bridge::store::RegisteredUser {
        matrix_user_id: user_id.to_string(),
        jmap_username: "alice".to_string(),
        jmap_token: "secret".to_string(),
        jmap_url: "http://localhost".to_string(),
    }).await.unwrap();

    // 1. Get when no signature is set
    let sig = store.get_user_signature(user_id).await.unwrap();
    assert_eq!(sig, None);

    // 2. Set signature
    store
        .set_user_signature(user_id, "Sent from my Matrix Client!")
        .await
        .unwrap();
    let sig = store.get_user_signature(user_id).await.unwrap();
    assert_eq!(sig, Some("Sent from my Matrix Client!".to_string()));

    // 3. Clear signature
    store.delete_user_signature(user_id).await.unwrap();
    let sig = store.get_user_signature(user_id).await.unwrap();
    assert_eq!(sig, None);
}

#[tokio::test]
async fn test_signature_commands() {
    let mock_server = MockServer::start().await;
    let store = Store::new_in_memory(None).await.unwrap();
    let user_id = "@bob:localhost";

    // Register user to satisfy foreign key constraint
    store.save_user(&jmap_matrix_bridge::store::RegisteredUser {
        matrix_user_id: user_id.to_string(),
        jmap_username: "bob".to_string(),
        jmap_token: "secret".to_string(),
        jmap_url: mock_server.uri(),
    }).await.unwrap();

    let matrix = MatrixClient::new(&mock_server.uri(), "token", "localhost")
        .await
        .unwrap();
    let state_store = StateStore::new();
    let client_manager = ClientManager::new(store.clone(), matrix, 10);

    let state = AppState {
        client_manager: Arc::new(client_manager),
        state_store: Arc::new(state_store),
        puppet_manager: std::sync::Arc::new(jmap_matrix_bridge::puppet::PuppetManager::new(String::new(), "@_jmap_bot:localhost".to_string())),
        hs_token: "hs_token".to_string(),
    };

    let room_id = "!room:localhost";

    // 1. Initially empty
    let sig = store.get_user_signature(user_id).await.unwrap();
    assert_eq!(sig, None);

    // 2. Set via signature command
    let set_body = "signature My Custom Signature";
    jmap_matrix_bridge::commands::handle_login_none(
        &state,
        user_id,
        set_body,
        Some(room_id),
        None,
        &RoomMessageEventContent::text_plain(set_body),
    )
    .await
    .unwrap();

    let sig = store.get_user_signature(user_id).await.unwrap();
    assert_eq!(sig, Some("My Custom Signature".to_string()));

    // 3. Clear via signature command
    let clear_body = "signature clear";
    jmap_matrix_bridge::commands::handle_login_none(
        &state,
        user_id,
        clear_body,
        Some(room_id),
        None,
        &RoomMessageEventContent::text_plain(clear_body),
    )
    .await
    .unwrap();

    let sig = store.get_user_signature(user_id).await.unwrap();
    assert_eq!(sig, None);
}
