use crate::commands::{Command, CommandContext};
use crate::routes::{AppState, notify};
use crate::sender::JmapSender;
use anyhow::{Context, Result};
use jmap_client::email::Property;
use matrix_sdk::ruma::events::room::message::MessageType;
use tracing::warn;

#[derive(Debug)]
pub struct ReplyCommand;

impl Command for ReplyCommand {
    fn matches(&self, ctx: &CommandContext<'_>) -> bool {
        ctx.message_content.relates_to.is_some()
    }

    #[allow(clippy::too_many_lines)]
    fn execute<'a>(
        &'a self,
        state: &'a AppState,
        ctx: &'a CommandContext<'a>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            let Some(relates_to) = &ctx.message_content.relates_to else {
                return Ok(());
            };

            let target_event_id =
                if let matrix_sdk::ruma::events::room::message::Relation::Reply { in_reply_to } =
                    relates_to
                {
                    Some(&in_reply_to.event_id)
                } else if let matrix_sdk::ruma::events::room::message::Relation::Thread(t) =
                    relates_to
                {
                    Some(&t.event_id)
                } else {
                    None
                };

            let Some(target_event_id) = target_event_id else {
                return Ok(());
            };
            let target_event_id = target_event_id.as_str();

            let Some(email_id) = state
                .client_manager
                .store
                .get_email_id_from_event_id(target_event_id)
                .await?
            else {
                return Ok(());
            };

            let Some(client) = state.client_manager.get_client(ctx.sender_id).await else {
                return Ok(());
            };

            // Fetch original email to get threadId and subject
            let mut request = client.build();
            request
                .get_email()
                .ids(std::slice::from_ref(&email_id))
                .properties([
                    Property::Id,
                    Property::ThreadId,
                    Property::Subject,
                    Property::From,
                    Property::InReplyTo,
                ]);
            let mut response = request
                .send()
                .await?
                .pop_method_response()
                .context("Email/get failed")?
                .unwrap_get_email()?;

            let Some(email_obj) = response.take_list().pop() else {
                warn!("Original email {email_id} not found");
                return Ok(());
            };

            let thread_id = email_obj.thread_id().context("Email missing threadId")?;
            let subject = email_obj.subject().unwrap_or("");
            let from_email = email_obj
                .from()
                .and_then(|f| f.first())
                .map_or("", jmap_client::email::EmailAddress::email);

            let reply_subject = if subject.starts_with("Re:") {
                subject.to_owned()
            } else {
                format!("Re: {subject}")
            };

            let mut attachments = Vec::new();

            let is_media = matches!(
                &ctx.message_content.msgtype,
                MessageType::File(_)
                    | MessageType::Image(_)
                    | MessageType::Audio(_)
                    | MessageType::Video(_)
            );

            if is_media {
                let jmap_sender = JmapSender::new(client.clone());
                match jmap_sender
                    .upload_matrix_media(&state.client_manager.matrix, ctx.message_content)
                    .await
                {
                    Ok(att) => {
                        attachments.push(att);
                    }
                    Err(e) => {
                        notify(
                            state,
                            ctx.room_id,
                            &format!(
                                "⚠️ Could not attach file \"{}\":\n{e}",
                                ctx.message_content.msgtype.body()
                            ),
                        )
                        .await;
                    }
                }
            }

            let mut final_body = ctx.body_str.to_owned();
            let _ = crate::services::content::append_user_signature(
                &state.client_manager.store,
                ctx.sender_id,
                &mut final_body,
            )
            .await;

            let sender = JmapSender::new(client);
            match sender
                .reply_to_email(
                    from_email,
                    &reply_subject,
                    &final_body,
                    &email_id,
                    thread_id,
                    attachments,
                )
                .await
            {
                Ok(_) => notify(state, ctx.room_id, "Reply sent successfully!").await,
                Err(e) => notify(state, ctx.room_id, &format!("Failed to send reply: {e}")).await,
            }

            Ok(())
        })
    }
}
