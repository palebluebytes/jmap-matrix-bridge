//! `!compose` — start an email conversation with a new address.
//!
//! Lets the user email someone the bridge has never received mail from. It
//! provisions the same kind of ghost room the inbound poller creates, so the
//! user can then just type into it like any other contact room and have their
//! messages sent as email.

use crate::commands::{Command, CommandContext};
use crate::routes::{AppState, notify};
use anyhow::Result;
use tracing::error;

#[derive(Debug)]
pub struct ComposeCommand;

/// True if `body` invokes `name` as a whole command word (`name` alone, or
/// `name` followed by whitespace) — so `!compose` matches but `!composed`
/// doesn't.
fn is_command_word(body: &str, name: &str) -> bool {
    body == name
        || body
            .strip_prefix(name)
            .is_some_and(|rest| rest.starts_with(char::is_whitespace))
}

/// Parse `!compose <address> [subject]` into `(address, optional subject)`.
/// Returns `None` if no address was given.
fn parse_compose(body: &str) -> Option<(&str, Option<&str>)> {
    let body = body.trim_start();
    let rest = ["!compose", "!email-to"]
        .iter()
        .find_map(|p| body.strip_prefix(p))?
        .trim_start();
    let mut parts = rest.splitn(2, char::is_whitespace);
    let address = parts.next().unwrap_or("").trim();
    if address.is_empty() {
        return None;
    }
    let subject = parts.next().map(str::trim).filter(|s| !s.is_empty());
    Some((address, subject))
}

/// Minimal sanity check: exactly one `@`, non-empty localpart, and a domain that
/// contains a dot and isn't bounded by one. Enough to reject obvious typos
/// before we create a room and a ghost user for it.
fn looks_like_email(addr: &str) -> bool {
    let mut parts = addr.split('@');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(local), Some(domain), None) => {
            !local.is_empty()
                && domain.len() >= 3
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.ends_with('.')
        }
        _ => false,
    }
}

impl Command for ComposeCommand {
    fn matches(&self, ctx: &CommandContext<'_>) -> bool {
        let body = ctx.body_str.trim_start();
        is_command_word(body, "!compose") || is_command_word(body, "!email-to")
    }

    fn execute<'a>(
        &'a self,
        state: &'a AppState,
        ctx: &'a CommandContext<'a>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            let Some((address, subject_opt)) = parse_compose(ctx.body_str) else {
                notify(state, ctx.room_id, "Usage: !compose <address> [subject]").await;
                return Ok(());
            };

            if !looks_like_email(address) {
                notify(
                    state,
                    ctx.room_id,
                    &format!("'{address}' doesn't look like an email address."),
                )
                .await;
                return Ok(());
            }

            // Must have a JMAP session to send from.
            if state
                .client_manager
                .get_client(ctx.sender_id)
                .await
                .is_none()
            {
                notify(
                    state,
                    ctx.room_id,
                    "You are not logged in. Type `login` to connect.",
                )
                .await;
                return Ok(());
            }

            // Each `!compose` starts a new conversation room (one room per email
            // chain), via the same helper the inbound poller uses. The display
            // name defaults to the address since we've never seen this contact.
            let room_id = match crate::ghost::create_contact_room(
                &state.client_manager.matrix,
                &state.client_manager.store,
                ctx.sender_id,
                address,
                address,
            )
            .await
            {
                Ok(room_id) => room_id,
                Err(e) => {
                    error!("Failed to open conversation with {address}: {e}");
                    notify(
                        state,
                        ctx.room_id,
                        &format!("Couldn't open a conversation with {address}: {e}"),
                    )
                    .await;
                    return Ok(());
                }
            };

            // Name the room after the subject so the first outbound email uses
            // it (see ghost::fresh_email_subject).
            let subject = subject_opt.unwrap_or("Matrix Conversation");
            if let Err(e) = state
                .client_manager
                .matrix
                .set_room_name(&room_id, &crate::services::content::clean_subject(subject))
                .await
            {
                tracing::warn!(error = %e, "Failed to set composed room name");
            }

            notify(
                state,
                Some(&room_id),
                &format!(
                    "Opened a conversation with {address}. Type your message here to send it as an email."
                ),
            )
            .await;
            // If the command was issued from a different room (e.g. the bot
            // room), point the user at the new conversation.
            if ctx.room_id != Some(room_id.as_str()) {
                notify(
                    state,
                    ctx.room_id,
                    &format!("Opened a conversation with {address} — open the new room to write."),
                )
                .await;
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{is_command_word, looks_like_email, parse_compose};

    #[test]
    fn matches_only_whole_command_word() {
        assert!(is_command_word("!compose", "!compose"));
        assert!(is_command_word("!compose a@b.com", "!compose"));
        assert!(!is_command_word("!composed now", "!compose"));
        assert!(!is_command_word("please !compose", "!compose"));
    }

    #[test]
    fn parses_address_and_optional_subject() {
        assert_eq!(parse_compose("!compose a@b.com"), Some(("a@b.com", None)));
        assert_eq!(
            parse_compose("!compose a@b.com Hello there"),
            Some(("a@b.com", Some("Hello there")))
        );
        assert_eq!(
            parse_compose("!email-to a@b.com Subject"),
            Some(("a@b.com", Some("Subject")))
        );
        // No address -> None (caller shows usage).
        assert_eq!(parse_compose("!compose"), None);
        assert_eq!(parse_compose("!compose    "), None);
    }

    #[test]
    fn validates_email_addresses() {
        assert!(looks_like_email("thomassdk@pm.me"));
        assert!(looks_like_email("a@b.co"));
        assert!(!looks_like_email("notanemail"));
        assert!(!looks_like_email("two@@at.com"));
        assert!(!looks_like_email("no@domain"));
        assert!(!looks_like_email("@b.com"));
        assert!(!looks_like_email("a@.com"));
        assert!(!looks_like_email("a@com."));
    }
}
