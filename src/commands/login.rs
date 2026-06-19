use crate::commands::{Command, CommandContext};
use crate::routes::{AppState, notify};
use crate::state::LoginState;
use anyhow::Result;
use tracing::{error, info};

#[derive(Debug)]
pub struct LoginCommand;

impl Command for LoginCommand {
    fn matches(&self, ctx: &CommandContext<'_>) -> bool {
        let trimmed = ctx.body_str.trim();
        trimmed == "login" || trimmed == "!login" || ctx.body_str.starts_with("!login ")
    }

    fn execute<'a>(
        &'a self,
        state: &'a AppState,
        ctx: &'a CommandContext<'a>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            let trimmed = ctx.body_str.trim();
            if trimmed == "login" || trimmed == "!login" {
                state
                    .state_store
                    .set_login_state(ctx.sender_id, LoginState::WaitingForEmail)
                    .await;
                notify(state, ctx.room_id, "Please enter your email address:").await;
                return Ok(());
            }

            if ctx.body_str.starts_with("!login ") {
                return handle_one_shot_login(
                    state,
                    ctx.sender_id,
                    ctx.body_str,
                    ctx.room_id,
                    ctx.event_id,
                )
                .await;
            }

            Ok(())
        })
    }
}

async fn handle_one_shot_login(
    state: &AppState,
    sender_id: &str,
    body_str: &str,
    room_id: Option<&str>,
    event_id: Option<&str>,
) -> Result<()> {
    // Redact immediately to protect credentials
    if let (Some(ev_id), Some(rm_id)) = (event_id, room_id) {
        let _ = state
            .client_manager
            .matrix
            .redact_event(rm_id, ev_id, "Removing plain-text credentials")
            .await;
    }

    let parts: Vec<&str> = body_str.split_whitespace().collect();
    if parts.len() < 4 {
        notify(state, room_id, "Usage: !login <username> <token> <url>").await;
        return Ok(());
    }

    let username = parts[1];
    let password = parts[2];
    let url = parts[3];

    match state
        .client_manager
        .login(
            sender_id.to_owned(),
            username.to_owned(),
            password.to_owned(),
            url.to_owned(),
        )
        .await
    {
        Ok(()) => {
            info!("Login successful for {sender_id}");
            notify(state, room_id, "Successfully logged in!").await;
        }
        Err(e) => {
            error!("Login failed for {sender_id}: {e}");
            notify(state, room_id, &format!("Login failed: {e}")).await;
        }
    }
    Ok(())
}

// ─── Login Flow Steps ─────────────────────────────────────────────────────────

pub async fn handle_login_waiting_for_email(
    state: &AppState,
    sender_id: &str,
    body_str: &str,
    room_id: Option<&str>,
) -> Result<()> {
    let email = body_str.trim().to_owned();
    tracing::debug!("Interactive login step: user {sender_id} entered email");
    state
        .state_store
        .set_login_state(sender_id, LoginState::WaitingForPassword { email })
        .await;
    notify(
        state,
        room_id,
        "Great! Now enter your JMAP password or API token:",
    )
    .await;
    Ok(())
}

pub async fn handle_login_waiting_for_password(
    state: &AppState,
    sender_id: &str,
    body_str: &str,
    room_id: Option<&str>,
    event_id: Option<&str>,
    email: &str,
) -> Result<()> {
    tracing::debug!(
        "Interactive login step: user {} entered password (redacted)",
        sender_id
    );
    if let (Some(ev_id), Some(rm_id)) = (event_id, room_id) {
        let _ = state
            .client_manager
            .matrix
            .redact_event(rm_id, ev_id, "Removing password")
            .await;
    }
    let password = body_str.trim().to_owned();
    state
        .state_store
        .set_login_state(
            sender_id,
            LoginState::WaitingForUrl {
                email: email.to_owned(),
                password,
            },
        )
        .await;
    notify(
        state,
        room_id,
        "Finally, enter your JMAP Session URL (e.g. https://jmap.example.com/.well-known/jmap):",
    )
    .await;
    Ok(())
}

pub async fn handle_login_waiting_for_url(
    state: &AppState,
    sender_id: &str,
    body_str: &str,
    room_id: Option<&str>,
    email: &str,
    password: &str,
) -> Result<()> {
    let url = body_str.trim().to_owned();
    tracing::debug!("Interactive login step: user {sender_id} entered URL");
    notify(state, room_id, "Attempting to log in...").await;

    match state
        .client_manager
        .login(
            sender_id.to_owned(),
            email.to_owned(),
            password.to_owned(),
            url,
        )
        .await
    {
        Ok(()) => {
            state.state_store.clear_login_state(sender_id).await;
            notify(state, room_id, "Success! You are now logged in.").await;
        }
        Err(e) => {
            // Log the detail server-side, but return a GENERIC message to the
            // user: echoing the connection error turns this user-supplied-URL
            // login into a network reachability oracle (SSRF probing aid).
            error!("Interactive login failed: {e}");
            notify(
                state,
                room_id,
                "Login failed: could not log in with those details. \
                 Please try again from the start by typing `login`.",
            )
            .await;
            state.state_store.clear_login_state(sender_id).await;
        }
    }
    Ok(())
}
