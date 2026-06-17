//! JMAP client session manager.
//!
//! [`ClientManager`] owns one JMAP [`Client`] per registered Matrix user and
//! spawns a background [`run_event_loop`] task that receives JMAP push events
//! and triggers a debounced [`JmapPoller::poll`] to sync new mail into Matrix.

use anyhow::{Result, anyhow};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;

use futures_util::StreamExt;
use jmap_client::DataType;
use jmap_client::client::Client;

use crate::ingest::JmapPoller;
use crate::matrix::MatrixClient;
use crate::services::content::RenderMode;
use crate::store::{RegisteredUser, Store};

// ─── ClientManager ────────────────────────────────────────────────────────────

/// Manages JMAP client sessions per Matrix user.
///
/// Each registered user gets a persistent JMAP [`Client`] for sending and a
/// background event loop that drives [`JmapPoller::poll`].  The task handle is
/// tracked so that re-login cleanly cancels the old task before spawning a new
/// one, preventing a race where two pollers run for the same user.
pub struct ClientManager {
    pub(crate) store: Store,
    pub(crate) matrix: MatrixClient,
    /// Map of Matrix User ID → JMAP Client (for sending).
    pub(crate) clients: RwLock<HashMap<String, Arc<Client>>>,
    /// Map of Matrix User ID → [`JoinHandle`] for their background event loop.
    poller_handles: RwLock<HashMap<String, JoinHandle<()>>>,
    pub(crate) sync_limit: usize,
    pub(crate) bridge_mailboxes: bool,
    pub(crate) render_mode: RenderMode,
    /// When true, outbound threaded replies carry a quoted-original of the
    /// parent message (read by the reply `JmapSender` construction sites).
    pub(crate) quote_replies: bool,
}

impl std::fmt::Debug for ClientManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientManager")
            .field("store", &self.store)
            .field("matrix", &self.matrix)
            .field("sync_limit", &self.sync_limit)
            .finish_non_exhaustive()
    }
}

impl ClientManager {
    #[must_use]
    #[allow(clippy::missing_const_for_fn)]
    pub fn new(store: Store, matrix: MatrixClient, sync_limit: usize) -> Self {
        Self {
            store,
            matrix,
            clients: RwLock::new(HashMap::new()),
            poller_handles: RwLock::new(HashMap::new()),
            sync_limit,
            bridge_mailboxes: false,
            render_mode: RenderMode::default(),
            quote_replies: false,
        }
    }

    /// Enable mirroring JMAP mailboxes (Inbox/Sent/…) as their own Matrix rooms.
    /// Off by default — email content lives in per-contact/per-thread rooms.
    #[must_use]
    pub const fn with_bridge_mailboxes(mut self, enabled: bool) -> Self {
        self.bridge_mailboxes = enabled;
        self
    }

    /// Set how email bodies are rendered into Matrix messages (plain/links/rich).
    #[must_use]
    pub const fn with_render_mode(mut self, mode: RenderMode) -> Self {
        self.render_mode = mode;
        self
    }

    /// Enable quoting the parent message in outbound threaded replies. Off by
    /// default — read at the reply `JmapSender` construction sites.
    #[must_use]
    pub const fn with_quote_replies(mut self, enabled: bool) -> Self {
        self.quote_replies = enabled;
        self
    }

    /// Load all registered users and start their sync sessions.
    ///
    /// This is called once on bridge startup. It uses a concurrency limit
    /// (throttling) to avoid overwhelming the JMAP server or the local system
    /// if there are many users.
    pub async fn start(self: Arc<Self>) -> Result<()> {
        let users = self.store.get_all_users().await?;
        tracing::info!("Found {} registered users in database.", users.len());

        let concurrency_limit = 10;
        tracing::info!(
            "Starting user sessions with concurrency limit of {}...",
            concurrency_limit
        );

        futures_util::stream::iter(users)
            .map(|user| {
                let manager = self.clone();
                async move {
                    let user_id = user.matrix_user_id.clone();

                    // Add startup connection jitter (between 50ms and 250ms) to prevent handshake storm
                    let jitter_ms = rand::random::<u64>() % 200 + 50;
                    tokio::time::sleep(tokio::time::Duration::from_millis(jitter_ms)).await;

                    if let Err(e) = manager.spawn_user(user.clone()).await {
                        tracing::error!("Failed to spawn session for {}: {}. Starting background reconnect task...", user_id, e);

                        let manager_reconnect = manager.clone();

                        tokio::spawn(async move {
                            let mut backoff = tokio::time::Duration::from_secs(5);
                            let max_backoff = tokio::time::Duration::from_secs(600); // 10 minutes max
                            loop {
                                tokio::time::sleep(backoff).await;
                                tracing::info!("Retrying background JMAP connection for {}...", user_id);
                                match manager_reconnect.spawn_user(user.clone()).await {
                                    Ok(()) => {
                                        tracing::info!("Successfully reconnected and spawned session for {}", user_id);
                                        break;
                                    }
                                    Err(err) => {
                                        tracing::error!("Failed to reconnect for {}: {}. Retrying in {:?}", user_id, err, backoff);
                                        backoff = std::cmp::min(backoff * 2, max_backoff);
                                    }
                                }
                            }
                        });
                    }
                }
            })
            .buffer_unordered(concurrency_limit)
            .collect::<()>()
            .await;

        Ok(())
    }

