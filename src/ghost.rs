//! Logic for handling messages sent to "ghost" rooms (representing external email addresses).

use crate::routes::{AppState, notify};
use crate::sender::{JmapSender, human_bytes};
use crate::store::ThreadRepository;
use anyhow::{Context, Result};
use jmap_client::client::Client;
use jmap_client::email::Email;
use matrix_sdk::ruma::events::room::message::{Relation, RoomMessageEventContent};
use std::fmt::Write as _;
use std::sync::Arc;
use tracing::{error, info};

/// Handle a message sent by a Matrix user to a ghost room.
///
/// This bridges the Matrix message into an outbound JMAP email to the address
/// represented by the room's ghost mapping.
#[allow(
    clippy::too_many_arguments,
    clippy::used_underscore_binding,
    clippy::too_many_lines
)]
pub async fn handle_ghost_outbound(
    state: &AppState,
    sender_id: &str,
    content: &RoomMessageEventContent,
    room_id: Option<&str>,
    _event_id: &str,
) -> Result<()> {
    let rm_id = room_id.context("No room ID")?;
    let raw_body = content.body();
    let mut body_str = raw_body.to_owned();
    let _ = crate::services::content::append_user_signature(
        &state.client_manager.store,
        sender_id,
        &mut body_str,
    )
    .await;

    // 1. Resolve the ghost email address for this room
    let ghost_email = state
        .client_manager
        .store
        .get_ghost_email_by_room(rm_id)
        .await?
        .context("No ghost email for room")?;

    // 2. Get the sender's JMAP client
    let Some(client) = state.client_manager.get_client(sender_id).await else {
        notify(state, Some(rm_id), "You are not logged in. Please type `login` in your private bridge room to connect your account.").await;
        return Ok(());
    };

    // 3. Resolve thread context
    let thread_info =
        resolve_thread_context(state, rm_id, content.relates_to.as_ref(), &client).await?;

    if let Some((jmap_thread_id, parent_id, root_event_id, latest_event_id, subject)) = thread_info
    {
        let sender = JmapSender::new(client);
        let reply_subject = if subject.starts_with("Re:") {
            subject
        } else {
            format!("Re: {subject}")
        };

        info!(
            "Sending ghost room reply to {} (thread {})",
            ghost_email, jmap_thread_id
        );
        if let Err(e) = sender
            .reply_to_email(
                &ghost_email,
                &reply_subject,
                &body_str,
                &parent_id,
                &jmap_thread_id,
                vec![],
            )
            .await
        {
            error!(%sender_id, %rm_id, error = %e, "Failed to send reply email, adding to retry queue");
            let sep = crate::store::THREAD_QUEUE_SEPARATOR;
            let queue_thread_val = format!("{jmap_thread_id}{sep}{parent_id}{sep}{root_event_id}");
            state
                .client_manager
                .store
                .add_to_outbound_queue(
                    sender_id,
                    rm_id,
                    _event_id,
                    &body_str,
                    None,
                    Some(&queue_thread_val),
                    None,
                )
                .await?;

            notify(
                state,
                Some(rm_id),
                "⚠️ Network error while sending reply. Message queued for retry.",
            )
            .await;
        }
        // Update latest event ID tracking regardless of send success
        // (the outbound queue worker will update this on retry).
        drop(latest_event_id); // thread context noted; latest_event tracked by ingest path
        return Ok(());
    }

    // 4. Default: Send as a fresh email if no thread context found
    info!("Sending fresh email to {} from ghost room", ghost_email);
    let subject = "Matrix Conversation".to_owned();

    let sender = JmapSender::new(client);
    if let Err(e) = sender
        .send_email(&ghost_email, &subject, &body_str, vec![])
        .await
    {
        error!(%sender_id, %rm_id, error = %e, "Failed to send email, adding to retry queue");
        state
            .client_manager
            .store
            .add_to_outbound_queue(sender_id, rm_id, _event_id, &body_str, None, None, None)
            .await?;
        notify(
            state,
            Some(rm_id),
            "⚠️ Network error while sending. Message queued for retry.",
        )
        .await;
    }

    Ok(())
}

