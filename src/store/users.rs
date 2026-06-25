use crate::crypto;
use crate::store::{RegisteredUser, Store};
use anyhow::{Context, Result};
use sqlx::Sqlite;

impl Store {
    pub async fn save_user(&self, user: &RegisteredUser) -> Result<()> {
        let (username, token) = if let Some(key) = &self.encryption_key {
            let enc_user =
                crypto::encrypt(&user.jmap_username, key).context("Failed to encrypt username")?;
            let enc_token =
                crypto::encrypt(&user.jmap_token, key).context("Failed to encrypt token")?;
            (enc_user, enc_token)
        } else {
            (user.jmap_username.clone(), user.jmap_token.clone())
        };
        // Upsert in place. `INSERT OR REPLACE` would DELETE the existing row
        // before re-inserting, which fires the `ON DELETE CASCADE` on every
        // child table keyed by `matrix_user_id` (room_ghost_mapping, jmap_state,
        // user_signatures) — silently wiping all of a user's room↔email bindings
        // every time the user is re-saved (which declarative provisioning does on
        // every startup). `ON CONFLICT DO UPDATE` mutates the row in place, so no
        // delete and no cascade.
        sqlx::query(
            "INSERT INTO users (matrix_user_id, jmap_username, jmap_token, jmap_url) \
             VALUES (?, ?, ?, ?) \
             ON CONFLICT(matrix_user_id) DO UPDATE SET \
                 jmap_username = excluded.jmap_username, \
                 jmap_token = excluded.jmap_token, \
                 jmap_url = excluded.jmap_url",
        )
        .bind(&user.matrix_user_id)
        .bind(&username)
        .bind(&token)
        .bind(&user.jmap_url)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Fetch an *active* (logged-in) user. A logged-out user (blank
    /// `jmap_token`, see [`Store::clear_user_credentials`]) is reported as
    /// absent — its row survives only to anchor the kept room/thread mappings.
    /// The `jmap_token != ''` filter runs before decrypt, so the blanked token
    /// is never fed to `crypto::decrypt`.
    pub async fn get_user(&self, matrix_user_id: &str) -> Result<Option<RegisteredUser>> {
        sqlx::query_as::<_, RegisteredUser>(
            "SELECT matrix_user_id, jmap_username, jmap_token, jmap_url FROM users \
             WHERE matrix_user_id = ? AND jmap_token != ''",
        )
        .bind(matrix_user_id)
        .fetch_optional(&self.pool)
        .await?
        .map(|u| self.decrypt_user(u))
        .transpose()
    }

    /// All *active* users — logged-out rows (blank `jmap_token`) are skipped so
    /// startup never tries to reconnect a session the user explicitly ended.
    pub async fn get_all_users(&self) -> Result<Vec<RegisteredUser>> {
        sqlx::query_as::<_, RegisteredUser>(
            "SELECT matrix_user_id, jmap_username, jmap_token, jmap_url FROM users \
             WHERE jmap_token != ''",
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|u| self.decrypt_user(u))
        .collect()
    }

    /// Log a user out without losing their rooms: blank the stored JMAP
    /// credentials *in place* (ADR-0012). An in-place `UPDATE` — not a row
    /// delete — so the `ON DELETE CASCADE` on `room_ghost_mapping` / `jmap_state`
    /// does not fire, and a later `login` resumes against the same rooms. The
    /// row then reads as logged-out to [`Store::get_user`] / [`Store::get_all_users`].
    pub async fn clear_user_credentials(&self, matrix_user_id: &str) -> Result<()> {
        sqlx::query("UPDATE users SET jmap_token = '' WHERE matrix_user_id = ?")
            .bind(matrix_user_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Count a user's bridged conversation rooms, for the `status` command.
    pub async fn count_bridged_rooms(&self, matrix_user_id: &str) -> Result<i64> {
        sqlx::query_scalar::<Sqlite, i64>(
            "SELECT COUNT(*) FROM room_ghost_mapping WHERE matrix_user_id = ?",
        )
        .bind(matrix_user_id)
        .fetch_one(&self.pool)
        .await
        .map_err(Into::into)
    }

    /// Record the time of the most recent successful JMAP sync (RFC 3339), for
    /// the `status` command. Stored in the generic kv table.
    pub async fn set_last_sync(&self, matrix_user_id: &str, ts: &str) -> Result<()> {
        self.save_jmap_state(matrix_user_id, "last_sync_at", ts)
            .await
    }

    /// Read the time of the most recent successful JMAP sync, if any.
    pub async fn get_last_sync(&self, matrix_user_id: &str) -> Result<Option<String>> {
        self.get_jmap_state(matrix_user_id, "last_sync_at").await
    }

    /// Set the user's send-delay (undo) window in seconds (ADR-0012).
    pub async fn set_send_delay(&self, matrix_user_id: &str, secs: i64) -> Result<()> {
        self.save_jmap_state(matrix_user_id, "send_delay_seconds", &secs.to_string())
            .await
    }

    /// Read the user's send-delay override in seconds, if they've set one.
    /// `None` means "use the bridge default".
    pub async fn get_send_delay(&self, matrix_user_id: &str) -> Result<Option<i64>> {
        Ok(self
            .get_jmap_state(matrix_user_id, "send_delay_seconds")
            .await?
            .and_then(|s| s.parse::<i64>().ok()))
    }

    fn decrypt_user(&self, user: RegisteredUser) -> Result<RegisteredUser> {
        if let Some(key) = &self.encryption_key {
            let jmap_username =
                crypto::decrypt(&user.jmap_username, key).context("Failed to decrypt username")?;
            let jmap_token =
                crypto::decrypt(&user.jmap_token, key).context("Failed to decrypt token")?;
            Ok(RegisteredUser {
                matrix_user_id: user.matrix_user_id,
                jmap_username,
                jmap_token,
                jmap_url: user.jmap_url,
            })
        } else {
            Ok(user)
        }
    }

    /// Store a user's Matrix double-puppet access token, encrypted at rest if a
    /// Store the user's own primary email address (from their JMAP identity),
    /// used to label their email space. Plaintext in the kv table.
    pub async fn set_user_email(&self, matrix_user_id: &str, email: &str) -> Result<()> {
        self.save_jmap_state(matrix_user_id, "user_email", email)
            .await
    }

    /// Read the user's own primary email address, if known.
    pub async fn get_user_email(&self, matrix_user_id: &str) -> Result<Option<String>> {
        self.get_jmap_state(matrix_user_id, "user_email").await
    }

    /// Store the room id of the user's "email" space — the parent that collects
    /// all their bridged conversation rooms. Created lazily on first room.
    pub async fn set_email_space_room(&self, matrix_user_id: &str, room_id: &str) -> Result<()> {
        self.save_jmap_state(matrix_user_id, "email_space_room", room_id)
            .await
    }

    /// Read the user's email space room id, if it has been created.
    pub async fn get_email_space_room(&self, matrix_user_id: &str) -> Result<Option<String>> {
        self.get_jmap_state(matrix_user_id, "email_space_room")
            .await
    }

    /// key is configured. Reuses the generic state kv table so no schema change
    /// is needed. The user row must already exist (FK on `jmap_state`).
    pub async fn set_matrix_puppet_token(&self, matrix_user_id: &str, token: &str) -> Result<()> {
        let stored = if let Some(key) = &self.encryption_key {
            crypto::encrypt(token, key).context("Failed to encrypt puppet token")?
        } else {
            token.to_owned()
        };
        self.save_jmap_state(matrix_user_id, crate::puppet::PUPPET_TOKEN_KEY, &stored)
            .await
    }

    /// Read a user's stored Matrix double-puppet access token, if any.
    pub async fn get_matrix_puppet_token(&self, matrix_user_id: &str) -> Result<Option<String>> {
        let Some(stored) = self
            .get_jmap_state(matrix_user_id, crate::puppet::PUPPET_TOKEN_KEY)
            .await?
        else {
            return Ok(None);
        };
        if let Some(key) = &self.encryption_key {
            Ok(Some(
                crypto::decrypt(&stored, key).context("Failed to decrypt puppet token")?,
            ))
        } else {
            Ok(Some(stored))
        }
    }

    /// Save the user's custom signature.
    pub async fn set_user_signature(&self, matrix_user_id: &str, signature: &str) -> Result<()> {
        sqlx::query(
            "INSERT OR REPLACE INTO user_signatures (matrix_user_id, signature) VALUES (?, ?)",
        )
        .bind(matrix_user_id)
        .bind(signature)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Retrieve the user's custom signature.
    pub async fn get_user_signature(&self, matrix_user_id: &str) -> Result<Option<String>> {
        sqlx::query_scalar::<Sqlite, String>(
            "SELECT signature FROM user_signatures WHERE matrix_user_id = ?",
        )
        .bind(matrix_user_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(Into::into)
    }

    /// Delete the user's custom signature.
    pub async fn delete_user_signature(&self, matrix_user_id: &str) -> Result<()> {
        sqlx::query("DELETE FROM user_signatures WHERE matrix_user_id = ?")
            .bind(matrix_user_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
