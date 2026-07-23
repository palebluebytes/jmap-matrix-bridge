use super::{GhostUser, JmapPoller};
use crate::services::content::{EmailBody, handle_attachments};
use crate::store::ThreadRepository;
use anyhow::{Context, Result};
use jmap_client::email::{Email, EmailAddress, Property};
use tracing::{info, instrument, warn};

const UNKNOWN_SENDER: &str = "unknown@sender";
const NO_SUBJECT: &str = "(No Subject)";

/// Whether an email the poller fetched should NOT be bridged inbound: either it
/// has no resolvable sender (would create a bogus `unknown@sender` room), or it
/// was sent by the bridge user themselves (the Sent copy re-ingested via
/// `Email/changes`, which would otherwise loop).
fn should_skip_inbound(sender_email: Option<&str>, own_email: Option<&str>) -> bool {
    sender_email.is_none_or(|addr| own_email.is_some_and(|own| own.eq_ignore_ascii_case(addr)))
}

/// Matrix display name for a sender's ghost: `"Name (email)"`, or just the email
/// when the sender provides no usable display name — so the address is always
/// visible on every bridged message.
fn ghost_display_name(name: Option<&str>, email: &str) -> String {
    name.map(str::trim)
        .filter(|n| !n.is_empty())
        .map_or_else(|| email.to_owned(), |n| format!("{n} ({email})"))
}

/// The recipient address an inbound email was delivered to — used to show which
/// of the user's aliases (e.g. `…@palebluebytes.space` vs `…@palebluebytes.xyz`)
/// it reached. Prefers a `To`/`Cc` address sharing the user's localpart (so the
/// real alias is picked even when the user has several), then the first `To`,
/// then the user's own address.
fn recipient_alias(email: &Email, own_email: Option<&str>) -> Option<String> {
    let to: Vec<&str> = email
        .to()
        .unwrap_or(&[])
        .iter()
        .map(EmailAddress::email)
        .collect();
    let cc: Vec<&str> = email
        .cc()
        .unwrap_or(&[])
        .iter()
        .map(EmailAddress::email)
        .collect();
    choose_recipient(&to, &cc, own_email)
}

