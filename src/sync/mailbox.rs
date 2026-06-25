use super::JmapPoller;
use crate::store::MailboxRepository;
use anyhow::{Context, Result};
use tracing::{info, instrument, warn};

impl JmapPoller {
    /// Sync JMAP mailboxes into Matrix rooms.
    #[instrument(skip(self), fields(user = %self.matrix_user_id))]
    pub async fn sync_mailboxes(&self) -> Result<()> {
        let last_state = self
            .store
            .get_jmap_state(&self.matrix_user_id, "mailbox")
            .await?;

        let mut current_state = last_state.clone();
        let mut all_mailbox_ids = Vec::new();

        let final_state = loop {
            let mut request = self.client.build();
            let (mailbox_ids, new_state, has_more) = if let Some(state) = &current_state {
                request.changes_mailbox(state);
                let response = match request
                    .send()
                    .await?
                    .pop_method_response()
                    .context("Empty response for Mailbox/changes")?
                    .unwrap_changes_mailbox()
                {
                    Ok(res) => res,
                    Err(jmap_client::Error::Method(method_err))
                        if method_err.error() == &jmap_client::core::error::MethodErrorType::CannotCalculateChanges =>
                    {
                        warn!("cannotCalculateChanges error for mailboxes, resetting state and performing full bootstrap");
                        self.store.delete_jmap_state(&self.matrix_user_id, "mailbox").await?;
                        current_state = None;
                        continue;
                    }
                    Err(e) => return Err(e.into()),
                };

                let new_state = response.new_state().to_owned();
                let mut ids = response.created().to_vec();
                ids.extend_from_slice(response.updated());
                (ids, new_state, response.has_more_changes())
            } else {
                request.query_mailbox();
                let mut response = request
                    .send()
                    .await?
                    .pop_method_response()
                    .context("Empty response for Mailbox/query")?
                    .unwrap_query_mailbox()?;

                let _query_state = response.take_query_state();
                let ids = response.take_ids();

                // Bootstrap: get the proper Mailbox/changes state via Mailbox/get.
                // queryState != changesState; feeding one into the other causes
                // cannotCalculateChanges on every server restart (RFC 8620 §5.3).
                let mut get_req = self.client.build();
                get_req.get_mailbox().ids(&[] as &[String]);
                let get_resp = get_req
                    .send()
                    .await?
                    .pop_method_response()
                    .context("Empty response for Mailbox/get state bootstrap")?
                    .unwrap_get_mailbox()?;
                let changes_state = get_resp.state().to_owned();

                (ids, changes_state, false)
            };

            all_mailbox_ids.extend(mailbox_ids);

            if !has_more {
                break new_state.clone();
            }
            current_state = Some(new_state);
        };

        if all_mailbox_ids.is_empty() {
            tracing::debug!("No mailbox changes detected since state {:?}", last_state);
            self.store
                .save_jmap_state(&self.matrix_user_id, "mailbox", &final_state)
                .await?;
            return Ok(());
        }

        tracing::debug!(
            "Found {} mailbox changes/creations. Processing...",
            all_mailbox_ids.len()
        );
        self.fetch_and_map_mailboxes(&all_mailbox_ids).await?;
        self.store
            .save_jmap_state(&self.matrix_user_id, "mailbox", &final_state)
            .await?;
        Ok(())
    }

    async fn fetch_and_map_mailboxes(&self, ids: &[String]) -> Result<()> {
        tracing::debug!("Fetching detail for mailboxes: {:?}", ids);
        let mut request = self.client.build();
        request.get_mailbox().ids(ids);
        let mut response = request
            .send()
            .await?
            .pop_method_response()
            .context("Empty response for Mailbox/get")?
            .unwrap_get_mailbox()?;

        for mailbox in response.take_list() {
            let (Some(id), Some(name)) = (mailbox.id(), mailbox.name()) else {
                continue;
            };
            let room_id = if let Some(existing) = self.store.get_room_id(id).await? {
                existing
            } else {
                let room_id = self.matrix.create_room_for_mailbox(name).await?;
                self.store.save_room_mapping(id, &room_id).await?;
                info!(mailbox.id = id, mailbox.name = name, %room_id, "Mapped mailbox to room");
                room_id
            };
            // File the mailbox room under the user's email space (idempotent), so
            // mailbox rooms don't float loose in the room list (#24). Best-effort.
            if let Err(e) = crate::ghost::ensure_room_in_email_space(
                &self.matrix,
                &self.store,
                &self.matrix_user_id,
                &room_id,
            )
            .await
            {
                warn!(error = %e, %room_id, "Failed to add mailbox room to email space");
            }
        }
        Ok(())
    }
}
