//! Global, non-user-scoped bridge state (see `migrations/*_bridge_state.sql`).
//!
//! A small key/value store for facts about the bridge itself rather than any
//! one user — currently the bot profile fingerprints used to make avatar and
//! display-name application idempotent across restarts.

use crate::store::Store;
use anyhow::Result;
use sqlx::Sqlite;

impl Store {
    /// Read a bridge-wide state value, or `None` if the key was never set.
    pub async fn get_bridge_state(&self, key: &str) -> Result<Option<String>> {
        sqlx::query_scalar::<Sqlite, String>(
            "SELECT state_value FROM bridge_state WHERE state_key = ?",
        )
        .bind(key)
        .fetch_optional(&self.pool)
        .await
        .map_err(Into::into)
    }

    /// Upsert a bridge-wide state value.
    pub async fn set_bridge_state(&self, key: &str, value: &str) -> Result<()> {
        sqlx::query("INSERT OR REPLACE INTO bridge_state (state_key, state_value) VALUES (?, ?)")
            .bind(key)
            .bind(value)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
