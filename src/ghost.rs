//! Logic for handling messages sent to "ghost" rooms (representing external email addresses).

use crate::matrix::MatrixClient;
use crate::routes::{AppState, notify};
use crate::sender::{JmapSender, human_bytes};
use crate::store::{Store, ThreadRepository};
use anyhow::{Context, Result};
use jmap_client::client::Client;
use jmap_client::email::Email;
use jmap_client::mailbox::Role as MailboxRole;
use matrix_sdk::ruma::events::room::message::{Relation, RoomMessageEventContent};
use std::fmt::Write as _;
use std::sync::Arc;
use tracing::{info, warn};

/// Subject for an outbound email sent into a room with no thread context.
/// Prefers the room's own name (set by `!compose` to the user's chosen subject),
/// falling back to a generic label.
#[must_use]
pub fn fresh_email_subject(room_name: Option<String>) -> String {
    room_name
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Matrix Conversation".to_owned())
}

/// Create a fresh Matrix contact room for `email`, returning its room id.
///
/// Registers the ghost user, creates the room (inviting the real user + ghost),
/// persists the room↔email binding, and files the room under the user's email
/// space. The bridge uses one room per email **thread**, and `!compose` starts a
/// new conversation each time, so this always creates — callers that need to
/// avoid duplicating a known thread lock and re-check the thread mapping first.
pub async fn create_contact_room(
    matrix: &MatrixClient,
    store: &Store,
    matrix_user_id: &str,
    email: &str,
    display_name: &str,
) -> Result<String> {
    // Register the ghost before creating the room. Set the display name only on
    // first creation — re-setting it churns m.room.member events across all of
    // the ghost's rooms (bumping them to "now" and breaking date ordering).
    let localpart = email_to_localpart(email);
    let ghost_user_id = format!("@{localpart}:{}", matrix.domain);
    if matrix.ensure_user_exists(&localpart).await? {
        if let Err(e) = matrix.set_display_name(&ghost_user_id, display_name).await {
            warn!(error = %e, "Failed to set ghost display name");
        }
    }

    let room_id = matrix
        .create_room_for_contact(display_name, email, matrix_user_id)
        .await?;
    info!("Created contact room {room_id} for ghost email: {email} (user: {matrix_user_id})");
    store
        .save_room_ghost_mapping(&room_id, email, matrix_user_id)
        .await?;
    let _ = matrix.join_room_as(&room_id, &ghost_user_id).await;

    // Join the REAL user synchronously (double-puppet) BEFORE any message is
    // posted into this room. The appservice can only invite; if the join is left
    // to the async /sync auto-accept loop (puppet.rs), the first email lands while
    // the user is merely invited, so Matrix files it as pre-join history (never
    // unread) and the later join becomes the room's newest event ("… joined the
    // room" as the last message). Joining first makes the email the latest event
    // AND counts it as unread. Best-effort: with no stored puppet token we fall
    // back to the async auto-accept loop (unconfigured double-puppet).
    if let Ok(Some(token)) = store.get_matrix_puppet_token(matrix_user_id).await {
        if let Err(e) =
            crate::puppet::join_room_via_token(&matrix.homeserver_url, &token, &room_id).await
        {
            warn!(
                error = %e, %matrix_user_id,
                "Failed to pre-join real user to contact room; relying on async auto-accept"
            );
        }
    }

    // Group the new conversation under the user's "email <address>" space.
    // Best-effort: a space failure must not fail room provisioning.
    if let Err(e) = ensure_room_in_email_space(matrix, store, matrix_user_id, &room_id).await {
        warn!(error = %e, "Failed to add contact room to email space");
    }
    Ok(room_id)
}

/// Ensure the user's email space exists and that `room_id` is a child of it.
pub(crate) async fn ensure_room_in_email_space(
    matrix: &MatrixClient,
    store: &Store,
    matrix_user_id: &str,
    room_id: &str,
) -> Result<()> {
    let space_id = ensure_email_space(matrix, store, matrix_user_id).await?;
    matrix.add_room_to_space(&space_id, room_id).await
}

