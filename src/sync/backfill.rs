use super::JmapPoller;
use anyhow::{Context, Result};
use tracing::{info, warn};

impl JmapPoller {
    /// Performs a background backfill catch-up process for older emails.
    /// It queries one batch of emails at a time and sleeps to throttle server load.
    pub async fn run_backfill_loop(&self) {
        // Initial delay to avoid storming the server on startup/login
        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

        loop {
            // Check if initial sync has occurred
            let has_initial_sync = match self
                .store
                .get_jmap_state(&self.matrix_user_id, "changes")
                .await
            {
                Ok(state) => state.is_some(),
                Err(e) => {
                    warn!(user = %self.matrix_user_id, error = %e, "Failed to check changes state from store");
                    false
                }
            };

            let pos_opt = match self
                .store
                .get_jmap_state(&self.matrix_user_id, "backfill_position")
                .await
            {
                Ok(pos) => pos,
                Err(e) => {
                    warn!(user = %self.matrix_user_id, error = %e, "Failed to retrieve backfill position from store");
                    None
                }
            };

            let Some(pos_str) = pos_opt else {
                if has_initial_sync {
                    info!(user = %self.matrix_user_id, "Initial sync complete and no backfill position found. Terminating backfill task.");
                    break;
                }
                // If initial sync has not occurred yet, check again later
                tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
                continue;
            };

            let pos: usize = match pos_str.parse() {
                Ok(p) => p,
                Err(e) => {
                    warn!(user = %self.matrix_user_id, error = %e, "Failed to parse backfill position; resetting backfill");
                    let _ = self
                        .store
                        .delete_jmap_state(&self.matrix_user_id, "backfill_position")
                        .await;
                    continue;
                }
            };

            info!(user = %self.matrix_user_id, position = pos, "Starting background email backfill batch");

            match self.backfill_batch(pos).await {
                Ok(has_more) => {
                    if !has_more {
                        info!(user = %self.matrix_user_id, "No more historical emails found. Backfill completed successfully. Terminating backfill task.");
                        let _ = self
                            .store
                            .delete_jmap_state(&self.matrix_user_id, "backfill_position")
                            .await;
                        break;
                    }
                    // Wait 5 seconds between batches to throttle load
                    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                }
                Err(e) => {
                    warn!(user = %self.matrix_user_id, error = %e, "Email backfill batch failed; retrying in 30 seconds");
                    tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
                }
            }
        }
    }

    /// Backfills a single batch of emails from the specified position.
    /// Returns `Ok(true)` if there might be more emails to fetch, or `Ok(false)` if reached the end.
    pub async fn backfill_batch(&self, pos: usize) -> Result<bool> {
        let mut request = self.client.build();
        let email_query = request.query_email();
        // Ascending (oldest-first): Element's room list orders by the server
        // stream position of each room's last message (sliding-sync bump_stamp),
        // NOT the message's origin_server_ts. Bridging oldest-first means the
        // newest email is processed last and gets the highest stream position,
        // so the list sorts newest-first like a mail client. (Ascending paging
        // is also stable: new mail lands at the high end, never shifting the
        // positions we're walking.)
        email_query
            .sort([jmap_client::email::query::Comparator::received_at().ascending()])
            .position(i32::try_from(pos).context("Position overflow")?)
            .limit(self.sync_limit);
        email_query.arguments().collapse_threads(false);

        let mut response = request
            .send()
            .await?
            .pop_method_response()
            .context("Empty response for Email/query (backfill)")?
            .unwrap_query_email()?;

        let ids = response.take_ids();
        if ids.is_empty() {
            return Ok(false);
        }

        info!(
            user = %self.matrix_user_id,
            position = pos,
            count = ids.len(),
            "Retrieved {} emails for backfill",
            ids.len()
        );

        let emails = self.fetch_emails(&ids).await?;
        for email in &emails {
            if let Err(e) = self.process_email(email).await {
                warn!(user = %self.matrix_user_id, error = %e, "Failed to process backfilled email");
            }
        }

        if ids.len() < self.sync_limit {
            Ok(false)
        } else {
            let next_pos = pos + ids.len();
            self.store
                .save_jmap_state(
                    &self.matrix_user_id,
                    "backfill_position",
                    &next_pos.to_string(),
                )
                .await?;
            info!(user = %self.matrix_user_id, next_position = next_pos, "Updated backfill position in database");
            Ok(true)
        }
    }
}
