use crate::store::{OutboundMessage, Store};
use anyhow::Result;

impl Store {
    #[allow(clippy::too_many_arguments)]
    pub async fn add_to_outbound_queue(
        &self,
        user_id: &str,
        room_id: &str,
        event_id: &str,
        body: &str,
        html: Option<&str>,
        thread_root: Option<&str>,
        attachments_json: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO outbound_queue (matrix_user_id, room_id, event_id, body_text, formatted_body, thread_root_id, attachments_json) VALUES (?, ?, ?, ?, ?, ?, ?)"
        )
        .bind(user_id)
        .bind(room_id)
        .bind(event_id)
        .bind(body)
        .bind(html)
        .bind(thread_root)
        .bind(attachments_json)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_pending_outbound(&self) -> Result<Vec<OutboundMessage>> {
        sqlx::query_as::<_, OutboundMessage>(
            "SELECT id, matrix_user_id, room_id, event_id, body_text, formatted_body, thread_root_id, attachments_json, retry_count \
             FROM outbound_queue \
             WHERE retry_count < 10 AND ( \
                 retry_count = 0 OR \
                 last_retry_at < datetime('now', '-' || CASE retry_count \
                     WHEN 1 THEN 1 \
                     WHEN 2 THEN 2 \
                     WHEN 3 THEN 4 \
                     WHEN 4 THEN 8 \
                     WHEN 5 THEN 16 \
                     WHEN 6 THEN 32 \
                     WHEN 7 THEN 64 \
                     WHEN 8 THEN 128 \
                     ELSE 256 \
                 END || ' minutes') \
             ) LIMIT 10"
        )
        .fetch_all(&self.pool)
        .await
        .map_err(Into::into)
    }

    pub async fn update_retry_count(&self, id: i64) -> Result<()> {
        sqlx::query("UPDATE outbound_queue SET retry_count = retry_count + 1, last_retry_at = CURRENT_TIMESTAMP WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn remove_from_outbound_queue(&self, id: i64) -> Result<()> {
        sqlx::query("DELETE FROM outbound_queue WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