    /// Authenticate a new user and start their session.
    ///
    /// Connects once to verify credentials, saves the user to the database,
    /// then passes the already-verified client into [`spawn_user`] so we do
    /// not reconnect immediately.
    pub async fn login(
        &self,
        matrix_user_id: String,
        username: String,
        token: String,
        url: String,
    ) -> Result<()> {
        // Connect once — both verifies credentials AND gives us the client.
        let user = RegisteredUser {
            matrix_user_id: matrix_user_id.clone(),
            jmap_username: username,
            jmap_token: token,
            jmap_url: url,
        };

        let client = self
            .connect_jmap(&user.jmap_username, &user.jmap_token, &user.jmap_url)
            .await?;
        self.store.save_user(&user).await?;

        // Cache the user's own email address (from their JMAP identity) so the
        // email space can be labelled with it. Best-effort — a failure here must
        // not block the session.
        if let Some(email) = Self::fetch_primary_email(&client).await {
            if let Err(e) = self.store.set_user_email(&matrix_user_id, &email).await {
                tracing::warn!(error = %e, "Failed to store primary email for {matrix_user_id}");
            }
        } else {
            tracing::warn!("Could not determine primary email for {matrix_user_id}");
        }

        // Abort any existing poller and start a fresh session.
        self.abort_poller(&matrix_user_id).await;
        self.start_session(user, Arc::new(client)).await;
        Ok(())
    }

    /// Fetch the first email address from the account's JMAP identities, used to
    /// label the user's email space. Returns `None` if the server exposes no
    /// identity with an email (e.g. some admin accounts).
    async fn fetch_primary_email(client: &Client) -> Option<String> {
        let mut request = client.build();
        request.get_identity();
        let response = request
            .send()
            .await
            .ok()?
            .pop_method_response()?
            .unwrap_get_identity()
            .ok()?;
        response
            .list()
            .iter()
            .find_map(|identity| identity.email())
            .map(str::to_owned)
    }

    /// Look up the JMAP client for `matrix_user_id` (for sending operations).
    pub async fn get_client(&self, matrix_user_id: &str) -> Option<Arc<Client>> {
        self.clients.read().await.get(matrix_user_id).cloned()
    }

    /// Abort the background event loop for a given user (if any).
    ///
    /// Safe to call even if no loop is running for that user.
    pub async fn abort_poller(&self, matrix_user_id: &str) {
        let handle = {
            let mut handles = self.poller_handles.write().await;
            handles.remove(matrix_user_id)
        };
        if let Some(h) = handle {
            tracing::info!("Aborting event loop for {}", matrix_user_id);
            h.abort();
            let _ = h.await; // JoinError is expected after abort — ignore it.
        }
    }

    /// Gracefully shut down all background event loops.
    pub async fn shutdown(&self) {
        tracing::info!("Shutting down all JMAP pollers...");
        let user_ids: Vec<String> = self.poller_handles.read().await.keys().cloned().collect();
        for uid in user_ids {
            self.abort_poller(&uid).await;
        }
        tracing::info!("All pollers shut down.");
    }

    // ── Internal ──────────────────────────────────────────────────────────────

    async fn spawn_user(&self, user: RegisteredUser) -> Result<()> {
        let client = self
            .connect_jmap(&user.jmap_username, &user.jmap_token, &user.jmap_url)
            .await?;
        self.abort_poller(&user.matrix_user_id).await;
        self.start_session(user, Arc::new(client)).await;
        Ok(())
    }

    /// Register `client` in the client map and spawn the event loop task.
    async fn start_session(&self, user: RegisteredUser, client: Arc<Client>) {
        let user_id = user.matrix_user_id.clone();

        let syncer = JmapPoller::new(
            user_id.clone(),
            client.clone(),
            self.matrix.clone(),
            self.store.clone(),
            self.sync_limit,
            self.bridge_mailboxes,
            self.render_mode,
        );

        self.clients
            .write()
            .await
            .insert(user_id.clone(), client.clone());

        let handle = tokio::spawn(run_event_loop(client, syncer, user_id.clone()));
        self.poller_handles.write().await.insert(user_id, handle);

        tracing::info!("Started session for {}", user.matrix_user_id);
    }

    async fn connect_jmap(&self, username: &str, token: &str, url: &str) -> Result<Client> {
        // jmap-client refuses to follow any redirect during session discovery
        // unless the destination host is in its trusted-hosts list (default:
        // empty). Stalwart's `/.well-known/jmap` always 307-redirects to
        // `/jmap/session`, so without trusting the connect host, discovery
        // aborts with "Aborting redirect request to unknown host". The redirect
        // is same-host, so trusting the URL's host is sufficient.
        let mut builder =
            Client::new().credentials((username.to_owned(), token.to_owned()));
        if let Some(host) = reqwest::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_owned))
        {
            builder = builder.follow_redirects([host]);
        }
        builder
            .connect(url)
            .await
            .map_err(|e| anyhow!("Failed to connect to JMAP: {e}"))
    }
}

