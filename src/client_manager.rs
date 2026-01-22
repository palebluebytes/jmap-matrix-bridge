use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use anyhow::{Result, anyhow};
use crate::store::{Store, RegisteredUser};
use crate::ingest::JmapPoller;
use crate::matrix::MatrixClient;
use jmap_client::client::Client;

pub struct ClientManager {
    store: Store,
    matrix: MatrixClient,
    // Map of Matrix User ID -> JmapClient (for sending)
    clients: RwLock<HashMap<String, Arc<Client>>>,
}

impl ClientManager {
    pub fn new(store: Store, matrix: MatrixClient) -> Self {
        Self {
            store,
            matrix,
            clients: RwLock::new(HashMap::new()),
        }
    }

    pub async fn start(&self) -> Result<()> {
        let users = self.store.get_all_users().await?;
        tracing::info!("Found {} registered users in database.", users.len());
        for user in users {
            if let Err(e) = self.spawn_user(user.clone()).await {
                tracing::error!("Failed to spawn session for {}: {}", user.matrix_user_id, e);
            }
        }
        Ok(())
    }

    pub async fn login(&self, matrix_user_id: String, username: String, token: String, url: String) -> Result<()> {
         // Create client to verify credentials
         let auth = format!("{}:{}", username, token);
         use base64::{Engine as _, engine::general_purpose};
         let encoded = general_purpose::STANDARD.encode(auth);

         // Just verify we can connect authentication works (implied by connect usually)
         // Note: jmap-client connect() performs a session retrieval.
         let _test_client = Client::new()
             .credentials(jmap_client::client::Credentials::Basic(encoded))
             .connect(&url)
             .await
             .map_err(|e| anyhow!("Failed to connect to JMAP: {}", e))?;

         // Save to DB
         let user = RegisteredUser {
             matrix_user_id: matrix_user_id.clone(),
             jmap_username: username,
             jmap_token: token,
             jmap_url: url,
         };
         self.store.save_user(&user).await?;

         // Spawn persistent session
         self.spawn_user(user).await?;
         Ok(())
    }

    async fn spawn_user(&self, user: RegisteredUser) -> Result<()> {
        let auth = format!("{}:{}", user.jmap_username, user.jmap_token);
        use base64::{Engine as _, engine::general_purpose};
        let encoded = general_purpose::STANDARD.encode(auth);

        let client = Client::new()
            .credentials(jmap_client::client::Credentials::Basic(encoded))
            .connect(&user.jmap_url)
            .await?;
        let client = Arc::new(client);

        // Update map
        {
            let mut clients = self.clients.write().await;
            clients.insert(user.matrix_user_id.clone(), client.clone());
        }

        // Spawn poller
        let poller = JmapPoller::new(client.clone(), self.matrix.clone(), self.store.clone()).await?;

        let user_id = user.matrix_user_id.clone();
        tokio::spawn(async move {
            loop {
                if let Err(e) = poller.poll().await {
                   tracing::error!("Polling error for user {}: {}", user_id, e);
                }
                tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
            }
        });

        tracing::info!("Started session for {}", user.matrix_user_id);
        Ok(())
    }

    pub async fn get_client(&self, matrix_user_id: &str) -> Option<Arc<Client>> {
        let clients = self.clients.read().await;
        clients.get(matrix_user_id).cloned()
    }
}
