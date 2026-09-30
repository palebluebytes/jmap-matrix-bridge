//! Background ingestion and bridging of JMAP events into Matrix.

pub mod backfill;
pub mod email;
pub mod mailbox;

use crate::matrix::MatrixClient;
use crate::services::content::RenderMode;
use crate::store::Store;
use anyhow::{Context, Result};
use jmap_client::client::Client;
use std::sync::Arc;

/// Drives the JMAP → Matrix sync loop for a single authenticated user.
#[derive(Clone)]
pub struct JmapPoller {
    pub(crate) client: Arc<Client>,
    pub(crate) matrix: MatrixClient,
    pub(crate) store: Store,
    pub(crate) matrix_user_id: String,
    pub(crate) sync_limit: usize,
    /// When false, JMAP mailboxes (Inbox/Sent/Drafts/…) are not mirrored as
    /// their own Matrix rooms — email content lives in per-contact rooms, so
    /// those rooms are just clutter.
    pub(crate) bridge_mailboxes: bool,
    /// How email bodies are rendered into Matrix messages (plain / links / rich).
    pub(crate) render_mode: RenderMode,
    /// Optional age limit on historical backfill: only mail received within this
    /// span of the walk's start is bridged. `None` backfills the whole mailbox.
    pub(crate) backfill_window: Option<jiff::Span>,
}

impl std::fmt::Debug for JmapPoller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JmapPoller")
            .field("matrix", &self.matrix)
            .field("store", &self.store)
            .field("matrix_user_id", &self.matrix_user_id)
            .field("sync_limit", &self.sync_limit)
            .field(
                "backfill_window",
                &self.backfill_window.map(|w| w.to_string()),
            )
            .finish_non_exhaustive()
    }
}

impl JmapPoller {
    #[must_use]
    pub const fn new(
        matrix_user_id: String,
        client: Arc<Client>,
        matrix: MatrixClient,
        store: Store,
        sync_limit: usize,
        bridge_mailboxes: bool,
        render_mode: RenderMode,
    ) -> Self {
        Self {
            client,
            matrix,
            store,
            matrix_user_id,
            sync_limit,
            bridge_mailboxes,
            render_mode,
            backfill_window: None,
        }
    }

    /// Limit historical backfill to mail received within `window` of the walk's
    /// start. `None` (the default) backfills the entire mailbox.
    #[must_use]
    pub const fn with_backfill_window(mut self, window: Option<jiff::Span>) -> Self {
        self.backfill_window = window;
        self
    }

    /// Primary entry point for the poller. Synchronizes mailboxes and emails.
    pub async fn poll(&self) -> Result<()> {
        tracing::info!(user = %self.matrix_user_id, "Starting JMAP poll");

        if self.bridge_mailboxes {
            self.sync_mailboxes().await.context("Mailbox sync failed")?;
        }
        self.sync_emails().await.context("Email sync failed")?;

        // Record a successful-sync timestamp for the `status` command.
        // Best-effort: a kv write failure must not fail the poll.
        if let Err(e) = self
            .store
            .set_last_sync(&self.matrix_user_id, &jiff::Timestamp::now().to_string())
            .await
        {
            tracing::warn!(error = %e, "Failed to record last-sync time");
        }

        Ok(())
    }
}

pub(crate) struct GhostUser {
    pub(crate) email: String,
    pub(crate) user_id: String,
}
