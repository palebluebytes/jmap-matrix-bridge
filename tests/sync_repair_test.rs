#![allow(clippy::unwrap_used)]

//! Tests for the `sync` command and the email-space repair (#24).
//!
//! The Matrix-facing half (creating the space, writing `m.space.child`) is
//! covered by `space_test.rs`; here we cover the new store query that the repair
//! iterates and the command matching.

use jmap_matrix_bridge::commands::sync::SyncCommand;
use jmap_matrix_bridge::commands::{Command, CommandContext};
use jmap_matrix_bridge::store::{RegisteredUser, Store};
use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;

async fn save_user(store: &Store, mxid: &str) {
    store
        .save_user(&RegisteredUser {
            matrix_user_id: mxid.to_owned(),
            jmap_username: "user".to_owned(),
            jmap_token: "tok".to_owned(),
            jmap_url: "https://jmap.example/".to_owned(),
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn get_user_room_ids_is_scoped_to_the_user() {
    let store = Store::new_in_memory(None).await.unwrap();
    save_user(&store, "@a:localhost").await;
    save_user(&store, "@b:localhost").await;

    store
        .save_room_ghost_mapping("!r1:localhost", "x@e.com", "@a:localhost")
        .await
        .unwrap();
    store
        .save_room_ghost_mapping("!r2:localhost", "y@e.com", "@a:localhost")
        .await
        .unwrap();
    store
        .save_room_ghost_mapping("!r3:localhost", "z@e.com", "@b:localhost")
        .await
        .unwrap();

    let mut a_rooms = store.get_user_room_ids("@a:localhost").await.unwrap();
    a_rooms.sort();
    assert_eq!(a_rooms, vec!["!r1:localhost", "!r2:localhost"]);
    assert_eq!(
        store.get_user_room_ids("@b:localhost").await.unwrap(),
        vec!["!r3:localhost"]
    );
    assert!(
        store
            .get_user_room_ids("@nobody:localhost")
            .await
            .unwrap()
            .is_empty()
    );
}

#[test]
fn sync_command_matches() {
    for body in ["sync", "!sync"] {
        let content = RoomMessageEventContent::text_plain(body);
        let ctx = CommandContext {
            sender_id: "@a:localhost",
            body_str: body,
            room_id: Some("!r:localhost"),
            event_id: Some("$e:localhost"),
            message_content: &content,
        };
        assert!(SyncCommand.matches(&ctx), "should match {body}");
    }
    let content = RoomMessageEventContent::text_plain("synced up");
    let ctx = CommandContext {
        sender_id: "@a:localhost",
        body_str: "synced up",
        room_id: Some("!r:localhost"),
        event_id: Some("$e:localhost"),
        message_content: &content,
    };
    assert!(!SyncCommand.matches(&ctx));
}
