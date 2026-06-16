use super::{GhostUser, JmapPoller};
use crate::services::content::{EmailBody, handle_attachments};
use crate::store::ThreadRepository;
use anyhow::{Context, Result};
use jmap_client::email::{Email, EmailAddress, Property};
use tracing::{info, instrument, warn};

const UNKNOWN_SENDER: &str = "unknown@sender";
const NO_SUBJECT: &str = "(No Subject)";

impl JmapPoller {
    #[instrument(skip(self), fields(user = %self.matrix_user_id))]
    #[allow(clippy::too_many_lines)]
    pub async fn sync_emails(&self) -> Result<()> {
        let last_state = self
            .store
            .get_jmap_state(&self.matrix_user_id, "changes")
            .await?;
        tracing::debug!("Starting email sync. Last sync state: {:?}", last_state);

        let mut current_state = last_state;

        let final_state = loop {
            let mut request = self.client.build();
            let (email_ids, new_state, has_more) = if let Some(state) = &current_state {
                request.changes_email(state);
                let response = match request
                    .send()
                    .await?
                    .pop_method_response()
                    .context("Empty response for Email/changes")?
                    .unwrap_changes_email()
                {
                    Ok(res) => res,
                    Err(jmap_client::Error::Method(method_err))
                        if method_err.error() == &jmap_client::core::error::MethodErrorType::CannotCalculateChanges =>
                    {
                        warn!("cannotCalculateChanges error for emails, resetting state and performing full bootstrap");
                        self.store.delete_jmap_state(&self.matrix_user_id, "changes").await?;
                        current_state = None;
                        continue;
                    }
                    Err(e) => return Err(e.into()),
                };

                let new_state = response.new_state().to_owned();
                // Handle destroyed emails: mark them so future syncs skip them.
                for destroyed_id in response.destroyed() {
                    if let Err(e) = self.store.mark_email_destroyed(destroyed_id).await {
                        warn!(error = %e, %destroyed_id, "Failed to mark email as destroyed");
                    }
                }
                let mut ids = response.created().to_vec();
                ids.extend_from_slice(response.updated());
                (ids, new_state, response.has_more_changes())
            } else {
                let email_query = request.query_email();
                email_query
                    .sort([jmap_client::email::query::Comparator::received_at().descending()])
                    .limit(self.sync_limit);
                email_query.arguments().collapse_threads(false);
                let mut response = request
                    .send()
                    .await?
                    .pop_method_response()
                    .context("Empty response for Email/query")?
                    .unwrap_query_email()?;

                let query_state = response.take_query_state();
                let ids = response.take_ids();

                if ids.len() == self.sync_limit {
                    if let Err(e) = self
                        .store
                        .save_jmap_state(
                            &self.matrix_user_id,
                            "backfill_position",
                            &self.sync_limit.to_string(),
                        )
                        .await
                    {
                        warn!(error = %e, "Failed to initialize backfill position");
                    } else {
                        info!(
                            user = %self.matrix_user_id,
                            next_position = self.sync_limit,
                            "Initial email sync returned full page; registered backfill position"
                        );
                    }
                }

                if let Err(e) = self
                    .store
                    .save_jmap_state(&self.matrix_user_id, "query", &query_state)
                    .await
                {
                    warn!(error = %e, "Failed to save JMAP query state");
                }

                // Bootstrap: obtain the proper Email/changes state via Email/get.
                // queryState from Email/query MUST NOT be used as sinceState for
                // Email/changes — they are different opaque tokens (RFC 8621 §4.3).
                let mut get_req = self.client.build();
                get_req.get_email().ids(&[] as &[String]);
                let get_resp = get_req
                    .send()
                    .await?
                    .pop_method_response()
                    .context("Empty response for Email/get state bootstrap")?
                    .unwrap_get_email()?;
                let changes_state = get_resp.state().to_owned();

                (ids, changes_state, false)
            };

            tracing::debug!(
                "Retrieved JMAP email sync results: {} email IDs found",
                email_ids.len()
            );

            if !email_ids.is_empty() {
                let emails = self.fetch_emails(&email_ids).await?;
                for email in &emails {
                    if let Err(e) = self.process_email(email).await {
                        warn!(error = %e, "Failed to process email");
                    }
                }
            }

            if !has_more {
                break new_state.clone();
            }
            current_state = Some(new_state);
        };

        self.store
            .save_jmap_state(&self.matrix_user_id, "changes", &final_state)
            .await?;
        Ok(())
    }