/// Pure core of [`recipient_alias`]: pick the `To`/`Cc` address sharing the
/// user's localpart, else the first `To`, else the user's own address.
fn choose_recipient(to: &[&str], cc: &[&str], own_email: Option<&str>) -> Option<String> {
    if let Some(local) = own_email.and_then(|e| e.split('@').next()) {
        for addr in to.iter().chain(cc.iter()) {
            if addr
                .split('@')
                .next()
                .is_some_and(|l| l.eq_ignore_ascii_case(local))
            {
                return Some((*addr).to_owned());
            }
        }
    }
    to.first()
        .map(|a| (*a).to_owned())
        .or_else(|| own_email.map(str::to_owned))
}

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
                // Ascending (oldest-first) so the newest email is bridged last
                // and gets the highest server stream position — Element's room
                // list orders by that (sliding-sync bump_stamp), not the
                // backdated origin_server_ts. This first page is the oldest
                // emails; the backfill task walks forward to the newest, which
                // therefore land on top. See backfill::backfill_batch.
                let email_query = request.query_email();
                email_query
                    .sort([jmap_client::email::query::Comparator::received_at().ascending()])
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
            Property::To,
            Property::Cc,
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
            // Cap the per-email body we pull from JMAP. 32 KB was too small for
            // HTML newsletters (routinely 100-200 KB of layout markup + invisible
            // spacer padding), which tripped the "truncated by server" notice on
            // ordinary marketing mail. 512 KB covers those while still bounding
            // pathological bodies.
            .max_body_value_bytes(524_288);
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
            // Already bridged — but if it's since been read elsewhere, mirror that
            // read state back to Matrix (#27). Best-effort.
            self.sync_read_state(email).await;
            return Ok(());
        }

        // Don't bridge the user's own outgoing mail. `Email/changes` returns the
        // Sent copy of anything just sent, and bridging it would spawn a contact
        // room for ourselves — or, when the sender header is absent, a bogus
        // `unknown@sender` room — and loop. Mail with no resolvable sender is
        // dropped for the same reason: a real inbound email always has a From.
        let sender_email = email
            .from()
            .and_then(<[_]>::first)
            .map(jmap_client::email::EmailAddress::email);
        let own_email = self.store.get_user_email(&self.matrix_user_id).await?;
        if should_skip_inbound(sender_email, own_email.as_deref()) {
            tracing::debug!(%email_id, ?sender_email, "Skipping the user's own / senderless email");
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
        let body = EmailBody::from_email(email, self.render_mode);

        if let Some((_root_event_id, room_id, _latest_event_id)) =
            self.store.get_thread_info(thread_id).await?
        {
            tracing::debug!(
                "Email thread {} already mapped to room {}. Processing as reply.",
                thread_id,
                room_id
            );
            self.process_reply(email, &ghost, &body, &room_id).await
        } else {
            tracing::debug!(
                "Email thread {} is not mapped yet. Creating new thread.",
                thread_id
            );
            self.process_new_thread(email, &ghost, &body).await
        }
    }

    /// Mirror an already-bridged email's read state from JMAP to Matrix, both
    /// ways: a `$seen` gained emits an `m.read` receipt (#27); a `$seen` lost on a
    /// previously-read email flags the room `m.marked_unread` (MSC2867). Both act
    /// as the user's double-puppet, so without a stored puppet token this is a
    /// silent no-op (surfaced via `status`).
    ///
    /// The single `read_synced` gate makes each edge fire once and breaks the
    /// Matrix-read → `$seen` → `Email/changes` → receipt cycle (and its unread
    /// counterpart). Genuinely-new unread mail — never synced read — is left
    /// alone; it's unread in Element already, so no `marked_unread` is needed.
    async fn sync_read_state(&self, email: &Email) {
        let Some(email_id) = email.id() else {
            return;
        };
        let seen = email.keywords().contains(&"$seen");
        // One gate drives a two-edge state machine. `read_synced` means "we have
        // mirrored this email to Matrix as read". Only the two transitions do
        // work; the steady states (unseen & not-synced, seen & synced) are no-ops,
        // which also makes a re-delivery of the same Email/changes idempotent.
        //   seen  synced_read  -> action
        //   T     F            -> mirror READ  (receipt; clear any manual unread)
        //   F     T            -> mirror UNREAD (set m.marked_unread) [#seen→unseen]
        //   else                -> nothing
        let key = format!("read_synced:{email_id}");
        let synced_read = matches!(
            self.store.get_jmap_state(&self.matrix_user_id, &key).await,
            Ok(Some(_))
        );
        if seen == synced_read {
            return;
        }
        // Both edges need the double-puppet token + the room; without a stored
        // token the appservice can't touch the user's read state at all.
        let Ok(Some(token)) = self
            .store
            .get_matrix_puppet_token(&self.matrix_user_id)
            .await
        else {
            return;
        };
        let Some(thread_id) = email.thread_id() else {
            return;
        };
        let Ok(Some((_root, room_id, _latest))) = self.store.get_thread_info(thread_id).await
        else {
            return;
        };

        if seen {
            // unseen→seen in the mail client: emit a read receipt, and clear any
            // prior manual "mark unread" so the room isn't left stuck unread.
            let Ok(Some(event_id)) = self.store.get_event_id_by_email(email_id).await else {
                return;
            };
            if let Err(e) = self
                .matrix
                .send_read_receipt(&room_id, &event_id, &token)
                .await
            {
                tracing::warn!(error = %e, %email_id, "Failed to mirror read state to Matrix");
                return;
            }
            if let Err(e) = self
                .matrix
                .set_marked_unread(&self.matrix_user_id, &room_id, false, &token)
                .await
            {
                tracing::debug!(error = %e, %room_id, "Failed to clear marked_unread on read");
            }
            let _ = self
                .store
                .save_jmap_state(&self.matrix_user_id, &key, "1")
                .await;
            tracing::debug!(%email_id, "Mirrored read state to Matrix via puppet receipt");
        } else {
            // seen→unseen in the mail client: flag the room unread in Element via
            // m.marked_unread, and drop the read gate (which also dedupes this).
            if let Err(e) = self
                .matrix
                .set_marked_unread(&self.matrix_user_id, &room_id, true, &token)
                .await
            {
                tracing::warn!(error = %e, %email_id, "Failed to mirror unread state to Matrix");
                return;
            }
            let _ = self
                .store
                .delete_jmap_state(&self.matrix_user_id, &key)
                .await;
            tracing::debug!(%email_id, "Mirrored unread state to Matrix via m.marked_unread");
        }
    }

    async fn process_reply(
        &self,
        email: &Email,
        ghost: &GhostUser,
        body: &EmailBody,
        room_id: &str,
    ) -> Result<()> {
        // Saturating multiply avoids i64 overflow for far-future timestamps.
        let timestamp = email
            .received_at()
            .map(|t| u64::try_from(t).unwrap_or(0).saturating_mul(1000));
        // No Matrix m.thread relation: the room IS the email thread, so every
        // message is posted flat as a plain timeline event.
        let event_id = self
            .matrix
            .send_message_as(
                room_id,
                &body.plain,
                body.html.as_deref(),
                None,
                None,
                &ghost.user_id,
                timestamp,
            )
            .await?;
        let (Some(thread_id), Some(email_id)) = (email.thread_id(), email.id()) else {
            warn!("JMAP email missing thread_id/id; skipping mapping");
            return Ok(());
        };
        self.store.save_message_mapping(email_id, &event_id).await?;
        // Track the latest bridged event for the thread (used by outbound reply
        // context), even though messages are no longer Matrix-threaded.
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
            None,
            None,
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
        let (Some(thread_id), Some(email_id)) = (email.thread_id(), email.id()) else {
            warn!("JMAP email missing thread_id/id; skipping new-thread bridging");
            return Ok(());
        };
        let subject = email.subject().unwrap_or(NO_SUBJECT);

        // One Matrix room per email thread. Lock by thread so two emails of the
        // same new thread arriving together don't create two rooms; if another
        // email won the race and already created the room, fall through to reply
        // handling instead.
        let lock_key = format!("thread:{thread_id}");
        loop {
            if let Some((_root, room_id, _latest)) = self.store.get_thread_info(thread_id).await? {
                return self.process_reply(email, ghost, body, &room_id).await;
            }
            if self.store.try_acquire_room_creation_lock(&lock_key).await? {
                break;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }
        let store_clone = self.store.clone();
        let lock_key_clone = lock_key.clone();
        let _guard = scopeguard::guard((), move |()| {
            tokio::spawn(async move {
                let _ = store_clone
                    .release_room_creation_lock(&lock_key_clone)
                    .await;
            });
        });
        // Re-check under the lock.
        if let Some((_root, room_id, _latest)) = self.store.get_thread_info(thread_id).await? {
            return self.process_reply(email, ghost, body, &room_id).await;
        }

        // Create a fresh room for this thread and name it after the subject.
        let from_vec = email.from().unwrap_or(&[]);
        let sender_name = from_vec.first().and_then(|f: &EmailAddress| f.name());
        let display_name = ghost_display_name(sender_name, &ghost.email);
        let room_id = crate::ghost::create_contact_room(
            &self.matrix,
            &self.store,
            &self.matrix_user_id,
            &ghost.email,
            &display_name,
        )
        .await?;
        // The room IS the thread, so strip Re:/Fwd: for the NAME (display only —
        // the stored subject below keeps its prefix so outbound replies still
        // carry "Re:" per email convention).
        let room_subject = crate::services::content::clean_subject(subject);
        if let Err(e) = self.matrix.set_room_name(&room_id, &room_subject).await {
            warn!(error = %e, "Failed to set thread room name");
        }
        // The email context is conveyed by the room NAME (the subject) and the
        // TOPIC below — both are state events Element renders as its grey
        // "changed the room name / topic" tiles at the top of the room. No
        // separate intro MESSAGE is posted: it would be redundant with the topic
        // tile and, carrying a send timestamp, would disturb date ordering. The
        // topic shows from→to, including which of the user's aliases received it.
        let own_email = self.store.get_user_email(&self.matrix_user_id).await?;
        let topic = recipient_alias(email, own_email.as_deref()).map_or_else(
            || format!("Email from {}", ghost.email),
            |to| format!("Email from {} to {to}", ghost.email),
        );
        if let Err(e) = self.matrix.set_room_topic(&room_id, &topic).await {
            warn!(error = %e, "Failed to set thread room topic");
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
            .save_thread_mapping_atomic(thread_id, &event_id, &room_id, subject)
            .await?;
        self.store.save_message_mapping(email_id, &event_id).await?;
        handle_attachments(
            &self.client,
            &self.matrix,
            &self.store,
            &self.matrix_user_id,
            email,
            &room_id,
            None,
            None,
            &ghost.user_id,
            timestamp,
        )
        .await?;
        Ok(())
    }

    async fn resolve_ghost(&self, email: &Email) -> Result<GhostUser> {
        let from_vec = email.from().unwrap_or(&[]);
        let sender = from_vec.first();
        let email_addr = sender.map_or(UNKNOWN_SENDER, jmap_client::email::EmailAddress::email);
        let name = sender.and_then(|f| f.name().map(str::to_owned));

        let localpart = crate::ghost::email_to_localpart(email_addr);
        let user_id = format!("@{}:{}", localpart, self.matrix.domain);
        tracing::debug!(
            "Resolving ghost user mapping for email: {} (localpart: {}, user_id: {})",
            email_addr,
            localpart,
            user_id
        );

        // Auto-register ghost. Only set the display name when the ghost is first
        // created: re-setting it per email (senders reuse one address with
        // varying From-names, e.g. newsletters) emits an m.room.member event in
        // every room the ghost is in, bumping those rooms to "now" and breaking
        // Element's date ordering. The name is "Name (email)" — or just the
        // email when the sender has none — so the address is always visible.
        let created = self.matrix.ensure_user_exists(&localpart).await?;
        if created {
            let display_name = ghost_display_name(name.as_deref(), email_addr);
            if let Err(e) = self.matrix.set_display_name(&user_id, &display_name).await {
                warn!(error = %e, "Failed to set ghost display name");
            }
        }

        Ok(GhostUser {
            email: email_addr.to_owned(),
            user_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{choose_recipient, ghost_display_name, should_skip_inbound};

    #[test]
    fn recipient_picks_the_alias_that_received_the_mail() {
        // The user has two aliases (same localpart). Whichever the mail used in
        // To is the one shown — picked by matching the user's localpart.
        let own = Some("thomas@palebluebytes.space");
        assert_eq!(
            choose_recipient(&["thomas@palebluebytes.xyz"], &[], own).as_deref(),
            Some("thomas@palebluebytes.xyz")
        );
        assert_eq!(
            choose_recipient(&["thomas@palebluebytes.space"], &[], own).as_deref(),
            Some("thomas@palebluebytes.space")
        );
        // User in Cc, a list in To -> the user's alias still wins.
        assert_eq!(
            choose_recipient(&["list@news.com"], &["thomas@palebluebytes.xyz"], own).as_deref(),
            Some("thomas@palebluebytes.xyz")
        );
        // No localpart match -> first To.
        assert_eq!(
            choose_recipient(&["someone@else.com"], &[], own).as_deref(),
            Some("someone@else.com")
        );
        // No recipients at all -> fall back to the user's own address.
        assert_eq!(choose_recipient(&[], &[], own).as_deref(), own);
    }

    #[test]
    fn display_name_combines_name_and_email() {
        assert_eq!(
            ghost_display_name(Some("Thomas Sean Dominic Kelly"), "thomassdk@pm.me"),
            "Thomas Sean Dominic Kelly (thomassdk@pm.me)"
        );
    }

    #[test]
    fn display_name_falls_back_to_email() {
        // No name, empty name, or whitespace -> just the address.
        assert_eq!(ghost_display_name(None, "a@b.com"), "a@b.com");
        assert_eq!(ghost_display_name(Some(""), "a@b.com"), "a@b.com");
        assert_eq!(ghost_display_name(Some("   "), "a@b.com"), "a@b.com");
    }

    #[test]
    fn skips_senderless_email() {
        // No From -> would otherwise create an `unknown@sender` room.
        assert!(should_skip_inbound(None, Some("me@example.com")));
        assert!(should_skip_inbound(None, None));
    }

    #[test]
    fn skips_own_outgoing_email() {
        // The Sent copy of our own message, re-ingested via Email/changes.
        assert!(should_skip_inbound(
            Some("me@example.com"),
            Some("me@example.com")
        ));
        // Case-insensitive on the address.
        assert!(should_skip_inbound(
            Some("Me@Example.COM"),
            Some("me@example.com")
        ));
    }

    #[test]
    fn bridges_real_inbound_from_a_contact() {
        assert!(!should_skip_inbound(
            Some("contact@elsewhere.com"),
            Some("me@example.com")
        ));
        // If we don't yet know our own address, a real sender still bridges.
        assert!(!should_skip_inbound(Some("contact@elsewhere.com"), None));
    }
}
