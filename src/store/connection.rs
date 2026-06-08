use crate::store::Store;
use anyhow::{Context, Result};
use sqlx::{Pool, Sqlite, sqlite::SqlitePoolOptions};

impl Store {
    pub async fn new(db_url: &str, encryption_key: Option<[u8; 32]>) -> Result<Self> {
        use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous};
        use std::str::FromStr;

        let options = SqliteConnectOptions::from_str(db_url)?
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(std::time::Duration::from_secs(5))
            .foreign_keys(true)
            .create_if_missing(true);

        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await?;
        let store = Self {
            pool,
            encryption_key,
        };
        store.init().await?;
        Ok(store)
    }

    pub async fn new_in_memory(encryption_key: Option<[u8; 32]>) -> Result<Self> {
        Self::new("sqlite::memory:", encryption_key).await
    }

    #[must_use]
    pub const fn pool(&self) -> &Pool<Sqlite> {
        &self.pool
    }

    async fn init(&self) -> Result<()> {
        sqlx::migrate!("./migrations")
            .run(&self.pool)
            .await
            .context("Failed to run database migrations")?;

        // Clear any stale locks on startup
        sqlx::query("DELETE FROM room_creation_locks")
            .execute(&self.pool)
            .await
            .context("Failed to clear stale room creation locks")?;

        Ok(())
    }
}
