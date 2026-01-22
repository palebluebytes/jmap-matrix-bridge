use sqlx::{sqlite::SqlitePoolOptions, Pool, Sqlite, Row};
use anyhow::Result;

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct RegisteredUser {
    pub matrix_user_id: String,
    pub jmap_username: String,
    pub jmap_token: String,
    pub jmap_url: String,
}

#[derive(Clone)]
pub struct Store {
    pool: Pool<Sqlite>,
}

impl Store {
    pub async fn new(db_url: &str) -> Result<Self> {
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect(db_url)
            .await?;
            
        let store = Self { pool };
        store.init().await?;
        Ok(store)
    }

    pub async fn new_in_memory() -> Result<Self> {
        Self::new("sqlite::memory:").await
    }

    async fn init(&self) -> Result<()> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS mailbox_mapping (
                jmap_mailbox_id TEXT PRIMARY KEY,
                matrix_room_id TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS thread_mapping (
                jmap_thread_id TEXT PRIMARY KEY,
                matrix_root_event_id TEXT NOT NULL,
                matrix_room_id TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS message_mapping (
                jmap_email_id TEXT PRIMARY KEY,
                matrix_event_id TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS users (
                matrix_user_id TEXT PRIMARY KEY,
                jmap_username TEXT NOT NULL,
                jmap_token TEXT NOT NULL,
                jmap_url TEXT NOT NULL
            );"
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_room_id(&self, mailbox_id: &str) -> Result<Option<String>> {
        let record = sqlx::query(
            "SELECT matrix_room_id FROM mailbox_mapping WHERE jmap_mailbox_id = ?"
        )
        .bind(mailbox_id)
        .fetch_optional(&self.pool)
        .await?;
        
        Ok(record.map(|r| r.get("matrix_room_id")))
    }

    pub async fn save_room_mapping(&self, mailbox_id: &str, room_id: &str) -> Result<()> {
        sqlx::query(
            "INSERT OR REPLACE INTO mailbox_mapping (jmap_mailbox_id, matrix_room_id) VALUES (?, ?)"
        )
        .bind(mailbox_id)
        .bind(room_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_thread_info(&self, thread_id: &str) -> Result<Option<(String, String)>> {
        let record = sqlx::query(
            "SELECT matrix_root_event_id, matrix_room_id FROM thread_mapping WHERE jmap_thread_id = ?"
        )
        .bind(thread_id)
        .fetch_optional(&self.pool)
        .await?;
        
        Ok(record.map(|r| (r.get("matrix_root_event_id"), r.get("matrix_room_id"))))
    }

    pub async fn save_thread_mapping(&self, thread_id: &str, root_event_id: &str, room_id: &str) -> Result<()> {
        sqlx::query(
            "INSERT OR REPLACE INTO thread_mapping (jmap_thread_id, matrix_root_event_id, matrix_room_id) VALUES (?, ?, ?)"
        )
        .bind(thread_id)
        .bind(root_event_id)
        .bind(room_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn save_message_mapping(&self, email_id: &str, event_id: &str) -> Result<()> {
        sqlx::query(
            "INSERT OR REPLACE INTO message_mapping (jmap_email_id, matrix_event_id) VALUES (?, ?)"
        )
        .bind(email_id)
        .bind(event_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn save_user(&self, user: &RegisteredUser) -> Result<()> {
        sqlx::query(
            "INSERT OR REPLACE INTO users (matrix_user_id, jmap_username, jmap_token, jmap_url) VALUES (?, ?, ?, ?)"
        )
        .bind(&user.matrix_user_id)
        .bind(&user.jmap_username)
        .bind(&user.jmap_token)
        .bind(&user.jmap_url)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_user(&self, matrix_user_id: &str) -> Result<Option<RegisteredUser>> {
        let user = sqlx::query_as::<_, RegisteredUser>(
            "SELECT matrix_user_id, jmap_username, jmap_token, jmap_url FROM users WHERE matrix_user_id = ?"
        )
        .bind(matrix_user_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(user)
    }

    pub async fn get_all_users(&self) -> Result<Vec<RegisteredUser>> {
        let users = sqlx::query_as::<_, RegisteredUser>(
            "SELECT matrix_user_id, jmap_username, jmap_token, jmap_url FROM users"
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(users)
    }
}
