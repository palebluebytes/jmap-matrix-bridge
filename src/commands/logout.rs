use crate::commands::{Command, CommandContext};
use crate::routes::{AppState, notify};
use anyhow::Result;
use tracing::error;

/// `logout` — disconnect the user's JMAP account, keeping their rooms so a later
/// `login` resumes in place (ADR-0012).
#[derive(Debug)]
pub struct LogoutCommand;

impl Command for LogoutCommand {
    fn matches(&self, ctx: &CommandContext<'_>) -> bool {
        let trimmed = ctx.body_str.trim();
        trimmed == "logout" || trimmed == "!logout"
    }

    fn execute<'a>(
        &'a self,
        state: &'a AppState,
        ctx: &'a CommandContext<'a>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            // Nothing to do if there's no active session — but say so rather
            // than silently no-op, so the user isn't left guessing.
            if state
                .client_manager
                .get_client(ctx.sender_id)
                .await
                .is_none()
                && state
                    .client_manager
                    .store
                    .get_user(ctx.sender_id)
                    .await?
                    .is_none()
            {
                notify(state, ctx.room_id, "You are not logged in.").await;
                return Ok(());
            }

            if let Err(e) = state.client_manager.logout(ctx.sender_id).await {
                error!(sender = %ctx.sender_id, error = %e, "Logout failed");
                notify(
                    state,
                    ctx.room_id,
                    "Logout failed — please try again or contact the operator.",
                )
                .await;
                return Ok(());
            }

            notify(
                state,
                ctx.room_id,
                "Logged out. Your rooms are kept — type `login` to reconnect. Any unsent mail was discarded.",
            )
            .await;
            Ok(())
        })
    }
}
