use crate::commands::{Command, CommandContext};
use crate::routes::{AppState, notify};
use anyhow::Result;

#[derive(Debug)]
pub struct HelpCommand;

impl Command for HelpCommand {
    fn matches(&self, ctx: &CommandContext<'_>) -> bool {
        let trimmed = ctx.body_str.trim();
        trimmed == "help" || trimmed == "!help"
    }

    fn execute<'a>(
        &'a self,
        state: &'a AppState,
        ctx: &'a CommandContext<'a>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            let help_text = "Available commands:\n\nlogin - Start interactive login\nlogout - Disconnect your JMAP account (keeps your rooms)\nstatus - Show bridge connection/sync status (alias: ping)\nhelp - Show this message\nsignature <text> - Set custom signature\nsignature clear - Clear signature\n!compose <address> [subject] - Start a new email conversation (then just type)\n!email <to> <subject> <body> - Send a one-off email\nshow-images - In an email room, reply to a message with this (or react 🖼️) to load its remote images";
            notify(state, ctx.room_id, help_text).await;
            Ok(())
        })
    }
}
