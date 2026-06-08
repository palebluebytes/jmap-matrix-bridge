use crate::commands::{Command, CommandContext};
use crate::routes::{AppState, notify};
use crate::sender::JmapSender;
use anyhow::Result;
use tracing::error;

#[derive(Debug)]
pub struct EmailCommand;

impl Command for EmailCommand {
    fn matches(&self, ctx: &CommandContext<'_>) -> bool {
        ctx.body_str.starts_with("!email ")
    }

    fn execute<'a>(
        &'a self,
        state: &'a AppState,
        ctx: &'a CommandContext<'a>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            let parts: Vec<&str> = ctx.body_str.splitn(4, ' ').collect();
            if parts.len() < 4 {
                notify(state, ctx.room_id, "Usage: !email <to> <subject> <body>").await;
                return Ok(());
            }

            let to = parts[1];
            let subject = parts[2];
            let email_body = parts[3];

            let Some(client) = state.client_manager.get_client(ctx.sender_id).await else {
                notify(
                    state,
                    ctx.room_id,
                    "You are not logged in. Type `login` to connect.",
                )
                .await;
                return Ok(());
            };

            let mut final_body = email_body.to_owned();
            let _ = crate::services::content::append_user_signature(
                &state.client_manager.store,
                ctx.sender_id,
                &mut final_body,
            )
            .await;

            let sender = JmapSender::new(client);
            match sender.send_email(to, subject, &final_body, vec![]).await {
                Ok(_) => notify(state, ctx.room_id, "Email sent successfully!").await,
                Err(e) => {
                    error!("Failed to send email: {e}");
                    notify(state, ctx.room_id, &format!("Failed to send email: {e}")).await;
                }
            }
            Ok(())
        })
    }
}
