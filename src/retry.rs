use crate::{
    client_manager::ClientManager,
    matrix::MatrixClient,
    sender::{AttachmentInfo, JmapSender},
    store::{OutboundMessage, Store, ThreadRepository},
};
use jmap_client::client::Client;
use std::sync::Arc;

/// The outbound submission worker.
///
/// Every outbound message — first attempt included — flows through the
/// `outbound_queue` and is submitted here once its `release_at` send-delay hold
/// passes (ADR-0012); a redaction or edit inside that window cancels or rewrites
/// it before this ever sees it. Prior failures are retried on the exponential
/// backoff encoded in `get_pending_outbound`. The short tick keeps the hold
/// responsive without busy-spinning.
pub async fn run_retry_loop(store: Store, manager: Arc<ClientManager>, matrix: MatrixClient) {
    loop {
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
        match store.get_pending_outbound().await {
            Ok(messages) => {
                for msg in messages {
                    let resolved = submit_one(&store, &manager, &msg).await;
                    if !resolved {
                        handle_unresolved(&store, &matrix, &msg).await;
                    }
                }
            }
            Err(e) => tracing::error!("Failed to fetch pending outbound: {}", e),
        }
    }
}

/// Submit one queued message to JMAP. Returns `true` when the message is
/// resolved (delivered and removed, or purged because its room is gone) and
/// `false` when it should be retried/aged by the caller.
async fn submit_one(store: &Store, manager: &Arc<ClientManager>, msg: &OutboundMessage) -> bool {
    let Some(client) = manager.get_client(&msg.matrix_user_id).await else {
        tracing::warn!(
            "No JMAP session for user {} on outbound message {}.",
            msg.matrix_user_id,
            msg.id
        );
        return false;
    };

    let email = match store.get_ghost_email_by_room(&msg.room_id).await {
        Ok(Some(email)) => email,
        Ok(None) => {
            // Room mapping deleted (e.g. the thread was trashed) — nothing to
            // deliver to. Purge so it doesn't linger.
            tracing::warn!("Ghost room gone for message {}. Purging.", msg.id);
            let _ = store.remove_from_outbound_queue(msg.id).await;
            return true;
        }
        Err(e) => {
            tracing::error!("Failed to look up ghost room for message {}: {}", msg.id, e);
            return false;
        }
    };

    let sender = JmapSender::new(client.clone()).with_quote_replies(manager.quote_replies);
    let attachments: Vec<AttachmentInfo> = msg
        .attachments_json
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default();

    let send_result = if let Some((jmap_thread_id, parent_email_id, root_event_id)) =
        parse_thread_root(msg.thread_root_id.as_deref())
    {
        submit_reply(
            store,
            &sender,
            &email,
            &msg.body_text,
            (jmap_thread_id, parent_email_id, root_event_id),
            attachments,
        )
        .await
    } else {
        submit_fresh(manager, store, &client, &sender, &email, msg, attachments).await
    };

    match send_result {
        Ok(()) => {
            let _ = store.remove_from_outbound_queue(msg.id).await;
            tracing::info!("Submitted outbound message {}", msg.id);
            true
        }
        Err(e) => {
            tracing::error!("Failed to submit outbound message {}: {}", msg.id, e);
            false
        }
    }
}

/// Submit a threaded reply. Subject comes from the thread's root event — the
/// same lookup the inline path used — so queued/retried replies keep their real
/// "Re: …" subject rather than a generic placeholder.
async fn submit_reply(
    store: &Store,
    sender: &JmapSender,
    email: &str,
    body: &str,
    (jmap_thread_id, parent_email_id, root_event_id): (&str, &str, &str),
    attachments: Vec<AttachmentInfo>,
) -> anyhow::Result<()> {
    let subject = store
        .get_thread_subject(root_event_id)
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| "Matrix Conversation".to_owned());
    let reply_subject = if subject.starts_with("Re:") {
        subject
    } else {
        format!("Re: {subject}")
    };
    tracing::info!("Sending ghost room reply to {email} (thread {jmap_thread_id})");
    sender
        .reply_to_email(
            email,
            &reply_subject,
            body,
            parent_email_id,
            jmap_thread_id,
            attachments,
        )
        .await
        .map(|_| ())
}

/// Submit a fresh (non-threaded) email and map its JMAP thread → room so a later
/// reply continues the same room instead of spawning a duplicate.
async fn submit_fresh(
    manager: &Arc<ClientManager>,
    store: &Store,
    client: &Arc<Client>,
    sender: &JmapSender,
    email: &str,
    msg: &OutboundMessage,
    attachments: Vec<AttachmentInfo>,
) -> anyhow::Result<()> {
    let subject = crate::ghost::fresh_email_subject(manager.matrix.room_name(&msg.room_id).await);
    tracing::info!("Sending fresh email to {email} from ghost room");
    let email_id = sender
        .send_email(email, &subject, &msg.body_text, attachments)
        .await?;
    if let Ok(Some(thread_id)) = crate::ghost::try_resolve_thread(client, &email_id).await {
        if let Err(e) = store
            .save_thread_mapping_atomic(&thread_id, &msg.event_id, &msg.room_id, &subject)
            .await
        {
            tracing::warn!(error = %e, "Failed to map composed thread to room");
        }
    } else {
        tracing::warn!(
            "Could not resolve thread for fresh email {email_id}; a reply may spawn a new room"
        );
    }
    Ok(())
}

/// Parse the `jmap_thread_id|parent_email_id|root_event_id` triple, or `None`
/// for a fresh (non-threaded) email.
fn parse_thread_root(thread_root_id: Option<&str>) -> Option<(&str, &str, &str)> {
    let raw = thread_root_id?;
    let parts: Vec<&str> = raw.split(crate::store::THREAD_QUEUE_SEPARATOR).collect();
    if parts.len() == 3 {
        Some((parts[0], parts[1], parts[2]))
    } else {
        None
    }
}

/// A message that failed to submit: either age its retry counter (exponential
/// backoff) or, past the attempt cap, give up with a user-visible notice rather
/// than dropping it silently (ADR-0007).
async fn handle_unresolved(store: &Store, matrix: &MatrixClient, msg: &OutboundMessage) {
    if msg.retry_count >= 9 {
        let failure_msg = "❌ Permanent delivery failure: the bridge could not deliver your message to the recipient after 10 attempts.";
        if let Err(e) = matrix
            .send_message(&msg.room_id, failure_msg, None, None)
            .await
        {
            tracing::error!("Failed to send delivery-failure notice: {}", e);
        }
        let _ = store.remove_from_outbound_queue(msg.id).await;
        tracing::warn!(
            "Message {} hit the retry cap. Purged and user notified.",
            msg.id
        );
    } else {
        let _ = store.update_retry_count(msg.id).await;
    }
}
