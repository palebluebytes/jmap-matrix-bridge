#![allow(clippy::unwrap_used)]

//! Tests for the `status` / `logout` commands (#21).
//!
//! The network-touching half of logout (aborting the poller, dropping the
//! in-memory JMAP client) lives in `ClientManager::logout`; here we exercise the
//! durable half — the store teardown that must *keep* rooms/mappings while
//! clearing credentials and the outbound queue (ADR-0012) — plus command
//! matching.

use jmap_matrix_bridge::commands::Command;
use jmap_matrix_bridge::commands::{CommandContext, logout::LogoutCommand, status::StatusCommand};
use jmap_matrix_bridge::store::{RegisteredUser, Store};
use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;

async fn store_with_user(mxid: &str) -> Store {
    let store = Store::new_in_memory(None).await.unwrap();
    store
        .save_user(&RegisteredUser {
            matrix_user_id: mxid.to_owned(),
            jmap_username: "user".to_owned(),
            jmap_token: "tok".to_owned(),
            jmap_url: "https://jmap.example/".to_owned(),
        })
        .await
        .unwrap();
    store
}

#[tokio::test]
async fn logout_clears_creds_and_queue_but_keeps_rooms_and_puppet() {
    let mxid = "@alice:localhost";
    let store = store_with_user(mxid).await;
    store
        .save_room_ghost_mapping("!r:localhost", "bob@example.com", mxid)
        .await
        .unwrap();
    store
        .add_to_outbound_queue(mxid, "!r:localhost", "$e", "body", None, None, None, 0)
        .await
        .unwrap();
    store
        .set_matrix_puppet_token(mxid, "puppet-tok")
        .await
        .unwrap();

    assert!(store.get_user(mxid).await.unwrap().is_some());
    assert_eq!(store.count_bridged_rooms(mxid).await.unwrap(), 1);
    assert_eq!(store.count_outbound_queue(mxid).await.unwrap(), 1);

    // The durable half of logout.
    store.clear_user_credentials(mxid).await.unwrap();
    store.clear_outbound_queue(mxid).await.unwrap();

    // Reads as logged out, and is skipped on startup...
    assert!(store.get_user(mxid).await.unwrap().is_none());
    assert!(
        store
            .get_all_users()
            .await
            .unwrap()
            .iter()
            .all(|u| u.matrix_user_id != mxid)
    );
    // ...but the room mapping survives (not cascade-deleted)...
    assert_eq!(store.count_bridged_rooms(mxid).await.unwrap(), 1);
    // ...the queue is abandoned...
    assert_eq!(store.count_outbound_queue(mxid).await.unwrap(), 0);
    // ...and the double-puppet token is untouched.
    assert_eq!(
        store
            .get_matrix_puppet_token(mxid)
            .await
            .unwrap()
            .as_deref(),
        Some("puppet-tok")
    );
}

#[tokio::test]
async fn relogin_after_logout_resumes_against_existing_rooms() {
    let mxid = "@alice:localhost";
    let store = store_with_user(mxid).await;
    store
        .save_room_ghost_mapping("!r:localhost", "bob@example.com", mxid)
        .await
        .unwrap();

    store.clear_user_credentials(mxid).await.unwrap();
    assert!(store.get_user(mxid).await.unwrap().is_none());

    // Re-login upserts the row in place; the kept room mapping is still there.
    store
        .save_user(&RegisteredUser {
            matrix_user_id: mxid.to_owned(),
            jmap_username: "user".to_owned(),
            jmap_token: "tok2".to_owned(),
            jmap_url: "https://jmap.example/".to_owned(),
        })
        .await
        .unwrap();
    assert!(store.get_user(mxid).await.unwrap().is_some());
    assert_eq!(store.count_bridged_rooms(mxid).await.unwrap(), 1);
}

#[tokio::test]
async fn last_sync_round_trips() {
    let mxid = "@alice:localhost";
    let store = store_with_user(mxid).await;
    assert!(store.get_last_sync(mxid).await.unwrap().is_none());
    store
        .set_last_sync(mxid, "2026-06-25T12:00:00Z")
        .await
        .unwrap();
    assert_eq!(
        store.get_last_sync(mxid).await.unwrap().as_deref(),
        Some("2026-06-25T12:00:00Z")
    );
}

#[test]
fn status_command_matches_status_and_ping_aliases() {
    for body in ["status", "!status", "ping", "!ping"] {
        let content = RoomMessageEventContent::text_plain(body);
        let ctx = CommandContext {
            sender_id: "@alice:localhost",
            body_str: body,
            room_id: Some("!r:localhost"),
            event_id: Some("$e:localhost"),
            message_content: &content,
        };
        assert!(
            StatusCommand.matches(&ctx),
            "expected status to match {body}"
        );
    }
    // A near-miss must not match.
    let content = RoomMessageEventContent::text_plain("pinging");
    let ctx = CommandContext {
        sender_id: "@alice:localhost",
        body_str: "pinging",
        room_id: Some("!r:localhost"),
        event_id: Some("$e:localhost"),
        message_content: &content,
    };
    assert!(!StatusCommand.matches(&ctx));
}

#[test]
fn logout_command_matches() {
    let content = RoomMessageEventContent::text_plain("logout");
    let ctx = CommandContext {
        sender_id: "@alice:localhost",
        body_str: "logout",
        room_id: Some("!r:localhost"),
        event_id: Some("$e:localhost"),
        message_content: &content,
    };
    assert!(LogoutCommand.matches(&ctx));
}
