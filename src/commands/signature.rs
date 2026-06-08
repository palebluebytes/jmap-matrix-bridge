use crate::commands::{Command, CommandContext};
use crate::routes::{AppState, notify};
use anyhow::Result;
use tracing::error;

#[derive(Debug)]
pub struct SignatureCommand;

impl Command for SignatureCommand {
    fn matches(&self, ctx: &CommandContext<'_>) -> bool {
        ctx.body_str.starts_with("!signature") || ctx.body_str.starts_with("signature")
    }

    #[allow(clippy::too_many_lines)]
    fn execute<'a>(
        &'a self,
        state: &'a AppState,
        ctx: &'a CommandContext<'a>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            let trimmed = ctx.body_str.trim();
            tracing::debug!(
                "Processing signature command for user {}: {trimmed}",
                ctx.sender_id
            );
            let command_args = if trimmed.starts_with("!signature") {
                trimmed.strip_prefix("!signature").unwrap_or("").trim()
            } else {
                trimmed.strip_prefix("signature").unwrap_or("").trim()
            };

            if command_args.is_empty() {
                match state
                    .client_manager
                    .store
                    .get_user_signature(ctx.sender_id)
                    .await
                {
                    Ok(Some(sig)) => {
                        if sig.is_empty() {
                            notify(
                                state,
                                ctx.room_id,
                                "You currently have no signature configured.\n\nTo set one, type:\nsignature <your signature text>",
                            )
                            .await;
                        } else {
                            notify(
                                state,
                                ctx.room_id,
                                &format!("Your current signature is:\n\n-- \n{sig}"),
                            )
                            .await;
                        }
                    }
                    Ok(None) => {
                        notify(
                            state,
                            ctx.room_id,
                            "You currently have no signature configured.\n\nTo set one, type:\nsignature <your signature text>",
                        )
                        .await;
                    }
                    Err(e) => {
                        error!("Failed to fetch signature: {e}");
                        notify(
                            state,
                            ctx.room_id,
                            &format!("Failed to fetch signature: {e}"),
                        )
                        .await;
                    }
                }
            } else if command_args == "clear" {
                match state
                    .client_manager
                    .store
                    .delete_user_signature(ctx.sender_id)
                    .await
                {
                    Ok(()) => {
                        notify(
                            state,
                            ctx.room_id,
                            "Your signature has been successfully cleared!",
                        )
                        .await;
                    }
                    Err(e) => {
                        error!("Failed to clear signature: {e}");
                        notify(
                            state,
                            ctx.room_id,
                            &format!("Failed to clear signature: {e}"),
                        )
                        .await;
                    }
                }
            } else {
                // Strip optional surrounding quotes if the user typed them
                let sig_text = if (command_args.starts_with('"') && command_args.ends_with('"'))
                    || (command_args.starts_with('\'') && command_args.ends_with('\''))
                {
                    &command_args[1..command_args.len() - 1]
                } else {
                    command_args
                };

                match state
                    .client_manager
                    .store
                    .set_user_signature(ctx.sender_id, sig_text)
                    .await
                {
                    Ok(()) => {
                        notify(
                            state,
                            ctx.room_id,
                            &format!(
                                "Signature updated successfully!\n\nIt will appear on outbound emails as:\n\n-- \n{sig_text}"
                            ),
                        )
                        .await;
                    }
                    Err(e) => {
                        error!("Failed to set signature: {e}");
                        notify(state, ctx.room_id, &format!("Failed to set signature: {e}")).await;
                    }
                }
            }

            Ok(())
        })
    }
}
