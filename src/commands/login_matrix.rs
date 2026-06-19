use crate::commands::{Command, CommandContext};
use crate::routes::{AppState, notify};
use anyhow::Result;
use tracing::info;

/// `login-matrix <access-token>` — store a Matrix access token for the sender
/// so the bridge can double-puppet them and auto-accept its room invites.
#[derive(Debug)]
pub struct LoginMatrixCommand;

impl Command for LoginMatrixCommand {
    fn matches(&self, ctx: &CommandContext<'_>) -> bool {
        let t = ctx.body_str.trim_start();
        t.starts_with("login-matrix") || t.starts_with("!login-matrix")
    }

    fn execute<'a>(
        &'a self,
        state: &'a AppState,
        ctx: &'a CommandContext<'a>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            // The message contains an access token — redact it immediately.
            if let (Some(ev_id), Some(rm_id)) = (ctx.event_id, ctx.room_id) {
                let _ = state
                    .client_manager
                    .matrix
                    .redact_event(rm_id, ev_id, "Removing Matrix access token")
                    .await;
            }

            let token = ctx
                .body_str
                .split_whitespace()
                .nth(1)
                .map(str::to_owned)
                .filter(|t| !t.is_empty());
            let Some(token) = token else {
                notify(
                    state,
                    ctx.room_id,
                    "Usage: `login-matrix <access-token>` — paste a Matrix access token \
                     (in Element: Settings → Help & About → Access Token) so I can \
                     auto-accept invites to your email rooms.",
                )
                .await;
                return Ok(());
            };

            let homeserver = state.client_manager.matrix.homeserver_url.clone();

            // Validate the token and make sure it belongs to the sender — never
            // let one user puppet a different account.
            match crate::puppet::whoami(&homeserver, &token).await {
                Ok(id) if id == ctx.sender_id => {}
                Ok(_) => {
                    notify(
                        state,
                        ctx.room_id,
                        "That access token belongs to a different account. \
                         Provide a token for your own account.",
                    )
                    .await;
                    return Ok(());
                }
                Err(e) => {
                    notify(
                        state,
                        ctx.room_id,
                        &format!("That access token was rejected: {e}"),
                    )
                    .await;
                    return Ok(());
                }
            }

            // The token is stored against the user's row, so they must be
            // JMAP-logged-in first.
            if state
                .client_manager
                .store
                .get_user(ctx.sender_id)
                .await?
                .is_none()
            {
                notify(
                    state,
                    ctx.room_id,
                    "Please `login` to connect your JMAP account first, then run \
                     `login-matrix` again.",
                )
                .await;
                return Ok(());
            }

            if let Err(e) = state
                .client_manager
                .store
                .set_matrix_puppet_token(ctx.sender_id, &token)
                .await
            {
                notify(
                    state,
                    ctx.room_id,
                    &format!("Failed to store the token: {e}"),
                )
                .await;
                return Ok(());
            }

            state
                .puppet_manager
                .ensure_running(ctx.sender_id.to_owned(), token)
                .await;
            info!(
                "Enabled Matrix double-puppet auto-join for {}",
                ctx.sender_id
            );
            notify(
                state,
                ctx.room_id,
                "Done — I'll auto-accept invites to your email rooms from now on.",
            )
            .await;
            Ok(())
        })
    }
}
