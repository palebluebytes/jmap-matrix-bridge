use crate::commands::{Command, CommandContext};
use crate::routes::{AppState, notify};
use anyhow::Result;

/// `status` (alias `ping`) — a read-only health report for the requesting user:
/// connection state, last sync, pending-send depth, double-puppet, room count.
#[derive(Debug)]
pub struct StatusCommand;

impl Command for StatusCommand {
    fn matches(&self, ctx: &CommandContext<'_>) -> bool {
        let trimmed = ctx.body_str.trim();
        matches!(trimmed, "status" | "!status" | "ping" | "!ping")
    }

    fn execute<'a>(
        &'a self,
        state: &'a AppState,
        ctx: &'a CommandContext<'a>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            let store = &state.client_manager.store;
            let sender = ctx.sender_id;

            let logged_in = store.get_user(sender).await?.is_some();
            if !logged_in {
                notify(
                    state,
                    ctx.room_id,
                    "Not logged in. Type `login` to connect your JMAP account.",
                )
                .await;
                return Ok(());
            }

            // The in-memory client is the live-session signal: present means the
            // event loop is running and the JMAP session connected.
            let connected = state.client_manager.get_client(sender).await.is_some();
            let last_sync = store
                .get_last_sync(sender)
                .await?
                .unwrap_or_else(|| "never".to_owned());
            let queued = store.count_outbound_queue(sender).await?;
            let rooms = store.count_bridged_rooms(sender).await?;
            let double_puppet = store.get_matrix_puppet_token(sender).await?.is_some();

            let report = format!(
                "Bridge status:\n\
                 • JMAP session: {}\n\
                 • Last sync: {last_sync}\n\
                 • Pending outbound: {queued}\n\
                 • Bridged rooms: {rooms}\n\
                 • Double-puppet: {}",
                if connected {
                    "connected"
                } else {
                    "disconnected (reconnecting)"
                },
                if double_puppet { "on" } else { "off" },
            );
            notify(state, ctx.room_id, &report).await;
            Ok(())
        })
    }
}
