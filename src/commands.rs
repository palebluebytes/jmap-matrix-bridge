//! Matrix → JMAP command parsing and interactive login flow.

pub mod compose;
pub mod email;
pub mod help;
pub mod login;
pub mod login_matrix;
pub mod reply;
pub mod signature;

use crate::permissions::Level;
use crate::routes::{AppState, notify};
use anyhow::Result;
use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;

#[derive(Debug)]
pub struct CommandContext<'a> {
    pub sender_id: &'a str,
    pub body_str: &'a str,
    pub room_id: Option<&'a str>,
    pub event_id: Option<&'a str>,
    pub message_content: &'a RoomMessageEventContent,
}

pub trait Command: Send + Sync + std::fmt::Debug {
    /// Returns true if the command matches the input.
    fn matches(&self, ctx: &CommandContext<'_>) -> bool;

    /// The minimum permission [`Level`] a sender needs to run this command.
    /// Defaults to [`Level::User`]; destructive/global commands override to
    /// [`Level::Admin`] ([ADR-0010](../docs/adr/0010-permission-model.md)).
    fn min_level(&self) -> Level {
        Level::User
    }

    /// Executes the command.
    fn execute<'a>(
        &'a self,
        state: &'a AppState,
        ctx: &'a CommandContext<'a>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>>;
}

/// Router to dispatch messages to registered commands.
#[derive(Debug)]
pub struct CommandRouter {
    commands: Vec<Box<dyn Command>>,
}

impl Default for CommandRouter {
    fn default() -> Self {
        Self::new()
    }
}

impl CommandRouter {
    #[must_use]
    pub fn new() -> Self {
        Self {
            commands: vec![
                Box::new(help::HelpCommand),
                Box::new(login_matrix::LoginMatrixCommand),
                Box::new(login::LoginCommand),
                Box::new(compose::ComposeCommand),
                Box::new(email::EmailCommand),
                Box::new(signature::SignatureCommand),
                Box::new(reply::ReplyCommand),
            ],
        }
    }

    /// Dispatch context to the first matching command, enforcing the command's
    /// minimum permission [`Level`] against the sender's resolved `level`.
    pub async fn dispatch(
        &self,
        state: &AppState,
        ctx: &CommandContext<'_>,
        level: Level,
    ) -> Result<()> {
        for cmd in &self.commands {
            if cmd.matches(ctx) {
                if level < cmd.min_level() {
                    notify(
                        state,
                        ctx.room_id,
                        "That command requires admin permission on this bridge.",
                    )
                    .await;
                    return Ok(());
                }
                return cmd.execute(state, ctx).await;
            }
        }
        Ok(())
    }

    /// Check if any command matches.
    #[must_use]
    pub fn matches_any(&self, ctx: &CommandContext<'_>) -> bool {
        self.commands.iter().any(|cmd| cmd.matches(ctx))
    }
}

// Re-export original helper entrypoints to keep other modules unmodified
pub async fn handle_login_none(
    state: &AppState,
    sender_id: &str,
    body_str: &str,
    room_id: Option<&str>,
    event_id: Option<&str>,
    message_content: &RoomMessageEventContent,
) -> Result<()> {
    let ctx = CommandContext {
        sender_id,
        body_str,
        room_id,
        event_id,
        message_content,
    };
    let router = CommandRouter::new();

    // Default-deny access control (ADR-0010). A sender matching no permission
    // entry may not use the bridge at all. We only answer apparent command
    // attempts so arbitrary chatter from an unpermitted sender isn't spammed
    // with refusals.
    let Some(level) = state.permissions.level_for(sender_id) else {
        if router.matches_any(&ctx) {
            notify(
                state,
                room_id,
                "You are not permitted to use this bridge. Ask the operator for access.",
            )
            .await;
        }
        return Ok(());
    };

    router.dispatch(state, &ctx, level).await
}

// Re-export login state handlers from sub-module
pub use login::{
    handle_login_waiting_for_email, handle_login_waiting_for_password, handle_login_waiting_for_url,
};
