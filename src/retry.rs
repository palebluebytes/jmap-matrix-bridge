use crate::{
    client_manager::ClientManager,
    matrix::MatrixClient,
    sender::{AttachmentInfo, JmapSender},
    store::{Store, ThreadRepository},
};
use std::sync::Arc;

#[allow(clippy::too_many_lines)]
pub async fn run_retry_loop(store: Store, manager: Arc<ClientManager>, matrix: MatrixClient) {
    loop {
        tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
        match store.get_pending_outbound().await {
            Ok(messages) => {
                for msg in messages {
                    tracing::info!(
                        "Retrying outbound message {} (attempt {})",
                        msg.id,
                        msg.retry_count + 1
                    );

                    let mut updated = false;
                    if let Some(client) = manager.get_client(&msg.matrix_user_id).await {
                        let sender =
                            JmapSender::new(client).with_quote_replies(manager.quote_replies);

                        match store.get_ghost_email_by_room(&msg.room_id).await {
                            Ok(Some(email)) => {
                                let subject = store
                                    .get_thread_subject(&msg.room_id)
                                    .await
                                    .ok()
                                    .flatten()
                                    .unwrap_or_else(|| "Matrix Conversation".to_owned());
                                let attachments: Vec<AttachmentInfo> = msg
                                    .attachments_json
                                    .as_deref()
                                    .and_then(|s| serde_json::from_str(s).ok())
                                    .unwrap_or_default();

                                let result = if let Some(ref thread_id_str) = msg.thread_root_id {
                                    // Use the same separator constant as the enqueue side
                                    // in ghost.rs to prevent silent divergence.
                                    let sep = crate::store::THREAD_QUEUE_SEPARATOR;
                                    let parts: Vec<&str> = thread_id_str.split(sep).collect();
                                    if parts.len() == 3 {
                                        let jmap_thread_id = parts[0];
                                        let parent_email_id = parts[1];
                                        let reply_subject = if subject.starts_with("Re:") {
                                            subject.clone()
                                        } else {
                                            format!("Re: {subject}")
                                        };
                                        sender
                                            .reply_to_email(
                                                &email,
                                                &reply_subject,
                                                &msg.body_text,
                                                parent_email_id,
                                                jmap_thread_id,
                                                attachments,
                                            )
                                            .await
                                    } else {
                                        sender
                                            .send_email(
                                                &email,
                                                &subject,
                                                &msg.body_text,
                                                attachments,
                                            )
                                            .await
                                    }
                                } else {
                                    sender
                                        .send_email(&email, &subject, &msg.body_text, attachments)
                                        .await
                                };

                                if result.is_ok() {
                                    let _ = store.remove_from_outbound_queue(msg.id).await;
                                    tracing::info!("Successfully retried message {}", msg.id);
                                    updated = true;
                                } else if let Err(ref e) = result {
                                    tracing::error!("Failed to retry message {}: {}", msg.id, e);
                                }
                            }
                            Ok(None) => {
                                // Ghost room is unmapped / deleted. Purge from queue immediately.
                                tracing::warn!(
                                    "Ghost room mapping deleted for message {}. Purging.",
                                    msg.id
                                );
                                let _ = store.remove_from_outbound_queue(msg.id).await;
                                updated = true;
                            }
                            Err(e) => {
                                tracing::error!(
                                    "Failed to check ghost room mapping for message {}: {}",
                                    msg.id,
                                    e
                                );
                            }
                        }
                    } else {
                        tracing::warn!(
                            "User client session not found for user {} on message retry {}.",
                            msg.matrix_user_id,
                            msg.id
                        );
                    }

                    if !updated {
                        if msg.retry_count >= 9 {
                            let failure_msg = "❌ Permanent delivery failure: The JMAP-Matrix bridge was unable to deliver your message to the recipient after 10 attempts.";
                            if let Err(e) = matrix
                                .send_message(&msg.room_id, failure_msg, None, None)
                                .await
                            {
                                tracing::error!(
                                    "Failed to send delivery failure notification: {}",
                                    e
                                );
                            }
                            let _ = store.remove_from_outbound_queue(msg.id).await;
                            tracing::warn!(
                                "Message {} reached maximum retry attempts. Purged and user notified.",
                                msg.id
                            );
                        } else {
                            // Increment retry count to avoid busy spinning on transient client/mapping errors
                            let _ = store.update_retry_count(msg.id).await;
                        }
                    }
                }
            }
            Err(e) => tracing::error!("Failed to fetch pending outbound: {}", e),
        }
    }
}
