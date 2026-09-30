use super::JmapPoller;
use anyhow::{Context, Result};
use tracing::{info, warn};

/// `jmap_state` key holding the persisted `receivedAt` floor of a windowed
/// backfill, as epoch seconds. Shares the lifecycle of `backfill_position`.
const BACKFILL_CUTOFF_KEY: &str = "backfill_cutoff";

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
                        // Drop the window anchor too, so a later walk (after a
                        // `matrix-reset`, say) anchors afresh rather than reusing
                        // a cutoff from months ago.
                        let _ = self
                            .store
                            .delete_jmap_state(&self.matrix_user_id, BACKFILL_CUTOFF_KEY)
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

    /// Resolve the `receivedAt` floor for a windowed backfill, `None` when the
    /// operator set no window.
    ///
    /// The cutoff is **persisted on first use** rather than recomputed per batch,
    /// because the walk is positional: `position` indexes into the *filtered*
    /// result set, so a cutoff that crept forward between batches would shrink the
    /// set from the front and make the saved position point past mail it had not
    /// reached yet — silently skipping it. Restarts are the common case, not a rare
    /// one (every deploy restarts the unit, and at the default `sync_limit` of 10
    /// with a 5s throttle a large mailbox takes hours), so an in-memory anchor
    /// would not have been enough.
    pub(crate) async fn resolve_backfill_cutoff(&self) -> Option<i64> {
        let window = self.backfill_window?;

        match self
            .store
            .get_jmap_state(&self.matrix_user_id, BACKFILL_CUTOFF_KEY)
            .await
        {
            Ok(Some(saved)) => match saved.parse::<i64>() {
                Ok(cutoff) => return Some(cutoff),
                Err(e) => {
                    warn!(user = %self.matrix_user_id, error = %e, saved = %saved, "Unparseable backfill cutoff; re-anchoring");
                }
            },
            Ok(None) => {}
            Err(e) => {
                // Without the store we cannot anchor stably. Fall through and
                // anchor from now: a fresh cutoff is far better than dropping the
                // window and replaying the entire mailbox into Matrix.
                warn!(user = %self.matrix_user_id, error = %e, "Failed to read backfill cutoff; anchoring from now");
            }
        }

        // Calendar units (`1mo`, `1y`) are not fixed numbers of seconds, so the
        // subtraction has to happen on a zoned datetime rather than a bare
        // timestamp. UTC is deliberate: the anchor is machine state, not something
        // a user reads, and a DST-shifted local zone would only add ambiguity.
        let now = jiff::Timestamp::now().to_zoned(jiff::tz::TimeZone::UTC);
        let cutoff = match now.checked_sub(window) {
            Ok(t) => t.timestamp().as_second(),
            Err(e) => {
                warn!(user = %self.matrix_user_id, error = %e, "Backfill window overflowed; backfilling without a window");
                return None;
            }
        };

        if let Err(e) = self
            .store
            .save_jmap_state(
                &self.matrix_user_id,
                BACKFILL_CUTOFF_KEY,
                &cutoff.to_string(),
            )
            .await
        {
            warn!(user = %self.matrix_user_id, error = %e, "Failed to persist backfill cutoff; it will be re-anchored next restart");
        }
        info!(user = %self.matrix_user_id, cutoff, window = %format_args!("{window:#}"), "Anchored windowed backfill");
        Some(cutoff)
    }

    /// Backfills a single batch of emails from the specified position.
    /// Returns `Ok(true)` if there might be more emails to fetch, or `Ok(false)` if reached the end.
    pub async fn backfill_batch(&self, pos: usize) -> Result<bool> {
        let cutoff = self.resolve_backfill_cutoff().await;

        let mut request = self.client.build();
        let email_query = request.query_email();
        if let Some(cutoff) = cutoff {
            // JMAP `after` is `receivedAt >= cutoff`, matching the sort key below —
            // deliberately not `sentAfter`, which filters on the Date: header and
            // would disagree with the ordering we page by.
            email_query.filter(jmap_client::email::query::Filter::after(cutoff));
        }
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
