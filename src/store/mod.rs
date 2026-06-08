//! SQLite-backed persistence for the JMAP-Matrix bridge.
//!
//! All credential fields are encrypted at rest when an `encryption_key` is
//! supplied; the encryption is transparent to callers.

use anyhow::Result;
use sqlx::{Pool, Sqlite};

pub mod connection;
pub mod queue;
pub mod sync;
pub mod threads;
pub mod users;

/// Delimiter used to pack `(jmap_thread_id, parent_email_id, root_event_id)`
/// into the `thread_root_id` column of the outbound queue.
///
/// Both the ghost handler (which enqueues) and the retry worker (which dequeues)
/// MUST use this constant so the encoding never silently diverges.
pub const THREAD_QUEUE_SEPARATOR: char = '|';

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct RegisteredUser {
    pub matrix_user_id: String,
    pub jmap_username: String,
    pub jmap_token: String,
    pub jmap_url: String,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct OutboundMessage {
    pub id: i64,
    pub matrix_user_id: String,
    pub room_id: String,
    pub event_id: String,
    pub body_text: String,
    pub formatted_body: Option<String>,
    pub thread_root_id: Option<String>,
    pub attachments_json: Option<String>,
    pub retry_count: i32,
}

#[derive(Clone, Debug)]
pub struct Store {
    pub(crate) pool: Pool<Sqlite>,
    pub(crate) encryption_key: Option<[u8; 32]>,
}

#[allow(async_fn_in_trait)]
pub trait ThreadRepository {
    async fn set_thread_subject(&self, root_event_id: &str, subject: &str) -> Result<()>;
    async fn get_thread_subject(&self, root_event_id: &str) -> Result<Option<String>>;
    async fn get_thread_info(
        &self,
        thread_id: &str,
    ) -> Result<Option<(String, String, Option<String>)>>;
    async fn get_jmap_thread_id_by_root_event(&self, root_event_id: &str)
    -> Result<Option<String>>;
    async fn get_latest_thread_in_room(
        &self,
        room_id: &str,
    ) -> Result<Option<(String, String, Option<String>)>>;
    async fn save_thread_mapping_atomic(
        &self,
        thread_id: &str,
        root_event_id: &str,
        room_id: &str,
        subject: &str,
    ) -> Result<()>;
    async fn update_thread_latest_event(
        &self,
        thread_id: &str,
        latest_event_id: &str,
    ) -> Result<()>;
}

#[allow(async_fn_in_trait)]
pub trait MailboxRepository {
    async fn get_room_id(&self, mailbox_id: &str) -> Result<Option<String>>;
    async fn save_room_mapping(&self, mailbox_id: &str, room_id: &str) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_jmap_state_roundtrip() {
        let store = Store::new_in_memory(None).await.unwrap();
        store
            .save_user(&RegisteredUser {
                matrix_user_id: "@alice:localhost".to_string(),
                jmap_username: "alice".to_string(),
                jmap_token: "secret".to_string(),
                jmap_url: "http://localhost".to_string(),
            })
            .await
            .unwrap();
        let state = store
            .get_jmap_state("@alice:localhost", "changes")
            .await
            .unwrap();
        assert!(state.is_none());
        store
            .save_jmap_state("@alice:localhost", "changes", "s42")
            .await
            .unwrap();
        let state = store
            .get_jmap_state("@alice:localhost", "changes")
            .await
            .unwrap();
        assert_eq!(state, Some("s42".to_string()));
        store
            .save_jmap_state("@alice:localhost", "changes", "s99")
            .await
            .unwrap();
        let state = store
            .get_jmap_state("@alice:localhost", "changes")
            .await
            .unwrap();
        assert_eq!(state, Some("s99".to_string()));
        store
            .delete_jmap_state("@alice:localhost", "changes")
            .await
            .unwrap();
        let state = store
            .get_jmap_state("@alice:localhost", "changes")
            .await
            .unwrap();
        assert!(state.is_none());
    }

    #[tokio::test]
    async fn test_jmap_state_isolation() {
        let store = Store::new_in_memory(None).await.unwrap();
        store
            .save_user(&RegisteredUser {
                matrix_user_id: "@alice:localhost".to_string(),
                jmap_username: "alice".to_string(),
                jmap_token: "secret".to_string(),
                jmap_url: "http://localhost".to_string(),
            })
            .await
            .unwrap();
        store
            .save_user(&RegisteredUser {
                matrix_user_id: "@bob:localhost".to_string(),
                jmap_username: "bob".to_string(),
                jmap_token: "secret".to_string(),
                jmap_url: "http://localhost".to_string(),
            })
            .await
            .unwrap();
        store
            .save_jmap_state("@alice:localhost", "changes", "s1")
            .await
            .unwrap();
        store
            .save_jmap_state("@bob:localhost", "changes", "s2")
            .await
            .unwrap();
        let alice = store
            .get_jmap_state("@alice:localhost", "changes")
            .await
            .unwrap();
        let bob = store
            .get_jmap_state("@bob:localhost", "changes")
            .await
            .unwrap();
        assert_eq!(alice, Some("s1".to_string()));
        assert_eq!(bob, Some("s2".to_string()));
        store
            .delete_jmap_state("@alice:localhost", "changes")
            .await
            .unwrap();
        let bob = store
            .get_jmap_state("@bob:localhost", "changes")
            .await
            .unwrap();
        assert_eq!(bob, Some("s2".to_string()));
    }

    #[tokio::test]
    async fn test_jmap_state_empty_user_id() {
        let store = Store::new_in_memory(None).await.unwrap();
        store
            .save_user(&RegisteredUser {
                matrix_user_id: "".to_string(),
                jmap_username: "empty".to_string(),
                jmap_token: "secret".to_string(),
                jmap_url: "http://localhost".to_string(),
            })
            .await
            .unwrap();
        store.save_jmap_state("", "changes", "s0").await.unwrap();
        let state = store.get_jmap_state("", "changes").await.unwrap();
        assert_eq!(state, Some("s0".to_string()));
    }

    #[tokio::test]
    async fn test_outbound_queue_backoff_and_retry() {
        let store = Store::new_in_memory(None).await.unwrap();
        store
            .save_user(&RegisteredUser {
                matrix_user_id: "@sender:localhost".to_string(),
                jmap_username: "sender".to_string(),
                jmap_token: "secret".to_string(),
                jmap_url: "http://localhost".to_string(),
            })
            .await
            .unwrap();

        // 1. Initial queue state: empty
        let pending = store.get_pending_outbound().await.unwrap();
        assert!(pending.is_empty());

        store
            .add_to_outbound_queue(
                "@sender:localhost",
                "!room:localhost",
                "$event1",
                "Hello, this is a test email.",
                None,
                Some("thread123|parent456|root789"),
                None,
            )
            .await
            .unwrap();

        // 3. Verify it is pending instantly (since retry_count = 0)
        let pending = store.get_pending_outbound().await.unwrap();
        assert_eq!(pending.len(), 1);
        let msg = &pending[0];
        assert_eq!(msg.matrix_user_id, "@sender:localhost");
        assert_eq!(msg.room_id, "!room:localhost");
        assert_eq!(msg.event_id, "$event1");
        assert_eq!(msg.body_text, "Hello, this is a test email.");
        assert_eq!(
            msg.thread_root_id,
            Some("thread123|parent456|root789".to_owned())
        );
        assert_eq!(msg.retry_count, 0);

        // 4. Update retry count (retry_count becomes 1, last_retry_at becomes now)
        store.update_retry_count(msg.id).await.unwrap();

        // 5. Verify it is NOT pending now (since backoff says wait 1 min, and last_retry_at is now)
        let pending = store.get_pending_outbound().await.unwrap();
        assert!(pending.is_empty());

        // 6. Artificially set last_retry_at to 30 seconds ago (still not pending, needs 1 min)
        sqlx::query(
            "UPDATE outbound_queue SET last_retry_at = datetime('now', '-30 seconds') WHERE id = ?",
        )
        .bind(msg.id)
        .execute(&store.pool)
        .await
        .unwrap();
        let pending = store.get_pending_outbound().await.unwrap();
        assert!(pending.is_empty());

        // 7. Artificially set last_retry_at to 61 seconds ago (now it is pending!)
        sqlx::query(
            "UPDATE outbound_queue SET last_retry_at = datetime('now', '-61 seconds') WHERE id = ?",
        )
        .bind(msg.id)
        .execute(&store.pool)
        .await
        .unwrap();
        let pending = store.get_pending_outbound().await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].retry_count, 1);

        // 8. Update retry count again (retry_count becomes 2, needs 2 mins)
        store.update_retry_count(msg.id).await.unwrap();
        let pending = store.get_pending_outbound().await.unwrap();
        assert!(pending.is_empty());

        // 9. Verify that at 61 seconds ago it is STILL not pending (since for retry_count=2, backoff is 2 mins)
        sqlx::query(
            "UPDATE outbound_queue SET last_retry_at = datetime('now', '-61 seconds') WHERE id = ?",
        )
        .bind(msg.id)
        .execute(&store.pool)
        .await
        .unwrap();
        let pending = store.get_pending_outbound().await.unwrap();
        assert!(pending.is_empty());

        // 10. Artificially set last_retry_at to 121 seconds ago (now it is pending!)
        sqlx::query("UPDATE outbound_queue SET last_retry_at = datetime('now', '-121 seconds') WHERE id = ?")
            .bind(msg.id)
            .execute(&store.pool)
            .await.unwrap();
        let pending = store.get_pending_outbound().await.unwrap();
        assert_eq!(pending.len(), 1);

        // 11. Remove from queue
        store.remove_from_outbound_queue(msg.id).await.unwrap();
        let pending = store.get_pending_outbound().await.unwrap();
        assert!(pending.is_empty());
    }

    #[tokio::test]
    async fn test_outbound_queue_with_attachments() {
        let store = Store::new_in_memory(None).await.unwrap();
        store
            .save_user(&RegisteredUser {
                matrix_user_id: "@sender:localhost".to_string(),
                jmap_username: "sender".to_string(),
                jmap_token: "secret".to_string(),
                jmap_url: "http://localhost".to_string(),
            })
            .await
            .unwrap();

        // 1. Initial queue state: empty
        let pending = store.get_pending_outbound().await.unwrap();
        assert!(pending.is_empty());

        // 2. Add message with attachments to queue
        let atts_json = r#"[{"blob_id":"b123","name":"photo.png","mime_type":"image/png"}]"#;
        store
            .add_to_outbound_queue(
                "@sender:localhost",
                "!room:localhost",
                "$event2",
                "Sent an attachment from Matrix.",
                None,
                None,
                Some(atts_json),
            )
            .await
            .unwrap();

        // 3. Verify it is pending with the attachment JSON intact
        let pending = store.get_pending_outbound().await.unwrap();
        assert_eq!(pending.len(), 1);
        let msg = &pending[0];
        assert_eq!(msg.matrix_user_id, "@sender:localhost");
        assert_eq!(msg.room_id, "!room:localhost");
        assert_eq!(msg.event_id, "$event2");
        assert_eq!(msg.attachments_json.as_deref(), Some(atts_json));

        // 4. Remove from queue
        store.remove_from_outbound_queue(msg.id).await.unwrap();
        let pending = store.get_pending_outbound().await.unwrap();
        assert!(pending.is_empty());
    }

    #[tokio::test]
    async fn test_room_creation_locks() {
        let store = Store::new_in_memory(None).await.unwrap();

        let acquired1 = store.try_acquire_room_creation_lock("lock1").await.unwrap();
        assert!(acquired1);

        let acquired2 = store.try_acquire_room_creation_lock("lock1").await.unwrap();
        assert!(!acquired2);

        store.release_room_creation_lock("lock1").await.unwrap();

        let acquired3 = store.try_acquire_room_creation_lock("lock1").await.unwrap();
        assert!(acquired3);
    }
}
