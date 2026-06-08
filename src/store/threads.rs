use crate::store::{Store, ThreadRepository};
use anyhow::Result;
use sqlx::Row;

impl ThreadRepository for Store {
    async fn set_thread_subject(&self, root_event_id: &str, subject: &str) -> Result<()> {
        sqlx::query(
            "INSERT OR REPLACE INTO matrix_thread_subjects (matrix_root_event_id, subject) VALUES (?, ?)"
        )
        .bind(root_event_id)
        .bind(subject)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get_thread_subject(&self, root_event_id: &str) -> Result<Option<String>> {
        sqlx::query_scalar(
            "SELECT subject FROM matrix_thread_subjects WHERE matrix_root_event_id = ?",
        )
        .bind(root_event_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(Into::into)
    }

    /// Returns `(root_event_id, room_id, latest_event_id)` for a JMAP thread.
    async fn get_thread_info(
        &self,
        thread_id: &str,
    ) -> Result<Option<(String, String, Option<String>)>> {
        let record = sqlx::query(
            "SELECT matrix_root_event_id, matrix_room_id, latest_event_id FROM thread_mapping WHERE jmap_thread_id = ?"
        )
        .bind(thread_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(record.map(|r| {
            (
                r.get("matrix_root_event_id"),
                r.get("matrix_room_id"),
                r.get("latest_event_id"),
            )
        }))
    }

    async fn get_jmap_thread_id_by_root_event(
        &self,
        root_event_id: &str,
    ) -> Result<Option<String>> {
        sqlx::query_scalar(
            "SELECT jmap_thread_id FROM thread_mapping WHERE matrix_root_event_id = ?",
        )
        .bind(root_event_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(Into::into)
    }

    async fn get_latest_thread_in_room(
        &self,
        room_id: &str,
    ) -> Result<Option<(String, String, Option<String>)>> {
        let record = sqlx::query(
            "SELECT t.jmap_thread_id, t.matrix_root_event_id, s.subject 
             FROM thread_mapping t
             LEFT JOIN matrix_thread_subjects s ON t.matrix_root_event_id = s.matrix_root_event_id
             WHERE t.matrix_room_id = ? 
             ORDER BY t.rowid DESC LIMIT 1",
        )
        .bind(room_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(record.map(|r| {
            let thread_id: String = r.get("jmap_thread_id");
            let root_event_id: String = r.get("matrix_root_event_id");
            let subject: Option<String> = r.get("subject");
            (thread_id, root_event_id, subject)
        }))
    }

    async fn save_thread_mapping_atomic(
        &self,
        thread_id: &str,
        root_event_id: &str,
        room_id: &str,
        subject: &str,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;

        // latest_event_id starts as root_event_id for a new thread.
        sqlx::query(
            "INSERT OR REPLACE INTO thread_mapping \
             (jmap_thread_id, matrix_root_event_id, matrix_room_id, latest_event_id) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(thread_id)
        .bind(root_event_id)
        .bind(room_id)
        .bind(root_event_id)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "INSERT OR REPLACE INTO matrix_thread_subjects (matrix_root_event_id, subject) VALUES (?, ?)"
        )
        .bind(root_event_id)
        .bind(subject)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(())
    }

    /// Update the latest bridged event for a thread after a new reply is sent.
    async fn update_thread_latest_event(
        &self,
        thread_id: &str,
        latest_event_id: &str,
    ) -> Result<()> {
        sqlx::query("UPDATE thread_mapping SET latest_event_id = ? WHERE jmap_thread_id = ?")
            .bind(latest_event_id)
            .bind(thread_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

impl Store {
    pub async fn save_room_ghost_mapping(
        &self,
        room_id: &str,
        ghost_email: &str,
        matrix_user_id: &str,
    ) -> Result<()> {
        sqlx::query(
            "INSERT OR REPLACE INTO room_ghost_mapping (matrix_room_id, ghost_email, matrix_user_id) VALUES (?, ?, ?)",
        )
        .bind(room_id)
        .bind(ghost_email)
        .bind(matrix_user_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_ghost_email_by_room(&self, room_id: &str) -> Result<Option<String>> {
        sqlx::query_scalar("SELECT ghost_email FROM room_ghost_mapping WHERE matrix_room_id = ?")
            .bind(room_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(Into::into)
    }

    pub async fn get_room_by_ghost(
        &self,
        ghost_email: &str,
        matrix_user_id: &str,
    ) -> Result<Option<String>> {
        sqlx::query_scalar("SELECT matrix_room_id FROM room_ghost_mapping WHERE ghost_email = ? AND matrix_user_id = ?")
            .bind(ghost_email)
            .bind(matrix_user_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(Into::into)
    }

    pub async fn get_last_email_id_by_room(&self, room_id: &str) -> Result<Option<String>> {
        sqlx::query_scalar("SELECT last_email_id FROM room_ghost_mapping WHERE matrix_room_id = ?")
            .bind(room_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(Into::into)
    }

    pub async fn update_last_email_id(&self, room_id: &str, last_email_id: &str) -> Result<()> {
        sqlx::query("UPDATE room_ghost_mapping SET last_email_id = ? WHERE matrix_room_id = ?")
            .bind(last_email_id)
            .bind(room_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn save_message_mapping(&self, email_id: &str, event_id: &str) -> Result<()> {
        sqlx::query(
            "INSERT OR REPLACE INTO message_mapping (jmap_email_id, matrix_event_id) VALUES (?, ?)",
        )
        .bind(email_id)
        .bind(event_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn has_message_mapped(&self, email_id: &str) -> Result<bool> {
        let record = sqlx::query(
            "SELECT 1 FROM message_mapping WHERE jmap_email_id = ? \
             UNION ALL SELECT 1 FROM destroyed_emails WHERE jmap_email_id = ?",
        )
        .bind(email_id)
        .bind(email_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(record.is_some())
    }

    /// Mark a JMAP email as destroyed so it is skipped during future syncs.
    pub async fn mark_email_destroyed(&self, email_id: &str) -> Result<()> {
        sqlx::query("INSERT OR IGNORE INTO destroyed_emails (jmap_email_id) VALUES (?)")
            .bind(email_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn get_email_id_from_event_id(&self, event_id: &str) -> Result<Option<String>> {
        sqlx::query_scalar("SELECT jmap_email_id FROM message_mapping WHERE matrix_event_id = ?")
            .bind(event_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(Into::into)
    }
}