struct TaskGuard(tokio::task::JoinHandle<()>);

impl Drop for TaskGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

// ─── Event loop ───────────────────────────────────────────────────────────────

/// Background task that subscribes to JMAP push events and calls
/// [`JmapPoller::poll`] whenever the server signals a state change.
///
/// Uses a debounce channel so that a burst of server-sent events only triggers
/// one poll, avoiding thundering-herd problems on initial sync.
async fn run_event_loop(client: Arc<Client>, poller: JmapPoller, user_id: String) {
    let types = vec![DataType::Email, DataType::Mailbox];
    let mut retry_delay = tokio::time::Duration::from_secs(1);

    // Debounce channel: the SSE listener sends a unit and the poller task
    // drains duplicates and waits for a quiet period before calling poll().
    let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(100);

    // 1. Trigger initial sync on startup.
    let _ = tx.send(()).await;

    // 2. Debounced poll task.
    let poller_clone = poller.clone();
    let uid_clone = user_id.clone();
    let _poller_task = TaskGuard(tokio::spawn(async move {
        while rx.recv().await.is_some() {
            // Drain any events that arrived while we were asleep.
            while rx.try_recv().is_ok() {}

            // Wait for a quiet period to avoid multiple polls during a burst of emails.
            tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

            if let Err(e) = poller_clone.poll().await {
                // `{:#}` prints the full anyhow context chain (e.g. the
                // underlying JMAP method error), not just the outermost
                // "Mailbox sync failed" wrapper.
                tracing::error!("Sync error for user {}: {:#}", uid_clone, e);
            }
        }
    }));

    // 3. Background backfill task: catches up on older email history over time.
    let poller_backfill = poller.clone();
    let _backfill_task = TaskGuard(tokio::spawn(async move {
        poller_backfill.run_backfill_loop().await;
    }));

    // 4. SSE listener loop with exponential backoff.
    loop {
        match client
            .event_source(types.clone().into(), false, 60.into(), None)
            .await
        {
            Ok(mut stream) => {
                tracing::info!("Subscribed to JMAP EventSource for user {}", user_id);
                retry_delay = tokio::time::Duration::from_secs(1);

                loop {
                    let next_event =
                        tokio::time::timeout(tokio::time::Duration::from_secs(3600), stream.next())
                            .await;

                    match next_event {
                        Ok(Some(event_result)) => {
                            match event_result {
                                Ok(event) => {
                                    tracing::debug!(
                                        "Received JMAP event for {}: {:?}",
                                        user_id,
                                        event
                                    );
                                    let _ = tx.send(()).await;
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        "Error in JMAP EventSource stream for {}: {}",
                                        user_id,
                                        e
                                    );
                                    break; // Exit inner loop to reconnect
                                }
                            }
                        }
                        Ok(None) => {
                            tracing::warn!(
                                "EventSource stream ended for {}, reconnecting…",
                                user_id
                            );
                            break; // Exit inner loop to reconnect
                        }
                        Err(_) => {
                            tracing::warn!(
                                "No JMAP push events received for 1 hour for user {}. Triggering heartbeat poll and reconnecting stream…",
                                user_id
                            );
                            let _ = tx.send(()).await;
                            break; // Exit inner loop to reconnect
                        }
                    }
                }
            }
            Err(e) => {
                tracing::error!(
                    "Failed to connect to JMAP EventSource for {}: {}. Retrying in {:?}…",
                    user_id,
                    e,
                    retry_delay
                );
                tokio::time::sleep(retry_delay).await;
                retry_delay = retry_delay
                    .saturating_mul(2)
                    .min(tokio::time::Duration::from_secs(60));
            }
        }
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_client_manager_initialization() {
        let store = Store::new_in_memory(None).await.unwrap();
        let matrix = MatrixClient::new("http://localhost", "token", "localhost")
            .await
            .unwrap();
        let cm = ClientManager::new(store, matrix, 10);
        assert!(cm.clients.read().await.is_empty());
        assert!(cm.poller_handles.read().await.is_empty());
    }

    #[tokio::test]
    async fn test_abort_poller_nonexistent_user() {
        let store = Store::new_in_memory(None).await.unwrap();
        let matrix = MatrixClient::new("http://localhost", "token", "localhost")
            .await
            .unwrap();
        let cm = ClientManager::new(store, matrix, 10);
        // Should not panic for an unknown user.
        cm.abort_poller("@nonexistent:localhost").await;
    }

    #[tokio::test]
    async fn test_shutdown_with_no_pollers() {
        let store = Store::new_in_memory(None).await.unwrap();
        let matrix = MatrixClient::new("http://localhost", "token", "localhost")
            .await
            .unwrap();
        let cm = ClientManager::new(store, matrix, 10);
        cm.shutdown().await;
    }
}
