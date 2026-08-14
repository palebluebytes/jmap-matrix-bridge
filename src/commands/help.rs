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
            // `send-delay` is deliberately absent: it still dispatches, but the
            // hold window it configures is WIP (ADR-0012), so we don't advertise
            // it. Restore the line when the feature is viable.
            let help_text = "Available commands:\n\nlogin - Start interactive login\nlogout - Disconnect your JMAP account (keeps your rooms)\nstatus - Show bridge connection/sync status (alias: ping)\nsync - Reconcile mail now and re-file your rooms into the email space\nhelp - Show this message\nsignature <text> - Set custom signature\nsignature clear - Clear signature\n!compose <address> [subject] - Start a new email conversation (then just type)\n!email <to> <subject> <body> - Send a one-off email\nshow-images - In an email room, reply to a message with this (or react 🖼️) to load its remote images\ndelete-room - In an email room, move the whole thread to Trash and unbridge it (or react 🗑)\nspam - In an email room, move the whole thread to Junk and unbridge it (or react 🚫)";
            notify(state, ctx.room_id, help_text).await;
            Ok(())
        })
    }
}
