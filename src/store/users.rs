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
        sqlx::query(
            "INSERT OR REPLACE INTO users (matrix_user_id, jmap_username, jmap_token, jmap_url) VALUES (?, ?, ?, ?)"
        )
        .bind(&user.matrix_user_id)
        .bind(&username)
        .bind(&token)
        .bind(&user.jmap_url)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_user(&self, matrix_user_id: &str) -> Result<Option<RegisteredUser>> {
        sqlx::query_as::<_, RegisteredUser>(
            "SELECT matrix_user_id, jmap_username, jmap_token, jmap_url FROM users WHERE matrix_user_id = ?"
        )
        .bind(matrix_user_id)
        .fetch_optional(&self.pool)
        .await?
        .map(|u| self.decrypt_user(u))
        .transpose()
    }

    pub async fn get_all_users(&self) -> Result<Vec<RegisteredUser>> {
        sqlx::query_as::<_, RegisteredUser>(
            "SELECT matrix_user_id, jmap_username, jmap_token, jmap_url FROM users",
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|u| self.decrypt_user(u))
        .collect()
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