    pub(crate) async fn fetch_emails(&self, ids: &[String]) -> Result<Vec<Email>> {
        tracing::debug!("Fetching email content from JMAP for IDs: {:?}", ids);
        let mut request = self.client.build();
        // Request only the properties we actually use to reduce bandwidth and
        // memory pressure, especially during large backfill operations.
        let email_req = request.get_email();
        email_req.ids(ids).properties([
            Property::Id,
            Property::ThreadId,
            Property::Subject,
            Property::From,
            Property::ReceivedAt,
            Property::TextBody,
            Property::HtmlBody,
            Property::BodyValues,
            Property::Attachments,
        ]);
        email_req
            .arguments()
            .fetch_html_body_values(true)
            .fetch_text_body_values(true)
            .max_body_value_bytes(32_768);
        let mut response = request
            .send()
            .await?
            .pop_method_response()
            .context("Email/get failed")?
            .unwrap_get_email()?;
        Ok(response.take_list())
    }

    #[instrument(skip(self, email), fields(email.id = ?email.id(), email.thread_id = ?email.thread_id()))]
    pub(crate) async fn process_email(&self, email: &Email) -> Result<()> {
        let email_id = email.id().context("Email missing id")?;
        if self.store.has_message_mapped(email_id).await? {
            tracing::debug!(%email_id, "Email already mapped, skipping processing.");
            return Ok(());
        }

        let thread_id = email.thread_id().context("Email missing threadId")?;
        tracing::debug!(
            "Processing email: id={:?}, thread_id={}, subject={:?}, from={:?}",
            email.id(),
            thread_id,
            email.subject(),
            email.from().map(|f| f
                .iter()
                .map(jmap_client::email::EmailAddress::email)
                .collect::<Vec<_>>())
        );

        let ghost = self.resolve_ghost(email).await?;
        let body = EmailBody::from_email(email);

        if let Some((root_event_id, room_id, latest_event_id)) =
            self.store.get_thread_info(thread_id).await?
        {
            tracing::debug!(
                "Email thread {} already mapped to room {}. Processing as reply.",
                thread_id,
                room_id
            );
            self.process_reply(
                email,
                &ghost,
                &body,
                &room_id,
                &root_event_id,
                latest_event_id.as_deref(),
            )
            .await
        } else {
            tracing::debug!(
                "Email thread {} is not mapped yet. Creating new thread.",
                thread_id
            );
            self.process_new_thread(email, &ghost, &body).await
        }
    }

    async fn process_reply(
        &self,
        email: &Email,
        ghost: &GhostUser,
        body: &EmailBody,
        room_id: &str,
        root_event_id: &str,
        latest_event_id: Option<&str>,
    ) -> Result<()> {
        // Saturating multiply avoids i64 overflow for far-future timestamps.
        let timestamp = email
            .received_at()
            .map(|t| u64::try_from(t).unwrap_or(0).saturating_mul(1000));
        let event_id = self
            .matrix
            .send_message_as(
                room_id,
                &body.plain,
                body.html.as_deref(),
                Some(root_event_id),
                latest_event_id,
                &ghost.user_id,
                timestamp,
            )
            .await?;
        let thread_id = email.thread_id().expect("email thread_id must be present");
        self.store
            .save_message_mapping(email.id().expect("email id must be present"), &event_id)
            .await?;
        // Update the latest event so the next reply threads correctly.
        if let Err(e) = self
            .store
            .update_thread_latest_event(thread_id, &event_id)
            .await
        {
            warn!(error = %e, %thread_id, "Failed to update thread latest event");
        }
        handle_attachments(
            &self.client,
            &self.matrix,
            &self.store,
            &self.matrix_user_id,
            email,
            room_id,
            Some(root_event_id),
            Some(&event_id),
            &ghost.user_id,
            timestamp,
        )
        .await?;
        Ok(())
    }

