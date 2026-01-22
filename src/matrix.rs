use anyhow::{Result, Context};
use tracing::info;
use serde_json::json;

#[derive(Clone)]
pub struct MatrixClient {
    client: reqwest::Client,
    homeserver_url: String,
    as_token: String,
}

impl MatrixClient {
    pub fn new(homeserver_url: &str, as_token: &str) -> Self {
        Self {
            client: reqwest::Client::new(),
            homeserver_url: homeserver_url.trim_end_matches('/').to_string(),
            as_token: as_token.to_string(),
        }
    }

    pub async fn ensure_user_exists(&self, localpart: &str) -> Result<()> {
        info!("Ensuring Matrix user exists: {}", localpart);
        let url = format!("{}/_matrix/client/v3/register", self.homeserver_url);
        
        let body = json!({
            "username": localpart,
            "type": "m.login.application_service"
        });

        // We don't check the result too strictly because 400 M_USER_IN_USE is expected
        let _ = self.client.post(&url)
            .header("Authorization", format!("Bearer {}", self.as_token))
            .json(&body)
            .send()
            .await?;
            
        Ok(())
    }

    pub async fn create_room_for_thread(&self, thread_subject: &str, inviter_user_id: &str) -> Result<String> {
        info!("Creating room for thread: {}", thread_subject);
        let url = format!("{}/_matrix/client/v3/createRoom", self.homeserver_url);
        
        // Impersonate the bridge bot or specific user
        let user_id = format!("@_jmap_bot:palebluebytes.xyz"); // TODO: Make domain configurable
        
        let body = json!({
            "name": thread_subject,
            "topic": format!("Email Thread: {}", thread_subject),
            "preset": "private_chat",
            "invite": [inviter_user_id],
            "is_direct": true
        });

        let resp = self.client.post(&url)
            .header("Authorization", format!("Bearer {}", self.as_token))
            .query(&[("user_id", &user_id)]) // AS Impersonation
            .json(&body)
            .send()
            .await?
            .error_for_status()
            .context("Failed to create room")?;

        let json: serde_json::Value = resp.json().await?;
        let room_id = json["room_id"].as_str()
            .context("Response missing room_id")?
            .to_string();
            
        Ok(room_id)
    }

    pub async fn create_room_for_mailbox(&self, mailbox_name: &str) -> Result<String> {
        info!("Creating room for mailbox: {}", mailbox_name);
        let url = format!("{}/_matrix/client/v3/createRoom", self.homeserver_url);
        
        let user_id = format!("@_jmap_bot:palebluebytes.xyz"); 
        
        let body = json!({
            "name": mailbox_name,
            "topic": format!("Mailbox: {}", mailbox_name),
            "preset": "public_chat", // Mailboxes are public to the space? Or private? Let's say public for now for ease.
            "is_direct": false
        });

        let resp = self.client.post(&url)
            .header("Authorization", format!("Bearer {}", self.as_token))
            .query(&[("user_id", &user_id)]) 
            .json(&body)
            .send()
            .await?
            .error_for_status()
            .context("Failed to create room")?;

        let json: serde_json::Value = resp.json().await?;
        let room_id = json["room_id"].as_str()
            .context("Response missing room_id")?
            .to_string();
            
        Ok(room_id)
    }

    pub async fn send_message(&self, room_id: &str, body_text: &str) -> Result<String> {
        // Simple Txn ID
        let txn_id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
            .to_string();

        let url = format!("{}/_matrix/client/v3/rooms/{}/send/m.room.message/{}", self.homeserver_url, room_id, txn_id);
        
        let user_id = format!("@_jmap_bot:palebluebytes.xyz"); 

        let body = json!({
            "msgtype": "m.text",
            "body": body_text
        });

        let resp = self.client.put(&url) // Use PUT for txn ID
            .header("Authorization", format!("Bearer {}", self.as_token))
            .query(&[("user_id", &user_id)]) 
            .json(&body)
            .send()
            .await?
            .error_for_status()
            .context("Failed to send message")?;

        let json: serde_json::Value = resp.json().await?;
        let event_id = json["event_id"].as_str()
            .context("Response missing event_id")?
            .to_string();
            
        Ok(event_id)
    }
}
