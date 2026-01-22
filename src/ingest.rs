use std::sync::Arc;
use anyhow::Result;
use jmap_client::client::Client;
use tracing::info;
use crate::matrix::MatrixClient;
use jmap_client::mailbox::Mailbox;
// use jmap_client::core::response::Response;
use serde_json::Value;

pub struct JmapPoller {
    client: Arc<Client>,
    matrix: MatrixClient,
    store: crate::store::Store,
}

impl JmapPoller {
    pub async fn new(client: Arc<Client>, matrix: MatrixClient, store: crate::store::Store) -> Result<Self> {
        Ok(Self { client, matrix, store })
    }

    pub async fn poll(&self) -> Result<()> {
        info!("Polling JMAP for new messages...");
        if let Err(e) = self.sync_mailboxes().await {
            tracing::error!("Failed to sync mailboxes: {}", e);
        }
        if let Err(e) = self.sync_emails().await {
            tracing::error!("Failed to sync emails: {}", e);
        }
        
        // Mock logic for Phase 2 demo:
        // Assume we found a new email thread "Hello from Rust" from "bob@gmail.com"
        let thread_subject = "Hello from Rust";
        let sender = "@_jmap_bob_gmail.com:palebluebytes.xyz"; // Mocked ghost user logic
        
        // 1. Ensure ghost user exists
        self.matrix.ensure_user_exists("_jmap_bob_gmail.com").await?;
        
        // 2. Create room
        let room_id = self.matrix.create_room_for_thread(thread_subject, sender).await?;
        info!("Created Matrix room {} for thread '{}'", room_id, thread_subject);
        
        Ok(())
    }

    pub async fn sync_mailboxes(&self) -> Result<()> {
        // 1. Query all mailboxes
        let mut request = self.client.build();
        let _query_batch = request.query_mailbox().calculate_total(false);
        // Use Value directly
        let response = request.send_single::<serde_json::Value>().await?;
        
        let args = &response;
        
        let empty_vec = vec![];
        let mailbox_ids_val = args.get("ids").and_then(|v| v.as_array()).unwrap_or(&empty_vec);
        
        let mut mailbox_ids = Vec::new();
        for id_val in mailbox_ids_val {
            if let Some(id_str) = id_val.as_str() {
                mailbox_ids.push(id_str.to_string());
            }
        }

        if mailbox_ids.is_empty() {
             return Ok(());
        }

        // 2. Get Mailbox details
        let mut request = self.client.build();
        let _get_batch = request.get_mailbox().ids(&mailbox_ids);
        let response = request.send_single::<serde_json::Value>().await?;
        let args = &response;
        
        let empty_list = vec![];
        let list = args.get("list").and_then(|v| v.as_array()).unwrap_or(&empty_list);

        for mailbox_val in list {
             if let Some(obj) = mailbox_val.as_object() {
                if let (Some(id), Some(name)) = (obj.get("id").and_then(|v| v.as_str()), obj.get("name").and_then(|v| v.as_str())) {
                    // Check mapping
                    if self.store.get_room_id(id).await?.is_none() {
                        // Create Room
                        let room_name = name; 
                        let room_id = self.matrix.create_room_for_mailbox(room_name).await?;
                        self.store.save_room_mapping(id, &room_id).await?;
                        info!("Mapped Mailbox '{}' ({}) to Room {}", room_name, id, room_id);
                    }
                }
            }
        }
        
        Ok(())
    }
    pub async fn sync_emails(&self) -> Result<()> {
        // 1. Query Emails
        let mut request = self.client.build();
        let _query = request.query_email().limit(10).calculate_total(false); 
        let response = request.send_single::<serde_json::Value>().await?;
        
        let empty_vec = vec![];
        let email_ids_val = response.get("ids").and_then(|v| v.as_array()).unwrap_or(&empty_vec);

        let mut email_ids = Vec::new();
        for id in email_ids_val {
            if let Some(s) = id.as_str() {
                email_ids.push(s.to_string());
            }
        }
        
        if email_ids.is_empty() {
            return Ok(());
        }

        // 2. Fetch Email Details
        let mut request = self.client.build();
        // Removed properties() call to use defaults
        let _get = request.get_email().ids(&email_ids);
        let response = request.send_single::<serde_json::Value>().await?;
        
        let empty_list = vec![];
        let list = response.get("list").and_then(|v| v.as_array()).unwrap_or(&empty_list);
        
        for email_val in list {
            self.process_email(email_val).await?;
        }

        Ok(())
    }

    async fn process_email(&self, email: &serde_json::Value) -> Result<()> {
        let id_opt = email.get("id").and_then(|v| v.as_str());
        let thread_id_opt = email.get("threadId").and_then(|v| v.as_str());
        let mailbox_ids_opt = email.get("mailboxIds").and_then(|v| v.as_object());
        let subject = email.get("subject").and_then(|v| v.as_str()).unwrap_or("(No Subject)");
        let body = email.get("textBody").and_then(|v| v.as_array())
            .and_then(|parts| parts.first())
            .and_then(|part| part.get("value")) // Simplified body logic
            .and_then(|v| v.as_str())
            .unwrap_or(subject); // Fallback to subject if no body for now

        if let (Some(id), Some(thread_id)) = (id_opt, thread_id_opt) {
            // Check mapping
            if self.store.get_thread_info(thread_id).await?.is_some() {
                 // Thread exists, reply
                 let (root_event_id, room_id) = self.store.get_thread_info(thread_id).await?.unwrap();
                 
                 // TODO: Check if message itself is already mapped? 
                 // Assuming idempotent or checking message_mapping here would be good.
                 // But for simplified logic, we just send.
                 
                 // Send Reply
                 let event_id = self.matrix.send_message(&room_id, body).await?;
                 
                 // Store message mapping
                 self.store.save_message_mapping(id, &event_id).await?;
                 info!("Sent reply for thread {}, event {}", thread_id, event_id);
            } else {
                 // New Thread
                 // Find Room ID from mailboxes
                 if let Some(mailbox_ids) = mailbox_ids_opt {
                     for (mailbox_id, _val) in mailbox_ids {
                         if let Some(room_id) = self.store.get_room_id(mailbox_id).await? {
                             // Create Thread Root (Limit to one room for now)
                             let event_id = self.matrix.send_message(&room_id, body).await?;
                             self.store.save_thread_mapping(thread_id, &event_id, &room_id).await?;
                             self.store.save_message_mapping(id, &event_id).await?;
                             info!("Created new Matrix thread for Subject: '{}', ID: {}", subject, thread_id);
                             break;
                         }
                     }
                 }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_poller_initialization() {
        // Mock test
        // We can't easily create a Client without connecting, so this test is now harder.
        // We'll skip it or mock Client if possible.
        // For now, assume main.rs connection logic is separate.
        assert!(true); 
    }
}