    async fn process_new_thread(
        &self,
        email: &Email,
        ghost: &GhostUser,
        body: &EmailBody,
    ) -> Result<()> {
        let room_id = self.resolve_or_create_room(email, ghost).await?;

        // Sync subject to room name
        let subject = email.subject().unwrap_or(NO_SUBJECT);
        if let Ok(rid) = <&matrix_sdk::ruma::RoomId>::try_from(room_id.as_str())
            && let Some(room) = self.matrix.client.get_room(rid)
            && let Err(e) = room.set_name(subject.to_owned()).await
        {
            warn!(error = %e, "Failed to set room name");
        }

        // Saturating multiply avoids i64 overflow for far-future timestamps.
        let timestamp = email
            .received_at()
            .map(|t| u64::try_from(t).unwrap_or(0).saturating_mul(1000));
        let event_id = self
            .matrix
            .send_message_as(
                &room_id,
                &body.plain,
                body.html.as_deref(),
                None,
                None,
                &ghost.user_id,
                timestamp,
            )
            .await?;
        self.store
            .save_thread_mapping_atomic(
                email.thread_id().expect("email thread_id must be present"),
                &event_id,
                &room_id,
                subject,
            )
            .await?;
        self.store
            .save_message_mapping(email.id().expect("email id must be present"), &event_id)
            .await?;
        handle_attachments(
            &self.client,
            &self.matrix,
            &self.store,
            &self.matrix_user_id,
            email,
            &room_id,
            Some(&event_id),
            Some(&event_id),
            &ghost.user_id,
            timestamp,
        )
        .await?;
        Ok(())
    }

    async fn resolve_or_create_room(&self, email: &Email, ghost: &GhostUser) -> Result<String> {
        // Inbound rooms are provisioned by the same shared helper the `!compose`
        // command uses, so the two paths can never drift. The display name comes
        // from the sender's From header, falling back to the bare address.
        let from_vec = email.from().unwrap_or(&[]);
        let display_name = from_vec
            .first()
            .and_then(|f: &EmailAddress| f.name())
            .unwrap_or(&ghost.email);
        crate::ghost::ensure_contact_room(
            &self.matrix,
            &self.store,
            &self.matrix_user_id,
            &ghost.email,
            display_name,
        )
        .await
    }

    async fn resolve_ghost(&self, email: &Email) -> Result<GhostUser> {
        let from_vec = email.from().unwrap_or(&[]);
        let sender = from_vec.first();
        let email_addr = sender.map_or(UNKNOWN_SENDER, jmap_client::email::EmailAddress::email);
        let name = sender.and_then(|f| f.name().map(ToString::to_string));

        let localpart = crate::ghost::email_to_localpart(email_addr);
        let user_id = format!("@{}:{}", localpart, self.matrix.domain);
        tracing::debug!(
            "Resolving ghost user mapping for email: {} (localpart: {}, user_id: {})",
            email_addr,
            localpart,
            user_id
        );

        // Auto-register ghost
        self.matrix.ensure_user_exists(&localpart).await?;

        // Sync profile display name
        if let Some(display_name) = &name
            && let Err(e) = self.matrix.set_display_name(&user_id, display_name).await
        {
            warn!(error = %e, "Failed to sync ghost display name");
        }

        Ok(GhostUser {
            email: email_addr.to_owned(),
            user_id,
        })
    }
}
