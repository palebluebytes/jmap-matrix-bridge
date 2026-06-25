use crate::commands::{Command, CommandContext};
use crate::routes::{AppState, notify};
use anyhow::Result;
use tracing::error;

/// `sync` — force an immediate JMAP reconcile and repair the user's email space
/// (re-file any rooms that drifted out of it). Does not re-run a full historical
/// backfill.
#[derive(Debug)]
pub struct SyncCommand;

impl Command for SyncCommand {
    fn matches(&self, ctx: &CommandContext<'_>) -> bool {
        let t = ctx.body_str.trim();
        t == "sync" || t == "!sync"
    }

    fn execute<'a>(
        &'a self,
        state: &'a AppState,
        ctx: &'a CommandContext<'a>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            if state
                .client_manager
                .get_client(ctx.sender_id)
                .await
                .is_none()
            {
                notify(
                    state,
                    ctx.room_id,
                    "Not logged in. Type `login` to connect your JMAP account.",
                )
                .await;
                return Ok(());
            }

            notify(state, ctx.room_id, "Syncing…").await;

            // Reconcile mail first, then repair the space. Each is independent —
            // a failure in one shouldn't skip the other.
            if let Err(e) = state.client_manager.poll_now(ctx.sender_id).await {
                error!(sender = %ctx.sender_id, error = %e, "Manual sync poll failed");
            }
            if let Err(e) = state.client_manager.repair_space(ctx.sender_id).await {
                error!(sender = %ctx.sender_id, error = %e, "Manual space repair failed");
            }

            notify(
                state,
                ctx.room_id,
                "Sync complete — reconciled mail and re-filed your rooms into the email space.",
            )
            .await;
            Ok(())
        })
    }
}
