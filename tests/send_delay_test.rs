#![allow(clippy::unwrap_used)]

//! Tests for the send-delay hold window and its redact/edit affordances (#23).
//!
//! Eligibility is driven by SQL `datetime('now')`, so we drive the hold with the
//! delay argument: a negative delay places `release_at` in the past (released),
//! a positive one in the future (still held) — deterministic without sleeping.

use jmap_matrix_bridge::commands::send_delay::SendDelayCommand;
use jmap_matrix_bridge::commands::{Command, CommandContext};
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
async fn held_message_is_not_eligible_until_release() {
    let mxid = "@a:localhost";
    let store = store_with_user(mxid).await;

    // Held 60s into the future — the worker must not see it yet.
    store
        .add_to_outbound_queue(mxid, "!r:localhost", "$held", "body", None, None, None, 60)
        .await
        .unwrap();
    assert!(store.get_pending_outbound().await.unwrap().is_empty());

    // A zero delay sets release_at to now, eligible immediately.
    store
        .add_to_outbound_queue(mxid, "!r:localhost", "$ready", "body", None, None, None, 0)
        .await
        .unwrap();
    let pending = store.get_pending_outbound().await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].event_id, "$ready");
}

#[tokio::test]
async fn redact_cancels_and_edit_rewrites_within_window() {
    let mxid = "@a:localhost";
    let store = store_with_user(mxid).await;
    store
        .add_to_outbound_queue(mxid, "!r:localhost", "$e1", "original", None, None, None, 0)
        .await
        .unwrap();

    // Edit rewrites the queued body.
    assert!(
        store
            .update_outbound_body_by_event("$e1", "edited")
            .await
            .unwrap()
    );
    let pending = store.get_pending_outbound().await.unwrap();
    assert_eq!(pending[0].body_text, "edited");

    // Redact cancels the still-queued send.
    assert!(store.cancel_outbound_by_event("$e1").await.unwrap());
    assert!(store.get_pending_outbound().await.unwrap().is_empty());

    // Both are no-ops (false) once the message is gone / never existed.
    assert!(!store.cancel_outbound_by_event("$e1").await.unwrap());
    assert!(
        !store
            .update_outbound_body_by_event("$missing", "x")
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn send_delay_setting_round_trips() {
    let mxid = "@a:localhost";
    let store = store_with_user(mxid).await;
    assert!(store.get_send_delay(mxid).await.unwrap().is_none());
    store.set_send_delay(mxid, 30).await.unwrap();
    assert_eq!(store.get_send_delay(mxid).await.unwrap(), Some(30));
}

#[test]
fn send_delay_command_matches_forms() {
    for body in [
        "send-delay",
        "send-delay 10",
        "send-delay off",
        "!send-delay 5",
    ] {
        let content = RoomMessageEventContent::text_plain(body);
        let ctx = CommandContext {
            sender_id: "@a:localhost",
            body_str: body,
            room_id: Some("!r:localhost"),
            event_id: Some("$e:localhost"),
            message_content: &content,
        };
        assert!(SendDelayCommand.matches(&ctx), "should match {body}");
    }
    // A word that merely starts with the command name must not match.
    let content = RoomMessageEventContent::text_plain("send-delayed thoughts");
    let ctx = CommandContext {
        sender_id: "@a:localhost",
        body_str: "send-delayed thoughts",
        room_id: Some("!r:localhost"),
        event_id: Some("$e:localhost"),
        message_content: &content,
    };
    assert!(!SendDelayCommand.matches(&ctx));
}