/// Re-file all of a user's bridged thread rooms into their email space
/// (idempotent). Backs the `sync` command and the startup repair pass, closing
/// the gap where a room created while the space was unreachable never got linked.
/// Mailbox rooms are filed separately by the mailbox sync (they aren't user-keyed
/// in the store).
pub(crate) async fn repair_email_space(
    matrix: &MatrixClient,
    store: &Store,
    matrix_user_id: &str,
) -> Result<()> {
    let rooms = store.get_user_room_ids(matrix_user_id).await?;
    if rooms.is_empty() {
        return Ok(());
    }
    let space_id = ensure_email_space(matrix, store, matrix_user_id).await?;
    for room in rooms {
        if let Err(e) = matrix.add_room_to_space(&space_id, &room).await {
            warn!(error = %e, %room, "Failed to re-file room into email space");
        }
    }
    Ok(())
}

/// Return the user's email space room id, creating it on first use. Guarded by a
/// creation lock so two concurrent room provisions can't make two spaces.
async fn ensure_email_space(
    matrix: &MatrixClient,
    store: &Store,
    matrix_user_id: &str,
) -> Result<String> {
    let lock_key = format!("email_space:{matrix_user_id}");
    loop {
        if let Some(space) = store.get_email_space_room(matrix_user_id).await? {
            return Ok(space);
        }
        if store.try_acquire_room_creation_lock(&lock_key).await? {
            // Release the lock however we leave this block.
            let store_clone = store.clone();
            let lock_key_clone = lock_key.clone();
            let _guard = scopeguard::guard((), move |()| {
                tokio::spawn(async move {
                    let _ = store_clone
                        .release_room_creation_lock(&lock_key_clone)
                        .await;
                });
            });
            // Another trigger may have created it while we waited for the lock.
            if let Some(space) = store.get_email_space_room(matrix_user_id).await? {
                return Ok(space);
            }
            let space = create_email_space(matrix, store, matrix_user_id).await?;
            store.set_email_space_room(matrix_user_id, &space).await?;
            return Ok(space);
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

async fn create_email_space(
    matrix: &MatrixClient,
    store: &Store,
    matrix_user_id: &str,
) -> Result<String> {
    let label = store
        .get_user_email(matrix_user_id)
        .await?
        .unwrap_or_else(|| user_label(matrix_user_id));
    let name = format!("email {label}");
    let topic = format!(
        "Bridged email for {label}. Every room in this space is one email conversation, \
         mirrored to and from your mailbox by the JMAP bridge. Reply in a room to answer by \
         email, or use !compose <address> to start a new one."
    );
    let space = matrix.create_space(&name, &topic, matrix_user_id).await?;
    info!("Created email space {space} ({name}) for {matrix_user_id}");

    // Brand the space with the bridge logo. `main.rs` uploads the logo once at
    // startup and stores its `mxc` in `bot_avatar` ("<hash> <mxc>"); reuse that
    // rather than re-uploading. Best-effort — a missing/failed avatar must not
    // block the space, and the space is unusable without it either way.
    match store.get_bridge_state("bot_avatar").await {
        Ok(Some(state)) => {
            if let Some(mxc) = state.split_whitespace().nth(1) {
                if let Err(e) = matrix.set_room_avatar(&space, mxc).await {
                    warn!("Failed to set email space avatar for {matrix_user_id}: {e}");
                }
            } else {
                warn!("bot_avatar state {state:?} has no mxc; skipping space avatar");
            }
        }
        Ok(None) => warn!("No bot_avatar state yet; email space {space} created without avatar"),
        Err(e) => warn!("Failed to read bot_avatar state: {e}"),
    }

    Ok(space)
}

/// Fallback label when the user's email address isn't known yet: the localpart
/// of their Matrix id.
fn user_label(matrix_user_id: &str) -> String {
    matrix_user_id
        .trim_start_matches('@')
        .split(':')
        .next()
        .unwrap_or(matrix_user_id)
        .to_owned()
}

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

    // `show-images` is the text twin of the 🖼️ reaction (ADR-0011): when a user
    // replies to a bridged email with it, load that email's remote images
    // instead of treating the text as outbound mail. Both paths funnel into the
    // same `images::handle_load_images_reaction` core, so they can't diverge.
    if matches!(raw_body.trim(), "show-images" | "!show-images") {
        return handle_show_images(state, sender_id, rm_id, content).await;
    }
    // `delete-room`/`spam` are the text twins of the 🗑/🚫 reactions (ADR-0011):
    // move the whole thread to Trash/Junk and unbridge the room, rather than
    // sending the word as mail.
    if matches!(raw_body.trim(), "delete-room" | "!delete-room") {
        return handle_thread_action(state, sender_id, rm_id, MailboxRole::Trash, "Trash").await;
    }
    if matches!(raw_body.trim(), "spam" | "!spam") {
        return handle_thread_action(state, sender_id, rm_id, MailboxRole::Junk, "Junk").await;
    }

    let mut body_str = raw_body.to_owned();
    let _ = crate::services::content::append_user_signature(
        &state.client_manager.store,
        sender_id,
        &mut body_str,
    )
    .await;

    // Require an active session before queuing — the worker can't submit without
    // one, and the user should hear about it now rather than silently.
    let Some(client) = state.client_manager.get_client(sender_id).await else {
        notify(state, Some(rm_id), "You are not logged in. Please type `login` in your private bridge room to connect your account.").await;
        return Ok(());
    };

    // Resolve thread context now (while we have the relation + client) and encode
    // it for the worker; a fresh email has none. The actual JMAP submission is
    // deferred to the send-delay worker (ADR-0012), so a redaction inside the
    // window can still cancel it.
    let thread_root = resolve_thread_context(state, rm_id, content.relates_to.as_ref(), &client)
        .await?
        .map(encode_thread_root);
    enqueue_held_send(
        state,
        sender_id,
        rm_id,
        _event_id,
        &body_str,
        thread_root.as_deref(),
        None,
    )
    .await
}

/// Encode a resolved thread context into the `thread_root_id` the worker parses
/// (`jmap_thread_id|parent_email_id|root_event_id`).
fn encode_thread_root(
    (jmap_thread_id, parent_id, root_event_id, _latest, _subject): (
        String,
        String,
        String,
        Option<String>,
        String,
    ),
) -> String {
    let sep = crate::store::THREAD_QUEUE_SEPARATOR;
    format!("{jmap_thread_id}{sep}{parent_id}{sep}{root_event_id}")
}

/// Enqueue an outbound message to be submitted after the user's send-delay
/// window (ADR-0012). The worker (`retry::run_retry_loop`) performs the JMAP
/// submission once `release_at` passes; a redaction in between cancels it and an
/// edit rewrites its body.
async fn enqueue_held_send(
    state: &AppState,
    sender_id: &str,
    rm_id: &str,
    event_id: &str,
    body: &str,
    thread_root: Option<&str>,
    attachments_json: Option<&str>,
) -> Result<()> {
    let delay = state.client_manager.send_delay_for(sender_id).await;
    state
        .client_manager
        .store
        .add_to_outbound_queue(
            sender_id,
            rm_id,
            event_id,
            body,
            None,
            thread_root,
            attachments_json,
            delay,
        )
        .await?;

    // Show the held state (#26): a ⏳ reaction while in the send-delay window,
    // plus a one-time per-room explainer. Only when there's an actual hold.
    if delay > 0 {
        let matrix = &state.client_manager.matrix;
        let store = &state.client_manager.store;
        crate::services::send_state::mark_held(matrix, store, sender_id, rm_id, event_id).await;
        maybe_send_hold_hint(state, sender_id, rm_id, delay).await;
    }
    Ok(())
}

/// Post a one-time-per-room explainer the first time a held send happens there,
/// so the ⏳→✅ flow and the undo window are discoverable (#26).
async fn maybe_send_hold_hint(state: &AppState, sender_id: &str, rm_id: &str, delay: i64) {
    let key = format!("send_hint:{rm_id}");
    let store = &state.client_manager.store;
    if matches!(store.get_jmap_state(sender_id, &key).await, Ok(Some(_))) {
        return;
    }
    notify(
        state,
        Some(rm_id),
        &format!(
            "Your messages are held {delay}s before sending: ⏳ = waiting (redact to undo, edit to change), ✅ = sent, ❌ = failed. Change the window with `send-delay`."
        ),
    )
    .await;
    let _ = store.save_jmap_state(sender_id, &key, "1").await;
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

    // Require an active session before uploading/queuing (the worker resolves the
    // ghost email from the room mapping at submit time).
    let Some(client) = state.client_manager.get_client(sender_id).await else {
        notify(state, Some(rm_id), "You are not logged in. Please type `login` in your private bridge room to connect your account.").await;
        return Ok(());
    };

    let sender =
        JmapSender::new(client.clone()).with_quote_replies(state.client_manager.quote_replies);
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

    // The media is uploaded to the JMAP blob store now (we have the Matrix
    // content here), but submission is deferred to the send-delay worker like
    // text (ADR-0012): encode the thread context and queue with the attachment.
    let thread_root = resolve_thread_context(state, rm_id, content.relates_to.as_ref(), &client)
        .await?
        .map(encode_thread_root);
    let atts_json = serde_json::to_string(&vec![att])?;
    enqueue_held_send(
        state,
        sender_id,
        rm_id,
        _event_id,
        "Sent an attachment from Matrix.",
        thread_root.as_deref(),
        Some(&atts_json),
    )
    .await
}

/// Handle the `show-images` text command in a ghost room: load the replied-to
/// email's remote images via the same core the 🖼️ reaction uses (ADR-0011).
async fn handle_show_images(
    state: &AppState,
    sender_id: &str,
    rm_id: &str,
    content: &RoomMessageEventContent,
) -> Result<()> {
    // A de-permissioned user can't act, even on a room they once used (ADR-0010).
    if state.permissions.level_for(sender_id).is_none() {
        return Ok(());
    }
    let Some(target_event_id) = reply_target_event(content.relates_to.as_ref()) else {
        notify(
            state,
            Some(rm_id),
            "Reply to the email message with `show-images` (or react 🖼️) to load its images.",
        )
        .await;
        return Ok(());
    };
    crate::services::images::handle_load_images_reaction(state, sender_id, rm_id, target_event_id)
        .await
}

/// 🗑 wastebasket (U+1F5D1) — the reaction twin of `delete-room` (→ Trash).
const TRASH_CODEPOINT: char = '\u{1F5D1}';
/// 🚫 no-entry (U+1F6AB) — the reaction twin of `spam` (→ Junk).
const JUNK_CODEPOINT: char = '\u{1F6AB}';

/// True if a reaction key is the 🗑 trash gesture (tolerating variation/skin
/// selectors), i.e. the same action as the `delete-room` command.
#[must_use]
pub(crate) fn is_trash_reaction(key: &str) -> bool {
    key.chars().any(|c| c == TRASH_CODEPOINT)
}

/// True if a reaction key is the 🚫 junk gesture, i.e. the `spam` command.
#[must_use]
pub(crate) fn is_junk_reaction(key: &str) -> bool {
    key.chars().any(|c| c == JUNK_CODEPOINT)
}

/// Move a room's whole thread to `role` (Trash or Junk) on the JMAP server, then
/// unbridge the room (ADR-0012). The unit is the thread — a reaction on any one
/// message acts on the entire conversation, because one room maps to one thread.
/// `label` is the human name ("Trash"/"Junk") used in notices.
///
/// Reversible by design: this only *moves* mail, never destroys it. If the
/// account has no mailbox for `role`, the room is unbridged locally and the user
/// is told the server-side move couldn't happen — never a guess or a destroy. A
/// transient server error aborts without unbridging, so the user can retry.
pub(crate) async fn handle_thread_action(
    state: &AppState,
    sender_id: &str,
    rm_id: &str,
    role: MailboxRole,
    label: &str,
) -> Result<()> {
    if state.permissions.level_for(sender_id).is_none() {
        return Ok(());
    }
    let Some(client) = state.client_manager.get_client(sender_id).await else {
        notify(state, Some(rm_id), "You are not logged in.").await;
        return Ok(());
    };
    let store = &state.client_manager.store;
    // Resolve the ghost before unbridging (the lookup is gone afterwards).
    let ghost_email = store.get_ghost_email_by_room(rm_id).await?;

    let mut had_mailbox = true;
    if let Some((thread_id, _root, _subject)) = store.get_latest_thread_in_room(rm_id).await? {
        let sender = JmapSender::new(client);
        match sender.move_thread_to_role(&thread_id, role).await {
            Ok(true) => {}
            Ok(false) => had_mailbox = false,
            Err(e) => {
                warn!(error = %e, %rm_id, "Failed to move thread; not unbridging");
                notify(
                    state,
                    Some(rm_id),
                    &format!("Couldn't move this conversation to {label} (server error) — nothing changed. Try again."),
                )
                .await;
                return Ok(());
            }
        }
    }

    // Unbridge: drop mappings, then have the bot and ghost leave so the room is
    // defunct. The real user can leave on their own.
    store.unbridge_room(rm_id).await?;
    let matrix = &state.client_manager.matrix;
    let _ = matrix.leave_room(rm_id, &matrix.bot_user_id()).await;
    if let Some(email) = ghost_email {
        let ghost_user_id = format!("@{}:{}", email_to_localpart(&email), matrix.domain);
        let _ = matrix.leave_room(rm_id, &ghost_user_id).await;
    }

    let msg = if had_mailbox {
        format!("Moved this conversation to {label} and unbridged the room.")
    } else {
        format!(
            "No {label} mailbox on your account, so I unbridged this room locally but couldn't move the mail server-side."
        )
    };
    notify(state, Some(rm_id), &msg).await;
    Ok(())
}

/// The event id a reply/thread relation points at — the message `show-images`
/// (or a reaction) targets. `None` when the message isn't a reply.
fn reply_target_event(
    relates_to: Option<
        &Relation<matrix_sdk::ruma::events::room::message::RoomMessageEventContentWithoutRelation>,
    >,
) -> Option<&str> {
    match relates_to? {
        Relation::Reply { in_reply_to } => Some(in_reply_to.event_id.as_str()),
        Relation::Thread(thread) => Some(thread.event_id.as_str()),
        _ => None,
    }
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
    {
        if let Some(event_id) = reply_target_event(relates_to) {
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

    // 2. Fall back to the room's most recent thread. The parent email id is
    //    best-effort only — threading now resolves Message-IDs from the JMAP
    //    thread itself (see JmapSender::reply_headers), so a missing parent must
    //    NOT prevent us from taking the reply path.
    if let Ok(Some((jmap_thread_id, root_event_id, subject))) = state
        .client_manager
        .store
        .get_latest_thread_in_room(rm_id)
        .await
    {
        let parent_email_id = state
            .client_manager
            .store
            .get_last_email_id_by_room(rm_id)
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
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

    Ok(None)
}

pub(crate) async fn try_resolve_thread(
    client: &Arc<Client>,
    email_id: &str,
) -> Result<Option<String>> {
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

    #[test]
    #[allow(clippy::unwrap_used)]
    fn reply_target_event_resolves_only_replies() {
        use matrix_sdk::ruma::events::relation::InReplyTo;
        use matrix_sdk::ruma::events::room::message::{
            MessageType, RoomMessageEventContent, TextMessageEventContent,
        };

        // A bare message (no relation) has no target — `show-images` here just
        // shows the usage hint.
        let plain = RoomMessageEventContent::text_plain("show-images");
        assert_eq!(reply_target_event(plain.relates_to.as_ref()), None);

        // A reply resolves to the message it answers — the email to load.
        let mut reply = RoomMessageEventContent::new(MessageType::Text(
            TextMessageEventContent::plain("show-images"),
        ));
        reply.relates_to = Some(Relation::Reply {
            in_reply_to: InReplyTo::new("$target:localhost".try_into().unwrap()),
        });
        assert_eq!(
            reply_target_event(reply.relates_to.as_ref()),
            Some("$target:localhost")
        );
    }

    #[test]
    fn trash_and_junk_reaction_glyphs() {
        assert!(is_trash_reaction("🗑️")); // U+1F5D1 + U+FE0F
        assert!(is_trash_reaction("🗑")); // bare U+1F5D1
        assert!(!is_trash_reaction("🚫"));
        assert!(is_junk_reaction("🚫")); // U+1F6AB
        assert!(!is_junk_reaction("👍"));
        assert!(!is_junk_reaction(""));
    }
}
