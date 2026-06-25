use crate::store::{OutboundMessage, Store};
use anyhow::Result;

impl Store {
    /// Enqueue an outbound message, held until `delay_secs` from now before the
    /// worker may submit it (the send-delay undo window, ADR-0012). A retry
    /// re-enqueue passes `delay_secs = 0` for immediate eligibility.
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
        delay_secs: i64,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO outbound_queue \
                 (matrix_user_id, room_id, event_id, body_text, formatted_body, thread_root_id, attachments_json, release_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, datetime('now', '+' || ? || ' seconds'))"
        )
        .bind(user_id)
        .bind(room_id)
        .bind(event_id)
        .bind(body)
        .bind(html)
        .bind(thread_root)
        .bind(attachments_json)
        .bind(delay_secs)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Pending messages eligible for submission *now*: past their `release_at`
    /// hold, and (for prior failures) past their exponential-backoff window.
    pub async fn get_pending_outbound(&self) -> Result<Vec<OutboundMessage>> {
        sqlx::query_as::<_, OutboundMessage>(
            "SELECT id, matrix_user_id, room_id, event_id, body_text, formatted_body, thread_root_id, attachments_json, retry_count \
             FROM outbound_queue \
             WHERE retry_count < 10 AND release_at <= datetime('now') AND ( \
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

    /// Cancel a still-queued outbound message by its Matrix event id (a redaction
    /// within the send-delay window). Returns whether a row was removed — `false`
    /// means it was already submitted, so there is nothing to unsend.
    pub async fn cancel_outbound_by_event(&self, event_id: &str) -> Result<bool> {
        let res = sqlx::query("DELETE FROM outbound_queue WHERE event_id = ?")
            .bind(event_id)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }

    /// Rewrite a still-queued message's body by its Matrix event id (an edit
    /// within the send-delay window). Returns whether a row was updated.
    pub async fn update_outbound_body_by_event(
        &self,
        event_id: &str,
        new_body: &str,
    ) -> Result<bool> {
        let res = sqlx::query("UPDATE outbound_queue SET body_text = ? WHERE event_id = ?")
            .bind(new_body)
            .bind(event_id)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
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

    /// Drop every pending outbound message for a user. Used by `logout`, which
    /// abandons unsent mail rather than flushing it (ADR-0012).
    pub async fn clear_outbound_queue(&self, matrix_user_id: &str) -> Result<()> {
        sqlx::query("DELETE FROM outbound_queue WHERE matrix_user_id = ?")
            .bind(matrix_user_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Count a user's pending outbound messages, for the `status` command.
    pub async fn count_outbound_queue(&self, matrix_user_id: &str) -> Result<i64> {
        sqlx::query_scalar::<sqlx::Sqlite, i64>(
            "SELECT COUNT(*) FROM outbound_queue WHERE matrix_user_id = ?",
        )
        .bind(matrix_user_id)
        .fetch_one(&self.pool)
        .await
        .map_err(Into::into)
    }
}