/// Handle a media message (image, file, etc.) sent by a Matrix user to a ghost room.
#[allow(
    clippy::too_many_arguments,
    clippy::used_underscore_binding,
    clippy::too_many_lines
)]
pub async fn handle_ghost_media_outbound(
    state: &AppState,
    sender_id: &str,
    content: &RoomMessageEventContent,
    room_id: Option<&str>,
    _event_id: &str,
) -> Result<()> {
    let rm_id = room_id.context("No room ID")?;

    // 1. Resolve the ghost email address for this room
    let ghost_email = state
        .client_manager
        .store
        .get_ghost_email_by_room(rm_id)
        .await?
        .context("No ghost email for room")?;

    // 2. Get the sender's JMAP client
    let Some(client) = state.client_manager.get_client(sender_id).await else {
        notify(state, Some(rm_id), "You are not logged in. Please type `login` in your private bridge room to connect your account.").await;
        return Ok(());
    };

    let sender = JmapSender::new(client.clone());
    let max_size = sender.max_upload_size();

    let att = match sender
        .upload_matrix_media(&state.client_manager.matrix, content)
        .await
    {
        Ok(a) => a,
        Err(e) => {
            notify(
                state,
                Some(rm_id),
                &format!(
                    "⚠️ Media attachment upload failed or size exceeded JMAP server upload limit ({}).",
                    human_bytes(max_size)
                ),
            )
            .await;
            return Err(e);
        }
    };

    // 7. Resolve thread context (shared helper, same logic as text outbound)
    let thread_info =
        resolve_thread_context(state, rm_id, content.relates_to.as_ref(), &client).await?;

    if let Some((jmap_thread_id, parent_id, root_event_id, _latest_event_id, subject)) = thread_info
    {
        let reply_subject = if subject.starts_with("Re:") {
            subject
        } else {
            format!("Re: {subject}")
        };
        info!(
            "Sending media reply to {} (thread {})",
            ghost_email, jmap_thread_id
        );
        if let Err(e) = sender
            .reply_to_email(
                &ghost_email,
                &reply_subject,
                "Sent an attachment from Matrix.",
                &parent_id,
                &jmap_thread_id,
                vec![att.clone()],
            )
            .await
        {
            error!(%sender_id, %rm_id, error = %e, "Failed to send media reply email, adding to retry queue");
            let sep = crate::store::THREAD_QUEUE_SEPARATOR;
            let queue_thread_val = format!("{jmap_thread_id}{sep}{parent_id}{sep}{root_event_id}");
            let atts_json = serde_json::to_string(&vec![att])?;
            state
                .client_manager
                .store
                .add_to_outbound_queue(
                    sender_id,
                    rm_id,
                    _event_id,
                    "Sent an attachment from Matrix.",
                    None,
                    Some(&queue_thread_val),
                    Some(&atts_json),
                )
                .await?;
            notify(state, Some(rm_id), "⚠️ Network error while sending media reply. Message and attachment queued for retry.").await;
        }
        return Ok(());
    }

    info!("Sending media as fresh email to {}", ghost_email);
    let subject = "Matrix Media Attachment".to_owned();
    if let Err(e) = sender
        .send_email(
            &ghost_email,
            &subject,
            "Sent an attachment from Matrix.",
            vec![att.clone()],
        )
        .await
    {
        error!(%sender_id, %rm_id, error = %e, "Failed to send media email, adding to retry queue");
        let atts_json = serde_json::to_string(&vec![att])?;
        state
            .client_manager
            .store
            .add_to_outbound_queue(
                sender_id,
                rm_id,
                _event_id,
                "Sent an attachment from Matrix.",
                None,
                None,
                Some(&atts_json),
            )
            .await?;
        notify(
            state,
            Some(rm_id),
            "⚠️ Network error while sending media. Message and attachment queued for retry.",
        )
        .await;
    }

    Ok(())
}

