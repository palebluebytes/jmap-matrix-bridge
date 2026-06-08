use crate::store::{MailboxRepository, Store};
use anyhow::Result;
use sqlx::{Row, Sqlite};

impl MailboxRepository for Store {
    async fn get_room_id(&self, mailbox_id: &str) -> Result<Option<String>> {
        let record =
            sqlx::query("SELECT matrix_room_id FROM mailbox_mapping WHERE jmap_mailbox_id = ?")
                .bind(mailbox_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(record.map(|r| r.get("matrix_room_id")))
    }

    async fn save_room_mapping(&self, mailbox_id: &str, room_id: &str) -> Result<()> {
        sqlx::query(
            "INSERT OR REPLACE INTO mailbox_mapping (jmap_mailbox_id, matrix_room_id) VALUES (?, ?)"
        )
        .bind(mailbox_id)
        .bind(room_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

impl Store {
    /// Save the JMAP sync state for a user.
    pub async fn save_jmap_state(
        &self,
        matrix_user_id: &str,
        key: &str,
        state: &str,
    ) -> Result<()> {
        sqlx::query(
            "INSERT OR REPLACE INTO jmap_state (matrix_user_id, state_key, state_value) VALUES (?, ?, ?)",
        )
        .bind(matrix_user_id)
        .bind(key)
        .bind(state)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Retrieve the last saved JMAP sync state for a user.
    pub async fn get_jmap_state(&self, matrix_user_id: &str, key: &str) -> Result<Option<String>> {
        sqlx::query_scalar::<Sqlite, String>(
            "SELECT state_value FROM jmap_state WHERE matrix_user_id = ? AND state_key = ?",
        )
        .bind(matrix_user_id)
        .bind(key)
        .fetch_optional(&self.pool)
        .await
        .map_err(Into::into)
    }

    /// Delete the JMAP sync state for a user.
    pub async fn delete_jmap_state(&self, matrix_user_id: &str, key: &str) -> Result<()> {
        sqlx::query("DELETE FROM jmap_state WHERE matrix_user_id = ? AND state_key = ?")
            .bind(matrix_user_id)
            .bind(key)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Check if a Matrix transaction ID has already been processed.
    pub async fn is_transaction_processed(&self, txn_id: &str) -> Result<bool> {
        let record = sqlx::query_scalar::<Sqlite, i64>(
            "SELECT 1 FROM processed_transactions WHERE txn_id = ?",
        )
        .bind(txn_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(record.is_some())
    }

    /// Record a Matrix transaction ID as processed.
    pub async fn mark_transaction_processed(&self, txn_id: &str) -> Result<()> {
        sqlx::query("INSERT INTO processed_transactions (txn_id) VALUES (?)")
            .bind(txn_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Prune old data from the store.
    /// - Matrix transactions older than 7 days.
    /// - Destroyed email records older than 30 days.
    pub async fn prune_old_data(&self) -> Result<()> {
        let rows = sqlx::query(
            "DELETE FROM processed_transactions WHERE processed_at < datetime('now', '-7 days')",
        )
        .execute(&self.pool)
        .await?
        .rows_affected();

        if rows > 0 {
            tracing::info!("Pruned {} old transaction records", rows);
        }

        let destroyed_rows = sqlx::query(
            "DELETE FROM destroyed_emails WHERE destroyed_at < datetime('now', '-30 days')",
        )
        .execute(&self.pool)
        .await?
        .rows_affected();

        if destroyed_rows > 0 {
            tracing::info!("Pruned {} old destroyed email records", destroyed_rows);
        }

        Ok(())
    }

    pub async fn try_acquire_room_creation_lock(&self, lock_key: &str) -> Result<bool> {
        let res = sqlx::query("INSERT INTO room_creation_locks (lock_key) VALUES (?)")
            .bind(lock_key)
            .execute(&self.pool)
            .await;

        match res {
            Ok(_) => Ok(true),
            Err(sqlx::Error::Database(db_err)) => {
                if db_err.is_unique_violation()
                    || db_err.code().as_deref() == Some("1555")
                    || db_err.code().as_deref() == Some("2067")
                    || db_err.message().contains("UNIQUE constraint failed")
                {
                    Ok(false)
                } else {
                    Err(sqlx::Error::Database(db_err).into())
                }
            }
            Err(e) => Err(e.into()),
        }
    }

    pub async fn release_room_creation_lock(&self, lock_key: &str) -> Result<()> {
        sqlx::query("DELETE FROM room_creation_locks WHERE lock_key = ?")
            .bind(lock_key)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
