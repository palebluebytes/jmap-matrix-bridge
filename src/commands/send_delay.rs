use crate::client_manager::MAX_SEND_DELAY_SECS;
use crate::commands::{Command, CommandContext};
use crate::routes::{AppState, notify};
use anyhow::Result;
use tracing::error;

/// `send-delay <seconds>` / `send-delay off` — set the per-user undo window
/// before outbound mail is submitted (ADR-0012). No argument shows the current
/// value. Capped at [`MAX_SEND_DELAY_SECS`].
#[derive(Debug)]
pub struct SendDelayCommand;

impl Command for SendDelayCommand {
    fn matches(&self, ctx: &CommandContext<'_>) -> bool {
        let t = ctx.body_str.trim();
        t == "send-delay"
            || t == "!send-delay"
            || t.starts_with("send-delay ")
            || t.starts_with("!send-delay ")
    }

    fn execute<'a>(
        &'a self,
        state: &'a AppState,
        ctx: &'a CommandContext<'a>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            let store = &state.client_manager.store;
            let arg = ctx
                .body_str
                .trim()
                .trim_start_matches('!')
                .trim_start_matches("send-delay")
                .trim();

            // No argument: report the effective window.
            if arg.is_empty() {
                let effective = state.client_manager.send_delay_for(ctx.sender_id).await;
                notify(
                    state,
                    ctx.room_id,
                    &format!(
                        "Send delay is {effective}s. Use `send-delay <seconds>` (0–{MAX_SEND_DELAY_SECS}) or `send-delay off`."
                    ),
                )
                .await;
                return Ok(());
            }

            let secs: i64 = if arg.eq_ignore_ascii_case("off") {
                0
            } else {
                match arg.parse::<i64>() {
                    Ok(n) if (0..=MAX_SEND_DELAY_SECS).contains(&n) => n,
                    Ok(_) => {
                        notify(
                            state,
                            ctx.room_id,
                            &format!(
                                "Send delay must be between 0 and {MAX_SEND_DELAY_SECS} seconds."
                            ),
                        )
                        .await;
                        return Ok(());
                    }
                    Err(_) => {
                        notify(
                            state,
                            ctx.room_id,
                            "Usage: `send-delay <seconds>` or `send-delay off`.",
                        )
                        .await;
                        return Ok(());
                    }
                }
            };

            if let Err(e) = store.set_send_delay(ctx.sender_id, secs).await {
                error!(sender = %ctx.sender_id, error = %e, "Failed to set send delay");
                notify(state, ctx.room_id, "Failed to save send delay.").await;
                return Ok(());
            }

            let msg = if secs == 0 {
                "Send delay turned off — mail sends immediately. Redact a message to unsend only while it's still queued.".to_owned()
            } else {
                format!(
                    "Send delay set to {secs}s. Redact within the window to unsend, or edit to change the text."
                )
            };
            notify(state, ctx.room_id, &msg).await;
            Ok(())
        })
    }
}