/// Resolve the JMAP thread context for a reply, either from Matrix
/// relation metadata or from the room's most-recent thread.
///
/// Returns `Some((jmap_thread_id, parent_email_id, root_event_id, latest_event_id, subject))`.
async fn resolve_thread_context(
    state: &AppState,
    rm_id: &str,
    relates_to: Option<
        &matrix_sdk::ruma::events::room::message::Relation<
            matrix_sdk::ruma::events::room::message::RoomMessageEventContentWithoutRelation,
        >,
    >,
    client: &std::sync::Arc<jmap_client::client::Client>,
) -> anyhow::Result<Option<(String, String, String, Option<String>, String)>> {
    // 1. Try to resolve from the Matrix reply/thread relation.
    if let Some(rel) = relates_to {
        let target_event_id = match rel {
            Relation::Reply { in_reply_to } => Some(in_reply_to.event_id.as_str()),
            Relation::Thread(thread) => Some(thread.event_id.as_str()),
            _ => None,
        };

        if let Some(event_id) = target_event_id {
            if let Ok(Some(parent_email_id)) = state
                .client_manager
                .store
                .get_email_id_from_event_id(event_id)
                .await
            {
                // Fast path: thread is already in our store.
                if let Ok(Some(jmap_thread_id)) = state
                    .client_manager
                    .store
                    .get_jmap_thread_id_by_root_event(event_id)
                    .await
                {
                    let subject = state
                        .client_manager
                        .store
                        .get_thread_subject(event_id)
                        .await?
                        .unwrap_or_else(|| "No Subject".to_owned());
                    let latest_event_id = state
                        .client_manager
                        .store
                        .get_thread_info(&jmap_thread_id)
                        .await?
                        .and_then(|(_, _, latest)| latest);
                    return Ok(Some((
                        jmap_thread_id,
                        parent_email_id,
                        event_id.to_owned(),
                        latest_event_id,
                        subject,
                    )));
                }

                // Slow path: look up thread via JMAP.
                if let Ok(Some(jmap_thread_id)) = try_resolve_thread(client, &parent_email_id).await
                {
                    let thread_info = state
                        .client_manager
                        .store
                        .get_thread_info(&jmap_thread_id)
                        .await?;
                    let root_event_id = thread_info
                        .as_ref()
                        .map_or_else(|| event_id.to_owned(), |(root_id, _, _)| root_id.clone());
                    let latest_event_id = thread_info.and_then(|(_, _, latest)| latest);
                    let subject = state
                        .client_manager
                        .store
                        .get_thread_subject(&root_event_id)
                        .await?
                        .unwrap_or_else(|| "No Subject".to_owned());
                    return Ok(Some((
                        jmap_thread_id,
                        parent_email_id,
                        root_event_id,
                        latest_event_id,
                        subject,
                    )));
                }
            }
        }
    }

    // 2. Fall back to the room's most recent thread.
    if let Ok(Some((jmap_thread_id, root_event_id, subject))) = state
        .client_manager
        .store
        .get_latest_thread_in_room(rm_id)
        .await
    {
        if let Ok(Some(parent_email_id)) = state
            .client_manager
            .store
            .get_last_email_id_by_room(rm_id)
            .await
        {
            let latest_event_id = state
                .client_manager
                .store
                .get_thread_info(&jmap_thread_id)
                .await?
                .and_then(|(_, _, latest)| latest);
            let subject_str = subject.unwrap_or_else(|| "No Subject".to_owned());
            return Ok(Some((
                jmap_thread_id,
                parent_email_id,
                root_event_id,
                latest_event_id,
                subject_str,
            )));
        }
    }

    Ok(None)
}

async fn try_resolve_thread(client: &Arc<Client>, email_id: &str) -> Result<Option<String>> {
    let mut request = client.build();
    request.get_email().ids(&[email_id.to_owned()]);
    let response = request
        .send()
        .await?
        .pop_method_response()
        .context("Empty JMAP response")?
        .unwrap_get_email()?;

    Ok(response
        .list()
        .first()
        .and_then(|e: &Email| e.thread_id().map(|id: &str| id.to_owned())))
}

/// Helper to generate the Matrix localpart for a ghost user representing an email.
#[must_use]
pub fn email_to_localpart(email: &str) -> String {
    let mut localpart = String::with_capacity(email.len() + 8);
    localpart.push_str("_jmap_");

    for c in email.to_lowercase().chars() {
        match c {
            'a'..='z' | '0'..='9' | '.' | '_' | '-' => localpart.push(c),
            c => {
                // Hex-encode other characters as =xx
                localpart.push('=');
                let _ = write!(localpart, "{:02x}", c as u32);
            }
        }
    }
    localpart
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::ruma::user_id;

    #[test]
    fn test_email_to_localpart_logic() {
        let email = "user@example.com";
        let localpart = email_to_localpart(email);
        assert_eq!(localpart, "_jmap_user=40example.com");

        let email_complex = "user+extra@example.com";
        let localpart_complex = email_to_localpart(email_complex);
        assert_eq!(localpart_complex, "_jmap_user=2bextra=40example.com");
    }

    #[test]
    fn test_ruma_localpart_extraction() {
        let user = user_id!("@_jmap_user=40example.com:localhost");
        assert_eq!(user.localpart(), "_jmap_user=40example.com");
    }
}
